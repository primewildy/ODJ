//! Bundled fonts.
//!
//! Two TTFs live under `crates/ui/assets/fonts/` and are baked into
//! the binary with `include_bytes!`:
//!
//! - **Roboto-Regular.ttf** — proportional UI font (Apache 2.0).
//! - **JetBrainsMono-Regular.ttf** — monospace for numeric readouts
//!   like BPM, time, key, knob values (OFL 1.1).
//!
//! The two fonts together add ~800 KB to the binary, which is a
//! reasonable tax for typography that matches the design.
//!
//! `apply(ctx)` is called once at startup; egui takes ownership of
//! the font data and we don't need to re-register it later.

use eframe::egui;

const ROBOTO: &[u8] = include_bytes!("../assets/fonts/Roboto-Regular.ttf");
const JETBRAINS_MONO: &[u8] =
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");

/// Register the bundled fonts on the given egui context.
///
/// Strategy:
/// - Roboto is added to `Proportional` as the first-priority font
///   (egui's default `Inter` stays as a fallback for glyphs Roboto
///   doesn't cover — punctuation, dingbats).
/// - JetBrains Mono is added to `Monospace` as the first-priority
///   font, replacing egui's bundled `Hack`. Numeric readouts
///   already go through `RichText::monospace()` so they pick it up
///   automatically.
pub fn apply(ctx: &egui::Context) {
    let mut defs = egui::FontDefinitions::default();
    defs.font_data.insert(
        "roboto".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(ROBOTO)),
    );
    defs.font_data.insert(
        "jetbrains-mono".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(JETBRAINS_MONO)),
    );
    defs.families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "roboto".to_owned());
    defs.families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "jetbrains-mono".to_owned());
    ctx.set_fonts(defs);
}
