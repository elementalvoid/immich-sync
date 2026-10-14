//! Headless tests for the deletion review view (issue #12, R5.3-R5.5, N5).
//!
//! The view is driven through its real controls: the rows it renders, their
//! accessible names, the keyboard tab order, the Keep and Trash buttons, and
//! the refusal message R5.5 asks for. A separate test drives the notification
//! R5.4 raises for a new review item.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Point, Role, TestAppContext, Window,
    WindowBounds, WindowOptions, px, size,
};
use immich_sync::connection::ConnectionModel;
use immich_sync::engine::EngineModel;
use immich_sync::paths::AppPaths;
use immich_sync::review_view::ReviewView;
use immich_sync::store::StoreModel;
use sync_core::connection::ServerProfile;
use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};
use sync_core::hash::Sha1;
use sync_core::store::{ItemRecord, ItemState, SourceFolder, StateStore};

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
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            size: size(px(1200.), px(800.)),
        })),
        ..Default::default()
    }
}

/// One missing file, written straight to the store before the view opens.
fn missing(path: &str) -> ItemRecord {
    ItemRecord {
        path: path.to_string(),
        size: 1024,
        mtime_ns: 1_700_000_000_000_000_000,
        sha1: Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").expect("a hex SHA-1"),
        asset_id: Some("asset-1".to_string()),
        state: ItemState::Missing,
        error_class: None,
        error_message: None,
        attempts: 0,
        sidecar: None,
    }
}

/// Seeds one source folder and `names` missing files under it.
fn seed(paths: &AppPaths, root: &Path, names: &[&str]) {
    let store = StateStore::open(&paths.state_db).expect("open the state database");
    store
        .add_folder(&SourceFolder {
            path: root.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: sync_core::store::AlbumScope::Leaf,
        })
        .expect("add the source folder");
    for name in names {
        store
            .record(&missing(&root.join(name).display().to_string()))
            .expect("write the missing row");
    }
}

/// Opens the review view over the seeded state database.
fn open_review_view(cx: &mut TestAppContext, paths: &AppPaths) -> AnyWindowHandle {
    let paths = paths.clone();
    cx.update(|cx| {
        gpui_kit::init(cx);
        let connection = cx.new(|_| {
            let store: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(ServerProfile::default(), store)
        });
        let store_model = cx.new(|cx| StoreModel::open(&paths, cx));
        gpui_kit::open_window(window_options(), cx, |window, cx| {
            let engine = cx.new(|cx| EngineModel::new(connection, store_model.clone(), &paths, cx));
            cx.new(|cx| ReviewView::new(engine, window, cx))
        })
        .expect("open the review window")
        .0
    })
}

/// Pumps the executor until the rendered view holds `count` rows.
async fn wait_for_rows(cx: &mut TestAppContext, window: AnyWindowHandle, count: usize) {
    for _ in 0..600 {
        if review_count(cx, window) == count {
            return;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    panic!("the review view never held {count} rows");
}

/// The number of rows the rendered view holds, read from the window's element
/// tree rather than from the model.
fn review_count(cx: &mut TestAppContext, window: AnyWindowHandle) -> usize {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        (0..16)
            .take_while(|index| window.try_find(format!("review-row-{index}")).is_some())
            .count()
    })
    .unwrap_or(0)
}

/// Moves focus to the next tab stop, as the Root's Tab binding does.
fn tab(window: &mut Window, cx: &mut App) {
    window.focus_next(cx);
    window.render_frame(cx);
}

/// Tabs until `id` holds the keyboard focus.
fn tab_to(window: &mut Window, id: &'static str, cx: &mut App) {
    for _ in 0..24 {
        if window.find(id).focused() == Some(true) {
            return;
        }
        tab(window, cx);
    }
    assert_eq!(
        window.find(id).focused(),
        Some(true),
        "{id} is not keyboard reachable"
    );
}

#[gpui_kit::test]
async fn the_review_names_every_row_and_both_actions(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    seed(&paths, &photos, &["IMG_0001.jpg", "CLIP_0002.mp4"]);
    let window = open_review_view(cx, &paths);
    wait_for_rows(cx, window, 2).await;

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("review-summary").label(),
            Some("2 files disappeared from your source folders."),
            "the summary states how much needs the user"
        );
        assert_eq!(window.find("review-list").role(), Some(Role::List));
        // The list is in path order, so CLIP_0002.mp4 is the first row.
        let row = window.find("review-row-0");
        assert_eq!(row.role(), Some(Role::ListItem));
        assert!(
            row.label()
                .is_some_and(|label| label.starts_with("CLIP_0002.mp4, missing from ")),
            "R5.3: the row is announced with its file: {:?}",
            row.label()
        );
        assert_eq!(
            window.find("review-keep-0").label(),
            Some("Keep CLIP_0002.mp4 on the server"),
            "the keep control is named for the file it keeps"
        );
        assert_eq!(
            window.find("review-trash-0").label(),
            Some("Move CLIP_0002.mp4 to the trash"),
            "the trash control is named for the file it trashes"
        );
    })
    .expect("read the review window");
}

#[gpui_kit::test]
async fn every_review_control_is_a_keyboard_reachable_tab_stop(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    seed(&paths, &photos, &["IMG_0001.jpg", "CLIP_0002.mp4"]);
    let window = open_review_view(cx, &paths);
    wait_for_rows(cx, window, 2).await;

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        // N5: every control has a name, and the keyboard reaches each one.
        tab_to(window, "review-keep-0", cx);
        tab_to(window, "review-trash-0", cx);
        tab_to(window, "review-keep-1", cx);
        tab_to(window, "review-trash-1", cx);
    })
    .expect("tab through the review controls");
}

#[gpui_kit::test]
async fn keeping_a_file_leaves_the_asset_and_clears_its_row(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    seed(&paths, &photos, &["keep.jpg", "other.jpg"]);
    let window = open_review_view(cx, &paths);
    wait_for_rows(cx, window, 2).await;

    let keep_path = photos.join("keep.jpg").display().to_string();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("review-keep-0", cx);
    })
    .expect("click Keep");

    wait_for_rows(cx, window, 1).await;
    let store = StateStore::open(&paths.state_db).expect("reopen the state database");
    assert_eq!(
        store
            .item(&keep_path)
            .expect("read the row")
            .expect("the row exists")
            .state,
        ItemState::ReviewKeep,
        "R5.3: Keep clears the item and leaves the server asset"
    );
    assert_eq!(store.review_count().expect("count the review"), 1);
}

#[gpui_kit::test]
async fn a_trash_while_signed_out_says_so_and_keeps_the_row(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    seed(&paths, &photos, &["denied.jpg"]);
    let window = open_review_view(cx, &paths);
    wait_for_rows(cx, window, 1).await;

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("review-trash-0", cx);
    })
    .expect("click Trash");

    for _ in 0..600 {
        if cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.find("review-problem").label().map(str::to_owned)
            })
            .ok()
            .flatten()
            .is_some()
        {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("review-problem").label(),
            Some("Sign in before moving an asset to the trash."),
            "R5.5: the refusal is actionable"
        );
        assert!(
            window.try_find("review-row-0").is_some(),
            "R5.5: the item stays in the review"
        );
    })
    .expect("read the review window");
}

#[gpui_kit::test]
async fn a_new_review_item_raises_one_notification(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    seed(&paths, &photos, &["gone.jpg"]);
    let paths_for_model = paths.clone();
    // The entity is kept alive: gpui deactivates a model whose last handle
    // goes away, and the status poll that raises the notification with it.
    let _engine = cx.update(|cx| {
        gpui_kit::init(cx);
        // The platform records a notification only with an identity set, as
        // the Windows backend needs one (R5.4, R10.1).
        cx.set_app_identity("com.immichsync.desktop", "Immich Sync");
        let connection = cx.new(|_| {
            let store: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(ServerProfile::default(), store)
        });
        let store_model = cx.new(|cx| StoreModel::open(&paths_for_model, cx));
        cx.new(|cx| EngineModel::new(connection, store_model, &paths_for_model, cx))
    });

    let mut shown = Vec::new();
    for _ in 0..600 {
        cx.run_until_parked();
        shown = cx.shown_system_notifications();
        if !shown.is_empty() {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
    }

    assert_eq!(
        shown.len(),
        1,
        "R5.4: a new review item raises exactly one notification"
    );
    assert_eq!(shown[0].tag.as_ref(), "deletion-review");
    assert_eq!(shown[0].title.as_ref(), "Immich Sync");
    assert!(
        shown[0].body.contains("disappeared"),
        "the notification says what happened: {}",
        shown[0].body
    );

    // The 250 ms status poll keeps running; the count did not rise again.
    for _ in 0..20 {
        cx.executor().timer(Duration::from_millis(100)).await;
        cx.run_until_parked();
    }
    assert_eq!(
        cx.shown_system_notifications().len(),
        1,
        "R5.4: the status poll must not repeat the notification"
    );
}
