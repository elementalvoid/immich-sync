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
use gpui_kit::base::test_support::snapshots;
use gpui_kit::test::{ElementSnapshot, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ClipboardItem, ElementId, Entity, Point, Role,
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

/// The copy chord the Root binds on this platform.
///
/// gpui-base binds `cmd-c` on macOS and `ctrl-c` elsewhere (`root.rs`), so a
/// test that hardcodes `ctrl-c` copies nothing on macOS.
fn copy_chord() -> &'static str {
    if cfg!(target_os = "macos") {
        "cmd-c"
    } else {
        "ctrl-c"
    }
}

fn temporary_paths(dir: &Path) -> AppPaths {
    AppPaths {
        config_file: dir.join("config").join("settings.toml"),
        state_db: dir.join("data").join("state.db"),
        autostart_dir: dir.join("config").join("autostart"),
    }
}

fn window_options() -> WindowOptions {
    window_options_at(1200., 1400.)
}

fn window_options_at(width: f32, height: f32) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            // Tall enough for the location card, the level control, and some
            // log lines at once.
            size: size(px(width), px(height)),
        })),
        ..Default::default()
    }
}

/// The sentence the empty tail explains itself with (#77).
const EMPTY_DESCRIPTION: &str = "Lines appear as the client works.";

/// The empty tail's one action, named for where it sits (#77).
const EMPTY_ACTION_LABEL: &str = "Refresh the log tail";

/// How many observed elements announce `text`: as a name, as a value, or as the
/// description a screen reader reads after the name.
fn announced(window: &Window, text: &str) -> usize {
    snapshots(window)
        .into_iter()
        .filter(|element| {
            element.label() == Some(text)
                || element.value() == Some(text)
                || element.description() == Some(text)
        })
        .count()
}

/// The accessible name of every control in the view, in tree order.
fn control_names(window: &Window) -> Vec<String> {
    snapshots(window)
        .into_iter()
        .filter(|element| element.role() == Some(Role::Button))
        .filter_map(|element| element.label().map(str::to_owned))
        .collect()
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
    open_shell_at(cx, store, paths, (1200., 1400.))
}

/// The same shell in a window of the given size, so a test can check what the
/// layout does at 900x600 and in a window too short for the whole page.
fn open_shell_at(
    cx: &mut TestAppContext,
    store: SettingsStore,
    paths: AppPaths,
    (width, height): (f32, f32),
) -> (AnyWindowHandle, Entity<AppShell>) {
    let upload_paths = paths.clone();
    cx.update(|cx| {
        gpui_kit::init(cx);
        let profile = profile_from_settings(store.settings());
        let connection = cx.new(|_| {
            let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(profile, credentials)
        });
        gpui_kit::open_window(window_options_at(width, height), cx, |window, cx| {
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

#[gpui_kit::test]
async fn an_empty_tail_is_one_status_inside_the_narrowest_window(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    // 520x480 is the shell's smallest window (#75).
    let (window, _shell) = open_shell_at(cx, store, paths, (520., 480.));

    show_logs(cx, window);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let empty = window.find("logs-empty");
        assert_eq!(
            empty.label(),
            Some("No log lines yet."),
            "#77: the empty tail states what it shows"
        );
        assert_eq!(
            empty.description(),
            Some(EMPTY_DESCRIPTION),
            "#77: the explanation is announced after the title, not instead of it"
        );
        assert_eq!(
            empty.role(),
            Some(Role::Status),
            "#77: the empty tail is a status for assistive technology"
        );
        assert!(
            empty.visible(),
            "#77: the empty tail is on screen: {empty:?}"
        );
        assert_eq!(
            announced(window, "No log lines yet."),
            1,
            "#77: the title is announced once"
        );
        assert_eq!(
            announced(window, EMPTY_DESCRIPTION),
            1,
            "#77: the explanation is announced once"
        );
        // The narrowest window must not clip the state it is showing.
        let size = window.viewport_size();
        assert!(
            empty.bounds().right() <= size.width && empty.bounds().bottom() <= size.height,
            "#77: the empty tail stays inside {size:?}: {:?}",
            empty.bounds()
        );
        assert!(
            window
                .try_find((ElementId::from("logs-empty"), "action"))
                .is_some(),
            "#77: the empty tail offers the one step it can honour"
        );
    })
    .expect("read the empty logs view");
}

#[gpui_kit::test]
async fn an_empty_tail_is_re_read_from_where_it_is_shown(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell(cx, store, paths.clone());

    show_logs(cx, window);
    cx.update_window(window, |_, window, cx| {
        assert!(window.find("logs-empty").visible(), "the tail starts empty");
        assert_eq!(
            window.find("logs-empty-refresh").label(),
            Some(EMPTY_ACTION_LABEL),
            "the empty tail's one action is named"
        );
        write_log(&paths, &["the first line ever written"]);
        window.click("logs-empty-refresh", cx);
    })
    .expect("update the shell window");

    settle(cx, window);
    cx.update_window(window, |_, window, _| {
        assert_eq!(
            window.find("logs-line-0").label(),
            Some("the first line ever written"),
            "the empty tail's action re-reads the file"
        );
        assert!(
            window.try_find("logs-empty").is_none(),
            "and the empty state clears with it"
        );
    })
    .expect("update the shell window");
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
            // The tail no longer has a fixed cap (#54): it takes the height
            // the Recent lines card has left below its rows, so it ends at
            // that card's inner bottom rather than at 24rem.
            let card = window.find("logs-lines-section").bounds();
            let tail = window.find("logs-lines").bounds();
            let card_inner_bottom =
                f32::from(card.origin.y + card.size.height) - CARD_PADDING - 1.;
            assert!(
                (f32::from(tail.origin.y + tail.size.height) - card_inner_bottom).abs() <= 2.,
                "round {round}: the tail ends at {}, the inner bottom of the card, not {card_inner_bottom}",
                f32::from(tail.origin.y + tail.size.height)
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
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell(cx, store, paths.clone());

    show_logs(cx, window);
    // The empty tail is the one state that shows both refreshes at once, so it
    // is where two controls could end up sharing one name (#77).
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("logs-empty-refresh").label(),
            Some(EMPTY_ACTION_LABEL),
            "the empty tail's action is named for where it sits"
        );
        let names = control_names(window);
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            names.len(),
            "two controls share an accessible name: {names:?}"
        );
        tab_to(window, "logs-empty-refresh", cx);
        // Walk the tab order below from its start again.
        window.blur(cx);
    })
    .expect("update the shell window");

    // One line, so Copy all is enabled and therefore a tab stop.
    write_log(&paths, &["a stable line"]);
    cx.update_window(window, |_, window, cx| window.click("logs-refresh", cx))
        .expect("update the shell window");
    settle(cx, window);

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
        window.press(copy_chord(), cx);
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
        window.press(copy_chord(), cx);
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
async fn a_selection_pauses_the_tail_instead_of_replacing_the_rows_under_it(
    cx: &mut TestAppContext,
) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut lines = long_tail(6);
    write_lines(&paths, &lines);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell(cx, store, paths.clone());

    show_logs(cx, window);
    cx.update_window(window, |_, window, cx| {
        window.blur(cx);
        window.render_frame(cx);
        drag_across(window, cx, 0, 2);
        assert!(
            TextSelection::has_selection(window, cx),
            "the drag selects the followed tail"
        );
    })
    .expect("update the shell window");
    let before: Vec<(usize, String, f32)> = cx
        .update_window(window, |_, window, _| shown_rows(window, lines.len()))
        .expect("update the shell window");
    assert!(!before.is_empty(), "the followed tail shows its rows");

    for index in 0..2 {
        lines.push(format!("line appended under the selection {index}"));
    }
    write_lines(&paths, &lines);
    tick_live(cx, window);

    let after = cx
        .update_window(window, |_, window, _| shown_rows(window, lines.len()))
        .expect("update the shell window");
    assert_eq!(
        after, before,
        "a live replace must not overwrite the text under an existing selection"
    );
    cx.update_window(window, |_, window, cx| {
        assert!(
            TextSelection::has_selection(window, cx),
            "the selection survives the poll"
        );
        assert!(
            window.try_find("logs-paused").is_some(),
            "a selection pauses the tail, so the jump control appears"
        );
        assert_eq!(
            window.find("logs-jump-latest").label(),
            Some("2 new lines · Jump to latest"),
            "the arrival is counted, not shown"
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
    cx.update_window(window, |_, window, _| {
        assert_eq!(
            window.find("logs-jump-latest").label(),
            Some("3 new lines · Jump to latest"),
            "the control counts the lines that arrived"
        );
    })
    .expect("update the shell window");

    // A poll with nothing written between ticks counts nothing: the window the
    // count measures against advances with every read while the rows stay
    // frozen, so the same arrival is not added again.
    tick_live(cx, window);
    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            window.find("logs-jump-latest").label(),
            Some("3 new lines · Jump to latest"),
            "an idle poll does not count the same lines twice"
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
        window.press(copy_chord(), cx);
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

/// The timer's read decides whether to follow before the file read finishes.
/// A scroll up while that read is in flight must keep the tail where the user
/// put it, or the read that lands pulls the rows back to the end.
#[gpui_kit::test]
async fn a_scroll_up_during_the_read_is_not_undone_when_it_lands(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut lines = long_tail(60);
    write_lines(&paths, &lines);
    let store = SettingsStore::open(paths.config_file.clone());
    // The Logs view alone: the only timer pending is its live poll, so the
    // three ticks below are the poll and the read it starts.
    let (window, logs) = open_logs_view(cx, store, paths.clone());
    cx.update(|cx| logs.update(cx, |logs, cx| logs.set_visible(true, cx)));
    settle(cx, window);
    let (at_end, paused) = cx
        .update_window(window, |_, window, _| {
            (
                window
                    .find(ElementId::Name("logs-line-59".into()))
                    .visible(),
                window.try_find("logs-paused").is_some(),
            )
        })
        .expect("update the Logs window");
    assert!(at_end, "the view opens at the tail's end");
    assert!(!paused, "nothing is paused yet");

    for index in 0..3 {
        lines.push(format!("line appended during the read {index}"));
    }
    write_lines(&paths, &lines);

    // Expire the poll timer, then run the poll. The tail is still at its end,
    // so the poll latches "follow" and its read is now in flight. The frame
    // the settle painted can leave work queued, and a random pick of two
    // runnables would not be the poll, so drain it first and pin the timer.
    cx.run_until_parked();
    let before_timer = cx.executor().now();
    assert!(
        cx.dispatcher.advance_clock_to_next_timer(),
        "the live poll timer is pending"
    );
    assert_eq!(
        cx.executor().now().duration_since(before_timer),
        LIVE_TAIL_POLL,
        "the next timer is the live poll, not another task's"
    );
    // One tick expires the timer, one completes the timer's own task and wakes
    // the poll, and one runs the poll: it reads the viewport as "following"
    // and starts the read. The read has not landed yet, so stop here.
    assert!(cx.dispatcher.tick(false), "the poll timer expires");
    assert!(cx.dispatcher.tick(false), "the timer wakes the poll");
    assert!(cx.dispatcher.tick(false), "the poll starts its read");
    cx.update_window(window, |_, window, _| {
        assert!(
            window
                .try_find(ElementId::Name("logs-line-60".into()))
                .is_none(),
            "the read has not landed yet"
        );
    })
    .expect("update the Logs window");

    // Scroll up, away from the end of the tail, before that read lands.
    cx.update_window(window, |_, window, cx| {
        window.scroll(
            "logs-lines",
            ScrollDelta::Pixels(point(px(0.), px(240.))),
            cx,
        );
    })
    .expect("update the Logs window");

    settle(cx, window);
    cx.update_window(window, |_, window, _| {
        assert!(
            window.try_find("logs-paused").is_some(),
            "the read that landed kept the tail where the user scrolled to"
        );
        assert!(
            window
                .try_find(ElementId::Name("logs-line-60".into()))
                .is_none(),
            "the read did not replace the rows the user scrolled away from"
        );
        assert_eq!(
            window.find("logs-jump-latest").label(),
            Some("3 new lines · Jump to latest"),
            "the arrival is counted instead"
        );
    })
    .expect("update the Logs window");
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
        window.press(copy_chord(), cx);
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

/// A read already in flight when the surface hides lands on no surface at all:
/// it must not fill the rows behind the one the user switched to.
#[gpui_kit::test]
async fn a_read_that_lands_after_the_surface_hides_leaves_the_rows_alone(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    write_log(&paths, &["a line appended while the read was out"]);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, logs) = open_logs_view(cx, store, paths.clone());

    // Showing starts a read, and hiding before it lands leaves it in flight.
    cx.update(|cx| logs.update(cx, |logs, cx| logs.set_visible(true, cx)));
    cx.update(|cx| logs.update(cx, |logs, cx| logs.set_visible(false, cx)));
    settle(cx, window);
    cx.update_window(window, |_, window, _| {
        assert!(
            window.try_find("logs-line-0").is_none(),
            "a read that lands while the surface is hidden changes nothing"
        );
    })
    .expect("update the shell window");

    // Showing it again reads, so the tail was never the hidden read's.
    cx.update(|cx| logs.update(cx, |logs, cx| logs.set_visible(true, cx)));
    settle(cx, window);
    cx.update_window(window, |_, window, _| {
        assert_eq!(
            window.find("logs-line-0").label(),
            Some("a line appended while the read was out"),
            "showing the surface reads it"
        );
    })
    .expect("update the shell window");
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

/// The window rectangle an element occupies, in window pixels.
///
/// The harness cannot read a scroll offset, but it can read where an element
/// was laid out, and every layout fact in this file is one of those.
fn window_rect(window: &Window, id: &'static str) -> (f32, f32, f32, f32) {
    let bounds = window.find(id).bounds();
    (
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.y),
        f32::from(bounds.size.width),
        f32::from(bounds.size.height),
    )
}

/// The Logs page pads its content by `p_4`, and a section card by `p_3`.
const PAGE_PADDING: f32 = 16.;
const CARD_PADDING: f32 = 12.;

/// Where the Recent lines card's body ends: its outside bottom, less the card's
/// padding and its 1px border.
fn pane_inner_bottom(window: &Window) -> f32 {
    let pane = window_rect(window, "logs-lines-section");
    pane.1 + pane.3 - CARD_PADDING - 1.
}

/// The tail pane owns the height the header, the location card, and the level
/// control leave, and the page around it has nothing left to scroll: the newest
/// line is on screen with no page scroll and no window resize (#54).
///
/// 900x1000 is the window the issue measured: with the 24rem cap the page
/// overflowed by ~209px, so the newest line sat below the window's edge.
#[gpui_kit::test]
async fn the_tail_pane_fills_the_height_left_below_the_level_control(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let lines = long_tail(200);
    write_lines(&paths, &lines);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell_at(cx, store, paths, (900., 1000.));
    show_logs(cx, window);

    cx.update_window(window, |_, window, _| {
        let view = window_rect(window, "logs-view");
        let level = window_rect(window, "logs-level");
        let pane = window_rect(window, "logs-lines-section");
        let tail = window_rect(window, "logs-lines");
        let (view_top, view_bottom) = (view.1, view.1 + view.3);
        let level_bottom = level.1 + level.3;
        let tail_bottom = tail.1 + tail.3;
        let pane_bottom = pane.1 + pane.3;

        assert!(
            tail.1 >= level_bottom,
            "the tail pane begins at y={} below the level control, which ends at {level_bottom}",
            tail.1
        );
        assert!(
            tail.3 > 150.,
            "the tail pane takes the remaining height, not the old 384px cap: it is {}px tall",
            tail.3
        );
        assert!(
            (pane_bottom - (view_bottom - PAGE_PADDING)).abs() <= 2.,
            "the Recent lines card ends at {pane_bottom}, at the bottom of the {view_top}..{view_bottom} page"
        );
        // The card pads its body by `p_3` and draws a 1px border.
        assert!(
            (tail_bottom - (pane_bottom - CARD_PADDING - 1.)).abs() <= 2.,
            "the tail box ends at {tail_bottom}, inside the card that ends at {pane_bottom}"
        );

        let newest = window.find(ElementId::Name(format!("logs-line-{}", lines.len() - 1).into()));
        assert!(
            newest.visible(),
            "the newest line is on screen at 900x1000 without scrolling the page first"
        );
    })
    .expect("update the shell window");
}

/// The scrollbar thumb a scroll box painted, as the frame painted it.
///
/// The bar is an overlay the harness cannot find by id, so this reads the quads
/// the frame painted: the thumb is the only filled strip narrower than the
/// 16px track at the box's right edge. A hidden bar is painted off the edge,
/// and a bar with nothing to scroll is not painted at all.
fn scrollbar_thumb(window: &Window, area: (f32, f32, f32, f32)) -> Option<(f32, f32, f32, f32)> {
    // The painted frame is in device pixels and the layout bounds are logical
    // ones, so the strip is measured in device pixels and reported in logical.
    let scale = window.scale_factor();
    let (left, top, width, height) = (
        area.0 * scale,
        area.1 * scale,
        area.2 * scale,
        area.3 * scale,
    );
    let (right, bottom) = (left + width, top + height);
    let track = 16. * scale;
    window.painted_quads().into_iter().find_map(|quad| {
        let (x, y) = (quad.bounds.origin.x.as_f32(), quad.bounds.origin.y.as_f32());
        let (w, h) = (
            quad.bounds.size.width.as_f32(),
            quad.bounds.size.height.as_f32(),
        );
        let in_track = w <= track / 2. && x >= right - track && x + w <= right + 0.5;
        let in_area = y >= top - 1. && y + h <= bottom + 1.;
        (in_track && in_area && h > 0.).then_some((x / scale, y / scale, w / scale, h / scale))
    })
}

/// At 900x600 the cards above the tail do not fit in the window, so that strip
/// scrolls as the fallback and the tail keeps its own minimum height: the
/// newest line is on screen without a resize and without scrolling first (#54).
#[gpui_kit::test]
async fn a_short_window_keeps_the_tail_on_screen_and_scrolls_the_cards_above_it(
    cx: &mut TestAppContext,
) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let lines = long_tail(200);
    write_lines(&paths, &lines);
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell_at(cx, store, paths, (900., 600.));
    show_logs(cx, window);

    cx.update_window(window, |_, window, cx| {
        let cards = window_rect(window, "logs-cards");
        let tail = window_rect(window, "logs-lines");
        assert!(
            cards.3 < 400.,
            "the cards above the tail are shorter than their content, so that strip scrolls: {}px",
            cards.3
        );
        assert!(
            cards.1 + cards.3 + 8. <= tail.1 + 1.,
            "the cards strip ends at {} above the tail, which starts at {}",
            cards.1 + cards.3,
            tail.1
        );
        assert!(
            tail.3 >= 126.,
            "the tail never collapses: it keeps at least its 8rem, and is {}px here",
            tail.3
        );
        let newest = window.find(ElementId::Name(
            format!("logs-line-{}", lines.len() - 1).into(),
        ));
        assert!(
            newest.visible(),
            "the newest line is on screen at 900x600 without resizing the window"
        );
        let level_before = window_rect(window, "logs-level");
        assert!(
            level_before.1 + level_before.3 > tail.1,
            "the level control is taller than the strip, so it is the part that scrolls"
        );

        // Two wheels down over the cards reach the strip's end, and the level
        // control then sits right above the tail: the fallback scroll keeps
        // every control reachable.
        for _ in 0..2 {
            window.scroll(
                "logs-cards",
                ScrollDelta::Pixels(point(px(0.), px(-240.))),
                cx,
            );
        }
        let level = window_rect(window, "logs-level");
        assert!(
            level.1 + level.3 <= window_rect(window, "logs-lines").1 + 1.,
            "the level control ends at {} where the tail begins",
            level.1 + level.3
        );
        assert_eq!(
            window_rect(window, "logs-lines"),
            tail,
            "scrolling the cards leaves the tail where it is"
        );
        assert_eq!(
            window
                .find(ElementId::Name(
                    format!("logs-line-{}", lines.len() - 1).into()
                ))
                .bounds(),
            newest.bounds(),
            "and leaves the newest line on screen"
        );
    })
    .expect("update the shell window");
}

/// At 900x600 the paused row appears below the tail, and its jump control must
/// stay fully inside the window: the Recent lines card keeps a floor tall
/// enough for the paused row plus the tail's own minimum (#54).
#[gpui_kit::test]
async fn a_paused_tail_keeps_the_jump_control_inside_a_short_window(cx: &mut TestAppContext) {
    // 900x600 is the window the review measured, and 720x480 is the shell's
    // minimum: the narrowest window wraps the paused row's labels the most.
    for window_size in [(900., 600.), (720., 480.)] {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = temporary_paths(temp.path());
        write_lines(&paths, &long_tail(200));
        let store = SettingsStore::open(paths.config_file.clone());
        let (window, _shell) = open_shell_at(cx, store, paths, window_size);
        show_logs(cx, window);

        // A wheel up in the tail pauses the live tail on the next poll tick,
        // which adds the paused row and its jump control below the lines.
        cx.update_window(window, |_, window, cx| {
            window.scroll(
                "logs-lines",
                ScrollDelta::Pixels(point(px(0.), px(400.))),
                cx,
            );
        })
        .expect("update the shell window");
        tick_live(cx, window);

        cx.update_window(window, |_, window, _| {
            assert!(
                window.find("logs-paused").visible(),
                "the paused tail draws its paused row"
            );
            let paused = window_rect(window, "logs-paused");
            assert!(
                paused.1 + paused.3 <= pane_inner_bottom(window) + 0.5,
                "the paused row ends at {} inside the card's body, which ends at {}",
                paused.1 + paused.3,
                pane_inner_bottom(window)
            );
            let jump = window.find("logs-jump-latest").bounds();
            let viewport = window.viewport_size();
            let (jump_right, jump_bottom) = (
                f32::from(jump.origin.x + jump.size.width),
                f32::from(jump.origin.y + jump.size.height),
            );
            assert!(
                jump_bottom <= f32::from(viewport.height) && jump_right <= f32::from(viewport.width),
                "the jump control at {jump:?} is fully inside the {viewport:?} viewport at {window_size:?}, not clipped by the page"
            );
        })
        .expect("update the shell window");
    }
}

/// A tail read that fails draws a notice inside the Recent lines card, and the
/// paused row sits below the tail. At the shell's minimum window each of those
/// rows is taller than the floor the other states left, so the card's floor must
/// cover every row it shows (#54).
///
/// The notice quotes the path of the failed log directory, and a long data
/// directory wraps it to more lines. The test builds a long one rather than
/// trusting `TMPDIR`, so the clamp and the geometry below do not depend on it.
#[gpui_kit::test]
async fn a_failed_tail_read_keeps_its_notice_and_paused_row_inside_the_card(
    cx: &mut TestAppContext,
) {
    // 900x600 is the window the review measured, and 720x480 is the shell's
    // minimum: the narrowest window wraps the notice and the paused row most.
    for window_size in [(900., 600.), (720., 480.)] {
        // A long prefix stands in for a long data directory, and it is long
        // enough that the unclamped notice wraps to four lines at both sizes,
        // two more than the floor covers. Its full text stays announced.
        let temp = tempfile::Builder::new()
            .prefix(&"long-data-directory-".repeat(10))
            .tempdir()
            .expect("create a temp dir");
        let paths = temporary_paths(temp.path());
        let directory = paths.log_directory();
        // The prefix's 200 bytes plus tempfile's 6 stay under NAME_MAX (255),
        // and a shorter path here would silently stop unguarding the clamp.
        assert!(
            directory.as_os_str().len() > 150,
            "the log directory {directory:?} is too short to wrap the notice past the floor"
        );
        write_lines(&paths, &long_tail(200));
        let store = SettingsStore::open(paths.config_file.clone());
        let (window, _shell) = open_shell_at(cx, store, paths.clone(), window_size);
        show_logs(cx, window);

        // The log directory stops being a directory, and the poll reports the
        // failed read on the surface while the tail still follows it.
        fs::remove_dir_all(&directory).expect("remove the log directory");
        fs::write(&directory, "not a directory").expect("replace the directory with a file");
        tick_live(cx, window);

        cx.update_window(window, |_, window, _| {
            assert!(
                window.try_find("logs-paused").is_none(),
                "a failed read alone does not pause the tail"
            );
            // The clamp is visual: the full message still names the path.
            assert!(
                window
                    .find("logs-problem")
                    .label()
                    .unwrap_or_default()
                    .contains(&directory.display().to_string()),
                "the notice still announces the whole message, including the log directory"
            );
            // The notice and the tail both end inside the card: the floor covers
            // the row the notice adds, or the tail would paint over the border.
            assert_inside_card(window, "logs-problem", window_rect(window, "logs-problem"));
            assert_inside_card(window, "logs-lines", window_rect(window, "logs-lines"));
        })
        .expect("update the shell window");

        // Wheel up: the next poll pauses the tail, and the failed read keeps its
        // notice above the paused row.
        cx.update_window(window, |_, window, cx| {
            window.scroll(
                "logs-lines",
                ScrollDelta::Pixels(point(px(0.), px(400.))),
                cx,
            );
        })
        .expect("update the shell window");
        tick_live(cx, window);

        cx.update_window(window, |_, window, _| {
            let paused = window_rect(window, "logs-paused");
            let notice = window_rect(window, "logs-problem");
            assert_eq!(
                window.find("logs-jump-latest").label(),
                Some("Jump to latest"),
                "the paused row is on screen with a read problem"
            );
            assert!(
                notice.1 + notice.3 <= paused.1,
                "the notice at {notice:?} sits above the paused row at {paused:?}"
            );
            for id in [
                "logs-problem",
                "logs-lines",
                "logs-paused",
                "logs-jump-latest",
            ] {
                assert_inside_card(window, id, window_rect(window, id));
            }
        })
        .expect("update the shell window");
    }
}

/// Asserts a row ends inside the Recent lines card's body and the viewport.
fn assert_inside_card(window: &Window, id: &'static str, rect: (f32, f32, f32, f32)) {
    let inner_bottom = pane_inner_bottom(window);
    let viewport_bottom = f32::from(window.viewport_size().height);
    let bottom = rect.1 + rect.3;
    assert!(
        bottom <= inner_bottom + 0.5,
        "{id} ends at {bottom}, below the card's body which ends at {inner_bottom}: the \
         floor does not cover every row the card shows"
    );
    assert!(
        bottom <= viewport_bottom,
        "{id} ends at {bottom}, below the {viewport_bottom}px viewport"
    );
}

/// The strip above the tail is the page's fallback scroll when the window is
/// too short for the cards, so it draws a bar of its own while it overflows and
/// none when everything fits (#54).
#[gpui_kit::test]
async fn the_cards_strip_draws_its_own_scrollbar_while_it_overflows(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell_at(cx, store, paths, (900., 600.));
    show_logs(cx, window);
    cx.update_window(window, |_, window, cx| {
        let cards = window_rect(window, "logs-cards");
        let thumb = scrollbar_thumb(window, cards)
            .expect("the cards strip draws a scrollbar while its controls overflow");
        assert!(
            thumb.1 - cards.1 <= 6.,
            "the strip opens at its top, so the thumb starts at the track's start: {thumb:?} in \
             {cards:?}"
        );
        window.scroll(
            "logs-cards",
            ScrollDelta::Pixels(point(px(0.), px(-400.))),
            cx,
        );
        let moved =
            scrollbar_thumb(window, cards).expect("the scrollbar is still drawn at the end");
        assert!(
            moved.1 > thumb.1 + 4.,
            "a wheel down the strip moves the thumb with the offset: {thumb:?} -> {moved:?}"
        );
    })
    .expect("update the shell window");

    // Everything fits in a tall window, so the strip draws no bar.
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell_at(cx, store, paths, (1200., 1400.));
    show_logs(cx, window);
    cx.update_window(window, |_, window, _| {
        let cards = window_rect(window, "logs-cards");
        assert!(
            scrollbar_thumb(window, cards).is_none(),
            "a cards strip whose controls fit draws no scrollbar"
        );
    })
    .expect("update the shell window");
}

/// The wheel belongs to whatever the pointer is over: over the tail it moves
/// the tail and nothing else, over the cards it moves the cards (#54).
#[gpui_kit::test]
async fn a_wheel_over_the_tail_moves_the_tail_and_a_wheel_over_the_cards_moves_the_cards(
    cx: &mut TestAppContext,
) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    write_lines(&paths, &long_tail(200));
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell_at(cx, store, paths, (900., 600.));
    show_logs(cx, window);

    cx.update_window(window, |_, window, cx| {
        let tail = window_rect(window, "logs-lines");
        let cards = window_rect(window, "logs-cards");
        let rows_before = shown_rows(window, 200);
        assert!(!rows_before.is_empty(), "the tail shows lines");

        window.scroll(
            "logs-lines",
            ScrollDelta::Pixels(point(px(0.), px(240.))),
            cx,
        );
        let rows_after = shown_rows(window, 200);
        assert_ne!(
            rows_before, rows_after,
            "a wheel over the tail moves the tail"
        );
        assert_eq!(
            window_rect(window, "logs-cards"),
            cards,
            "and leaves the cards above it where they are"
        );
        assert_eq!(
            window_rect(window, "logs-lines"),
            tail,
            "and leaves the tail's own box where it is"
        );

        let level_before = window_rect(window, "logs-level");
        window.scroll(
            "logs-cards",
            ScrollDelta::Pixels(point(px(0.), px(-240.))),
            cx,
        );
        assert!(
            window_rect(window, "logs-level").1 < level_before.1,
            "a wheel over the cards moves the cards"
        );
        assert_eq!(
            shown_rows(window, 200),
            rows_after,
            "and leaves the tail where it is"
        );
        assert_eq!(
            window_rect(window, "logs-lines"),
            tail,
            "the tail box stays"
        );

        // At the end of the tail a further wheel down stays in the tail: the
        // tail's box does not move and the cards do not either.
        let level_at_end = window_rect(window, "logs-level");
        window.scroll(
            "logs-lines",
            ScrollDelta::Pixels(point(px(0.), px(-5000.))),
            cx,
        );
        assert_eq!(
            window_rect(window, "logs-level").1,
            level_at_end.1,
            "a wheel at the tail's end does not move the cards"
        );
        assert!(
            window
                .find(ElementId::Name("logs-line-199".into()))
                .visible(),
            "it lands the tail back on its newest line"
        );
    })
    .expect("update the shell window");
}
/// The tail draws its own scrollbar from the view's own scroll handle: it
/// shows while the lines overflow, follows the offset a wheel or the jump
/// control sets, drags to any offset, and is not drawn at all when the lines
/// fit (#54).
#[gpui_kit::test]
async fn the_tail_scrollbar_follows_the_scroll_handle_it_was_given(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    write_lines(&paths, &long_tail(200));
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell_at(cx, store, paths, (1200., 1400.));
    // The bar slides in and out. The test reads where it was painted, so the
    // motion is off and every frame is a settled one.
    cx.update(|cx| cx.set_reduce_motion(true));
    show_logs(cx, window);
    // The bar is on screen while the lines overflow, not for two seconds after
    // the last wheel: past the idle a Scrolling-mode bar hides on, an
    // overflowing tail still paints it (#54). The bar's idle reads the real
    // clock, which `advance_clock` does not move, so the test waits it out.
    std::thread::sleep(Duration::from_millis(2500));
    settle(cx, window);

    let at_end = cx
        .update_window(window, |_, window, cx| {
            let tail = window_rect(window, "logs-lines");
            let thumb = scrollbar_thumb(window, tail)
                .expect("the tail draws a scrollbar while its lines overflow");
            assert!(
                tail.1 + tail.3 - (thumb.1 + thumb.3) <= 6.,
                "the view opens on the newest line, so the thumb starts at the track's end: {thumb:?} in {tail:?}"
            );

            // A wheel to the top moves the thumb to the track's start, and a
            // wheel back moves it down: the bar reads the offset the wheel set.
            window.scroll(
                "logs-lines",
                ScrollDelta::Pixels(point(px(0.), px(4000.))),
                cx,
            );
            let top = scrollbar_thumb(window, tail).expect("the scrollbar is still drawn at the top");
            assert!(
                top.1 - tail.1 <= 6.,
                "a wheel to the tail's start moves the thumb to the track's start: {top:?} in {tail:?}"
            );
            window.scroll(
                "logs-lines",
                ScrollDelta::Pixels(point(px(0.), px(-600.))),
                cx,
            );
            let moved =
                scrollbar_thumb(window, tail).expect("the scrollbar is still drawn mid-way down");
            assert!(
                moved.1 > top.1 + 4.,
                "a wheel down moves the thumb with the offset: {top:?} -> {moved:?}"
            );
            (tail, moved)
        })
        .expect("update the shell window");

    // Scrolling up paused the live tail, so the jump control is there: it lands
    // the tail on its newest line, and the thumb follows to the track's end.
    tick_live(cx, window);
    cx.update_window(window, |_, window, cx| window.click("logs-jump-latest", cx))
        .expect("update the shell window");
    settle(cx, window);
    cx.update_window(window, |_, window, cx| {
        let (tail, _) = at_end;
        let jumped =
            scrollbar_thumb(window, tail).expect("the scrollbar is still drawn after the jump");
        assert!(
            tail.1 + tail.3 - (jumped.1 + jumped.3) <= 6.,
            "the jump control reaches the tail's end and the thumb follows it: {jumped:?} in {tail:?}"
        );

        // Dragging the thumb is a real offset: the same handle the wheel and
        // the jump use, so the tail lands where the pointer leaves it.
        let column = tail.0 + tail.2 - 8.;
        window.drag(
            point(px(column), px(jumped.1 + jumped.3 / 2.)),
            point(px(column), px(tail.1 + 6.)),
            cx,
        );
        assert!(
            window.find("logs-line-0").visible(),
            "dragging the thumb to the track's start reaches the oldest line"
        );
        let dragged = scrollbar_thumb(window, tail).expect("the scrollbar is still drawn");
        window.drag(
            point(px(column), px(dragged.1 + dragged.3 / 2.)),
            point(px(column), px(tail.1 + tail.3 - 6.)),
            cx,
        );
        assert!(
            window
                .find(ElementId::Name("logs-line-199".into()))
                .visible(),
            "and dragging it back to the track's end reaches the newest line"
        );
    })
    .expect("update the shell window");

    // A tail whose lines all fit draws no bar, and a drag where the bar would
    // be moves nothing: the bar is the only thing that turns a drag there into
    // an offset.
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    write_lines(&paths, &long_tail(3));
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, _shell) = open_shell_at(cx, store, paths, (1200., 1400.));
    show_logs(cx, window);
    cx.update_window(window, |_, window, cx| {
        let tail = window_rect(window, "logs-lines");
        assert!(
            scrollbar_thumb(window, tail).is_none(),
            "a tail whose lines fit draws no scrollbar"
        );
        let rows = shown_rows(window, 3);
        assert_eq!(rows.len(), 3, "every line of the short tail is on screen");
        let column = tail.0 + tail.2 - 8.;
        window.drag(
            point(px(column), px(tail.1 + 6.)),
            point(px(column), px(tail.1 + tail.3 - 6.)),
            cx,
        );
        assert_eq!(
            shown_rows(window, 3),
            rows,
            "a drag where the bar would be leaves a tail that fits where it is"
        );
    })
    .expect("update the shell window");
}
