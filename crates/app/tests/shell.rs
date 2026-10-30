//! Headless tests for the shell's window effects.
//!
//! `cx.quit()` and the tray icon are platform effects that a headless test
//! cannot observe, so those are checked by the probe and the GUI smoke job.
//! The state-to-tooltip mapping is pure, so it is tested here.

mod common;

use std::path::Path;
use std::sync::Arc;

use common::scroll_dashboard_into_view;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{
    Action, AnyWindowHandle, App, AppContext as _, Bounds, MenuItem, Point, Role, ScrollDelta,
    TestAppContext, Window, WindowBounds, WindowOptions, point, px, size,
};
use immich_sync::connection::ConnectionModel;
use immich_sync::dashboard::Dashboard;
use immich_sync::diagnostics::DiagnosticsModel;
use immich_sync::diagnostics_view::DiagnosticsView;
use immich_sync::engine::EngineModel;
use immich_sync::failed_view::FailedView;
use immich_sync::paths::AppPaths;
use immich_sync::review_view::ReviewView;
use immich_sync::settings_view::SettingsView;
use immich_sync::shell::{
    self, About, AppShell, Hide, Minimize, Pause, Quit, Resume, Show, SyncNow, ViewMode,
};
use immich_sync::store::StoreModel;
use immich_sync::upload::UploadModel;
use immich_sync::upload_view::UploadView;
use sync_core::connection::{Attention, ConnectionState, ServerProfile};
use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};

fn boot(cx: &mut TestAppContext) {
    // The saved-profile restore runs on the app's Tokio worker, whose result
    // wakes the foreground executor from that thread. Parking mode is how the
    // harness permits a task to be woken off the test thread.
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        shell::init(cx);
    });
}

fn windows(cx: &TestAppContext) -> usize {
    cx.read(|cx| cx.windows().len())
}

#[gpui_kit::test]
fn show_opens_the_window_when_none_is_open(cx: &mut TestAppContext) {
    boot(cx);
    assert_eq!(windows(cx), 0);

    cx.update(|cx| cx.dispatch_action(&Show));
    cx.run_until_parked();

    assert_eq!(windows(cx), 1);
    // The shipped path, not only the builder: the test platform keeps the
    // requested bounds, so a revert to `WindowOptions::default()` fails here
    // (issue #26).
    let window = cx
        .read(|cx| cx.windows().first().copied())
        .expect("the window is open");
    let bounds = cx
        .update_window(window, |_, window, _| window.window_bounds())
        .expect("read the opened window's bounds");
    assert_eq!(
        bounds,
        cx.read(|cx| WindowBounds::centered(size(px(900.), px(650.)), cx)),
        "issue #26: the window opens centred at 900 x 650"
    );
}

#[gpui_kit::test]
fn hide_closes_the_open_window(cx: &mut TestAppContext) {
    boot(cx);
    cx.update(|cx| cx.dispatch_action(&Show));
    cx.run_until_parked();
    assert_eq!(windows(cx), 1);

    cx.update(|cx| cx.dispatch_action(&Hide));
    cx.run_until_parked();

    assert_eq!(windows(cx), 0);
}

#[gpui_kit::test]
fn show_reopens_the_window_after_a_hide(cx: &mut TestAppContext) {
    boot(cx);
    cx.update(|cx| cx.dispatch_action(&Show));
    cx.update(|cx| cx.dispatch_action(&Hide));
    cx.run_until_parked();
    assert_eq!(windows(cx), 0);

    cx.update(|cx| cx.dispatch_action(&Show));
    cx.run_until_parked();

    assert_eq!(windows(cx), 1);
}

#[gpui_kit::test]
fn the_connection_model_outlives_the_window(cx: &mut TestAppContext) {
    boot(cx);
    cx.update(|cx| cx.dispatch_action(&Show));
    cx.run_until_parked();
    assert_eq!(windows(cx), 1);

    // Hide closes the window and drops its root view, which used to drop the
    // model and its tray observation with it.
    cx.update(|cx| cx.dispatch_action(&Hide));
    cx.run_until_parked();
    assert_eq!(windows(cx), 0);

    // The app-level model is still alive and can transition while hidden.
    let connection = cx.read(shell::connection);
    let attention = ConnectionState::NeedsAttention(Attention::Rejected);
    cx.update(|cx| {
        connection.update(cx, |model, cx| model.set_state(attention.clone(), cx));
    });
    cx.run_until_parked();

    // The reopened window renders from that same model, not a fresh one that
    // would have forgotten the transition. With a configured profile (a real
    // settings file, or XDG_CONFIG_HOME in CI), the reopen used to restart the
    // startup restore here and reset the state; the model's once guard keeps
    // it (R1.3).
    cx.update(|cx| cx.dispatch_action(&Show));
    cx.run_until_parked();
    let window = cx
        .read(|cx| cx.windows().first().copied())
        .expect("the window is open again");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("status-connection-global").label(),
            Some(attention.message().as_str()),
            "the reopened window shows the state the app-level model holds"
        );
    })
    .expect("update the reopened window");
}

#[gpui_kit::test]
fn showing_twice_keeps_a_single_window(cx: &mut TestAppContext) {
    boot(cx);
    cx.update(|cx| cx.dispatch_action(&Show));
    cx.update(|cx| cx.dispatch_action(&Show));
    cx.run_until_parked();

    assert_eq!(windows(cx), 1);
}

#[gpui_kit::test]
fn the_main_window_opens_compact_resizable_and_with_a_floor(cx: &mut TestAppContext) {
    let options = cx.read(shell::main_window_options);
    let bounds = match options.window_bounds {
        Some(WindowBounds::Windowed(bounds)) => bounds,
        other => panic!("the main window opens windowed, not {other:?}"),
    };

    assert_eq!(
        bounds.size,
        size(px(900.), px(650.)),
        "issue #26: compact default"
    );
    assert!(
        options.is_resizable,
        "issue #26: the user resizes the window from its edges and corners"
    );
    let minimum = options.window_min_size.expect("a sensible minimum size");
    assert!(
        minimum.width < bounds.size.width && minimum.height < bounds.size.height,
        "the minimum leaves the default resizable: {minimum:?}"
    );
    assert!(
        minimum.width >= px(320.) && minimum.height >= px(320.),
        "the minimum is a real window, not a sliver: {minimum:?}"
    );
}

#[test]
fn tray_menu_labels_match_their_actions() {
    let items = shell::tray_menu();
    let entries: Vec<(String, &dyn Action)> = items
        .iter()
        .filter_map(|item| match item {
            MenuItem::Action { name, action, .. } => Some((name.to_string(), action.as_ref())),
            _ => None,
        })
        .collect();

    assert_eq!(
        entries.len(),
        5,
        "R8.3: Show, Pause, Resume, Sync now, and Quit"
    );
    assert_eq!(entries[0].0, "Show");
    assert!(entries[0].1.partial_eq(&Show));
    assert_eq!(entries[1].0, "Pause");
    assert!(entries[1].1.partial_eq(&Pause));
    assert_eq!(entries[2].0, "Resume");
    assert!(entries[2].1.partial_eq(&Resume));
    assert_eq!(entries[3].0, "Sync now");
    assert!(entries[3].1.partial_eq(&SyncNow));
    assert_eq!(entries[4].0, "Quit");
    assert!(entries[4].1.partial_eq(&shell::Quit));
    assert!(
        matches!(items[1], MenuItem::Separator) && matches!(items[5], MenuItem::Separator),
        "separators divide the window action, the sync controls, and Quit"
    );
}

#[test]
fn the_tray_tooltip_carries_the_activity_and_the_progress() {
    let counts = sync_core::store::FolderCounts {
        uploaded: 3,
        pending: 1,
        failed: 0,
        review: 2,
    };
    let tooltip = shell::tray_status_tooltip(
        sync_core::status::Activity::Syncing,
        &ConnectionState::Connected,
        counts,
        None,
    );
    assert!(tooltip.contains("Syncing"), "R8.2: {tooltip}");
    assert!(tooltip.contains("Connected"), "{tooltip}");
    assert!(tooltip.contains("3/4 uploaded"), "R8.2: {tooltip}");
    assert!(
        tooltip.contains("2 to review"),
        "R5.4: the tray shows the review count: {tooltip}"
    );

    let attention = shell::tray_status_tooltip(
        sync_core::status::Activity::NeedsAttention,
        &ConnectionState::NeedsAttention(Attention::Rejected),
        sync_core::store::FolderCounts::default(),
        Some("unsupported_format"),
    );
    assert!(attention.contains("Needs attention"), "{attention}");
    assert!(
        attention.contains("last error: unsupported_format"),
        "R8.2: the last error reaches the tooltip: {attention}"
    );
}

#[test]
fn the_tray_tooltip_shows_every_connection_state() {
    let not_configured = shell::tray_tooltip(&ConnectionState::NotConfigured);
    assert!(not_configured.contains("Immich Sync"), "{not_configured}");
    assert!(not_configured.contains("Not signed in"), "{not_configured}");

    assert!(
        shell::tray_tooltip(&ConnectionState::Connecting).contains("Signing in"),
        "the connecting state is shown"
    );
    assert!(
        shell::tray_tooltip(&ConnectionState::Connected).contains("Connected"),
        "the connected state is shown"
    );
}

#[test]
fn the_needs_attention_tooltip_carries_the_reason() {
    let state = ConnectionState::NeedsAttention(Attention::Rejected);

    let tooltip = shell::tray_tooltip(&state);
    assert!(tooltip.contains("Needs attention"), "{tooltip}");
    assert!(
        tooltip.contains("refused the session again"),
        "a second 401 must be visible without the window open: {tooltip}"
    );
}

// ---------------------------------------------------------------------------
// The filtered-open seam (#41). The folder detail (#43) will call
// `AppShell::show_filtered` to open the failed-items or deletion-review view
// with one source folder already selected.

fn temporary_paths(dir: &Path) -> AppPaths {
    AppPaths {
        config_file: dir.join("config").join("settings.toml"),
        state_db: dir.join("data").join("state.db"),
        autostart_dir: dir.join("config").join("autostart"),
    }
}

/// Opens a shell over a temporary state database, the way `open_main_window`
/// builds it, and returns the shell entity so a test can drive it.
fn open_shell(
    cx: &mut TestAppContext,
    paths: &AppPaths,
) -> (AnyWindowHandle, gpui_kit::Entity<AppShell>) {
    open_shell_at(cx, paths, (1200., 800.))
}

/// The same shell in a window of the given size, so a test can stand at the
/// window's own minimum.
fn open_shell_at(
    cx: &mut TestAppContext,
    paths: &AppPaths,
    (width, height): (f32, f32),
) -> (AnyWindowHandle, gpui_kit::Entity<AppShell>) {
    let paths = paths.clone();
    cx.update(|cx| {
        gpui_kit::init(cx);
        let connection = cx.new(|_| {
            let store: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(ServerProfile::default(), store)
        });
        let store_model = cx.new(|cx| StoreModel::open(&paths, cx));
        let engine =
            cx.new(|cx| EngineModel::new(connection.clone(), store_model.clone(), &paths, cx));
        let upload = cx.new(|cx| UploadModel::new(connection.clone(), store_model.clone(), cx));
        let bundle_dir = paths.state_db.parent().expect("a state dir").to_path_buf();
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(width), px(height)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| {
                let dashboard = cx.new(|cx| Dashboard::with_engine(engine.clone(), window, cx));
                let settings =
                    cx.new(|cx| SettingsView::from_environment(window, cx, connection.clone()));
                let upload_view = cx.new(|cx| UploadView::new(upload.clone(), window, cx));
                let failed = cx.new(|cx| FailedView::new(engine.clone(), window, cx));
                let diagnostics_model = cx.new(|cx| {
                    DiagnosticsModel::new(
                        store_model.clone(),
                        engine.clone(),
                        connection.clone(),
                        bundle_dir,
                        cx,
                    )
                });
                let diagnostics = cx.new(|cx| DiagnosticsView::new(diagnostics_model, cx));
                let review = cx.new(|cx| ReviewView::new(engine.clone(), window, cx));
                cx.new(|cx| {
                    AppShell::new(
                        dashboard,
                        settings,
                        upload_view,
                        failed,
                        diagnostics,
                        review,
                        connection,
                        cx,
                    )
                })
            },
        )
        .expect("open the shell window")
    })
}

/// #43: the folder detail's Failed and Review links open the matching view with
/// that folder already selected, through the shell's own surface switch.
#[gpui_kit::test]
async fn the_folder_detail_links_open_a_filtered_view(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let camera_roll = temp.path().join("Camera Roll");
    std::fs::create_dir_all(&camera_roll).expect("create the source folder");
    let selected = camera_roll.display().to_string();
    {
        let store = sync_core::store::StateStore::open(&paths.state_db).expect("open the store");
        store
            .add_folder(&sync_core::store::SourceFolder {
                path: selected.clone(),
                recursive: true,
                album_mapping: false,
                album_scope: sync_core::store::AlbumScope::Leaf,
                album_title: None,
            })
            .expect("add the source folder");
    }
    let (window, shell) = open_shell(cx, &paths);

    // The card appears once the store has opened, off the UI thread.
    cx.wait_for(window, std::time::Duration::from_secs(30), |window, _| {
        window.try_find(("folder-details", 0_usize)).is_some()
    })
    .await;
    cx.update_window(window, |_, window, cx| {
        // #56: the global card carries the scan line, so on a short window the
        // card's Details control sits below the fold. Scroll it into view the
        // way a person would before clicking it.
        scroll_dashboard_into_view(window, cx, ("folder-details", 0_usize));
        window.click(("folder-details", 0_usize), cx);
    })
    .expect("open the folder detail");
    cx.wait_for(window, std::time::Duration::from_secs(10), |window, _| {
        window.try_find("folder-panel").is_some()
    })
    .await;

    cx.update_window(window, |_, window, cx| {
        window.click("folder-detail-failed", cx);
    })
    .expect("follow the failed link");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            shell.read(cx).mode(),
            ViewMode::Failed,
            "#43: the Failed link opens the failed-items view"
        );
        assert_eq!(
            window.find("failed-folder-chip").label(),
            Some("Showing Camera Roll"),
            "the failed-items view opens filtered to the detail's folder"
        );
        assert!(
            window.try_find("folder-panel").is_none(),
            "the detail closes as the view opens"
        );
    })
    .expect("read the failed view");

    // The same detail, the other link.
    cx.update_window(window, |_, _, cx| {
        shell.update(cx, |shell, cx| shell.show(ViewMode::Dashboard, cx));
    })
    .expect("back to the dashboard");
    cx.update_window(window, |_, window, cx| {
        scroll_dashboard_into_view(window, cx, ("folder-details", 0_usize));
        window.click(("folder-details", 0_usize), cx);
    })
    .expect("open the folder detail again");
    cx.wait_for(window, std::time::Duration::from_secs(10), |window, _| {
        window.try_find("folder-panel").is_some()
    })
    .await;
    cx.update_window(window, |_, window, cx| {
        window.click("folder-detail-review", cx);
    })
    .expect("follow the review link");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            shell.read(cx).mode(),
            ViewMode::Review,
            "#43: the Review link opens the deletion review"
        );
        assert_eq!(
            window.find("review-folder-chip").label(),
            Some("Showing Camera Roll"),
            "the review opens filtered to the detail's folder"
        );
    })
    .expect("read the review");
}

/// #64: Cmd+Q and Cmd+W have no `OsAction` in this GPUI, so only a key
/// binding makes those keys fire, and it must be registered on macOS alone.
/// The keys actually firing is manual; this reads the chord strings, so a typo
/// (`cmd-q` to `cmd-p`) fails here rather than shipping.
#[gpui_kit::test]
fn the_menu_shortcuts_are_bound_only_on_macos(cx: &mut TestAppContext) {
    boot(cx);
    let (quit, hide) = cx.update(|cx| {
        let os = cx.platform_info().os;
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let chords = |action: &dyn Action| -> Vec<String> {
            keymap
                .bindings_for_action(action)
                .map(|binding| binding.keystrokes()[0].inner().unparse(os))
                .collect()
        };
        (chords(&Quit), chords(&Hide))
    });

    #[cfg(target_os = "macos")]
    assert_eq!(quit, ["cmd-q"], "Cmd+Q quits");
    #[cfg(target_os = "macos")]
    assert_eq!(hide, ["cmd-w"], "Cmd+W closes the window");
    #[cfg(not(target_os = "macos"))]
    assert!(
        quit.is_empty() && hide.is_empty(),
        "the macOS shortcuts stay off every other platform"
    );
}

/// #64: `shell::init` installs the menu bar, not just the function that builds
/// it: deleting the `set_menus` call must fail here. The harness records what
/// the app set (`gpui_kit::test`'s platform keeps it), so this reads the real
/// installed bar.
#[gpui_kit::test]
fn the_menu_bar_is_installed_by_init(cx: &mut TestAppContext) {
    boot(cx);
    let names: Vec<String> = cx.update(|cx| {
        cx.get_menus()
            .expect("the test platform records the menu bar")
            .iter()
            .map(|menu| menu.name.to_string())
            .collect()
    });

    #[cfg(target_os = "macos")]
    assert_eq!(names, ["Immich Sync", "Edit", "Window"]);
    #[cfg(not(target_os = "macos"))]
    assert!(
        names.is_empty(),
        "the menu bar stays off every other platform"
    );
}

/// #64: About is not a dead item. The Diagnostics surface already holds the
/// version, the git revision, and the paths, so About opens it rather than a
/// dialog this app does not have.
#[gpui_kit::test]
fn about_opens_the_window_on_the_diagnostics_surface(cx: &mut TestAppContext) {
    boot(cx);
    assert_eq!(windows(cx), 0);

    cx.update(|cx| cx.dispatch_action(&About));
    cx.run_until_parked();
    assert_eq!(windows(cx), 1, "About opens the window when it is closed");

    let window = cx
        .read(|cx| cx.windows().first().copied())
        .expect("the window is open");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("diagnostics-view").is_some(),
            "About opens the Diagnostics surface, which holds the version, the \
             git revision, and the paths"
        );
    })
    .expect("read the About surface");
}

/// #64: the Window menu's Close is `Hide`, and neither it nor Minimize may
/// conjure a window when none is open.
#[gpui_kit::test]
fn window_menu_actions_need_an_open_window(cx: &mut TestAppContext) {
    boot(cx);
    assert_eq!(windows(cx), 0);

    cx.update(|cx| cx.dispatch_action(&Minimize));
    cx.run_until_parked();
    assert_eq!(windows(cx), 0, "Minimize has no window to act on");

    cx.update(|cx| cx.dispatch_action(&Hide));
    cx.run_until_parked();
    assert_eq!(windows(cx), 0, "Close has no window to act on");
}

#[gpui_kit::test]
fn show_filtered_opens_either_view_on_one_source_folder(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let camera_roll = temp.path().join("Camera Roll");
    std::fs::create_dir_all(&camera_roll).expect("create the source folder");
    let (window, shell) = open_shell(cx, &paths);
    let selected = camera_roll.display().to_string();

    // The filter's state is what the seam sets, so the chip is readable on the
    // first frame; the store read that fills the lists is covered by the two
    // views' own tests.
    cx.update(|cx| {
        shell.update(cx, |shell, cx| {
            shell.show_filtered(ViewMode::Failed, selected.clone(), cx);
        });
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-folder-chip").label(),
            Some("Showing Camera Roll"),
            "#43's seam opens the failed-items view on the folder it names"
        );
        assert_eq!(
            window.find("failed-folder-filter").label(),
            Some("Folder filter: Camera Roll")
        );
    })
    .expect("read the filtered failed-items view");

    cx.update(|cx| {
        shell.update(cx, |shell, cx| {
            shell.show_filtered(ViewMode::Review, selected.clone(), cx);
        });
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("review-folder-chip").label(),
            Some("Showing Camera Roll"),
            "the same seam opens the deletion review on the same folder"
        );
    })
    .expect("read the filtered review");

    // The seam carries a folder, so a surface without a folder filter ignores
    // it and simply opens.
    cx.update(|cx| {
        shell.update(cx, |shell, cx| {
            shell.show_filtered(ViewMode::Dashboard, selected, cx);
        });
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("status-global").is_some(),
            "a surface without a folder filter still opens"
        );
    })
    .expect("read the dashboard");
}

// ---------------------------------------------------------------------------
// The surface switch (#75). The switch is the kit's tab bar, so it is asserted
// through the ids the other surfaces' tests click, the labels it announces, and
// the selection it reports.

/// The seven surfaces in the switch's order: the surface, the nav element id its
/// tab carries, and the label it announces. Written out here rather than read
/// back from the shell, so a renamed id or label fails instead of following the
/// code.
const SWITCH: [(ViewMode, &str, &str); 7] = [
    (ViewMode::Dashboard, "nav-dashboard", "Dashboard"),
    (ViewMode::Upload, "nav-upload", "Upload"),
    (ViewMode::Failed, "nav-failed", "Failed items"),
    (ViewMode::Review, "nav-review", "Review"),
    (ViewMode::Settings, "nav-settings", "Settings"),
    (ViewMode::Logs, "nav-logs", "Logs"),
    (ViewMode::Diagnostics, "nav-diagnostics", "Diagnostics"),
];

/// The strip the tabs scroll in. It clips them at a narrow window, so it is what
/// the overflow assertions measure against.
const STRIP: &str = "nav-tabs";

/// Focuses `id` with Tab presses, the way a keyboard user reaches it.
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

/// The window's minimum size, as the frame would apply it.
fn window_minimum(cx: &TestAppContext) -> (f32, f32) {
    let options = cx.read(shell::main_window_options);
    let minimum = options.window_min_size.expect("a minimum window size");
    (f32::from(minimum.width), f32::from(minimum.height))
}

/// #75: pointer and keyboard both reach all seven surfaces. Each tab is its own
/// tab stop in the bar's order, and Enter shows it.
#[gpui_kit::test]
fn every_surface_is_reachable_by_pointer_and_by_keyboard(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let (window, shell) = open_shell(cx, &paths);

    for (mode, id, _) in SWITCH {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .expect("click the surface's tab");
        assert_eq!(
            cx.read(|cx| shell.read(cx).mode()),
            mode,
            "the pointer shows {mode:?} through {id}"
        );
    }

    for (mode, id, _) in SWITCH {
        cx.update_window(window, |_, window, cx| {
            // Start from no focus, so every round walks the same stops.
            window.blur(cx);
            window.render_frame(cx);
            tab_to(window, id, cx);
            window.press("enter", cx);
        })
        .expect("show the surface from the keyboard");
        assert_eq!(
            cx.read(|cx| shell.read(cx).mode()),
            mode,
            "the keyboard shows {mode:?} through {id}"
        );
    }
}

/// #75: the bar is a tab list, and every tab is named and reports whether it is
/// the surface on screen, so the selection reaches assistive technology and not
/// only the eye.
#[gpui_kit::test]
fn the_switch_names_every_surface_and_reports_the_selected_one(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let (window, shell) = open_shell(cx, &paths);

    for (mode, _, _) in SWITCH {
        cx.update(|cx| shell.update(cx, |shell, cx| shell.show(mode, cx)));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            for (other, id, label) in SWITCH {
                let tab = window.find(id);
                assert_eq!(tab.role(), Some(Role::Tab), "{id} is announced as a tab");
                assert_eq!(
                    tab.label(),
                    Some(label),
                    "{id} is announced by its surface's name"
                );
                assert_eq!(
                    tab.selected(),
                    Some(other == mode),
                    "{id} reports whether {mode:?} is the surface on screen"
                );
            }
        })
        .expect("read the switch");
    }
}

/// #75: a window narrower than the seven labels clips nothing. The strip
/// scrolls, and a tab that was out of sight both comes into sight and selects
/// its surface once it is picked.
#[gpui_kit::test]
fn a_narrow_window_scrolls_the_switch_instead_of_clipping_it(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let (window, shell) = open_shell_at(cx, &paths, (420., 600.));

    let (strip, last) = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let strip = window.find(STRIP).bounds();
            let last = window.find("nav-diagnostics").bounds();
            let viewport = f32::from(window.viewport_size().width);
            assert!(
                f32::from(strip.origin.x + strip.size.width) <= viewport + 0.5,
                "the strip stays inside the window: {strip:?} in {viewport} px"
            );
            assert!(
                f32::from(last.origin.x + last.size.width)
                    > f32::from(strip.origin.x + strip.size.width),
                "the seven labels do not fit at 420 px, so the last tab starts out of \
                 sight: {last:?} in {strip:?}"
            );
            (strip, last)
        })
        .expect("measure the switch");

    // A plain wheel's vertical lines scroll the strip sideways, the way the
    // kit's tab bars do for a wheel that has no horizontal axis.
    cx.update_window(window, |_, window, cx| {
        window.scroll(STRIP, ScrollDelta::Lines(point(0., -100.)), cx);
    })
    .expect("scroll the strip");

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let scrolled = window.find("nav-diagnostics").bounds();
        assert!(
            f32::from(scrolled.origin.x) < f32::from(last.origin.x),
            "the wheel moved the tabs: {scrolled:?} was {last:?}"
        );
        assert!(
            f32::from(scrolled.origin.x + scrolled.size.width)
                <= f32::from(strip.origin.x + strip.size.width) + 0.5,
            "the last tab is on screen after scrolling: {scrolled:?} in {strip:?}"
        );
        window.click("nav-diagnostics", cx);
    })
    .expect("pick the tab that was out of sight");

    assert_eq!(
        cx.read(|cx| shell.read(cx).mode()),
        ViewMode::Diagnostics,
        "picking a tab that was out of sight shows its surface"
    );
}

/// #75: the window's minimum is the narrowest the seven surfaces still lay out
/// in, not the width the seven labels need. At the minimum the strip has to
/// scroll, so the tab row is provably not what holds the window open; and every
/// surface still fits inside it, so the floor is theirs.
#[gpui_kit::test]
fn the_minimum_window_comes_from_the_surfaces_not_the_tab_row(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let (width, height) = window_minimum(cx);
    assert!(
        width > 320. && height > 320.,
        "the minimum stays a real window: {width} by {height}"
    );
    let (window, shell) = open_shell_at(cx, &paths, (width, height));

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let strip = window.find(STRIP).bounds();
        let last = window.find("nav-diagnostics").bounds();
        assert!(
            f32::from(last.origin.x + last.size.width)
                > f32::from(strip.origin.x + strip.size.width),
            "the seven labels fit at the {width} px minimum, so the tab row can still \
             be what sets it: {last:?} in {strip:?}"
        );
    })
    .expect("measure the switch at the minimum");

    for (mode, _, _) in SWITCH {
        cx.update(|cx| shell.update(cx, |shell, cx| shell.show(mode, cx)));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let viewport_right = f32::from(window.viewport_size().width);
            for element in gpui_kit::base::test_support::snapshots(window) {
                let path = format!("{:?}", element.path());
                // The tabs are the one thing allowed past the edge: that is the
                // strip scrolling instead of the row widening the window.
                if path.contains("nav-") {
                    continue;
                }
                let bounds = element.bounds();
                let right = f32::from(bounds.origin.x + bounds.size.width);
                assert!(
                    right <= viewport_right + 0.5,
                    "{mode:?}: {path} reaches {right} px, past the {viewport_right} px \
                     minimum window at {bounds:?}"
                );
            }
        })
        .expect("lay out the surface at the minimum");
    }
}
