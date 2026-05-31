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

#[derive(Parser, Debug)]
#[command(name = "dj", about = "DJ controller — two decks, GUI + LPD8")]
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

    /// Substring to match the MIDI input port name. Default "LPD8".
    /// Set to "" to disable MIDI entirely.
    #[arg(long, default_value = "LPD8")]
    midi: String,

    /// Directory to scan for audio files (relative to CWD by default).
    #[arg(long, default_value = "music")]
    music_dir: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(node) = &cli.device {
        unsafe {
            std::env::set_var("PIPEWIRE_NODE", node);
        }
        eprintln!("PIPEWIRE_NODE = {node}");
    }

    let engine = audio::Engine::start(
        cli.device.as_deref(),
        cli.cue_device.as_deref(),
    )?;
    let sender = engine.sender();

    let midi_status = if cli.midi.is_empty() {
        "MIDI: disabled".to_string()
    } else {
        let tel_a = engine.telemetry(control::DeckId::A);
        let controls = load_controls();
        match midi::start(&cli.midi, sender.clone(), tel_a, controls.invert_cc) {
            Ok(m) => {
                eprintln!("midi: connected to {}", m.port_name);
                // Keep alive for the whole program lifetime by leaking. Simpler
                // than passing ownership across the eframe boundary.
                Box::leak(Box::new(m));
                format!("MIDI: {}", cli.midi)
            }
            Err(e) => {
                eprintln!("midi: not started ({e})");
                format!("MIDI: {e}")
            }
        }
    };

    let app = ui::DjApp::new(engine, cli.music_dir, midi_status);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("DJ")
            .with_inner_size([1100.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "DJ",
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
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("eframe: {e}"))?;

    Ok(())
}
