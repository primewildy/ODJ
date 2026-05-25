//! Control surface: `DeckCommand` enum + lock-free SPSC ring types.
//!
//! Producers (keyboard, MIDI, GUI) push `DeckCommand`s; the audio thread
//! drains the ring at the top of each callback. No locks, no allocations
//! on send or drain in steady state.

use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckId {
    A,
    B,
}

/// Decoded, ready-to-play audio buffer. Interleaved f32 samples.
pub struct TrackBuffer {
    pub samples: Vec<f32>,
    pub channels: u16,
    pub sample_rate: u32,
}

impl TrackBuffer {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }

    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.frames() as f64 / self.sample_rate as f64
        }
    }
}

/// Per-track analysis. v1 fills `bpm` + `beat_grid` (spectral-flux + autocorr)
/// + `key` (Krumhansl-Schmuckler).
/// v1.5 will fill `downbeats`, `phrase_boundaries`, `auto_cue`. The audio
/// engine reads via the shared Arc; doesn't care which version wrote.
pub struct TrackAnalysis {
    pub analysis_version: u32,
    pub bpm: f32,
    /// Beat times in seconds from start of the track.
    pub beat_grid: Vec<f64>,
    pub duration_secs: f64,
    pub sample_rate: u32,
    /// Detected musical key; None if detection failed or the signal is
    /// non-tonal.
    pub key: Option<MusicalKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MusicalKey {
    /// Tonic 0..12, where 0 = C, 1 = C#, …, 11 = B.
    pub tonic: u8,
    pub is_minor: bool,
}

impl MusicalKey {
    /// Camelot Wheel notation (the DJ-standard number format).
    /// Minor keys get the suffix `A`, major keys get `B`. Position on the
    /// wheel follows the circle of fifths starting at 8B = C major.
    pub fn label(&self) -> String {
        // Tonic → Camelot number for *major* keys (circle of fifths).
        // For minor keys we route via the relative major (tonic + 3 semitones).
        const MAJOR: [u8; 12] = [8, 3, 10, 5, 12, 7, 2, 9, 4, 11, 6, 1];
        let major_tonic = if self.is_minor {
            (self.tonic + 3) % 12
        } else {
            self.tonic % 12
        };
        let number = MAJOR[major_tonic as usize];
        let letter = if self.is_minor { 'A' } else { 'B' };
        format!("{number}{letter}")
    }
}

pub enum DeckCommand {
    LoadTrack {
        deck: DeckId,
        buffer: Arc<TrackBuffer>,
        analysis: Arc<TrackAnalysis>,
    },
    Play(DeckId),
    Pause(DeckId),
    /// Toggle play <-> pause. Engine is authoritative — producers don't need
    /// to track their own play-state mirror.
    PlayToggle(DeckId),
    Stop(DeckId),
    SetCue { deck: DeckId, sample_pos: u64 },
    JumpToCue(DeckId),
    /// Pioneer-style CUE button down. State-machine:
    ///   Playing → jump to cue, pause (release is a no-op).
    ///   Paused  → cue := current playhead, start playing (preview).
    CuePress(DeckId),
    /// CUE button release. If a preview was triggered by CuePress (paused
    /// path), return to cue and pause. Otherwise no-op.
    CueRelease(DeckId),
    Seek { deck: DeckId, sample_pos: u64 },
    /// Vinyl-coupled pitch/speed. 1.0 = unity. Typical DJ range 0.92..1.08.
    SetSpeed { deck: DeckId, ratio: f32 },
    /// Small persistent additive adjustment to speed_ratio (clamped to ±8%).
    /// Kept for future use; not currently bound to MIDI pads.
    NudgeSpeed { deck: DeckId, delta: f32 },
    /// Temporary, while-held nudge: adds `offset` to the deck's effective
    /// speed_ratio for playback only. Reset to 0 on pad release. Used like
    /// a vinyl push/pull to fine-align tracks by ear.
    SetNudge { deck: DeckId, offset: f32 },
    /// When true, CuePress (paused → set-cue branch) snaps the new cue
    /// point to the nearest beat in the deck's beat_grid.
    SetQuantize { deck: DeckId, on: bool },
    /// Per-deck linear gain. 1.0 = unity. Clamped to [0.0, 2.0] by engine.
    SetGain { deck: DeckId, gain: f32 },
    /// When true, the deck uses time-stretching (phase vocoder) instead of
    /// vinyl-style resampling — pitch stays at native, only tempo changes.
    SetPitchLock { deck: DeckId, on: bool },
    /// Low-shelf gain in dB. Typical -25..+6, 0 = flat. Shelf at 250 Hz.
    SetEqLow { deck: DeckId, db: f32 },
    /// High-shelf gain in dB. Typical -25..+6, 0 = flat. Shelf at 4 kHz.
    SetEqHigh { deck: DeckId, db: f32 },
    /// Match this deck's tempo to the OTHER deck's effective BPM. Clamped
    /// to ±8%. No-op if either deck's analysis BPM is missing.
    Sync { deck: DeckId },
    /// When ON, transitions from paused→playing on this deck (CuePress
    /// paused branch or PlayToggle pause→play) shift this deck's playhead
    /// so its nearest beat aligns with the OTHER deck's nearest beat. Used
    /// to correct slight cue mis-timing automatically.
    SetBeatAlign { deck: DeckId, on: bool },
    /// PFL / cue-monitor toggle. When ON, this deck's post-EQ pre-fader
    /// signal is summed into the cue bus (routed to the secondary audio
    /// output, typically headphones). Multiple decks can be cued at once;
    /// their cue contributions sum.
    SetCueOn { deck: DeckId, on: bool },
}

pub type CommandProducer = rtrb::Producer<DeckCommand>;
pub type CommandConsumer = rtrb::Consumer<DeckCommand>;

pub fn channel(capacity: usize) -> (CommandProducer, CommandConsumer) {
    rtrb::RingBuffer::new(capacity)
}
