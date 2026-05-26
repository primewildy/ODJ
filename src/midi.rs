//! MIDI input: forwards LPD8 pad/knob events to the audio engine.
//!
//! Default LPD8 PROG-1 mapping (factory defaults). Physical pad layout:
//!
//!     [5][6][7][8]   ← top row    = Deck A
//!     [1][2][3][4]   ← bottom row = Deck B
//!
//! Notes (PROG 1 default): pads 1..8 = MIDI notes 36..43.
//! CCs   (PROG 1 default): knobs 1..8 = MIDI CC 1..8.
//!
//! - Pad 1 (note 36): Deck B Play/Pause toggle
//! - Pad 2 (note 37): Deck B CUE (Pioneer state machine)
//! - Pad 5 (note 40): Deck A Play/Pause toggle
//! - Pad 6 (note 41): Deck A CUE (Pioneer state machine)
//! - K1   (CC 1):     Deck A pitch (0..127 → 0.92..1.08, CC 64 = 1.0)
//! - K2   (CC 2):     Deck A gain  (0..127 → 0..1.0)
//! - K3   (CC 3):     Deck A high shelf (0..127 → -25..+6 dB, CC 64 = 0 dB)
//! - K4   (CC 4):     Deck A low shelf  (0..127 → -25..+6 dB, CC 64 = 0 dB)
//! - K5   (CC 5):     Deck B pitch
//! - K6   (CC 6):     Deck B gain
//! - K7   (CC 7):     Deck B high shelf
//! - K8   (CC 8):     Deck B low shelf
//!
//! - Pad 3 (note 38): Deck B pull (slower while held — vinyl push/pull)
//! - Pad 4 (note 39): Deck B push (faster while held)
//! - Pad 7 (note 42): Deck A pull (slower while held)
//! - Pad 8 (note 43): Deck A push (faster while held)
//!
//! - CC 16:           Deck A jog encoder (relative, 64 = 0, 64±delta).
//!                    Sets a temporary nudge proportional to spin speed;
//!                    a watchdog thread clears the nudge ~50 ms after the
//!                    last CC 16 message (i.e. when you let go of the
//!                    wheel) so the deck returns to its set tempo.
//!
//! Note release brings the deck back to its set tempo. The BPM readout
//! doesn't move while nudge is held — base speed is what's published.
//! (Sync via the UI button only.)

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use audio::{DeckTelemetry, Sender};
use control::{DeckCommand, DeckId};
use midir::{Ignore, MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};

/// Tunables for the encoder → nudge mapping. SCALE is offset per quadrature
/// tick (firmware sends accumulated ticks per 5 ms scan). TIMEOUT_MS is how
/// long to wait without a CC 16 message before clearing the nudge to 0.
const JOG_SCALE: f32 = 0.001;
const JOG_TIMEOUT_MS: u64 = 60;
/// Effective-playback-rate per tick when scrubbing a paused deck. At a
/// brisk spin (≈30 ticks per 5 ms scan) this gives roughly 2.4× normal
/// playback rate.
const JOG_SCRUB_RATE_PER_TICK: f64 = 0.08;
/// CC number the firmware emits for the Deck A jog. Mirrored in
/// `hardware/firmware/src/main.c::ENCODER_CC`.
const JOG_CC_DECK_A: u8 = 16;

/// Shared between the MIDI callback (writes timestamp on each CC 16) and
/// the watchdog thread (reads it; sends `SetNudge(0)` once the encoder
/// has been quiet long enough). Also carries the per-deck telemetry so
/// the jog handler can branch on playing-vs-paused without re-plumbing,
/// and a flag for "currently scrubbing" so the watchdog can re-pause.
struct JogState {
    /// Microseconds-since-epoch of the last CC 16 message. 0 = none / cleared.
    deck_a_last_us: AtomicU64,
    /// True while we're in "paused-deck scrub" mode — we forced the deck
    /// into playback to produce audio during scrubbing. Watchdog reverses
    /// this on timeout.
    deck_a_scrubbing: AtomicBool,
    deck_a_tel: DeckTelemetry,
}

static JOG_EPOCH: OnceLock<Instant> = OnceLock::new();
static JOG_STATE: OnceLock<Arc<JogState>> = OnceLock::new();

/// CCs whose 0-127 value should be inverted, from controls.toml. Lets
/// backward-wired faders/pots be flipped without a recompile. Set once
/// at start(); empty if no config.
static INVERT_CC: OnceLock<HashSet<u8>> = OnceLock::new();

fn micros_since_epoch() -> u64 {
    let epoch = *JOG_EPOCH.get_or_init(Instant::now);
    Instant::now().saturating_duration_since(epoch).as_micros() as u64
}

pub struct MidiThread {
    _conn: MidiInputConnection<()>,
    _out: Option<Arc<Mutex<MidiOutputConnection>>>,
    pub port_name: String,
}

/// MIDI notes the host emits *back* to the device to drive button LEDs.
/// Mirrors the firmware's `handle_note` mapping.
const LED_NOTE_DECK_A_PLAY: u8 = 40;

pub fn start(
    port_filter: &str,
    sender: Sender,
    deck_a_tel: DeckTelemetry,
    invert_cc: Vec<u8>,
) -> Result<MidiThread> {
    let _ = INVERT_CC.set(invert_cc.into_iter().collect());
    // Lazy init the jog state + spawn the watchdog. Safe to call repeatedly;
    // OnceLock guards both. The watchdog runs forever and is detached.
    let jog_state = JOG_STATE
        .get_or_init(|| {
            let state = Arc::new(JogState {
                deck_a_last_us: AtomicU64::new(0),
                deck_a_scrubbing: AtomicBool::new(false),
                deck_a_tel: deck_a_tel.clone(),
            });
            let watch_state = Arc::clone(&state);
            let watch_sender = sender.clone();
            thread::Builder::new()
                .name("dj-jog-watchdog".into())
                .spawn(move || jog_watchdog(watch_state, watch_sender))
                .expect("spawn jog watchdog");
            state
        })
        .clone();
    let _ = jog_state; // (kept for symmetry; on_cc reaches the state via the static)
    let mut input = MidiInput::new("dj-midi")?;
    input.ignore(Ignore::None);

    let ports = input.ports();
    if ports.is_empty() {
        return Err(anyhow!("no MIDI input ports found"));
    }
    let port = ports
        .iter()
        .find(|p| {
            input
                .port_name(p)
                .map(|n| n.contains(port_filter))
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            let names: Vec<_> = ports
                .iter()
                .filter_map(|p| input.port_name(p).ok())
                .collect();
            anyhow!("no MIDI port matched {port_filter:?}. seen: {names:?}")
        })?;
    let port_name = input.port_name(port)?;

    let conn = input
        .connect(
            port,
            "dj-midi-in",
            move |_stamp, msg, _| {
                handle_message(msg, &sender);
            },
            (),
        )
        .map_err(|e| anyhow!("midi connect failed: {e}"))
        .context("midir connect")?;

    // Open an output connection to the same device for LED feedback.
    // If the device doesn't have a matching output port (e.g., LPD8 has
    // outputs but a future receive-only controller won't), we skip silently
    // — the rest still works.
    let out_conn = open_output_for(&port_name);
    if out_conn.is_some() {
        let watcher_out = out_conn.clone().unwrap();
        thread::Builder::new()
            .name("dj-led-watcher".into())
            .spawn(move || led_watcher(watcher_out, deck_a_tel))
            .ok();
    }

    Ok(MidiThread {
        _conn: conn,
        _out: out_conn,
        port_name,
    })
}

fn open_output_for(port_name: &str) -> Option<Arc<Mutex<MidiOutputConnection>>> {
    let out = MidiOutput::new("dj-midi-out").ok()?;
    let ports = out.ports();
    let port = ports
        .iter()
        .find(|p| out.port_name(p).map(|n| n == port_name).unwrap_or(false))
        .or_else(|| {
            // Fallback: same substring match as the input opener used.
            ports.iter().find(|p| {
                out.port_name(p)
                    .map(|n| port_name.contains(&n) || n.contains(port_name))
                    .unwrap_or(false)
            })
        })?;
    let conn = out.connect(port, "dj-midi-out").ok()?;
    Some(Arc::new(Mutex::new(conn)))
}

/// Polls Deck A's `playing` AtomicBool ~30 Hz; when it changes, emits
/// note_on / note_off so the firmware can light the Play LED. Cheap to
/// run — only sends on edges.
///
/// During *scrub*, the deck is technically `playing` (we force it so
/// audio comes out), but it's a transient navigation mode, not real
/// playback — so the LED stays dark.
fn led_watcher(out: Arc<Mutex<MidiOutputConnection>>, deck_a_tel: DeckTelemetry) {
    let mut last_playing: Option<bool> = None;
    let tick = Duration::from_millis(33);
    loop {
        thread::sleep(tick);
        let scrubbing = JOG_STATE
            .get()
            .map(|s| s.deck_a_scrubbing.load(Ordering::Relaxed))
            .unwrap_or(false);
        let playing = deck_a_tel.is_playing() && !scrubbing;
        if Some(playing) == last_playing {
            continue;
        }
        last_playing = Some(playing);
        let msg = if playing {
            [0x90, LED_NOTE_DECK_A_PLAY, 0x7F]
        } else {
            [0x80, LED_NOTE_DECK_A_PLAY, 0x00]
        };
        if let Ok(mut o) = out.lock() {
            let _ = o.send(&msg);
        }
    }
}

fn handle_message(msg: &[u8], sender: &Sender) {
    if msg.is_empty() {
        return;
    }
    let status = msg[0] & 0xF0;
    let channel = msg[0] & 0x0F;

    match status {
        0x90 if msg.len() >= 3 => {
            let note = msg[1];
            let vel = msg[2];
            log(&format!("midi: note_on  ch{} n{:>3} v{:>3}", channel, note, vel));
            if vel == 0 {
                note_off(note, sender);
            } else {
                note_on(note, sender);
            }
        }
        0x80 if msg.len() >= 3 => {
            let note = msg[1];
            log(&format!("midi: note_off ch{} n{:>3}", channel, note));
            note_off(note, sender);
        }
        0xB0 if msg.len() >= 3 => {
            let cc = msg[1];
            let val = msg[2];
            log(&format!("midi: cc       ch{} cc{:>3} v{:>3}", channel, cc, val));
            on_cc(cc, val, sender);
        }
        _ => {
            log(&format!("midi: ?        {:02X?}", msg));
        }
    }
}

/// How much the deck's effective speed shifts while a nudge pad is held.
/// 4% gives an audible push/pull comparable to fingertip vinyl nudging.
const NUDGE_HELD_OFFSET: f32 = 0.04;

fn note_on(note: u8, sender: &Sender) {
    // Deck-A transport buttons end an in-progress scrub cleanly first.
    if matches!(note, 40 | 41) && try_commit_scrub_on_button(note, sender) {
        // PlayToggle while scrubbing committed to normal playback — don't
        // forward PlayToggle (that'd pause the freshly-committed playback).
        return;
    }
    let cmd = match note {
        // Bottom row = Deck B
        36 => Some(DeckCommand::PlayToggle(DeckId::B)),
        37 => Some(DeckCommand::CuePress(DeckId::B)),
        38 => Some(DeckCommand::SetNudge {
            deck: DeckId::B,
            offset: -NUDGE_HELD_OFFSET,
        }),
        39 => Some(DeckCommand::SetNudge {
            deck: DeckId::B,
            offset: NUDGE_HELD_OFFSET,
        }),
        // Top row = Deck A
        40 => Some(DeckCommand::PlayToggle(DeckId::A)),
        41 => Some(DeckCommand::CuePress(DeckId::A)),
        42 => Some(DeckCommand::SetNudge {
            deck: DeckId::A,
            offset: -NUDGE_HELD_OFFSET,
        }),
        43 => Some(DeckCommand::SetNudge {
            deck: DeckId::A,
            offset: NUDGE_HELD_OFFSET,
        }),
        _ => None,
    };
    if let Some(cmd) = cmd {
        if let Err(e) = sender.send(cmd) {
            log(&format!("midi: send failed: {e}"));
        }
    }
}

fn note_off(note: u8, sender: &Sender) {
    let cmd = match note {
        37 => Some(DeckCommand::CueRelease(DeckId::B)),
        41 => Some(DeckCommand::CueRelease(DeckId::A)),
        // Nudge release → clear offset on the deck the pad belongs to.
        38 | 39 => Some(DeckCommand::SetNudge {
            deck: DeckId::B,
            offset: 0.0,
        }),
        42 | 43 => Some(DeckCommand::SetNudge {
            deck: DeckId::A,
            offset: 0.0,
        }),
        _ => None,
    };
    if let Some(cmd) = cmd {
        let _ = sender.send(cmd);
    }
}

fn on_cc(cc: u8, raw_val: u8, sender: &Sender) {
    // Jog is a relative encoder — handle it before the invert step (which
    // is for absolute pots/faders only).
    if cc == JOG_CC_DECK_A {
        on_jog(DeckId::A, raw_val, sender);
        return;
    }

    // Flip the value for CCs listed in controls.toml (backward-wired
    // faders/pots).
    let val = if INVERT_CC
        .get()
        .map(|s| s.contains(&cc))
        .unwrap_or(false)
    {
        127u8.saturating_sub(raw_val)
    } else {
        raw_val
    };

    match cc {
        // Pitch (K1/K5): CC 64 = unity, ±8% range.
        1 => send_speed(DeckId::A, val, sender),
        5 => send_speed(DeckId::B, val, sender),
        // Volume (K2/K6): linear 0..127 → 0..1.
        2 => send_gain(DeckId::A, val, sender),
        6 => send_gain(DeckId::B, val, sender),
        // EQ: K3 = A high, K4 = A low, K9 = A mid, K7 = B high, K8 = B low.
        3 => send_eq_high(DeckId::A, val, sender),
        4 => send_eq_low(DeckId::A, val, sender),
        9 => send_eq_mid(DeckId::A, val, sender),
        7 => send_eq_high(DeckId::B, val, sender),
        8 => send_eq_low(DeckId::B, val, sender),
        _ => {}
    }
}

/// Encoder relative-CC handler. Two modes:
///
/// - **Playing (already)**: nudge. The signed delta becomes a small
///   temporary `SetNudge(offset)` proportional to spin speed; the
///   watchdog clears the offset to 0 when the wheel stops.
/// - **Paused**: scrub *with audio*. We force the deck into playback
///   and use `SetNudge` to drive effective playback rate equal to a
///   scrub target — forward or reverse, proportional to spin. When the
///   user lets go, the watchdog flips the deck back to paused at the
///   new position.
fn on_jog(deck: DeckId, val: u8, sender: &Sender) {
    let signed_delta = val as i32 - 64;
    let Some(state) = JOG_STATE.get() else {
        return;
    };
    state
        .deck_a_last_us
        .store(micros_since_epoch(), Ordering::Relaxed);

    let is_playing = state.deck_a_tel.is_playing();
    let was_scrubbing = state.deck_a_scrubbing.load(Ordering::Relaxed);

    // Transition into scrub mode if we were paused-and-not-scrubbing.
    if !is_playing && !was_scrubbing {
        state.deck_a_scrubbing.store(true, Ordering::Relaxed);
        let _ = sender.send(DeckCommand::Play(deck));
    }

    if was_scrubbing || !is_playing {
        // Scrub mode: drive effective playback rate directly. The deck's
        // base speed_ratio is whatever the pitch slider's at; we set
        // nudge such that (speed_ratio + nudge) = scrub_target.
        let scrub_target = signed_delta as f64 * JOG_SCRUB_RATE_PER_TICK;
        let base = state.deck_a_tel.current_speed() as f64;
        let offset = (scrub_target - base) as f32;
        let _ = sender.send(DeckCommand::SetNudge { deck, offset });
    } else {
        // Already playing — small ±-nudge for fine BPM correction.
        let offset = signed_delta as f32 * JOG_SCALE;
        let _ = sender.send(DeckCommand::SetNudge { deck, offset });
    }
}

/// Called from `note_on` to commit-or-cancel an in-progress scrub when the
/// user presses a transport button. Returns true if a Play-Toggle should
/// be suppressed (because we've effectively committed scrub → normal play
/// already).
fn try_commit_scrub_on_button(note: u8, sender: &Sender) -> bool {
    let Some(state) = JOG_STATE.get() else {
        return false;
    };
    if !state.deck_a_scrubbing.swap(false, Ordering::AcqRel) {
        return false;
    }
    // Clear the nudge so the deck plays at its set tempo from here on.
    let _ = sender.send(DeckCommand::SetNudge {
        deck: DeckId::A,
        offset: 0.0,
    });
    if note == 40 {
        // PlayToggle while scrubbing = commit scrub to normal play. The
        // deck was already "playing" (in scrub), so do NOT also toggle —
        // that would pause it. Caller should skip forwarding PlayToggle.
        return true;
    }
    // Any other button: pause the deck first so the regular handler
    // (e.g., CuePress's paused branch) gets the expected state.
    let _ = sender.send(DeckCommand::Pause(DeckId::A));
    false
}

/// Watchdog: once per 25 ms, check whether the last CC 16 has aged past
/// the timeout. If so, return the deck to its set tempo (SetNudge(0)) and,
/// if we'd forced the deck into playback for scrub, pause it again.
fn jog_watchdog(state: Arc<JogState>, sender: Sender) {
    let timeout_us = JOG_TIMEOUT_MS * 1_000;
    let tick = Duration::from_millis(25);
    loop {
        thread::sleep(tick);
        let last = state.deck_a_last_us.load(Ordering::Relaxed);
        if last == 0 {
            continue;
        }
        let now = micros_since_epoch();
        if now.saturating_sub(last) <= timeout_us {
            continue;
        }
        let _ = sender.send(DeckCommand::SetNudge {
            deck: DeckId::A,
            offset: 0.0,
        });
        if state.deck_a_scrubbing.swap(false, Ordering::AcqRel) {
            let _ = sender.send(DeckCommand::Pause(DeckId::A));
        }
        state.deck_a_last_us.store(0, Ordering::Relaxed);
    }
}

fn send_speed(deck: DeckId, cc: u8, sender: &Sender) {
    let _ = sender.send(DeckCommand::SetSpeed {
        deck,
        ratio: cc_to_speed(cc),
    });
}
fn send_gain(deck: DeckId, cc: u8, sender: &Sender) {
    let _ = sender.send(DeckCommand::SetGain {
        deck,
        gain: cc_to_gain(cc),
    });
}
fn send_eq_high(deck: DeckId, cc: u8, sender: &Sender) {
    let _ = sender.send(DeckCommand::SetEqHigh {
        deck,
        db: cc_to_eq_db(cc),
    });
}
fn send_eq_low(deck: DeckId, cc: u8, sender: &Sender) {
    let _ = sender.send(DeckCommand::SetEqLow {
        deck,
        db: cc_to_eq_db(cc),
    });
}
fn send_eq_mid(deck: DeckId, cc: u8, sender: &Sender) {
    let _ = sender.send(DeckCommand::SetEqMid {
        deck,
        db: cc_to_eq_db(cc),
    });
}

/// CC 0..127 → speed ratio in roughly 0.92..1.08, with CC 64 = exactly 1.0.
fn cc_to_speed(cc: u8) -> f32 {
    let normalized = (cc as f32 - 64.0) / 63.0; // ~ -1.0..+1.0
    (1.0 + normalized * 0.08).clamp(0.92, 1.08)
}

/// CC 0..127 → linear gain 0..1.0.
fn cc_to_gain(cc: u8) -> f32 {
    (cc as f32 / 127.0).clamp(0.0, 1.0)
}

/// CC 0..127 → EQ band gain in dB. Piecewise linear: CC 0 = -25 dB
/// (effective kill), CC 64 = 0 dB (flat), CC 127 = +6 dB.
fn cc_to_eq_db(cc: u8) -> f32 {
    if cc <= 64 {
        -25.0 + (cc as f32 / 64.0) * 25.0
    } else {
        (cc as f32 - 64.0) / 63.0 * 6.0
    }
}

fn log(s: &str) {
    use std::io::Write;
    let mut out = std::io::stderr().lock();
    let _ = writeln!(out, "{s}");
    let _ = out.flush();
}
