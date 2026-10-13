//! Headless tests for the Logs view: its place in the shell, the location and
//! tail it shows, the refresh action, and the persisted level control.
//!
//! The harness cannot read pixels, so "readable in both themes" is checked by
//! rendering the view under both themes and reading the same accessibility
//! properties; the colour tokens themselves are covered by the WCAG test in
//! `tests/dashboard.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Point, TestAppContext, Window, WindowBounds,
    WindowOptions, px, size,
};
use immich_sync::autostart::{Autostart, UnavailableAutostart};
use immich_sync::connection::{ConnectionModel, profile_from_settings};
use immich_sync::dashboard::Dashboard;
use immich_sync::logging::{LOG_PREFIX, LogLevel};
use immich_sync::paths::AppPaths;
use immich_sync::settings::SettingsStore;
use immich_sync::settings_view::SettingsView;
use immich_sync::shell::{self, AppShell, ViewMode};
use immich_sync::{status, theme};
use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};

fn temporary_paths(dir: &Path) -> AppPaths {
    AppPaths {
        config_file: dir.join("config").join("settings.toml"),
        state_db: dir.join("data").join("state.db"),
        autostart_dir: dir.join("config").join("autostart"),
    }
}

fn window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            // Tall enough for the location card, the level control, and some
            // log lines at once.
            size: size(px(1200.), px(1400.)),
        })),
        ..Default::default()
    }
}

fn autostart() -> Rc<dyn Autostart> {
    Rc::new(UnavailableAutostart::new(
        "the test run has no OS autostart mechanism".to_string(),
    ))
}

/// Writes one log file into the state directory and returns its path.
fn write_log(paths: &AppPaths, lines: &[&str]) -> PathBuf {
    let directory = paths.log_directory();
    fs::create_dir_all(&directory).expect("create the log directory");
    let path = directory.join(format!("{LOG_PREFIX}.2026-01-01"));
    fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write the log file");
    path
}

/// Opens the real shell over the real settings and dashboard views.
fn open_shell(cx: &mut TestAppContext, store: SettingsStore, paths: AppPaths) -> AnyWindowHandle {
    cx.update(|cx| {
        gpui_kit::init(cx);
        let profile = profile_from_settings(store.settings());
        let connection = cx.new(|_| {
            let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(profile, credentials)
        });
        gpui_kit::open_window(window_options(), cx, |window, cx| {
            let dashboard = cx.new(|_| Dashboard::new(status::sample()));
            let settings = cx.new(|cx| {
                SettingsView::new(store, paths, autostart(), connection.clone(), window, cx)
            });
            cx.new(|cx| AppShell::new(dashboard, settings, connection.clone(), cx))
        })
        .expect("open the shell window")
        .0
    })
}

/// Moves focus to the next tab stop, as the Root's Tab binding does.
fn tab_to(window: &mut Window, id: &'static str, cx: &mut App) {
    for _ in 0..16 {
        if window.find(id).focused() == Some(true) {
            return;
        }
        window.focus_next(cx);
        window.render_frame(cx);
    }
    assert_eq!(
        window.find(id).focused(),
        Some(true),
        "{id} is not keyboard reachable"
    );
}

/// Switches the shell to the Logs surface.
fn show_logs(window: &mut Window, cx: &mut App) {
    window.render_frame(cx);
    tab_to(window, "nav-logs", cx);
    window.press("enter", cx);
}

#[test]
fn the_screenshot_view_override_names_a_surface() {
    assert_eq!(shell::view_for_name("dashboard"), Some(ViewMode::Dashboard));
    assert_eq!(shell::view_for_name("settings"), Some(ViewMode::Settings));
    assert_eq!(shell::view_for_name("logs"), Some(ViewMode::Logs));
    assert_eq!(shell::view_for_name("nonsense"), None);
}

#[gpui_kit::test]
fn the_logs_entry_shows_the_location_and_the_recent_lines(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let directory = paths.log_directory().display().to_string();
    write_log(&paths, &["first line", "second line"]);
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_shell(cx, store, paths);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.find("status-global").visible(),
            "the shell starts on the dashboard"
        );

        show_logs(window, cx);

        assert!(window.find("logs-view").visible(), "the Logs view is shown");
        assert!(
            window.try_find("settings-general").is_none(),
            "the settings are not rendered"
        );
        assert!(
            window.try_find("status-global").is_none(),
            "the dashboard is not rendered"
        );
        assert_eq!(
            window.find("logs-path-directory-value").label(),
            Some(directory.as_str()),
            "the view names the log folder"
        );
        assert_eq!(
            window.find("logs-line-0").label(),
            Some("first line"),
            "the first recent line is shown"
        );
        assert_eq!(
            window.find("logs-line-1").label(),
            Some("second line"),
            "the latest line is shown"
        );
    })
    .expect("update the shell window");
}

#[gpui_kit::test]
fn refresh_reads_the_lines_written_since_the_view_opened(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let log = write_log(&paths, &["first line"]);
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_shell(cx, store, paths);

    cx.update_window(window, |_, window, cx| {
        show_logs(window, cx);
        assert!(
            window.try_find("logs-line-1").is_none(),
            "only the one line exists so far"
        );
    })
    .expect("update the shell window");

    fs::write(&log, "first line\nsecond line\n").expect("append a line to the log file");

    cx.update_window(window, |_, window, cx| {
        window.click("logs-refresh", cx);
        assert_eq!(
            window.find("logs-line-1").label(),
            Some("second line"),
            "refresh reads the new line"
        );
    })
    .expect("update the shell window");
}

#[gpui_kit::test]
fn every_logs_control_is_named_and_keyboard_reachable(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_shell(cx, store, paths);

    cx.update_window(window, |_, window, cx| {
        show_logs(window, cx);

        for (id, label) in [
            ("nav-logs", "Logs"),
            ("logs-refresh", "Refresh the log lines"),
            ("logs-level-trace", "Log level: Trace"),
            ("logs-level-debug", "Log level: Debug"),
            ("logs-level-info", "Log level: Info"),
            ("logs-level-warn", "Log level: Warn"),
            ("logs-level-error", "Log level: Error"),
        ] {
            assert_eq!(window.find(id).label(), Some(label), "{id} is named");
        }

        for id in [
            "nav-dashboard",
            "nav-settings",
            "nav-logs",
            "logs-refresh",
            "logs-level-trace",
            "logs-level-error",
        ] {
            tab_to(window, id, cx);
        }
    })
    .expect("update the shell window");
}

#[gpui_kit::test]
fn choosing_a_level_persists_it_in_the_settings_file(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    assert_eq!(store.settings().log_level, LogLevel::Info, "the default");
    let window = open_shell(cx, store, paths.clone());

    cx.update_window(window, |_, window, cx| {
        show_logs(window, cx);
        window.click("logs-level-debug", cx);
    })
    .expect("update the shell window");

    let reloaded = SettingsStore::open(paths.config_file);
    assert_eq!(
        reloaded.settings().log_level,
        LogLevel::Debug,
        "the chosen level is stored in the settings file"
    );
}

#[gpui_kit::test]
fn the_logs_view_renders_in_both_themes(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    write_log(&paths, &["a stable line"]);

    for system in [theme::SystemTheme::Dark, theme::SystemTheme::Light] {
        let store = SettingsStore::open(paths.config_file.clone());
        let window = open_shell(cx, store, paths.clone());
        cx.update_window(window, |_, window, cx| {
            theme::apply_system_theme(system, cx);
            show_logs(window, cx);
            assert!(
                window.find("logs-view").visible(),
                "{system:?} shows the Logs view"
            );
            assert_eq!(
                window.find("logs-line-0").label(),
                Some("a stable line"),
                "{system:?} shows the log line"
            );
        })
        .expect("update the shell window");
    }
}
