//! The source folders and user decisions the client tracks (SPEC R2.1, R2.4,
//! R3.4, R5.3, R6.5).
//!
//! Every test drives the interface the shell calls, with no engine and no
//! server: the signed-out adapter is the one the no-observer cases cover.

use std::sync::{Arc, Mutex};

use sync_core::hash::Sha1;
use sync_core::store::{AlbumScope, ItemRecord, ItemState, SourceFolder, StateStore};
use sync_core::tracking::{Observer, Tracking, TrackingError};

/// An observer that only records what it was told, in order.
#[derive(Default)]
struct Recorded(Mutex<Vec<String>>);

impl Recorded {
    fn events(&self) -> Vec<String> {
        self.0.lock().expect("the recorded events").clone()
    }
}

impl Observer for Recorded {
    fn folder_added(&self, folder: &SourceFolder) {
        self.0
            .lock()
            .expect("the recorded events")
            .push(format!("added {}", folder.path));
    }

    fn folder_changed(&self, folder: &SourceFolder) {
        self.0
            .lock()
            .expect("the recorded events")
            .push(format!("changed {}", folder.path));
    }

    fn folder_removed(&self, path: &str) {
        self.0
            .lock()
            .expect("the recorded events")
            .push(format!("removed {path}"));
    }

    fn review_decided(&self, path: &str) {
        self.0
            .lock()
            .expect("the recorded events")
            .push(format!("review {path}"));
    }

    fn item_requeued(&self, path: &str) {
        self.0
            .lock()
            .expect("the recorded events")
            .push(format!("requeued {path}"));
    }
}

fn tracking_in(temp: &tempfile::TempDir) -> Arc<Tracking> {
    let store =
        StateStore::open(temp.path().join("data").join("state.db")).expect("open the state store");
    Arc::new(Tracking::new(Arc::new(store)))
}

fn record(path: &str, state: ItemState) -> ItemRecord {
    ItemRecord {
        path: path.to_string(),
        size: 11,
        mtime_ns: 1_700_000_000_000_000_000,
        sha1: Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").expect("a hex SHA-1"),
        asset_id: Some("asset-1".to_string()),
        state,
        error_class: None,
        error_message: None,
        attempts: 0,
        sidecar: None,
    }
}

#[test]
fn a_blank_path_is_refused_and_writes_nothing() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let tracking = tracking_in(&temp);

    let refused = tracking.add_folder("   ", true);

    assert!(
        matches!(refused, Err(TrackingError::EmptyFolder)),
        "{refused:?}"
    );
    assert!(tracking.folders().expect("read the folders").is_empty());
}

#[test]
fn a_re_add_under_a_different_spelling_keeps_the_album_settings() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let tracking = tracking_in(&temp);
    let photos = temp.path().join("photos");
    let path = photos.display().to_string();
    tracking.add_folder(&path, true).expect("add the folder");
    tracking
        .set_album_mapping(&path, true)
        .expect("map the folder");
    tracking
        .set_album_scope(&path, AlbumScope::WholeFolder)
        .expect("set the scope");

    // R3.4, R2.4: the store's upsert writes every column, so the re-add must
    // carry the settings, and the key must match the path spelled differently.
    tracking
        .add_folder(&format!("{path}/"), false)
        .expect("re-add the folder");

    let folders = tracking.folders().expect("read the folders");
    assert_eq!(folders.len(), 1, "the re-add must not make a second row");
    assert!(folders[0].album_mapping, "the re-add cleared the mapping");
    assert_eq!(folders[0].album_scope, AlbumScope::WholeFolder);
    assert!(!folders[0].recursive, "the re-add sets the recursive flag");
}

#[test]
fn an_album_edit_reaches_the_row_under_a_different_spelling() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let tracking = tracking_in(&temp);
    let path = temp.path().join("photos").display().to_string();
    tracking.add_folder(&path, true).expect("add the folder");

    // The engine compared path keys; the signed-out path compared the string
    // exactly. Both adapters now take this route.
    tracking
        .set_album_mapping(&format!("{path}/"), true)
        .expect("map the folder spelled with a trailing separator");
    tracking
        .set_album_scope(&path, AlbumScope::WholeFolder)
        .expect("set the scope");

    let folders = tracking.folders().expect("read the folders");
    assert!(folders[0].album_mapping);
    assert_eq!(folders[0].album_scope, AlbumScope::WholeFolder);
}

#[test]
fn an_album_edit_for_a_folder_the_client_does_not_hold_is_refused() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let tracking = tracking_in(&temp);
    tracking
        .add_folder(&temp.path().join("photos").display().to_string(), true)
        .expect("add the folder");

    let refused =
        tracking.set_album_mapping(&temp.path().join("elsewhere").display().to_string(), true);

    assert!(
        matches!(refused, Err(TrackingError::UnknownFolder(_))),
        "{refused:?}"
    );
}

#[test]
fn removing_a_folder_keeps_its_items_and_tells_the_observer() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let tracking = tracking_in(&temp);
    let observer = Arc::new(Recorded::default());
    tracking.attach(observer.clone());
    let photos = temp.path().join("photos");
    let path = photos.display().to_string();
    tracking.add_folder(&path, true).expect("add the folder");
    let item = format!("{path}/one.jpg");
    tracking
        .store()
        .record(&record(&item, ItemState::Uploaded))
        .expect("write the item");

    assert!(
        tracking.remove_folder(&path).expect("remove the folder"),
        "the client held the folder"
    );
    assert!(
        !tracking.remove_folder(&path).expect("remove it again"),
        "a folder the client no longer holds is not removed twice"
    );

    assert!(tracking.folders().expect("read the folders").is_empty());
    assert!(
        tracking
            .store()
            .item(&item)
            .expect("read the item")
            .is_some(),
        "a removed folder must not delete its items"
    );
    assert_eq!(
        observer.events(),
        vec![format!("added {path}"), format!("removed {path}")]
    );
}

#[test]
fn a_keep_clears_the_review_only_while_it_waits_and_tells_the_observer() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let tracking = tracking_in(&temp);
    let observer = Arc::new(Recorded::default());
    tracking.attach(observer.clone());
    let photos = temp.path().join("photos");
    let path = photos.display().to_string();
    tracking.add_folder(&path, true).expect("add the folder");
    let item = format!("{path}/gone.jpg");
    tracking
        .store()
        .record(&record(&item, ItemState::Missing))
        .expect("write the missing item");

    assert!(tracking.keep_review(&item).expect("keep the asset"));
    assert!(
        !tracking.keep_review(&item).expect("keep it again"),
        "a decided item is not decided twice"
    );

    assert_eq!(
        tracking
            .store()
            .item(&item)
            .expect("read")
            .expect("the row")
            .state,
        ItemState::ReviewKeep
    );
    assert_eq!(
        observer.events(),
        vec![format!("added {path}"), format!("review {item}")]
    );
}

#[test]
fn a_retry_requeues_a_parked_item_and_the_engine_hears_it() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let tracking = tracking_in(&temp);
    let observer = Arc::new(Recorded::default());
    tracking.attach(observer.clone());
    let photos = temp.path().join("photos");
    let path = photos.display().to_string();
    tracking.add_folder(&path, true).expect("add the folder");
    let item = format!("{path}/stuck.jpg");
    tracking
        .store()
        .record(&record(&item, ItemState::DeadLetter))
        .expect("write the parked item");

    assert!(tracking.retry_failed(&item).expect("retry the item"));
    assert!(
        !tracking.retry_failed(&item).expect("retry it again"),
        "a queued item is not queued twice"
    );

    assert_eq!(
        tracking
            .store()
            .item(&item)
            .expect("read")
            .expect("the row")
            .state,
        ItemState::Pending
    );
    assert_eq!(
        observer.events(),
        vec![format!("added {path}"), format!("requeued {item}")]
    );
}

#[test]
fn every_change_works_with_no_engine_attached() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let tracking = tracking_in(&temp);
    let path = temp.path().join("photos").display().to_string();

    // The signed-out adapter: nothing observes, and every write still lands.
    let folder = tracking.add_folder(&path, true).expect("add the folder");
    assert_eq!(folder.path, path);
    tracking.set_album_mapping(&path, true).expect("map it");
    tracking
        .store()
        .record(&record(&format!("{path}/one.jpg"), ItemState::Missing))
        .expect("write a missing item");
    assert!(
        tracking
            .keep_review(&format!("{path}/one.jpg"))
            .expect("keep it")
    );
    assert_eq!(tracking.review_items().expect("read the review").len(), 0);
}

/// An observer may edit a folder from its callback: the write lock is
/// released before the notification runs, so nothing waits on a lock its own
/// thread holds.
#[test]
fn an_observer_may_edit_a_folder_from_its_callback() {
    struct Mapper(Arc<Tracking>);

    impl Observer for Mapper {
        fn folder_added(&self, folder: &SourceFolder) {
            let _ = self.0.set_album_mapping(&folder.path, true);
        }
    }

    let temp = tempfile::tempdir().expect("create a temp dir");
    let tracking = tracking_in(&temp);
    tracking.attach(Arc::new(Mapper(Arc::clone(&tracking))));

    let path = temp.path().join("photos").display().to_string();
    tracking.add_folder(&path, true).expect("add the folder");

    assert!(
        tracking.folders().expect("read the folders")[0].album_mapping,
        "the observer's edit from the callback did not land"
    );
}
