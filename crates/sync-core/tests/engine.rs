//! The folder sync engine against the offline Immich double (SPEC R2.1–R2.5,
//! R3.1, R3.2, R4.5, R6.7, R8.7).
//!
//! Every test drives the real pipeline: a real directory, the real watcher and
//! scan, the real state store, and the mock server's request counters.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use immich_client::ImmichClient;
use mock_immich::{Fault, MockImmich, Operation};
use sync_core::connection::{AuthMethod, Connection, RequestCredential, Secret, SignIn};
use sync_core::credential::MemoryCredentialStore;
use sync_core::engine::{Activity, Engine, EngineConfig};
use sync_core::hash::{Sha1, sha1_file};
use sync_core::store::{AlbumScope, FolderCounts, ItemRecord, ItemState, SourceFolder, StateStore};

/// Starts the mock and signs in, returning the client and credential the
/// engine attaches to every request.
async fn signed_in(server: &MockImmich) -> (ImmichClient, RequestCredential) {
    let client = ImmichClient::new(server.base_url()).expect("a valid mock URL");
    let mut connection = Connection::new(server.base_url(), Arc::new(MemoryCredentialStore::new()))
        .expect("a valid URL");
    connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        })
        .await;
    let credential = connection
        .credential()
        .expect("a signed-in session")
        .clone();
    (client, credential)
}

/// Opens a store in a fresh temp directory.
fn store_in(temp: &tempfile::TempDir) -> Arc<StateStore> {
    Arc::new(StateStore::open(temp.path().join("data").join("state.db")).expect("open the store"))
}

/// Adds a folder straight to the store, so the engine sees it at construction.
fn add_folder(store: &StateStore, path: &Path, recursive: bool) {
    store
        .add_folder(&SourceFolder {
            path: path.display().to_string(),
            recursive,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
        })
        .expect("add the source folder");
}

/// Adds a folder whose files join an album (R3.4).
fn add_mapped_folder(store: &StateStore, path: &Path, recursive: bool, scope: AlbumScope) {
    store
        .add_folder(&SourceFolder {
            path: path.display().to_string(),
            recursive,
            album_mapping: true,
            album_scope: scope,
        })
        .expect("add the mapped source folder");
}

/// Builds and starts an engine over `store`.
async fn start_engine(
    server: &MockImmich,
    store: &Arc<StateStore>,
    config: EngineConfig,
) -> Arc<Engine> {
    let (client, credential) = signed_in(server).await;
    let engine =
        Arc::new(Engine::new(store.clone(), client, credential, config).expect("build the engine"));
    engine.start();
    engine
}

/// Polls `check` until it returns true, or the timeout elapses.
async fn wait_until(mut check: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether every path has a terminal success row.
fn all_terminal(store: &StateStore, paths: &[PathBuf]) -> bool {
    paths.iter().all(|path| {
        matches!(
            store.item(&path.display().to_string()),
            Ok(Some(row)) if row.state.is_terminal_success()
        )
    })
}

/// The modification time the engine records, in nanoseconds since the epoch.
fn mtime_ns_of(path: &Path) -> i64 {
    let modified = std::fs::metadata(path)
        .expect("read the metadata")
        .modified()
        .expect("a modification time");
    i64::try_from(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a time after the epoch")
            .as_nanos(),
    )
    .expect("the nanoseconds fit")
}

/// Writes `bytes` to `root/name`, creating parent directories.
fn write_file(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create the fixture directory");
    }
    std::fs::write(&path, bytes).expect("write the fixture");
    path
}

#[tokio::test]
async fn a_folder_with_100_files_syncs_without_manual_action() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let mut paths = Vec::with_capacity(100);
    for index in 0..100 {
        // Four subfolders, so the scan walks a real tree.
        paths.push(write_file(
            &photos,
            &format!("set-{}/photo-{index:03}.jpg", index % 4),
            format!("photo {index}").as_bytes(),
        ));
    }
    add_folder(&store, &photos, true);

    let engine = start_engine(&server, &store, EngineConfig::default()).await;

    assert!(
        wait_until(|| all_terminal(&store, &paths), Duration::from_secs(30)).await,
        "not every file reached a terminal success state"
    );
    assert_eq!(server.upload_requests(), 100, "R3.1: every file uploaded");
    assert_eq!(
        server.bulk_check_requests(),
        1,
        "R3.1: 100 checksums fit one default batch"
    );
    for path in &paths {
        let row = store
            .item(&path.display().to_string())
            .expect("read the row")
            .expect("the row exists");
        assert!(row.state.is_terminal_success(), "{path:?}: {:?}", row.state);
        assert!(row.asset_id.is_some(), "R3.6: the asset id was recorded");
    }
    engine.stop();
}

#[tokio::test]
async fn the_batch_size_is_configurable() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let mut paths = Vec::new();
    for index in 0..7 {
        paths.push(write_file(
            &photos,
            &format!("photo-{index}.jpg"),
            format!("batch {index}").as_bytes(),
        ));
    }
    add_folder(&store, &photos, true);
    let config = EngineConfig {
        batch_size: 3,
        ..EngineConfig::default()
    };

    let engine = start_engine(&server, &store, config).await;

    assert!(
        wait_until(|| all_terminal(&store, &paths), Duration::from_secs(30)).await,
        "the batch did not finish"
    );
    assert_eq!(
        server.bulk_check_requests(),
        3,
        "R3.1: seven checksums at batch size three are 3 + 3 + 1"
    );
    engine.stop();
}

#[tokio::test]
async fn a_file_added_while_the_engine_runs_uploads_within_ten_seconds() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the folder");
    add_folder(&store, &photos, true);

    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    // The startup scan has nothing to do; the watcher is installed.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = Instant::now();
    let added = write_file(&photos, "arrived.jpg", b"a new photo");
    let uploaded = wait_until(
        || all_terminal(&store, std::slice::from_ref(&added)),
        Duration::from_secs(10),
    )
    .await;
    let elapsed = started.elapsed();

    println!("watched file uploaded in {elapsed:?}");
    assert!(
        uploaded,
        "R2.3: a watched file did not upload within 10 seconds ({elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "R2.3: the watched file took {elapsed:?}, over the 10 second budget"
    );
    assert_eq!(server.upload_requests(), 1);
    engine.stop();
}

#[tokio::test]
async fn a_file_added_while_the_engine_is_stopped_uploads_after_startup() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let added = write_file(&photos, "overnight.jpg", b"added while stopped");
    add_folder(&store, &photos, true);

    let engine = start_engine(&server, &store, EngineConfig::default()).await;

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&added)),
            Duration::from_secs(10)
        )
        .await,
        "R2.3: the startup scan did not find the file"
    );
    engine.stop();
}

#[tokio::test]
async fn a_change_the_watcher_missed_is_found_by_the_next_scan() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the folder");
    add_folder(&store, &photos, true);
    let config = EngineConfig {
        watch: false,
        ..EngineConfig::default()
    };
    let engine = start_engine(&server, &store, config).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // No watcher runs, so this file is invisible until a reconciliation scan.
    let missed = write_file(&photos, "missed.jpg", b"the watcher never saw me");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        store.item(&missed.display().to_string()).expect("read"),
        None,
        "the file must not be picked up before the scan"
    );

    engine.sync_now();

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&missed)),
            Duration::from_secs(10)
        )
        .await,
        "R2.3: the reconciliation scan did not find the missed file"
    );
    engine.stop();
}

#[tokio::test]
async fn pause_stops_new_uploads_within_five_seconds_and_resume_continues() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let first = write_file(&photos, "first.jpg", b"first");
    let second = write_file(&photos, "second.jpg", b"second");
    add_folder(&store, &photos, true);
    // The first upload occupies the only worker for a second, which gives the
    // test a real in-flight window to pause inside.
    server.fail(Operation::Upload, Fault::Slow(Duration::from_secs(1)));
    let config = EngineConfig {
        concurrency: 1,
        watch: false,
        ..EngineConfig::default()
    };
    let engine = start_engine(&server, &store, config).await;

    assert!(
        wait_until(|| server.upload_requests() == 1, Duration::from_secs(5)).await,
        "the first upload did not start"
    );
    let paused_at = Instant::now();
    engine.pause();
    assert_eq!(engine.status().activity, Activity::Paused);
    // Let the in-flight upload finish; the second must stay queued.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        server.upload_requests(),
        1,
        "R8.7: no new upload may begin after the pause"
    );
    println!(
        "pause held for {:?} with no new upload request",
        paused_at.elapsed()
    );
    // The scan order decides which of the two uploads went first; exactly one
    // is in the single worker and the other must still be waiting.
    let states: Vec<ItemState> = [&first, &second]
        .iter()
        .map(|path| {
            store
                .item(&path.display().to_string())
                .expect("read")
                .expect("the file has a row")
                .state
        })
        .collect();
    assert_eq!(
        states
            .iter()
            .filter(|state| **state == ItemState::Pending)
            .count(),
        1,
        "one file waits in the pending state while paused: {states:?}"
    );
    assert_eq!(
        states
            .iter()
            .filter(|state| state.is_terminal_success())
            .count(),
        1,
        "the in-flight file is the only one that finished: {states:?}"
    );

    engine.resume();

    assert!(
        wait_until(
            || all_terminal(&store, &[first.clone(), second.clone()]),
            Duration::from_secs(10),
        )
        .await,
        "R8.7: resume did not continue the queue"
    );
    assert_eq!(server.upload_requests(), 2);
    engine.stop();
}

#[tokio::test]
async fn stopping_the_engine_aborts_its_running_upload_workers() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let photo = write_file(&photos, "slow.jpg", b"slow bytes");
    add_folder(&store, &photos, true);
    // The worker sits on the wire long enough to stop the engine underneath it.
    server.fail(Operation::Upload, Fault::Slow(Duration::from_millis(800)));
    let config = EngineConfig {
        concurrency: 1,
        watch: false,
        ..EngineConfig::default()
    };
    let engine = start_engine(&server, &store, config).await;

    assert!(
        wait_until(|| server.upload_requests() == 1, Duration::from_secs(5)).await,
        "the upload worker did not start"
    );
    engine.stop();
    drop(engine);

    // The slow response is due well within this wait. A worker that `stop` did
    // not abort finishes the upload and records the success; an aborted one
    // leaves the row for the next run to requeue (R6.7).
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let row = store
        .item(&photo.display().to_string())
        .expect("read the row")
        .expect("the row exists");
    assert!(
        !row.state.is_terminal_success(),
        "a stopped engine's upload worker must not keep running: {:?}",
        row.state
    );
}

#[tokio::test]
async fn stopping_an_engine_that_never_started_is_safe() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let (client, credential) = signed_in(&server).await;
    let engine =
        Engine::new(store, client, credential, EngineConfig::default()).expect("build the engine");

    engine.stop();
    // Drop stops it again; neither call may panic with nothing to abort.
    drop(engine);
}

#[tokio::test]
async fn a_restart_resumes_without_reuploading_completed_items() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let first = write_file(&photos, "first.jpg", b"first");
    let second = write_file(&photos, "second.jpg", b"second");
    add_folder(&store, &photos, true);
    let config = EngineConfig {
        watch: false,
        ..EngineConfig::default()
    };

    // A first run completes both files.
    let engine = start_engine(&server, &store, config.clone()).await;
    assert!(
        wait_until(
            || all_terminal(&store, &[first.clone(), second.clone()]),
            Duration::from_secs(15)
        )
        .await,
        "the first run did not finish"
    );
    assert_eq!(server.upload_requests(), 2);
    let checks = server.bulk_check_requests();
    engine.stop();
    drop(engine);

    // The client restarts: a new file appeared while it was stopped, and one
    // row was left `uploading` by the simulated crash.
    let third = write_file(&photos, "third.jpg", b"third");
    let third_sha1 = sha1_file(&third).expect("hash the fixture");
    let third_meta = std::fs::metadata(&third).expect("read the fixture metadata");
    store
        .record(&ItemRecord {
            path: third.display().to_string(),
            size: third_meta.len(),
            mtime_ns: 1,
            sha1: third_sha1,
            asset_id: None,
            state: ItemState::Uploading,
            error_class: None,
            sidecar_sha1: None,
        })
        .expect("write the interrupted row");
    drop(store);

    let reopened = store_in(&temp);
    let engine = start_engine(&server, &reopened, config).await;

    assert!(
        wait_until(
            || all_terminal(&reopened, std::slice::from_ref(&third)),
            Duration::from_secs(15)
        )
        .await,
        "R6.7: the unfinished item was not re-queued after the restart"
    );
    assert_eq!(
        server.upload_requests(),
        3,
        "R6.7: completed items must not upload again after a restart"
    );
    assert_eq!(
        server.bulk_check_requests(),
        checks + 1,
        "only the new file is checked; the completed files short-circuit locally"
    );
    for path in [&first, &second] {
        let row = reopened
            .item(&path.display().to_string())
            .expect("read")
            .expect("the completed row survived the restart");
        assert!(row.state.is_terminal_success(), "{path:?}");
    }
    engine.stop();
}

#[tokio::test]
async fn skipped_files_never_reach_the_store_or_the_wire() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let visible = write_file(&photos, "visible.jpg", b"visible");
    let hidden = write_file(&photos, ".hidden.jpg", b"hidden");
    let hidden_dir = write_file(&photos, ".cache/inside.jpg", b"hidden folder");
    let temporary = write_file(&photos, "half.tmp", b"partial");
    let system = write_file(&photos, "Thumbs.db", b"system");
    let real = write_file(&photos, "real.jpg", b"real");
    let link = photos.join("link.jpg");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &link).expect("create the fixture symlink");
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&real, &link).expect("create the fixture symlink");
    add_folder(&store, &photos, true);
    let config = EngineConfig {
        watch_debounce: Duration::from_millis(200),
        ..EngineConfig::default()
    };
    let engine = start_engine(&server, &store, config).await;

    assert!(
        wait_until(
            || all_terminal(&store, &[visible.clone(), real.clone()]),
            Duration::from_secs(15)
        )
        .await,
        "the visible files did not upload"
    );
    for skipped in [&hidden, &hidden_dir, &temporary, &system, &link] {
        assert_eq!(
            store.item(&skipped.display().to_string()).expect("read"),
            None,
            "R2.2: {:?} must never reach the store",
            skipped
        );
    }
    assert_eq!(server.upload_requests(), 2, "only the visible files upload");

    // The watcher path applies the same predicate: a hidden file added while
    // the engine runs never reaches the queue.
    let live_hidden = write_file(&photos, ".live-hidden.jpg", b"hidden live");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        store
            .item(&live_hidden.display().to_string())
            .expect("read"),
        None,
        "R2.2: the watcher must refuse hidden files too"
    );
    assert_eq!(server.upload_requests(), 2);
    engine.stop();
}

#[tokio::test]
async fn a_folder_that_disappears_is_a_folder_status_not_per_file_errors() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "photo.jpg", b"photo");
    add_folder(&store, &photos, true);
    let config = EngineConfig {
        watch: false,
        ..EngineConfig::default()
    };
    let engine = start_engine(&server, &store, config).await;
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(15)
        )
        .await,
        "the file did not upload first"
    );

    // The drive is unplugged: the folder itself is gone.
    std::fs::remove_dir_all(&photos).expect("remove the fixture folder");
    engine.sync_now();

    assert!(
        wait_until(
            || {
                let status = engine.status();
                status
                    .folders
                    .first()
                    .is_some_and(|folder| !folder.available)
            },
            Duration::from_secs(10)
        )
        .await,
        "R2.5: the missing folder was not reported as a folder status"
    );
    let status = engine.status();
    assert_eq!(status.activity, Activity::NeedsAttention);
    let row = store
        .item(&file.display().to_string())
        .expect("read")
        .expect("the completed row stays");
    assert!(
        row.state.is_terminal_success(),
        "R2.5: a missing folder must not rewrite per-file outcomes: {:?}",
        row.state
    );
    assert_eq!(status.counts.failed, 0, "no per-file errors are raised");
    engine.stop();
}

#[tokio::test]
async fn a_restart_requeues_a_pending_row_whose_file_is_unchanged() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "pending.jpg", b"pending bytes");
    add_folder(&store, &photos, true);
    let meta = std::fs::metadata(&file).expect("read the fixture metadata");
    store
        .record(&ItemRecord {
            path: file.display().to_string(),
            size: meta.len(),
            mtime_ns: 1,
            sha1: sha1_file(&file).expect("hash the fixture"),
            asset_id: None,
            state: ItemState::Pending,
            error_class: None,
            sidecar_sha1: None,
        })
        .expect("write the pending row");

    let config = EngineConfig {
        watch: false,
        ..EngineConfig::default()
    };
    let engine = start_engine(&server, &store, config).await;

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(15)
        )
        .await,
        "R6.7: the pending row was not resumed"
    );
    assert_eq!(server.upload_requests(), 1);
    engine.stop();
}

#[tokio::test]
async fn an_unchanged_size_and_mtime_skips_the_hash_of_a_finished_row() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "settled.jpg", b"settled bytes");
    add_folder(&store, &photos, true);
    let meta = std::fs::metadata(&file).expect("read the fixture metadata");
    // A finished row whose recorded SHA-1 differs from the file, but whose
    // size and mtime match. R4.5 reads the row first, so the bytes are never
    // hashed and the stale checksum is never compared.
    store
        .record(&ItemRecord {
            path: file.display().to_string(),
            size: meta.len(),
            mtime_ns: mtime_ns_of(&file),
            sha1: Sha1::from_hex("0000000000000000000000000000000000000000").expect("a hex SHA-1"),
            asset_id: Some("asset-1".to_string()),
            state: ItemState::Uploaded,
            error_class: None,
            sidecar_sha1: None,
        })
        .expect("write the settled row");
    let config = EngineConfig {
        watch: false,
        ..EngineConfig::default()
    };
    let engine = start_engine(&server, &store, config).await;
    engine.sync_now();
    // Let the scan and any would-be upload run.
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        server.upload_requests(),
        0,
        "R4.5: an unchanged settled file must not be re-read"
    );
    assert_eq!(server.bulk_check_requests(), 0);
    assert_eq!(engine.status().counts.pending, 0);
    engine.stop();
}

#[tokio::test]
async fn a_transient_failure_that_later_succeeds_clears_the_folder_error() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    write_file(&photos, "retry.jpg", b"first bytes");
    add_folder(&store, &photos, true);
    // The first upload fails with a retryable server error.
    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 500,
            retry_after: None,
        },
    );
    let config = EngineConfig {
        watch: false,
        ..EngineConfig::default()
    };
    let engine = start_engine(&server, &store, config).await;

    assert!(
        wait_until(
            || engine
                .status()
                .folders
                .iter()
                .any(|folder| folder.last_error.is_some()),
            Duration::from_secs(10)
        )
        .await,
        "the transient failure was not surfaced"
    );

    // The file changes; a later scan re-uploads it, this time successfully.
    let file = write_file(&photos, "retry.jpg", b"changed bytes that succeed");
    engine.sync_now();

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(15)
        )
        .await,
        "the changed file did not upload"
    );
    assert!(
        wait_until(
            || engine
                .status()
                .folders
                .iter()
                .all(|folder| folder.last_error.is_none()),
            Duration::from_secs(5)
        )
        .await,
        "a later success must clear the parked error: {:?}",
        engine.status()
    );
    assert_ne!(engine.status().activity, Activity::NeedsAttention);
    engine.stop();
}

#[tokio::test]
async fn a_restart_with_a_pending_row_does_not_double_count_the_pending_item() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "pending.jpg", b"pending bytes");
    add_folder(&store, &photos, true);
    // A row a previous run left pending, exactly as the store seeds the
    // dashboard's count at construction.
    let meta = std::fs::metadata(&file).expect("read the fixture metadata");
    store
        .record(&ItemRecord {
            path: file.display().to_string(),
            size: meta.len(),
            mtime_ns: 1,
            sha1: sha1_file(&file).expect("hash the fixture"),
            asset_id: None,
            state: ItemState::Pending,
            error_class: None,
            sidecar_sha1: None,
        })
        .expect("write the pending row");
    let config = EngineConfig {
        watch: false,
        ..EngineConfig::default()
    };
    let engine = start_engine(&server, &store, config).await;

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(15)
        )
        .await,
        "the pending row was not resumed"
    );
    assert_eq!(server.upload_requests(), 1);
    // R8.1: the seed already counted the pending row, so `prepare` must not
    // count it a second time and leak the count past its one `finish_item`.
    assert!(
        wait_until(
            || engine.status().counts.pending == 0,
            Duration::from_secs(5)
        )
        .await,
        "the pending count leaked: {:?}",
        engine.status().counts
    );
    assert_eq!(
        engine.status().progress(),
        1.0,
        "the drained queue must read as complete"
    );
    engine.stop();
}

#[tokio::test]
async fn a_re_added_folder_seeds_its_counts_from_the_store() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let files = [
        write_file(&photos, "a.jpg", b"first"),
        write_file(&photos, "b.jpg", b"second"),
    ];
    // Rows an earlier run left behind while the folder was not a source.
    for file in &files {
        let meta = std::fs::metadata(file).expect("read the fixture metadata");
        store
            .record(&ItemRecord {
                path: file.display().to_string(),
                size: meta.len(),
                mtime_ns: mtime_ns_of(file),
                sha1: sha1_file(file).expect("hash the fixture"),
                asset_id: Some("asset-1".to_string()),
                state: ItemState::Uploaded,
                error_class: None,
                sidecar_sha1: None,
            })
            .expect("write the terminal row");
    }
    let config = EngineConfig {
        watch: false,
        ..EngineConfig::default()
    };
    let engine = start_engine(&server, &store, config).await;

    engine
        .add_folder(&photos.display().to_string(), true)
        .expect("re-add the folder");

    // R8.1: the card shows what the store already holds, not a zeroed card
    // the settled rows never fill.
    assert_eq!(
        engine.status().folders.first().map(|folder| folder.counts),
        Some(FolderCounts {
            pending: 0,
            uploaded: 2,
            failed: 0,
        }),
        "a re-added folder starts from the rows the store holds"
    );
    assert_eq!(server.upload_requests(), 0, "settled rows stay settled");
    engine.stop();
}

#[tokio::test]
async fn adding_a_folder_through_the_engine_syncs_its_existing_files() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "existing.jpg", b"existing");
    let config = EngineConfig {
        watch: false,
        ..EngineConfig::default()
    };
    let engine = start_engine(&server, &store, config).await;

    engine
        .add_folder(&photos.display().to_string(), true)
        .expect("add the folder");

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(15)
        )
        .await,
        "R2.1: adding a folder must discover its existing files"
    );
    assert_eq!(
        engine.status().folders.len(),
        1,
        "the dashboard sees the new folder"
    );
    // The folder persists for the next run.
    assert_eq!(
        store.folders().expect("read the folders"),
        vec![SourceFolder {
            path: photos.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
        }]
    );

    engine
        .remove_folder(&photos.display().to_string())
        .expect("remove the folder");
    assert!(engine.status().folders.is_empty());
    assert!(store.folders().expect("read the folders").is_empty());
    engine.stop();
}

/// The asset id the store recorded for one file.
fn asset_id_of(store: &StateStore, path: &Path) -> String {
    store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the upload was recorded")
        .asset_id
        .expect("a finished row carries an asset id (R3.6)")
}

/// The album names the server holds, sorted, because upload order decides the
/// order albums were created in.
fn sorted_album_names(server: &MockImmich) -> Vec<String> {
    let mut names = server.album_names();
    names.sort();
    names
}

/// Waits until every file's asset is in some album.
///
/// The request counters alone cannot be the signal: the double counts a request
/// when it arrives, so a count can be complete while its album row is not. The
/// membership the client is after is the observable end state.
async fn wait_for_albums(store: &StateStore, server: &MockImmich, files: &[PathBuf]) -> bool {
    wait_until(
        || {
            files.iter().all(|file| {
                store
                    .item(&file.display().to_string())
                    .ok()
                    .flatten()
                    .and_then(|row| row.asset_id)
                    .is_some_and(|asset| server.album_for_asset(&asset).is_some())
            })
        },
        Duration::from_secs(15),
    )
    .await
}

#[tokio::test]
async fn a_mapped_folder_puts_every_asset_in_the_album_named_after_it() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let nested = write_file(&photos, "Holiday/beach.jpg", b"beach");
    let at_root = write_file(&photos, "cover.jpg", b"cover");
    // The whole-source-folder scope maps every file below it, whatever folder
    // the file sits in (R3.4).
    add_mapped_folder(&store, &photos, true, AlbumScope::WholeFolder);

    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    assert!(
        wait_until(
            || all_terminal(&store, &[nested.clone(), at_root.clone()]),
            Duration::from_secs(15)
        )
        .await,
        "both files must reach a terminal success state"
    );
    assert!(
        wait_for_albums(&store, &server, &[nested.clone(), at_root.clone()]).await,
        "both assets must join an album"
    );

    assert_eq!(
        sorted_album_names(&server),
        vec!["Pictures".to_string()],
        "R3.4: whole-folder scope makes one album for the source folder"
    );
    assert_eq!(
        server
            .album_for_asset(&asset_id_of(&store, &nested))
            .as_deref(),
        Some("Pictures"),
        "a file in a subfolder joins the source folder's album"
    );
    assert_eq!(
        server
            .album_for_asset(&asset_id_of(&store, &at_root))
            .as_deref(),
        Some("Pictures")
    );
    assert_eq!(
        server.create_album_requests(),
        1,
        "the album is created once, whatever holds how many files"
    );
    assert_eq!(
        server.add_album_requests(),
        2,
        "one call per uploaded asset"
    );
    engine.stop();
}

#[tokio::test]
async fn leaf_scope_creates_one_album_per_folder_that_contains_files() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let files = vec![
        write_file(&photos, "Holiday/beach.jpg", b"beach"),
        write_file(&photos, "Holiday/sunset.jpg", b"sunset"),
        write_file(&photos, "Winter/snow.jpg", b"snow"),
        // A folder with no files of its own makes no album: the album scope is
        // per folder that directly contains files (GLOSSARY).
        write_file(&photos, "Winter/2024/deep.jpg", b"deep"),
        // A file directly in the source folder maps to the source folder.
        write_file(&photos, "cover.jpg", b"cover"),
    ];
    add_mapped_folder(&store, &photos, true, AlbumScope::Leaf);

    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    assert!(
        wait_until(|| all_terminal(&store, &files), Duration::from_secs(20)).await,
        "every file must reach a terminal success state"
    );
    assert!(
        wait_for_albums(&store, &server, &files).await,
        "every asset must join its folder's album"
    );

    assert_eq!(
        sorted_album_names(&server),
        vec![
            "2024".to_string(),
            "Holiday".to_string(),
            "Pictures".to_string(),
            "Winter".to_string()
        ],
        "R3.4: leaf scope makes one album per folder that directly contains files"
    );
    for file in &files {
        let album = server
            .album_for_asset(&asset_id_of(&store, file))
            .expect("every uploaded asset joined an album");
        let folder = file
            .parent()
            .and_then(|parent| parent.file_name())
            .expect("the fixture has a folder")
            .to_str()
            .expect("the fixture name is UTF-8");
        assert_eq!(
            album,
            folder,
            "{} joined the album of the folder that holds it",
            file.display()
        );
    }
    engine.stop();
}

#[tokio::test]
async fn two_leaf_folders_with_the_same_name_are_two_albums() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let in_2023 = write_file(&photos, "2023/Holiday/beach.jpg", b"beach");
    let in_2024 = write_file(&photos, "2024/Holiday/snow.jpg", b"snow");
    add_mapped_folder(&store, &photos, true, AlbumScope::Leaf);

    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    assert!(
        wait_until(
            || all_terminal(&store, &[in_2023.clone(), in_2024.clone()]),
            Duration::from_secs(15)
        )
        .await,
        "both files must reach a terminal success state"
    );
    assert!(
        wait_for_albums(&store, &server, &[in_2023.clone(), in_2024.clone()]).await,
        "both assets must join an album"
    );

    // R3.4: the album scope is one album per folder, so two folders are two
    // albums. The name is a copy of the folder name, not the album's identity,
    // and the client's only album write is "add to album", so an album that
    // held both could never be split again.
    assert_eq!(
        sorted_album_names(&server),
        vec!["Holiday".to_string(), "Holiday".to_string()],
        "GLOSSARY: one album per folder that directly contains files"
    );
    assert_eq!(
        server.create_album_requests(),
        2,
        "each folder gets its own album, so two folders share nothing"
    );
    for file in [&in_2023, &in_2024] {
        assert_eq!(
            server
                .album_for_asset(&asset_id_of(&store, file))
                .as_deref(),
            Some("Holiday"),
            "{} joined an album named after its folder",
            file.display()
        );
    }
    engine.stop();
}

#[tokio::test]
async fn two_added_folders_with_the_same_name_are_two_albums() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    // Two folders the user added, whose basenames agree (R2.4).
    let first = temp.path().join("a").join("Pictures");
    let second = temp.path().join("b").join("Pictures");
    let in_first = write_file(&first, "beach.jpg", b"beach");
    let in_second = write_file(&second, "snow.jpg", b"snow");
    add_mapped_folder(&store, &first, true, AlbumScope::WholeFolder);
    add_mapped_folder(&store, &second, true, AlbumScope::WholeFolder);

    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    assert!(
        wait_until(
            || all_terminal(&store, &[in_first.clone(), in_second.clone()]),
            Duration::from_secs(15)
        )
        .await,
        "both folders must reach a terminal success state"
    );
    assert!(
        wait_for_albums(&store, &server, &[in_first.clone(), in_second.clone()]).await,
        "both assets must join an album"
    );

    // R3.4: whole-folder scope creates one album for the source folder, so two
    // added folders are two albums, whatever their names agree on.
    assert_eq!(
        sorted_album_names(&server),
        vec!["Pictures".to_string(), "Pictures".to_string()],
        "each source folder has its own album"
    );
    assert_eq!(
        server.create_album_requests(),
        2,
        "one album per source folder, not per name"
    );
    for file in [&in_first, &in_second] {
        assert_eq!(
            server
                .album_for_asset(&asset_id_of(&store, file))
                .as_deref(),
            Some("Pictures"),
            "{} joined its own folder's album",
            file.display()
        );
    }

    // R3.4: the album ids live in the state database, so a restart joins the
    // albums it already made instead of creating a second one for the folder.
    // The store is reopened so the ids come off disk, not a warm cache.
    engine.stop();
    drop(engine);
    drop(store);
    let store = store_in(&temp);
    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    let after_restart = [
        write_file(&first, "reef.jpg", b"reef"),
        write_file(&second, "ice.jpg", b"ice"),
    ];
    assert!(
        wait_until(
            || all_terminal(&store, &after_restart),
            Duration::from_secs(15)
        )
        .await,
        "both files must reach a terminal success state after the restart"
    );
    assert!(
        wait_for_albums(&store, &server, &after_restart).await,
        "both assets must join the albums the first run made"
    );
    assert_eq!(
        server.create_album_requests(),
        2,
        "R3.4: a restart reuses the recorded album ids, one per folder"
    );
    for file in &after_restart {
        assert_eq!(
            server
                .album_for_asset(&asset_id_of(&store, file))
                .as_deref(),
            Some("Pictures"),
            "{} joined the album its folder already had",
            file.display()
        );
    }
    engine.stop();
}

#[tokio::test]
async fn an_unmapped_folder_creates_no_album_at_all() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let mapped = write_file(&photos, "Holiday/beach.jpg", b"beach");
    let videos = temp.path().join("Videos");
    let unmapped = write_file(&videos, "Holiday/snow.jpg", b"snow");
    add_mapped_folder(&store, &photos, true, AlbumScope::Leaf);
    // Album mapping is opt-in per folder (R3.4).
    add_folder(&store, &videos, true);

    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    assert!(
        wait_until(
            || all_terminal(&store, &[mapped.clone(), unmapped.clone()]),
            Duration::from_secs(15)
        )
        .await,
        "both folders must reach a terminal success state"
    );
    assert!(
        wait_for_albums(&store, &server, std::slice::from_ref(&mapped)).await,
        "the mapped folder's asset must join its album"
    );

    assert_eq!(
        server.create_album_requests(),
        1,
        "R3.4: the unmapped folder sends no album request at all"
    );
    assert_eq!(
        server.add_album_requests(),
        1,
        "one call for one mapped file, none for the unmapped one"
    );
    assert_eq!(
        server.album_for_asset(&asset_id_of(&store, &unmapped)),
        None,
        "a file from an unmapped folder is in no album"
    );
    engine.stop();
}

#[tokio::test]
async fn a_duplicate_the_server_already_has_joins_the_album_too() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let file = write_file(&photos, "Holiday/beach.jpg", b"beach");
    add_mapped_folder(&store, &photos, true, AlbumScope::Leaf);

    // The content is already on the server from another client, in no album:
    // a duplicate counts as success (R3.2) and still maps (R3.4).
    let (client, credential) = signed_in(&server).await;
    let existing = client
        .upload_asset(
            credential.auth(),
            &immich_client::AssetUpload {
                path: &file,
                checksum: &sha1_file(&file).expect("hash the fixture").base64(),
                created_at: std::time::UNIX_EPOCH,
                modified_at: std::time::UNIX_EPOCH,
                sidecar: None,
                live_photo_video_id: None,
            },
        )
        .await
        .expect("the first upload is created");
    assert!(!existing.duplicate);

    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(15)
        )
        .await,
        "the duplicate must reach a terminal success state"
    );
    assert!(
        wait_for_albums(&store, &server, std::slice::from_ref(&file)).await,
        "the duplicate must join the album"
    );

    assert_eq!(
        server.upload_requests(),
        1,
        "R3.2: the file was not uploaded again"
    );
    assert_eq!(
        store
            .item(&file.display().to_string())
            .expect("read the row")
            .expect("the row")
            .state,
        ItemState::Duplicate
    );
    assert_eq!(
        server.album_for_asset(&existing.asset_id).as_deref(),
        Some("Holiday"),
        "R3.4: a duplicate that is missing from the album is added to it"
    );
    engine.stop();
}

#[tokio::test]
async fn the_same_content_twice_in_one_folder_keeps_one_membership() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let first = write_file(&photos, "Holiday/beach.jpg", b"beach");
    // The same bytes under another name in the same folder: one asset, so one
    // album membership, and the album itself is created once (R3.2, R3.4).
    let second = write_file(&photos, "Holiday/beach-copy.jpg", b"beach");
    add_mapped_folder(&store, &photos, true, AlbumScope::Leaf);

    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    assert!(
        wait_until(
            || all_terminal(&store, &[first.clone(), second.clone()]),
            Duration::from_secs(15)
        )
        .await,
        "both rows must reach a terminal success state"
    );
    assert!(
        wait_for_albums(&store, &server, &[first.clone(), second.clone()]).await,
        "both rows must have reached the album"
    );
    // Both rows point at the same asset, so the membership above is satisfied
    // by one call; the second item's own call is what this waits for.
    assert!(
        wait_until(|| server.add_album_requests() >= 2, Duration::from_secs(15)).await,
        "the second item must have asked for the album too"
    );

    assert_eq!(
        sorted_album_names(&server),
        vec!["Holiday".to_string()],
        "one folder, one album"
    );
    assert_eq!(
        server.create_album_requests(),
        1,
        "the second item finds the album the first one created"
    );
    assert_eq!(
        server.add_album_requests(),
        2,
        "both items ask for the album, and adding an asset the album already holds is harmless"
    );
    assert_eq!(
        server.album_for_asset(&asset_id_of(&store, &second)),
        Some("Holiday".to_string())
    );
    engine.stop();
}

#[tokio::test]
async fn an_album_failure_never_loses_the_uploaded_item() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let file = write_file(&photos, "Holiday/beach.jpg", b"beach");
    add_mapped_folder(&store, &photos, true, AlbumScope::Leaf);
    server.fail(Operation::CreateAlbum, Fault::PermissionDenied);

    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(15)
        )
        .await,
        "the upload itself must finish even when the album call is refused"
    );
    assert!(
        wait_until(
            || engine
                .status()
                .folders
                .first()
                .and_then(|folder| folder.last_error.as_deref())
                .is_some_and(|error| error.contains("album")),
            Duration::from_secs(15)
        )
        .await,
        "the folder status must report what could not be done"
    );

    let row = store
        .item(&file.display().to_string())
        .expect("read the row")
        .expect("the upload was recorded");
    assert_eq!(
        row.state,
        ItemState::Uploaded,
        "R3.2: the item is uploaded; only the album mapping failed"
    );
    assert!(
        row.asset_id.is_some(),
        "the asset mapping is what the deletion review keys on, so it survives"
    );
    let status = engine.status();
    assert_eq!(
        status.activity,
        Activity::NeedsAttention,
        "the dashboard and the tray show that something needs the user (R8.1)"
    );
    assert!(
        status.folders[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.starts_with("album mapping failed")),
        "the folder status says what could not be done: {:?}",
        status.folders[0].last_error
    );
    assert_eq!(
        server.album_names(),
        Vec::<String>::new(),
        "no album exists after a refused create"
    );
    engine.stop();
}
