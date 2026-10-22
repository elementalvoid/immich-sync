//! Headless tests for the main window's scrolling (issue #26).
//!
//! The window opens compact, so every long surface must let the user reach its
//! last element by scrolling at a short height. The tests drive the real
//! `AppShell` in a short test window and dispatch real wheel events; the
//! accessibility snapshot's `visible()` reports the clip mask, so a clipped
//! element is measurable, not guessed.

use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, InputEvent as _, Point,
    ScrollDelta, ScrollWheelEvent, TestAppContext, Window, WindowBounds, WindowOptions, point, px,
    size,
};
use immich_sync::autostart::{Autostart, UnavailableAutostart};
use immich_sync::connection::{ConnectionModel, profile_from_settings};
use immich_sync::dashboard::Dashboard;
use immich_sync::diagnostics::DiagnosticsModel;
use immich_sync::diagnostics_view::DiagnosticsView;
use immich_sync::engine::EngineModel;
use immich_sync::logging::LOG_PREFIX;
use immich_sync::paths::AppPaths;
use immich_sync::settings::SettingsStore;
use immich_sync::settings_view::SettingsView;
use immich_sync::shell::{AppShell, ViewMode};
use immich_sync::store::StoreModel;
use immich_sync::theme;
use immich_sync::upload::UploadModel;
use immich_sync::upload_view::UploadView;
use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};
use sync_core::failure::FailureClass;
use sync_core::hash::Sha1;
use sync_core::store::{AlbumScope, ItemRecord, ItemState, SourceFolder, StateStore};

/// The number of parked and missing rows the state database is seeded with.
const SEEDED_ROWS: usize = 6;
/// The number of source folders the state database is seeded with.
const SEEDED_FOLDERS: usize = 1;
/// The number of log lines the log file is seeded with.
const SEEDED_LINES: usize = 80;

fn temporary_paths(dir: &Path) -> AppPaths {
    AppPaths {
        config_file: dir.join("config").join("settings.toml"),
        state_db: dir.join("data").join("state.db"),
        autostart_dir: dir.join("config").join("autostart"),
    }
}

/// A window short enough that no long surface fits in one screen.
fn window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            size: size(px(900.), px(420.)),
        })),
        ..Default::default()
    }
}

fn autostart() -> Rc<dyn Autostart> {
    Rc::new(UnavailableAutostart::new(
        "the test run has no OS autostart mechanism".to_string(),
    ))
}

/// One item row, as the views read it from the store.
fn item(path: &str, state: ItemState, class: Option<FailureClass>) -> ItemRecord {
    ItemRecord {
        path: path.to_string(),
        size: 1024,
        mtime_ns: 1_700_000_000_000_000_000,
        sha1: Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").expect("a hex SHA-1"),
        asset_id: None,
        state,
        error_class: class,
        error_message: class.map(|_| "the server does not accept this file format".to_string()),
        attempts: 1,
        sidecar: None,
    }
}

/// Seeds the state database with enough parked and missing rows that both
/// lists overflow the short window, and writes a long log file.
fn seed(paths: &AppPaths, root: &Path) {
    fs::create_dir_all(root).expect("create the source folder");
    let store = StateStore::open(&paths.state_db).expect("open the state database");
    store
        .add_folder(&SourceFolder {
            path: root.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");
    for index in 0..SEEDED_ROWS {
        store
            .record(&item(
                &root
                    .join(format!("GONE_{index:04}.jpg"))
                    .display()
                    .to_string(),
                ItemState::Missing,
                None,
            ))
            .expect("write the missing row");
        store
            .record(&item(
                &format!("/parked/BROKEN_{index:04}.jpg"),
                ItemState::FailedPermanent,
                Some(FailureClass::UnsupportedFormat),
            ))
            .expect("write the parked row");
    }

    let directory = paths.log_directory();
    fs::create_dir_all(&directory).expect("create the log directory");
    let lines: Vec<String> = (0..SEEDED_LINES)
        .map(|index| format!("line {index:04} of the log tail"))
        .collect();
    fs::write(
        directory.join(format!("{LOG_PREFIX}.2026-01-01")),
        format!("{}\n", lines.join("\n")),
    )
    .expect("write the log file");
}

/// Opens the real shell over the real views in the short window.
fn open_shell(
    cx: &mut TestAppContext,
    store: SettingsStore,
    paths: AppPaths,
) -> (AnyWindowHandle, Entity<AppShell>) {
    let model_paths = paths.clone();
    cx.update(|cx| {
        gpui_kit::init(cx);
        let profile = profile_from_settings(store.settings());
        let connection = cx.new(|_| {
            let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(profile, credentials)
        });
        let mut opened = None;
        let (window, _view) = gpui_kit::open_window(window_options(), cx, |window, cx| {
            let store_model = cx.new(|cx| StoreModel::open(&model_paths, cx));
            let engine = cx.new(|cx| {
                EngineModel::new(connection.clone(), store_model.clone(), &model_paths, cx)
            });
            let dashboard = cx.new(|cx| Dashboard::with_engine(engine.clone(), window, cx));
            let settings = cx.new(|cx| {
                SettingsView::new(
                    store,
                    model_paths.clone(),
                    autostart(),
                    connection.clone(),
                    window,
                    cx,
                )
            });
            let upload_model =
                cx.new(|cx| UploadModel::new(connection.clone(), store_model.clone(), cx));
            let upload = cx.new(|cx| UploadView::new(upload_model, window, cx));
            let failed =
                cx.new(|cx| immich_sync::failed_view::FailedView::new(engine.clone(), window, cx));
            let diagnostics_model = cx.new(|cx| {
                DiagnosticsModel::new(
                    store_model.clone(),
                    engine.clone(),
                    connection.clone(),
                    model_paths.log_directory(),
                    cx,
                )
            });
            let diagnostics = cx.new(|cx| DiagnosticsView::new(diagnostics_model, cx));
            let review = cx.new(|cx| immich_sync::review_view::ReviewView::new(engine, window, cx));
            let shell = cx.new(|cx| {
                AppShell::new(
                    dashboard,
                    settings,
                    upload,
                    failed,
                    diagnostics,
                    review,
                    connection,
                    cx,
                )
            });
            opened = Some(shell.clone());
            shell
        })
        .expect("open the shell window");
        theme::apply_system_theme(theme::SystemTheme::Light, cx);
        (window, opened.expect("the shell built"))
    })
}

/// The scroll container, the last content element of one surface, and whether
/// the surface must overflow the short test window.
///
/// The probe is the bottom-most element the view registers, so it only becomes
/// visible when the scroll actually reaches the content's end. Upload is short
/// enough to fit at the test height, so its probe must be visible without
/// scrolling; that still catches a clipped control. The Logs tail is on screen
/// without a page scroll (#54): its own box takes the height the cards above it
/// leave, so its newest line must be visible without scrolling either, and the
/// tail's own scroll is covered by `the_log_tail_scrolls_inside_its_own_container`.
fn surface_probe(mode: ViewMode) -> (ElementId, ElementId, bool) {
    match mode {
        ViewMode::Dashboard => (
            "dashboard-scroll".into(),
            ("folder-remove", SEEDED_FOLDERS - 1).into(),
            true,
        ),
        ViewMode::Settings => (
            "settings-scroll".into(),
            "settings-path-state-value".into(),
            true,
        ),
        ViewMode::Upload => ("upload-view".into(), "upload-button".into(), false),
        ViewMode::Failed => (
            "failed-view".into(),
            format!("failed-retry-{}", SEEDED_ROWS - 1).into(),
            true,
        ),
        ViewMode::Review => (
            "review-view".into(),
            format!("review-trash-{}", SEEDED_ROWS - 1).into(),
            true,
        ),
        ViewMode::Logs => (
            "logs-lines".into(),
            ElementId::from(format!("logs-line-{}", SEEDED_LINES - 1)),
            false,
        ),
        ViewMode::Diagnostics => (
            "diagnostics-view".into(),
            "diagnostics-errors-empty".into(),
            true,
        ),
    }
}

/// A point inside `container` that the window actually shows.
///
/// `TestWindowExt::scroll` aims at the element's center. A scroll container
/// taller than the window (the log tail) has its center below the window,
/// where no wheel event arrives, so this uses the visible part's center.
fn visible_center(window: &Window, container: &ElementId) -> Point<gpui_kit::Pixels> {
    let bounds = window.find(container.clone()).bounds();
    let viewport = Bounds::new(Point::default(), window.viewport_size());
    bounds.intersect(&viewport).center()
}

/// Scrolls `container` with wheel events until `target` is visible.
///
/// `lines` is the wheel's vertical delta per event: negative scrolls towards
/// the end of the content, positive back towards its start. Fails when the
/// target never becomes visible, which is exactly the clipping the ticket
/// reports: the element exists but the user cannot reach it.
fn scroll_until_visible(
    window: &mut Window,
    cx: &mut App,
    container: &ElementId,
    target: &ElementId,
    lines: f32,
) {
    window.render_frame(cx);
    for _ in 0..60 {
        if window.find(target.clone()).visible() {
            return;
        }
        let position = visible_center(window, container);
        window.dispatch_event(
            ScrollWheelEvent {
                position,
                delta: ScrollDelta::Lines(point(0., lines)),
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
    }
    panic!("target {target:?} never became visible inside {container:?}");
}

/// Waits until the surface registers `id`, so a queued off-thread read lands.
async fn wait_for_element(cx: &mut TestAppContext, window: AnyWindowHandle, id: ElementId) {
    for _ in 0..600 {
        let rendered = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.try_find(id.clone()).is_some()
            })
            .unwrap_or(false);
        if rendered {
            return;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    let paths = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            gpui_kit::base::test_support::registered_paths(window)
        })
        .unwrap_or_default();
    panic!("{id:?} never rendered; registered: {paths}");
}

#[gpui_kit::test]
async fn every_long_surface_reaches_its_last_element_at_a_short_height(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    seed(&paths, &temp.path().join("photos"));
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, shell) = open_shell(cx, store, paths);

    for mode in [
        ViewMode::Dashboard,
        ViewMode::Settings,
        ViewMode::Upload,
        ViewMode::Failed,
        ViewMode::Review,
        ViewMode::Logs,
        ViewMode::Diagnostics,
    ] {
        let (container, target, must_scroll) = surface_probe(mode);
        shell.update(cx, |shell, cx| shell.show(mode, cx));
        wait_for_element(cx, window, target.clone()).await;
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            if must_scroll {
                assert!(
                    !window.find(target.clone()).visible(),
                    "{mode:?}: {target:?} already fits in the short window, so this surface does \
                     not prove scrolling"
                );
            } else {
                assert!(
                    window.find(target.clone()).visible(),
                    "{mode:?}: {target:?} is not visible at the short window without scrolling"
                );
            }
            scroll_until_visible(window, cx, &container, &target, -4.);
        })
        .expect("update the shell window");
    }
}

#[gpui_kit::test]
async fn the_log_tail_scrolls_inside_its_own_container(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    seed(&paths, &temp.path().join("photos"));
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, shell) = open_shell(cx, store, paths);

    let newest = ElementId::from(format!("logs-line-{}", SEEDED_LINES - 1));
    let oldest = ElementId::from("logs-line-0");
    shell.update(cx, |shell, cx| shell.show(ViewMode::Logs, cx));
    wait_for_element(cx, window, newest.clone()).await;
    cx.update_window(window, |_, window, cx| {
        // The tail is on screen without the page scrolling first (#54), and the
        // live tail opens at its newest line, so the oldest rows sit above the
        // box's top edge.
        assert!(
            window.find("logs-lines").visible(),
            "the tail is on screen at a short window without a page scroll"
        );
        let box_top = window.find("logs-lines").bounds().origin.y;
        assert!(
            window.find(oldest.clone()).bounds().origin.y < box_top,
            "the tail opens at its end, with its oldest line above the box"
        );
        // The tail's box scrolls on its own: a wheel event inside it moves its
        // rows relative to the box, which a surface scroll could not do.
        let before = f32::from(window.find(oldest.clone()).bounds().origin.y)
            - f32::from(window.find("logs-lines").bounds().origin.y);
        let box_bounds = window.find("logs-lines").bounds();
        window.dispatch_event(
            ScrollWheelEvent {
                position: point(box_bounds.center().x, box_bounds.origin.y + px(20.)),
                delta: ScrollDelta::Lines(point(0., 4.)),
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        let after = f32::from(window.find(oldest.clone()).bounds().origin.y)
            - f32::from(window.find("logs-lines").bounds().origin.y);
        assert!(
            after > before + 40.,
            "a wheel event inside the tail scrolls the tail's own rows: {before} -> {after}"
        );
    })
    .expect("update the shell window");
}
