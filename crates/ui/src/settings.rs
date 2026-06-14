//! User-editable settings persisted at `~/.config/odj/settings.toml`.
//!
//! Holds **startup defaults** — preferences the user wants reapplied
//! every launch, not the engine's live state. Editing a value here
//! doesn't move the running engine; it changes what the *next*
//! engine reads at boot. (Per FEATURES.md §1: settings hold startup
//! defaults, the engine owns live state.)
//!
//! Precedence from main.rs's point of view:
//!     CLI flag (Option<T>) > settings.toml field > built-in default
//!
//! On-disk format is plain TOML; missing fields silently fall back
//! to the struct's `Default` impl via `#[serde(default)]`. Atomic
//! writes use the standard write-temp + rename pattern so a crash
//! mid-save can never leave a half-written file.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The values the engine is actually running with this session,
/// resolved by main.rs through the precedence CLI > settings.toml >
/// built-in default. The settings window uses these as **placeholder
/// text** so empty fields don't lie about what's in effect — empty
/// in settings.toml just means "I'm not overriding the default", not
/// "there is no value".
#[derive(Debug, Clone, Default)]
pub struct EffectiveDefaults {
    pub music_dir: std::path::PathBuf,
    pub audio_device: Option<String>,
    pub cue_device: Option<String>,
    pub midi_port: String,
    /// All cpal output devices on the system, captured at startup.
    /// Populates the dropdown in the settings UI; if a connected
    /// device is renamed between runs it just won't appear here
    /// until next launch.
    pub audio_devices: Vec<String>,
    /// All MIDI input ports captured at startup.
    pub midi_ports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Library directory. None = let main.rs use its CLI default.
    #[serde(default)]
    pub music_dir: Option<PathBuf>,
    /// cpal device name for the master output (substring match).
    #[serde(default)]
    pub audio_device: Option<String>,
    /// cpal device name for the headphone / cue output.
    #[serde(default)]
    pub cue_device: Option<String>,
    /// MIDI port-name filter (comma-separated substrings). Default
    /// from main.rs is `"ODJ,LPD8"`.
    #[serde(default)]
    pub midi_port: Option<String>,
    /// Mirror every MIDI message to stderr. Useful when wiring a new
    /// controller; noisy in regular use.
    #[serde(default = "default_true")]
    pub log_midi: bool,
    #[serde(default)]
    pub deck_a: DeckDefaults,
    #[serde(default)]
    pub deck_b: DeckDefaults,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeckDefaults {
    pub pitch_lock: bool,
    pub beat_align: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            music_dir: None,
            audio_device: None,
            cue_device: None,
            midi_port: None,
            log_midi: true,
            deck_a: DeckDefaults::default(),
            deck_b: DeckDefaults::default(),
        }
    }
}

impl Default for DeckDefaults {
    fn default() -> Self {
        // CLAUDE.md notes both default ON for the engine; mirror that
        // so a fresh settings.toml doesn't silently flip behaviour.
        Self {
            pitch_lock: true,
            beat_align: true,
        }
    }
}

impl Settings {
    /// XDG location: `$XDG_CONFIG_HOME/odj/settings.toml`, falling
    /// back to `~/.config/odj/settings.toml`.
    pub fn config_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
            })?;
        Some(base.join("odj").join("settings.toml"))
    }

    /// Best-effort load. Missing file or parse error → defaults
    /// (loud-ish on parse error so the user sees they typo'd a key
    /// instead of silently getting defaults).
    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        match fs::read_to_string(&path) {
            Ok(s) => match toml::from_str::<Settings>(&s) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!(
                        "settings: parse error in {} ({}); using defaults",
                        path.display(),
                        e
                    );
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Atomic save. Writes to a temp file in the same directory then
    /// renames over the destination — rename(2) within one filesystem
    /// is atomic, so a power-loss / crash mid-write leaves the old
    /// file intact rather than corrupting it.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = Self::config_path() else {
            return Err(std::io::Error::other("no XDG_CONFIG_HOME or HOME"));
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let serialized = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::other(format!("toml encode: {e}")))?;
        let tmp = path.with_extension("toml.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(serialized.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)
    }
}

impl Settings {
    /// Get the per-deck default block for a given DeckId. Lets the
    /// startup-apply logic loop over both decks without a match.
    pub fn deck(&self, deck: control::DeckId) -> &DeckDefaults {
        match deck {
            control::DeckId::A => &self.deck_a,
            control::DeckId::B => &self.deck_b,
        }
    }

    pub fn deck_mut(&mut self, deck: control::DeckId) -> &mut DeckDefaults {
        match deck {
            control::DeckId::A => &mut self.deck_a,
            control::DeckId::B => &mut self.deck_b,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_engine_defaults() {
        let s = Settings::default();
        assert!(s.deck_a.pitch_lock);
        assert!(s.deck_a.beat_align);
        assert!(s.deck_b.pitch_lock);
        assert!(s.deck_b.beat_align);
        assert!(s.log_midi);
    }

    #[test]
    fn roundtrip_preserves_overrides() {
        let mut s = Settings::default();
        s.deck_a.pitch_lock = false;
        s.midi_port = Some("FOO".into());
        s.log_midi = false;
        let encoded = toml::to_string_pretty(&s).unwrap();
        let decoded: Settings = toml::from_str(&encoded).unwrap();
        assert!(!decoded.deck_a.pitch_lock);
        assert!(decoded.deck_b.pitch_lock); // unchanged
        assert_eq!(decoded.midi_port.as_deref(), Some("FOO"));
        assert!(!decoded.log_midi);
    }

    #[test]
    fn missing_field_falls_back_to_default() {
        // A minimal file with just one override should load with
        // every other field at its default.
        let toml_src = "log_midi = false\n";
        let s: Settings = toml::from_str(toml_src).unwrap();
        assert!(!s.log_midi);
        assert!(s.deck_a.pitch_lock); // default came back
        assert_eq!(s.midi_port, None);
    }
}
