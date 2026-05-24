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
//! Note release brings the deck back to its set tempo. The BPM readout
//! doesn't move while nudge is held — base speed is what's published.
//! (Sync via the UI button only.)

use anyhow::{Context, Result, anyhow};
use audio::Sender;
use control::{DeckCommand, DeckId};
use midir::{Ignore, MidiInput, MidiInputConnection};

pub struct MidiThread {
    _conn: MidiInputConnection<()>,
    pub port_name: String,
}

pub fn start(port_filter: &str, sender: Sender) -> Result<MidiThread> {
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

    Ok(MidiThread {
        _conn: conn,
        port_name,
    })
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

fn on_cc(cc: u8, val: u8, sender: &Sender) {
    match cc {
        // Pitch (K1/K5): CC 64 = unity, ±8% range.
        1 => send_speed(DeckId::A, val, sender),
        5 => send_speed(DeckId::B, val, sender),
        // Volume (K2/K6): linear 0..127 → 0..1.
        2 => send_gain(DeckId::A, val, sender),
        6 => send_gain(DeckId::B, val, sender),
        // EQ: K3 = A high, K4 = A low, K7 = B high, K8 = B low.
        3 => send_eq_high(DeckId::A, val, sender),
        4 => send_eq_low(DeckId::A, val, sender),
        7 => send_eq_high(DeckId::B, val, sender),
        8 => send_eq_low(DeckId::B, val, sender),
        _ => {}
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
