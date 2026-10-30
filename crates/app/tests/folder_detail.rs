//! Headless tests for the folder detail (issue #43, SPEC R8.1, R5.4, N5).
//!
//! The detail is one source folder's live view, opened from its dashboard card
//! or that card's Details control. Every test drives the real controls: a real
//! state database, the real engine model, and the view's own click and keyboard
//! handling.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::scroll_dashboard_into_view;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, Point, ScrollDelta,
    TestAppContext, Window, WindowBounds, WindowOptions, point, px, rems, size,
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
    window_options_of(1200., 800.)
}

/// The same window at an explicit size, for a test about a short one.
fn window_options_of(width: f32, height: f32) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            size: size(px(width), px(height)),
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
    open_dashboard_at(cx, paths, server_url, window_options())
}

/// Opens the live dashboard in a window of `options`, as the shell does.
fn open_dashboard_at(
    cx: &mut TestAppContext,
    paths: &AppPaths,
    server_url: &str,
    options: WindowOptions,
) -> (AnyWindowHandle, Entity<StoreModel>, Entity<Dashboard>) {
    let paths = paths.clone();
    let profile = ServerProfile {
        server_url: server_url.to_string(),
        ..ServerProfile::default()
    };
    cx.update(|cx| {
        gpui_kit::init(cx);
        // #81: the panel is a dialog layer now, and its entrance slides for a
        // quarter second on the wall clock. A press is dispatched at the bounds
        // read a moment earlier, so under load it would land beside a control
        // that is still moving (#73). Reduced motion shows the final surface on
        // the first frame, as the kit's own dialog tests do.
        cx.set_reduce_motion(true);
        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        let connection = cx.new(|_| ConnectionModel::new(profile, credentials));
        let store = cx.new(|cx| StoreModel::open(&paths, cx));
        let engine = cx.new(|cx| EngineModel::new(connection, store.clone(), &paths, cx));
        let (window, view) = gpui_kit::open_window(options, cx, |window, cx| {
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

/// The settings line one folder card states (#57).
fn card_settings(window: &Window, index: usize) -> String {
    window
        .find((ElementId::from(("status-folder", index)), "settings"))
        .label()
        .unwrap_or_default()
        .to_string()
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

/// Opens the detail and waits for it to be on screen.
///
/// The Details control is focused before it is pressed, because the harness's
/// synthetic press moves no keyboard focus (a real one does, through the
/// button's own focus handle). That makes the state the layer sees the same one
/// a person leaves behind, so the focus return it performs is what the tests
/// below read (#81).
fn open_detail_now(cx: &mut TestAppContext, window: AnyWindowHandle) {
    act(cx, window, |window, cx| {
        scroll_dashboard_into_view(window, cx, ("folder-details", 0_usize));
        tab_to(window, ("folder-details", 0_usize), cx);
        window.click(("folder-details", 0_usize), cx);
    });
    read(cx, window, |window, _| {
        assert!(
            detail_open(window),
            "#43: the Details control opens the detail"
        );
    });
}

/// Scrolls the detail's body to its end with the wheel, as a person does when
/// the detail is taller than the panel (#81).
fn scroll_detail_to_bottom(window: &mut Window, cx: &mut App) {
    for _ in 0..8 {
        window.scroll(
            "folder-detail-body-dim",
            ScrollDelta::Pixels(point(px(0.), px(-120.))),
            cx,
        );
    }
}

#[gpui_kit::test]
async fn a_card_click_and_its_details_control_open_the_detail(cx: &mut TestAppContext) {
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
        // #53: the footer row keeps Details hugging its label at the card's
        // left edge, not stretched across the card.
        let details = f32::from(window.find(("folder-details", 0_usize)).bounds().size.width);
        let card = f32::from(window.find(("folder-card", 0_usize)).bounds().size.width);
        assert!(
            details < card / 2.,
            "#53: Details hugs its label ({details}px) instead of filling the card ({card}px)"
        );
        // N5: the Details control is a keyboard-reachable tab stop of the card,
        // not only a pointer target.
        tab_to(window, ("folder-details", 0_usize), cx);
        // #53: Details is the card's last tab stop, so Tab wraps to the
        // Dashboard's first control; any control after Details takes that stop.
        window.focus_next(cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("dashboard-sync-now").focused(),
            Some(true),
            "#53: Tab after the card's last control wraps to the Dashboard's first"
        );
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

    // Esc closes it and the card gets the keyboard back (#43).
    act(cx, window, |window, cx| window.press("escape", cx));
    read(cx, window, |window, _| {
        assert!(!detail_open(window), "#43: Escape closes the detail");
        assert_eq!(
            window.find(("folder-card", 0_usize)).focused(),
            Some(true),
            "#43, #81: a press on the card body gets the keyboard back on that card"
        );
    });
    // The card is a container, not a tab stop: Tab descends to its Details
    // control, which is where a keyboard user continues (#43).
    act(cx, window, |window, cx| window.press("tab", cx));
    read(cx, window, |window, _| {
        assert_eq!(
            window.find(("folder-details", 0_usize)).focused(),
            Some(true),
            "#43: Tab from the restored card reaches its Details control"
        );
    });

    // #81: the layer hands the keyboard back to the control that opened the
    // panel. The Details control is focused before it is pressed, because the
    // harness's synthetic press moves no keyboard focus where a real one does.
    act(cx, window, |window, cx| {
        tab_to(window, ("folder-details", 0_usize), cx);
        window.click(("folder-details", 0_usize), cx);
    });
    read(cx, window, |window, _| {
        assert!(detail_open(window), "the Details control opens the detail");
    });
    act(cx, window, |window, cx| window.press("escape", cx));
    read(cx, window, |window, _| {
        assert!(!detail_open(window), "Escape closes it again");
        assert_eq!(
            window.find(("folder-details", 0_usize)).focused(),
            Some(true),
            "#43, #81: Escape returns focus to the card's Details control"
        );
    });

    // #57: the card states the settings and offers no control of its own, so
    // the only interactive parts are the card itself and Details.
    read(cx, window, |window, _| {
        let card = ElementId::from(("folder-card", 0_usize));
        let controls: Vec<_> = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|snapshot| snapshot.path().contains(&card))
            .filter_map(|snapshot| snapshot.role())
            .filter(|role| matches!(role, gpui_kit::Role::Switch | gpui_kit::Role::RadioButton))
            .collect();
        assert!(
            controls.is_empty(),
            "#57: the card has no switch or radio: {controls:?}"
        );
        assert!(
            !detail_open(window),
            "nothing on the card opened the detail"
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

    // R8.1, #57: the card behind the dialog states the same values once it
    // closes.
    act(cx, window, |window, cx| window.press("escape", cx));
    read(cx, window, |window, _| {
        assert!(!detail_open(window));
        assert_eq!(
            card_settings(window, 0),
            "Recursive: off · Album mapping: on · Album scope: One album for this folder"
        );
    });
}

#[gpui_kit::test]
async fn the_detail_gives_each_setting_its_own_row(cx: &mut TestAppContext) {
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
        // #57: one setting per row, so each control starts below the one
        // before it and the two scopes sit together on the scope's row.
        let recursive = window.find("folder-panel-recursive").bounds();
        let mapping = window.find("folder-panel-album").bounds();
        let leaf = window.find("folder-panel-album-leaf").bounds();
        let whole = window.find("folder-panel-album-whole").bounds();
        assert!(
            recursive.bottom() <= mapping.top(),
            "#57: Recursive gets its own row: {recursive:?}"
        );
        assert!(
            mapping.bottom() <= leaf.top().min(whole.top()),
            "#57: Album mapping gets its own row: {mapping:?}"
        );
        assert!(
            leaf.bottom() <= whole.top(),
            "#57: the two scopes sit together in the scope's row: {leaf:?} vs {whole:?}"
        );
        assert!(
            (leaf.left() - whole.left()).abs() <= px(0.5),
            "#57: the scopes line up as one control block: {leaf:?} vs {whole:?}"
        );
    });
}

#[gpui_kit::test]
async fn the_detail_settings_reach_the_store_by_pointer_and_by_keyboard(cx: &mut TestAppContext) {
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
    // #57: the card no longer carries the settings, so the detail is where
    // each one is turned by pointer and by keyboard (N5).
    act(cx, window, |window, cx| {
        tab_to(window, "folder-panel-album", cx);
        window.press("space", cx);
    });
    let mapped = wait_for_folder(cx, &store, |folder| folder.album_mapping).await;
    assert_eq!(
        mapped.album_scope,
        AlbumScope::Leaf,
        "R3.4: the scope is per folder and starts at one album per folder"
    );

    // With the mapping on, the scope radios take focus and answer the keyboard.
    act(cx, window, |window, cx| {
        tab_to(window, "folder-panel-album-whole", cx);
        // #81: Enter reaches the focused control inside the panel again, because
        // the dialog no longer binds it to a Confirm the panel does not have.
        window.press("enter", cx);
    });
    let whole = wait_for_folder(cx, &store, |folder| {
        folder.album_scope == AlbumScope::WholeFolder
    })
    .await;
    assert!(whole.album_mapping, "the mapping is still on");

    // The pointer chooses the other scope without touching the mapping.
    act(cx, window, |window, cx| {
        window.click("folder-panel-album-leaf", cx);
    });
    let leaf = wait_for_folder(cx, &store, |folder| folder.album_scope == AlbumScope::Leaf).await;
    assert!(leaf.album_mapping);

    // R2.1: recursion toggles by pointer and by keyboard, and neither the
    // mapping nor the scope moves with it.
    act(cx, window, |window, cx| {
        window.click("folder-panel-recursive", cx);
    });
    let flat = wait_for_folder(cx, &store, |folder| !folder.recursive).await;
    assert!(
        flat.album_mapping,
        "R2.1: the pointer toggle keeps the mapping"
    );
    assert_eq!(flat.album_scope, AlbumScope::Leaf);

    act(cx, window, |window, cx| {
        tab_to(window, "folder-panel-recursive", cx);
        window.press("space", cx);
    });
    let recursive = wait_for_folder(cx, &store, |folder| folder.recursive).await;
    assert!(
        recursive.album_mapping,
        "R2.1: the keyboard toggle keeps the mapping"
    );
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
            for behind in ["dashboard-folder-path", "dashboard-add-folder"] {
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

/// #81: Enter activates the focused control inside the panel again.
///
/// The layer's dialog binds `enter` to its own `Confirm`, and a matched binding
/// swallows the key before the raw event reaches the focused control, so Enter
/// was inert on every non-text control in the panel while Space still worked.
/// `.keyboard(false)` drops that context, so this test tells the fix from the
/// bug for Enter, for Space, for Escape, and for the focus trap the same
/// context supplied.
#[gpui_kit::test]
async fn enter_activates_the_focused_control_and_the_other_keys_still_work(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    // Enter on the panel's own Close control closes it, and the layer still
    // hands the keyboard back to the control that opened the panel.
    open_detail_now(cx, window);
    act(cx, window, |window, cx| {
        tab_to(window, "folder-detail-close", cx);
        window.press("enter", cx);
    });
    read(cx, window, |window, _| {
        assert!(
            !detail_open(window),
            "#81: Enter activates the focused Close control"
        );
        assert_eq!(
            window.find(("folder-details", 0_usize)).focused(),
            Some(true),
            "#81: closing with Enter still returns the keyboard to the opener"
        );
    });

    // Space is unchanged: it activates the same control. That the detail opens
    // again at all is the other half of the first proof: the panel's own event
    // closed it, not the layer confirming the dialog behind the panel's back
    // (an open panel would refuse this reopen).
    open_detail_now(cx, window);
    act(cx, window, |window, cx| {
        tab_to(window, "folder-detail-close", cx);
        window.press("space", cx);
    });
    read(cx, window, |window, _| {
        assert!(
            !detail_open(window),
            "#81: Space still activates the focused Close control"
        );
    });

    // A second, non-closing control: Enter on Remove opens its confirmation
    // over the panel, so the click that control dispatches really ran.
    open_detail_now(cx, window);
    act(cx, window, |window, cx| {
        tab_to(window, "folder-detail-remove", cx);
        window.press("enter", cx);
        common::settle_alert(window, cx);
    });
    read(cx, window, |window, _| {
        assert!(
            window.try_find("folder-detail-confirm").is_some(),
            "#81: Enter activates Remove, which asks before it removes"
        );
        assert!(
            detail_open(window),
            "#81: Enter on a panel control does not confirm the dialog away"
        );
    });

    // Escape closes the topmost layer and then the panel, as before.
    act(cx, window, |window, cx| {
        window.press("escape", cx);
        common::settle_alert(window, cx);
    });
    read(cx, window, |window, _| {
        assert!(
            window.try_find("folder-detail-confirm").is_none(),
            "#81: Escape dismisses the confirmation first"
        );
        assert!(
            detail_open(window),
            "the panel is still there behind its confirmation"
        );
    });
    act(cx, window, |window, cx| window.press("escape", cx));
    read(cx, window, |window, _| {
        assert!(!detail_open(window), "#81: Escape still closes the panel");
    });

    // The focus trap the layer supplies is not part of what `.keyboard(false)`
    // turned off: Tab from the panel's last control wraps inside the dialog.
    open_detail_now(cx, window);
    act(cx, window, |window, cx| {
        tab_to(window, "folder-detail-remove", cx);
        window.press("tab", cx);
    });
    read(cx, window, |window, _| {
        assert_eq!(
            window.find("folder-detail-close").focused(),
            Some(true),
            "#81: Tab from the last control wraps to the panel's first, not to the page"
        );
    });
}

/// The panel's surface is centred on the window in both axes (#81).
fn assert_centred(window: &Window, what: &str) {
    let surface = window.find("surface").bounds();
    let viewport = Bounds::new(Point::default(), window.viewport_size());
    assert!(
        (surface.center().x - viewport.center().x).abs() <= px(1.),
        "#81: {what} is centred across the window: {surface:?} in {viewport:?}"
    );
    assert!(
        (surface.center().y - viewport.center().y).abs() <= px(1.),
        "#81: {what} is centred down the window: {surface:?} in {viewport:?}"
    );
}

/// #81: the layer sizes the panel, and both modes keep the width they had:
/// the setup is the narrow one and the detail carries more.
#[gpui_kit::test]
async fn both_panel_widths_go_through_the_layer_and_stay_centred(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    let (window, store, _view) = open_dashboard(cx, &paths);
    wait_for_store(cx, &store).await;

    // #40: the setup dialog.
    act(cx, window, |window, cx| {
        window.click("dashboard-add-folder", cx);
    });
    read(cx, window, |window, _| {
        assert_eq!(label(window, "folder-panel"), "Folder setup");
        let rem = window.rem_size();
        assert_eq!(
            window.find("surface").bounds().size.width,
            rems(30.).to_pixels(rem),
            "#81: the setup keeps its 30rem width"
        );
        // The layer sizes the surface from its content, so the short setup
        // dialog does not become a full-height page.
        assert!(
            window.find("surface").bounds().size.height < px(600.),
            "#81: the setup keeps its short surface: {:?}",
            window.find("surface").bounds()
        );
        assert_centred(window, "the setup dialog");
    });

    // #43: the detail.
    act(cx, window, |window, cx| window.press("escape", cx));
    read(cx, window, |window, _| {
        assert!(!detail_open(window), "Escape closes the setup")
    });
    open_detail_now(cx, window);
    read(cx, window, |window, _| {
        assert_eq!(label(window, "folder-panel"), "Folder detail");
        let rem = window.rem_size();
        assert_eq!(
            window.find("surface").bounds().size.width,
            rems(34.).to_pixels(rem),
            "#81: the detail keeps its 34rem width"
        );
        assert_centred(window, "the folder detail");
    });

    // #81: the narrowest window the shell allows (720×480). The detail is too
    // tall for it, so the layer caps the surface and centres what is left.
    let (window, store, _view) = open_dashboard_at(cx, &paths, "", window_options_of(720., 480.));
    wait_for_store(cx, &store).await;
    open_detail_now(cx, window);
    read(cx, window, |window, _| {
        let surface = window.find("surface").bounds();
        let viewport = Bounds::new(Point::default(), window.viewport_size());
        assert_centred(window, "the folder detail at the narrowest window");
        assert!(
            surface.top() >= px(0.) && surface.bottom() <= viewport.size.height,
            "#81: the detail fits the narrowest window: {surface:?} in {viewport:?}"
        );
        assert!(
            surface.size.width <= viewport.size.width - px(32.),
            "#81: the detail keeps the layer's margin at the narrowest window: {surface:?}"
        );
    });
}

/// #81: the layer sizes the panel, so at the widest content the detail carries
/// (a full page of uploaded assets) the list keeps #44's five rows, and the
/// wheel — not a test-side scroll — reaches the last row of controls.
///
/// The detail with 50 assets loaded is 866 px tall and the layer allows the
/// panel 734 px, so this window scrolls the panel's body: that is #44's list at
/// its own 18rem cap, not a data view shrunk to fit. A folder with a few assets
/// fits whole, and at 1200x480 the same body scrolls for the same reason (#43).
#[gpui_kit::test]
async fn the_list_keeps_five_rows_and_the_wheel_reaches_the_last_control(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
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
    read(cx, window, |window, cx| {
        // #44, #81: the data view is not the region that pays for the controls.
        let first = window.find(("folder-asset", 0_usize)).bounds().origin.y;
        let second = window.find(("folder-asset", 1_usize)).bounds().origin.y;
        let rows = f32::from(window.find("folder-assets-list").bounds().size.height)
            / f32::from(second - first);
        assert!(
            rows >= 5.,
            "#81: the list keeps five rows at the default window: {rows}"
        );

        let viewport = Bounds::new(Point::default(), window.viewport_size());
        // Everything above the list is on screen before any scroll.
        for id in [
            "folder-detail-close",
            "folder-panel-path",
            "folder-detail-state",
            "folder-detail-counts",
            "folder-panel-title",
            "folder-panel-recursive",
            "folder-detail-upload",
            "folder-detail-scan",
        ] {
            let bounds = window.find(id).bounds();
            assert!(window.find(id).visible(), "#81: {id} is on screen");
            assert!(
                viewport.contains(&bounds.origin)
                    && viewport.contains(&point(bounds.right(), bounds.bottom())),
                "#81: {id} is inside the window: {bounds:?} in {viewport:?}"
            );
        }

        // The panel's own body is what scrolls; the dashboard stays put.
        let title = window.find("folder-detail-title").bounds().origin.y;
        scroll_detail_to_bottom(window, cx);
        assert!(
            window.find("folder-detail-title").bounds().origin.y < title,
            "#81: the panel scrolled, not the dashboard behind it"
        );
        for id in [
            "folder-assets-more",
            "folder-detail-open",
            "folder-detail-failed",
            "folder-detail-review",
            "folder-detail-remove",
        ] {
            let bounds = window.find(id).bounds();
            assert!(
                window.find(id).visible(),
                "#81: {id} is on screen after a user's scroll"
            );
            assert!(
                viewport.contains(&bounds.origin)
                    && viewport.contains(&point(bounds.right(), bounds.bottom())),
                "#81: {id} is inside the window after the scroll: {bounds:?} in {viewport:?}"
            );
        }
    });
    // The harness refuses a clipped target, so this press is the proof that the
    // bottom-most control is reachable with the wheel alone.
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove", cx);
        common::settle_alert(window, cx);
    });
    read(cx, window, |window, _| {
        assert!(
            window.try_find("folder-detail-confirm").is_some(),
            "#81: Remove answers a press after a user's scroll"
        );
    });
}

/// #43, #81: on a window too short for the panel the body scrolls, so the wheel
/// still reaches the last control instead of clipping it away.
#[gpui_kit::test]
async fn a_short_window_scrolls_the_panels_body_instead_of_clipping_it(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    seed_folder(&paths, &photos, true);
    let (window, store, _view) = open_dashboard_at(cx, &paths, "", window_options_of(1200., 480.));
    wait_for_store(cx, &store).await;

    open_detail_now(cx, window);
    read(cx, window, |window, cx| {
        assert!(
            !window.find("folder-detail-remove").visible(),
            "a window this short pushes the last control past the panel's edge"
        );
        let first = window.find("folder-detail-title").bounds().origin.y;
        for _ in 0..8 {
            window.scroll(
                "folder-detail-body-dim",
                ScrollDelta::Pixels(point(px(0.), px(-120.))),
                cx,
            );
        }
        assert!(
            window.find("folder-detail-remove").visible(),
            "#43: the body scrolls, so the wheel reaches the last control"
        );
        assert!(
            window.find("folder-detail-title").bounds().origin.y < first,
            "#81: the panel scrolled, not the dashboard behind it"
        );
        assert!(
            window.find("folder-panel").bounds().bottom() <= window.viewport_size().height,
            "the panel never runs past the window"
        );
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
    // #81: the outside press is the window dialog layer's backdrop now, so
    // this aims at the layer's own surface frame, well clear of the panel. The
    // base dialog ignores presses in its title-bar band, so the point is below
    // it.
    act(cx, window, |window, cx| {
        window.click_at("dialog", point(px(8.), px(160.)), cx);
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
    // click: the layer's surface occludes the backdrop under it (#81).
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

    // #53: the card offers no Remove, so the detail is the only way in.
    read(cx, window, |window, _| {
        assert!(
            window.try_find(("folder-remove", 0_usize)).is_none(),
            "#53: a source folder card has no Remove control"
        );
        assert!(
            window.try_find(("folder-details", 0_usize)).is_some(),
            "#53: the card's footer keeps Details"
        );
    });

    open_detail_now(cx, window);
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove", cx);
        common::settle_alert(window, cx);
    });
    read(cx, window, |window, _| {
        assert!(
            detail_open(window),
            "#43: the first Remove asks instead of removing"
        );
        assert!(
            window.try_find(("folder-remove", 0_usize)).is_none(),
            "#53: no card state offers a second, unconfirmed removal path"
        );
        assert_eq!(
            label(window, "folder-detail-confirm"),
            "Remove photos?",
            "the confirmation names the folder"
        );
        // #73: the answer is a window-level alert, so it is on screen wherever
        // the detail's body has scrolled to. It is not a child of the panel, and
        // no test-side scrolling is needed to reach it.
        assert!(
            window
                .within("folder-panel")
                .try_find("folder-detail-confirm")
                .is_none(),
            "the confirmation is its own layer, not panel content"
        );
        let confirm = window.find("folder-detail-remove-confirm").bounds();
        let viewport = window.viewport_size();
        assert!(
            confirm.top() >= px(0.) && confirm.bottom() <= viewport.height,
            "the confirmation opens on screen without scrolling: {confirm:?} in {viewport:?}"
        );
        // #43 kept the panel readable but subordinate: it stays behind the
        // alert rather than being replaced by it.
        assert!(
            detail_open(window) && window.try_find("folder-detail-body-dim").is_some(),
            "the panel stays behind the confirmation"
        );
        // #73: one pair of answers, not two. The alert builds its own OK and
        // Cancel row unless a footer is set, and its buttons carry the bare
        // `ok` and `cancel` ids, so their absence is what pins the single row.
        assert!(
            window.try_find("ok").is_none() && window.try_find("cancel").is_none(),
            "the confirmation renders one pair of buttons"
        );
    });

    // The alert owns the keyboard, so Tab reaches the answer instead of the
    // panel behind it.
    act(cx, window, |window, cx| window.press("tab", cx));
    read(cx, window, |window, _| {
        let on_cancel = window.find("folder-detail-remove-cancel").focused();
        let on_confirm = window.find("folder-detail-remove-confirm").focused();
        assert!(
            on_cancel == Some(true) || on_confirm == Some(true),
            "the keyboard reaches the confirmation's own controls"
        );
    });

    // Escape cancels the confirmation only, not the dialog.
    act(cx, window, |window, cx| window.press("escape", cx));
    read(cx, window, |window, _| {
        assert!(
            detail_open(window),
            "#43: Escape during the confirmation keeps the dialog"
        );
    });
    // The alert layer clears on a frame of its own, so this waits instead of
    // reading the frame right after the press (#73).
    cx.wait_for(window, Duration::from_secs(10), |window, _| {
        window.try_find("folder-detail-confirm").is_none()
    })
    .await;

    // Off, and on again: Cancel does the same.
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove", cx);
        common::settle_alert(window, cx);
    });
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove-cancel", cx);
    });
    read(cx, window, |window, _| assert!(detail_open(window)));
    cx.wait_for(window, Duration::from_secs(10), |window, _| {
        window.try_find("folder-detail-confirm").is_none()
    })
    .await;
    assert_eq!(
        cx.read(|cx| store.read(cx).folders().len()),
        1,
        "#43: neither Escape nor Cancel removed the folder"
    );

    // The second Remove is the one that drops it.
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove", cx);
        common::settle_alert(window, cx);
    });
    act(cx, window, |window, cx| {
        window.click("folder-detail-remove-confirm", cx);
    });
    // The press has to reach the button: the alert closes on it whether the
    // write succeeds or not, so an alert still up means the click landed
    // nowhere and nothing was ever asked of the store. The layer clears on a
    // frame of its own, so this waits rather than reading the next frame.
    cx.wait_for(window, Duration::from_secs(10), |window, _| {
        window.try_find("folder-detail-confirm").is_none()
    })
    .await;
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
        window.click("folder-detail-remove", cx);
        common::settle_alert(window, cx);
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
        window.click("folder-detail-remove", cx);
        common::settle_alert(window, cx);
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
        window.click("folder-detail-remove", cx);
        common::settle_alert(window, cx);
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
        window.click("folder-detail-remove", cx);
        common::settle_alert(window, cx);
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
        window.click("folder-detail-remove", cx);
        common::settle_alert(window, cx);
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
        scroll_detail_to_bottom(window, cx);
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
    // #81: Enter reaches the focused control inside the panel again.
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
        scroll_detail_to_bottom(window, cx);
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
    window.press(crate::common::select_all_chord(), cx);
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
        window.press(crate::common::select_all_chord(), cx);
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
