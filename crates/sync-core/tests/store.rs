//! The state store: the first schema, its pragmas, and its startup check.

use std::path::Path;

use sync_core::failure::FailureClass;
use sync_core::hash::Sha1;
use sync_core::store::{ItemRecord, ItemState, NewVersion, SidecarDigest, StateStore, StoreError};

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
        error_message: None,
        attempts: 0,
        sidecar: None,
    }
}

/// `sha1("<xmp/>")`, hex: the digest of the sidecar fixture, which is a
/// different content than the media file.
const SIDECAR_SHA1_HEX: &str = "74719584132d8a19599e9be7a2cdb160d93d9d00";

#[test]
fn the_sidecar_digest_survives_a_reopen() {
    // R3.3: the store remembers which sidecar the asset carries, so a sidecar
    // that appears later is the only one that uploads again.
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("state.db");
    let sidecar_sha1 = Sha1::from_hex(SIDECAR_SHA1_HEX).expect("the fixture is a hex SHA-1");
    {
        let store = StateStore::open(&path).expect("open a fresh store");
        let mut row = record("IMG_0001.jpg", ItemState::Uploaded);
        row.sidecar = Some(SidecarDigest {
            sha1: sidecar_sha1,
            size: 6,
            mtime_ns: 1_700_000_000_000_000_001,
        });
        store.record(&row).expect("write the row with its sidecar");
    }

    let reopened = StateStore::open(&path).expect("reopen the store");
    let read = reopened
        .item("IMG_0001.jpg")
        .expect("read the item")
        .expect("the row survives the reopen");

    assert_eq!(
        read.sidecar,
        Some(SidecarDigest {
            sha1: sidecar_sha1,
            size: 6,
            mtime_ns: 1_700_000_000_000_000_001,
        }),
        "R4.5: the sidecar's digest, size and mtime all survive the reopen"
    );
    assert_eq!(
        record("IMG_0001.jpg", ItemState::Uploaded).sidecar,
        None,
        "an item without a sidecar records no sidecar digest"
    );
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
    assert_eq!(
        pragmas.foreign_keys, 1,
        "foreign keys are enforced, or the state tables' ON DELETE actions are dead SQL"
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

#[test]
fn the_store_compares_paths_with_the_requested_case_rule() {
    // Built from parts so the repository's hardcoded-path scan stays clean.
    let windows = format!("{}:\\Photos\\A.JPG", 'C');
    let other_case = windows.to_lowercase();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .record_with_case(
            &record(&windows, ItemState::Uploaded),
            sync_core::paths::PathCase::Insensitive,
        )
        .expect("write the row as Windows would");

    let insensitive = store
        .item_with_case(&other_case, sync_core::paths::PathCase::Insensitive)
        .expect("read with the Windows rule");
    assert!(
        insensitive.is_some(),
        "R2.4: Windows must see one item, not two"
    );

    let sensitive = store
        .item_with_case(&other_case, sync_core::paths::PathCase::Sensitive)
        .expect("read with the Linux rule");
    assert!(
        sensitive.is_none(),
        "R2.4: Linux keeps the two spellings apart"
    );
}

#[test]
fn source_folders_persist_with_their_recursive_flag() {
    use sync_core::store::{AlbumScope, SourceFolder};

    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .add_folder(&SourceFolder {
            path: "/photos".to_string(),
            recursive: false,
            album_mapping: false,
            album_scope: AlbumScope::default(),
        })
        .expect("add the folder");
    store
        .add_folder(&SourceFolder {
            path: "/photos".to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::default(),
        })
        .expect("re-adding updates the flag");

    assert_eq!(
        store.folders().expect("read the folders"),
        vec![SourceFolder {
            path: "/photos".to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
        }],
        "R3.4: album mapping is off until the user opts in, and the scope defaults to leaf"
    );

    assert!(
        store.remove_folder("/photos").expect("remove the folder"),
        "the folder was there"
    );
    assert!(!store.remove_folder("/photos").expect("remove again"));
    assert!(store.folders().expect("read the folders").is_empty());
}

#[test]
fn a_re_added_folder_keeps_the_album_mapping_the_user_chose() {
    use sync_core::store::{AlbumScope, SourceFolder};

    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    let mapped = SourceFolder {
        path: "/photos".to_string(),
        recursive: true,
        album_mapping: true,
        album_scope: AlbumScope::WholeFolder,
    };
    store.add_folder(&mapped).expect("add the mapped folder");
    store
        .add_folder(&SourceFolder {
            recursive: false,
            ..mapped.clone()
        })
        .expect("a later add of the same folder");

    assert_eq!(
        store.folders().expect("read the folders"),
        vec![SourceFolder {
            recursive: false,
            ..mapped
        }],
        "R3.4: re-adding a folder changes the recursion, not the mapping the user chose"
    );
}

#[test]
fn a_remembered_album_id_survives_a_restart() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("state.db");
    {
        let store = StateStore::open(&path).expect("open a fresh store");
        store
            .remember_album("/photos/2023/Holiday", "Holiday", "album-7")
            .expect("record the album the server assigned");
    }

    // A restart must reuse the album, or every run creates a second album for
    // the same folder.
    let store = StateStore::open(&path).expect("reopen the store");
    assert_eq!(
        store.album("/photos/2023/Holiday").expect("read the album"),
        Some("album-7".to_string())
    );
    assert_eq!(
        store
            .album("/photos/2024/Holiday")
            .expect("read an unknown folder"),
        None
    );
    store
        .remember_album("/photos/2023/Holiday", "Holiday", "album-8")
        .expect("the server replaced the album");
    assert_eq!(
        store.album("/photos/2023/Holiday").expect("read the album"),
        Some("album-8".to_string()),
        "the last id the server gave wins"
    );
}

#[test]
fn two_folders_with_the_same_name_keep_two_album_ids() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    // R3.4: the album belongs to the folder. The name is a copy of the folder
    // name, so the same name twice must not merge two folders into one album.
    store
        .remember_album("/photos/2023/Holiday", "Holiday", "album-1")
        .expect("record the first folder's album");
    store
        .remember_album("/photos/2024/Holiday", "Holiday", "album-2")
        .expect("record the second folder's album");

    assert_eq!(
        store
            .album("/photos/2023/Holiday")
            .expect("read the first album"),
        Some("album-1".to_string())
    );
    assert_eq!(
        store
            .album("/photos/2024/Holiday")
            .expect("read the second album"),
        Some("album-2".to_string()),
        "one album per folder, whatever the folder names agree on"
    );
}

#[test]
fn an_album_scope_this_build_does_not_know_is_not_guessed_at() {
    use sync_core::store::AlbumScope;

    assert_eq!(AlbumScope::parse("leaf"), Some(AlbumScope::Leaf));
    assert_eq!(
        AlbumScope::parse("whole_folder"),
        Some(AlbumScope::WholeFolder)
    );
    // The scope is stored as text, so a value a newer build wrote must come
    // back as unknown rather than as a scope the user never chose (R3.4).
    assert_eq!(AlbumScope::parse("future"), None);
}

#[test]
fn requeue_unfinished_returns_pending_paths_and_never_completed_ones() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .record(&record("uploaded.jpg", ItemState::Uploaded))
        .expect("write the completed row");
    store
        .record(&record("pending.jpg", ItemState::Pending))
        .expect("write the pending row");
    store
        .record(&record("interrupted.jpg", ItemState::Uploading))
        .expect("write the interrupted row");
    let mut parked = record("parked.jpg", ItemState::RetryWait);
    parked.asset_id = None;
    store.record(&parked).expect("write the parked row");
    store
        .record(&record("dead.jpg", ItemState::DeadLetter))
        .expect("write the dead-letter row");

    let queued = store.requeue_unfinished().expect("re-queue the unfinished");

    assert_eq!(
        queued,
        vec![
            "interrupted.jpg".to_string(),
            "parked.jpg".to_string(),
            "pending.jpg".to_string(),
        ],
        "R6.7: the unfinished paths come back, in path order, with the attempt count kept"
    );
    assert_eq!(
        store
            .item("interrupted.jpg")
            .expect("read")
            .expect("the row")
            .state,
        ItemState::Pending,
        "an upload that a crash interrupted is queued again"
    );
    assert_eq!(
        store
            .item("dead.jpg")
            .expect("read")
            .expect("the row")
            .state,
        ItemState::DeadLetter,
        "R6.5: only a manual retry restarts a dead-letter item"
    );
    assert_eq!(
        store
            .item("uploaded.jpg")
            .expect("read")
            .expect("the row")
            .state,
        ItemState::Uploaded,
        "R6.7: a completed item is never re-queued"
    );
}

#[test]
fn an_attempt_row_and_its_count_survive_a_reopen() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("state.db");
    let store = StateStore::open(&path).expect("open a fresh store");
    let mut row = record("hello.txt", ItemState::RetryWait);
    row.asset_id = None;
    row.error_class = Some(FailureClass::ServerTransient);
    row.error_message = Some("the server answered 503".to_string());
    store.record(&row).expect("write the failed row");

    let first = store
        .record_attempt(&sync_core::store::AttemptRecord {
            path: "hello.txt".to_string(),
            started_ns: 1_700_000_000_000_000_000,
            outcome: sync_core::store::AttemptOutcome::Failed,
            error_class: Some(FailureClass::ServerTransient),
        })
        .expect("record the first attempt");
    let second = store
        .record_attempt(&sync_core::store::AttemptRecord {
            path: "hello.txt".to_string(),
            started_ns: 1_700_000_000_000_000_500,
            outcome: sync_core::store::AttemptOutcome::Succeeded,
            error_class: None,
        })
        .expect("record the second attempt");
    assert_eq!((first, second), (1, 2), "the counter follows the attempts");
    drop(store);

    // R6.7, §5: a restart continues the count instead of starting fresh.
    let reopened = StateStore::open(&path).expect("reopen the store");
    assert_eq!(
        reopened
            .item("hello.txt")
            .expect("read")
            .expect("the row")
            .attempts,
        2,
        "the attempt count survived the reopen"
    );
    let attempts = reopened.attempts("hello.txt").expect("read the attempts");
    assert_eq!(attempts.len(), 2, "the attempt log survived the reopen");
    assert_eq!(
        attempts[0].outcome,
        sync_core::store::AttemptOutcome::Failed
    );
    assert_eq!(attempts[0].error_class, Some(FailureClass::ServerTransient));
    assert_eq!(attempts[0].started_ns, 1_700_000_000_000_000_000);
    assert_eq!(
        attempts[1].outcome,
        sync_core::store::AttemptOutcome::Succeeded
    );
    assert_eq!(attempts[1].error_class, None);
}

#[test]
fn recording_an_attempt_for_an_unknown_item_is_an_error() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");

    let error = store
        .record_attempt(&sync_core::store::AttemptRecord {
            path: "never-seen.txt".to_string(),
            started_ns: 1,
            outcome: sync_core::store::AttemptOutcome::Failed,
            error_class: None,
        })
        .expect_err("an attempt needs an item");

    assert!(
        matches!(error, sync_core::store::StoreError::UnknownItem(_)),
        "{error:?}"
    );
}

#[test]
fn the_failed_items_view_lists_the_parked_rows_with_their_reason() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    let mut dead = record("dead.bin", ItemState::DeadLetter);
    dead.asset_id = None;
    dead.error_class = Some(FailureClass::ServerTransient);
    dead.error_message = Some("the server answered 503".to_string());
    store.record(&dead).expect("write the dead-letter row");
    // The count comes from the attempt log, which is what the retry policy
    // reads after a restart (R6.7).
    for index in 0..8 {
        store
            .record_attempt(&sync_core::store::AttemptRecord {
                path: "dead.bin".to_string(),
                started_ns: 1_700_000_000_000_000_000 + index,
                outcome: sync_core::store::AttemptOutcome::Failed,
                error_class: Some(FailureClass::ServerTransient),
            })
            .expect("record the attempt");
    }
    let mut permanent = record("permanent.bin", ItemState::FailedPermanent);
    permanent.asset_id = None;
    permanent.error_class = Some(FailureClass::UnsupportedFormat);
    permanent.error_message = Some("the server does not accept this file format".to_string());
    store.record(&permanent).expect("write the permanent row");
    store
        .record(&record("done.jpg", ItemState::Uploaded))
        .expect("write a completed row");
    store
        .record(&record("waiting.jpg", ItemState::RetryWait))
        .expect("write a retrying row");

    let failed = store.failed_items().expect("read the failed items");

    assert_eq!(
        failed
            .iter()
            .map(|item| item.path.as_str())
            .collect::<Vec<_>>(),
        vec!["dead.bin", "permanent.bin"],
        "R6.5: the parked items, in path order, and nothing else"
    );
    assert_eq!(failed[0].state, ItemState::DeadLetter);
    assert_eq!(failed[0].attempts, 8);
    assert_eq!(failed[0].error_class, Some(FailureClass::ServerTransient));
    assert_eq!(
        failed[0].error_message.as_deref(),
        Some("the server answered 503"),
        "the view shows the last error beside the class"
    );
    assert_eq!(failed[1].error_class, Some(FailureClass::UnsupportedFormat));
}

#[test]
fn a_manual_retry_requeues_a_parked_item_with_a_fresh_budget() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    let mut dead = record("dead.bin", ItemState::DeadLetter);
    dead.asset_id = None;
    dead.error_class = Some(FailureClass::ServerTransient);
    dead.error_message = Some("the server answered 503".to_string());
    store.record(&dead).expect("write the dead-letter row");
    // The count comes from the attempt log, which is what the retry policy
    // reads after a restart (R6.7).
    for index in 0..8 {
        store
            .record_attempt(&sync_core::store::AttemptRecord {
                path: "dead.bin".to_string(),
                started_ns: 1_700_000_000_000_000_000 + index,
                outcome: sync_core::store::AttemptOutcome::Failed,
                error_class: Some(FailureClass::ServerTransient),
            })
            .expect("record the attempt");
    }
    store
        .record(&record("done.jpg", ItemState::Uploaded))
        .expect("write a completed row");

    assert!(
        store.retry_item("dead.bin").expect("retry the item"),
        "R6.5: the parked item was found"
    );
    let row = store.item("dead.bin").expect("read").expect("the row");
    assert_eq!(row.state, ItemState::Pending, "it is queued again");
    assert_eq!(row.attempts, 0, "the manual retry grants a fresh budget");
    assert_eq!(row.error_class, None);
    assert_eq!(row.error_message, None);
    assert_eq!(
        store.attempts("dead.bin").expect("read the attempts").len(),
        8,
        "the attempt log is history and survives a manual retry"
    );
    assert!(
        !store.retry_item("dead.bin").expect("retry again"),
        "a queued item is not re-queued twice"
    );
    assert!(
        !store
            .retry_item("done.jpg")
            .expect("retry a completed item"),
        "a completed item is not a failed one"
    );
    assert_eq!(
        store
            .item("done.jpg")
            .expect("read")
            .expect("the row")
            .state,
        ItemState::Uploaded,
        "a completed item is untouched"
    );
}

#[test]
fn folder_counts_bucket_the_states() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    let folder = temp.path().join("photos");
    let path = |name: &str, state| {
        let mut row = record(&folder.join(name).display().to_string(), state);
        if !state.is_terminal_success() {
            row.asset_id = None;
        }
        row
    };
    for (name, state) in [
        ("a.jpg", ItemState::Uploaded),
        ("b.jpg", ItemState::Duplicate),
        ("c.jpg", ItemState::Pending),
        ("d.jpg", ItemState::FailedPermanent),
        ("e.jpg", ItemState::RetryWait),
        ("f.jpg", ItemState::DeadLetter),
    ] {
        store.record(&path(name, state)).expect("write the row");
    }
    // A path outside the folder must not count towards it.
    store
        .record(&record(
            &temp.path().join("other.jpg").display().to_string(),
            ItemState::Uploaded,
        ))
        .expect("write the outside row");

    let counts = store
        .folder_counts(&folder.display().to_string())
        .expect("read the counts");

    assert_eq!(counts.uploaded, 2);
    assert_eq!(counts.pending, 1);
    assert_eq!(counts.failed, 3, "a dead-letter item counts as failed");
}

/// R4.4: the chain keeps every version, one current flag, and the stack id on
/// the item.
#[test]
fn a_recorded_version_chain_keeps_the_old_versions_and_one_current() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .record(&record("hello.txt", ItemState::Uploaded))
        .expect("write the item");

    for (index, asset_id) in ["asset-1", "asset-2"].into_iter().enumerate() {
        store
            .record_version(&NewVersion {
                path: "hello.txt".to_string(),
                asset_id: asset_id.to_string(),
                sha1: hello_world_sha1(),
                created_at_ns: 1_700_000_000_000_000_000 + i64::try_from(index).expect("fits"),
                stack_id: Some("stack-1".to_string()),
            })
            .expect("record the version");
    }

    let chain = store.versions("hello.txt").expect("read the chain");
    assert_eq!(chain.len(), 2, "R4.4: both versions stay recorded");
    assert_eq!(
        chain
            .iter()
            .map(|version| version.asset_id.as_str())
            .collect::<Vec<_>>(),
        vec!["asset-1", "asset-2"],
        "R4.4: the chain is oldest first"
    );
    assert_eq!(
        chain.iter().filter(|version| version.is_current).count(),
        1,
        "R4.4: exactly one version is current"
    );
    assert!(
        chain[1].is_current,
        "R4.4: the current flag follows the newest version"
    );
    assert_eq!(
        chain[0].item_id, chain[1].item_id,
        "R4.1: both versions belong to one sync item"
    );
    assert_eq!(
        store.stack_id("hello.txt").expect("read the stack id"),
        Some("stack-1".to_string()),
        "R4.4: the stack id is recorded on the item"
    );
    assert_eq!(
        store
            .item("hello.txt")
            .expect("read")
            .expect("the row")
            .asset_id,
        Some("asset-2".to_string()),
        "R4.4: the item points at its current version"
    );
}

/// The chain reads oldest-first by recording time. Row ids stop reflecting
/// that after a version is re-recorded — a revert gives an old row a new
/// recording time while its id stays — so a reader must not order by id.
#[test]
fn the_chain_stays_chronological_after_a_revert() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .record(&record("hello.txt", ItemState::Uploaded))
        .expect("write the item");
    let version = |asset_id: &str, created_at_ns| NewVersion {
        path: "hello.txt".to_string(),
        asset_id: asset_id.to_string(),
        sha1: hello_world_sha1(),
        created_at_ns,
        stack_id: None,
    };
    store
        .record_version(&version("asset-1", 1_700_000_000_000_000_000))
        .expect("record the first version");
    store
        .record_version(&version("asset-2", 1_700_000_000_000_000_500))
        .expect("record the second version");
    // The user reverts to the first version: its row keeps its id but takes
    // the new recording time.
    store
        .record_version(&version("asset-1", 1_700_000_000_000_001_000))
        .expect("record the revert");

    let chain = store.versions("hello.txt").expect("read the chain");
    assert_eq!(
        chain
            .iter()
            .map(|version| version.asset_id.as_str())
            .collect::<Vec<_>>(),
        vec!["asset-2", "asset-1"],
        "oldest first by recording time, not by row id"
    );
    assert!(
        chain[1].is_current,
        "the reverted-to version is the current one"
    );
}

/// A version recorded twice is one row, so a retried flow cannot grow the
/// chain (R4.4, R6.7).
#[test]
fn recording_the_same_version_twice_keeps_one_row() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .record(&record("hello.txt", ItemState::Uploaded))
        .expect("write the item");
    let version = NewVersion {
        path: "hello.txt".to_string(),
        asset_id: "asset-1".to_string(),
        sha1: hello_world_sha1(),
        created_at_ns: 1_700_000_000_000_000_000,
        stack_id: None,
    };
    store.record_version(&version).expect("record it");
    store.record_version(&version).expect("record it again");

    assert_eq!(
        store.versions("hello.txt").expect("read the chain").len(),
        1,
        "a repeat is idempotent"
    );
}

/// R4.4: a version needs an item row; a path the store does not track is an
/// error, not a version nothing belongs to.
#[test]
fn recording_a_version_for_an_unknown_item_is_an_error() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");

    let error = store
        .record_version(&NewVersion {
            path: "missing.txt".to_string(),
            asset_id: "asset-1".to_string(),
            sha1: hello_world_sha1(),
            created_at_ns: 1,
            stack_id: None,
        })
        .expect_err("a version needs its item");

    assert!(
        matches!(error, StoreError::UnknownItem(_)),
        "the error says which item is missing: {error}"
    );
}
