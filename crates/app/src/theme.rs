//! Follows the system light/dark setting (SPEC R8.6).
//!
//! The mapping from a system signal to the gpui-kit theme is a pure function so
//! the decision is testable without a window. The screenshot override is
//! [`FORCE_THEME_ENV`]; see the crate README note in `docs/theme-seam.md`.

use gpui_kit::WindowAppearance;
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Window};

/// The environment variable that pins the theme for screenshots.
///
/// Set it to `light` or `dark` to stop following the system. Any other value is
/// ignored.
pub const FORCE_THEME_ENV: &str = "IMMICH_SYNC_THEME";

/// The system light/dark signal, named without reference to any UI toolkit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemTheme {
    Light,
    Dark,
}

impl From<WindowAppearance> for SystemTheme {
    fn from(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
        }
    }
}

/// Maps a system theme to the gpui-kit theme mode.
#[must_use]
pub fn theme_mode(system: SystemTheme) -> ThemeMode {
    match system {
        SystemTheme::Light => ThemeMode::Light,
        SystemTheme::Dark => ThemeMode::Dark,
    }
}

/// Applies a system theme to the running client.
pub fn apply_system_theme(system: SystemTheme, cx: &mut App) {
    Theme::change(theme_mode(system), None, cx);
}

/// Applies the window's system theme and keeps following it while it changes.
///
/// A [`FORCE_THEME_ENV`] value pins the theme and skips the observer, so a
/// screenshot run is reproducible.
pub fn follow_system_theme(window: &mut Window, cx: &mut App) {
    if let Some(forced) = forced_theme() {
        apply_system_theme(forced, cx);
        return;
    }

    apply_system_theme(window.appearance().into(), cx);
    window
        .observe_window_appearance(|window, cx| {
            apply_system_theme(window.appearance().into(), cx);
        })
        .detach();
}

/// The screenshot override, if one is set.
#[must_use]
pub fn forced_theme() -> Option<SystemTheme> {
    match std::env::var(FORCE_THEME_ENV).ok()?.as_str() {
        "light" => Some(SystemTheme::Light),
        "dark" => Some(SystemTheme::Dark),
        _ => None,
    }
}
