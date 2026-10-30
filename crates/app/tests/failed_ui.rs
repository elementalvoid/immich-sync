//! Headless tests for the failed-items view (issue #7, R6.5, N5).
//!
//! The view is driven through its real controls: the rows it renders, their
//! accessible names, the keyboard tab order, and the retry button.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::base::test_support::snapshots;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Point, Role, SharedString,
    TestAppContext, Window, WindowBounds, WindowOptions, px, size,
};
use immich_sync::connection::ConnectionModel;
use immich_sync::engine::EngineModel;
use immich_sync::failed_view::FailedView;
use immich_sync::paths::AppPaths;
use immich_sync::store::StoreModel;
use sync_core::connection::ServerProfile;
use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};
use sync_core::failure::FailureClass;
use sync_core::hash::Sha1;
use sync_core::store::{AlbumScope, ItemRecord, ItemState, SourceFolder, StateStore};

/// The sentence the empty list explains itself with (#77).
const EMPTY_DESCRIPTION: &str = "A file the client gives up on appears here, with the reason.";

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

/// One parked row, written straight to the store before the view opens.
fn parked(
    path: &str,
    state: ItemState,
    class: Option<FailureClass>,
    message: Option<&str>,
    attempts: u32,
) -> ItemRecord {
    ItemRecord {
        path: path.to_string(),
        size: 1024,
        mtime_ns: 1_700_000_000_000_000_000,
        sha1: Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").expect("a hex SHA-1"),
        asset_id: None,
        state,
        error_class: class,
        error_message: message.map(str::to_string),
        attempts,
        // R3.3: no sidecar has been sent in the fixture.
        sidecar: None,
    }
}

/// Seeds the state database and opens the failed-items view over it.
fn open_failed_view(
    cx: &mut TestAppContext,
    paths: &AppPaths,
) -> (AnyWindowHandle, gpui_kit::Entity<FailedView>) {
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
            cx.new(|cx| FailedView::new(engine, window, cx))
        })
        .expect("open the failed-items window")
    })
}

/// Adds one source folder to the state database, before the view opens.
///
/// The folder path does not have to exist: the client keeps an unavailable
/// source folder in its list (R2.5).
fn seed_folder(paths: &AppPaths, path: &Path, recursive: bool) {
    let store = StateStore::open(&paths.state_db).expect("open the state database");
    store
        .add_folder(&SourceFolder {
            path: path.display().to_string(),
            recursive,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");
}

/// Pumps the executor until the rendered view holds `count` rows.
async fn wait_for_rows(cx: &mut TestAppContext, window: AnyWindowHandle, count: usize) {
    for _ in 0..600 {
        if failed_count(cx, window) == count {
            return;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    panic!("the failed-items view never held {count} rows");
}

/// The number of rows the rendered view holds, read from the window's element
/// tree rather than from the model.
fn failed_count(cx: &mut TestAppContext, window: AnyWindowHandle) -> usize {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        (0..16)
            .take_while(|index| window.try_find(format!("failed-row-{index}")).is_some())
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
fn tab_to(window: &mut Window, id: impl Into<SharedString>, cx: &mut App) {
    let id: SharedString = id.into();
    for _ in 0..24 {
        if window.find(id.clone()).focused() == Some(true) {
            return;
        }
        tab(window, cx);
    }
    assert_eq!(
        window.find(id.clone()).focused(),
        Some(true),
        "{id} is not keyboard reachable"
    );
}

#[gpui_kit::test]
async fn the_failed_view_names_every_row_and_its_retry_control(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    {
        let store = StateStore::open(&paths.state_db).expect("open the state database");
        store
            .record(&parked(
                "/photos/IMG_0001.cr3",
                ItemState::FailedPermanent,
                Some(FailureClass::UnsupportedFormat),
                Some("the server does not accept this file format"),
                1,
            ))
            .expect("write the permanent row");
        store
            .record(&parked(
                "/videos/CLIP_0002.mp4",
                ItemState::DeadLetter,
                Some(FailureClass::ServerTransient),
                Some("the server answered 503"),
                8,
            ))
            .expect("write the dead-letter row");
        store
            .record(&parked(
                "/videos/CLIP_0003.mp4",
                ItemState::DeadLetter,
                None,
                None,
                8,
            ))
            .expect("write the unclassified row");
    }
    let (window, _view) = open_failed_view(cx, &paths);
    wait_for_rows(cx, window, 3).await;

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-summary").label(),
            Some("3 files could not be uploaded."),
            "the summary states how much needs the user"
        );
        assert_eq!(window.find("failed-list").role(), Some(Role::List));
        let row = window.find("failed-row-0");
        assert_eq!(row.role(), Some(Role::ListItem));
        assert_eq!(
            row.label(),
            Some("IMG_0001.cr3, unsupported_format: the server does not accept this file format"),
            "R6.5: the row is announced with its class and its last error"
        );
        assert_eq!(
            window.find("failed-retry-0").label(),
            Some("Retry IMG_0001.cr3"),
            "the retry control is named for the file it retries"
        );
        assert_eq!(
            window.find("failed-row-2").label(),
            Some("CLIP_0003.mp4, unclassified: The client did not record a reason."),
            "a row without a recorded reason says so instead of inventing one"
        );
    })
    .expect("read the failed-items window");
}

/// #76: a failed row states its class and error once, and a list of N rows is
/// N names, not N alerts.
///
/// The row's accessible name carries the reason; the strip inside it paints the
/// accent edge and the class, and adds no accessibility node of its own, so the
/// same information does not arrive a second time and nothing announces over
/// the list.
#[gpui_kit::test]
async fn a_failed_row_states_its_reason_once_and_the_list_is_not_a_stack_of_alerts(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let reasons = [
        "the server does not accept this file format",
        "the server answered 503",
    ];
    {
        let store = StateStore::open(&paths.state_db).expect("open the state database");
        for (index, reason) in reasons.iter().enumerate() {
            store
                .record(&parked(
                    &format!("/photos/IMG_000{index}.cr3"),
                    ItemState::FailedPermanent,
                    Some(FailureClass::ServerTransient),
                    Some(reason),
                    2,
                ))
                .expect("write the parked row");
        }
    }
    let (window, _view) = open_failed_view(cx, &paths);
    wait_for_rows(cx, window, 2).await;

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        for (index, reason) in reasons.iter().enumerate() {
            let row: ElementId = format!("failed-row-{index}").into();
            let carrying: Vec<_> = snapshots(window)
                .into_iter()
                .filter(|element| element.path().contains(&row))
                .filter(|element| {
                    [element.label(), element.value()]
                        .into_iter()
                        .flatten()
                        .any(|text| text.contains(reason))
                })
                .collect();
            assert_eq!(
                carrying.len(),
                1,
                "#76: row {index} states its error once: {:?}",
                carrying
                    .iter()
                    .map(|element| (element.role(), element.path().to_vec()))
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                carrying[0].role(),
                Some(Role::ListItem),
                "#76: the row's own name is the announcement"
            );
            let strip = window.find(format!("failed-row-notice-{index}"));
            assert!(
                strip.visible(),
                "#76: the quiet strip is still painted: {strip:?}"
            );
            assert_eq!(
                (strip.role(), strip.label()),
                (None, None),
                "#76: the quiet strip names nothing, so it cannot repeat the row"
            );
        }
        let alerts: Vec<_> = snapshots(window)
            .into_iter()
            .filter(|element| element.role() == Some(Role::Alert))
            .collect();
        assert!(
            alerts.is_empty(),
            "#76: two failed rows are two names, not two alerts: {:?}",
            alerts
                .iter()
                .map(|element| (element.label(), element.path().to_vec()))
                .collect::<Vec<_>>()
        );
    })
    .expect("read the failed-items window");
}

#[gpui_kit::test]
async fn every_retry_control_is_a_keyboard_reachable_tab_stop(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    {
        let store = StateStore::open(&paths.state_db).expect("open the state database");
        for index in 0..2 {
            store
                .record(&parked(
                    &format!("/videos/CLIP_{index}.mp4"),
                    ItemState::DeadLetter,
                    Some(FailureClass::ServerTransient),
                    Some("the server answered 503"),
                    8,
                ))
                .expect("write the dead-letter row");
        }
    }
    let (window, _view) = open_failed_view(cx, &paths);
    wait_for_rows(cx, window, 2).await;

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        // N5: every control has a name, and the keyboard reaches each one.
        tab_to(window, "failed-retry-0", cx);
        tab_to(window, "failed-retry-1", cx);
    })
    .expect("tab through the retry controls");
}

#[gpui_kit::test]
async fn a_retry_click_requeues_the_item_and_clears_its_row(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let parked_path = "/videos/CLIP_0002.mp4".to_string();
    {
        let store = StateStore::open(&paths.state_db).expect("open the state database");
        store
            .record(&parked(
                &parked_path,
                ItemState::DeadLetter,
                Some(FailureClass::ServerTransient),
                Some("the server answered 503"),
                8,
            ))
            .expect("write the dead-letter row");
    }
    let (window, _view) = open_failed_view(cx, &paths);
    wait_for_rows(cx, window, 1).await;

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("failed-retry-0", cx);
    })
    .expect("click Retry");

    // R6.5: the item is queued again with a fresh budget, and it leaves the
    // failed-items list.
    let mut queued = false;
    for _ in 0..600 {
        let row = StateStore::open(&paths.state_db)
            .expect("reopen the state database")
            .item(&parked_path)
            .expect("read the row")
            .expect("the row exists");
        if row.state == ItemState::Pending && row.attempts == 0 {
            queued = true;
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(queued, "the retry did not re-queue the item");
    for _ in 0..600 {
        if failed_count(cx, window) == 0 {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert_eq!(
        failed_count(cx, window),
        0,
        "a re-queued item leaves the failed-items view"
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-summary").label(),
            Some("Nothing has failed."),
            "the empty state is stated"
        );
    })
    .expect("read the empty state");
}

/// One parked row under `path`, so the folder it belongs to is decided by the
/// path alone.
fn failed_under(paths: &AppPaths, path: &Path) {
    let store = StateStore::open(&paths.state_db).expect("open the state database");
    store
        .record(&parked(
            &path.display().to_string(),
            ItemState::DeadLetter,
            Some(FailureClass::ServerTransient),
            Some("the server answered 503"),
            8,
        ))
        .expect("write the dead-letter row");
}

#[gpui_kit::test]
async fn the_failed_view_offers_every_source_folder_under_all_folders(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    // `archive` is not on disk: an unavailable source folder stays in the
    // client's list (R2.5), so the filter must offer it too.
    let archive = temp.path().join("archive");
    seed_folder(&paths, &photos, true);
    seed_folder(&paths, &archive, true);
    failed_under(&paths, &photos.join("IMG_0001.jpg"));
    let (window, _view) = open_failed_view(cx, &paths);
    wait_for_rows(cx, window, 1).await;

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let trigger = window.find("failed-folder-filter");
        assert_eq!(
            trigger.role(),
            Some(Role::ComboBox),
            "#79: the filter is the kit's select, which a screen reader can place"
        );
        assert_eq!(
            trigger.label(),
            Some("Folder filter"),
            "#79: the control's own name is stable; the choice is its value"
        );
        assert_eq!(
            trigger.value(),
            Some("All folders"),
            "the filter starts on All folders and says so as its value"
        );
        assert_eq!(trigger.expanded(), Some(false));

        window.within("failed-folder-filter").click("input", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-folder-filter").expanded(),
            Some(true),
            "clicking the control opens its options"
        );
        assert_eq!(
            window.find("failed-folder-option-0").role(),
            Some(Role::ListBoxOption),
            "#79: each option is an option of the select's list"
        );
        assert_eq!(
            window.find("failed-folder-option-0").label(),
            Some("Show items from all folders"),
            "#79: the name says what the choice does; its state is not part of it"
        );
        assert_eq!(
            window.find("failed-folder-option-0").selected(),
            Some(true),
            "#79: the current choice reports itself as selected, so a screen reader gets the state"
        );
        assert_eq!(
            window.find("failed-folder-option-1").selected(),
            Some(false),
            "#79: every option reports the state, so an unchecked one says it is unchecked"
        );
        assert!(
            window.try_find("failed-folder-option-0-selected").is_some(),
            "the current choice carries a visible marker, so the selection is not colour alone"
        );
        assert_eq!(
            window.find("failed-folder-option-1").label(),
            Some(format!("Show only {}", archive.display()).as_str()),
            "the unavailable folder is offered like any other, named by its full path"
        );
        assert_eq!(
            window.find("failed-folder-option-2").label(),
            Some(format!("Show only {}", photos.display()).as_str())
        );
        assert!(
            window.try_find("failed-folder-option-1-selected").is_none(),
            "only the current choice is marked"
        );
        assert!(
            window.try_find("review-folder-option-0").is_none(),
            "#79: each surface keeps its own element ids"
        );

        // #79: Escape dismisses the options the way the kit's overlay contract
        // says, and the keyboard focus stays with the control.
        window.press("escape", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-folder-filter").expanded(),
            Some(false),
            "Escape closes the options"
        );
        assert!(window.try_find("failed-folder-option-0").is_none());
        assert_eq!(
            window.find("failed-folder-filter").focused(),
            Some(true),
            "Escape leaves the keyboard focus on the filter control"
        );
        assert_eq!(
            window.find("failed-folder-filter").value(),
            Some("All folders"),
            "dismissing without choosing keeps the choice"
        );
    })
    .expect("read the folder filter");
}

#[gpui_kit::test]
async fn choosing_a_folder_narrows_the_list_its_summary_and_its_empty_state(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let camera_roll = temp.path().join("Camera Roll");
    let videos = temp.path().join("Videos");
    seed_folder(&paths, &camera_roll, true);
    seed_folder(&paths, &videos, true);
    failed_under(&paths, &camera_roll.join("IMG_0001.cr3"));
    failed_under(&paths, &temp.path().join("outside.jpg"));
    let (window, _view) = open_failed_view(cx, &paths);
    wait_for_rows(cx, window, 2).await;

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-summary").label(),
            Some("2 files could not be uploaded."),
            "All folders counts every item"
        );

        // Videos sorts after Camera Roll, so it is the second folder option.
        window.within("failed-folder-filter").click("input", cx);
        assert_eq!(
            window.find("failed-folder-option-2").label(),
            Some(format!("Show only {}", videos.display()).as_str()),
            "labels stay short; the accessible name carries the full path"
        );
        window.click("failed-folder-option-2", cx);
    })
    .expect("choose the Videos folder");
    // The kit commits the selection after the click handler returns.
    cx.run_until_parked();

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let empty = window.find("failed-empty");
        assert_eq!(
            empty.label(),
            Some("No failed items in Videos"),
            "the empty state names the folder the filter shows"
        );
        assert_eq!(
            empty.description(),
            Some(EMPTY_DESCRIPTION),
            "#77: the filtered state explains the list in the same words as before"
        );
        assert_eq!(
            announced(window, "Nothing has failed"),
            0,
            "#77: an empty filtered list never claims the whole client is empty"
        );
        assert_eq!(
            window.find("failed-summary").label(),
            Some("No failed items in Videos"),
            "an empty filtered list is summarised for its folder, not for the whole client"
        );
        assert!(window.try_find("failed-row-0").is_none());

        window.within("failed-folder-filter").click("input", cx);
        window.click("failed-folder-option-1", cx);
    })
    .expect("choose the Camera Roll folder");
    cx.run_until_parked();

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-summary").label(),
            Some("1 file could not be uploaded."),
            "the summary counts the selected folder's items"
        );
        assert_eq!(failed_rows(window), 1);
        assert_eq!(
            window.find("failed-folder-chip").label(),
            Some("Showing Camera Roll"),
            "the active folder has a chip"
        );

        // #79: All folders is a choice of the list, not only the chip's Clear.
        window.within("failed-folder-filter").click("input", cx);
        window.click("failed-folder-option-0", cx);
    })
    .expect("choose All folders from the list");
    cx.run_until_parked();

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-summary").label(),
            Some("2 files could not be uploaded."),
            "choosing All folders lists every item again"
        );
        assert_eq!(
            window.find("failed-folder-filter").value(),
            Some("All folders"),
            "and the control says so"
        );
        assert!(
            window.try_find("failed-folder-chip").is_none(),
            "with no folder chosen there is no chip to clear"
        );
    })
    .expect("read the unfiltered failed-items view");
}

#[gpui_kit::test]
async fn the_empty_list_is_one_status_and_offers_nothing_to_retry(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    seed_folder(&paths, &temp.path().join("Camera Roll"), true);
    let (window, _view) = open_failed_view(cx, &paths);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let empty = window.find("failed-empty");
        assert_eq!(
            empty.label(),
            Some("Nothing has failed"),
            "#77: the unfiltered list states what has failed"
        );
        assert_eq!(
            empty.description(),
            Some(EMPTY_DESCRIPTION),
            "#77: the explanation is announced after the title, not instead of it"
        );
        assert_eq!(
            empty.role(),
            Some(Role::Status),
            "#77: the empty state is a status for assistive technology"
        );
        assert!(
            empty.visible(),
            "#77: the empty state is on screen: {empty:?}"
        );
        assert_eq!(
            announced(window, "Nothing has failed"),
            1,
            "#77: the empty state carries its title in one node; the summary above \
             states the same fact as \"Nothing has failed.\": {}",
            registered(window)
        );
        assert_eq!(
            announced(window, EMPTY_DESCRIPTION),
            1,
            "#77: the explanation is announced once: {}",
            registered(window)
        );
        assert!(
            window
                .try_find((ElementId::from("failed-empty"), "action"))
                .is_none(),
            "#77: with no row to retry the empty state offers no action"
        );
    })
    .expect("read the empty failed-items view");
}

/// Every observed element that announces `text`, for a failure message.
fn registered(window: &Window) -> String {
    snapshots(window)
        .iter()
        .filter(|element| element.label().is_some() || element.value().is_some())
        .map(|element| format!("{:?} {:?}", element.role(), element.path()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[gpui_kit::test]
async fn the_filter_and_its_clear_action_are_keyboard_reachable_and_named(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let camera_roll = temp.path().join("Camera Roll");
    seed_folder(&paths, &camera_roll, true);
    failed_under(&paths, &camera_roll.join("IMG_0001.cr3"));
    failed_under(&paths, &temp.path().join("outside.jpg"));
    let (window, _view) = open_failed_view(cx, &paths);
    wait_for_rows(cx, window, 2).await;

    // #79: the whole choice is made from the keyboard — no pointer, and no
    // tabbing through the options: Enter opens the list the way the kit
    // defines it, Down moves the cursor, Enter commits.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "failed-folder-filter", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-folder-filter").expanded(),
            Some(true),
            "Enter on the control opens its options"
        );
        window.press("down", cx);
        window.press("enter", cx);
        window.render_frame(cx);
    })
    .expect("choose a folder from the keyboard");

    cx.wait_for(window, Duration::from_secs(5), |window, _| {
        window.find("failed-folder-filter").value() == Some("Camera Roll")
    })
    .await;
    assert_eq!(
        failed_count(cx, window),
        1,
        "the keyboard choice narrows the list"
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-folder-filter").focused(),
            Some(true),
            "choosing a folder keeps the keyboard focus on the control"
        );
        assert_eq!(
            window.find("failed-folder-chip").label(),
            Some("Showing Camera Roll"),
            "the chip still names the folder on screen"
        );

        // The clear action is named and a tab stop of its own.
        assert_eq!(
            window.find("failed-folder-clear").label(),
            Some("Clear the folder filter")
        );
        tab_to(window, "failed-folder-clear", cx);
        window.click("failed-folder-clear", cx);
        window.render_frame(cx);
    })
    .expect("tab to the clear action and use it");

    cx.wait_for(window, Duration::from_secs(5), |window, _| {
        window.find("failed-folder-filter").value() == Some("All folders")
    })
    .await;
    assert_eq!(
        failed_count(cx, window),
        2,
        "clearing the filter restores every item"
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-folder-filter").focused(),
            Some(true),
            "clearing keeps the keyboard focus on the filter control"
        );
        assert!(window.try_find("failed-folder-chip").is_none());
    })
    .expect("read the cleared filter");
}

#[gpui_kit::test]
async fn the_view_opens_with_a_folder_already_selected(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let camera_roll = temp.path().join("Camera Roll");
    let videos = temp.path().join("Videos");
    seed_folder(&paths, &camera_roll, true);
    seed_folder(&paths, &videos, true);
    failed_under(&paths, &camera_roll.join("IMG_0001.cr3"));
    failed_under(&paths, &videos.join("CLIP_0002.mp4"));
    let (window, view) = open_failed_view(cx, &paths);
    wait_for_rows(cx, window, 2).await;

    // The seam the folder detail will call (#43).
    let selected = camera_roll.display().to_string();
    cx.update(|cx| {
        view.update(cx, |view, cx| view.select_folder(selected.clone(), cx));
    });
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .expect("render the selected folder");

    assert_eq!(failed_count(cx, window), 1, "the seam narrows the list");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-folder-chip").label(),
            Some("Showing Camera Roll")
        );
        assert_eq!(
            window.find("failed-folder-filter").value(),
            Some("Camera Roll"),
            "#79: a folder chosen elsewhere shows as the control's value"
        );
    })
    .expect("read the selected folder");
}

/// #79 S4: a folder that is no longer a source folder is no longer offered as a
/// choice, and the filter that names it still filters instead of quietly
/// listing everything.
#[gpui_kit::test]
async fn a_folder_that_is_no_longer_a_source_folder_still_filters(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    // `gone` was a source folder when the filter was chosen; it is not one now.
    let gone = temp.path().join("Gone");
    seed_folder(&paths, &photos, true);
    failed_under(&paths, &photos.join("IMG_0001.jpg"));
    failed_under(&paths, &gone.join("IMG_0002.jpg"));
    let (window, view) = open_failed_view(cx, &paths);
    wait_for_rows(cx, window, 2).await;

    let selected = gone.display().to_string();
    cx.update(|cx| {
        view.update(cx, |view, cx| view.select_folder(selected.clone(), cx));
    });
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .expect("render the selected folder");

    assert_eq!(
        failed_count(cx, window),
        1,
        "the folder still narrows the list, so it does not list every item"
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-folder-chip").label(),
            Some("Showing Gone")
        );
        assert_eq!(
            window.find("failed-folder-filter").value(),
            Some("Gone"),
            "#79: the control still names the folder the list is narrowed to"
        );
        window.within("failed-folder-filter").click("input", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("failed-folder-option-1").label(),
            Some(format!("Show only {}", photos.display()).as_str()),
            "the list holds only the folder that is still a source folder"
        );
        assert!(
            window.try_find("failed-folder-option-2").is_none(),
            "a folder that is no longer a source folder is not offered as a choice"
        );
    })
    .expect("read the stale filter");
}

/// The rows the rendered view holds, counted from `window`'s element tree.
fn failed_rows(window: &Window) -> usize {
    (0..16)
        .take_while(|index| window.try_find(format!("failed-row-{index}")).is_some())
        .count()
}
