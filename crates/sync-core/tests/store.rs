//! The state store: the first schema, its pragmas, and its startup check.

use std::path::Path;

use sync_core::failure::FailureClass;
use sync_core::hash::Sha1;
use sync_core::store::{
    AlbumScope, AttemptOutcome, AttemptRecord, FolderCounts, ItemRecord, ItemState, NewVersion,
    ReviewDecision, SidecarDigest, SourceFolder, StateStore, StoreError, TagBackfill,
};

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

/// The startup check must wait out a transient lock instead of reporting it as
/// corruption: a rebuild that trusted that verdict would delete a healthy
/// database (N4, R10.4). The store pins its timeout before the check runs.
#[test]
fn the_startup_check_waits_out_a_transient_lock() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("state.db");
    {
        // A valid database in the default journal mode, so the blocking
        // connection below takes a real lock on it.
        let connection = rusqlite::Connection::open(&path).expect("create the database");
        connection
            .execute_batch("CREATE TABLE marker (value INTEGER)")
            .expect("create a table");
    }
    let blocker = rusqlite::Connection::open(&path).expect("open the blocking connection");
    blocker
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("take the exclusive lock");
    let released = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(blocker);
    });

    let store = StateStore::open(&path)
        .expect("the startup check waits out the lock instead of failing the file");
    store
        .quick_check()
        .expect("the database is sound once the lock is gone");
    released.join().expect("the releasing thread finishes");
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
            album_title: None,
        })
        .expect("add the folder");
    store
        .add_folder(&SourceFolder {
            path: "/photos".to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::default(),
            album_title: None,
        })
        .expect("re-adding updates the flag");

    assert_eq!(
        store.folders().expect("read the folders"),
        vec![SourceFolder {
            path: "/photos".to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
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
        album_title: None,
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
fn inserting_a_new_folder_writes_every_setting() {
    use sync_core::store::{AlbumScope, SourceFolder};

    // #40: the setup dialog confirms all four values, and the store records
    // them in one insert.
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    let folder = SourceFolder {
        path: "/photos".to_string(),
        recursive: false,
        album_mapping: true,
        album_scope: AlbumScope::WholeFolder,
        album_title: None,
    };

    assert!(
        store.insert_folder(&folder).expect("insert the folder"),
        "a folder the store does not hold is inserted"
    );
    assert_eq!(
        store.folders().expect("read the folders"),
        vec![folder],
        "#40: the insert keeps the settings the user confirmed"
    );
}

#[test]
fn inserting_a_tracked_folder_changes_nothing() {
    use sync_core::store::{AlbumScope, SourceFolder};

    // #40: the setup dialog must not overwrite a folder the client already
    // tracks, even when the dialog carries different settings.
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    let original = SourceFolder {
        path: "/photos".to_string(),
        recursive: false,
        album_mapping: true,
        album_scope: AlbumScope::WholeFolder,
        album_title: None,
    };
    store
        .add_folder(&original)
        .expect("seed the tracked folder");

    let changed = SourceFolder {
        recursive: true,
        album_mapping: false,
        album_scope: AlbumScope::Leaf,
        album_title: None,
        ..original.clone()
    };
    assert!(
        !store
            .insert_folder(&changed)
            .expect("the insert is refused"),
        "#40: an already-tracked folder reports that it was not inserted"
    );
    assert_eq!(
        store.folders().expect("read the folders"),
        vec![original],
        "#40: the stored settings stay untouched"
    );
}

#[test]
fn inserting_a_tracked_folder_keeps_its_album_title() {
    use sync_core::store::{Album, AlbumScope, SourceFolder};

    // #47: the setup dialog can carry a title the tracked folder does not hold,
    // and a refused insert must not queue a rename for it either.
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    let original = SourceFolder {
        path: "/photos".to_string(),
        recursive: true,
        album_mapping: true,
        album_scope: AlbumScope::WholeFolder,
        album_title: Some("Holiday 2024".to_string()),
    };
    store
        .add_folder(&original)
        .expect("seed the tracked folder");
    store
        .record_album(
            "/photos",
            "album-7",
            Some("Holiday 2024"),
            Some("Holiday 2024"),
        )
        .expect("record the album the server made");

    let changed = SourceFolder {
        recursive: false,
        album_title: Some("Another title".to_string()),
        ..original.clone()
    };
    assert!(
        !store
            .insert_folder(&changed)
            .expect("the insert is refused"),
        "#40: an already-tracked folder reports that it was not inserted"
    );
    assert_eq!(
        store.folders().expect("read the folders"),
        vec![original],
        "#40: the stored settings stay untouched"
    );
    assert_eq!(
        store.album("/photos").expect("read the album row"),
        Some(Album {
            title: Some("Holiday 2024".to_string()),
            album_id: Some("album-7".to_string()),
            rename_pending: false,
            rename_problem: None,
        }),
        "#47: a refused insert leaves the title, and its pending rename, alone"
    );
}

#[test]
fn a_recorded_album_id_survives_a_restart() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("state.db");
    {
        let store = StateStore::open(&path).expect("open a fresh store");
        store
            .record_album("/photos/2023/Holiday", "album-7", None, None)
            .expect("record the album the server assigned");
    }

    // A restart must reuse the album, or every run creates a second album for
    // the same folder.
    let store = StateStore::open(&path).expect("reopen the store");
    assert_eq!(
        store
            .album("/photos/2023/Holiday")
            .expect("read the album")
            .and_then(|album| album.album_id),
        Some("album-7".to_string())
    );
    assert_eq!(
        store
            .album("/photos/2024/Holiday")
            .expect("read an unknown folder"),
        None
    );
    store
        .record_album("/photos/2023/Holiday", "album-8", None, None)
        .expect("the server replaced the album");
    assert_eq!(
        store
            .album("/photos/2023/Holiday")
            .expect("read the album")
            .and_then(|album| album.album_id),
        Some("album-8".to_string()),
        "the last id the server gave wins"
    );
}

#[test]
fn two_folders_with_the_same_name_keep_two_album_ids() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    // R3.4: the album belongs to the folder, so the same name twice must not
    // merge two folders into one album.
    store
        .record_album("/photos/2023/Holiday", "album-1", None, None)
        .expect("record the first folder's album");
    store
        .record_album("/photos/2024/Holiday", "album-2", None, None)
        .expect("record the second folder's album");

    assert_eq!(
        store
            .album("/photos/2023/Holiday")
            .expect("read the first album")
            .and_then(|album| album.album_id),
        Some("album-1".to_string())
    );
    assert_eq!(
        store
            .album("/photos/2024/Holiday")
            .expect("read the second album")
            .and_then(|album| album.album_id),
        Some("album-2".to_string()),
        "one album per folder, whatever the folder names agree on"
    );
}

/// #47: the title is stored with the folder's album row even before an album
/// exists, and it reads back through the folder.
#[test]
fn an_album_title_is_stored_before_the_album_exists() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    // Album mapping is off: no album was ever created for the folder.
    store
        .add_folder(&SourceFolder {
            path: "/photos".to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");
    store
        .set_album_title("/photos", Some("Holiday 2024"))
        .expect("store the title");

    assert_eq!(
        store.folders().expect("read the folders")[0].album_title,
        Some("Holiday 2024".to_string()),
        "#47: the folder carries the title it was given"
    );
    assert_eq!(
        store.album("/photos").expect("read the album"),
        Some(sync_core::store::Album {
            title: Some("Holiday 2024".to_string()),
            album_id: None,
            rename_pending: false,
            rename_problem: None,
        }),
        "#47: no album exists yet, so nothing is pending; the create takes the title"
    );

    // A write of the folder row that carries the stored title (a recursive
    // toggle, say) must not queue a rename.
    store
        .add_folder(&SourceFolder {
            path: "/photos".to_string(),
            recursive: false,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: Some("Holiday 2024".to_string()),
        })
        .expect("toggle the folder");
    assert!(
        !store
            .album("/photos")
            .expect("read the album")
            .expect("the album row")
            .rename_pending,
        "#47: an unchanged title owes the server nothing"
    );
}

/// #47: a title the server does not hold yet is pending, and the reason the
/// last attempt failed is kept until a later attempt lands.
#[test]
fn a_title_change_for_an_existing_album_is_pending_until_the_server_answers() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .record_album("/photos", "album-1", None, None)
        .expect("record the album");

    store
        .set_album_title("/photos", Some("Holiday"))
        .expect("store the new title");
    assert_eq!(
        store.album("/photos").expect("read the album"),
        Some(sync_core::store::Album {
            title: Some("Holiday".to_string()),
            album_id: Some("album-1".to_string()),
            rename_pending: true,
            rename_problem: None,
        }),
        "#47: the album exists, so the title has to be sent"
    );

    store
        .album_rename_failed("/photos", "the server could not be reached")
        .expect("record the failure");
    let failed = store.album("/photos").expect("read").expect("the row");
    assert!(failed.rename_pending, "#47: a failure keeps the retry");
    assert_eq!(
        failed.rename_problem.as_deref(),
        Some("the server could not be reached"),
        "#47: the reason is kept for the user"
    );

    // The server answered with the name the client meant.
    store
        .record_album("/photos", "album-1", Some("Holiday"), Some("Holiday"))
        .expect("record the answer");
    let done = store.album("/photos").expect("read").expect("the row");
    assert!(!done.rename_pending, "#47: the rename is done");
    assert_eq!(done.rename_problem, None, "and its failure is cleared");
}

/// #47: an answer for a title the user has already replaced keeps the newer
/// title. The request was about the old one, so the row stays pending.
#[test]
fn a_late_answer_does_not_overwrite_a_newer_title() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .record_album("/photos", "album-1", None, None)
        .expect("record the album");
    store
        .set_album_title("/photos", Some("Holiday"))
        .expect("store the first title");
    store
        .set_album_title("/photos", Some("Holiday 2025"))
        .expect("the user typed again");

    // The answer for the first title lands after the second was stored.
    store
        .record_album("/photos", "album-1", Some("Holiday"), Some("Holiday"))
        .expect("record the late answer");

    let album = store.album("/photos").expect("read").expect("the row");
    assert_eq!(
        album.title.as_deref(),
        Some("Holiday 2025"),
        "#47: the newer title is not overwritten by an answer about the older one"
    );
    assert!(album.rename_pending, "#47: and it still has to be sent");
}

/// #47: clearing the title back to the folder's name is a change like any
/// other, so the album has to be renamed back.
#[test]
fn clearing_the_title_asks_the_server_for_the_folder_name() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .record_album("/photos", "album-1", Some("Holiday"), Some("Holiday"))
        .expect("record the album");

    store
        .set_album_title("/photos", None)
        .expect("clear the title");

    let album = store.album("/photos").expect("read").expect("the row");
    assert_eq!(
        album.title, None,
        "#47: a blank title means the folder name"
    );
    assert!(
        album.rename_pending,
        "#47: the server still holds the old name"
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
        ("g.jpg", ItemState::Missing),
        ("h.jpg", ItemState::ReviewKeep),
        ("i.jpg", ItemState::ReviewTrash),
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
        .folder_counts_by_owner(&[SourceFolder {
            path: folder.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
        }])
        .expect("read the counts");
    assert_eq!(counts.len(), 1, "one card per folder");
    let counts = counts[0];

    assert_eq!(
        counts.uploaded, 3,
        "a kept item still holds its server copy"
    );
    assert_eq!(counts.pending, 1);
    assert_eq!(counts.failed, 3, "a dead-letter item counts as failed");
    assert_eq!(
        counts.review, 1,
        "R5.4: only the missing item waits for a decision"
    );
}

/// The counts as one card shows them, with the fields named, so the expectation
/// reads like the card rather than like the struct (#52).
fn counts_of(pending: u32, uploaded: u32, failed: u32, review: u32) -> FolderCounts {
    FolderCounts {
        pending,
        uploaded,
        failed,
        review,
    }
}

/// R8.1, #52: a row belongs to the first source folder that contains it, the
/// same owner the engine meters its upload by, so a nested source folder never
/// counts a row the outer folder already counts.
#[test]
fn nested_folders_each_own_their_rows_once() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    let outer = temp.path().join("photos");
    let inner = outer.join("2024");
    for root in [&outer, &inner] {
        std::fs::create_dir_all(root).expect("create the source folder");
        store
            .add_folder(&SourceFolder {
                path: root.display().to_string(),
                recursive: true,
                album_mapping: false,
                album_scope: AlbumScope::Leaf,
                album_title: None,
            })
            .expect("add the source folder");
    }
    for (file, state) in [
        (inner.join("a.jpg"), ItemState::Uploaded),
        (inner.join("b.jpg"), ItemState::Pending),
        (inner.join("c.jpg"), ItemState::FailedPermanent),
        (inner.join("d.jpg"), ItemState::Missing),
        (outer.join("e.jpg"), ItemState::Duplicate),
        (outer.join("f.jpg"), ItemState::RetryWait),
        // A sibling that shares the outer folder's name prefix but is not
        // inside it: no folder owns it, and no card counts it.
        (
            temp.path().join("photos-extra").join("g.jpg"),
            ItemState::Uploaded,
        ),
    ] {
        store
            .record(&record(&file.display().to_string(), state))
            .expect("write the row");
    }

    // The stored order is the path order, so the outer folder comes first and
    // owns every row under it, the nested folder's included.
    let folders = store.folders().expect("read the folders");
    assert_eq!(folders.len(), 2, "both source folders are stored");
    let counts = store
        .folder_counts_by_owner(&folders)
        .expect("read the counts");
    assert_eq!(
        counts[0],
        counts_of(1, 2, 2, 1),
        "the outer folder owns the rows under the nested folder too"
    );
    assert_eq!(
        counts[1],
        FolderCounts::default(),
        "#52: a nested folder owns nothing the earlier folder already contains"
    );
    let mut total = FolderCounts::default();
    for count in &counts {
        total.pending += count.pending;
        total.uploaded += count.uploaded;
        total.failed += count.failed;
        total.review += count.review;
    }
    assert_eq!(
        total,
        counts_of(1, 2, 2, 1),
        "the cards sum to the rows that belong to a folder, each counted once"
    );

    // The engine's live list is in the order the folders were added, so the
    // first entry in that order owns, whatever the paths say.
    let reversed: Vec<SourceFolder> = folders.iter().rev().cloned().collect();
    let counts = store
        .folder_counts_by_owner(&reversed)
        .expect("read the counts");
    assert_eq!(
        counts[0],
        counts_of(1, 1, 1, 1),
        "the nested folder owns its own rows when it comes first"
    );
    assert_eq!(
        counts[1],
        counts_of(0, 1, 1, 0),
        "the outer folder keeps only the rows the nested one does not own"
    );
}

/// R5.1: the review states are the SPEC §5 names, and a second discovery pass
/// leaves one row for one deleted file.
#[test]
fn a_missing_file_is_marked_once_and_listed_for_review() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .record(&record("photos/IMG_0001.jpg", ItemState::Uploaded))
        .expect("write the uploaded row");

    assert!(
        store
            .mark_missing("photos/IMG_0001.jpg")
            .expect("mark the item missing"),
        "R5.1: a tracked path that disappeared becomes a missing file"
    );
    assert!(
        !store
            .mark_missing("photos/IMG_0001.jpg")
            .expect("mark it again"),
        "a second discovery pass must not add a second review item"
    );

    let items = store.review_items().expect("read the review");
    assert_eq!(items.len(), 1, "R5.1: exactly one review item per file");
    assert_eq!(items[0].path, "photos/IMG_0001.jpg");
    assert_eq!(items[0].size, 11);
    assert_eq!(items[0].asset_id.as_deref(), Some("asset-1"));
    assert_eq!(items[0].state, ItemState::Missing);
    assert_eq!(store.review_count().expect("count the review"), 1);
    assert_eq!(
        store
            .item("photos/IMG_0001.jpg")
            .expect("read the item")
            .expect("the row exists")
            .state,
        ItemState::Missing,
        "the item is in the missing state the spec names"
    );
}

/// R5.1: only an item the server holds can be reviewed. A row whose upload
/// never landed has nothing to keep or trash.
#[test]
fn a_row_without_a_server_copy_is_never_a_review_item() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    for (name, state) in [
        ("pending.jpg", ItemState::Pending),
        ("uploading.jpg", ItemState::Uploading),
        ("retry.jpg", ItemState::RetryWait),
        ("failed.jpg", ItemState::FailedPermanent),
        ("parked.jpg", ItemState::DeadLetter),
    ] {
        let mut row = record(name, state);
        row.asset_id = None;
        store.record(&row).expect("write the row");
        assert!(
            !store.mark_missing(name).expect("try to mark it missing"),
            "{name}: {state:?} has no server copy to review"
        );
        assert_eq!(
            store.item(name).expect("read it").expect("the row").state,
            state,
            "a refused mark leaves the row where it was"
        );
    }
    assert_eq!(store.review_count().expect("count the review"), 0);
}

/// R5.3: both actions clear the item from the review, and only a missing item
/// can be decided on.
#[test]
fn keep_and_trash_clear_the_review_item() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    for name in ["keep.jpg", "trash.jpg"] {
        store
            .record(&record(name, ItemState::Uploaded))
            .expect("write the row");
        store.mark_missing(name).expect("mark it missing");
    }

    assert!(
        store
            .decide_review("keep.jpg", ReviewDecision::Keep)
            .expect("keep the asset"),
        "a missing item can be kept"
    );
    assert!(
        store
            .decide_review("trash.jpg", ReviewDecision::Trash)
            .expect("trash the asset"),
        "a missing item can be trashed"
    );

    assert_eq!(
        store
            .item("keep.jpg")
            .expect("read it")
            .expect("the row")
            .state,
        ItemState::ReviewKeep,
        "R5.3: Keep records the SPEC §5 review_keep state"
    );
    assert_eq!(
        store
            .item("trash.jpg")
            .expect("read it")
            .expect("the row")
            .state,
        ItemState::ReviewTrash,
        "R5.3: Trash records the SPEC §5 review_trash state"
    );
    assert_eq!(store.review_count().expect("count the review"), 0);
    assert!(
        !store
            .decide_review("keep.jpg", ReviewDecision::Keep)
            .expect("decide again"),
        "a decided item is no longer in the review"
    );
}

/// R5.3: a trash report only rewrites the Keep that landed while the DELETE
/// was in flight. A row the file recreated mid-request is live, so stamping
/// it `review_trash` would count it nowhere and freeze it settled.
#[test]
fn record_review_trash_rewrites_only_a_kept_row() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    for (name, state) in [
        ("kept.jpg", ItemState::ReviewKeep),
        ("uploaded.jpg", ItemState::Uploaded),
        ("duplicate.jpg", ItemState::Duplicate),
    ] {
        store.record(&record(name, state)).expect("write the row");
    }

    assert!(
        store
            .record_review_trash("kept.jpg")
            .expect("record the trash"),
        "a Keep that landed while the DELETE was in flight becomes the trash"
    );
    assert_eq!(
        store
            .item("kept.jpg")
            .expect("read the row")
            .expect("the row exists")
            .state,
        ItemState::ReviewTrash,
        "R5.3: the server's answer is the last word"
    );

    for (name, state) in [
        ("uploaded.jpg", ItemState::Uploaded),
        ("duplicate.jpg", ItemState::Duplicate),
    ] {
        assert!(
            !store
                .record_review_trash(name)
                .expect("try to record the trash"),
            "{name}: a live row is not the in-flight Keep's row"
        );
        assert_eq!(
            store
                .item(name)
                .expect("read the row")
                .expect("the row exists")
                .state,
            state,
            "{name}: the recreated row keeps its state"
        );
    }

    assert!(
        !store
            .record_review_trash("kept.jpg")
            .expect("record the trash again"),
        "a second report of the same trash changes nothing"
    );
}

/// R5.2 and the second-copy acceptance line: an item whose content another
/// tracked row still holds leaves the review without a user decision.
#[test]
fn a_content_twin_retires_an_item_and_the_twin_is_findable() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    let folder = temp.path().join("photos");
    for name in ["a.jpg", "b.jpg"] {
        store
            .record(&record(
                &folder.join(name).display().to_string(),
                ItemState::Uploaded,
            ))
            .expect("write the row");
    }
    let a = folder.join("a.jpg").display().to_string();
    let b = folder.join("b.jpg").display().to_string();

    store.mark_missing(&a).expect("mark a.jpg missing");
    let twins = store
        .content_twins(hello_world_sha1(), 11, &a)
        .expect("find the other copy");
    assert_eq!(twins.len(), 1);
    assert_eq!(twins[0].path, b);
    assert_eq!(
        store
            .review_candidates(&folder.display().to_string())
            .expect("list the candidates")
            .len(),
        2,
        "both the settled row and the missing one are reconciled"
    );

    assert!(store.retire_item(&a).expect("retire a.jpg"));
    assert_eq!(
        store.item(&a).expect("read it").expect("the row").state,
        ItemState::ReviewKeep,
        "the asset is kept, and no decision was needed"
    );
    assert_eq!(store.review_count().expect("count the review"), 0);
    assert!(
        !store.retire_item(&a).expect("retire again"),
        "a retired item is not in the review any more"
    );
}

/// R5.3: a row that is not waiting for a decision is not disturbed by one.
#[test]
fn a_decision_does_not_disturb_a_row_that_is_not_in_review() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .record(&record("IMG.jpg", ItemState::Uploaded))
        .expect("write the row");

    assert!(
        !store
            .decide_review("IMG.jpg", ReviewDecision::Trash)
            .expect("decide an item that is not missing"),
        "only a missing item is in the review"
    );
    assert_eq!(
        store
            .item("IMG.jpg")
            .expect("read it")
            .expect("the row")
            .state,
        ItemState::Uploaded,
        "an uploaded item stays uploaded"
    );
}

/// R5.2: a rename moves the item's row, so the asset mapping and the version
/// chain follow the file to its new path.
#[test]
fn a_rename_moves_the_row_and_keeps_its_asset_and_its_versions() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .record(&record("photos/old.jpg", ItemState::Uploaded))
        .expect("write the row");
    store
        .record_version(&NewVersion {
            path: "photos/old.jpg".to_string(),
            asset_id: "asset-1".to_string(),
            sha1: hello_world_sha1(),
            created_at_ns: 1_700_000_000_000_000_000,
            stack_id: Some("stack-1".to_string()),
        })
        .expect("seed the version chain");

    assert!(
        store
            .move_item(
                "photos/old.jpg",
                "photos/new.jpg",
                11,
                1_700_000_000_000_000_001
            )
            .expect("move the row"),
        "an untracked destination accepts the move"
    );

    assert!(
        store
            .item("photos/old.jpg")
            .expect("read the old")
            .is_none(),
        "the old path is no longer tracked"
    );
    let moved = store
        .item("photos/new.jpg")
        .expect("read the new")
        .expect("the row moved");
    assert_eq!(
        moved.asset_id.as_deref(),
        Some("asset-1"),
        "the mapping stayed"
    );
    assert_eq!(moved.state, ItemState::Uploaded);
    assert_eq!(moved.size, 11);
    assert_eq!(moved.mtime_ns, 1_700_000_000_000_000_001);
    let versions = store.versions("photos/new.jpg").expect("read the chain");
    assert_eq!(
        versions.len(),
        1,
        "R5.2: the version chain follows the item"
    );
    assert!(versions[0].is_current);
    assert_eq!(
        store.stack_id("photos/new.jpg").expect("read the stack"),
        Some("stack-1".to_string())
    );

    assert!(
        !store
            .move_item("photos/old.jpg", "photos/ghost.jpg", 11, 1)
            .expect("move an untracked source"),
        "an untracked source has no row to move"
    );
    store
        .record(&record("photos/other.jpg", ItemState::Uploaded))
        .expect("write a second row");
    assert!(
        !store
            .move_item("photos/new.jpg", "photos/other.jpg", 11, 1)
            .expect("move onto a tracked path"),
        "a move never overwrites another item's row"
    );
    assert_eq!(store.review_count().expect("count the review"), 0);
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

/// The diagnostics read (R10.2): the schema version, the size, the per-table
/// counts, and the integrity check, read without changing anything.
#[test]
fn the_sqlite_info_reports_the_schema_the_size_and_the_table_counts() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");

    let empty = store.sqlite_info().expect("read the sqlite info");
    assert_eq!(
        empty.migration_version, 12,
        "the schema stands at the twelfth migration (#47 added the album title)"
    );
    assert!(empty.size_bytes > 0, "a migrated database occupies pages");
    assert_eq!(empty.integrity, "ok", "a fresh database is sound");
    assert_eq!(
        table_rows(&empty, "sync_item"),
        0,
        "a fresh database holds no items"
    );
    for table in ["sync_item", "source_folder", "album", "attempt", "version"] {
        assert!(
            empty.tables.iter().any(|count| count.table == table),
            "the catalog names {table}: {:?}",
            empty.tables
        );
    }

    store
        .record(&record("hello.txt", ItemState::Uploaded))
        .expect("write the item");
    store
        .add_folder(&SourceFolder {
            path: temp.path().join("photos").display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the folder");

    let filled = store.sqlite_info().expect("read the sqlite info again");
    assert_eq!(table_rows(&filled, "sync_item"), 1);
    assert_eq!(table_rows(&filled, "source_folder"), 1);
    assert!(
        filled.size_bytes >= empty.size_bytes,
        "the size does not shrink as rows arrive: {} then {}",
        empty.size_bytes,
        filled.size_bytes
    );
}

/// The per-table row count for one table, which must be counted.
fn table_rows(info: &sync_core::store::SqliteInfo, table: &str) -> i64 {
    info.tables
        .iter()
        .find(|count| count.table == table)
        .unwrap_or_else(|| panic!("the catalog holds {table}: {:?}", info.tables))
        .rows
}

/// Issue #25: the debug bundle's sizing reports the library's bytes and the
/// shape of its state in one read.
#[test]
fn the_sizing_reports_the_tracked_bytes_and_the_state_counts() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .add_folder(&SourceFolder {
            path: temp.path().join("photos").display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");

    for (path, state, size) in [
        ("uploaded.jpg", ItemState::Uploaded, 100),
        ("duplicate.jpg", ItemState::Duplicate, 50),
        ("parked.jpg", ItemState::DeadLetter, 10),
        ("gone.jpg", ItemState::Missing, 5),
    ] {
        let mut row = record(path, state);
        row.size = size;
        store.record(&row).expect("write the item");
    }
    for asset_id in ["asset-1", "asset-2"] {
        store
            .record_version(&NewVersion {
                path: "uploaded.jpg".to_string(),
                asset_id: asset_id.to_string(),
                sha1: hello_world_sha1(),
                created_at_ns: 1_700_000_000_000_000_000,
                stack_id: None,
            })
            .expect("record the version");
    }

    let sizing = store.sizing().expect("read the sizing");

    assert_eq!(
        sizing.tracked_bytes, 165,
        "the sum of the four recorded sizes"
    );
    assert_eq!(
        sizing.by_state,
        vec![
            (ItemState::DeadLetter, 1),
            (ItemState::Duplicate, 1),
            (ItemState::Missing, 1),
            (ItemState::Uploaded, 1),
        ],
        "one entry per stored state, ordered by the stored name"
    );
    assert_eq!(sizing.folders, 1, "the one source folder is counted");
    assert_eq!(sizing.versions, 2, "both recorded versions are counted");
}

/// The most recent failure of each class the attempt log holds (R10.2).
#[test]
fn the_last_error_of_each_class_is_reported_with_its_message() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");

    let mut unsupported = record("raw.cr3", ItemState::FailedPermanent);
    unsupported.error_class = Some(FailureClass::UnsupportedFormat);
    unsupported.error_message = Some("the server does not accept this file format".to_string());
    store.record(&unsupported).expect("write the item");
    store
        .record_attempt(&AttemptRecord {
            path: unsupported.path.clone(),
            started_ns: 100,
            outcome: AttemptOutcome::Failed,
            error_class: Some(FailureClass::UnsupportedFormat),
        })
        .expect("record the attempt");

    let mut transient = record("large.bin", ItemState::DeadLetter);
    transient.error_class = Some(FailureClass::ServerTransient);
    transient.error_message = Some("the server answered 503".to_string());
    store.record(&transient).expect("write the item");
    store
        .record_attempt(&AttemptRecord {
            path: transient.path.clone(),
            started_ns: 200,
            outcome: AttemptOutcome::Failed,
            error_class: Some(FailureClass::ServerTransient),
        })
        .expect("record the attempt");

    let errors = store
        .last_error_per_class()
        .expect("read the last errors by class");

    assert_eq!(
        errors.iter().map(|error| error.class).collect::<Vec<_>>(),
        vec![
            FailureClass::ServerTransient,
            FailureClass::UnsupportedFormat
        ],
        "newest first, one entry per class"
    );
    assert_eq!(
        errors[0].message.as_deref(),
        Some("the server answered 503")
    );
    assert_eq!(
        errors[1].message.as_deref(),
        Some("the server does not accept this file format")
    );
}

/// A rebuild starts the state tables over (R10.4): the item, attempt, album,
/// and version rows go, and the folder list is re-added by the caller that
/// knows the user's choices.
#[test]
fn a_reset_empties_every_state_table_and_leaves_the_database_sound() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    store
        .record(&record("hello.txt", ItemState::Uploaded))
        .expect("write the item");
    store
        .record_attempt(&AttemptRecord {
            path: "hello.txt".to_string(),
            started_ns: 1,
            outcome: AttemptOutcome::Succeeded,
            error_class: None,
        })
        .expect("record the attempt");
    store
        .add_folder(&SourceFolder {
            path: temp.path().join("photos").display().to_string(),
            recursive: true,
            album_mapping: true,
            album_scope: AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the folder");

    store.reset().expect("reset the state tables");

    let info = store.sqlite_info().expect("read the sqlite info");
    assert_eq!(info.integrity, "ok", "the reset database is sound");
    for table in ["sync_item", "source_folder", "album", "attempt", "version"] {
        assert_eq!(table_rows(&info, table), 0, "{table} starts over");
    }
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

/// R3.7: the backfill enumerates every asset the client ever uploaded, once
/// each and in id order, so a cursor can resume the run.
#[test]
fn the_tag_backfill_enumerates_every_uploaded_asset_once() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    // An edited file: the chain holds both assets and the item points at the
    // newest, so the old version is enumerated and the current one only once.
    store
        .record(&record("edited.jpg", ItemState::Uploaded))
        .expect("write the row");
    for (asset, at) in [("asset-b", 1), ("asset-c", 2)] {
        store
            .record_version(&NewVersion {
                path: "edited.jpg".to_string(),
                asset_id: asset.to_string(),
                sha1: hello_world_sha1(),
                created_at_ns: at,
                stack_id: None,
            })
            .expect("seed the version chain");
    }
    // A duplicate: the row holds the asset the server already had.
    store
        .record(&ItemRecord {
            asset_id: Some("asset-d".to_string()),
            state: ItemState::Duplicate,
            ..record("duplicate.jpg", ItemState::Duplicate)
        })
        .expect("write the duplicate row");
    // A row no upload settled contributes no id.
    store
        .record(&ItemRecord {
            asset_id: None,
            state: ItemState::Pending,
            ..record("pending.jpg", ItemState::Pending)
        })
        .expect("write the pending row");

    assert_eq!(
        store.tag_backfill_total().expect("count the library"),
        3,
        "R3.7: the current asset, the old version, and the duplicate, once each"
    );
    assert_eq!(
        store
            .tag_backfill_batch(None, 2)
            .expect("read the first batch"),
        vec!["asset-b".to_string(), "asset-c".to_string()],
        "R3.7: a batch is the ids after the cursor, in id order, without a repeat"
    );
    assert_eq!(
        store
            .tag_backfill_batch(Some("asset-c"), 2)
            .expect("read the next batch"),
        vec!["asset-d".to_string()],
        "the cursor continues after the last id of the previous batch"
    );
    assert!(
        store
            .tag_backfill_batch(Some("asset-d"), 2)
            .expect("read past the end")
            .is_empty(),
        "past the last id the batch is empty, which is how the run finishes"
    );
}

/// R3.7: the backfill records the last completed batch and its completion, so
/// a restart resumes instead of starting over and "tag now" clears it.
#[test]
fn the_tag_backfill_marker_survives_a_reopen_and_the_restart_clears_it() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("state.db");
    {
        let store = StateStore::open(&path).expect("open a fresh store");
        assert_eq!(
            store.tag_backfill().expect("read the state"),
            TagBackfill::default(),
            "a library nothing was tagged in has no marker and no cursor"
        );
        store
            .record_tag_batch("asset-c", 2)
            .expect("record a completed batch");
    }

    let store = StateStore::open(&path).expect("reopen the store");
    assert_eq!(
        store.tag_backfill().expect("read the state"),
        TagBackfill {
            completed_ns: None,
            cursor: Some("asset-c".to_string()),
            tagged: 2,
        },
        "R3.7: the last completed batch survives a restart"
    );
    store
        .finish_tag_backfill(42)
        .expect("record the completion");
    assert_eq!(
        store.tag_backfill().expect("read the state").completed_ns,
        Some(42),
        "R3.7: a finished backfill is recorded so a restart does not repeat it"
    );

    store
        .restart_tag_backfill()
        .expect("clear the backfill state");
    assert_eq!(
        store.tag_backfill().expect("read the state"),
        TagBackfill::default(),
        "R3.7: \"tag existing assets now\" clears the marker, the cursor and the count"
    );
    assert_eq!(
        store.tag_backfill_total().expect("count the library"),
        0,
        "the total counts the assets the state holds"
    );
}

/// Writes one uploaded item with one current version, the shape the folder
/// detail lists (#44).
fn seed_asset(store: &StateStore, path: &str, asset_id: &str, size: u64, created_at_ns: i64) {
    let mut row = record(path, ItemState::Uploaded);
    row.size = size;
    row.asset_id = Some(asset_id.to_string());
    store.record(&row).expect("write the item");
    store
        .record_version(&NewVersion {
            path: path.to_string(),
            asset_id: asset_id.to_string(),
            sha1: hello_world_sha1(),
            created_at_ns,
            stack_id: None,
        })
        .expect("record the version");
}

/// #44: the folder detail's page is one row per current asset of the folder's
/// own tree, newest first. An edited file appears once, with its latest asset;
/// an older version stays in the chain and is not listed.
#[test]
fn the_folder_asset_page_lists_one_current_asset_per_item_newest_first() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    let photos = temp.path().join("photos");
    let folder = photos.display().to_string();
    let at = |index: i64| 1_700_000_000_000_000_000 + index;

    seed_asset(
        &store,
        &photos.join("a.jpg").display().to_string(),
        "asset-a",
        10,
        at(1),
    );
    seed_asset(
        &store,
        &photos.join("b.jpg").display().to_string(),
        "asset-b",
        20,
        at(2),
    );
    // Edited once: the old asset stays recorded, the new one is current.
    seed_asset(
        &store,
        &photos.join("edited.jpg").display().to_string(),
        "asset-old",
        30,
        at(3),
    );
    store
        .record_version(&NewVersion {
            path: photos.join("edited.jpg").display().to_string(),
            asset_id: "asset-new".to_string(),
            sha1: hello_world_sha1(),
            created_at_ns: at(5),
            stack_id: Some("stack-1".to_string()),
        })
        .expect("record the edited file's latest version");
    // A subfolder, which the folder's recursion setting decides on.
    seed_asset(
        &store,
        &photos.join("sub").join("c.jpg").display().to_string(),
        "asset-c",
        40,
        at(4),
    );
    // A sibling whose path merely shares the prefix must not count.
    let other = temp.path().join("photos2");
    seed_asset(
        &store,
        &other.join("d.jpg").display().to_string(),
        "asset-d",
        50,
        at(6),
    );
    // A file still in flight has no asset and no version yet.
    let mut pending = record(
        &photos.join("pending.jpg").display().to_string(),
        ItemState::Pending,
    );
    pending.asset_id = None;
    store.record(&pending).expect("write the pending row");

    let rows = store
        .folder_assets(&folder, true, 50, 0)
        .expect("read the folder's assets");
    let listed: Vec<(String, String, u64)> = rows
        .iter()
        .map(|row| (row.path.clone(), row.asset_id.clone(), row.size))
        .collect();
    assert_eq!(
        listed,
        vec![
            (
                photos.join("edited.jpg").display().to_string(),
                "asset-new".to_string(),
                30
            ),
            (
                photos.join("sub").join("c.jpg").display().to_string(),
                "asset-c".to_string(),
                40
            ),
            (
                photos.join("b.jpg").display().to_string(),
                "asset-b".to_string(),
                20
            ),
            (
                photos.join("a.jpg").display().to_string(),
                "asset-a".to_string(),
                10
            ),
        ],
        "#44: newest first, one current asset per item, the folder's tree only"
    );
    assert_eq!(
        rows[0].created_at_ns,
        at(5),
        "#44: the row carries when its current version was uploaded"
    );

    // R2.1: a folder that does not recurse lists its direct children only.
    let direct = store
        .folder_assets(&folder, false, 50, 0)
        .expect("read the folder's own files");
    assert_eq!(
        direct.len(),
        3,
        "#44: a non-recursive folder leaves its subfolders' assets out"
    );
    assert!(
        direct
            .iter()
            .all(|row| Path::new(&row.path).parent() == Some(photos.as_path())),
        "no subfolder row belongs to a non-recursive folder"
    );
}

/// #44: paging never repeats a row and never loses one, so Load more appends
/// the next page of the same order.
#[test]
fn the_folder_asset_page_pages_without_repeating_or_losing_rows() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = StateStore::open(temp.path().join("state.db")).expect("open a fresh store");
    let photos = temp.path().join("photos");
    let folder = photos.display().to_string();
    let names: Vec<String> = (0..5).map(|index| format!("{index}.jpg")).collect();
    for (index, name) in names.iter().enumerate() {
        let index = i64::try_from(index).expect("a small index");
        seed_asset(
            &store,
            &photos.join(name).display().to_string(),
            &format!("asset-{index}"),
            1,
            1_700_000_000_000_000_000 + index,
        );
    }

    let mut listed: Vec<String> = Vec::new();
    let mut offset = 0;
    loop {
        let page = store
            .folder_assets(&folder, true, 2, offset)
            .expect("read one page");
        let full = page.len() == 2;
        listed.extend(page.into_iter().map(|row| row.path));
        if !full {
            break;
        }
        offset += 2;
    }

    let expected: Vec<String> = names
        .iter()
        .rev()
        .map(|name| photos.join(name).display().to_string())
        .collect();
    assert_eq!(
        listed, expected,
        "#44: the pages are the one order, once each"
    );
}
