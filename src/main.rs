//! DJ controller — eframe app. Engine owns audio, MIDI thread runs alongside,
//! UI runs on the main thread.

mod midi;

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "dj", about = "DJ controller — two decks, GUI + LPD8")]
struct Cli {
    /// Optional PipeWire node name to route this process to.
    /// Sets PIPEWIRE_NODE before audio init — per-process, won't affect other apps.
    #[arg(long)]
    device: Option<String>,

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

    let engine = audio::Engine::start(None)?;
    let sender = engine.sender();

    let midi_status = if cli.midi.is_empty() {
        "MIDI: disabled".to_string()
    } else {
        match midi::start(&cli.midi, sender.clone()) {
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
    eframe::run_native("DJ", options, Box::new(|_cc| Ok(Box::new(app))))
        .map_err(|e| anyhow!("eframe: {e}"))?;

    Ok(())
}
