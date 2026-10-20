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
use std::time::{Duration, SystemTime};

use gpui_kit::base::TextSelection;
use gpui_kit::test::{ElementSnapshot, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ClipboardItem, ElementId, Entity, Point,
    ScrollDelta, TestAppContext, Window, WindowBounds, WindowOptions, point, px, size,
};
use immich_sync::autostart::{Autostart, UnavailableAutostart};
use immich_sync::connection::{ConnectionModel, profile_from_settings};
use immich_sync::dashboard::Dashboard;
use immich_sync::logging::{LOG_PREFIX, LogLevel};
use immich_sync::logs_view::{LIVE_TAIL_POLL, LogsView};
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
    write_named_log(paths, "immich-sync.log.2026-01-01", lines)
}

/// Writes one named log file into the state directory and returns its path.
fn write_named_log(paths: &AppPaths, name: &str, lines: &[&str]) -> PathBuf {
    let directory = paths.log_directory();
    fs::create_dir_all(&directory).expect("create the log directory");
    let path = directory.join(name);
    fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write the log file");
    path
}

/// Ages a log file, so the rotation's newest-file rule has something to pick.
fn age(path: &Path, by: Duration) {
    fs::File::options()
        .write(true)
        .open(path)
        .expect("open the log file")
        .set_modified(SystemTime::now() - by)
        .expect("age the log file");
}

/// Opens the real shell over the real settings and dashboard views.
fn open_shell(
    cx: &mut TestAppContext,
    store: SettingsStore,
    paths: AppPaths,
) -> (AnyWindowHandle, Entity<AppShell>) {
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
    })
}

/// Opens a window on the Logs view alone, so a test can keep the view rendered
/// while the shell would have it hidden.
fn open_logs_view(
    cx: &mut TestAppContext,
    store: SettingsStore,
    paths: AppPaths,
) -> (AnyWindowHandle, Entity<LogsView>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        let profile = profile_from_settings(store.settings());
        let connection = cx.new(|_| {
            let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(profile, credentials)
        });
        gpui_kit::open_window(window_options(), cx, |window, cx| {
            let settings = cx.new(|cx| {
                SettingsView::new(store, paths, autostart(), connection.clone(), window, cx)
            });
            cx.new(|cx| LogsView::new(settings, cx))
        })
        .expect("open the Logs window")
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

/// Switches the shell to the Logs surface through the view switch, then waits
/// for the read the shell starts with it.
fn show_logs(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "nav-logs", cx);
        window.press("enter", cx);
    })
    .expect("update the shell window");
    settle(cx, window);
}

/// Lets a tail read land on the UI thread and paints the frame it notifies.
fn settle(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .expect("update the shell window");
}

/// One tick of the live tail. The test clock stands in for the timer's second,
/// so no test sleeps and the poll is the one the real loop runs.
fn tick_live(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.executor().advance_clock(LIVE_TAIL_POLL);
    settle(cx, window);
}

#[test]
fn the_screenshot_view_override_names_a_surface() {
    assert_eq!(shell::view_for_name("dashboard"), Some(ViewMode::Dashboard));
    assert_eq!(shell::view_for_name("settings"), Some(ViewMode::Settings));
    assert_eq!(shell::view_for_name("logs"), Some(ViewMode::Logs));
    assert_eq!(shell::view_for_name("nonsense"), None);
}

#[gpui_kit::test]
async fn the_logs_entry_shows_the_location_and_the_recent_lines(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let directory = paths.log_directory().display().to_string();
    write_log(&paths, &["first line", "second line"]);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell(cx, store, paths);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.find("status-global").visible(),
            "the shell starts on the dashboard"
        );
    })
    .expect("update the shell window");

    show_logs(cx, window);

    cx.update_window(window, |_, window, _| {
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
async fn refresh_reads_the_lines_written_since_the_view_opened(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let log = write_log(&paths, &["first line"]);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell(cx, store, paths);

    show_logs(cx, window);
    cx.update_window(window, |_, window, _| {
        assert!(
            window.try_find("logs-line-1").is_none(),
            "only the one line exists so far"
        );
    })
    .expect("update the shell window");

    fs::write(&log, "first line\nsecond line\n").expect("append a line to the log file");

    cx.update_window(window, |_, window, cx| window.click("logs-refresh", cx))
        .expect("update the shell window");
    settle(cx, window);
    cx.update_window(window, |_, window, _| {
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

/// The text of the rows the harness can still see, in the order they appear,
/// with where each one sits. Two identical results are a view whose paused tail
/// did not move.
fn shown_rows(window: &Window, count: usize) -> Vec<(usize, String, f32)> {
    tail_rows(window, count)
        .into_iter()
        .filter(|(_, row)| row.visible())
        .map(|(index, row)| {
            (
                index,
                row.label().unwrap_or_default().to_string(),
                f32::from(row.bounds().origin.y),
            )
        })
        .collect()
}

/// A numbered tail of `count` lines, long enough that its box has to scroll.
fn long_tail(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("line {index:03} of the followed tail"))
        .collect()
}

/// Writes a numbered tail into the one log file.
fn write_lines(paths: &AppPaths, lines: &[String]) -> PathBuf {
    write_log(paths, &lines.iter().map(String::as_str).collect::<Vec<_>>())
}

/// Scrolls the tail with wheel events until one row is on screen.
fn scroll_tail_to(window: &mut Window, cx: &mut App, delta: f32, index: usize) {
    window.render_frame(cx);
    for _ in 0..60 {
        let id = ElementId::Name(format!("logs-line-{index}").into());
        if window.try_find(id).is_some_and(|row| row.visible()) {
            return;
        }
        window.scroll(
            "logs-lines",
            ScrollDelta::Pixels(point(px(0.), px(delta))),
            cx,
        );
    }
    panic!("logs-line-{index} never became visible in the tail");
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
async fn ten_refreshes_keep_every_row_at_full_height(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    write_log(&paths, &["the one line a fresh tail holds"]);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell(cx, store, paths.clone());

    show_logs(cx, window);
    let one_line: f32 = cx
        .update_window(window, |_, window, _| {
            window.find("logs-line-0").bounds().size.height.into()
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
        cx.update_window(window, |_, window, cx| window.click("logs-refresh", cx))
            .expect("update the shell window");
        settle(cx, window);
        cx.update_window(window, |_, window, _| {
            let rows = tail_rows(window, count);
            assert!(
                !rows.is_empty(),
                "round {round}: {count} lines paint at least one row"
            );
            // A refresh pins the tail to its end, so the painted rows are the
            // last ones the box holds: they must still be contiguous, in order,
            // and full height.
            let first = rows[0].0;
            for (position, (index, row)) in rows.iter().enumerate() {
                assert_eq!(
                    *index,
                    first + position,
                    "round {round}: the painted rows leave no gap"
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
                if let Some((previous_index, previous)) =
                    position.checked_sub(1).and_then(|at| rows.get(at))
                {
                    assert!(
                        bounds.origin.y
                            >= previous.bounds().origin.y + previous.bounds().size.height,
                        "round {round}: logs-line-{index} overlaps logs-line-{previous_index}"
                    );
                }
            }
            let cap = f32::from(window.rem_size()) * 24.;
            assert!(
                f32::from(window.find("logs-lines").bounds().size.height) <= cap + 0.5,
                "round {round}: the tail keeps its 24rem cap"
            );
        })
        .expect("update the shell window");
    }

    // The box that overflows scrolls: a refresh lands on the newest line, and
    // the rows below the fold in both directions stay reachable and full
    // height.
    let lines: Vec<String> = (0..200).map(|index| line_of(11, index)).collect();
    write_log(
        &paths,
        &lines.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    cx.update_window(window, |_, window, cx| window.click("logs-refresh", cx))
        .expect("update the shell window");
    settle(cx, window);
    cx.update_window(window, |_, window, cx| {
        let newest = window.find(ElementId::Name("logs-line-199".into()));
        assert_eq!(
            newest.label(),
            Some(lines[199].as_str()),
            "a refreshed tail starts at its newest line"
        );
        assert!(
            f32::from(newest.bounds().size.height) >= one_line - 0.5,
            "the newest line of a full tail keeps the height one line needs"
        );

        // A row in the middle of the tail starts out above the fold.
        assert!(
            !window
                .try_find(ElementId::Name("logs-line-100".into()))
                .is_some_and(|row| row.visible()),
            "logs-line-100 already fits, so this tail does not prove scrolling"
        );
        scroll_tail_to(window, cx, 240., 100);
        let middle = window.find(ElementId::Name("logs-line-100".into()));
        assert_eq!(
            middle.label(),
            Some(lines[100].as_str()),
            "scrolling up inside the tail reaches the middle"
        );
        assert!(
            f32::from(middle.bounds().size.height) >= one_line - 0.5,
            "a middle row keeps the height one line needs"
        );
    })
    .expect("update the shell window");
}

#[gpui_kit::test]
async fn every_logs_control_is_named_and_keyboard_reachable(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    // One line, so Copy all is enabled and therefore a tab stop.
    write_log(&paths, &["a stable line"]);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell(cx, store, paths);

    show_logs(cx, window);
    cx.update_window(window, |_, window, cx| {
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
    let (window, _shell) = open_shell(cx, store, paths.clone());

    show_logs(cx, window);
    cx.update_window(window, |_, window, cx| window.click("logs-level-debug", cx))
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
async fn the_logs_view_renders_in_both_themes(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    write_log(&paths, &["a stable line"]);

    for system in [theme::SystemTheme::Dark, theme::SystemTheme::Light] {
        let store = SettingsStore::open(paths.config_file.clone());
        let (window, _shell) = open_shell(cx, store, paths.clone());
        cx.update(|cx| theme::apply_system_theme(system, cx));
        show_logs(cx, window);
        cx.update_window(window, |_, window, _| {
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
async fn a_drag_across_lines_and_ctrl_c_copies_the_selection(cx: &mut TestAppContext) {
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
    let (window, _shell) = open_shell(cx, store, paths);

    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("SENTINEL".into())));
    show_logs(cx, window);
    cx.update_window(window, |_, window, cx| {
        // `show_logs` leaves focus on `nav-logs`, and the Root's Ctrl+C binding
        // fires for any focus inside it, so leaving that focus in place would
        // let the copy pass without the tail ever taking focus. Blur it first:
        // the drag must be what puts keyboard focus inside the tail.
        window.blur(cx);
        window.render_frame(cx);
        drag_across(window, cx, 0, 2);
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
async fn a_refresh_clears_the_old_selection_and_the_new_tail_still_copies(cx: &mut TestAppContext) {
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
    let (window, _shell) = open_shell(cx, store, paths.clone());

    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("SENTINEL".into())));
    show_logs(cx, window);
    cx.update_window(window, |_, window, cx| {
        window.blur(cx);
        window.render_frame(cx);
        drag_across(window, cx, 0, 2);
        assert!(
            TextSelection::has_selection(window, cx),
            "the drag selects the first tail"
        );
    })
    .expect("update the shell window");

    write_log(
        &paths,
        &[
            "delta after the refresh",
            "echo after the refresh",
            "foxtrot after the refresh",
            "golf after the refresh",
        ],
    );
    // Refresh from the keyboard: a click on the button would clear the
    // selection on mouse-down, and the criterion must hold for the refresh
    // itself, not for the pointer event that asked for it.
    cx.update_window(window, |_, window, cx| {
        tab_to(window, "logs-refresh", cx);
        window.press("enter", cx);
    })
    .expect("update the shell window");
    settle(cx, window);
    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            window.find(ElementId::Name("logs-line-0".into())).label(),
            Some("delta after the refresh"),
            "the keyboard refresh replaces the tail"
        );
        assert!(
            !TextSelection::has_selection(window, cx),
            "a refresh clears the selection the old tail held"
        );

        // The new tail must copy as the text now on screen, not the text the
        // refreshed rows used to hold.
        window.blur(cx);
        window.render_frame(cx);
        drag_across(window, cx, 0, 3);
        window.press("ctrl-c", cx);
    })
    .expect("update the shell window");

    let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(
        copied.as_deref(),
        Some(
            "delta after the refresh\necho after the refresh\nfoxtrot after the refresh\ngolf after the refresh"
        ),
        "Ctrl+C copies the refreshed tail, in order"
    );

    // A blank line survives Copy all, which takes the tail the view holds
    // rather than the selection layer's projection.
    write_log(&paths, &["one", "", "three"]);
    cx.update_window(window, |_, window, cx| window.click("logs-refresh", cx))
        .expect("update the shell window");
    settle(cx, window);
    cx.update_window(window, |_, window, cx| window.click("logs-copy-all", cx))
        .expect("update the shell window");

    let copied = cx
        .update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .unwrap_or_default();
    assert_eq!(
        copied, "one\n\nthree",
        "Copy all keeps the refreshed tail's blank line"
    );
}

/// Drags from the middle of one tail row to the end of another, as a pointer
/// gesture does, and renders the frame the selection needs.
fn drag_across(window: &mut Window, cx: &mut App, from: usize, to: usize) {
    let start = window
        .find(ElementId::Name(format!("logs-line-{from}").into()))
        .bounds();
    let end = window
        .find(ElementId::Name(format!("logs-line-{to}").into()))
        .bounds();
    window.drag(
        point(
            start.origin.x + px(2.),
            start.origin.y + start.size.height / 2.,
        ),
        point(
            end.origin.x + end.size.width - px(2.),
            end.origin.y + end.size.height / 2.,
        ),
        cx,
    );
}

#[gpui_kit::test]
async fn copy_all_writes_every_displayed_line_in_order(cx: &mut TestAppContext) {
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
    let (window, _shell) = open_shell(cx, store, paths);

    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("SENTINEL".into())));
    show_logs(cx, window);
    cx.update_window(window, |_, window, cx| window.click("logs-copy-all", cx))
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
async fn copy_all_with_an_empty_tail_copies_nothing_and_does_not_panic(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell(cx, store, paths);

    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("SENTINEL".into())));
    show_logs(cx, window);
    cx.update_window(window, |_, window, cx| {
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

#[gpui_kit::test]
async fn a_new_log_line_reaches_the_view_without_a_click(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let log = write_log(&paths, &["the line the view opened on"]);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell(cx, store, paths);

    show_logs(cx, window);
    cx.update_window(window, |_, window, _| {
        assert_eq!(
            window.find("logs-line-0").label(),
            Some("the line the view opened on"),
            "the surface opened on the tail it read"
        );
    })
    .expect("update the shell window");

    fs::write(
        &log,
        "the line the view opened on\na line nobody clicked for\n",
    )
    .expect("append a line to the log file");
    tick_live(cx, window);

    cx.update_window(window, |_, window, _| {
        assert_eq!(
            window.find("logs-line-1").label(),
            Some("a line nobody clicked for"),
            "one live poll shows the appended line, with no click and no Refresh"
        );
    })
    .expect("update the shell window");
}

#[gpui_kit::test]
async fn scrolling_up_pauses_the_tail_and_counts_the_lines_it_misses(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut lines = long_tail(60);
    write_lines(&paths, &lines);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell(cx, store, paths.clone());

    show_logs(cx, window);
    cx.update_window(window, |_, window, _| {
        assert!(
            window
                .find(ElementId::Name("logs-line-59".into()))
                .visible(),
            "the view opens on the newest line, at the tail's end"
        );
        assert!(
            window.try_find("logs-paused").is_none(),
            "the paused control is not there while the tail follows"
        );
    })
    .expect("update the shell window");

    // Wheel up, away from the end of the tail.
    cx.update_window(window, |_, window, cx| {
        window.scroll(
            "logs-lines",
            ScrollDelta::Pixels(point(px(0.), px(240.))),
            cx,
        );
    })
    .expect("update the shell window");
    tick_live(cx, window);
    cx.update_window(window, |_, window, _| {
        assert!(
            window.try_find("logs-paused").is_some(),
            "scrolling up pauses the tail"
        );
        assert_eq!(
            window.find("logs-jump-latest").label(),
            Some("Jump to latest"),
            "nothing has arrived yet"
        );
    })
    .expect("update the shell window");

    let before: Vec<(usize, String, f32)> = cx
        .update_window(window, |_, window, _| shown_rows(window, 60))
        .expect("update the shell window");
    assert!(
        !before.is_empty(),
        "the paused tail still shows the rows the user scrolled to"
    );

    for index in 0..3 {
        lines.push(format!("line appended while paused {index}"));
    }
    write_lines(&paths, &lines);
    tick_live(cx, window);

    let after = cx
        .update_window(window, |_, window, _| shown_rows(window, lines.len()))
        .expect("update the shell window");
    assert_eq!(
        after, before,
        "the paused tail keeps every line exactly where the user left it"
    );
    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            window.find("logs-jump-latest").label(),
            Some("3 new lines · Jump to latest"),
            "the control counts the lines that arrived"
        );
        tab_to(window, "logs-jump-latest", cx);
    })
    .expect("update the shell window");

    // Selection and Ctrl+C keep working while the tail is paused. The rows in
    // the middle of the visible tail are clear of its clipped edges.
    let middle = before.len() / 2;
    let (first, second) = (before[middle].0, before[middle + 1].0);
    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("SENTINEL".into())));
    cx.update_window(window, |_, window, cx| {
        window.blur(cx);
        window.render_frame(cx);
        drag_across(window, cx, first, second);
        window.press("ctrl-c", cx);
    })
    .expect("update the shell window");
    let copied = cx
        .update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .unwrap_or_default();
    assert_eq!(
        copied,
        format!("{}\n{}", lines[first], lines[second]),
        "the paused tail still copies the lines the drag covers"
    );
}

#[gpui_kit::test]
async fn jump_to_latest_loads_the_current_tail_and_follows_again(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut lines = long_tail(60);
    write_lines(&paths, &lines);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell(cx, store, paths.clone());

    show_logs(cx, window);
    cx.update_window(window, |_, window, cx| {
        window.scroll(
            "logs-lines",
            ScrollDelta::Pixels(point(px(0.), px(240.))),
            cx,
        );
    })
    .expect("update the shell window");
    tick_live(cx, window);
    for index in 0..5 {
        lines.push(format!("line that arrived while paused {index}"));
    }
    write_lines(&paths, &lines);
    tick_live(cx, window);
    cx.update_window(window, |_, window, _| {
        assert_eq!(
            window.find("logs-jump-latest").label(),
            Some("5 new lines · Jump to latest"),
            "the jump control counts the lines it will show"
        );
    })
    .expect("update the shell window");

    cx.update_window(window, |_, window, cx| window.click("logs-jump-latest", cx))
        .expect("update the shell window");
    settle(cx, window);
    let newest = lines.len() - 1;
    cx.update_window(window, |_, window, _| {
        let row = window.find(ElementId::Name(format!("logs-line-{newest}").into()));
        assert!(row.visible(), "the jump lands on the newest line");
        assert_eq!(
            row.label(),
            Some(lines[newest].as_str()),
            "it shows the current tail"
        );
        assert!(
            window.try_find("logs-paused").is_none(),
            "following resumes, so the paused control is gone"
        );
    })
    .expect("update the shell window");

    // Following again: the next line arrives with no click at all.
    lines.push("the line after the jump".to_string());
    let followed = lines.len() - 1;
    write_lines(&paths, &lines);
    tick_live(cx, window);
    cx.update_window(window, |_, window, _| {
        assert_eq!(
            window
                .find(ElementId::Name(format!("logs-line-{followed}").into()))
                .label(),
            Some("the line after the jump"),
            "the resumed tail follows new lines"
        );
    })
    .expect("update the shell window");

    // And the resumed tail still copies.
    let (first, last) = (lines.len() - 2, lines.len() - 1);
    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("SENTINEL".into())));
    cx.update_window(window, |_, window, cx| {
        window.blur(cx);
        window.render_frame(cx);
        drag_across(window, cx, first, last);
        window.press("ctrl-c", cx);
    })
    .expect("update the shell window");
    let copied = cx
        .update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .unwrap_or_default();
    assert_eq!(
        copied,
        format!("{}\n{}", lines[first], lines[last]),
        "the tail copies after a pause and a resume"
    );
}

#[gpui_kit::test]
async fn a_rotated_log_file_becomes_the_followed_file(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let old = write_log(&paths, &["a line from the file being written"]);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell(cx, store, paths.clone());

    show_logs(cx, window);
    cx.update_window(window, |_, window, _| {
        assert_eq!(
            window.find("logs-line-0").label(),
            Some("a line from the file being written"),
            "the view opened on the file being written"
        );
    })
    .expect("update the shell window");

    // The rotation opens a new file and leaves the old one behind.
    age(&old, Duration::from_secs(60));
    write_named_log(
        &paths,
        &format!("{LOG_PREFIX}.2026-01-02"),
        &["a line from the rotated file"],
    );
    tick_live(cx, window);

    cx.update_window(window, |_, window, _| {
        assert_eq!(
            window.find("logs-line-0").label(),
            Some("a line from the rotated file"),
            "the live tail follows the newest file"
        );
        assert_eq!(
            window.find("logs-path-current-value").label(),
            Some("immich-sync.log.2026-01-02"),
            "the view names the file it follows"
        );
    })
    .expect("update the shell window");
}

#[gpui_kit::test]
async fn an_empty_log_fills_in_when_the_first_line_is_written(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell(cx, store, paths.clone());

    show_logs(cx, window);
    cx.update_window(window, |_, window, _| {
        assert!(
            window.find("logs-empty").visible(),
            "a log with no lines says so"
        );
        assert_eq!(
            window.find("logs-path-current-value").label(),
            Some("No log file yet."),
            "and names no file"
        );
    })
    .expect("update the shell window");

    write_log(&paths, &["the first line ever written"]);
    tick_live(cx, window);

    cx.update_window(window, |_, window, _| {
        assert!(
            window.try_find("logs-empty").is_none(),
            "the empty state clears as soon as the first line arrives"
        );
        assert_eq!(
            window.find("logs-line-0").label(),
            Some("the first line ever written"),
            "the first line appears without a click"
        );
    })
    .expect("update the shell window");
}

#[gpui_kit::test]
async fn leaving_the_logs_surface_stops_the_live_tail(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    write_log(&paths, &["a stable line"]);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, shell) = open_shell(cx, store, paths);
    let logs = cx.update(|cx| shell.read(cx).logs());

    assert!(
        !cx.update(|cx| logs.read(cx).is_live()),
        "the dashboard starts no live tail"
    );
    show_logs(cx, window);
    assert!(
        cx.update(|cx| logs.read(cx).is_live()),
        "showing the Logs surface starts it"
    );

    cx.update_window(window, |_, window, cx| window.click("nav-dashboard", cx))
        .expect("update the shell window");
    settle(cx, window);
    assert!(
        !cx.update(|cx| logs.read(cx).is_live()),
        "switching away stops it, so a hidden surface reads nothing"
    );
}

#[gpui_kit::test]
async fn a_hidden_surface_reads_no_log_file(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    write_log(&paths, &["a line while the surface is visible"]);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, logs) = open_logs_view(cx, store, paths.clone());

    cx.update(|cx| logs.update(cx, |logs, cx| logs.set_visible(true, cx)));
    settle(cx, window);
    cx.update_window(window, |_, window, _| {
        assert_eq!(
            window.find("logs-line-0").label(),
            Some("a line while the surface is visible"),
            "a visible surface reads its tail"
        );
    })
    .expect("update the shell window");

    // The log directory becomes unreadable, so any read from here on reports
    // itself on the surface.
    let directory = paths.log_directory();
    fs::remove_dir_all(&directory).expect("remove the log directory");
    fs::write(&directory, "not a directory").expect("replace the directory with a file");

    cx.update(|cx| logs.update(cx, |logs, cx| logs.set_visible(false, cx)));
    cx.executor().advance_clock(LIVE_TAIL_POLL * 5);
    settle(cx, window);
    cx.update_window(window, |_, window, _| {
        assert!(
            window.try_find("logs-problem").is_none(),
            "a hidden surface read the log file"
        );
        assert_eq!(
            window.find("logs-line-0").label(),
            Some("a line while the surface is visible"),
            "and it left the displayed tail alone"
        );
    })
    .expect("update the shell window");

    // Showing it again reads once, and the failure is what the surface shows.
    cx.update(|cx| logs.update(cx, |logs, cx| logs.set_visible(true, cx)));
    settle(cx, window);
    cx.update_window(window, |_, window, _| {
        assert!(
            window.find("logs-problem").visible(),
            "showing the surface reads, and a read that fails says so"
        );
    })
    .expect("update the shell window");
}
