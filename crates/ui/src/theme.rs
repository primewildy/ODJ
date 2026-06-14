//! System theme (light/dark) detection + live following.
//!
//! eframe's `follow_system_theme` is wired through winit's
//! `WindowEvent::ThemeChanged`, which winit only emits on macOS
//! and Windows (still true on 0.34). On Linux/Wayland we have to
//! detect it ourselves:
//!
//!   1. **freedesktop portal** — the cross-DE standard. Asks
//!      `org.freedesktop.portal.Settings` for the `color-scheme`
//!      key (`0` = no preference, `1` = prefer dark, `2` = prefer
//!      light). This is what GNOME, KDE, and Flatpak apps speak.
//!   2. **gsettings** fallback — for the cases where no portal is
//!      running (rare these days, but cheap to keep).
//!
//! A 5-second poll covers the lifetime of a session without much
//! cost (~50 ms of shell-out, off the UI thread).
//!
//! Both probes go via small subprocess calls to avoid pulling in
//! `zbus` (heavy) just for one read.
//!
//! Manual override is intentionally out of scope here — settings UI
//! lands next and will expose a "force light / dark / follow OS"
//! preference.

use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemTheme {
    Light,
    Dark,
}

/// Synchronous detection. Returns Dark if every probe fails — that's
/// our pre-existing default, so a broken probe never makes the UI
/// look worse than before.
pub fn detect() -> SystemTheme {
    if let Some(t) = probe_portal() {
        return t;
    }
    if let Some(t) = probe_gsettings() {
        return t;
    }
    SystemTheme::Dark
}

/// Apply a system theme to an egui context. Visuals are built from
/// the design-token `Palette` so framework-managed widgets pick up
/// the same neutral surfaces our custom widgets use.
pub fn apply(ctx: &eframe::egui::Context, theme: SystemTheme) {
    let palette = match theme {
        SystemTheme::Light => crate::palette::Palette::light(),
        SystemTheme::Dark => crate::palette::Palette::dark(),
    };
    ctx.set_visuals(crate::palette::visuals_from(&palette));
}

/// Spawn a background thread that polls the OS theme and re-applies
/// it to the given context when it changes. Runs forever; the
/// thread exits when the process does.
pub fn spawn_watcher(ctx: eframe::egui::Context) {
    let mut last = detect();
    apply(&ctx, last);
    std::thread::Builder::new()
        .name("dj-theme-watch".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let cur = detect();
            if cur != last {
                last = cur;
                apply(&ctx, cur);
                ctx.request_repaint();
            }
        })
        .ok();
}

/// freedesktop portal probe via `gdbus`. Output format is
/// `(<<uint32 N>>,)` where N is 0/1/2. We just substring-match.
fn probe_portal() -> Option<SystemTheme> {
    let out = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.portal.Desktop",
            "--object-path",
            "/org/freedesktop/portal/desktop",
            "--method",
            "org.freedesktop.portal.Settings.Read",
            "org.freedesktop.appearance",
            "color-scheme",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    if s.contains("uint32 2") {
        Some(SystemTheme::Light)
    } else if s.contains("uint32 1") {
        Some(SystemTheme::Dark)
    } else {
        // 0 = "no preference" — let the fallback decide.
        None
    }
}

/// GNOME settings probe — the GTK-world equivalent of the portal,
/// kept as a fallback for environments without a portal running.
/// Returns `prefer-light` / `prefer-dark` / `default`.
fn probe_gsettings() -> Option<SystemTheme> {
    let out = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    if s.contains("prefer-light") {
        Some(SystemTheme::Light)
    } else if s.contains("prefer-dark") {
        Some(SystemTheme::Dark)
    } else {
        None
    }
}
