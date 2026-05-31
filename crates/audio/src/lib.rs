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
// 128 frames (2.9 ms @ 44.1 k) is the latency sweet-spot but it's tight
// when running two ALSA streams (master + cue) under PipeWire — one can
// starve the other and trigger snd_pcm_recover underruns. 256 frames
// (5.8 ms) gives the scheduler enough headroom while staying well under
// the ~10 ms threshold where DJ feel starts to suffer.
const TARGET_BUFFER_FRAMES: u32 = 256;
/// Cue audio ring (master callback writes, cue callback reads).
/// Stereo f32 — sized for ~23 ms of audio at 44.1k (4096 / 2 / 44100).
const CUE_RING_CAPACITY: usize = 4096;

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
    pub eq_mid_db: Arc<AtomicU32>,  // f32 bits
    pub eq_high_db: Arc<AtomicU32>, // f32 bits
    pub cue_on: Arc<AtomicBool>,
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
            eq_mid_db: Arc::new(AtomicU32::new(0.0_f32.to_bits())),
            eq_high_db: Arc::new(AtomicU32::new(0.0_f32.to_bits())),
            cue_on: Arc::new(AtomicBool::new(false)),
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
    pub fn is_cue_on(&self) -> bool {
        self.cue_on.load(Ordering::Relaxed)
    }
    pub fn current_eq_low_db(&self) -> f32 {
        f32::from_bits(self.eq_low_db.load(Ordering::Relaxed))
    }
    pub fn current_eq_mid_db(&self) -> f32 {
        f32::from_bits(self.eq_mid_db.load(Ordering::Relaxed))
    }
    pub fn current_eq_high_db(&self) -> f32 {
        f32::from_bits(self.eq_high_db.load(Ordering::Relaxed))
    }
}

pub struct Engine {
    _stream: Stream,
    _cue_stream: Option<Stream>,
    sender: Sender,
    deck_a_tel: DeckTelemetry,
    deck_b_tel: DeckTelemetry,
}

impl Engine {
    /// Build the engine and start the output stream(s).
    ///
    /// `device_name`: master / main output. Picks `pipewire` if None.
    /// `cue_device_name`: optional secondary output for PFL/cue
    /// monitoring. If None, no cue stream is opened and `SetCueOn`
    /// commands have no audible effect.
    pub fn start(
        device_name: Option<&str>,
        cue_device_name: Option<&str>,
    ) -> Result<Self> {
        // Label this process's first ALSA-via-PipeWire stream as "DJ Master"
        // so pw-top / pavucontrol shows distinct names for the two streams.
        // The pipewire-alsa plugin reads `PIPEWIRE_PROPS` (SPA-JSON, multiple
        // props in one string) at PCM-open time. We set application.name,
        // node.description and media.name so whichever one pw-top / your
        // mixer GUI picks for display, it lands on our chosen label.
        unsafe {
            std::env::set_var(
                "PIPEWIRE_PROPS",
                r#"{ application.name = "DJ Master" node.description = "DJ Master" media.name = "DJ Master" }"#,
            );
        }

        let host = cpal::default_host();
        let device = pick_device(&host, device_name)?;
        eprintln!(
            "audio: master = {}",
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
            "audio: master {} ch, {} Hz, buffer {} frames ({:.2} ms)",
            channels,
            sample_rate,
            TARGET_BUFFER_FRAMES,
            TARGET_BUFFER_FRAMES as f32 * 1000.0 / sample_rate as f32
        );

        // Optionally open the cue device. We open it first (so we can hand
        // its consumer half into the master callback), then build streams.
        //
        // cpal's ALSA enumeration on PipeWire systems doesn't always surface
        // every sink (USB DACs in particular). If the user-supplied name
        // doesn't match a cpal device, we fall back to opening the
        // `pipewire` cpal device and routing it to a specific PipeWire
        // sink via the PIPEWIRE_NODE env var (set just around the cue
        // stream's open, then restored, so master keeps its own routing).
        let cue_setup = if let Some(name) = cue_device_name {
            let (cue_dev, cue_pw_node): (cpal::Device, Option<String>) =
                match pick_device(&host, Some(name)) {
                    Ok(d) => (d, None),
                    Err(orig_err) => {
                        if let Some(pw_node) = find_pipewire_sink(name) {
                            let pw_dev = pick_device(&host, Some("pipewire"))
                                .context("the 'pipewire' cpal device is needed for PipeWire-routed cue but wasn't found")?;
                            eprintln!(
                                "audio: cue routed via PipeWire node {pw_node:?}"
                            );
                            (pw_dev, Some(pw_node))
                        } else {
                            return Err(orig_err).with_context(|| {
                                format!("cue device {name:?} not found in cpal enumeration or as a PipeWire sink")
                            });
                        }
                    }
                };
            let cue_cfg = cue_dev
                .default_output_config()
                .context("cue device has no default output config")?;
            if cue_cfg.sample_format() != SampleFormat::F32 {
                bail!(
                    "cue device sample format must be F32, got {:?}",
                    cue_cfg.sample_format()
                );
            }
            let cue_channels = cue_cfg.channels();
            let cue_rate = cue_cfg.sample_rate().0;
            eprintln!(
                "audio: cue    = {} ({} ch, {} Hz)",
                cue_dev.name().unwrap_or_else(|_| "?".into()),
                cue_channels,
                cue_rate,
            );
            if cue_rate != sample_rate || cue_channels != channels {
                eprintln!(
                    "audio: WARNING cue rate/channels differ from master \
                     (cue {} Hz/{} ch vs master {} Hz/{} ch). The cue \
                     stream will play at its own rate which may sound \
                     wrong.",
                    cue_rate, cue_channels, sample_rate, channels,
                );
            }
            let (prod, cons) =
                rtrb::RingBuffer::<f32>::new(CUE_RING_CAPACITY);
            Some((cue_dev, cue_cfg, cue_pw_node, prod, cons))
        } else {
            None
        };

        let (cue_producer, cue_consumer_and_device) = match cue_setup {
            Some((d, c, pw, p, cons)) => (Some(p), Some((d, c, pw, cons))),
            None => (None, None),
        };

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
            cue_producer,
        )
        .context("building master output stream")?;
        stream.play().context("starting master stream")?;

        let cue_stream = if let Some((cd, ccfg, cue_pw_node, ccons)) = cue_consumer_and_device {
            // cpal's ALSA play() is async — the audio thread does the actual
            // snd_pcm_start, which is when pipewire-alsa reads PIPEWIRE_NODE
            // and connects the stream to a target sink. So before we change
            // PIPEWIRE_NODE for the cue stream, give master's audio thread
            // a brief moment to finish connecting — otherwise it can read
            // the env var we set for cue and end up on the cue sink too.
            // Also leave PIPEWIRE_NODE set after; master is already connected
            // by this point, and we don't open more streams.
            std::thread::sleep(std::time::Duration::from_millis(200));
            unsafe {
                // Rename for pw-top / pavucontrol so the two dj streams are
                // distinguishable. This applies to the next stream opened
                // (the cue, just below).
                std::env::set_var(
                    "PIPEWIRE_PROPS",
                    r#"{ application.name = "DJ Cue" node.description = "DJ Cue" media.name = "DJ Cue" }"#,
                );
                if let Some(node) = &cue_pw_node {
                    std::env::set_var("PIPEWIRE_NODE", node);
                }
            }
            let s = build_cue_stream(&cd, &ccfg, ccons)
                .context("building cue output stream")?;
            s.play().context("starting cue stream")?;
            Some(s)
        } else {
            None
        };

        Ok(Self {
            _stream: stream,
            _cue_stream: cue_stream,
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
    let all: Vec<cpal::Device> = host.output_devices()?.collect();
    let names: Vec<String> = all.iter().map(|d| d.name().unwrap_or_default()).collect();
    let usable: Vec<bool> = all.iter().map(|d| d.default_output_config().is_ok()).collect();

    // Match: exact first, then case-insensitive substring. For the default
    // (no request) prefer "pipewire" — it's the right pick on PipeWire
    // systems — and fall back to the first usable device.
    let idx = match requested {
        Some(req) => {
            let req_l = req.to_lowercase();
            (0..all.len()).find(|&i| usable[i] && names[i] == req).or_else(|| {
                (0..all.len()).find(|&i| usable[i] && names[i].to_lowercase().contains(&req_l))
            })
        }
        None => (0..all.len())
            .find(|&i| usable[i] && names[i] == "pipewire")
            .or_else(|| (0..all.len()).find(|&i| usable[i])),
    };

    if let Some(i) = idx {
        return Ok(all.into_iter().nth(i).unwrap());
    }

    let listing: String = names
        .iter()
        .enumerate()
        .map(|(i, n)| format!("  - {n}{}", if usable[i] { "" } else { " (unusable)" }))
        .collect::<Vec<_>>()
        .join("\n");
    let req = requested.unwrap_or("<default>");
    Err(anyhow!(
        "no usable cpal output device matching {req:?}.\navailable devices:\n{listing}"
    ))
}

/// Best-effort: resolve a user-supplied name to a PipeWire sink node
/// (e.g. "alsa_output.usb-..."). Used as a fallback when cpal's ALSA
/// enumeration doesn't surface a device but PipeWire knows about it —
/// common for USB DACs on PipeWire systems. Shells out to `pactl`; if
/// pactl isn't installed or no sink matches the substring, returns None.
fn find_pipewire_sink(query: &str) -> Option<String> {
    // Normalise so a friendly "KT USB Audio" matches the underscored
    // PipeWire node name "alsa_output.usb-KTMicro_KT_USB_Audio_...".
    fn norm(s: &str) -> String {
        s.to_lowercase().replace(['_', '-'], " ")
    }
    let out = std::process::Command::new("pactl")
        .args(["list", "short", "sinks"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let q = norm(query);
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        // Tab-separated: "<id>\t<node_name>\t<driver>\t<format>\t<state>".
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() >= 2 {
            let name = cols[1];
            if norm(name).contains(&q) {
                return Some(name.to_string());
            }
        }
    }
    None
}

struct DeckState {
    buffer: Option<Arc<TrackBuffer>>,
    analysis: Option<Arc<TrackAnalysis>>,
    /// Playhead in source-frames (fractional → linear interp).
    playhead: f64,
    playing: bool,
    cue_frame: u64,
    /// True if this deck's pre-fader signal should be mixed into the cue
    /// bus. Independent per deck; multiple decks can be cued at once.
    cue_on: bool,
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
    /// Stereo low-shelf, mid-peaking, and high-shelf EQ biquads (in series).
    eq_low: Biquad,
    eq_mid: Biquad,
    eq_high: Biquad,
    eq_low_db: f32,
    eq_mid_db: f32,
    eq_high_db: f32,
    /// Engine sample rate (cached so EQ knob handlers can recompute coeffs).
    eq_sample_rate: f32,
    /// Output amplitude envelope, 0.0..1.0. Ramps toward 1.0 while
    /// `playing` is true and toward 0.0 when paused. Prevents the
    /// audible click on play-start / pause.
    play_envelope: f32,
}

impl DeckState {
    fn new(engine_rate: u32) -> Self {
        Self {
            buffer: None,
            analysis: None,
            playhead: 0.0,
            playing: false,
            cue_frame: 0,
            cue_on: false,
            in_preview: false,
            gain_linear: 1.0,
            speed_ratio: 1.0,
            nudge_offset: 0.0,
            quantize: true,
            pitch_lock: true,
            beat_align: true,
            pvoc: PhaseVocoder::new(2),
            eq_low: Biquad::passthrough(),
            eq_mid: Biquad::passthrough(),
            eq_high: Biquad::passthrough(),
            eq_low_db: 0.0,
            eq_mid_db: 0.0,
            eq_high_db: 0.0,
            eq_sample_rate: engine_rate as f32,
            play_envelope: 0.0,
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
    /// Per-callback accumulator for the cue mix (sum of pre-fader signals
    /// from decks with `cue_on`).
    cue_scratch: Vec<f32>,
    /// Lock-free SPSC producer to the cue stream's callback. None if no
    /// cue device was configured at engine start.
    cue_producer: Option<rtrb::Producer<f32>>,
    /// Linear gain on the headphone bus. 1.0 = unity.
    cue_gain: f32,
    /// CUE↔MASTER blend in the headphones. 0 = pure master, 1 = pure cue.
    cue_mix: f32,
}

impl Mixer {
    fn apply(&mut self, cmd: DeckCommand) {
        // Global headphone-bus commands (no deck field).
        if let DeckCommand::SetCueGain { gain } = cmd {
            self.cue_gain = gain.clamp(0.0, 2.0);
            return;
        }
        if let DeckCommand::SetCueMix { mix } = cmd {
            self.cue_mix = mix.clamp(0.0, 1.0);
            return;
        }

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
                // Snap the playhead + cue point to the first detected
                // downbeat. Most tracks have a second or three of
                // silence / room tone before the first kick lands;
                // dropping it as the natural start means Play / CUE
                // both behave the way a DJ expects without any manual
                // seeking. Falls back to t=0 when we don't yet have
                // downbeats (v1 cache or no model installed).
                let first_db_sample = analysis
                    .downbeats
                    .first()
                    .and_then(|&i| analysis.beat_grid.get(i as usize).copied())
                    .map(|t| t * analysis.sample_rate as f64)
                    .filter(|s| s.is_finite() && *s >= 0.0)
                    .unwrap_or(0.0);
                deck.analysis = Some(analysis);
                deck.playhead = first_db_sample;
                deck.cue_frame = first_db_sample as u64;
                deck.in_preview = false;
                // Keep the deck's play state across a track swap — if the
                // user was playing and loads a new track, it starts right
                // away. Reset only when paused.
                deck.playing = was_playing;
                // Reset envelope so a fresh load doesn't carry over
                // residual fade-out from the previous track.
                deck.play_envelope = 0.0;
            }
            DeckCommand::UpdateAnalysis { analysis, .. } => {
                // Slow-path arrival from the async analyser. Swap in
                // the refined beat grid + downbeats without touching
                // the playhead / play state — the user may already
                // have started playing the track. If the cue point
                // is still at t=0 (i.e. the deck loaded without
                // downbeats), snap it to the first downbeat now so
                // CUE works correctly.
                if deck.cue_frame == 0 {
                    if let Some(&i) = analysis.downbeats.first() {
                        if let Some(&t) = analysis.beat_grid.get(i as usize) {
                            let s = t * analysis.sample_rate as f64;
                            if s.is_finite() && s >= 0.0 {
                                deck.cue_frame = s as u64;
                                if !deck.playing && deck.playhead == 0.0 {
                                    deck.playhead = s;
                                }
                            }
                        }
                    }
                }
                deck.analysis = Some(analysis);
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
                // Clamp to the loaded buffer's range so a wild seek (e.g.,
                // from jog-scrub or a click past the waveform) leaves the
                // deck at a valid position rather than playing into garbage.
                let pos = if let Some(buf) = deck.buffer.as_ref() {
                    let max = (buf.frames() as u64).saturating_sub(1);
                    sample_pos.min(max)
                } else {
                    sample_pos
                };
                deck.playhead = pos as f64;
            }
            DeckCommand::SetSpeed { ratio, .. } => {
                deck.speed_ratio = ratio.clamp(0.5, 2.0);
            }
            DeckCommand::NudgeSpeed { delta, .. } => {
                deck.speed_ratio = (deck.speed_ratio + delta).clamp(0.92, 1.08);
            }
            DeckCommand::SetNudge { offset, .. } => {
                // Allow large magnitudes so jog-scrub can drive effective
                // playback rate to several times normal (and negative for
                // reverse). Vinyl render clamps the final effective speed
                // again, so this isn't unbounded.
                deck.nudge_offset = offset.clamp(-10.0, 10.0);
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
            DeckCommand::SetEqMid { db, .. } => {
                deck.eq_mid_db = db.clamp(-25.0, 6.0);
                deck.eq_mid
                    .set_peaking(deck.eq_sample_rate, 1000.0, 0.7, deck.eq_mid_db);
            }
            DeckCommand::SetEqHigh { db, .. } => {
                deck.eq_high_db = db.clamp(-25.0, 6.0);
                deck.eq_high
                    .set_high_shelf(deck.eq_sample_rate, 4000.0, deck.eq_high_db);
            }
            DeckCommand::SetBeatAlign { on, .. } => {
                deck.beat_align = on;
            }
            DeckCommand::SetCueOn { on, .. } => {
                deck.cue_on = on;
            }
            DeckCommand::Sync { .. } => unreachable!("handled above"),
            DeckCommand::SetCueGain { .. } => unreachable!("handled above"),
            DeckCommand::SetCueMix { .. } => unreachable!("handled above"),
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
        if self.cue_producer.is_some() && self.cue_scratch.len() < needed {
            self.cue_scratch.resize(needed, 0.0);
        }
        let scratch_a = &mut self.scratch[..needed];
        if self.cue_producer.is_some() {
            self.cue_scratch[..needed].fill(0.0);
        }

        // Deck A: render → EQ → fade envelope → (cue tap) → gain → master
        scratch_a.fill(0.0);
        render_into(&mut self.deck_a, scratch_a, out_channels, self.engine_sample_rate);
        apply_eq(&mut self.deck_a, scratch_a, out_channels);
        apply_play_envelope(
            &mut self.deck_a,
            scratch_a,
            out_channels,
            self.engine_sample_rate,
        );
        if self.cue_producer.is_some() && self.deck_a.cue_on {
            for (c, s) in self.cue_scratch[..needed].iter_mut().zip(scratch_a.iter()) {
                *c += *s;
            }
        }
        let g_a = self.deck_a.gain_linear;
        for (o, s) in out.iter_mut().zip(scratch_a.iter()) {
            *o += *s * g_a;
        }

        // Deck B: same flow. (Re-borrow `scratch` since we used `scratch_a`
        // above; same underlying buffer.)
        let scratch_b = &mut self.scratch[..needed];
        scratch_b.fill(0.0);
        render_into(&mut self.deck_b, scratch_b, out_channels, self.engine_sample_rate);
        apply_eq(&mut self.deck_b, scratch_b, out_channels);
        apply_play_envelope(
            &mut self.deck_b,
            scratch_b,
            out_channels,
            self.engine_sample_rate,
        );
        if self.cue_producer.is_some() && self.deck_b.cue_on {
            for (c, s) in self.cue_scratch[..needed].iter_mut().zip(scratch_b.iter()) {
                *c += *s;
            }
        }
        let g_b = self.deck_b.gain_linear;
        for (o, s) in out.iter_mut().zip(scratch_b.iter()) {
            *o += *s * g_b;
        }

        // Headphone bus: blend the cued-decks sum with the master mix, then
        // apply the global headphone gain. cue_mix = 0 → pure master, 1 →
        // pure cue. Done in-place on cue_scratch before pushing to the ring.
        if self.cue_producer.is_some() {
            let g = self.cue_gain;
            let m = self.cue_mix;
            let one_m = 1.0 - m;
            for (c, o) in self.cue_scratch[..needed].iter_mut().zip(out.iter()) {
                *c = g * (m * *c + one_m * *o);
            }
        }

        // Push the cue mix to the secondary stream's ring buffer. If the
        // ring is full (cue stream lagging) we drop the excess — silently
        // accepting that cue will fall behind master rather than blocking
        // the audio thread.
        if let Some(prod) = self.cue_producer.as_mut() {
            for s in self.cue_scratch[..needed].iter() {
                if prod.push(*s).is_err() {
                    break;
                }
            }
        }

        publish_telemetry(&self.deck_a, &self.deck_a_tel);
        publish_telemetry(&self.deck_b, &self.deck_b_tel);
    }
}

fn render_into(deck: &mut DeckState, scratch: &mut [f32], out_channels: usize, engine_rate: u32) {
    // PV path doesn't support reverse playback (FFT analysis hop is
    // positive by construction). If the user nudged effective speed
    // non-positive — e.g., backward jog scrub — fall through to the
    // vinyl renderer for this callback. PV's internal phase/OLA state
    // persists across the bypass, so play resumes cleanly when speed
    // goes positive again.
    let effective_speed = deck.speed_ratio + deck.nudge_offset;
    if deck.pitch_lock && effective_speed > 0.0 {
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
            s = deck.eq_mid.process(ch, s);
            s = deck.eq_high.process(ch, s);
            frame[ch] = s;
        }
    }
}

/// Apply the deck's play envelope per-sample. Ramps the envelope toward
/// 1.0 while `playing` is set, toward 0.0 when not, over FADE_SECS of
/// real time. Multiplies the scratch buffer by the (instantaneous)
/// envelope value — prevents the click that an abrupt 0→full or full→0
/// transition would produce.
fn apply_play_envelope(
    deck: &mut DeckState,
    buf: &mut [f32],
    out_channels: usize,
    engine_rate: u32,
) {
    const FADE_SECS: f32 = 0.005; // 5 ms ramp
    let target = if deck.playing { 1.0_f32 } else { 0.0_f32 };
    let step = 1.0 / (engine_rate as f32 * FADE_SECS);
    for frame in buf.chunks_mut(out_channels) {
        if deck.play_envelope < target {
            deck.play_envelope = (deck.play_envelope + step).min(target);
        } else if deck.play_envelope > target {
            deck.play_envelope = (deck.play_envelope - step).max(target);
        }
        let e = deck.play_envelope;
        for ch in frame.iter_mut() {
            *ch *= e;
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
    tel.eq_mid_db
        .store(deck.eq_mid_db.to_bits(), Ordering::Relaxed);
    tel.eq_high_db
        .store(deck.eq_high_db.to_bits(), Ordering::Relaxed);
    tel.cue_on.store(deck.cue_on, Ordering::Relaxed);
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
    // Keep rendering for a few samples after `playing` flips false so
    // `apply_play_envelope` can ramp the output down to 0 (click-free
    // pause). Once the envelope has decayed, bail.
    if !deck.playing && deck.play_envelope <= 0.0 {
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
    // Allow negative effective_speed so the deck can play backwards (used
    // by paused-jog scrub). Magnitude clamped at 4× either direction.
    let effective_speed = (deck.speed_ratio + deck.nudge_offset).clamp(-4.0, 4.0);
    let step = (buf.sample_rate as f64 / engine_rate as f64) * effective_speed as f64;

    for frame in out.chunks_mut(out_channels) {
        let pos_f = deck.playhead;
        // Bounds: stop at either end of the track. Lower bound checks
        // pos_f < 0.0 explicitly because `pos_f as usize` is undefined
        // for negative floats on some targets.
        if pos_f < 0.0 {
            deck.playing = false;
            deck.playhead = 0.0;
            break;
        }
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
    // Same rationale as render_deck: keep producing samples while the
    // envelope is still ramping down so the fade-out is audible.
    if !deck.playing && deck.play_envelope <= 0.0 {
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
        | DeckCommand::UpdateAnalysis { deck, .. }
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
        | DeckCommand::SetEqMid { deck, .. }
        | DeckCommand::SetEqHigh { deck, .. }
        | DeckCommand::SetBeatAlign { deck, .. }
        | DeckCommand::SetCueOn { deck, .. }
        | DeckCommand::Sync { deck } => *deck,
        DeckCommand::SetCueGain { .. } | DeckCommand::SetCueMix { .. } => {
            unreachable!("global headphone-bus commands have no deck target — handled in apply()")
        }
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
    cue_producer: Option<rtrb::Producer<f32>>,
) -> Result<Stream> {
    let mut mixer = Mixer {
        deck_a: DeckState::new(sample_rate),
        deck_b: DeckState::new(sample_rate),
        deck_a_tel,
        deck_b_tel,
        engine_sample_rate: sample_rate,
        scratch: Vec::with_capacity(4096),
        cue_scratch: Vec::with_capacity(4096),
        cue_producer,
        cue_gain: 0.15,
        cue_mix: 1.0,
    };
    let err_fn = |e| eprintln!("audio: master stream error: {e}");
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

/// Cue stream callback: pop interleaved stereo samples from the SPSC
/// ring and write to `out`. Underrun → silence (0.0). Trivial; no engine
/// state lives here.
fn build_cue_stream(
    device: &cpal::Device,
    supported: &cpal::SupportedStreamConfig,
    mut consumer: rtrb::Consumer<f32>,
) -> Result<Stream> {
    let config = StreamConfig {
        channels: supported.channels(),
        sample_rate: supported.sample_rate(),
        buffer_size: BufferSize::Fixed(TARGET_BUFFER_FRAMES),
    };
    let err_fn = |e| eprintln!("audio: cue stream error: {e}");
    device
        .build_output_stream(
            &config,
            move |out: &mut [f32], _info| {
                for sample in out.iter_mut() {
                    *sample = consumer.pop().unwrap_or(0.0);
                }
            },
            err_fn,
            None,
        )
        .context("cue build_output_stream failed")
}
