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
use std::time::Duration;

use gpui_kit::test::{ElementSnapshot, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ClipboardItem, ElementId, Point, ScrollDelta,
    TestAppContext, Window, WindowBounds, WindowOptions, point, px, size,
};
use immich_sync::autostart::{Autostart, UnavailableAutostart};
use immich_sync::connection::{ConnectionModel, profile_from_settings};
use immich_sync::dashboard::Dashboard;
use immich_sync::logging::{LOG_PREFIX, LogLevel};
use immich_sync::paths::AppPaths;
use immich_sync::settings::SettingsStore;
use immich_sync::settings_view::SettingsView;
use immich_sync::shell::{self, AppShell, ViewMode};
use immich_sync::upload::UploadModel;
use immich_sync::upload_view::UploadView;
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
    let upload_paths = paths.clone();
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
            let store_model = cx.new(|cx| immich_sync::store::StoreModel::open(&upload_paths, cx));
            let upload_model =
                cx.new(|cx| UploadModel::new(connection.clone(), store_model.clone(), cx));
            let upload = cx.new(|cx| UploadView::new(upload_model, window, cx));
            let engine_model = cx.new(|cx| {
                immich_sync::engine::EngineModel::new(
                    connection.clone(),
                    store_model.clone(),
                    &upload_paths,
                    cx,
                )
            });
            let failed = cx.new(|cx| {
                immich_sync::failed_view::FailedView::new(engine_model.clone(), window, cx)
            });
            let diagnostics_model = cx.new(|cx| {
                immich_sync::diagnostics::DiagnosticsModel::new(
                    store_model.clone(),
                    engine_model.clone(),
                    connection.clone(),
                    upload_paths.log_directory(),
                    cx,
                )
            });
            let diagnostics = cx.new(|cx| {
                immich_sync::diagnostics_view::DiagnosticsView::new(diagnostics_model, cx)
            });
            let review =
                cx.new(|cx| immich_sync::review_view::ReviewView::new(engine_model, window, cx));
            cx.new(|cx| {
                AppShell::new(
                    dashboard,
                    settings,
                    upload,
                    failed,
                    diagnostics,
                    review,
                    connection.clone(),
                    cx,
                )
            })
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

/// One log line of the length a round asks for: short, medium, or long
/// enough that the row has to truncate it.
fn line_of(round: usize, index: usize) -> String {
    let width = match (round + index) % 3 {
        1 => 90,
        2 => 400,
        _ => 12,
    };
    format!("round {round} line {index} {}", "x".repeat(width))
}

/// The tail rows the harness can still see, with their index and snapshot. A
/// row below the scroll box is not painted, so the harness cannot find it;
/// every row it does find must be full height.
fn tail_rows(window: &Window, count: usize) -> Vec<(usize, ElementSnapshot)> {
    let mut rows = Vec::new();
    for index in 0..count {
        let id = ElementId::Name(format!("logs-line-{index}").into());
        if let Some(snapshot) = window.try_find(id) {
            rows.push((index, snapshot));
        }
    }
    rows
}

/// Repeated refreshes with lines appended and truncated between them must
/// always lay the newest tail out as single-line rows, however many lines the
/// box has to scroll (#38).
///
/// The failing paint the issue reports is a row squashed into a thin strip: the
/// box caps its height, so a flex column that lets its rows shrink squeezes
/// every line into the cap instead of overflowing. A squashed row's bounds are
/// shorter than the height one line of the same style needs, which is what this
/// test measures: the height a one-line tail gives its row is the independent
/// truth, and no row of any later tail may be shorter than it.
#[gpui_kit::test]
fn ten_refreshes_keep_every_row_at_full_height(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    write_log(&paths, &["the one line a fresh tail holds"]);
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_shell(cx, store, paths.clone());

    let mut one_line = 0.;
    cx.update_window(window, |_, window, cx| {
        show_logs(window, cx);
        one_line = window.find("logs-line-0").bounds().size.height.into();
    })
    .expect("update the shell window");
    assert!(
        one_line > 8.,
        "one line of the tail is {one_line}px tall, tall enough for a line of text"
    );

    // Appends and truncations alternate, so a round that adds lines is followed
    // by one that takes them away again.
    for (round, count) in [3_usize, 21, 60, 5, 200, 1, 40, 12, 120, 8]
        .into_iter()
        .enumerate()
    {
        let lines: Vec<String> = (0..count).map(|index| line_of(round, index)).collect();
        write_log(
            &paths,
            &lines.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        cx.update_window(window, |_, window, cx| {
            window.click("logs-refresh", cx);
            let rows = tail_rows(window, count);
            assert!(
                !rows.is_empty(),
                "round {round}: {count} lines paint at least their first row"
            );
            for (position, (index, row)) in rows.iter().enumerate() {
                assert_eq!(
                    *index, position,
                    "round {round}: the painted rows start at the top of the tail and leave no gap"
                );
                let bounds = row.bounds();
                assert_eq!(
                    row.label(),
                    Some(lines[*index].as_str()),
                    "round {round}: logs-line-{index} shows its own line"
                );
                assert!(
                    f32::from(bounds.size.height) >= one_line - 0.5,
                    "round {round}: logs-line-{index} is {}px tall, not the {one_line}px one \
                     line needs: the row is squashed and its glyphs are clipped",
                    f32::from(bounds.size.height)
                );
                if let Some((previous_index, previous)) = rows.get(position.wrapping_sub(1)) {
                    assert!(
                        bounds.origin.y
                            >= previous.bounds().origin.y + previous.bounds().size.height,
                        "round {round}: logs-line-{index} overlaps logs-line-{previous_index}"
                    );
                }
            }
            let box_bounds = window.find("logs-lines").bounds();
            assert!(
                f32::from(box_bounds.size.height) <= 24. * 16. + 0.5,
                "round {round}: the tail keeps its 24rem cap"
            );
        })
        .expect("update the shell window");
    }

    // The box that overflows scrolls: the lines a full tail holds beyond the
    // first screenful are reachable, at full height, with the newest last.
    let lines: Vec<String> = (0..200).map(|index| line_of(11, index)).collect();
    write_log(
        &paths,
        &lines.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    cx.update_window(window, |_, window, cx| {
        window.click("logs-refresh", cx);
        window.scroll(
            "logs-lines",
            ScrollDelta::Pixels(point(px(0.), px(-4000.))),
            cx,
        );
        window.render_frame(cx);
        let newest = window.find(ElementId::Name("logs-line-199".into()));
        assert_eq!(
            newest.label(),
            Some(lines[199].as_str()),
            "scrolling the tail reaches the newest line"
        );
        assert!(
            f32::from(newest.bounds().size.height) >= one_line - 0.5,
            "the newest line of a full tail keeps the height one line needs"
        );
    })
    .expect("update the shell window");
}

#[gpui_kit::test]
fn every_logs_control_is_named_and_keyboard_reachable(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    // One line, so Copy all is enabled and therefore a tab stop.
    write_log(&paths, &["a stable line"]);
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
            ("logs-copy-all", "Copy every displayed log line"),
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
            "logs-copy-all",
        ] {
            tab_to(window, id, cx);
        }
    })
    .expect("update the shell window");
}

#[gpui_kit::test]
async fn choosing_a_level_persists_it_in_the_settings_file(cx: &mut TestAppContext) {
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

    // The level write runs on the background executor (N2), so the test waits
    // for it to land instead of reading the file in the same tick.
    let mut observed = LogLevel::Info;
    for _ in 0..600 {
        observed = SettingsStore::open(paths.config_file.clone())
            .settings()
            .log_level;
        if observed == LogLevel::Debug {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert_eq!(
        observed,
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

#[gpui_kit::test]
fn a_drag_across_lines_and_ctrl_c_copies_the_selection(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    write_log(
        &paths,
        &[
            "alpha first line",
            "bravo second line",
            "charlie third line",
        ],
    );
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_shell(cx, store, paths);

    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("SENTINEL".into())));
    cx.update_window(window, |_, window, cx| {
        show_logs(window, cx);
        // `show_logs` leaves focus on `nav-logs`, and the Root's Ctrl+C binding
        // fires for any focus inside it, so leaving that focus in place would
        // let the copy pass without the tail ever taking focus. Blur it first:
        // the drag must be what puts keyboard focus inside the tail.
        window.blur(cx);
        window.render_frame(cx);
        let first = window.find("logs-line-0").bounds();
        let last = window.find("logs-line-2").bounds();
        window.drag(
            point(
                first.origin.x + px(2.),
                first.origin.y + first.size.height / 2.,
            ),
            point(
                last.origin.x + last.size.width - px(2.),
                last.origin.y + last.size.height / 2.,
            ),
            cx,
        );
        assert_eq!(
            window.find("logs-lines").focused(),
            Some(true),
            "the dragged tail holds keyboard focus, so Ctrl+C can reach it"
        );
        window.press("ctrl-c", cx);
    })
    .expect("update the shell window");

    let copied = cx
        .update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .unwrap_or_default();
    assert_eq!(
        copied, "alpha first line\nbravo second line\ncharlie third line",
        "Ctrl+C copies every dragged line, in order"
    );
}

#[gpui_kit::test]
fn copy_all_writes_every_displayed_line_in_order(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    write_log(
        &paths,
        &[
            "alpha first line",
            "",
            "charlie third line",
            "delta fourth line",
        ],
    );
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_shell(cx, store, paths);

    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("SENTINEL".into())));
    cx.update_window(window, |_, window, cx| {
        show_logs(window, cx);
        window.click("logs-copy-all", cx);
    })
    .expect("update the shell window");

    let copied = cx
        .update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .unwrap_or_default();
    assert_eq!(
        copied, "alpha first line\n\ncharlie third line\ndelta fourth line",
        "Copy all keeps every displayed line, blank ones included, in order"
    );
}

#[gpui_kit::test]
fn copy_all_with_an_empty_tail_copies_nothing_and_does_not_panic(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_shell(cx, store, paths);

    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("SENTINEL".into())));
    cx.update_window(window, |_, window, cx| {
        show_logs(window, cx);
        assert!(window.find("logs-empty").visible(), "an empty tail says so");
        assert_eq!(
            window.find("logs-copy-all").label(),
            Some("Copy every displayed log line"),
            "the control is there and named"
        );
        // The control is built disabled, but a disabled `Button`'s state is not
        // observable here: GPUI has no aria-disabled setter for stateful
        // elements (gpui-base `button.rs` records the same gap). Clicking it
        // must therefore be the check: no panic, and nothing copied.
        window.click("logs-copy-all", cx);
    })
    .expect("update the shell window");

    let copied = cx
        .update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .unwrap_or_default();
    assert_eq!(copied, "SENTINEL", "the clipboard is untouched");
}
