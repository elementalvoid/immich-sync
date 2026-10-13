//! The state store: the first schema, its pragmas, and its startup check.

use std::path::Path;

use sync_core::failure::FailureClass;
use sync_core::hash::Sha1;
use sync_core::store::{ItemRecord, ItemState, StateStore};

/// `sha1("hello world")`, hex.
const HELLO_WORLD_SHA1_HEX: &str = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";

fn hello_world_sha1() -> Sha1 {
    Sha1::from_hex(HELLO_WORLD_SHA1_HEX).expect("the fixture is a hex SHA-1")
}

fn record(path: &str, state: ItemState) -> ItemRecord {
    ItemRecord {
        path: path.to_string(),
        size: 11,
        mtime_ns: 1_700_000_000_000_000_000,
        sha1: hello_world_sha1(),
        asset_id: Some("asset-1".to_string()),
        state,
        error_class: None,
    }
}

#[test]
fn a_fresh_store_holds_no_items_and_passes_its_check() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");

    assert_eq!(store.item("hello.txt").expect("read a missing item"), None);
    store
        .quick_check()
        .expect("a fresh store passes quick_check");
}

#[test]
fn the_writer_uses_wal_and_full_synchronous() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");

    let pragmas = store.pragmas().expect("read the writer pragmas");
    assert_eq!(pragmas.journal_mode, "wal", "ADR 0005 requires WAL");
    assert_eq!(
        pragmas.synchronous, 2,
        "synchronous=FULL is 2 (ADR 0005, N4)"
    );
    assert!(
        pragmas.busy_timeout_ms >= 1_000,
        "the one writer waits for a lock instead of failing: {}",
        pragmas.busy_timeout_ms
    );
}

#[test]
fn a_recorded_upload_round_trips_every_column() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    let mut row = record("hello.txt", ItemState::Uploaded);
    row.error_class = None;

    store.record(&row).expect("write the upload result");

    let read = store
        .item("hello.txt")
        .expect("read the item")
        .expect("the row exists");
    assert_eq!(read, row, "path, size, mtime, SHA-1, and asset id survive");
    assert_eq!(read.asset_id.as_deref(), Some("asset-1"));
}

#[test]
fn recording_the_same_path_twice_replaces_the_row() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .record(&record("hello.txt", ItemState::Uploaded))
        .expect("write the first result");

    let mut changed = record("hello.txt", ItemState::Duplicate);
    changed.size = 22;
    changed.sha1 = Sha1::from_hex(HELLO_WORLD_SHA1_HEX).expect("a hex SHA-1");
    store.record(&changed).expect("replace the row");

    let read = store
        .item("hello.txt")
        .expect("read the item")
        .expect("the row exists");
    assert_eq!(read, changed, "the later result is the current one");
}

#[test]
fn a_failed_row_keeps_its_error_class() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    let mut failed = record("hello.bin", ItemState::FailedPermanent);
    failed.asset_id = None;
    failed.error_class = Some(FailureClass::UnsupportedFormat);

    store.record(&failed).expect("write the failure");

    let read = store.item("hello.bin").expect("read").expect("the row");
    assert_eq!(read.state, ItemState::FailedPermanent);
    assert_eq!(read.error_class, Some(FailureClass::UnsupportedFormat));
    assert_eq!(read.asset_id, None);
}

#[test]
fn opening_a_file_that_is_not_a_database_fails_the_startup_check() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("state.db");
    std::fs::write(&path, b"this is not a SQLite database at all").expect("write garbage");

    let error = StateStore::open(&path).expect_err("a corrupt file must not open");

    let message = error.to_string();
    assert!(
        message.contains("quick_check"),
        "the startup integrity check is what refused it, so it ran: {message}"
    );
}

#[test]
fn opening_twice_applies_no_migration_twice() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("state.db");
    let store = StateStore::open(&path).expect("open a fresh store");
    store
        .record(&record("hello.txt", ItemState::Uploaded))
        .expect("write a row");
    drop(store);

    let reopened = StateStore::open(&path).expect("reopen the store");
    let read = reopened.item("hello.txt").expect("read").expect("the row");
    assert_eq!(read.state, ItemState::Uploaded, "the row survived");
}

#[test]
fn opening_creates_the_local_data_directory() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("nested").join("data").join("state.db");

    let store = StateStore::open(&path).expect("open under a missing directory");

    assert!(Path::new(&path).exists(), "the database file was created");
    drop(store);
}
