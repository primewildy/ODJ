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

/// Wire-compatible identifier for an FX effect. The audio crate's
/// `FxKind` is the source of truth; this control-side mirror lets
/// the UI send `SetFxKind` without depending on the audio crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FxKindId {
    Echo,
    Reverb,
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

/// Four HTDemucs stems for a single track, all interleaved at the same
/// sample-rate / channel layout as the source `TrackBuffer`. The mixer
/// reads them via an `Arc<TrackStems>` so the async stem worker can
/// hand off without copying audio. The UI exposes only 3 controls;
/// vocals + other are summed at playback as "melody".
pub struct TrackStems {
    pub drums: Vec<f32>,
    pub bass: Vec<f32>,
    pub vocals: Vec<f32>,
    pub other: Vec<f32>,
    pub channels: u16,
    pub sample_rate: u32,
}

impl TrackStems {
    pub fn frames(&self) -> usize {
        self.drums.len() / self.channels.max(1) as usize
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
    /// Indices into `beat_grid` of beat-position-1 downbeats. Empty for
    /// v1-cache entries that pre-date model-driven detection — in that
    /// case the UI falls back to `i % 4 == 0`. Populated by the
    /// beat_this ONNX model from v2 onwards.
    pub downbeats: Vec<u32>,
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
    /// Replace the deck's analysis without resetting playback state.
    /// Used by the async load path — the deck initially loads with
    /// whatever analysis we have (cache or empty), and a background
    /// thread sends `UpdateAnalysis` once the model has finished.
    UpdateAnalysis {
        deck: DeckId,
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
    /// Mid peaking-filter gain in dB. Typical -25..+6, 0 = flat. ~1 kHz.
    SetEqMid { deck: DeckId, db: f32 },
    /// High-shelf gain in dB. Typical -25..+6, 0 = flat. Shelf at 4 kHz.
    SetEqHigh { deck: DeckId, db: f32 },
    /// Per-deck stem gains. Linear 0.0..=1.0+. Default 1.0 (unity).
    /// "instruments" = bass + other summed at playback — everything
    /// that isn't drums or vocals. (HTDemucs gives 4 stems; we expose
    /// 3 controls because mixing 6 is past human capacity in real
    /// time. See docs/notes/stem_separation.md.) No-op until the
    /// deck's stem buffers are loaded.
    SetStemDrums { deck: DeckId, gain: f32 },
    SetStemVocals { deck: DeckId, gain: f32 },
    SetStemInstruments { deck: DeckId, gain: f32 },
    /// Replace the deck's stem buffers without resetting playback. Sent
    /// by the async stem-separation worker when results land.
    SetStems {
        deck: DeckId,
        stems: Arc<TrackStems>,
    },
    /// Match this deck's tempo to the OTHER deck's effective BPM. Clamped
    /// to ±8%. No-op if either deck's analysis BPM is missing.
    Sync { deck: DeckId },

    // ---- Hot cues (8 slots per deck) -----------------------------
    //
    // One press dispatches to "set" or "jump" depending on whether
    // the slot is currently set — the engine is authoritative, same
    // shape as `PlayToggle`. Pioneer-style preview behaviour: while
    // paused, jumping to a set slot starts playback as a preview
    // that releases back to the slot's frame position on
    // `HotCueRelease`.

    /// Per-slot transport press.
    /// - Empty slot → store current playhead in the slot
    ///   (snapped to the nearest beat if `quantize` is on).
    /// - Set slot while playing → seek to the slot's frame, keep
    ///   playing.
    /// - Set slot while paused → seek to the slot's frame and
    ///   start playback as a *preview* (mirrors the CUE state
    ///   machine but the return point is the slot, not `cue_frame`).
    HotCueSetOrJump { deck: DeckId, slot: u8 },
    /// Release the preview started by a `HotCueSetOrJump` on a
    /// paused deck: jump back to the slot's frame and pause. No-op
    /// unless THIS slot's preview is active (so two pads pressed in
    /// quick succession can't crosswire).
    HotCueRelease { deck: DeckId, slot: u8 },
    /// Forget the slot's stored frame. UI-side shift-click or a
    /// future MIDI shift-pad chord. No-op if the slot was already
    /// empty.
    HotCueClear { deck: DeckId, slot: u8 },
    /// Bulk-load all eight slots — sent by the UI right after a
    /// `LoadTrack` with positions converted from the `.track-meta`
    /// store (UI does seconds↔frames using the track's sample
    /// rate). `None` clears the slot, `Some(frame)` sets it.
    HotCueLoad { deck: DeckId, slots: [Option<u64>; 8] },
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
    /// Headphone-bus output gain (global, not per-deck). 1.0 = unity.
    /// Clamped to [0, 2] by the engine. The headphone volume knob.
    SetCueGain { gain: f32 },
    /// Headphone CUE↔MASTER blend (Pioneer's "Headphones Mix" knob).
    /// 0 = pure master in the headphones, 1 = pure cue (only the decks
    /// with `cue_on`). Intermediate values mix the two. Clamped to [0, 1].
    SetCueMix { mix: f32 },
    /// Master-bus output gain (post-mix). Applied AFTER each deck's
    /// per-channel gain so it scales the whole mix uniformly. The cue
    /// bus is NOT affected — you can still hear what's playing while
    /// the master is dipped. Clamped to [0, 2].
    SetMasterGain { gain: f32 },

    // ----- FX (post-EQ pre-fader) ----------------------------------
    //
    // The deck's FX chain currently hosts a single Echo effect; more
    // effect types (Reverb, Filter, …) land as the chain grows. The
    // command surface is already shaped for that — pass the effect's
    // *parameter* names rather than the underlying DSP variable, so a
    // new effect just re-interprets `colour` / `time` / `mix`.

    /// Select the active FX kind for a deck. The UI's effect-picker
    /// dropdown maps each entry to one of these values; the engine
    /// swaps which apply path runs without re-allocating either
    /// effect's state.
    SetFxKind { deck: DeckId, kind: FxKindId },
    /// Turn the per-deck FX bypass on/off. When OFF, the chain still
    /// runs to decay its tail (no hard-zero clicks); the dry path is
    /// just routed straight through.
    SetFxOn { deck: DeckId, on: bool },
    /// FX "Colour" parameter (0..1). For Echo this is the feedback
    /// coefficient. For Reverb (when it lands) it'll be damping. The
    /// engine clamps; UI shows whatever label the active effect uses.
    SetFxColour { deck: DeckId, value: f32 },
    /// FX "Time" parameter (0..1, free-time effects only). Not used
    /// by tempo-synced effects like Echo — they get `SetFxBeats`
    /// instead.
    SetFxTime { deck: DeckId, value: f32 },
    /// FX wet/dry mix (0 = dry, 1 = fully wet).
    SetFxMix { deck: DeckId, value: f32 },
    /// Tempo-synced delay length in beats (typical values 0.25, 0.5,
    /// 1, 2). The engine converts to samples using the deck's
    /// *effective* BPM so the delay tracks tempo nudges.
    SetFxBeats { deck: DeckId, beats: f32 },
    /// Set the loop IN point to the deck's current playhead, snapped to
    /// the nearest beat. Does NOT activate the loop until LoopSetOut is
    /// also called — clears any pending exit.
    LoopSetIn { deck: DeckId },
    /// Set the loop OUT point to the deck's current playhead, snapped to
    /// the nearest beat. Activates the loop if IN is already set and the
    /// resulting OUT is strictly after IN; otherwise no-op.
    LoopSetOut { deck: DeckId },
    /// Mark the active loop for graceful exit. Playback continues looping
    /// until the playhead would next reach OUT, at which point the loop
    /// is cleared and playback continues past OUT into the track. No-op
    /// if no loop is active.
    LoopExit { deck: DeckId },
    /// Halve the active loop's length: OUT := IN + (OUT - IN) / 2,
    /// snapped to nearest beat. Clamped to ≥ 1 beat. No-op if no loop.
    LoopHalve { deck: DeckId },
    /// Double the active loop's length: OUT := IN + 2 × (OUT - IN),
    /// snapped to nearest beat, clamped to remain inside the buffer.
    /// No-op if no loop.
    LoopDouble { deck: DeckId },
    /// Clear both IN and OUT, deactivating any loop. CUE returns to the
    /// normal cue_frame after this.
    LoopClear { deck: DeckId },
    /// One-shot "auto loop" of `beats` whole beats: snap IN to the
    /// nearest beat of the current playhead, OUT to the beat `beats`
    /// steps further down the analysis grid. Same end-state as
    /// LoopSetIn → LoopSetOut, just from a single button press.
    LoopAuto { deck: DeckId, beats: u32 },
}

pub type CommandProducer = rtrb::Producer<DeckCommand>;
pub type CommandConsumer = rtrb::Consumer<DeckCommand>;

pub fn channel(capacity: usize) -> (CommandProducer, CommandConsumer) {
    rtrb::RingBuffer::new(capacity)
}
