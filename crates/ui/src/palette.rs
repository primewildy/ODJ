//! Design-token palette. One source of truth for every colour the UI
//! draws, both for the egui-managed widgets (text, panel fills,
//! widget backgrounds) and our custom widgets (waveforms, knobs,
//! arcade buttons).
//!
//! Hex values come from the design handoff (DJ FX Concept Board);
//! accents are shared across themes, only the neutrals change.
//! Touching values here changes them everywhere — never reach back
//! to a `Color32::from_rgb(...)` literal in widget code.

use eframe::egui::Color32;

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// True if this palette is the dark variant — let widgets that
    /// need to pick a tertiary tone (e.g. translucent fill on top of
    /// a colour) branch without comparing colours.
    pub dark: bool,

    // ----- Neutrals -------------------------------------------------
    /// App / desktop background — the layer behind every panel.
    pub board_bg: Color32,
    /// Top-level panel surface (mixer, library, decks).
    pub panel: Color32,
    /// Raised controls / interactive surfaces inside a panel (chips,
    /// well headers, buttons in their resting state).
    pub raised: Color32,
    /// Inset wells (waveform background, search field, anywhere we
    /// want a "deeper" surface).
    pub inset: Color32,
    /// Small chips / tags. Almost the same tone as raised but a
    /// fraction more saturated; reserved for status-bearing pills.
    pub chip: Color32,
    /// Borderlines.
    pub line: Color32,
    /// Stronger borderlines (cards, expanded panels).
    pub line_strong: Color32,
    /// Primary text colour.
    pub ink: Color32,
    /// Secondary text (artist, value readouts under the primary).
    pub muted: Color32,
    /// Tertiary text (column headers, "0" play counts, fine-print
    /// hints). Should disappear into the background a little.
    pub faint: Color32,
    /// Knob body fill (the disc).
    pub knob_face: Color32,
    /// Knob track / ring around the body.
    pub knob_track: Color32,

    // ----- Shared accents (same in both themes) --------------------
    /// EQ knobs, Sync, Auto-mix, Master, active chips.
    pub accent_blue: Color32,
    /// Play button, FX "ON", selected FX in list, Beat align.
    pub accent_green: Color32,
    /// Cue button outline.
    pub accent_red: Color32,
    /// FX identity (top-stripe, FX label, Colour knob), playhead line.
    pub accent_pink: Color32,
    /// Key column, secondary blues.
    pub accent_sky: Color32,
    /// Favourited star.
    pub accent_amber: Color32,
    /// Hot-cue markers on the waveforms. Placeholder — once the
    /// beat-grid editor lands, hot cues will carry per-slot colours
    /// and this token becomes the default for unlabelled cues.
    pub hot_cue: Color32,

    // ----- Stem colours (waveform + matching stem knobs) ----------
    pub stem_drums: Color32,
    pub stem_vocals: Color32,
    pub stem_instruments: Color32,
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            dark: true,
            board_bg: rgb(0x0B, 0x0F, 0x13),
            panel: rgb(0x15, 0x1B, 0x21),
            raised: rgb(0x1D, 0x24, 0x2B),
            inset: rgb(0x0E, 0x12, 0x17),
            chip: rgb(0x22, 0x2A, 0x31),
            line: Color32::from_rgba_unmultiplied(255, 255, 255, 18),     // ~7% alpha
            line_strong: Color32::from_rgba_unmultiplied(255, 255, 255, 33), // ~13%
            ink: rgb(0xE7, 0xEE, 0xF4),
            muted: rgb(0x82, 0x94, 0xA1),
            faint: rgb(0x56, 0x64, 0x72),
            knob_face: rgb(0x23, 0x2C, 0x34),
            knob_track: rgb(0x2B, 0x34, 0x3D),
            ..accents()
        }
    }

    pub fn light() -> Self {
        Self {
            dark: false,
            board_bg: rgb(0xE8, 0xEC, 0xF0),
            panel: rgb(0xFF, 0xFF, 0xFF),
            raised: rgb(0xF3, 0xF6, 0xF8),
            inset: rgb(0xEE, 0xF1, 0xF4),
            chip: rgb(0xF1, 0xF4, 0xF7),
            line: rgb(0xDD, 0xE4, 0xEA),
            line_strong: rgb(0xC9, 0xD2, 0xDB),
            ink: rgb(0x1B, 0x25, 0x2F),
            muted: rgb(0x5C, 0x6B, 0x77),
            faint: rgb(0x93, 0xA1, 0xAC),
            knob_face: rgb(0xEE, 0xF2, 0xF6),
            knob_track: rgb(0xD6, 0xDE, 0xE6),
            ..accents()
        }
    }
}

/// Pull the appropriate palette for an egui Ui based on its visuals.
pub fn for_ui(ui: &eframe::egui::Ui) -> Palette {
    if ui.visuals().dark_mode {
        Palette::dark()
    } else {
        Palette::light()
    }
}

/// Convert a `Palette` into an `egui::Visuals` so the framework's
/// own widgets (text, ScrollArea, ComboBox, panels) inherit the
/// design tokens. Called by `theme::apply` whenever the system theme
/// flips.
pub fn visuals_from(p: &Palette) -> eframe::egui::Visuals {
    use eframe::egui::{Stroke, Visuals};
    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    v.dark_mode = p.dark;
    v.panel_fill = p.panel;
    v.window_fill = p.panel;
    v.faint_bg_color = p.raised;
    v.extreme_bg_color = p.inset;
    v.override_text_color = Some(p.ink);
    v.window_stroke = Stroke::new(1.0, p.line);
    v.widgets.noninteractive.bg_fill = p.panel;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.muted);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.inactive.bg_fill = p.raised;
    v.widgets.inactive.weak_bg_fill = p.raised;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.ink);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.hovered.bg_fill = p.chip;
    v.widgets.hovered.weak_bg_fill = p.chip;
    v.widgets.hovered.fg_stroke = Stroke::new(1.5, p.ink);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.line_strong);
    v.widgets.active.bg_fill = p.accent_blue;
    v.widgets.active.fg_stroke = Stroke::new(1.5, p.ink);
    v
}

fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// The shared accent + stem block, factored out so `..accents()` in
/// the constructors keeps the constants in one place.
fn accents() -> Palette {
    Palette {
        // Neutrals get overwritten by `..accents()` callers; placeholders.
        dark: false,
        board_bg: Color32::BLACK,
        panel: Color32::BLACK,
        raised: Color32::BLACK,
        inset: Color32::BLACK,
        chip: Color32::BLACK,
        line: Color32::BLACK,
        line_strong: Color32::BLACK,
        ink: Color32::WHITE,
        muted: Color32::WHITE,
        faint: Color32::WHITE,
        knob_face: Color32::BLACK,
        knob_track: Color32::BLACK,

        accent_blue: rgb(0x4A, 0xC0, 0xE7),
        accent_green: rgb(0x22, 0xC5, 0x5E),
        accent_red: rgb(0xE5, 0x48, 0x4D),
        accent_pink: rgb(0xDE, 0x67, 0x78),
        accent_sky: rgb(0x2A, 0xA3, 0xD3),
        accent_amber: rgb(0xF5, 0xA6, 0x23),
        hot_cue: rgb(0xF7, 0xE1, 0x1A),

        stem_drums: rgb(0xEF, 0x5A, 0x5A),
        stem_vocals: rgb(0x2B, 0xD4, 0x6E),
        stem_instruments: rgb(0x3F, 0xB7, 0xE8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accents_match_across_themes() {
        let d = Palette::dark();
        let l = Palette::light();
        assert_eq!(d.accent_blue, l.accent_blue);
        assert_eq!(d.accent_pink, l.accent_pink);
        assert_eq!(d.stem_drums, l.stem_drums);
        assert_eq!(d.stem_vocals, l.stem_vocals);
        assert_eq!(d.stem_instruments, l.stem_instruments);
    }

    #[test]
    fn dark_flag_set_correctly() {
        assert!(Palette::dark().dark);
        assert!(!Palette::light().dark);
    }

    #[test]
    fn stem_colours_match_design_handoff() {
        let d = Palette::dark();
        assert_eq!(d.stem_drums, rgb(0xEF, 0x5A, 0x5A));
        assert_eq!(d.stem_vocals, rgb(0x2B, 0xD4, 0x6E));
        assert_eq!(d.stem_instruments, rgb(0x3F, 0xB7, 0xE8));
    }
}
