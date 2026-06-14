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
/// CC numbers the firmware emits for the jog encoders. Mirrored in
/// `hardware/firmware/src/main.c::ENCODER_CC_A/_B`.
const JOG_CC_DECK_A: u8 = 16;
const JOG_CC_DECK_B: u8 = 17;

/// MIDI notes the host emits *to* the controller to drive LEDs.
const LED_NOTE_DECK_A_PLAY:  u8 = 40;
const LED_NOTE_DECK_A_HPCUE: u8 = 44;
const LED_NOTE_DECK_B_PLAY:  u8 = 36;
const LED_NOTE_DECK_B_HPCUE: u8 = 45;

/// Transport-button notes that should commit a paused-scrub when
/// pressed. PLAY commits-and-continues; CUE commits-then-pauses.
const SCRUB_COMMIT_CUE_NOTES: &[u8] = &[41, 37]; // Deck A CUE (41), Deck B CUE (37)

/// When true, route the hardware EQ pots (CC 7/8/10) to stem gains
/// (drums/instr/vocals) instead of the EQ shelves. Toggled from the UI.
/// Set by `start()`; read by `on_cc`.
static STEM_MODE: OnceLock<Arc<AtomicBool>> = OnceLock::new();
fn stem_mode_on() -> bool {
    STEM_MODE.get().map(|f| f.load(Ordering::Relaxed)).unwrap_or(false)
}

/// Per-deck jog/scrub state. The MIDI callback writes `last_us` on
/// every encoder CC; the watchdog reads it and clears nudge/pause when
/// the encoder has been quiet long enough.
struct DeckJog {
    /// Microseconds-since-epoch of the last jog CC. 0 = none / cleared.
    last_us: AtomicU64,
    /// True while we're in "paused-deck scrub" mode — we forced the deck
    /// into playback to produce audio during scrubbing. Watchdog reverses
    /// this on timeout.
    scrubbing: AtomicBool,
    tel: DeckTelemetry,
}

struct JogState {
    a: DeckJog,
    b: DeckJog,
}

impl JogState {
    fn deck(&self, id: DeckId) -> &DeckJog {
        match id { DeckId::A => &self.a, DeckId::B => &self.b }
    }
}

static JOG_EPOCH: OnceLock<Instant> = OnceLock::new();
static JOG_STATE: OnceLock<Arc<JogState>> = OnceLock::new();

/// CCs whose 0-127 value should be inverted, from controls.toml. Lets
/// backward-wired faders/pots be flipped without a recompile. Set once
/// at start(); empty if no config.
static INVERT_CC: OnceLock<HashSet<u8>> = OnceLock::new();

/// When true, every MIDI byte we receive is mirrored to stderr.
/// Wired to the `log_midi` setting (settings.toml + the toggle in the
/// settings window); flipped live, no restart needed.
static LOG_MIDI: OnceLock<Arc<AtomicBool>> = OnceLock::new();

fn micros_since_epoch() -> u64 {
    let epoch = *JOG_EPOCH.get_or_init(Instant::now);
    Instant::now().saturating_duration_since(epoch).as_micros() as u64
}

pub struct MidiThread {
    _conn: MidiInputConnection<()>,
    _out: Option<Arc<Mutex<MidiOutputConnection>>>,
    pub port_name: String,
}

/// Enumerate every MIDI input port the system exposes. Used by the
/// settings UI to populate the MIDI port dropdown. Names only — the
/// caller substring-matches when picking the actual port to connect.
pub fn list_inputs() -> Vec<String> {
    let Ok(input) = MidiInput::new("dj-midi-enum") else { return Vec::new(); };
    let mut names: Vec<String> = input
        .ports()
        .iter()
        .filter_map(|p| input.port_name(p).ok())
        .collect();
    names.sort();
    names.dedup();
    names
}

pub fn start(
    port_filter: &str,
    sender: Sender,
    deck_a_tel: DeckTelemetry,
    deck_b_tel: DeckTelemetry,
    invert_cc: Vec<u8>,
    stem_mode: Arc<AtomicBool>,
    log_midi: Arc<AtomicBool>,
) -> Result<MidiThread> {
    let _ = INVERT_CC.set(invert_cc.into_iter().collect());
    let _ = STEM_MODE.set(stem_mode);
    let _ = LOG_MIDI.set(log_midi);
    // Lazy init the jog state + spawn the watchdog. Safe to call repeatedly;
    // OnceLock guards both. The watchdog runs forever and is detached.
    let jog_state = JOG_STATE
        .get_or_init(|| {
            let state = Arc::new(JogState {
                a: DeckJog {
                    last_us: AtomicU64::new(0),
                    scrubbing: AtomicBool::new(false),
                    tel: deck_a_tel.clone(),
                },
                b: DeckJog {
                    last_us: AtomicU64::new(0),
                    scrubbing: AtomicBool::new(false),
                    tel: deck_b_tel.clone(),
                },
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
    let filters: Vec<&str> = port_filter
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let port = filters
        .iter()
        .find_map(|f| {
            ports.iter().find(|p| {
                input.port_name(p).map(|n| n.contains(*f)).unwrap_or(false)
            })
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
        let tels = (deck_a_tel, deck_b_tel);
        thread::Builder::new()
            .name("dj-led-watcher".into())
            .spawn(move || led_watcher(watcher_out, tels.0, tels.1))
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

/// Polls both decks' `playing` and `cue_on` atomics ~30 Hz; emits
/// note_on/off on edges so the firmware lights/unlights:
///   note 36 / 40 → Deck B/A Play LED
///   note 44 / 45 → Deck A/B 🎧-CUE LED
///
/// During scrub the deck is technically `playing` (forced for audible
/// scrub), but it's a transient navigation mode — the LED stays dark.
fn led_watcher(
    out: Arc<Mutex<MidiOutputConnection>>,
    deck_a_tel: DeckTelemetry,
    deck_b_tel: DeckTelemetry,
) {
    let mut last_play_a: Option<bool> = None;
    let mut last_play_b: Option<bool> = None;
    let mut last_cue_a:  Option<bool> = None;
    let mut last_cue_b:  Option<bool> = None;
    let tick = Duration::from_millis(33);
    loop {
        thread::sleep(tick);
        let (scrub_a, scrub_b) = JOG_STATE
            .get()
            .map(|s| (s.a.scrubbing.load(Ordering::Relaxed),
                      s.b.scrubbing.load(Ordering::Relaxed)))
            .unwrap_or((false, false));
        let play_a = deck_a_tel.is_playing() && !scrub_a;
        let play_b = deck_b_tel.is_playing() && !scrub_b;
        let cue_a  = deck_a_tel.is_cue_on();
        let cue_b  = deck_b_tel.is_cue_on();
        emit_led_edge(&out, &mut last_play_a, play_a, LED_NOTE_DECK_A_PLAY);
        emit_led_edge(&out, &mut last_play_b, play_b, LED_NOTE_DECK_B_PLAY);
        emit_led_edge(&out, &mut last_cue_a,  cue_a,  LED_NOTE_DECK_A_HPCUE);
        emit_led_edge(&out, &mut last_cue_b,  cue_b,  LED_NOTE_DECK_B_HPCUE);
    }
}

fn emit_led_edge(
    out: &Arc<Mutex<MidiOutputConnection>>,
    last: &mut Option<bool>,
    now: bool,
    note: u8,
) {
    if Some(now) == *last { return; }
    *last = Some(now);
    let msg = if now { [0x90, note, 0x7F] } else { [0x80, note, 0x00] };
    if let Ok(mut o) = out.lock() {
        let _ = o.send(&msg);
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
    if scrub_commit_deck_for_note(note).is_some()
        && try_commit_scrub_on_button(note, sender)
    {
        // PlayToggle while scrubbing committed to normal playback — don't
        // forward PlayToggle (that'd pause the freshly-committed playback).
        return;
    }
    // Pressing transport CUE on a deck also switches headphone cueing
    // exclusively to that deck (this deck's 🎧-CUE on, the other's off).
    // The LED watcher picks up the cue_on edges and lights / unlights
    // the 🎧-CUE LEDs accordingly.
    if note == 41 || note == 37 {
        let (deck, other) = if note == 41 {
            (DeckId::A, DeckId::B)
        } else {
            (DeckId::B, DeckId::A)
        };
        let _ = sender.send(DeckCommand::SetCueOn { deck, on: true });
        let _ = sender.send(DeckCommand::SetCueOn { deck: other, on: false });
        // Mirror the exclusive routing into the HPCUE toggle state so a
        // subsequent HPCUE press flips from the correct base.
        if deck == DeckId::A {
            HPCUE_A.store(true, Ordering::Relaxed);
            HPCUE_B.store(false, Ordering::Relaxed);
        } else {
            HPCUE_A.store(false, Ordering::Relaxed);
            HPCUE_B.store(true, Ordering::Relaxed);
        }
        let _ = sender.send(DeckCommand::CuePress(deck));
        return;
    }
    // HPCUE buttons (44/45): toggle the deck's cue_on state. Press
    // only (we ignore release). Host-side state tracks the toggle so
    // the firmware can stay a dumb edge sender.
    if note == 44 || note == 45 {
        let deck = if note == 44 { DeckId::A } else { DeckId::B };
        let state = if note == 44 { &HPCUE_A } else { &HPCUE_B };
        let on = !state.fetch_xor(true, Ordering::Relaxed);
        let _ = sender.send(DeckCommand::SetCueOn { deck, on });
        return;
    }
    // SYNC buttons (46/47): match this deck's tempo to the other deck.
    if note == 46 || note == 47 {
        let deck = if note == 46 { DeckId::A } else { DeckId::B };
        let _ = sender.send(DeckCommand::Sync { deck });
        return;
    }
    let cmd = match note {
        // Bottom row = Deck B
        36 => Some(DeckCommand::PlayToggle(DeckId::B)),
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

/// Host-side toggle state for the HPCUE buttons (notes 44 = Deck A,
/// 45 = Deck B). Pressing the firmware button XORs the flag and pushes
/// the resulting bool through SetCueOn. The transport-CUE auto-route
/// (notes 41/37) writes through the audio engine directly without
/// touching these — meaning the HPCUE toggle state can fall out of
/// sync with the actual cue routing. That's tolerable: the next HPCUE
/// press just flips relative to whatever we last sent here.
static HPCUE_A: AtomicBool = AtomicBool::new(false);
static HPCUE_B: AtomicBool = AtomicBool::new(false);

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
    // Jogs are relative encoders — handle them before the invert step
    // (which is for absolute pots/faders only).
    if cc == JOG_CC_DECK_A {
        on_jog(DeckId::A, raw_val, sender);
        return;
    }
    if cc == JOG_CC_DECK_B {
        on_jog(DeckId::B, raw_val, sender);
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
        // EQ: K3=A high, K4=A low, K9=A mid; K7=B high, K8=B low, K10=B mid.
        // When stem-mode is on, each deck's EQ knobs re-route to stem
        // gains: HIGH→drums, LOW→instr, MID→vocals.
        3 => if stem_mode_on() {
            let _ = sender.send(DeckCommand::SetStemDrums { deck: DeckId::A, gain: cc_to_stem_gain(val) });
        } else {
            send_eq_high(DeckId::A, val, sender);
        },
        4 => if stem_mode_on() {
            let _ = sender.send(DeckCommand::SetStemInstruments { deck: DeckId::A, gain: cc_to_stem_gain(val) });
        } else {
            send_eq_low(DeckId::A, val, sender);
        },
        9 => if stem_mode_on() {
            let _ = sender.send(DeckCommand::SetStemVocals { deck: DeckId::A, gain: cc_to_stem_gain(val) });
        } else {
            send_eq_mid(DeckId::A, val, sender);
        },
        7 => if stem_mode_on() {
            let _ = sender.send(DeckCommand::SetStemDrums { deck: DeckId::B, gain: cc_to_stem_gain(val) });
        } else {
            send_eq_high(DeckId::B, val, sender);
        },
        8 => if stem_mode_on() {
            let _ = sender.send(DeckCommand::SetStemInstruments { deck: DeckId::B, gain: cc_to_stem_gain(val) });
        } else {
            send_eq_low(DeckId::B, val, sender);
        },
        10 => if stem_mode_on() {
            let _ = sender.send(DeckCommand::SetStemVocals { deck: DeckId::B, gain: cc_to_stem_gain(val) });
        } else {
            send_eq_mid(DeckId::B, val, sender);
        },
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
    let Some(state) = JOG_STATE.get() else { return };
    let dj = state.deck(deck);
    dj.last_us.store(micros_since_epoch(), Ordering::Relaxed);

    let is_playing = dj.tel.is_playing();
    let was_scrubbing = dj.scrubbing.load(Ordering::Relaxed);

    // Transition into scrub mode if we were paused-and-not-scrubbing.
    if !is_playing && !was_scrubbing {
        dj.scrubbing.store(true, Ordering::Relaxed);
        let _ = sender.send(DeckCommand::Play(deck));
    }

    if was_scrubbing || !is_playing {
        // Scrub mode: drive effective playback rate directly. The deck's
        // base speed_ratio is whatever the pitch slider's at; set nudge
        // such that (speed_ratio + nudge) = scrub_target.
        let scrub_target = signed_delta as f64 * JOG_SCRUB_RATE_PER_TICK;
        let base = dj.tel.current_speed() as f64;
        let offset = (scrub_target - base) as f32;
        let _ = sender.send(DeckCommand::SetNudge { deck, offset });
    } else {
        // Already playing — small ±-nudge for fine BPM correction.
        let offset = signed_delta as f32 * JOG_SCALE;
        let _ = sender.send(DeckCommand::SetNudge { deck, offset });
    }
}

/// Map a transport-button note to the deck it belongs to, if any. PLAY
/// commits scrub-to-play; CUE commits-then-pauses.
fn scrub_commit_deck_for_note(note: u8) -> Option<(DeckId, bool /* is_play */)> {
    if note == LED_NOTE_DECK_A_PLAY { return Some((DeckId::A, true)); }
    if note == LED_NOTE_DECK_B_PLAY { return Some((DeckId::B, true)); }
    if SCRUB_COMMIT_CUE_NOTES[0] == note { return Some((DeckId::A, false)); }
    if SCRUB_COMMIT_CUE_NOTES[1] == note { return Some((DeckId::B, false)); }
    None
}

/// Called from `note_on` when a transport button (Play/CUE on either deck)
/// is pressed. If that deck was mid-scrub, commit cleanly. Returns true
/// when the caller should swallow the PlayToggle (because scrub already
/// transitioned the deck into normal playback).
fn try_commit_scrub_on_button(note: u8, sender: &Sender) -> bool {
    let Some((deck, is_play)) = scrub_commit_deck_for_note(note) else {
        return false;
    };
    let Some(state) = JOG_STATE.get() else { return false };
    let dj = state.deck(deck);
    if !dj.scrubbing.swap(false, Ordering::AcqRel) {
        return false;
    }
    // Clear the nudge so the deck plays at its set tempo from here on.
    let _ = sender.send(DeckCommand::SetNudge { deck, offset: 0.0 });
    if is_play {
        // PlayToggle while scrubbing = commit scrub to normal play. The
        // deck was already "playing" (in scrub), so do NOT also toggle —
        // that would pause it. Caller should skip forwarding PlayToggle.
        return true;
    }
    // CUE: pause the deck first so the regular handler's paused branch
    // (set-cue-here) gets the expected state.
    let _ = sender.send(DeckCommand::Pause(deck));
    false
}

/// Watchdog: every 25 ms, check whether either deck's last jog CC has
/// aged past the timeout. If so, clear that deck's nudge and, if we'd
/// forced it into playback for scrub, pause it again.
fn jog_watchdog(state: Arc<JogState>, sender: Sender) {
    let timeout_us = JOG_TIMEOUT_MS * 1_000;
    let tick = Duration::from_millis(25);
    loop {
        thread::sleep(tick);
        for deck in [DeckId::A, DeckId::B] {
            let dj = state.deck(deck);
            let last = dj.last_us.load(Ordering::Relaxed);
            if last == 0 { continue; }
            let now = micros_since_epoch();
            if now.saturating_sub(last) <= timeout_us { continue; }
            let _ = sender.send(DeckCommand::SetNudge { deck, offset: 0.0 });
            if dj.scrubbing.swap(false, Ordering::AcqRel) {
                let _ = sender.send(DeckCommand::Pause(deck));
            }
            dj.last_us.store(0, Ordering::Relaxed);
        }
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

/// Stem gain curve: piecewise, CC 64 = unity (1.0), CC 0 = 0.0, CC 127 = 1.5.
/// Matches the visual centre of the stem knobs (neutral = 1.0).
fn cc_to_stem_gain(cc: u8) -> f32 {
    if cc <= 64 {
        cc as f32 / 64.0
    } else {
        1.0 + (cc as f32 - 64.0) / 63.0 * 0.5
    }
}

fn log(s: &str) {
    // Gated by the `log_midi` setting (settings.toml). When off, this
    // is a single relaxed atomic load and an early-return — the
    // hot-path cost of leaving the call sites in for "just turn it on
    // when wiring a new controller" is negligible.
    if !LOG_MIDI.get().map(|f| f.load(Ordering::Relaxed)).unwrap_or(true) {
        return;
    }
    use std::io::Write;
    let mut out = std::io::stderr().lock();
    let _ = writeln!(out, "{s}");
    let _ = out.flush();
}
