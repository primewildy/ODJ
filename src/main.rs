//! DJ controller — eframe app. Engine owns audio, MIDI thread runs alongside,
//! UI runs on the main thread.

mod midi;

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Parser;
use serde::Deserialize;

/// Controller tweaks loaded from `controls.toml` in the working directory.
/// Lets per-build hardware quirks (e.g. a backward-wired fader) be fixed
/// without recompiling. Absent file → defaults (no inversion).
#[derive(Deserialize, Default)]
struct Controls {
    /// MIDI CC numbers whose 0-127 value should be flipped.
    #[serde(default)]
    invert_cc: Vec<u8>,
}

fn load_controls() -> Controls {
    match std::fs::read_to_string("controls.toml") {
        Ok(s) => match toml::from_str::<Controls>(&s) {
            Ok(c) => {
                if !c.invert_cc.is_empty() {
                    eprintln!("controls.toml: inverting CCs {:?}", c.invert_cc);
                }
                c
            }
            Err(e) => {
                eprintln!("controls.toml parse error ({e}); using defaults");
                Controls::default()
            }
        },
        Err(_) => Controls::default(),
    }
}

/// CLI flags are all `Option` so we can do CLI > settings.toml > builtin
/// precedence in `main`. A flag that *isn't* passed leaves the field
/// `None`, and the resolver falls through to the persisted setting,
/// then to the built-in default below.
#[derive(Parser, Debug)]
#[command(name = "dj", about = "DJ controller — two decks, GUI + MIDI")]
struct Cli {
    /// Optional cpal device name for the master output. Defaults to the
    /// `pipewire` device on PipeWire systems (which then follows either
    /// the default sink or the PIPEWIRE_NODE env var).
    #[arg(long)]
    device: Option<String>,

    /// Optional cpal device name for the cue / PFL output (headphones).
    /// When set, an independent second audio stream is opened on this
    /// device and decks with their `cue` toggle on are mixed into it
    /// pre-fader. Example: `--cue-device hw:CARD=USB,DEV=0`.
    #[arg(long)]
    cue_device: Option<String>,

    /// Comma-separated substrings to match the MIDI input port name.
    /// First match wins. Default covers the ODJ controller plus the
    /// LPD8 dev controller. Set to "" to disable MIDI entirely.
    #[arg(long)]
    midi: Option<String>,

    /// Directory to scan for audio files (relative to CWD by default).
    #[arg(long)]
    music_dir: Option<PathBuf>,
}

const DEFAULT_MIDI: &str = "ODJ,LPD8";
const DEFAULT_MUSIC_DIR: &str = "music";

fn main() -> Result<()> {
    let cli = Cli::parse();

    // CLI > settings.toml > built-in default. We load settings up
    // front and resolve every field once, then never re-read CLI args
    // beyond this point — downstream code (engine, UI) only sees the
    // *resolved* values.
    let settings = ui::settings::Settings::load();
    let device = cli.device.or_else(|| settings.audio_device.clone());
    let cue_device = cli.cue_device.or_else(|| settings.cue_device.clone());
    let midi_filter = cli
        .midi
        .or_else(|| settings.midi_port.clone())
        .unwrap_or_else(|| DEFAULT_MIDI.to_string());
    let music_dir = cli
        .music_dir
        .or_else(|| settings.music_dir.clone())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MUSIC_DIR));

    if let Some(node) = &device {
        unsafe {
            std::env::set_var("PIPEWIRE_NODE", node);
        }
        eprintln!("PIPEWIRE_NODE = {node}");
    }

    let engine = audio::Engine::start(
        device.as_deref(),
        cue_device.as_deref(),
    )?;
    let sender = engine.sender();

    // Apply per-deck startup defaults from settings. These re-fire on
    // every launch so the engine's compile-time defaults can be
    // overridden persistently without code edits.
    for deck in [control::DeckId::A, control::DeckId::B] {
        let d = settings.deck(deck);
        let _ = sender.send(control::DeckCommand::SetPitchLock { deck, on: d.pitch_lock });
        let _ = sender.send(control::DeckCommand::SetBeatAlign { deck, on: d.beat_align });
    }

    let stem_mode = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let log_midi = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(settings.log_midi));
    let midi_status = if midi_filter.is_empty() {
        "MIDI: disabled".to_string()
    } else {
        // ODJ controller drives both decks now.
        let tel_a = engine.telemetry(control::DeckId::A);
        let tel_b = engine.telemetry(control::DeckId::B);
        let controls = load_controls();
        match midi::start(
            &midi_filter,
            sender.clone(),
            tel_a,
            tel_b,
            controls.invert_cc,
            std::sync::Arc::clone(&stem_mode),
            std::sync::Arc::clone(&log_midi),
        ) {
            Ok(m) => {
                eprintln!("midi: connected to {}", m.port_name);
                // Keep alive for the whole program lifetime by leaking. Simpler
                // than passing ownership across the eframe boundary.
                Box::leak(Box::new(m));
                format!("MIDI: {midi_filter}")
            }
            Err(e) => {
                eprintln!("midi: not started ({e})");
                format!("MIDI: {e}")
            }
        }
    };

    let effective = ui::settings::EffectiveDefaults {
        music_dir: music_dir.clone(),
        audio_device: device.clone(),
        cue_device: cue_device.clone(),
        midi_port: midi_filter.clone(),
        audio_devices: audio::list_output_devices(),
        midi_ports: midi::list_inputs(),
    };
    let app = ui::DjApp::new(
        engine, music_dir, midi_status, stem_mode, settings, log_midi, effective,
    );
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("ODJ")
            .with_inner_size([1100.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ODJ",
        options,
        Box::new(move |cc| {
            // Wayland heartbeat: when the window loses focus the
            // compositor's xdg ping can time out before our next
            // repaint lands, which makes Hyprland pop the
            // "force kill / wait?" dialog. A 1 s wake-up guarantees
            // the winit event loop pumps often enough to keep the
            // protocol happy.
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || loop {
                // 10 Hz — Hyprland's xdg ping has a ~5 s timeout, but
                // some surfaces seem to drop wake-ups, so we pad
                // heavily. Cost is negligible (a single atomic flip
                // plus a wl_display roundtrip).
                std::thread::sleep(std::time::Duration::from_millis(100));
                ctx.request_repaint();
            });
            // Bundled fonts (Roboto + JetBrains Mono). Registered
            // once at startup; egui owns the bytes from then on.
            ui::fonts::apply(&cc.egui_ctx);
            // System theme detection (Linux/Wayland — eframe's
            // built-in follow_system_theme only fires on macOS /
            // Windows). Applies the OS-preferred light/dark palette
            // immediately and re-applies whenever it changes.
            ui::theme::spawn_watcher(cc.egui_ctx.clone());
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("eframe: {e}"))?;

    Ok(())
}
