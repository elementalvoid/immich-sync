//! Headless tests for the folder detail (issue #43, SPEC R8.1, R5.4, N5).
//!
//! The detail is one source folder's live view, opened from its dashboard card
//! or that card's Details control. Every test drives the real controls: a real
//! state database, the real engine model, and the view's own click and keyboard
//! handling.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, ElementId, Entity, Point, ScrollDelta, TestAppContext,
    Window, WindowBounds, WindowOptions, point, px, size,
};
use immich_sync::connection::ConnectionModel;
use immich_sync::dashboard::Dashboard;
use immich_sync::engine::EngineModel;
use immich_sync::paths::AppPaths;
use immich_sync::store::StoreModel;
use immich_sync::times;
use sync_core::connection::ServerProfile;
use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};
use sync_core::failure::FailureClass;
use sync_core::hash::Sha1;
use sync_core::status::{Activity, FolderStatus, Status};
use sync_core::store::{
    AlbumScope, FolderCounts, ItemRecord, ItemState, NewVersion, SourceFolder, StateStore,
};

/// The path layout the other app tests use, so the view gets a temp database.
fn temporary_paths(dir: &Path) -> AppPaths {
    AppPaths {
        config_file: dir.join("config").join("settings.toml"),
        state_db: dir.join("data").join("state.db"),
        autostart_dir: dir.join("config").join("autostart"),
    }
}

fn window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds {
            origin: Point::default(),
            size: size(px(1200.), px(800.)),
        })),
        ..Default::default()
    }
}

/// One tracked folder, written straight to the state database before the view
/// opens.
fn seed_folder(paths: &AppPaths, path: &Path, recursive: bool) -> SourceFolder {
    let store = StateStore::open(&paths.state_db).expect("open the state database");
    let folder = SourceFolder {
        path: path.display().to_string(),
        recursive,
        album_mapping: false,
        album_scope: AlbumScope::Leaf,
        album_title: None,
    };
    store.add_folder(&folder).expect("add the source folder");
    folder
}

/// The album title the state database holds for `folder` (#47).
fn seed_album_title(paths: &AppPaths, folder: &Path, title: &str) {
    let store = StateStore::open(&paths.state_db).expect("open the state database");
    store
        .set_album_title(&sync_core::paths::path_key(folder), Some(title))
        .expect("store the album title");
}

/// One tracked item under `folder`, so the folder's counts are not all zero.
fn seed_row(paths: &AppPaths, folder: &Path, name: &str, state: ItemState) {
    let store = StateStore::open(&paths.state_db).expect("open the state database");
    store
        .record(&ItemRecord {
            path: folder.join(name).display().to_string(),
            size: 1024,
            mtime_ns: 1_700_000_000_000_000_000,
            sha1: Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").expect("a hex SHA-1"),
            asset_id: matches!(state, ItemState::Uploaded | ItemState::Duplicate)
                .then(|| format!("asset-{name}")),
            state,
            error_class: matches!(state, ItemState::DeadLetter | ItemState::FailedPermanent)
                .then_some(FailureClass::UnsupportedFormat),
            error_message: matches!(state, ItemState::DeadLetter).then(|| {
                "the server answered 503 in a form this client does not understand: \
                 Service Unavailable"
                    .to_string()
            }),
            attempts: 0,
            sidecar: None,
        })
        .expect("record the item");
}

/// Seeds both folder timestamps, so the detail has a moment to show.
fn seed_times(paths: &AppPaths, folder: &Path, scan_ns: i64, upload_ns: i64) {
    let store = StateStore::open(&paths.state_db).expect("open the state database");
    store
        .record_folder_scan(&folder.display().to_string(), scan_ns)
        .expect("record the scan time");
    store
        .record_folder_upload(&folder.display().to_string(), upload_ns)
        .expect("record the upload time");
}

/// Opens a live dashboard over a temporary state database, as the shell does.
fn open_dashboard(
    cx: &mut TestAppContext,
    paths: &AppPaths,
) -> (AnyWindowHandle, Entity<StoreModel>, Entity<Dashboard>) {
    open_dashboard_with_server(cx, paths, "")
}

/// Opens the same dashboard over a connection that holds `server_url`, as the
/// settings file leaves it after a sign-in (#44).
fn open_dashboard_with_server(
    cx: &mut TestAppContext,
    paths: &AppPaths,
    server_url: &str,
) -> (AnyWindowHandle, Entity<StoreModel>, Entity<Dashboard>) {
    let paths = paths.clone();
    let profile = ServerProfile {
        server_url: server_url.to_string(),
        ..ServerProfile::default()
    };
    cx.update(|cx| {
        gpui_kit::init(cx);
        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        let connection = cx.new(|_| ConnectionModel::new(profile, credentials));
        let store = cx.new(|cx| StoreModel::open(&paths, cx));
        let engine = cx.new(|cx| EngineModel::new(connection, store.clone(), &paths, cx));
        let (window, view) = gpui_kit::open_window(window_options(), cx, |window, cx| {
            let engine = engine.clone();
            cx.new(|cx| Dashboard::with_engine(engine, window, cx))
        })
        .expect("open the live dashboard window");
        (window, store, view)
    })
}

/// Waits until the shared state store has opened; it opens off the UI thread
/// (N2), so a control driven before that is refused.
async fn wait_for_store(cx: &mut TestAppContext, store: &Entity<StoreModel>) {
    for _ in 0..600 {
        if cx.read(|cx| store.read(cx).store().is_some()) {
            return;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    panic!("the state store never opened");
}

/// Waits until the first tracked folder satisfies `check`, and returns it.
async fn wait_for_folder(
    cx: &mut TestAppContext,
    store: &Entity<StoreModel>,
    check: impl Fn(&SourceFolder) -> bool,
) -> SourceFolder {
    let mut last = None;
    for _ in 0..600 {
        let folder = cx.read(|cx| store.read(cx).folders().first().cloned());
        if let Some(folder) = folder {
            if check(&folder) {
                return folder;
            }
            last = Some(folder);
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    panic!("the folder never reached the expected state: {last:?}");
}

/// Moves focus to the next tab stop, as the Root's Tab binding does.
fn tab_to(window: &mut Window, id: impl Into<ElementId> + Clone, cx: &mut App) {
    let id = id.into();
    for _ in 0..24 {
        if window.find(id.clone()).focused() == Some(true) {
            return;
        }
        window.focus_next(cx);
        window.render_frame(cx);
    }
    assert_eq!(
        window.find(id.clone()).focused(),
        Some(true),
        "{id:?} is not keyboard reachable"
    );
}

/// Whether the dialog is on screen.
fn detail_open(window: &Window) -> bool {
    window.try_find("folder-panel").is_some()
}

/// The accessible label the element with `id` announces.
fn label(window: &Window, id: impl Into<ElementId>) -> String {
    window.find(id).label().unwrap_or_default().to_string()
}

/// Runs one action inside a window update.
///
/// A click or a key press queues its effect — the dialog closing, a focus
/// move — and effects are flushed when the update ends. Every action therefore
/// sits in its own update, and the assertion that reads the result follows in
/// another one.
fn act(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    action: impl FnOnce(&mut Window, &mut App),
) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        action(window, cx);
    })
    .expect("act on the window");
}

/// Reads the window after the actions that preceded it.
fn read<R>(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    read: impl FnOnce(&mut Window, &mut App) -> R,
) -> R {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        read(window, cx)
    })
    .expect("read the window")
}

/// Scrolls the dialog until the element with `id` is on screen.
///
/// The detail card scrolls its body when the window is short (#43), and the
/// album title field above the list pushes Load more below the fold, so a
/// control is clicked only after it is really visible.
fn scroll_into_view(window: &mut Window, cx: &mut App, id: impl Into<ElementId>) {
    let id = id.into();
    window.render_frame(cx);
    for _ in 0..60 {
        if window
            .try_find(id.clone())
            .is_some_and(|target| target.visible())
        {
            return;
        }
        window.scroll(
            "folder-panel",
            ScrollDelta::Pixels(point(px(0.), px(-120.))),
            cx,
        );
    }
    panic!("{id:?} never became visible in the dialog");
}

/// Scrolls the dashboard until the element with `id` is on screen.
///
/// #56: the global card carries the reconciliation scan line, so on a short
/// window a card's footer sits below the fold; a control is clicked only after
/// it is really visible.
fn scroll_dashboard_into_view(window: &mut Window, cx: &mut App, id: impl Into<ElementId>) {
    let id = id.into();
    window.render_frame(cx);
    for _ in 0..60 {
        if window
            .try_find(id.clone())
            .is_some_and(|target| target.visible())
        {
            return;
        }
        window.scroll(
            "dashboard-scroll",
            ScrollDelta::Pixels(point(px(0.), px(-120.))),
            cx,
        );
    }
    panic!("{id:?} never became visible on the dashboard");
}

/// Opens the detail and waits for it to be on screen.
fn open_detail_now(cx: &mut TestAppContext, window: AnyWindowHandle) {
    act(cx, window, |window, cx| {
        scroll_dashboard_into_view(window, cx, ("folder-details", 0_usize));
        window.click(("folder-details", 0_usize), cx);
    });
    read(cx, window, |window, _| {
        assert!(
            detail_open(window),
            "#43: the Details control opens the detail"
        );
    });
}

#[gpui_kit::test]
async fn a_card_click_and_its_details_control_open_the_detail_but_its_settings_do_not(
    cx: &mut TestAppContext,
) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    let folder = seed_folder(&paths, &photos, true);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    read(cx, window, |window, cx| {
        // N5: the Details control is a keyboard-reachable tab stop of the card,
        // not only a pointer target.
        tab_to(window, ("folder-details", 0_usize), cx);
    });
    act(cx, window, |window, cx| window.blur(cx));
    // R8.1, #43: a press on the card's own surface opens its detail.
    act(cx, window, |window, cx| {
        window.click_at(("folder-card", 0_usize), point(px(6.), px(6.)), cx);
    });
    read(cx, window, |window, _| {
        assert!(detail_open(window), "a card click opens the detail");
        assert_eq!(
            window.find("folder-panel").role(),
            Some(gpui_kit::Role::Dialog),
            "N5: the detail is one named dialog"
        );
        assert_eq!(
            label(window, "folder-panel"),
            "Folder detail",
            "N5: the dialog has an accessible name"
        );
        // R8.1: the full path, not the truncated card copy.
        assert_eq!(
            label(window, "folder-panel-path"),
            format!("Folder path: {}", folder.path),
            "the detail shows the whole stored path"
        );
    });

    // Esc closes it and the card gets the keyboard back.
    act(cx, window, |window, cx| window.press("escape", cx));
    read(cx, window, |window, _| {
        assert!(!detail_open(window), "#43: Escape closes the detail");
        assert_eq!(
            window.find(("folder-details", 0_usize)).focused(),
            Some(true),
            "#43: focus returns to the card's Details control"
        );
    });

    // The named Details control opens the same dialog.
    act(cx, window, |window, cx| {
        window.click(("folder-details", 0_usize), cx);
    });
    read(cx, window, |window, _| {
        assert!(detail_open(window), "the Details control opens the detail");
    });
    act(cx, window, |window, cx| window.press("escape", cx));
    read(cx, window, |window, _| {
        assert!(!detail_open(window), "Escape closes it again");
    });

    // #43: a card control never opens it, with the pointer or the keyboard.
    for control in ["folder-recursive", "folder-album"] {
        act(cx, window, |window, cx| {
            window.click((control, 0_usize), cx)
        });
        read(cx, window, |window, _| {
            assert!(!detail_open(window), "{control} must not open the detail");
        });
    }
    act(cx, window, |window, cx| {
        tab_to(window, ("folder-recursive", 0_usize), cx);
        window.press("space", cx);
    });
    read(cx, window, |window, _| {
        assert!(
            !detail_open(window),
            "Space on a card control must not open the detail"
        );
    });
    act(cx, window, |window, cx| {
        window.click(("folder-remove", 0_usize), cx)
    });
    read(cx, window, |window, _| {
        assert!(
            !detail_open(window),
            "the Remove control must not open the detail"
        );
    });
}

#[gpui_kit::test]
async fn the_detail_shows_the_live_fields_and_writes_every_setting(cx: &mut TestAppContext) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    seed_row(&paths, &photos, "one.jpg", ItemState::Uploaded);
    seed_row(&paths, &photos, "two.jpg", ItemState::Uploaded);
    seed_row(&paths, &photos, "three.jpg", ItemState::Pending);
    seed_row(&paths, &photos, "four.jpg", ItemState::DeadLetter);
    seed_row(&paths, &photos, "five.jpg", ItemState::Missing);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    read(cx, window, |window, _| {
        assert_eq!(label(window, "folder-detail-title"), "photos");
        assert_eq!(label(window, "folder-detail-state"), "Syncing");
        assert_eq!(
            label(window, "folder-detail-counts"),
            "2 uploaded · 1 pending · 1 failed · 1 to review · 5 tracked",
            "R8.1: the detail names every count and the tracked total"
        );
        // The progress is the uploaded share of the work in flight (R8.1).
        assert_eq!(label(window, "folder-detail-percent"), "67%");
        // R8.1: the three settings start from the stored row.
        assert_eq!(window.find("folder-panel-recursive").checked(), Some(true));
        assert_eq!(window.find("folder-panel-album").checked(), Some(false));
        assert_eq!(
            window.find("folder-panel-album-leaf").selected(),
            Some(true)
        );

        // N5: every control has an accessible name.
        for (id, name) in [
            ("folder-panel-recursive", "Sync photos recursively"),
            ("folder-panel-album", "Album mapping for photos"),
            (
                "folder-panel-album-leaf",
                "Album scope for photos: one album per folder",
            ),
            (
                "folder-panel-album-whole",
                "Album scope for photos: one album for this folder",
            ),
            ("folder-detail-close", "Close the folder detail"),
            ("folder-detail-failed", "View 1 failed items in photos"),
            ("folder-detail-review", "View 1 missing files in photos"),
            ("folder-detail-remove", "Remove photos"),
            (
                "folder-detail-open",
                &format!("Open {} in the file manager", photos.display()),
            ),
        ] {
            assert_eq!(label(window, id), name, "N5: {id} is named");
        }
    });

    // R2.1, R3.4: each setting writes immediately and the dialog stays open.
    // One at a time: every write is its own worker request, so waiting for
    // each to land keeps the test measuring the write, not the runtime's
    // scheduling.
    for (control, arrived) in [
        (
            "folder-panel-recursive",
            &(|folder: &SourceFolder| !folder.recursive) as &dyn Fn(&SourceFolder) -> bool,
        ),
        (
            "folder-panel-album",
            &(|folder: &SourceFolder| folder.album_mapping),
        ),
        (
            "folder-panel-album-whole",
            &(|folder: &SourceFolder| folder.album_scope == AlbumScope::WholeFolder),
        ),
    ] {
        act(cx, window, |window, cx| window.click(control, cx));
        wait_for_folder(cx, &store, arrived).await;
        read(cx, window, |window, _| {
            assert!(
                detail_open(window),
                "#43: a setting write keeps the detail open"
            );
        });
    }

    // The store holds all three, and the dialog showed them back.
    let stored = wait_for_folder(cx, &store, |folder| {
        !folder.recursive && folder.album_mapping && folder.album_scope == AlbumScope::WholeFolder
    })
    .await;
    read(cx, window, |window, _| {
        assert_eq!(window.find("folder-panel-recursive").checked(), Some(false));
        assert_eq!(window.find("folder-panel-album").checked(), Some(true));
        assert_eq!(
            window.find("folder-panel-album-whole").selected(),
            Some(true)
        );
    });
    assert!(stored.album_mapping);

    // R8.1: the card behind the dialog shows the same values once it closes.
    act(cx, window, |window, cx| window.press("escape", cx));
    read(cx, window, |window, _| {
        assert!(!detail_open(window));
        assert_eq!(
            window.find(("folder-recursive", 0_usize)).checked(),
            Some(false)
        );
        assert_eq!(window.find(("folder-album", 0_usize)).checked(), Some(true));
        assert_eq!(
            window.find(("folder-album-whole", 0_usize)).selected(),
            Some(true)
        );
    });
}

#[gpui_kit::test]
async fn the_detail_traps_focus_and_every_control_is_keyboard_reachable(cx: &mut TestAppContext) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    read(cx, window, |window, cx| {
        // N5: Tab stays inside the dialog and never reaches the dashboard
        // controls behind the backdrop.
        for _ in 0..16 {
            window.press("tab", cx);
            for behind in [
                "dashboard-folder-path",
                "dashboard-add-folder",
                "folder-recursive",
                "folder-remove",
            ] {
                let focused = window
                    .try_find(behind)
                    .and_then(|snapshot| snapshot.focused());
                assert_ne!(focused, Some(true), "focus escaped the detail to {behind}");
            }
        }
        // N5: every control in the dialog answers the keyboard.
        for id in [
            "folder-panel-recursive",
            "folder-panel-album",
            "folder-panel-album-leaf",
            "folder-panel-album-whole",
            "folder-detail-open",
            "folder-detail-failed",
            "folder-detail-review",
            "folder-detail-remove",
            "folder-detail-close",
        ] {
            tab_to(window, id, cx);
        }
    });
}

#[gpui_kit::test]
async fn escape_the_backdrop_and_the_close_control_all_close_the_detail(cx: &mut TestAppContext) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    act(cx, window, |window, cx| {
        window.click("folder-detail-close", cx)
    });
    read(cx, window, |window, _| {
        assert!(!detail_open(window), "#43: the close control closes it");
        assert_eq!(
            window.find(("folder-details", 0_usize)).focused(),
            Some(true),
            "the card keeps the keyboard after a close"
        );
    });

    open_detail_now(cx, window);
    // The backdrop is everything outside the centered dialog.
    act(cx, window, |window, cx| {
        window.click_at("folder-panel-backdrop", point(px(8.), px(8.)), cx);
    });
    read(cx, window, |window, _| {
        assert!(!detail_open(window), "#43: the backdrop closes it");
        assert_eq!(
            window.find(("folder-details", 0_usize)).focused(),
            Some(true),
            "the card keeps the keyboard after the backdrop closes it"
        );
    });

    // #43: a click on a non-control area inside the dialog is not a backdrop
    // click: the card is a mouse barrier above it.
    open_detail_now(cx, window);
    act(cx, window, |window, cx| {
        window.click("folder-detail-counts", cx)
    });
    read(cx, window, |window, _| {
        assert!(
            detail_open(window),
            "a click inside the dialog on its text does not dismiss it"
        );
    });
}

#[gpui_kit::test]
async fn the_detail_follows_the_engine_while_it_stays_open(cx: &mut TestAppContext) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    let folder = seed_folder(&paths, &photos, true);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    read(cx, window, |window, _| {
        assert_eq!(
            label(window, "folder-detail-counts"),
            "0 uploaded · 0 pending · 0 failed · 0 to review · 0 tracked"
        );
    });

    // R8.1, #43: the client's next status carries the new work, and the open
    // dialog renders it without reopening.
    seed_row(&paths, &photos, "later.jpg", ItemState::Pending);
    let path = folder.path.clone();
    for _ in 0..600 {
        cx.update(|cx| {
            store.update(cx, |store, cx| store.refresh_counts(&path, cx));
        });
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
        let updated = read(cx, window, |window, _| {
            label(window, "folder-detail-counts").contains("1 pending") && detail_open(window)
        });
        if updated {
            return;
        }
    }
    panic!("the open detail never showed the new pending count");
}

#[gpui_kit::test]
async fn the_last_error_and_both_timestamps_render_in_full(cx: &mut TestAppContext) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    seed_row(&paths, &photos, "gone.jpg", ItemState::DeadLetter);
    // Two hours back, so the relative time cannot drift within one test.
    let scan_ns = times::now_ns() - 2 * 60 * 60 * 1_000_000_000;
    let upload_ns = scan_ns - 60 * 1_000_000_000;
    seed_times(&paths, &photos, scan_ns, upload_ns);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    read(cx, window, |window, _| {
        assert_eq!(
            label(window, "folder-detail-scan"),
            format!(
                "Last reconciliation scan: 2 hours ago · {}",
                times::rfc3339_local(scan_ns)
            ),
            "#43: the scan line carries a relative time and the exact local RFC-3339 one"
        );
        assert_eq!(
            label(window, "folder-detail-upload"),
            format!(
                "Last successful upload: 2 hours ago · {}",
                times::rfc3339_local(upload_ns)
            ),
            "#43: the upload line carries both forms too"
        );
    });
}

#[gpui_kit::test]
async fn a_folder_that_never_ran_reads_not_yet_for_both_times(cx: &mut TestAppContext) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    read(cx, window, |window, _| {
        assert_eq!(
            label(window, "folder-detail-upload"),
            "Last successful upload: not yet"
        );
        assert_eq!(
            label(window, "folder-detail-scan"),
            "Last reconciliation scan: not yet"
        );
    });
}

#[gpui_kit::test]
async fn the_whole_path_is_selectable_so_nothing_is_truncated(cx: &mut TestAppContext) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    // Deep enough that the path wraps onto several lines on every platform, so
    // the wrapped case this test exists for is exercised here and not only on
    // Windows CI, whose temp path is longer.
    let photos = temp
        .path()
        .join("photos")
        .join("a-rather-long-directory-name".repeat(3))
        .join("Camera Roll");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    let folder = seed_folder(&paths, &photos, true);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    read(cx, window, |window, cx| {
        // Corner to corner, not middle to middle: the path wraps onto more than
        // one line, and only a drag from the first line's start to the last
        // line's end covers every line. The 2px inset keeps both ends inside
        // the block whatever the DPI.
        let bounds = window.find("folder-panel-path").bounds();
        window.drag(
            bounds.origin + point(px(2.), px(2.)),
            bounds.origin + point(bounds.size.width - px(2.), bounds.size.height - px(2.)),
            cx,
        );
        let selected = gpui_kit::base::TextSelection::selected_text(window, cx);
        assert_eq!(
            selected.trim(),
            folder.path,
            "#43: the whole path can be selected and copied, not only the visible part"
        );
    });
}

#[gpui_kit::test]
async fn the_remove_confirmation_asks_twice_and_says_what_stays_on_the_server(
    cx: &mut TestAppContext,
) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove", cx)
    });
    read(cx, window, |window, _| {
        assert!(
            detail_open(window),
            "#43: the first Remove asks instead of removing"
        );
        assert_eq!(
            label(window, "folder-detail-confirm"),
            "Remove photos?",
            "the confirmation names the folder"
        );
        assert_eq!(
            window.find("folder-detail-confirm").focused(),
            Some(true),
            "#43: the confirmation takes the keyboard"
        );
        // #43: the dimming is structural. Only the normal body is dimmed, so
        // the confirmation the user must read and act on keeps its contrast.
        // The harness observes structure, not opacity, so the tree is what the
        // test can pin: the confirmation is not a child of the dimmed element.
        assert!(
            window
                .within("folder-detail-body-dim")
                .try_find("folder-detail-confirm")
                .is_none(),
            "#43: the confirmation is not inside the dimmed body"
        );
        assert!(
            window
                .within("folder-detail-body")
                .try_find("folder-detail-confirm")
                .is_some(),
            "#43: the confirmation still sits in the dialog body"
        );
    });

    // Escape cancels the confirmation only, not the dialog.
    act(cx, window, |window, cx| window.press("escape", cx));
    read(cx, window, |window, _| {
        assert!(
            detail_open(window),
            "#43: Escape during the confirmation keeps the dialog"
        );
        assert!(
            window.try_find("folder-detail-confirm").is_none(),
            "Escape cancels the confirmation"
        );
    });

    // Off, and on again: Cancel does the same.
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove", cx)
    });
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove-cancel", cx);
    });
    read(cx, window, |window, _| {
        assert!(detail_open(window));
        assert!(window.try_find("folder-detail-confirm").is_none());
    });
    assert_eq!(
        cx.read(|cx| store.read(cx).folders().len()),
        1,
        "#43: neither Escape nor Cancel removed the folder"
    );

    // The second Remove is the one that drops it.
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove", cx)
    });
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove-confirm", cx);
    });
    for _ in 0..600 {
        if cx.read(|cx| store.read(cx).folders().is_empty()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| store.read(cx).folders().is_empty()),
        "#43: the second Remove drops the folder"
    );
    cx.wait_for(window, Duration::from_secs(10), |window, _| {
        window.try_find("folder-panel").is_none()
    })
    .await;
    read(cx, window, |window, _| {
        assert_eq!(
            window.find("dashboard-folder-path").focused(),
            Some(true),
            "#43: with no card left to return to, the keyboard goes to the folder field"
        );
    });
}

/// #43: a Remove the store refuses keeps the dialog open, says why, and leaves
/// the button usable instead of stuck on `Removing…` forever.
#[gpui_kit::test]
async fn a_refused_remove_keeps_the_detail_open_with_its_reason(cx: &mut TestAppContext) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    // Refuse only the delete: every read the dialog makes still answers, so the
    // detail sees a folder that stays and a store that said no.
    {
        let raw = rusqlite::Connection::open(&paths.state_db).expect("open the database directly");
        raw.execute_batch(
            "CREATE TRIGGER refuse_folder_removal BEFORE DELETE ON source_folder
             BEGIN SELECT RAISE(ABORT, 'the state database refused the removal'); END;",
        )
        .expect("install a trigger that refuses the delete");
    }

    act(cx, window, |window, cx| {
        window.click("folder-detail-remove", cx)
    });
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove-confirm", cx);
    });

    cx.wait_for(window, Duration::from_secs(10), |window, _| {
        window.try_find("folder-panel-problem").is_some()
    })
    .await;
    read(cx, window, |window, _| {
        let problem = label(window, "folder-panel-problem");
        assert!(
            problem.contains("refused the removal"),
            "#43: the detail shows why the removal was refused, not only that it stopped: {problem}"
        );
        assert!(
            detail_open(window),
            "#43: a refused Remove keeps the dialog open"
        );
    });
    // The button is usable again, not stuck on Removing…: a disabled control
    // ignores the click, so the confirmation appearing is what proves it.
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove", cx)
    });
    read(cx, window, |window, _| {
        assert!(
            window.try_find("folder-detail-confirm").is_some(),
            "#43: Remove works again after the refusal, so the user can retry"
        );
    });

    // The retry is a real Remove: with nothing left to refuse it, the write
    // drops the folder and the detail closes.
    {
        let raw = rusqlite::Connection::open(&paths.state_db).expect("open the database directly");
        raw.execute_batch("DROP TRIGGER refuse_folder_removal")
            .expect("drop the trigger that refused the delete");
    }
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove-confirm", cx);
    });
    for _ in 0..600 {
        if cx.read(|cx| store.read(cx).folders().is_empty()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| store.read(cx).folders().is_empty()),
        "#43: the retried Remove reaches the store and drops the folder"
    );
    cx.wait_for(window, Duration::from_secs(10), |window, _| {
        window.try_find("folder-panel").is_none()
    })
    .await;
}

/// #43 (blocker): a settings edit while a Remove is pending is its own request.
/// It must not drop the removal's answer, which would leave the detail stuck on
/// `Removing…` with the store's reason never shown.
#[gpui_kit::test]
async fn a_setting_edit_during_a_pending_remove_still_answers_the_remove(cx: &mut TestAppContext) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    {
        let raw = rusqlite::Connection::open(&paths.state_db).expect("open the database directly");
        raw.execute_batch(
            "CREATE TRIGGER refuse_folder_removal BEFORE DELETE ON source_folder
             BEGIN SELECT RAISE(ABORT, 'the state database refused the removal'); END;",
        )
        .expect("install a trigger that refuses the delete");
    }

    // Hold the write lock so the Remove is still in flight when the setting is
    // written, whatever the runtime does with the two worker jobs.
    let lock = rusqlite::Connection::open(&paths.state_db).expect("open the database directly");
    lock.execute_batch("BEGIN IMMEDIATE")
        .expect("hold the write lock");

    act(cx, window, |window, cx| {
        window.click("folder-detail-remove", cx)
    });
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove-confirm", cx);
    });
    // The write lock keeps the removal pending while the detail writes a
    // setting of its own, which is the edit that must not answer the removal.
    act(cx, window, |window, cx| {
        window.click("folder-panel-recursive", cx);
    });
    lock.execute_batch("COMMIT")
        .expect("release the write lock");

    cx.wait_for(window, Duration::from_secs(10), |window, _| {
        window.try_find("folder-panel-problem").is_some()
    })
    .await;
    read(cx, window, |window, _| {
        let problem = label(window, "folder-panel-problem");
        assert!(
            problem.contains("refused the removal"),
            "#43: the setting edit does not drop the removal's reason: {problem}"
        );
        assert!(
            detail_open(window),
            "#43: a refused Remove keeps the dialog open"
        );
    });
    assert_eq!(
        cx.read(|cx| store.read(cx).folders().len()),
        1,
        "#43: the refused removal left the folder in the store"
    );
    // A disabled control ignores the click, so the confirmation appearing is
    // what proves the setting edit did not leave `removing` set forever.
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove", cx)
    });
    read(cx, window, |window, _| {
        assert!(
            window.try_find("folder-detail-confirm").is_some(),
            "#43: Remove works again after the setting edit, so `removing` was cleared"
        );
    });
}

#[gpui_kit::test]
async fn open_folder_passes_the_stored_path_and_is_disabled_when_unavailable(
    cx: &mut TestAppContext,
) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    let folder = seed_folder(&paths, &photos, true);
    let (window, store, view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    let opened: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = opened.clone();
    cx.update(|cx| {
        view.update(cx, |view, _| {
            view.set_folder_opener(move |path| {
                recorder
                    .lock()
                    .expect("the recorder lock")
                    .push(path.to_string());
                Ok(())
            });
        });
    });

    open_detail_now(cx, window);
    act(cx, window, |window, cx| {
        window.click("folder-detail-open", cx)
    });
    assert_eq!(
        opened.lock().expect("the recorder lock").as_slice(),
        std::slice::from_ref(&folder.path),
        "#43: Open folder hands the stored path to the opener"
    );

    // R2.5, #43: a folder that disappears says so, keeps its settings and
    // Remove, and cannot be opened.
    std::fs::remove_dir_all(&photos).expect("remove the source folder");
    let path = folder.path.clone();
    for _ in 0..600 {
        cx.update(|cx| {
            store.update(cx, |store, cx| store.refresh_counts(&path, cx));
        });
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
        let unavailable = read(cx, window, |window, _| {
            window.try_find("folder-detail-unavailable").is_some()
        });
        if unavailable {
            break;
        }
    }
    read(cx, window, |window, _| {
        assert_eq!(
            label(window, "folder-detail-unavailable"),
            format!("Folder unavailable: {path}"),
            "#43: the dialog says the folder is unavailable and keeps its path"
        );
    });

    // A disabled control does nothing: the opener is not asked again.
    let before = opened.lock().expect("the recorder lock").len();
    act(cx, window, |window, cx| {
        window.click("folder-detail-open", cx)
    });
    assert_eq!(
        opened.lock().expect("the recorder lock").len(),
        before,
        "#43: Open folder is disabled for an unavailable folder"
    );

    // The settings and Remove stay usable.
    act(cx, window, |window, cx| {
        window.click("folder-panel-recursive", cx)
    });
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove", cx)
    });
    read(cx, window, |window, _| {
        assert!(
            window.try_find("folder-detail-confirm").is_some(),
            "#43: Remove stays usable while the folder is unavailable"
        );
    });
}

#[gpui_kit::test]
async fn the_detail_renders_in_both_themes(cx: &mut TestAppContext) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    // #44: the list and its link are part of the detail, so they render in
    // both themes too.
    seed_asset(
        &paths,
        &photos.join("one.jpg"),
        1024,
        "asset-one",
        times::now_ns() - THREE_AND_A_HALF_HOURS_NS,
    );

    for system in [
        immich_sync::theme::SystemTheme::Dark,
        immich_sync::theme::SystemTheme::Light,
    ] {
        let (window, store, _view) =
            open_dashboard_with_server(cx, &paths, "https://immich.example.com");
        wait_for_store(cx, &store).await;
        cx.update(|cx| immich_sync::theme::apply_system_theme(system, cx));
        open_detail_now(cx, window);
        wait_for_asset_rows(cx, window, 1).await;
        read(cx, window, |window, _| {
            assert!(
                window.find("folder-panel").visible(),
                "{system:?} shows the detail"
            );
            assert_eq!(label(window, "folder-detail-title"), "photos");
            assert_eq!(
                label(window, "folder-panel-path"),
                format!("Folder path: {}", photos.display()),
                "{system:?} shows the whole path"
            );
            assert!(
                label(window, ("folder-asset", 0_usize)).starts_with("one.jpg"),
                "{system:?} shows the uploaded asset"
            );
            assert_eq!(
                label(window, ("folder-asset-link", 0_usize)),
                "Open one.jpg in Immich",
                "{system:?} names the link"
            );
        });
    }
}

#[gpui_kit::test]
fn a_folder_status_carries_its_times_into_the_detail_card() {
    let status = Status {
        activity: Activity::Idle,
        folders: vec![FolderStatus {
            path: "/photos".to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
            album_rename: None,
            available: true,
            counts: FolderCounts::default(),
            last_error: None,
            last_scan_ns: Some(1_700_000_000_000_000_000),
            last_upload_ns: Some(1_700_000_001_000_000_000),
            uploads: sync_core::meter::UploadTraffic::default(),
        }],
        counts: FolderCounts::default(),
        last_error: None,
        uploads: sync_core::meter::UploadTraffic::default(),
        scanning: false,
    };

    let model = immich_sync::status::DashboardModel::from_engine(&status, None);
    assert_eq!(
        model.folders[0].last_scan_ns,
        Some(1_700_000_000_000_000_000)
    );
    assert_eq!(
        model.folders[0].last_upload_ns,
        Some(1_700_000_001_000_000_000)
    );
    assert_eq!(
        model.global.last_scan_ns,
        Some(1_700_000_000_000_000_000),
        "#56: the global card carries the newest folder's scan"
    );
}

/// Seeds one uploaded asset and its current version, the shape the detail's
/// list reads (#44).
fn seed_asset(paths: &AppPaths, path: &Path, size: u64, asset_id: &str, created_at_ns: i64) {
    let store = StateStore::open(&paths.state_db).expect("open the state database");
    let sha1 = Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").expect("a hex SHA-1");
    store
        .record(&ItemRecord {
            path: path.display().to_string(),
            size,
            mtime_ns: 1_700_000_000_000_000_000,
            sha1,
            asset_id: Some(asset_id.to_string()),
            state: ItemState::Uploaded,
            error_class: None,
            error_message: None,
            attempts: 0,
            sidecar: None,
        })
        .expect("record the asset");
    store
        .record_version(&NewVersion {
            path: path.display().to_string(),
            asset_id: asset_id.to_string(),
            sha1,
            created_at_ns,
            stack_id: None,
        })
        .expect("record the current version");
}

/// Seeds `count` assets under `folder`, oldest first, so the newest is the
/// highest index (#44).
fn seed_assets(paths: &AppPaths, folder: &Path, count: usize, base_ns: i64) {
    for index in 0..count {
        let index = i64::try_from(index).expect("a small index");
        seed_asset(
            paths,
            &folder.join(format!("IMG_{index:04}.jpg")),
            1_024 + u64::try_from(index).expect("a small index"),
            &format!("asset-{index:04}"),
            base_ns + index,
        );
    }
}

/// Waits until the detail lists exactly `count` asset rows (#44).
async fn wait_for_asset_rows(cx: &mut TestAppContext, window: AnyWindowHandle, count: usize) {
    cx.wait_for(window, Duration::from_secs(10), |window, _| {
        (0..count).all(|index| window.try_find(("folder-asset", index)).is_some())
            && window.try_find(("folder-asset", count)).is_none()
    })
    .await;
}

/// The fixed point every seeded asset's time sits relative to: three and a half
/// hours back, so the relative form reads "3 hours ago" and cannot cross a scale
/// inside one test.
const THREE_AND_A_HALF_HOURS_NS: i64 = 3 * 60 * 60 * 1_000_000_000 + 30 * 60 * 1_000_000_000;

/// #44: one asset's row reads its name, size, and upload time, newest first.
#[gpui_kit::test]
async fn the_list_shows_the_newest_assets_with_their_name_size_and_time(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    let now = times::now_ns();
    let first = now - THREE_AND_A_HALF_HOURS_NS;
    let second = first + 60 * 1_000_000_000;
    seed_asset(
        &paths,
        &photos.join("older.jpg"),
        1024,
        "asset-older",
        first,
    );
    seed_asset(
        &paths,
        &photos.join("newer.jpg"),
        2048,
        "asset-newer",
        second,
    );
    // Edited once: the old asset stays in the chain, the new one is current.
    seed_asset(
        &paths,
        &photos.join("edited.jpg"),
        4096,
        "asset-edited-1",
        first + 30,
    );
    seed_asset(
        &paths,
        &photos.join("edited.jpg"),
        4096,
        "asset-edited-2",
        second + 30,
    );
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    wait_for_asset_rows(cx, window, 3).await;
    read(cx, window, |window, _| {
        assert_eq!(
            label(window, ("folder-asset", 0_usize)),
            format!(
                "edited.jpg · 4.0 KiB · uploaded 3 hours ago · {}",
                times::rfc3339_local(second + 30)
            ),
            "#44: the newest asset comes first, named, sized, and dated twice"
        );
        assert_eq!(
            label(window, ("folder-asset", 1_usize)),
            format!(
                "newer.jpg · 2.0 KiB · uploaded 3 hours ago · {}",
                times::rfc3339_local(second)
            )
        );
        assert_eq!(
            label(window, ("folder-asset", 2_usize)),
            format!(
                "older.jpg · 1.0 KiB · uploaded 3 hours ago · {}",
                times::rfc3339_local(first)
            )
        );
    });
}

/// #44: the detail lists the newest 50 and Load more appends the next 50
/// without losing the rows already shown.
#[gpui_kit::test]
async fn load_more_appends_the_next_page_and_keeps_the_loaded_rows(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    let base = times::now_ns() - THREE_AND_A_HALF_HOURS_NS;
    seed_assets(&paths, &photos, 120, base);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    wait_for_asset_rows(cx, window, 50).await;
    let newest = read(cx, window, |window, _| {
        label(window, ("folder-asset", 0_usize))
    });
    assert!(
        newest.starts_with("IMG_0119.jpg"),
        "#44: the first page starts at the newest asset: {newest}"
    );
    let last_before = read(cx, window, |window, _| {
        label(window, ("folder-asset", 49_usize))
    });

    act(cx, window, |window, cx| {
        scroll_into_view(window, cx, "folder-assets-more");
        window.click("folder-assets-more", cx)
    });
    wait_for_asset_rows(cx, window, 100).await;
    read(cx, window, |window, _| {
        assert_eq!(
            label(window, ("folder-asset", 0_usize)),
            newest,
            "#44: Load more does not lose or reorder the loaded rows"
        );
        assert_eq!(
            label(window, ("folder-asset", 49_usize)),
            last_before,
            "#44: the 50th row is the same one before and after Load more"
        );
        assert_eq!(
            label(window, ("folder-asset", 99_usize)),
            format!(
                "IMG_0020.jpg · 1.0 KiB · uploaded 3 hours ago · {}",
                times::rfc3339_local(base + 20)
            ),
            "#44: the appended page continues the one order"
        );
    });
}

/// #44: with no configured server URL there is no link, only the row.
#[gpui_kit::test]
async fn a_folder_without_a_server_url_shows_no_asset_link(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    seed_asset(
        &paths,
        &photos.join("one.jpg"),
        1024,
        "asset-one",
        times::now_ns() - THREE_AND_A_HALF_HOURS_NS,
    );
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    wait_for_asset_rows(cx, window, 1).await;
    read(cx, window, |window, _| {
        assert!(
            window.try_find(("folder-asset-link", 0_usize)).is_none(),
            "#44: a missing server URL hides the link instead of opening a wrong one"
        );
        assert!(
            window
                .try_find(("folder-asset", 0_usize))
                .is_some_and(|row| row.visible()),
            "the asset itself is still listed"
        );
    });
}

/// #44: the link is the configured server URL minus a trailing `/api`, plus
/// `/photos/{assetId}`, a reverse-proxy prefix included, and it opens in the
/// injected browser. It is named and reachable from the keyboard.
#[gpui_kit::test]
async fn an_asset_link_is_named_reachable_and_opens_the_configured_server(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    seed_asset(
        &paths,
        &photos.join("one.jpg"),
        1024,
        "asset-one",
        times::now_ns() - THREE_AND_A_HALF_HOURS_NS,
    );
    let (window, store, view) =
        open_dashboard_with_server(cx, &paths, "https://immich.example.com/immich/api");
    wait_for_store(cx, &store).await;

    let opened: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = opened.clone();
    cx.update(|cx| {
        view.update(cx, |view, _| {
            view.set_link_opener(move |url, _| {
                recorder
                    .lock()
                    .expect("the recorder lock")
                    .push(url.to_string());
            });
        });
    });

    open_detail_now(cx, window);
    wait_for_asset_rows(cx, window, 1).await;
    read(cx, window, |window, cx| {
        assert_eq!(
            label(window, ("folder-asset-link", 0_usize)),
            "Open one.jpg in Immich",
            "N5: the link names the asset it opens"
        );
        tab_to(window, ("folder-asset-link", 0_usize), cx);
        assert!(
            opened.lock().expect("the recorder lock").is_empty(),
            "reaching the link does not open it"
        );
    });
    act(cx, window, |window, cx| window.press("enter", cx));
    assert_eq!(
        opened.lock().expect("the recorder lock").as_slice(),
        ["https://immich.example.com/immich/photos/asset-one"],
        "#44: the link keeps the proxy prefix and drops the API base"
    );
}

/// #44: a folder with nothing uploaded says so instead of showing an empty
/// list.
#[gpui_kit::test]
async fn a_folder_with_no_uploaded_assets_says_so(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    cx.wait_for(window, Duration::from_secs(10), |window, _| {
        window.try_find("folder-assets-empty").is_some()
    })
    .await;
    read(cx, window, |window, _| {
        assert_eq!(
            label(window, "folder-assets-empty"),
            "No uploaded assets in this folder yet.",
            "#44: the empty state is explicit"
        );
        assert!(
            window.try_find("folder-assets-more").is_none(),
            "an empty list has no next page"
        );
    });
}

/// #44: a store that cannot answer replaces the list with an explicit reason.
#[gpui_kit::test]
async fn an_unreadable_store_says_why_the_list_is_missing(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    seed_asset(
        &paths,
        &photos.join("one.jpg"),
        1024,
        "asset-one",
        times::now_ns() - THREE_AND_A_HALF_HOURS_NS,
    );
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    // The detail's read is a fresh connection to the file, so a schema without
    // the table it joins cannot answer it, as a damaged database cannot. The
    // fault goes through SQLite rather than the filesystem: every open
    // connection keeps its handle to the file, which Windows insists on.
    {
        let raw = rusqlite::Connection::open(&paths.state_db).expect("open the database directly");
        raw.execute_batch("DROP TABLE version")
            .expect("drop the version table the asset list joins");
    }

    open_detail_now(cx, window);
    cx.wait_for(window, Duration::from_secs(10), |window, _| {
        window.try_find("folder-assets-problem").is_some()
    })
    .await;
    read(cx, window, |window, _| {
        let problem = label(window, "folder-assets-problem");
        assert!(
            problem.starts_with("The folder's uploaded assets could not be read:"),
            "#44: the read error is explicit: {problem}"
        );
        assert!(
            window.try_find(("folder-asset", 0_usize)).is_none(),
            "#44: a failed read replaces the rows"
        );
    });
}

/// #44: the list follows the folder's uploaded count while it stays open,
/// keeping the rows the user loaded rather than dropping back to the first
/// page.
#[gpui_kit::test]
async fn the_list_reloads_on_a_new_upload_and_keeps_the_loaded_row_count(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    let folder = seed_folder(&paths, &photos, true);
    seed_assets(
        &paths,
        &photos,
        120,
        times::now_ns() - THREE_AND_A_HALF_HOURS_NS,
    );
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    wait_for_asset_rows(cx, window, 50).await;
    act(cx, window, |window, cx| {
        scroll_into_view(window, cx, "folder-assets-more");
        window.click("folder-assets-more", cx)
    });
    wait_for_asset_rows(cx, window, 100).await;

    // One more upload lands: the folder's count moves, and the next status poll
    // reloads the 100 rows the user has loaded.
    seed_asset(
        &paths,
        &photos.join("IMG_9999.jpg"),
        999,
        "asset-newest",
        times::now_ns(),
    );
    let path = folder.path.clone();
    for _ in 0..600 {
        cx.update(|cx| {
            store.update(cx, |store, cx| store.refresh_counts(&path, cx));
        });
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
        let reloaded = read(cx, window, |window, _| {
            label(window, ("folder-asset", 0_usize)).starts_with("IMG_9999.jpg")
        });
        if reloaded {
            break;
        }
    }
    read(cx, window, |window, _| {
        assert!(
            label(window, ("folder-asset", 0_usize)).starts_with("IMG_9999.jpg"),
            "#44: the new upload leads the refreshed list"
        );
        assert!(
            window.try_find(("folder-asset", 99_usize)).is_some(),
            "#44: the refresh keeps the 100 rows the user loaded"
        );
        assert!(
            window.try_find(("folder-asset", 100_usize)).is_none(),
            "the refresh does not load more than the user asked for"
        );
        assert!(
            label(window, ("folder-asset", 1_usize)).starts_with("IMG_0119.jpg"),
            "#44: the rows that were loaded stay, in order, under the new one"
        );
    });
}

/// #44 (R2.1): turning Recursive off reloads the list at the folder's new
/// setting, so a subfolder's asset leaves the rows it was listed in.
#[gpui_kit::test]
async fn turning_recursion_off_reloads_the_list_with_direct_children_only(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    let sub = photos.join("sub");
    std::fs::create_dir_all(&sub).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    let base = times::now_ns() - THREE_AND_A_HALF_HOURS_NS;
    seed_asset(
        &paths,
        &photos.join("direct.jpg"),
        1024,
        "asset-direct",
        base,
    );
    seed_asset(
        &paths,
        &photos.join("second.jpg"),
        2048,
        "asset-second",
        base + 1,
    );
    seed_asset(
        &paths,
        &sub.join("nested.jpg"),
        4096,
        "asset-nested",
        base + 2,
    );
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    wait_for_asset_rows(cx, window, 3).await;
    read(cx, window, |window, _| {
        assert!(
            label(window, ("folder-asset", 0_usize)).starts_with("nested.jpg"),
            "R2.1: the recursive folder lists its subfolder's asset"
        );
    });

    // The setting writes immediately; the next status poll carries the new
    // value and reloads the rows at it.
    act(cx, window, |window, cx| {
        window.click("folder-panel-recursive", cx)
    });
    wait_for_folder(cx, &store, |folder| !folder.recursive).await;
    wait_for_asset_rows(cx, window, 2).await;
    read(cx, window, |window, _| {
        assert!(
            window.try_find(("folder-asset", 2_usize)).is_none(),
            "#44: the subfolder's row left with the recursion setting"
        );
        assert!(
            label(window, ("folder-asset", 0_usize)).starts_with("second.jpg")
                && label(window, ("folder-asset", 1_usize)).starts_with("direct.jpg"),
            "#44: the list now holds the folder's direct children only"
        );
    });
}

/// The album title the detail's field shows, or `None` when it holds none.
fn album_title_input(window: &Window) -> Option<String> {
    gpui_kit::base::test_support::snapshots(window)
        .into_iter()
        .find(|snapshot| {
            snapshot.role() == Some(gpui_kit::Role::TextInput)
                && snapshot
                    .path()
                    .contains(&ElementId::Name("folder-panel-title".into()))
        })
        .and_then(|snapshot| snapshot.value().map(str::to_string))
}

/// Replaces the focused field's text with `value` (#47).
fn retype(window: &mut Window, value: &str, cx: &mut App) {
    window.press("ctrl-a", cx);
    window.input(value, cx);
}

/// #47: the detail shows the stored album title and saves a new one.
#[gpui_kit::test]
async fn the_detail_shows_the_album_title_and_saves_a_new_one(cx: &mut TestAppContext) {
    // A worker thread wakes the foreground executor, which the harness
    // permits only in parking mode.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    seed_album_title(&paths, &photos, "Holiday 2024");
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;
    open_detail_now(cx, window);

    read(cx, window, |window, _| {
        assert_eq!(
            album_title_input(window).as_deref(),
            Some("Holiday 2024"),
            "#47: the detail shows the title the user chose"
        );
        assert_eq!(
            label(window, "folder-panel-title"),
            "Album title for photos",
            "N5: the title field is named for the folder it changes"
        );
    });

    // #47: the title saves through the field, and the detail stays open.
    act(cx, window, |window, cx| {
        tab_to(window, "folder-panel-title", cx);
        retype(window, "Holiday 2025", cx);
        window.render_frame(cx);
        window.click("folder-panel-title-save", cx);
    });
    let stored = wait_for_folder(cx, &store, |folder| {
        folder.album_title.as_deref() == Some("Holiday 2025")
    })
    .await;
    assert_eq!(stored.album_title.as_deref(), Some("Holiday 2025"));
    read(cx, window, |window, _| {
        assert!(
            detail_open(window),
            "#47: saving a title keeps the detail open"
        );
    });

    // #47: clearing the title stores no custom one, which means the folder's
    // name.
    act(cx, window, |window, cx| {
        tab_to(window, "folder-panel-title", cx);
        window.press("ctrl-a", cx);
        window.press("backspace", cx);
        window.render_frame(cx);
        window.press("enter", cx);
    });
    wait_for_folder(cx, &store, |folder| folder.album_title.is_none()).await;
    assert_eq!(
        cx.read(|cx| store.read(cx).folders()[0].album_title.clone()),
        None,
        "#47: a cleared title means the folder's name"
    );
}
