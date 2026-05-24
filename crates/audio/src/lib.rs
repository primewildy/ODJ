//! Audio engine: cpal output stream + two-deck mixer. Drains `DeckCommand`s
//! from a SPSC ring inside the audio callback. No locks, no allocations on
//! the hot path.
//!
//! Two playback modes per deck:
//! - Vinyl (default): sample-rate-aware linear interp. Pitch couples to tempo.
//! - Pitch-lock: streaming phase vocoder via [`pvoc`]. Tempo without pitch
//!   change. Enable with `DeckCommand::SetPitchLock { on: true }`.
//!
//! Multi-producer: producers share a cloneable [`Sender`]; the brief mutex
//! is producer-side only. The audio thread keeps a lock-free Consumer.
//!
//! Telemetry: per-deck atomics (`playhead`, `playing`, `speed`, `gain`,
//! `pitch_lock`) are updated once per audio callback so the UI thread can
//! poll current state without coordination.

mod eq;
mod pvoc;
use eq::Biquad;
use pvoc::PhaseVocoder;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use control::{CommandConsumer, CommandProducer, DeckCommand, DeckId, TrackAnalysis, TrackBuffer};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, Stream, StreamConfig};

const RING_CAPACITY: usize = 256;
const TARGET_BUFFER_FRAMES: u32 = 128;

/// Cheaply cloneable handle for sending commands to the audio thread.
#[derive(Clone)]
pub struct Sender {
    inner: Arc<Mutex<CommandProducer>>,
}

impl Sender {
    pub fn send(&self, cmd: DeckCommand) -> Result<()> {
        let mut p = self
            .inner
            .lock()
            .map_err(|_| anyhow!("audio command lock poisoned"))?;
        p.push(cmd).map_err(|_| anyhow!("audio command queue full"))
    }
}

/// Per-deck atomics published once per audio callback. Cloneable Arcs so
/// the UI can hold its own reference.
#[derive(Clone)]
pub struct DeckTelemetry {
    pub playhead: Arc<AtomicU64>,
    pub playing: Arc<AtomicBool>,
    pub speed: Arc<AtomicU32>,      // f32 bits
    pub gain: Arc<AtomicU32>,       // f32 bits
    pub pitch_lock: Arc<AtomicBool>,
    pub beat_align: Arc<AtomicBool>,
    pub eq_low_db: Arc<AtomicU32>,  // f32 bits
    pub eq_high_db: Arc<AtomicU32>, // f32 bits
}

impl DeckTelemetry {
    fn new() -> Self {
        Self {
            playhead: Arc::new(AtomicU64::new(0)),
            playing: Arc::new(AtomicBool::new(false)),
            speed: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
            gain: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
            // Defaults match DeckState::new defaults below.
            pitch_lock: Arc::new(AtomicBool::new(true)),
            beat_align: Arc::new(AtomicBool::new(true)),
            eq_low_db: Arc::new(AtomicU32::new(0.0_f32.to_bits())),
            eq_high_db: Arc::new(AtomicU32::new(0.0_f32.to_bits())),
        }
    }

    pub fn playhead_frames(&self) -> u64 {
        self.playhead.load(Ordering::Relaxed)
    }
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }
    pub fn current_speed(&self) -> f32 {
        f32::from_bits(self.speed.load(Ordering::Relaxed))
    }
    pub fn current_gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }
    pub fn is_pitch_locked(&self) -> bool {
        self.pitch_lock.load(Ordering::Relaxed)
    }
    pub fn is_beat_aligned(&self) -> bool {
        self.beat_align.load(Ordering::Relaxed)
    }
    pub fn current_eq_low_db(&self) -> f32 {
        f32::from_bits(self.eq_low_db.load(Ordering::Relaxed))
    }
    pub fn current_eq_high_db(&self) -> f32 {
        f32::from_bits(self.eq_high_db.load(Ordering::Relaxed))
    }
}

pub struct Engine {
    _stream: Stream,
    sender: Sender,
    deck_a_tel: DeckTelemetry,
    deck_b_tel: DeckTelemetry,
}

impl Engine {
    /// Build the engine and start the output stream.
    pub fn start(device_name: Option<&str>) -> Result<Self> {
        let host = cpal::default_host();
        let device = pick_device(&host, device_name)?;
        eprintln!(
            "audio: device = {}",
            device.name().unwrap_or_else(|_| "?".into())
        );

        let supported = device
            .default_output_config()
            .context("device has no default output config")?;
        let sample_rate = supported.sample_rate().0;
        let channels = supported.channels();
        let sample_format = supported.sample_format();
        if sample_format != SampleFormat::F32 {
            bail!("only F32 output supported in v1, got {sample_format:?}");
        }

        let config = StreamConfig {
            channels,
            sample_rate: supported.sample_rate(),
            buffer_size: BufferSize::Fixed(TARGET_BUFFER_FRAMES),
        };

        eprintln!(
            "audio: stream {} ch, {} Hz, buffer {} frames ({:.2} ms)",
            channels,
            sample_rate,
            TARGET_BUFFER_FRAMES,
            TARGET_BUFFER_FRAMES as f32 * 1000.0 / sample_rate as f32
        );

        let (prod, cons) = control::channel(RING_CAPACITY);
        let deck_a_tel = DeckTelemetry::new();
        let deck_b_tel = DeckTelemetry::new();
        let stream = build_stream(
            &device,
            &config,
            cons,
            sample_rate,
            channels as usize,
            deck_a_tel.clone(),
            deck_b_tel.clone(),
        )
        .context("building output stream")?;
        stream.play().context("starting stream")?;

        Ok(Self {
            _stream: stream,
            sender: Sender {
                inner: Arc::new(Mutex::new(prod)),
            },
            deck_a_tel,
            deck_b_tel,
        })
    }

    pub fn sender(&self) -> Sender {
        self.sender.clone()
    }

    pub fn send(&self, cmd: DeckCommand) -> Result<()> {
        self.sender.send(cmd)
    }

    pub fn telemetry(&self, deck: DeckId) -> DeckTelemetry {
        match deck {
            DeckId::A => self.deck_a_tel.clone(),
            DeckId::B => self.deck_b_tel.clone(),
        }
    }

    pub fn playhead(&self, deck: DeckId) -> u64 {
        self.telemetry(deck).playhead_frames()
    }
}

fn pick_device(host: &cpal::Host, requested: Option<&str>) -> Result<cpal::Device> {
    let mut first_working: Option<cpal::Device> = None;
    for d in host.output_devices()? {
        let name = d.name().unwrap_or_default();
        let ok = d.default_output_config().is_ok();
        if let Some(req) = requested {
            if name == req && ok {
                return Ok(d);
            }
        } else {
            if name == "pipewire" && ok {
                return Ok(d);
            }
            if ok && first_working.is_none() {
                first_working = Some(d);
            }
        }
    }
    if let Some(req) = requested {
        Err(anyhow!("no usable cpal device named {req:?}"))
    } else {
        first_working.ok_or_else(|| anyhow!("no usable output device found"))
    }
}

struct DeckState {
    buffer: Option<Arc<TrackBuffer>>,
    analysis: Option<Arc<TrackAnalysis>>,
    /// Playhead in source-frames (fractional → linear interp).
    playhead: f64,
    playing: bool,
    cue_frame: u64,
    /// True iff playback was started by a CuePress while paused.
    in_preview: bool,
    gain_linear: f32,
    /// 1.0 = native rate. Range commonly 0.92..1.08 (±8%).
    speed_ratio: f32,
    /// Temporary additive nudge to effective playback rate. 0.0 normally;
    /// non-zero while a nudge pad is held. NOT published in telemetry, so
    /// the BPM readout stays steady while you push/pull.
    nudge_offset: f32,
    /// When true, CuePress (paused branch) snaps to nearest beat.
    quantize: bool,
    /// When true, render via phase vocoder (tempo without pitch change).
    pitch_lock: bool,
    /// When true, paused→playing transitions snap playhead to align with
    /// the other deck's nearest beat (phase-align).
    beat_align: bool,
    /// Per-deck phase vocoder. Always allocated; reset on pitch_lock toggle.
    pvoc: PhaseVocoder,
    /// Stereo low-shelf and high-shelf EQ biquads.
    eq_low: Biquad,
    eq_high: Biquad,
    eq_low_db: f32,
    eq_high_db: f32,
    /// Engine sample rate (cached so EQ knob handlers can recompute coeffs).
    eq_sample_rate: f32,
}

impl DeckState {
    fn new(engine_rate: u32) -> Self {
        Self {
            buffer: None,
            analysis: None,
            playhead: 0.0,
            playing: false,
            cue_frame: 0,
            in_preview: false,
            gain_linear: 1.0,
            speed_ratio: 1.0,
            nudge_offset: 0.0,
            quantize: true,
            pitch_lock: true,
            beat_align: true,
            pvoc: PhaseVocoder::new(2),
            eq_low: Biquad::passthrough(),
            eq_high: Biquad::passthrough(),
            eq_low_db: 0.0,
            eq_high_db: 0.0,
            eq_sample_rate: engine_rate as f32,
        }
    }
}

struct Mixer {
    deck_a: DeckState,
    deck_b: DeckState,
    deck_a_tel: DeckTelemetry,
    deck_b_tel: DeckTelemetry,
    engine_sample_rate: u32,
    /// Per-callback scratch buffer (one deck at a time) for applying EQ
    /// before mixing into the shared output.
    scratch: Vec<f32>,
}

impl Mixer {
    fn apply(&mut self, cmd: DeckCommand) {
        // Sync needs to read both decks; handle separately before borrowing
        // a single deck mutably.
        if let DeckCommand::Sync { deck: which } = cmd {
            let (this_bpm, other_bpm, other_speed) = match which {
                DeckId::A => (
                    self.deck_a.analysis.as_ref().map(|a| a.bpm).unwrap_or(0.0),
                    self.deck_b.analysis.as_ref().map(|a| a.bpm).unwrap_or(0.0),
                    self.deck_b.speed_ratio,
                ),
                DeckId::B => (
                    self.deck_b.analysis.as_ref().map(|a| a.bpm).unwrap_or(0.0),
                    self.deck_a.analysis.as_ref().map(|a| a.bpm).unwrap_or(0.0),
                    self.deck_a.speed_ratio,
                ),
            };
            if this_bpm > 0.0 && other_bpm > 0.0 {
                let target =
                    (other_bpm * other_speed / this_bpm).clamp(0.92, 1.08);
                let deck_ref = match which {
                    DeckId::A => &mut self.deck_a,
                    DeckId::B => &mut self.deck_b,
                };
                deck_ref.speed_ratio = target;
            }
            return;
        }

        // Save the target id before the per-deck match (which may partially
        // move `cmd` for variants like LoadTrack). Also snapshot `playing`
        // pre-mutation so the post-apply step can detect paused→playing.
        let target_id = cmd_target(&cmd);
        let was_playing = match target_id {
            DeckId::A => self.deck_a.playing,
            DeckId::B => self.deck_b.playing,
        };

        let deck = match target_id {
            DeckId::A => &mut self.deck_a,
            DeckId::B => &mut self.deck_b,
        };
        match cmd {
            DeckCommand::LoadTrack {
                buffer, analysis, ..
            } => {
                deck.buffer = Some(buffer);
                deck.analysis = Some(analysis);
                deck.playhead = 0.0;
                deck.cue_frame = 0;
                deck.in_preview = false;
                deck.playing = false;
            }
            DeckCommand::Play(_) => {
                deck.playing = true;
                deck.in_preview = false;
            }
            DeckCommand::Pause(_) => {
                deck.playing = false;
                deck.in_preview = false;
            }
            DeckCommand::PlayToggle(_) => {
                if deck.in_preview {
                    // Pioneer "Cue Play": commit preview to normal playback.
                    deck.in_preview = false;
                } else {
                    deck.playing = !deck.playing;
                }
            }
            DeckCommand::Stop(_) => {
                deck.playing = false;
                deck.playhead = deck.cue_frame as f64;
                deck.in_preview = false;
            }
            DeckCommand::SetCue { sample_pos, .. } => {
                deck.cue_frame = sample_pos;
            }
            DeckCommand::JumpToCue(_) => {
                deck.playhead = deck.cue_frame as f64;
            }
            // Pioneer CUE state machine. With quantize on, the "set cue"
            // branch snaps the new cue to the nearest beat from analysis.
            DeckCommand::CuePress(_) => {
                if deck.playing {
                    deck.playhead = deck.cue_frame as f64;
                    deck.playing = false;
                    deck.in_preview = false;
                } else {
                    let snapped = snap_to_beat(deck);
                    deck.cue_frame = snapped;
                    deck.playhead = snapped as f64;
                    deck.playing = true;
                    deck.in_preview = true;
                }
            }
            DeckCommand::CueRelease(_) => {
                if deck.in_preview {
                    deck.playhead = deck.cue_frame as f64;
                    deck.playing = false;
                    deck.in_preview = false;
                }
            }
            DeckCommand::Seek { sample_pos, .. } => {
                deck.playhead = sample_pos as f64;
            }
            DeckCommand::SetSpeed { ratio, .. } => {
                deck.speed_ratio = ratio.clamp(0.5, 2.0);
            }
            DeckCommand::NudgeSpeed { delta, .. } => {
                deck.speed_ratio = (deck.speed_ratio + delta).clamp(0.92, 1.08);
            }
            DeckCommand::SetNudge { offset, .. } => {
                // Loose clamp: a momentary push can exceed the persistent
                // ±8% range without alarm.
                deck.nudge_offset = offset.clamp(-0.5, 0.5);
            }
            DeckCommand::SetQuantize { on, .. } => {
                deck.quantize = on;
            }
            DeckCommand::SetGain { gain, .. } => {
                deck.gain_linear = gain.clamp(0.0, 2.0);
            }
            DeckCommand::SetPitchLock { on, .. } => {
                deck.pitch_lock = on;
                // Reset PV state so a fresh switch doesn't replay stale
                // phase / overlap-add buffers.
                deck.pvoc.reset();
            }
            DeckCommand::SetEqLow { db, .. } => {
                deck.eq_low_db = db.clamp(-25.0, 6.0);
                deck.eq_low
                    .set_low_shelf(deck.eq_sample_rate, 250.0, deck.eq_low_db);
            }
            DeckCommand::SetEqHigh { db, .. } => {
                deck.eq_high_db = db.clamp(-25.0, 6.0);
                deck.eq_high
                    .set_high_shelf(deck.eq_sample_rate, 4000.0, deck.eq_high_db);
            }
            DeckCommand::SetBeatAlign { on, .. } => {
                deck.beat_align = on;
            }
            DeckCommand::Sync { .. } => unreachable!("handled above"),
        }

        // Post-apply: beat-align this deck if a paused→playing transition
        // just occurred and beat_align is on. Re-borrow decks here (the
        // single-deck mutable borrow above ended).
        if !was_playing {
            let now_playing = match target_id {
                DeckId::A => self.deck_a.playing,
                DeckId::B => self.deck_b.playing,
            };
            if now_playing {
                let (this_align, other_playing) = match target_id {
                    DeckId::A => (self.deck_a.beat_align, self.deck_b.playing),
                    DeckId::B => (self.deck_b.beat_align, self.deck_a.playing),
                };
                if this_align && other_playing {
                    let (this, other) = match target_id {
                        DeckId::A => (&mut self.deck_a, &self.deck_b),
                        DeckId::B => (&mut self.deck_b, &self.deck_a),
                    };
                    beat_align_to(this, other);
                }
            }
        }
    }

    fn render(&mut self, out: &mut [f32], out_channels: usize) {
        out.fill(0.0);
        let needed = out.len();
        if self.scratch.len() < needed {
            self.scratch.resize(needed, 0.0);
        }
        let scratch = &mut self.scratch[..needed];

        // Deck A: render → EQ → gain → mix
        scratch.fill(0.0);
        render_into(&mut self.deck_a, scratch, out_channels, self.engine_sample_rate);
        apply_eq(&mut self.deck_a, scratch, out_channels);
        let g_a = self.deck_a.gain_linear;
        for (o, s) in out.iter_mut().zip(scratch.iter()) {
            *o += *s * g_a;
        }

        // Deck B: same flow
        scratch.fill(0.0);
        render_into(&mut self.deck_b, scratch, out_channels, self.engine_sample_rate);
        apply_eq(&mut self.deck_b, scratch, out_channels);
        let g_b = self.deck_b.gain_linear;
        for (o, s) in out.iter_mut().zip(scratch.iter()) {
            *o += *s * g_b;
        }

        publish_telemetry(&self.deck_a, &self.deck_a_tel);
        publish_telemetry(&self.deck_b, &self.deck_b_tel);
    }
}

fn render_into(deck: &mut DeckState, scratch: &mut [f32], out_channels: usize, engine_rate: u32) {
    if deck.pitch_lock {
        render_deck_pv(deck, scratch, out_channels, engine_rate);
    } else {
        render_deck(deck, scratch, out_channels, engine_rate);
    }
}

fn apply_eq(deck: &mut DeckState, buf: &mut [f32], out_channels: usize) {
    let stereo = out_channels.min(2);
    for frame in buf.chunks_mut(out_channels) {
        for ch in 0..stereo {
            let mut s = frame[ch];
            s = deck.eq_low.process(ch, s);
            s = deck.eq_high.process(ch, s);
            frame[ch] = s;
        }
    }
}

fn publish_telemetry(deck: &DeckState, tel: &DeckTelemetry) {
    tel.playhead.store(deck.playhead as u64, Ordering::Relaxed);
    tel.playing.store(deck.playing, Ordering::Relaxed);
    tel.speed
        .store(deck.speed_ratio.to_bits(), Ordering::Relaxed);
    tel.gain.store(deck.gain_linear.to_bits(), Ordering::Relaxed);
    tel.pitch_lock.store(deck.pitch_lock, Ordering::Relaxed);
    tel.beat_align.store(deck.beat_align, Ordering::Relaxed);
    tel.eq_low_db
        .store(deck.eq_low_db.to_bits(), Ordering::Relaxed);
    tel.eq_high_db
        .store(deck.eq_high_db.to_bits(), Ordering::Relaxed);
}

/// Shift `this` deck's playhead so its nearest beat lines up in real time
/// with `other`'s nearest beat. No-op if either deck is missing analysis or
/// the other isn't playing.
///
/// Math: each deck's source-time phase = (source_t - nearest_beat). Convert
/// to real-time by dividing by speed_ratio (so post-Sync, real-time periods
/// match across decks). Compute the smallest shift (wrap to ±half a beat
/// period). Convert back to source-frames for `this` and update playhead +
/// cue_frame so a subsequent CueRelease returns to the aligned position.
fn beat_align_to(this: &mut DeckState, other: &DeckState) {
    let Some(this_an) = this.analysis.as_ref() else {
        return;
    };
    let Some(other_an) = other.analysis.as_ref() else {
        return;
    };
    let Some(this_buf) = this.buffer.as_ref() else {
        return;
    };
    let Some(other_buf) = other.buffer.as_ref() else {
        return;
    };
    if this_an.beat_grid.is_empty() || other_an.beat_grid.is_empty() {
        return;
    }
    if !other.playing {
        return;
    }

    let other_sr = other_buf.sample_rate as f64;
    let other_t = other.playhead / other_sr;
    let other_beat = nearest_beat_secs(other_t, &other_an.beat_grid);
    let other_real_phase = (other_t - other_beat) / other.speed_ratio as f64;

    let this_sr = this_buf.sample_rate as f64;
    let this_t = this.playhead / this_sr;
    let this_beat = nearest_beat_secs(this_t, &this_an.beat_grid);
    let this_real_phase = (this_t - this_beat) / this.speed_ratio as f64;

    let mut delta_real = other_real_phase - this_real_phase;
    // Wrap to ±half a real-time period so we make the smallest shift.
    if this_an.bpm > 0.0 && this.speed_ratio.abs() > 1e-6 {
        let real_period = 60.0 / (this_an.bpm as f64 * this.speed_ratio as f64);
        let half = real_period * 0.5;
        while delta_real > half {
            delta_real -= real_period;
        }
        while delta_real < -half {
            delta_real += real_period;
        }
    }

    let shift_source_time = delta_real * this.speed_ratio as f64;
    let shift_frames = shift_source_time * this_sr;
    let new_playhead = this.playhead + shift_frames;
    let total = this_buf.frames() as f64;
    // Only shift the playhead. cue_frame stays anchored to wherever the
    // user (or Q-quantise) put it — typically a B beat marker. CueRelease
    // returns there cleanly. The phase offset only lives in the playhead
    // during preview/play; if the user commits (Cue Play), the track
    // continues from the aligned playhead.
    if new_playhead >= 0.0 && new_playhead < total - 1.0 {
        this.playhead = new_playhead;
    }
}

/// Vinyl-mode render: linear interp from source, step accounts for both
/// sample-rate mismatch AND speed_ratio. Pitch couples to tempo.
fn render_deck(deck: &mut DeckState, out: &mut [f32], out_channels: usize, engine_rate: u32) {
    if !deck.playing {
        return;
    }
    let Some(buf) = deck.buffer.as_ref() else {
        return;
    };
    let in_channels = buf.channels as usize;
    let total_frames = buf.frames();
    if total_frames < 2 || in_channels == 0 {
        deck.playing = false;
        return;
    }
    let samples = &buf.samples;
    let effective_speed = (deck.speed_ratio + deck.nudge_offset).clamp(0.1, 4.0);
    let step = (buf.sample_rate as f64 / engine_rate as f64) * effective_speed as f64;

    for frame in out.chunks_mut(out_channels) {
        let pos_f = deck.playhead;
        let pos = pos_f as usize;
        if pos + 1 >= total_frames {
            deck.playing = false;
            break;
        }
        let t = (pos_f - pos as f64) as f32;
        let i0 = pos * in_channels;
        let i1 = i0 + in_channels;

        // Unit gain — EQ and master gain are applied in Mixer::render.
        if in_channels == 1 {
            let s = samples[i0] * (1.0 - t) + samples[i1] * t;
            for ch in frame.iter_mut() {
                *ch += s;
            }
        } else {
            let n = out_channels.min(in_channels);
            for ch in 0..n {
                let s = samples[i0 + ch] * (1.0 - t) + samples[i1 + ch] * t;
                frame[ch] += s;
            }
        }
        deck.playhead += step;
    }
}

/// Pitch-lock render: phase vocoder. Source is read at native pitch rate
/// (src_sr/eng_sr per engine input sample); the PV stretches in time so
/// tempo follows speed_ratio while pitch stays at native.
fn render_deck_pv(deck: &mut DeckState, out: &mut [f32], out_channels: usize, engine_rate: u32) {
    if !deck.playing {
        return;
    }
    let Some(buf) = deck.buffer.as_ref() else {
        return;
    };
    let in_channels = buf.channels as usize;
    let total_frames = buf.frames();
    if total_frames < 2 || in_channels == 0 {
        deck.playing = false;
        return;
    }

    // Independent Arc clone for sample reads; lets us mutably borrow deck
    // fields (playhead, pvoc, playing) without lifetime conflicts.
    let buf_arc = Arc::clone(buf);
    let samples = &buf_arc.samples[..];
    let src_step = buf_arc.sample_rate as f64 / engine_rate as f64;
    let total_out_frames = out.len() / out_channels;
    let mut written = 0;
    let speed = (deck.speed_ratio + deck.nudge_offset).clamp(0.1, 4.0);

    while written < total_out_frames {
        if deck.pvoc.ready() == 0 {
            let hop_a = deck.pvoc.next_hop_a(speed);
            // Pull hop_a engine-rate input samples from source by linear
            // interp at deck.playhead (advance at src_step per engine sample).
            let pv_channels = deck.pvoc.channels;
            let mut ended = false;
            for i in 0..hop_a {
                let pos = deck.playhead;
                let pos_i = pos as usize;
                if pos_i + 1 >= total_frames {
                    ended = true;
                    break;
                }
                let t = (pos - pos_i as f64) as f32;
                let i0 = pos_i * in_channels;
                let i1 = i0 + in_channels;
                if in_channels == 1 {
                    let s = samples[i0] * (1.0 - t) + samples[i1] * t;
                    for c in 0..pv_channels {
                        deck.pvoc.input_buf[c][i] = s;
                    }
                } else {
                    for c in 0..pv_channels {
                        let src_c = c.min(in_channels - 1);
                        let s = samples[i0 + src_c] * (1.0 - t)
                            + samples[i1 + src_c] * t;
                        deck.pvoc.input_buf[c][i] = s;
                    }
                }
                deck.playhead += src_step;
            }
            if ended {
                deck.playing = false;
                break;
            }
            deck.pvoc.process_frame(hop_a);
        }

        let want = total_out_frames - written;
        let slice = &mut out[written * out_channels..];
        // Unit gain — EQ and master gain applied in Mixer::render.
        let took = deck.pvoc.consume(slice, out_channels, 1.0, want);
        if took == 0 {
            break;
        }
        written += took;
    }
}

fn cmd_target(cmd: &DeckCommand) -> DeckId {
    match cmd {
        DeckCommand::LoadTrack { deck, .. }
        | DeckCommand::Play(deck)
        | DeckCommand::Pause(deck)
        | DeckCommand::PlayToggle(deck)
        | DeckCommand::Stop(deck)
        | DeckCommand::SetCue { deck, .. }
        | DeckCommand::JumpToCue(deck)
        | DeckCommand::CuePress(deck)
        | DeckCommand::CueRelease(deck)
        | DeckCommand::Seek { deck, .. }
        | DeckCommand::SetSpeed { deck, .. }
        | DeckCommand::NudgeSpeed { deck, .. }
        | DeckCommand::SetNudge { deck, .. }
        | DeckCommand::SetQuantize { deck, .. }
        | DeckCommand::SetGain { deck, .. }
        | DeckCommand::SetPitchLock { deck, .. }
        | DeckCommand::SetEqLow { deck, .. }
        | DeckCommand::SetEqHigh { deck, .. }
        | DeckCommand::SetBeatAlign { deck, .. }
        | DeckCommand::Sync { deck } => *deck,
    }
}

/// Return the source-frame index closest to a beat in the analysis grid,
/// or the current playhead if quantize is off / no analysis / no beats.
fn snap_to_beat(deck: &DeckState) -> u64 {
    if !deck.quantize {
        return deck.playhead as u64;
    }
    let (Some(an), Some(buf)) = (deck.analysis.as_ref(), deck.buffer.as_ref()) else {
        return deck.playhead as u64;
    };
    if an.beat_grid.is_empty() {
        return deck.playhead as u64;
    }
    let sr = buf.sample_rate as f64;
    if sr <= 0.0 {
        return deck.playhead as u64;
    }
    let t = deck.playhead / sr;
    let nearest = nearest_beat_secs(t, &an.beat_grid);
    ((nearest * sr).max(0.0)) as u64
}

fn nearest_beat_secs(t: f64, beats: &[f64]) -> f64 {
    debug_assert!(!beats.is_empty());
    match beats.binary_search_by(|b| {
        b.partial_cmp(&t).unwrap_or(std::cmp::Ordering::Equal)
    }) {
        Ok(i) => beats[i],
        Err(i) => {
            if i == 0 {
                beats[0]
            } else if i >= beats.len() {
                beats[beats.len() - 1]
            } else {
                let a = beats[i - 1];
                let b = beats[i];
                if (t - a).abs() <= (t - b).abs() { a } else { b }
            }
        }
    }
}

fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    mut consumer: CommandConsumer,
    sample_rate: u32,
    out_channels: usize,
    deck_a_tel: DeckTelemetry,
    deck_b_tel: DeckTelemetry,
) -> Result<Stream> {
    let mut mixer = Mixer {
        deck_a: DeckState::new(sample_rate),
        deck_b: DeckState::new(sample_rate),
        deck_a_tel,
        deck_b_tel,
        engine_sample_rate: sample_rate,
        scratch: Vec::with_capacity(4096),
    };
    let err_fn = |e| eprintln!("audio: stream error: {e}");
    device
        .build_output_stream(
            config,
            move |out: &mut [f32], _info| {
                while let Ok(cmd) = consumer.pop() {
                    mixer.apply(cmd);
                }
                mixer.render(out, out_channels);
            },
            err_fn,
            None,
        )
        .context("build_output_stream failed")
}
