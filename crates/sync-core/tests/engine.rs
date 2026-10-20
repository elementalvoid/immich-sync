//! The folder sync engine against the offline Immich double (SPEC R2.1–R2.5,
//! R3.1, R3.2, R4.1–R4.5, R6.7, R8.7).
//!
//! Every test drives the real pipeline: a real directory, the real watcher and
//! scan, the real state store, and the mock server's request counters.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use immich_client::{BandwidthLimit, ClientError};
use mock_immich::{DeleteCall, Fault, MockImmich, Operation};
use sync_core::backoff::RetryPolicy;
use sync_core::connection::{
    Attention, AuthMethod, Connection, ConnectionState, Secret, SharedSession, SignIn,
};
use sync_core::credential::MemoryCredentialStore;
use sync_core::engine::{Activity, Engine, EngineConfig, ReviewError};
use sync_core::failure::FailureClass;
use sync_core::hash::{Sha1, sha1_file};
use sync_core::store::{
    AlbumScope, AttemptOutcome, AttemptRecord, FolderCounts, ItemRecord, ItemState, SourceFolder,
    StateStore,
};
use sync_core::tracking::Tracking;

use common::wait_until;

/// Starts the mock and signs in, returning the session the engine attaches to
/// every request (R1.4).
async fn signed_in(server: &MockImmich) -> SharedSession {
    signed_in_with(server, AuthMethod::Password, "correct horse").await
}

/// Signs in with the credential the test names, so a test can drive the
/// API-key path of R1.5.
async fn signed_in_with(server: &MockImmich, method: AuthMethod, secret: &str) -> SharedSession {
    let mut connection = Connection::new(server.base_url(), Arc::new(MemoryCredentialStore::new()))
        .expect("a valid URL");
    connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new(secret),
            method,
        })
        .await;
    connection.shared()
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
    let session = signed_in(server).await;
    let engine = Arc::new(
        Engine::new(Arc::new(Tracking::new(store.clone())), session, config)
            .expect("build the engine"),
    );
    engine.start();
    engine
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

/// How many of `paths` have reached a terminal success row, so a wait can
/// measure the pipeline's progress instead of the clock.
fn terminal_count(store: &StateStore, paths: &[PathBuf]) -> u64 {
    let count = paths
        .iter()
        .filter(|path| {
            matches!(
                store.item(&path.display().to_string()),
                Ok(Some(row)) if row.state.is_terminal_success()
            )
        })
        .count();
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// Polls `check` until it returns true, giving up only after `stall` passes
/// with `progress` unchanged.
///
/// A wall-clock budget starves a real pipeline under load: the work a test
/// waits for can take longer than any second-based number while still moving.
/// This waits on the observable progress instead, so the test fails only when
/// the pipeline stops producing outcomes.
async fn wait_for_progress(
    mut progress: impl FnMut() -> u64,
    mut check: impl FnMut() -> bool,
    stall: Duration,
) -> bool {
    let mut last = progress();
    let mut deadline = Instant::now() + stall;
    // A pipeline that keeps trickling out progress would reset the stall
    // forever, so the wait also has a hard cap: ten stalls without the result.
    let hard_deadline = Instant::now() + stall.saturating_mul(10);
    loop {
        if check() {
            return true;
        }
        let current = progress();
        if current != last {
            last = current;
            deadline = Instant::now() + stall;
        }
        if Instant::now() >= deadline || Instant::now() >= hard_deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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
        wait_for_progress(
            || terminal_count(&store, &paths),
            || all_terminal(&store, &paths),
            Duration::from_secs(20),
        )
        .await,
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
    let session = signed_in(&server).await;
    let engine = Engine::new(
        Arc::new(Tracking::new(store)),
        session,
        EngineConfig::default(),
    )
    .expect("build the engine");

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
            error_message: None,
            attempts: 0,
            sidecar: None,
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
            error_message: None,
            attempts: 0,
            sidecar: None,
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
            error_message: None,
            attempts: 0,
            sidecar: None,
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
async fn a_recovered_folder_clears_the_global_last_error() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    write_file(&photos, "mapped.jpg", b"mapped bytes");
    add_mapped_folder(&store, &photos, true, AlbumScope::Leaf);
    // The upload lands, but the album call it maps to fails, which records the
    // failure on the folder and on the tray's global card (R3.4, R8.2).
    server.fail(
        Operation::CreateAlbum,
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
            || engine.status().last_error.is_some(),
            Duration::from_secs(15)
        )
        .await,
        "the album failure was not recorded globally"
    );

    // A later scan finds the folder available and nothing failing.
    engine.sync_now();
    assert!(
        wait_until(
            || engine.status().last_error.is_none(),
            Duration::from_secs(10)
        )
        .await,
        "the global last error stayed sticky after recovery: {:?}",
        engine.status()
    );
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
            error_message: None,
            attempts: 0,
            sidecar: None,
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

/// A restart that interrupts a retry must not seed a failed count that no
/// later success clears (R6.7, R8.1).
///
/// `Engine::new` read the folder counts before `requeue_unfinished`, so an
/// interrupted `retry_wait` row was counted as failed and then became
/// pending. `prepare` saw it already pending, so it never moved the count
/// back down, and the item's later success left a phantom failure on the
/// card.
#[tokio::test]
async fn a_restart_that_interrupts_a_retry_does_not_seed_a_failed_count() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "interrupted.jpg", b"interrupted bytes");
    add_folder(&store, &photos, true);
    let meta = std::fs::metadata(&file).expect("read the fixture metadata");
    store
        .record(&ItemRecord {
            path: file.display().to_string(),
            size: meta.len(),
            mtime_ns: mtime_ns_of(&file),
            sha1: sha1_file(&file).expect("hash the fixture"),
            asset_id: None,
            state: ItemState::RetryWait,
            error_class: Some(FailureClass::ServerTransient),
            error_message: Some("the server was busy".to_string()),
            attempts: 1,
            sidecar: None,
        })
        .expect("write the interrupted retry row");
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
        "the requeued row was not resumed"
    );
    assert_eq!(server.upload_requests(), 1);
    assert_eq!(
        engine.status().counts.failed,
        0,
        "the interrupted retry left a phantom failure on the card: {:?}",
        engine.status().counts
    );
    assert_eq!(engine.status().counts.uploaded, 1);
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
                error_message: None,
                attempts: 0,
                sidecar: None,
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
            review: 0,
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

/// Re-adding an existing folder applies the new recursive flag to the store,
/// the card, and the watch (R2.1).
///
/// A recursive watch also watches every subfolder; re-watching the root alone
/// leaves those behind, so a folder turned non-recursive would keep reporting
/// nested files. The watcher is real here: only the watcher can find the late
/// file, because the scan interval is an hour and nothing calls `sync_now`.
#[tokio::test]
async fn re_adding_a_folder_applies_the_new_recursive_flag() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    add_folder(&store, &photos, true);
    let nested = write_file(&photos, "Holiday/beach.jpg", b"nested bytes");
    let engine = start_engine(
        &server,
        &store,
        EngineConfig {
            watch: true,
            watch_debounce: Duration::from_millis(200),
            scan_interval: Duration::from_secs(3600),
            ..EngineConfig::default()
        },
    )
    .await;
    // The folder is recursive, so the nested file uploads.
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&nested)),
            Duration::from_secs(20)
        )
        .await,
        "the recursive folder did not find the nested file"
    );

    engine
        .add_folder(&photos.display().to_string(), false)
        .expect("re-add the folder");
    assert!(
        !engine.status().folders[0].recursive,
        "the card must show the new flag"
    );
    assert!(
        !store.folders().expect("read the folders")[0].recursive,
        "the store must hold the new flag"
    );

    // A file in a subfolder must not reach the queue: the watch moved to
    // non-recursive.
    let late = write_file(&photos, "Holiday/late.jpg", b"late nested bytes");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        store
            .item(&late.display().to_string())
            .expect("read the row")
            .is_none(),
        "R2.1: a nested file must not be discovered after the folder turned non-recursive"
    );
    engine.stop();
}

/// Re-adding an existing folder must not turn its album mapping off (R3.4).
///
/// The app's per-folder recursive toggle re-adds the folder through
/// `Engine::add_folder`. The store's upsert writes every tracked column, so a
/// folder constructed with `album_mapping: false` silently unmapped a folder
/// the user had mapped, and the loss only showed after a restart.
#[tokio::test]
async fn re_adding_a_folder_keeps_its_album_mapping() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the folder");
    let engine = start_engine(&server, &store, EngineConfig::default()).await;

    engine
        .add_folder(&photos.display().to_string(), true)
        .expect("add the folder");
    engine
        .set_album_mapping(&photos.display().to_string(), true)
        .expect("enable album mapping");
    engine
        .set_album_scope(&photos.display().to_string(), AlbumScope::WholeFolder)
        .expect("set the album scope");

    engine
        .add_folder(&photos.display().to_string(), false)
        .expect("re-add the folder");

    let stored = store
        .folders()
        .expect("read the folders")
        .into_iter()
        .find(|folder| folder.path == photos.display().to_string())
        .expect("the folder is stored");
    assert!(
        stored.album_mapping,
        "re-adding the folder turned album mapping off"
    );
    assert_eq!(
        stored.album_scope,
        AlbumScope::WholeFolder,
        "re-adding the folder reset the album scope"
    );
    let card = engine.status().folders[0].clone();
    assert!(card.album_mapping, "the card lost the album mapping");
    assert_eq!(
        card.album_scope,
        AlbumScope::WholeFolder,
        "the card lost the album scope"
    );
    engine.stop();
}

// The stale-stored-row case is gone by construction: every folder write goes
// through `Tracking`, which holds one write lock across the read and the
// upsert, so a live entry can no longer be ahead of the store. The
// interleaved test below still runs both edits at the same time through the
// public interface.

/// The two edits can run at the same time: album toggles (which take the
/// folders lock) and re-adds. Neither may lose the other's field (R3.4).
///
/// The album thread writes a value it remembers, then checks the store for a
/// while before its next write; a re-add that read the row before that write
/// and wrote it after leaves the old value in the store, and the check sees
/// it. The re-add thread only loops, so the checks cannot race a later album
/// edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interleaved_recursive_toggle_and_album_toggle_lose_nothing() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the folder");
    let engine = start_engine(
        &server,
        &store,
        EngineConfig {
            watch: false,
            ..EngineConfig::default()
        },
    )
    .await;
    let path = photos.display().to_string();
    engine.add_folder(&path, true).expect("add the folder");

    let stored_mapping = || {
        store
            .folders()
            .expect("read the folders")
            .into_iter()
            .find(|folder| folder.path == path)
            .expect("the folder is stored")
            .album_mapping
    };
    let barrier = std::sync::Barrier::new(2);
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            barrier.wait();
            let mut expected = false;
            for _ in 0..40 {
                expected = !expected;
                engine
                    .set_album_mapping(&path, expected)
                    .expect("toggle the album mapping");
                // The store must hold this toggle until the next one, unless
                // a re-add wrote back a row it read before it.
                for _ in 0..3 {
                    assert_eq!(
                        stored_mapping(),
                        expected,
                        "a re-add wrote the row it read before the album toggle"
                    );
                }
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        scope.spawn(|| {
            barrier.wait();
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                engine.add_folder(&path, true).expect("re-add the folder");
            }
        });
    });
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

// --- Retry, the attempt cap, the dead letter, and the circuit breaker (#7) ---

/// The retry policy a test pins, so the waits it observes are reproducible.
fn test_retry() -> RetryPolicy {
    RetryPolicy {
        base: Duration::from_millis(20),
        max: Duration::from_millis(80),
        max_retry_after: Duration::from_secs(5),
        seed: Some(7),
    }
}

/// The config the retry tests start from: one worker, no watcher, short waits.
fn retry_config(cap: u32) -> EngineConfig {
    EngineConfig {
        attempt_cap: cap,
        retry: test_retry(),
        breaker_threshold: 3,
        attention_threshold: 3,
        breaker_cooldown: Duration::from_millis(120),
        concurrency: 1,
        watch: false,
        ..EngineConfig::default()
    }
}

/// Builds and starts an engine and hands back the session, so a test can read
/// the connection state a 401 produced (R1.4).
async fn start_engine_with_session(
    server: &MockImmich,
    store: &Arc<StateStore>,
    config: EngineConfig,
) -> (Arc<Engine>, SharedSession) {
    let session = signed_in(server).await;
    (start_engine_over(&session, store, config), session)
}

/// Starts an engine over a session the test signed in itself, so the test
/// chooses the credential and can read the state it ends in (R1.4, R1.5).
fn start_engine_over(
    session: &SharedSession,
    store: &Arc<StateStore>,
    config: EngineConfig,
) -> Arc<Engine> {
    let engine = Arc::new(
        Engine::new(
            Arc::new(Tracking::new(store.clone())),
            session.clone(),
            config,
        )
        .expect("build the engine"),
    );
    engine.start();
    engine
}

/// The gaps between the attempt start times the store recorded, in order.
fn attempt_gaps(store: &StateStore, path: &Path) -> Vec<Duration> {
    let attempts = store
        .attempts(&path.display().to_string())
        .expect("read the attempt log");
    attempts
        .windows(2)
        .map(|pair| {
            let nanos = pair[1].started_ns - pair[0].started_ns;
            Duration::from_nanos(u64::try_from(nanos).unwrap_or(0))
        })
        .collect()
}

/// Whether one path holds a row in the dead letter.
fn is_dead_letter(store: &StateStore, path: &Path) -> bool {
    matches!(
        store.item(&path.display().to_string()),
        Ok(Some(row)) if row.state == ItemState::DeadLetter
    )
}

#[tokio::test]
async fn a_server_that_stops_answering_produces_backoff_not_a_hot_loop() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "retried.jpg", b"retried bytes");
    add_folder(&store, &photos, true);
    // Five refusals, then the server answers again.
    for _ in 0..5 {
        server.fail(
            Operation::Upload,
            Fault::Status {
                code: 503,
                retry_after: None,
            },
        );
    }
    let config = retry_config(8);
    let engine = start_engine(&server, &store, config.clone()).await;

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(20)
        )
        .await,
        "the retried upload never finished"
    );
    assert_eq!(
        server.upload_requests(),
        6,
        "R6.1: five refusals and the attempt that succeeded, nothing more"
    );
    let attempts = store
        .attempts(&file.display().to_string())
        .expect("read the attempt log");
    assert_eq!(attempts.len(), 6, "SPEC §5: every attempt was recorded");
    for attempt in &attempts[..5] {
        assert_eq!(attempt.outcome, AttemptOutcome::Failed);
        assert_eq!(attempt.error_class, Some(FailureClass::ServerTransient));
    }
    assert_eq!(attempts[5].outcome, AttemptOutcome::Succeeded);

    // R6.1: the observed waits follow the policy, and the policy's ceiling
    // grows, so the run is a backoff rather than a tight loop.
    let gaps = attempt_gaps(&store, &file);
    assert_eq!(gaps.len(), 5);
    for (index, gap) in gaps.iter().enumerate() {
        let attempt = u32::try_from(index + 1).expect("a small index");
        let expected = config.retry.delay(attempt, None);
        // A gap is the policy's wait plus the request round trip, so the
        // ceiling bounds the wait and this bounds the whole gap.
        assert!(
            *gap <= config.retry.ceiling(attempt) + Duration::from_secs(1),
            "attempt {attempt} waited {gap:?}, far past its ceiling {:?}",
            config.retry.ceiling(attempt)
        );
        assert!(
            *gap + Duration::from_millis(1) >= expected,
            "attempt {attempt} waited {gap:?}, less than the policy's {expected:?}"
        );
    }
    let total: Duration = gaps.iter().sum();
    println!("five refusals produced {total:?} of backoff in {gaps:?}");
    assert!(
        total >= Duration::from_millis(20),
        "the retries were a hot loop: {total:?} of backoff"
    );
    engine.stop();
}

#[tokio::test]
async fn a_415_is_one_permanent_failure_and_is_never_retried() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "photo.bin", b"unsupported bytes");
    add_folder(&store, &photos, true);
    server.fail(Operation::Upload, Fault::UnsupportedFormat);
    let engine = start_engine(&server, &store, retry_config(8)).await;

    let path_text = file.display().to_string();
    assert!(
        wait_until(
            || matches!(&store.item(&path_text), Ok(Some(row)) if row.state == ItemState::FailedPermanent),
            Duration::from_secs(10)
        )
        .await,
        "the 415 did not park the item as a permanent failure"
    );
    // R6.3: a permanent refusal is not retried, so nothing else happens.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(server.upload_requests(), 1, "a 415 is never retried");
    let attempts = store.attempts(&path_text).expect("read the attempt log");
    assert_eq!(attempts.len(), 1, "exactly one attempt was recorded");
    assert_eq!(
        attempts[0].error_class,
        Some(FailureClass::UnsupportedFormat)
    );
    let failed = engine.failed_items().expect("read the failed items");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].state, ItemState::FailedPermanent);
    assert_eq!(failed[0].error_class, Some(FailureClass::UnsupportedFormat));
    assert!(
        failed[0]
            .error_message
            .as_deref()
            .is_some_and(|message| !message.is_empty()),
        "R6.5: the view shows the last error"
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
async fn a_401_re_logs_in_once_and_the_retried_upload_succeeds() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "photo.jpg", b"photo bytes");
    add_folder(&store, &photos, true);
    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 401,
            retry_after: None,
        },
    );
    let (engine, session) = start_engine_with_session(&server, &store, retry_config(8)).await;

    assert!(
        wait_for_progress(
            || server.upload_requests() + server.login_requests(),
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(10),
        )
        .await,
        "R1.4: the upload was not retried after the one re-login"
    );
    assert_eq!(
        server.upload_requests(),
        2,
        "the upload was tried once more"
    );
    assert_eq!(
        server.login_requests(),
        2,
        "R1.4: exactly one re-login after the 401"
    );
    assert!(
        session.state().is_connected(),
        "the connection stayed usable: {:?}",
        session.state()
    );
    let attempts = store
        .attempts(&file.display().to_string())
        .expect("read the attempt log");
    assert_eq!(
        attempts.len(),
        1,
        "the re-login and its retry are one upload attempt"
    );
    assert_eq!(attempts[0].outcome, AttemptOutcome::Succeeded);
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

/// R1.4's attention run is its own, shorter run, separate from the circuit
/// breaker's (R6.6): a refused batch is not an alarm by itself, and the
/// connection needs the user only once the run reaches the engine's attention
/// threshold.
#[tokio::test]
async fn a_run_of_401s_needs_attention_at_the_threshold() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let first_folder = temp.path().join("first");
    let second_folder = temp.path().join("second");
    let first = write_file(&first_folder, "first.jpg", b"first bytes");
    let second = write_file(&second_folder, "second.jpg", b"second bytes");
    add_folder(&store, &first_folder, true);
    // retry_config pins the attention run at three failures. The first item's
    // upload is refused twice with nothing succeeding in between (the one
    // re-login and the refusal), and the second item's checksum check is the
    // third failed request in the run.
    for _ in 0..2 {
        server.fail(
            Operation::Upload,
            Fault::Status {
                code: 401,
                retry_after: None,
            },
        );
    }
    let (engine, session) = start_engine_with_session(&server, &store, retry_config(8)).await;

    let first_text = first.display().to_string();
    assert!(
        wait_for_progress(
            || server.upload_requests() + server.login_requests(),
            || matches!(&store.item(&first_text), Ok(Some(row)) if row.state == ItemState::FailedPermanent),
            Duration::from_secs(10),
        )
        .await,
        "the second 401 did not park the first item"
    );
    // Two failed requests in the run: the refusal is real, but the threshold
    // is not reached, so the connection stays usable and the tray stays quiet.
    assert!(
        session.state().is_connected(),
        "a run below the threshold must not need the user: {:?}",
        session.state()
    );
    assert_eq!(
        server.upload_requests(),
        2,
        "R1.4: the first item is one re-login and one refusal"
    );
    assert_eq!(server.login_requests(), 2);

    // The second item's checksum check is the third failed request in the run,
    // so the connection needs the user now. Its folder is added after the
    // first item parked and after the first item's own check, so the refusal
    // queued here is the next check and no request of it races the assertions
    // above.
    server.fail(
        Operation::BulkUploadCheck,
        Fault::Status {
            code: 401,
            retry_after: None,
        },
    );
    engine
        .add_folder(&second_folder.display().to_string(), true)
        .expect("add the second folder");
    assert!(
        wait_for_progress(
            || server.bulk_check_requests() + server.upload_requests() + server.login_requests(),
            || matches!(
                session.state(),
                ConnectionState::NeedsAttention(Attention::Rejected)
            ),
            Duration::from_secs(10),
        )
        .await,
        "R1.4: a run of 401s needs the user: {:?}",
        session.state()
    );
    assert_eq!(
        server.upload_requests(),
        2,
        "the second item was refused at the check, before any upload"
    );
    assert_eq!(server.bulk_check_requests(), 2);
    let first_row = store
        .item(&first_text)
        .expect("read the row")
        .expect("the row exists");
    assert_eq!(first_row.state, ItemState::FailedPermanent);
    assert_eq!(first_row.error_class, Some(FailureClass::AuthFailed));
    // R1.4: nothing is wrong with the second file, so its row waits for the
    // sign-in that fixes the credential instead of being parked.
    let second_row = store
        .item(&second.display().to_string())
        .expect("read the row")
        .expect("the row exists");
    assert_eq!(second_row.state, ItemState::Pending);
    // R8.2: the tray reads the same state.
    assert_eq!(engine.status().activity, Activity::NeedsAttention);
    engine.stop();
}

/// R1.4: the default attention threshold is its own, shorter run. One refused
/// batch spends two failures, so it stays below the default of three and the
/// next scan's refusal is what reaches the user, not the five the circuit
/// breaker waits for (R6.6).
#[tokio::test]
async fn the_default_attention_threshold_is_shorter_than_the_breaker_run() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    write_file(&photos, "photo.jpg", b"photo bytes");
    add_folder(&store, &photos, true);
    // Two refusals for the first batch: the 401, the one re-login, and the
    // retried check's 401.
    for _ in 0..2 {
        server.fail(
            Operation::BulkUploadCheck,
            Fault::Status {
                code: 401,
                retry_after: None,
            },
        );
    }
    let (engine, session) =
        start_engine_with_session(&server, &store, EngineConfig::default()).await;

    assert!(
        wait_for_progress(
            || server.bulk_check_requests() + server.login_requests(),
            || server.bulk_check_requests() >= 2,
            Duration::from_secs(10),
        )
        .await,
        "the first batch was never refused"
    );
    assert!(
        session.state().is_connected(),
        "one refused batch (two failures) is below the default attention threshold: {:?}",
        session.state()
    );

    // The next scan's refusal is the third failure in the run, so it reaches
    // the user. The breaker's own run is two failures further away.
    server.fail(
        Operation::BulkUploadCheck,
        Fault::Status {
            code: 401,
            retry_after: None,
        },
    );
    engine.sync_now();
    assert!(
        wait_for_progress(
            || server.bulk_check_requests() + server.login_requests(),
            || matches!(
                session.state(),
                ConnectionState::NeedsAttention(Attention::Rejected)
            ),
            Duration::from_secs(10),
        )
        .await,
        "the third failure in the run must reach the user: {:?}",
        session.state()
    );
    assert_eq!(
        server.validate_token_requests(),
        0,
        "R6.6: the breaker's longer run must not have opened yet"
    );
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
async fn a_retry_after_header_is_honoured_as_the_wait() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "throttled.jpg", b"throttled bytes");
    add_folder(&store, &photos, true);
    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 429,
            retry_after: Some(1),
        },
    );
    // The backoff cap is far below the header's wait, so only R6.2 can explain
    // a one-second gap.
    let config = EngineConfig {
        retry: RetryPolicy {
            base: Duration::from_millis(10),
            max: Duration::from_millis(40),
            max_retry_after: Duration::from_secs(5),
            seed: Some(7),
        },
        attempt_cap: 8,
        concurrency: 1,
        watch: false,
        ..EngineConfig::default()
    };
    let engine = start_engine(&server, &store, config).await;

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(20)
        )
        .await,
        "the throttled upload never finished"
    );
    let gaps = attempt_gaps(&store, &file);
    assert_eq!(gaps.len(), 1, "one refusal and one success");
    assert!(
        gaps[0] >= Duration::from_secs(1),
        "R6.2: the server's Retry-After was not honoured: {:?}",
        gaps[0]
    );
    // The policy caps what a header can buy (backoff.rs), so the only bound
    // worth making here is the load-sensitive one: a wait far past the header
    // would mean the header was ignored for the policy's own wait.
    println!("a Retry-After of 1s produced a {:?} wait", gaps[0]);
    engine.stop();
}

#[tokio::test]
async fn the_attempt_cap_parks_an_item_and_a_manual_retry_tries_again() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "stuck.jpg", b"stuck bytes");
    add_folder(&store, &photos, true);
    for _ in 0..3 {
        server.fail(
            Operation::Upload,
            Fault::Status {
                code: 503,
                retry_after: None,
            },
        );
    }
    let engine = start_engine(&server, &store, retry_config(3)).await;

    assert!(
        wait_until(|| is_dead_letter(&store, &file), Duration::from_secs(15)).await,
        "R6.5: the item never reached the dead letter"
    );
    assert_eq!(
        server.upload_requests(),
        3,
        "R6.1: the attempt cap is exactly three attempts"
    );
    let path_text = file.display().to_string();
    let row = store
        .item(&path_text)
        .expect("read")
        .expect("the row exists");
    assert_eq!(row.attempts, 3, "R6.7: the spent attempts are persisted");
    assert_eq!(row.error_class, Some(FailureClass::ServerTransient));
    let failed = engine.failed_items().expect("read the failed items");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].state, ItemState::DeadLetter);
    assert_eq!(failed[0].attempts, 3);
    assert_eq!(failed[0].error_class, Some(FailureClass::ServerTransient));
    assert!(
        failed[0]
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("503")),
        "R6.5: the view lists the last error: {:?}",
        failed[0].error_message
    );

    // R6.5: the manual retry re-attempts the item, and the server now answers.
    assert!(engine.retry_failed(&path_text).expect("retry the item"));
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(15)
        )
        .await,
        "the manual retry never uploaded"
    );
    assert_eq!(server.upload_requests(), 4);
    assert_eq!(
        store
            .item(&path_text)
            .expect("read")
            .expect("the row exists")
            .attempts,
        1,
        "the manual retry granted a fresh budget"
    );
    assert!(
        engine
            .failed_items()
            .expect("read the failed items")
            .is_empty(),
        "a recovered item leaves the failed-items view"
    );
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
async fn attempts_and_the_dead_letter_survive_a_restart_without_re_uploading() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let stuck = write_file(&photos, "stuck.jpg", b"stuck bytes");
    add_folder(&store, &photos, true);
    for _ in 0..2 {
        server.fail(
            Operation::Upload,
            Fault::Status {
                code: 503,
                retry_after: None,
            },
        );
    }
    let config = retry_config(2);
    let engine = start_engine(&server, &store, config.clone()).await;

    assert!(
        wait_until(|| is_dead_letter(&store, &stuck), Duration::from_secs(15)).await,
        "the first run did not park the item"
    );
    assert_eq!(server.upload_requests(), 2, "both attempts were refused");
    let before = store
        .attempts(&stuck.display().to_string())
        .expect("read the attempt log");
    engine.stop();
    drop(engine);
    drop(store);

    // R6.7: the client restarts with a new file to upload. The parked item is
    // not re-uploaded, and the attempt count is where the first run left it.
    let fine = write_file(&photos, "fine.jpg", b"fine bytes");
    let reopened = store_in(&temp);
    let engine = start_engine(&server, &reopened, config).await;
    assert!(
        wait_until(
            || all_terminal(&reopened, std::slice::from_ref(&fine)),
            Duration::from_secs(15)
        )
        .await,
        "the new file did not upload after the restart"
    );
    assert_eq!(
        server.upload_requests(),
        3,
        "R6.7: a restart must not re-upload a parked item"
    );
    let path_text = stuck.display().to_string();
    let row = reopened
        .item(&path_text)
        .expect("read")
        .expect("the row survived");
    assert_eq!(row.state, ItemState::DeadLetter);
    assert_eq!(row.attempts, 2, "the attempt count survived the restart");
    assert_eq!(
        reopened.attempts(&path_text).expect("read the attempt log"),
        before,
        "SPEC §5: the attempt log survived the restart"
    );

    // The manual retry works after the reopen too.
    assert!(engine.retry_failed(&path_text).expect("retry the item"));
    assert!(
        wait_until(
            || all_terminal(&reopened, std::slice::from_ref(&stuck)),
            Duration::from_secs(15)
        )
        .await,
        "the manual retry after the restart never uploaded"
    );
    assert_eq!(server.upload_requests(), 4);
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
    let session = signed_in(&server).await;
    let credential = session
        .credential()
        .expect("the test signed in, so the session holds a credential");
    let existing = session
        .client()
        .upload_asset(
            credential.auth(),
            &immich_client::AssetUpload {
                path: &file,
                checksum: &sha1_file(&file).expect("hash the fixture").base64(),
                created_at: std::time::UNIX_EPOCH,
                modified_at: std::time::UNIX_EPOCH,
                sidecar: None,
                live_photo_video_id: None,
                bandwidth: BandwidthLimit::default(),
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

/// R6.6: one dropped connection is not a run. The retry is held open on the
/// server, so the run of one failure can be read before anything succeeds.
#[tokio::test]
async fn one_transport_failure_does_not_need_attention() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    write_file(&photos, "offline.jpg", b"offline bytes");
    add_folder(&store, &photos, true);
    server.fail(Operation::Upload, Fault::Disconnect);
    // The retry reaches the server and is held there, so the test can read the
    // connection while the run is one failure long.
    server.fail(Operation::Upload, Fault::Slow(Duration::from_secs(30)));
    let (engine, session) = start_engine_with_session(&server, &store, retry_config(8)).await;

    assert!(
        wait_for_progress(
            || server.upload_requests(),
            || server.upload_requests() >= 2,
            Duration::from_secs(10),
        )
        .await,
        "the retry never reached the server"
    );
    assert!(
        session.state().is_connected(),
        "one transport failure must not need the user: {:?}",
        session.state()
    );
    engine.stop();
}

/// R6.6, R8.2: a run of dropped connections reaches the threshold, and the
/// connection needs the user with the transport reason.
#[tokio::test]
async fn a_run_of_transport_failures_needs_attention_with_the_reason() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    write_file(&photos, "offline.jpg", b"offline bytes");
    add_folder(&store, &photos, true);
    // Three dropped uploads reach the threshold; every probe after that fails
    // too, so the connection stays in attention while the test reads it.
    for _ in 0..3 {
        server.fail(Operation::Upload, Fault::Disconnect);
    }
    for _ in 0..40 {
        server.fail(
            Operation::ValidateToken,
            Fault::Status {
                code: 503,
                retry_after: None,
            },
        );
    }
    let (engine, session) = start_engine_with_session(&server, &store, retry_config(8)).await;

    assert!(
        wait_for_progress(
            || server.upload_requests() + server.validate_token_requests(),
            || matches!(
                session.state(),
                ConnectionState::NeedsAttention(Attention::Unreachable(_))
            ),
            Duration::from_secs(10),
        )
        .await,
        "the run did not reach attention: {:?}",
        session.state()
    );
    assert_eq!(
        engine.status().activity,
        Activity::NeedsAttention,
        "R8.2: the tray reads the run as needing attention"
    );
    let ConnectionState::NeedsAttention(Attention::Unreachable(detail)) = session.state() else {
        unreachable!("the wait above matched the transport attention")
    };
    assert!(!detail.is_empty(), "the reason names the transport failure");
    engine.stop();
}

#[tokio::test]
async fn a_run_of_connection_failures_trips_the_breaker_and_it_probes_before_resuming() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "offline.jpg", b"offline bytes");
    add_folder(&store, &photos, true);
    // The server is gone: every upload drops the connection, and every probe
    // fails too, so the queue cannot resume on its own.
    for _ in 0..40 {
        server.fail(Operation::Upload, Fault::Disconnect);
    }
    for _ in 0..40 {
        server.fail(
            Operation::ValidateToken,
            Fault::Status {
                code: 503,
                retry_after: None,
            },
        );
    }
    let engine = start_engine(&server, &store, retry_config(8)).await;

    assert!(
        wait_until(
            || server.validate_token_requests() >= 1,
            Duration::from_secs(10)
        )
        .await,
        "R6.6: the breaker never probed the credential-validation endpoint"
    );
    let while_open = server.upload_requests();
    assert!(
        while_open <= 3,
        "R6.6: the breaker did not stop the queue after its threshold: {while_open} uploads"
    );
    // The probe keeps failing, so no upload may go out while it does.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        server.upload_requests(),
        while_open,
        "R6.6: an upload went out while the breaker was open"
    );

    // The server comes back: the next probe succeeds and the queue resumes.
    server.reset_faults();
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(20)
        )
        .await,
        "R6.6: the queue did not resume after the probe succeeded"
    );
    assert!(
        server.validate_token_requests() >= 2,
        "the resume was gated by another probe"
    );
    assert!(server.upload_requests() > while_open, "the uploads resumed");
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
async fn a_revoked_api_key_parks_the_item_instead_of_probing_forever() {
    let server = MockImmich::start().await;
    let key = "api-key-abc123";
    server.set_api_key(key);
    let session = signed_in_with(&server, AuthMethod::ApiKey, key).await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "photo.jpg", b"photo bytes");
    add_folder(&store, &photos, true);
    // The server drops the uploads, so the breaker opens and the queue waits
    // for the probe. The key is revoked, so that probe is a refusal no amount
    // of waiting can turn into an answer.
    for _ in 0..4 {
        server.fail(Operation::Upload, Fault::Disconnect);
    }
    server.set_api_key("a-rotated-key");
    let engine = start_engine_over(&session, &store, retry_config(8));

    let path_text = file.display().to_string();
    assert!(
        wait_until(
            || matches!(&store.item(&path_text), Ok(Some(row)) if row.state == ItemState::FailedPermanent),
            Duration::from_secs(20)
        )
        .await,
        "a refused probe did not park the item: {:?}",
        store.item(&path_text)
    );
    let row = store
        .item(&path_text)
        .expect("read")
        .expect("the row exists");
    assert_eq!(
        row.error_class,
        Some(FailureClass::AuthFailed),
        "R1.5: a refused API key is the account's problem, not the file's"
    );
    // R6.6: the probe loop stops. Waiting for a server that refuses the key
    // again is a loop with no end, so the item needs the user instead.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let probes = server.validate_token_requests();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        server.validate_token_requests(),
        probes,
        "the refused key is still being probed on every cooldown"
    );
    assert_eq!(
        engine.status().activity,
        Activity::NeedsAttention,
        "R1.4: the tray reads the refused key as needing attention"
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

#[tokio::test]
async fn a_refused_probe_mirrors_the_refusal_and_leaves_the_batch_pending() {
    let server = MockImmich::start().await;
    let key = "api-key-abc123";
    server.set_api_key(key);
    let session = signed_in_with(&server, AuthMethod::ApiKey, key).await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let files = [
        write_file(&photos, "one.jpg", b"one bytes"),
        write_file(&photos, "two.jpg", b"two bytes"),
    ];
    add_folder(&store, &photos, true);
    // The server drops the checksum checks, so the breaker opens and the queue
    // waits for the probe. The key is then rotated, so the probe is a refusal
    // no wait can turn into an answer (R1.4).
    for _ in 0..3 {
        server.fail(Operation::BulkUploadCheck, Fault::Disconnect);
    }
    server.set_api_key("a-rotated-key");
    let engine = start_engine_over(&session, &store, retry_config(8));

    assert!(
        wait_until(
            || matches!(session.state(), ConnectionState::NeedsAttention(_)),
            Duration::from_secs(20)
        )
        .await,
        "R1.4: the refused probe left the session reading {:?}",
        session.state()
    );
    // R1.4: the refusal the probe learned is the connection's answer, so the
    // snapshot the tray reads must not keep saying Connected.
    assert_eq!(
        engine.status().activity,
        Activity::NeedsAttention,
        "R1.4: the tray reads a refused credential as needing attention"
    );

    // R1.4: nothing about the two files is wrong, so the batch is not
    // condemned. The rows stay pending for the re-login that fixes it.
    for file in &files {
        let row = store
            .item(&file.display().to_string())
            .expect("read")
            .expect("the row exists");
        assert_eq!(
            row.state,
            ItemState::Pending,
            "R1.4: a refused key must not park {:?} permanently",
            file.display()
        );
    }
    assert_eq!(
        engine.failed_items().expect("read the failed items").len(),
        0,
        "R1.4: a refused key is the account's problem, not a dead letter"
    );
    engine.stop();
}

#[tokio::test]
async fn a_batch_during_a_dead_session_stays_pending_until_the_user_signs_in_again() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let files = [
        write_file(&photos, "first.jpg", b"first bytes"),
        write_file(&photos, "second.jpg", b"second bytes"),
    ];
    add_folder(&store, &photos, true);
    // The checksum check is refused: the first 401 re-logs in, the second is
    // the first refusal in the run. The attention threshold retry_config pins
    // is three, so a later scan's refusal is the one that reaches the user
    // (R1.4, R6.6).
    for _ in 0..3 {
        server.fail(
            Operation::BulkUploadCheck,
            Fault::Status {
                code: 401,
                retry_after: None,
            },
        );
    }
    let (engine, session) = start_engine_with_session(&server, &store, retry_config(8)).await;

    // The first refused batch leaves its files pending for the next scan.
    assert!(
        wait_until(
            || server.bulk_check_requests() >= 2,
            Duration::from_secs(20)
        )
        .await,
        "the first batch was never refused"
    );
    engine.sync_now();
    assert!(
        wait_until(
            || matches!(session.state(), ConnectionState::NeedsAttention(_)),
            Duration::from_secs(20)
        )
        .await,
        "R1.4: the run of 401s did not leave the connection needing the user"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    for file in &files {
        let row = store
            .item(&file.display().to_string())
            .expect("read")
            .expect("the row exists");
        assert_eq!(
            row.state,
            ItemState::Pending,
            "R1.4: a refused session must not park {:?} permanently; it is still the user's to fix",
            file.display()
        );
    }
    assert_eq!(
        server.upload_requests(),
        0,
        "R1.4: nothing was uploaded, because nothing could authenticate"
    );
    assert_eq!(
        engine.failed_items().expect("read the failed items").len(),
        0
    );

    // R1.4: signing in again uploads what the refused batch left pending.
    let state = session
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        })
        .await;
    assert!(state.is_connected(), "{state:?}");
    engine.sync_now();

    assert!(
        wait_until(|| all_terminal(&store, &files), Duration::from_secs(20)).await,
        "R1.4: the re-login did not resume the queue"
    );
    assert_eq!(server.upload_requests(), 2);
    engine.stop();
}

#[tokio::test]
async fn a_success_between_two_connection_failures_is_not_a_run() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let files = [
        write_file(&photos, "one.jpg", b"first bytes"),
        write_file(&photos, "two.jpg", b"second bytes"),
    ];
    add_folder(&store, &photos, true);
    // One file per batch, so the first batch takes the first two failures and
    // the second batch takes the third.
    let config = EngineConfig {
        batch_size: 1,
        ..retry_config(8)
    };
    // Three dropped connections, and between them a request the server
    // answers: a success ends the run of failures the breaker counts (R6.6).
    server.fail(Operation::BulkUploadCheck, Fault::Disconnect);
    server.fail(Operation::Upload, Fault::Disconnect);
    server.fail(Operation::BulkUploadCheck, Fault::Disconnect);
    let (engine, session) = start_engine_with_session(&server, &store, config).await;

    assert!(
        wait_until(|| all_terminal(&store, &files), Duration::from_secs(20)).await,
        "the retried uploads never finished: {:?}",
        engine.status().counts
    );
    assert_eq!(
        server.validate_token_requests(),
        0,
        "R6.6: three failures that were never a run still tripped the breaker"
    );
    assert!(
        session.state().is_connected(),
        "the successes between the failures ended the run: {:?}",
        session.state()
    );
    assert_ne!(
        engine.status().activity,
        Activity::NeedsAttention,
        "the tray must not need attention for failures no success left in a run"
    );
    engine.stop();
}

#[tokio::test]
async fn a_restart_requeues_a_retry_wait_row_with_its_attempt_count() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "parked.jpg", b"parked bytes");
    add_folder(&store, &photos, true);
    let path_text = file.display().to_string();
    // The row a previous run parked after one attempt (R6.7).
    store
        .record(&ItemRecord {
            path: path_text.clone(),
            size: std::fs::metadata(&file)
                .expect("read the fixture metadata")
                .len(),
            mtime_ns: mtime_ns_of(&file),
            sha1: sha1_file(&file).expect("hash the fixture"),
            asset_id: None,
            state: ItemState::RetryWait,
            error_class: Some(FailureClass::ServerTransient),
            error_message: Some("the server answered 503".to_string()),
            attempts: 0,
            // R3.3: nothing has been sent, so the sidecar is still owed.
            sidecar: None,
        })
        .expect("write the parked row");
    store
        .record_attempt(&AttemptRecord {
            path: path_text.clone(),
            started_ns: 1_700_000_000_000_000_000,
            outcome: AttemptOutcome::Failed,
            error_class: Some(FailureClass::ServerTransient),
        })
        .expect("record the spent attempt");
    assert_eq!(
        store
            .item(&path_text)
            .expect("read")
            .expect("the row exists")
            .attempts,
        1
    );
    drop(store);

    // R6.7: the restart re-queues the parked row with the count it had, so the
    // cap of two leaves exactly one more attempt.
    let reopened = store_in(&temp);
    let engine = start_engine(&server, &reopened, retry_config(2)).await;

    assert!(
        wait_until(
            || all_terminal(&reopened, std::slice::from_ref(&file)),
            Duration::from_secs(20)
        )
        .await,
        "R6.7: the restart did not resume the parked row: {:?}",
        reopened.item(&path_text)
    );
    assert_eq!(
        server.upload_requests(),
        1,
        "R6.7: the spent attempt was not counted, so the item had a fresh budget"
    );
    assert_eq!(
        reopened
            .item(&path_text)
            .expect("read")
            .expect("the row exists")
            .attempts,
        2,
        "R6.7: the count continued across the restart"
    );
    engine.stop();
}

/// The version chain of one item, oldest first (R4.4).
fn chain(store: &StateStore, file: &Path) -> Vec<sync_core::store::Version> {
    store
        .versions(&file.display().to_string())
        .expect("read the version chain")
}

/// The number of recorded versions of one item.
fn chain_len(store: &StateStore, file: &Path) -> usize {
    chain(store, file).len()
}

/// The asset id one item currently points at.
fn asset_of(store: &StateStore, file: &Path) -> String {
    store
        .item(&file.display().to_string())
        .expect("read the row")
        .expect("the row exists")
        .asset_id
        .expect("a recorded asset id (R3.6)")
}

/// The state one item is in.
fn state_of(store: &StateStore, file: &Path) -> ItemState {
    store
        .item(&file.display().to_string())
        .expect("read the row")
        .expect("the row exists")
        .state
}

/// A client for the tests that set the server up themselves, the way the app
/// does. The double accepts any non-empty API key.
fn api(server: &MockImmich) -> sync_core::ImmichClient {
    sync_core::ImmichClient::new(server.base_url()).expect("the mock URL is valid")
}

/// A tag id, in the UUID form Immich uses.
fn tag_id(n: u64) -> String {
    format!("00000000-0000-4000-8000-0000000000{n:02}")
}

/// The asset id of one asset uploaded straight through the API, for the tests
/// that need a second asset on the server.
async fn other_asset_id(server: &MockImmich) -> String {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("other.jpg");
    std::fs::write(&path, b"other bytes").expect("write the fixture");
    let sha1 = sha1_file(&path).expect("hash the fixture");
    api(server)
        .upload_asset(
            immich_client::Auth::ApiKey("test-key"),
            &immich_client::AssetUpload {
                path: &path,
                checksum: &sha1.base64(),
                created_at: std::time::UNIX_EPOCH,
                modified_at: std::time::UNIX_EPOCH,
                sidecar: None,
                live_photo_video_id: None,
                bandwidth: BandwidthLimit::default(),
            },
        )
        .await
        .expect("the double accepts the upload")
        .asset_id
}

/// Waits for `file`'s first upload, edits it in place, and waits for the edit to
/// become a second version. Hands back the first asset id.
///
/// The file must already hold `first_bytes` when the engine starts, so the
/// startup scan finds it (R2.3).
async fn sync_twice(
    store: &Arc<StateStore>,
    engine: &Arc<Engine>,
    file: &Path,
    second_bytes: &[u8],
) -> String {
    assert!(
        wait_until(
            || all_terminal(store, std::slice::from_ref(&file.to_path_buf())),
            Duration::from_secs(20)
        )
        .await,
        "the first upload did not finish"
    );
    let first = asset_of(store, file);

    std::fs::write(file, second_bytes).expect("edit the file in place");
    engine.sync_now();
    assert!(
        wait_until(
            || chain_len(store, file) == 2 && asset_of(store, file) != first,
            Duration::from_secs(20)
        )
        .await,
        "the edit did not become a second version: {:?}",
        chain(store, file)
    );
    first
}

/// R4.1, R4.2, R4.3, R4.4: an edited file keeps its sync item, uploads as a new
/// asset, copies its associations, and both versions end in one stack with the
/// new asset primary. The old version stays recorded and retrievable.
#[tokio::test]
async fn an_edited_file_becomes_a_new_version_beside_the_old_one() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "photo.jpg", b"first bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, retry_config(4)).await;

    let old = sync_twice(&store, &engine, &file, b"second bytes").await;
    let new = asset_of(&store, &file);

    // R4.1: one item, one path, one identity. The chain holds both versions
    // under the same `sync_item.id`.
    let versions = chain(&store, &file);
    assert_eq!(versions.len(), 2, "R4.4: both versions are recorded");
    assert_eq!(
        versions[0].item_id, versions[1].item_id,
        "R4.1: the edited file keeps its sync item identity"
    );
    assert_eq!(
        versions
            .iter()
            .map(|version| version.asset_id.clone())
            .collect::<Vec<_>>(),
        vec![old.clone(), new.clone()],
        "R4.4: the chain keeps the old asset, so the old version stays retrievable"
    );
    assert_eq!(
        versions[0].sha1,
        sync_core::hash::sha1(b"first bytes"),
        "each version keeps the checksum of its own content (SPEC §5)"
    );
    assert_eq!(
        versions[1].sha1,
        sha1_file(&file).expect("hash the edited file")
    );
    assert!(
        !versions[0].is_current && versions[1].is_current,
        "R4.4: the current flag points at the newest version, and only there"
    );

    // R4.2: the copy ran with the flags the spec names, old as source and new as
    // target. The mock records the call, so this asserts the request itself and
    // not only the outcome.
    let copies = server.copy_calls();
    assert_eq!(copies.len(), 1, "R4.2: one copy for one edited file");
    let copy = &copies[0];
    assert_eq!(copy.source_id, old, "R4.2: the old asset is the source");
    assert_eq!(copy.target_id, new, "R4.2: the new asset is the target");
    assert!(
        copy.albums && copy.shared_links && copy.favorite && copy.sidecar,
        "R4.2: albums, shared links, favorite, and the sidecar carry over"
    );
    assert!(copy.stack, "R4.3: stack membership carries over");

    // R4.3: the old asset was in no stack, so one stack was made with the new
    // asset first.
    let stacks = server.stack_calls();
    assert_eq!(stacks.len(), 1, "R4.3: one stack for one version chain");
    assert_eq!(
        stacks[0].asset_ids,
        vec![new.clone(), old.clone()],
        "R4.3: the new asset is first, so it is the primary"
    );
    assert_eq!(
        store
            .stack_id(&file.display().to_string())
            .expect("read the stack id"),
        Some(stacks[0].stack_id.clone()),
        "R4.4: the state database records the stack id"
    );
    engine.stop();
}

/// R4.2: the old asset's tags are applied to the new asset, and the old version
/// keeps its own.
#[tokio::test]
async fn the_old_version_tags_carry_over_to_the_new_one() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "tagged.jpg", b"first bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, retry_config(4)).await;

    sync_twice(&store, &engine, &file, b"second bytes").await;
    let first = chain(&store, &file)[0].asset_id.clone();
    // The user tags the version that is current now, then edits the file, so
    // the tagged asset is the one this version replaces (R4.2).
    let current = asset_of(&store, &file);
    let tags = vec![tag_id(1), tag_id(2)];
    api(&server)
        .bulk_tag_assets(
            immich_client::Auth::ApiKey("test-key"),
            &tags,
            std::slice::from_ref(&current),
        )
        .await
        .expect("the double tags the asset");
    assert_eq!(server.tags_of(&current), tags);

    std::fs::write(&file, b"third bytes, edited again").expect("edit the file again");
    engine.sync_now();
    assert!(
        wait_until(|| chain_len(&store, &file) == 3, Duration::from_secs(20)).await,
        "R4.2: the third version did not upload: {:?}",
        chain(&store, &file)
    );
    let newest = asset_of(&store, &file);

    assert_ne!(newest, current);
    assert_eq!(
        server.tags_of(&newest),
        tags,
        "R4.2: the new version carries the tags of the version it replaces"
    );
    assert_eq!(
        server.tags_of(&current),
        tags,
        "R4.4: the old version keeps its tags; tags are copied, not moved"
    );
    assert_eq!(
        server.tags_of(&first),
        Vec::<String>::new(),
        "a version takes the tags of the version it replaces, not of one further back"
    );
    engine.stop();
}

/// R4.3: an old asset that is already in a stack keeps that stack, so the copy
/// alone moves the membership and no second stack is made.
#[tokio::test]
async fn an_old_version_that_is_already_stacked_joins_no_new_stack() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "stacked.jpg", b"first bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, retry_config(4)).await;

    sync_twice(&store, &engine, &file, b"second bytes").await;
    // The user stacks the version that is current now, so the version this edit
    // replaces is already in a stack (R4.3).
    let current = asset_of(&store, &file);
    let other = other_asset_id(&server).await;
    let existing = api(&server)
        .create_stack(
            immich_client::Auth::ApiKey("test-key"),
            &[current.clone(), other],
        )
        .await
        .expect("the user stacked the current version by hand");
    // One stack from the version flow above, one from the test.
    let stacks_before = server.stack_calls().len();

    std::fs::write(&file, b"third bytes, edited again").expect("edit the file again");
    engine.sync_now();
    assert!(
        wait_until(|| chain_len(&store, &file) == 3, Duration::from_secs(20)).await,
        "R4.3: the third version did not finish: {:?}",
        chain(&store, &file)
    );
    let newest = asset_of(&store, &file);

    assert_eq!(
        server.stack_calls().len(),
        stacks_before,
        "R4.3: the old version was already stacked, so the client asked for no new stack"
    );
    assert_eq!(
        store
            .stack_id(&file.display().to_string())
            .expect("read the stack id"),
        Some(existing.clone()),
        "R4.3: the item keeps the stack its versions were already in"
    );
    let members = server
        .stack_of(&newest)
        .expect("the copy moved the new asset into the existing stack")
        .asset_ids;
    assert!(
        members.contains(&current) && members.contains(&newest),
        "R4.3: the copy moved the new asset into the existing stack: {members:?}"
    );
    assert_eq!(
        server.stack_of(&newest).map(|stack| stack.stack_id),
        Some(existing),
        "R4.3: the stack is the one that already existed"
    );
    engine.stop();
}

/// R4.4 and R6.3: a refused copy parks the item permanently, keeps the uploaded
/// asset, and records no version the server did not make.
#[tokio::test]
async fn a_refused_copy_keeps_the_upload_and_records_no_new_version() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "refused.jpg", b"first bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, retry_config(4)).await;

    sync_twice(&store, &engine, &file, b"second bytes").await;
    let before = chain(&store, &file);
    let old = asset_of(&store, &file);
    // The credential lacks `asset.copy`, which no retry can fix (R6.3).
    server.fail(Operation::Copy, Fault::PermissionDenied);
    std::fs::write(&file, b"third bytes, edited again").expect("edit the file again");
    engine.sync_now();

    assert!(
        wait_until(
            || state_of(&store, &file) == ItemState::FailedPermanent,
            Duration::from_secs(20)
        )
        .await,
        "R6.3: the refused copy did not park the item: {:?}",
        store.item(&file.display().to_string())
    );
    let row = store
        .item(&file.display().to_string())
        .expect("read")
        .expect("the row exists");
    let uploaded = row.asset_id.expect("the uploaded asset stays recorded");
    assert_ne!(
        uploaded, old,
        "the new bytes are on the server, so the asset id must not go back"
    );
    assert_eq!(
        row.error_class,
        Some(FailureClass::PermissionDenied),
        "R6.4: the failure class is the one the refused call has"
    );
    assert_eq!(
        chain(&store, &file),
        before,
        "R4.4: no version was recorded, so the chain still points at the version \
         that really is current: {:?}",
        chain(&store, &file)
    );
    assert!(
        !chain(&store, &file)
            .iter()
            .any(|version| version.asset_id == uploaded),
        "R4.4: the store claims no version relationship the server does not have"
    );
    assert_eq!(
        chain(&store, &file)
            .iter()
            .filter(|version| version.is_current)
            .map(|version| version.asset_id.clone())
            .collect::<Vec<_>>(),
        vec![old],
        "R4.4: the current version is the one the server has complete"
    );
    assert_eq!(
        store
            .failed_items()
            .expect("read the failed items")
            .iter()
            .map(|item| item.path.clone())
            .collect::<Vec<_>>(),
        vec![file.display().to_string()],
        "R6.5: the item is in the failed-items view for a manual retry"
    );
    engine.stop();
}

/// A promote failure must keep the sidecar the upload already sent (R3.3,
/// R4.5).
///
/// The upload that creates the new version carries the changed sidecar and
/// records its digest before the version flow runs. When a promote call then
/// fails, the row is parked for a retry; if the digest is erased, the retry
/// offers the sidecar again, and the checksum header answers `duplicate`
/// before the body is read, so the item parks with a false "could not be
/// attached" although the asset already carries it.
#[tokio::test]
async fn a_promote_failure_keeps_the_sidecar_the_upload_already_sent() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "versioned.jpg", b"first bytes");
    let sidecar = write_file(
        &photos,
        "versioned.jpg.xmp",
        b"<x:xmpmeta>first</x:xmpmeta>",
    );
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, retry_config(4)).await;

    sync_one(&store, &file).await;
    let first = store
        .item(&file.display().to_string())
        .expect("read the row")
        .expect("the row exists");
    assert!(
        first.sidecar.is_some(),
        "the first upload carried the sidecar"
    );

    // One promote call fails after the changed upload landed with its new
    // sidecar; the automatic retry must finish without offering it again.
    server.fail(
        Operation::GetAsset,
        Fault::Status {
            code: 500,
            retry_after: None,
        },
    );
    std::fs::write(&file, b"second bytes").expect("edit the media");
    std::fs::write(&sidecar, b"<x:xmpmeta>second</x:xmpmeta>").expect("edit the sidecar");
    engine.sync_now();

    let path_text = file.display().to_string();
    // Waiting on the version chain, not only a terminal row: the first sync
    // already left a terminal row, and the promote is what the retry has to
    // finish.
    assert!(
        wait_until(
            || chain_len(&store, &file) == 2
                && matches!(&store.item(&path_text), Ok(Some(row)) if row.state.is_terminal_success()),
            Duration::from_secs(20)
        )
        .await,
        "the promote failure left the item parked instead of retrying: {:?}",
        store.item(&path_text)
    );
    let row = store
        .item(&path_text)
        .expect("read the row")
        .expect("the row exists");
    assert_eq!(
        row.sidecar.map(|digest| digest.sha1),
        Some(sha1_file(&sidecar).expect("hash the sidecar")),
        "the row keeps the sidecar digest the upload recorded"
    );
    assert!(
        engine
            .failed_items()
            .expect("read the failed items")
            .is_empty(),
        "the false 'could not be attached' must not park the item"
    );

    // A later pass must not offer the recorded sidecar again.
    let uploads = server.upload_requests();
    engine.sync_now();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        server.upload_requests(),
        uploads,
        "R4.5: a recorded sidecar is not sent again"
    );
    engine.stop();
}

/// R6.5 and R4.4: the dead letter keeps the asset the server already holds,
/// the same rule `park_row` follows when a promote fails — the upload made the
/// mapping, and dropping it claims the server holds nothing.
#[tokio::test]
async fn the_dead_letter_keeps_an_asset_the_upload_already_made() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "parked-after-upload.jpg", b"first bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, retry_config(4)).await;

    sync_twice(&store, &engine, &file, b"second bytes").await;
    let before = chain(&store, &file);
    // Every copy answers 503 until the attempt cap parks the item. The faults
    // are queued one per request, so the promote fails on each attempt until
    // the cap is spent — each one after the upload has landed.
    for _ in 0..4 {
        server.fail(
            Operation::Copy,
            Fault::Status {
                code: 503,
                retry_after: None,
            },
        );
    }
    std::fs::write(&file, b"third bytes, edited again").expect("edit the file again");
    engine.sync_now();

    assert!(
        wait_until(
            || state_of(&store, &file) == ItemState::DeadLetter,
            Duration::from_secs(30)
        )
        .await,
        "R6.5: the attempt cap did not park the item: {:?}",
        store.item(&file.display().to_string())
    );
    // The asset the edit's upload made, named by the checksum the server
    // holds — an expectation independent of the row under test.
    let uploaded = server
        .asset_id_for_checksum(&sha1_file(&file).expect("hash the edit").base64())
        .expect("the server holds the bytes the engine uploaded");
    let row = store
        .item(&file.display().to_string())
        .expect("read")
        .expect("the row exists");
    assert_eq!(
        row.asset_id.as_deref(),
        Some(uploaded.as_str()),
        "R6.5: the dead letter keeps the asset the upload already made; clearing \
         it hides the mapping the failed-items view and a manual retry need"
    );
    assert_eq!(
        chain(&store, &file),
        before,
        "R4.4: no version is recorded for an asset the flow did not finish with"
    );
    assert_eq!(
        store
            .failed_items()
            .expect("read the failed items")
            .iter()
            .map(|item| item.path.clone())
            .collect::<Vec<_>>(),
        vec![file.display().to_string()],
        "R6.5: the item is in the failed-items view"
    );
    engine.stop();
}

/// R4.4 and R6.1: a refused stack call is retried by the retry policy, and the
/// retry finishes the chain without sending the bytes twice.
///
/// The first transition is the one that creates a stack: from then on every
/// version joins the stack the copy moved it into (R4.3), so the stack call is
/// the one faulted on the first edit.
#[tokio::test]
async fn a_refused_stack_call_is_retried_and_the_chain_completes() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "stacked-late.jpg", b"first bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, retry_config(4)).await;

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(20)
        )
        .await,
        "the first upload did not finish"
    );
    let first = asset_of(&store, &file);
    let uploads_before = server.upload_requests();
    server.fail(
        Operation::CreateStack,
        Fault::Status {
            code: 503,
            retry_after: None,
        },
    );
    std::fs::write(&file, b"second bytes").expect("edit the file");
    engine.sync_now();

    assert!(
        wait_until(|| chain_len(&store, &file) == 2, Duration::from_secs(30)).await,
        "R6.1: the refused stack call was not retried: {:?}",
        chain(&store, &file)
    );
    let versions = chain(&store, &file);
    assert!(
        !versions[0].is_current && versions[1].is_current,
        "R4.4: the chain advances once, to the newest version"
    );
    assert_eq!(versions[0].asset_id, first, "R4.4: the old version stays");
    assert_eq!(
        store
            .stack_id(&file.display().to_string())
            .expect("read the stack id"),
        server
            .stack_calls()
            .last()
            .map(|stack| stack.stack_id.clone()),
        "R4.4: the retried stack is the one the item records"
    );
    assert_eq!(
        server.upload_requests(),
        uploads_before + 2,
        "R6.1: the retry sent one more request, which the server answered as a \
         duplicate from the checksum header, so the bytes did not go twice"
    );
    assert!(
        state_of(&store, &file).is_terminal_success(),
        "R6.1: the retry finished the item: {:?}",
        state_of(&store, &file)
    );
    engine.stop();
}

/// R4.4 and R6.1: a refused tag call is retried, and the tags of the version
/// being replaced land on the new one.
#[tokio::test]
async fn a_refused_tag_call_is_retried_and_the_tags_land() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "tagged-late.jpg", b"first bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, retry_config(4)).await;

    sync_twice(&store, &engine, &file, b"second bytes").await;
    let current = asset_of(&store, &file);
    let tags = vec![tag_id(7)];
    api(&server)
        .bulk_tag_assets(
            immich_client::Auth::ApiKey("test-key"),
            &tags,
            std::slice::from_ref(&current),
        )
        .await
        .expect("the double tags the asset");
    server.fail(
        Operation::TagAssets,
        Fault::Status {
            code: 503,
            retry_after: None,
        },
    );
    std::fs::write(&file, b"third bytes, edited again").expect("edit the file again");
    engine.sync_now();

    assert!(
        wait_until(
            || chain_len(&store, &file) == 3 && server.tags_of(&asset_of(&store, &file)) == tags,
            Duration::from_secs(30)
        )
        .await,
        "R6.1: the refused tag call was not retried: {:?}",
        chain(&store, &file)
    );
    assert_eq!(
        chain(&store, &file)
            .iter()
            .filter(|version| version.is_current)
            .map(|version| version.asset_id.clone())
            .collect::<Vec<_>>(),
        vec![asset_of(&store, &file)],
        "R4.4: one current version, the newest"
    );
    engine.stop();
}

/// R4.3, R4.4, R6.1: a retry after a half-finished promote must not delete the
/// stack the first attempt made.
///
/// Attempt one copies, creates the stack, and then loses the tag call, so
/// nothing is recorded. The retry finds both assets already in that stack; a
/// copy that moves stack membership again makes the server merge the stack
/// into itself and delete it (asset.service.ts `copyStack`), and the item
/// would record a stack id that names nothing. The retry must leave the stack
/// alone, and the recorded id must be a live stack holding both versions.
#[tokio::test]
async fn a_retry_after_a_partial_promote_keeps_the_recorded_stack_live() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "partial.jpg", b"first bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, retry_config(4)).await;

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(20)
        )
        .await,
        "the first upload did not finish"
    );
    let old = asset_of(&store, &file);
    // Without a tag on the old asset the tag call is skipped, and the tag call
    // is the one this test makes fail after the stack exists.
    let tags = vec![tag_id(3)];
    api(&server)
        .bulk_tag_assets(
            immich_client::Auth::ApiKey("test-key"),
            &tags,
            std::slice::from_ref(&old),
        )
        .await
        .expect("the double tags the asset");
    // One fault: the copy and the stack answer, the tag call does not, so the
    // first attempt records nothing and the retry runs the whole flow again.
    server.fail(
        Operation::TagAssets,
        Fault::Status {
            code: 503,
            retry_after: None,
        },
    );
    std::fs::write(&file, b"second bytes").expect("edit the file in place");
    engine.sync_now();

    assert!(
        wait_until(|| chain_len(&store, &file) == 2, Duration::from_secs(30)).await,
        "R6.1: the refused tag call was not retried: {:?}",
        chain(&store, &file)
    );
    let new = asset_of(&store, &file);
    assert_ne!(new, old, "the edit is a new asset");

    let recorded = store
        .stack_id(&file.display().to_string())
        .expect("read the stack id")
        .expect("the versions are stacked");
    let stack = server
        .stack_of(&new)
        .expect("the stack the first attempt made still exists after the retry");
    assert_eq!(
        stack.stack_id, recorded,
        "R4.4: the stack id the item records names a live stack"
    );
    assert!(
        stack.asset_ids.contains(&old) && stack.asset_ids.contains(&new),
        "R4.3: both versions are in the stack: {:?}",
        stack.asset_ids
    );
    let copies = server.copy_calls();
    assert_eq!(copies.len(), 2, "R6.1: one copy per attempt");
    assert!(
        !copies[1].stack,
        "R4.3: the retry must not ask the server to move stack membership the \
         target already has"
    );
    assert_eq!(
        server.tags_of(&new),
        tags,
        "R4.2: the retried tag call still lands"
    );
    engine.stop();
}

/// R4.4: the chain and the stack id survive a reopen of the state database, and
/// the current flag still points at the newest version.
#[tokio::test]
async fn the_version_chain_survives_a_store_reopen() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "kept.jpg", b"first bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, retry_config(4)).await;

    sync_twice(&store, &engine, &file, b"second bytes").await;
    let before = chain(&store, &file);
    let stack = store
        .stack_id(&file.display().to_string())
        .expect("read the stack id")
        .expect("the versions are stacked");
    engine.stop();
    drop(engine);
    drop(store);

    let reopened = store_in(&temp);
    assert_eq!(
        chain(&reopened, &file),
        before,
        "R4.4: the chain survives a store reopen"
    );
    assert_eq!(
        chain(&reopened, &file)
            .iter()
            .filter(|version| version.is_current)
            .map(|version| version.asset_id.clone())
            .collect::<Vec<_>>(),
        vec![asset_of(&reopened, &file)],
        "R4.4: the current flag points at the newest version after a reopen"
    );
    assert_eq!(
        reopened
            .stack_id(&file.display().to_string())
            .expect("read the stack id"),
        Some(stack),
        "R4.4: the stack id survives a store reopen"
    );
}

/// R4.2: bytes the server already holds are still a new version. The checksum
/// check resolves them to the asset another upload made, and the version flow
/// still runs on it.
#[tokio::test]
async fn an_edit_to_bytes_the_server_already_holds_becomes_a_version() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "elsewhere.jpg", b"first bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, retry_config(4)).await;
    sync_twice(&store, &engine, &file, b"second bytes").await;
    let old = asset_of(&store, &file);

    // The edited bytes reach the server by another route, so the check finds
    // them and answers `duplicate` instead of `accept` (R3.2).
    let elsewhere = upload_bytes(&server, b"third bytes, edited again").await;
    let uploads_before = server.upload_requests();
    std::fs::write(&file, b"third bytes, edited again").expect("edit the file again");
    engine.sync_now();

    assert!(
        wait_until(|| chain_len(&store, &file) == 3, Duration::from_secs(20)).await,
        "R4.2: the duplicate answer did not become a version: {:?}",
        chain(&store, &file)
    );
    let versions = chain(&store, &file);
    assert_eq!(
        versions[2].asset_id, elsewhere,
        "R4.2: the existing asset is the version"
    );
    assert!(versions[2].is_current, "R4.4: it is the current version");
    assert_eq!(
        server.upload_requests(),
        uploads_before,
        "R3.2: the bytes were not uploaded a second time"
    );
    let copies = server.copy_calls();
    let last = copies.last().expect("the copy call is recorded");
    assert_eq!(
        (last.source_id.as_str(), last.target_id.as_str()),
        (old.as_str(), elsewhere.as_str()),
        "R4.2: the associations carry onto the asset the server already had"
    );
    engine.stop();
}

/// R4.2, R4.4: the first edit of a file that existed before this upgrade
/// still copies from the asset the row already points at.
///
/// The row was written by the schema before the version table, so the chain
/// only holds what the migration seeds: `prepare` clears the item's asset id
/// before the promote runs, and the version table is the only record the
/// promote reads. Without the seed the edit uploads a second asset, records a
/// chain of one, and sends no copy — the loss ADR 0004 exists to prevent.
#[tokio::test]
async fn the_first_edit_of_a_migrated_item_copies_from_its_recorded_asset() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "migrated.jpg", b"first bytes");
    // The asset an earlier build uploaded: the server holds it, the pre-upgrade
    // row points at it.
    let old = upload_bytes(&server, b"first bytes").await;
    let metadata = std::fs::metadata(&file).expect("read the metadata");
    let mtime_ns = i64::try_from(
        metadata
            .modified()
            .expect("a modification time")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a time after the epoch")
            .as_nanos(),
    )
    .expect("the nanoseconds fit");
    // A database at the schema before this branch, holding the row that schema
    // wrote for the file.
    let db = temp.path().join("data").join("state.db");
    std::fs::create_dir_all(db.parent().expect("the db has a parent"))
        .expect("create the data directory");
    {
        let mut connection = rusqlite::Connection::open(&db).expect("open the raw database");
        sync_core::store::migrations_before_version_schema()
            .to_latest(&mut connection)
            .expect("create the fifth schema");
        connection
            .execute(
                "INSERT INTO sync_item (path, path_key, size, mtime_ns, sha1, asset_id, state, error_class)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'uploaded', NULL)",
                rusqlite::params![
                    file.display().to_string(),
                    sync_core::paths::path_key(&file),
                    i64::try_from(metadata.len()).expect("the size fits"),
                    mtime_ns,
                    sha1_file(&file).expect("hash the fixture").as_bytes().as_slice(),
                    old,
                ],
            )
            .expect("insert the pre-upgrade row");
    }
    // Opening runs this branch's migration, which seeds the chain from the row.
    let store = store_in(&temp);
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, retry_config(4)).await;

    std::fs::write(&file, b"second bytes").expect("edit the file in place");
    engine.sync_now();

    assert!(
        wait_until(|| chain_len(&store, &file) == 2, Duration::from_secs(30)).await,
        "R4.2: the first edit after the upgrade did not extend the seeded chain: {:?}",
        chain(&store, &file)
    );
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(20)
        )
        .await,
        "the edit did not finish: {:?}",
        store.item(&file.display().to_string())
    );
    let new = asset_of(&store, &file);
    let copies = server.copy_calls();
    assert_eq!(
        copies.len(),
        1,
        "R4.2: one copy for the first edit after the upgrade"
    );
    assert_eq!(
        copies[0].source_id, old,
        "R4.2: the asset the migrated row pointed at is the source"
    );
    assert_eq!(
        copies[0].target_id, new,
        "R4.2: the new asset is the target"
    );
    let versions = chain(&store, &file);
    assert_eq!(
        versions[0].asset_id, old,
        "R4.4: the chain starts at the asset the migration seeded"
    );
    assert_eq!(versions[1].asset_id, new, "R4.4: the edit is the newest");
    assert!(
        !versions[0].is_current && versions[1].is_current,
        "R4.4: the current flag moved to the edit"
    );
    assert_eq!(
        versions[0].item_id, versions[1].item_id,
        "R4.1: the migrated row and its successor are one item"
    );
    let recorded = store
        .stack_id(&file.display().to_string())
        .expect("read the stack id")
        .expect("the versions are stacked");
    assert_eq!(
        server.stack_of(&new).map(|stack| stack.stack_id),
        Some(recorded),
        "R4.4: the recorded stack id names a live stack"
    );
    engine.stop();
}

/// The asset id the server assigns to `bytes` uploaded straight through the
/// API, for a file whose bytes reach the server by another route.
async fn upload_bytes(server: &MockImmich, bytes: &[u8]) -> String {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let path = temp.path().join("uploaded.jpg");
    std::fs::write(&path, bytes).expect("write the fixture");
    let sha1 = sha1_file(&path).expect("hash the fixture");
    api(server)
        .upload_asset(
            immich_client::Auth::ApiKey("test-key"),
            &immich_client::AssetUpload {
                path: &path,
                checksum: &sha1.base64(),
                created_at: std::time::UNIX_EPOCH,
                modified_at: std::time::UNIX_EPOCH,
                sidecar: None,
                live_photo_video_id: None,
                bandwidth: BandwidthLimit::default(),
            },
        )
        .await
        .expect("the double accepts the upload")
        .asset_id
}

/// R10.2: the diagnostics throughput reads the engine's upload meter, so a
/// real upload must move it.
#[tokio::test]
async fn the_engine_reports_the_bytes_it_put_on_the_wire() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "clip.bin", &[7_u8; 8 * 1024]);
    add_folder(&store, &photos, true);

    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    assert_eq!(engine.uploaded_bytes(), 0, "nothing has been sent yet");

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&file)),
            Duration::from_secs(30)
        )
        .await,
        "the file never reached a terminal success state"
    );
    assert_eq!(
        engine.uploaded_bytes(),
        8 * 1024,
        "the meter counts every byte of the one uploaded file"
    );
    engine.stop();
}

/// An engine for the deletion review tests: the scan interval the test waits
/// out, and no watcher unless the test is about the watcher (R2.3, R5.1).
fn review_config(scan_interval: Duration, watch: bool) -> EngineConfig {
    EngineConfig {
        scan_interval,
        watch,
        watch_debounce: Duration::from_millis(100),
        ..EngineConfig::default()
    }
}

/// Syncs one file through the real pipeline and returns the asset id the
/// server assigned.
async fn sync_one(store: &Arc<StateStore>, file: &Path) -> String {
    assert!(
        wait_until(
            || all_terminal(store, &[file.to_path_buf()]),
            Duration::from_secs(30)
        )
        .await,
        "{} never reached a terminal state",
        file.display()
    );
    store
        .item(&file.display().to_string())
        .expect("read the row")
        .expect("the row exists")
        .asset_id
        .expect("R3.6: the upload recorded an asset id")
}

/// Waits until `count` files are in the review, or fails with the count it
/// last saw.
async fn wait_for_review(engine: &Arc<Engine>, count: usize) {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(usize::MAX));
    let watched = std::sync::Arc::clone(&seen);
    let reached = wait_until(
        || {
            let len = engine.review_items().map(|items| items.len()).unwrap_or(0);
            *watched.lock().expect("the review count lock") = len;
            len == count
        },
        Duration::from_secs(15),
    )
    .await;
    assert!(
        reached,
        "the review never held {count} items (last saw {})",
        seen.lock().expect("the review count lock")
    );
}

#[tokio::test]
async fn a_deleted_file_becomes_exactly_one_review_item() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "IMG_0001.jpg", b"photo bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(
        &server,
        &store,
        review_config(Duration::from_secs(1), false),
    )
    .await;
    let asset_id = sync_one(&store, &file).await;

    std::fs::remove_file(&file).expect("delete the fixture");
    wait_for_review(&engine, 1).await;

    let items = engine.review_items().expect("read the review");
    assert_eq!(items.len(), 1, "R5.1: exactly one review item per file");
    assert_eq!(items[0].path, file.display().to_string());
    assert_eq!(items[0].asset_id.as_deref(), Some(asset_id.as_str()));
    assert_eq!(
        engine.status().counts.review,
        1,
        "R5.4: the review count reaches the tray status"
    );
    assert_eq!(
        store
            .item(&file.display().to_string())
            .expect("read the row")
            .expect("the row exists")
            .state,
        ItemState::Missing,
        "R5.1: the item is in the spec's missing state"
    );

    // The scan interval is one second, so several more scans walk the same
    // disappearance. None of them may add a second row.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(
        engine.review_items().expect("read the review").len(),
        1,
        "R5.1: a later discovery pass must not add a second review item"
    );
    assert_eq!(store.review_count().expect("count the review"), 1);
    engine.stop();
}

/// A review row keeps waiting after its source folder is removed, so the
/// tray's total must count it too (R5.4): the list shows it, and a total of
/// zero is a tray that disagrees with what the user can see.
#[tokio::test]
async fn a_review_row_under_a_removed_folder_still_counts_once() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "gone.jpg", b"gone bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(
        &server,
        &store,
        review_config(Duration::from_secs(1), false),
    )
    .await;
    sync_one(&store, &file).await;

    // The file is deleted while the folder is still a source folder, so the
    // scan raises the review row; the folder is then removed, and the rows
    // under it stay in the store for the user to decide on.
    std::fs::remove_file(&file).expect("delete the fixture");
    wait_for_review(&engine, 1).await;
    engine
        .remove_folder(&photos.display().to_string())
        .expect("remove the source folder");

    let items = engine.review_items().expect("read the review");
    assert_eq!(items.len(), 1, "the removed folder's row is still listed");
    assert_eq!(
        engine.status().counts.review,
        1,
        "R5.4: the tray total must match the list, not only the current folders"
    );
    engine.stop();
}

#[tokio::test]
async fn keep_leaves_the_server_asset_and_clears_the_item() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "keep.jpg", b"keep me");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    let asset_id = sync_one(&store, &file).await;
    std::fs::remove_file(&file).expect("delete the fixture");
    wait_for_review(&engine, 1).await;

    assert!(
        engine
            .keep_review_item(&file.display().to_string())
            .expect("keep the asset"),
        "R5.3: Keep clears a missing item"
    );

    assert_eq!(engine.review_items().expect("read the review").len(), 0);
    assert_eq!(engine.status().counts.review, 0);
    assert!(
        !server.trashed(&asset_id),
        "R5.3: Keep leaves the server asset alone"
    );
    assert_eq!(
        store
            .item(&file.display().to_string())
            .expect("read the row")
            .expect("the row exists")
            .state,
        ItemState::ReviewKeep
    );
    assert!(
        !engine
            .keep_review_item(&file.display().to_string())
            .expect("keep it again"),
        "an item carries one decision"
    );
    engine.stop();
}

#[tokio::test]
async fn trash_moves_the_asset_to_the_trash_and_clears_the_item() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "trash.jpg", b"trash me");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    let asset_id = sync_one(&store, &file).await;
    std::fs::remove_file(&file).expect("delete the fixture");
    wait_for_review(&engine, 1).await;

    engine
        .trash_review_item(&file.display().to_string())
        .await
        .expect("R5.3: the asset moves to the trash");

    assert_eq!(engine.review_items().expect("read the review").len(), 0);
    assert_eq!(engine.status().counts.review, 0);
    assert!(
        server.trashed(&asset_id),
        "R5.3: the asset is in the trash, not deleted (ADR 0002)"
    );
    assert_eq!(
        server.delete_calls(),
        vec![DeleteCall {
            ids: vec![asset_id.clone()],
            force: false,
        }],
        "R5.3: the one request is a trash call, never a permanent delete"
    );
    assert_eq!(
        store
            .item(&file.display().to_string())
            .expect("read the row")
            .expect("the row exists")
            .state,
        ItemState::ReviewTrash
    );
    engine.stop();
}

#[tokio::test]
async fn a_keep_that_lands_while_the_delete_is_in_flight_still_records_the_trash() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "race.jpg", b"race bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    let asset_id = sync_one(&store, &file).await;
    std::fs::remove_file(&file).expect("delete the fixture");
    wait_for_review(&engine, 1).await;

    // The DELETE sits on the wire long enough for a Keep to land inside the
    // same window: Trash already read the row and is awaiting the response.
    server.fail(
        Operation::DeleteAssets,
        Fault::Slow(Duration::from_millis(500)),
    );
    let path = file.display().to_string();
    let trash_engine = Arc::clone(&engine);
    let trash_path = path.clone();
    let trash = tokio::spawn(async move { trash_engine.trash_review_item(&trash_path).await });
    // Yield so the trash task runs up to its awaiting DELETE before Keep.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        engine.keep_review_item(&path).expect("keep the asset"),
        "the item is still waiting when Keep lands"
    );

    let outcome = trash.await.expect("the trash task");
    assert!(outcome.is_ok(), "the DELETE succeeded: {outcome:?}");
    assert!(server.trashed(&asset_id), "the DELETE reached the server");
    assert_eq!(
        store
            .item(&path)
            .expect("read the row")
            .expect("the row exists")
            .state,
        ItemState::ReviewTrash,
        "a successful DELETE is the last word, whatever Keep recorded while it waited"
    );
    assert_eq!(engine.review_items().expect("read the review").len(), 0);
    assert_eq!(
        engine.status().counts.review,
        0,
        "Keep already took the item off the review count"
    );
    engine.stop();
}

#[tokio::test]
async fn a_trash_without_the_permission_keeps_the_item_in_the_review() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "denied.jpg", b"no permission");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    let asset_id = sync_one(&store, &file).await;
    std::fs::remove_file(&file).expect("delete the fixture");
    wait_for_review(&engine, 1).await;
    server.fail(Operation::DeleteAssets, Fault::PermissionDenied);

    let error = engine
        .trash_review_item(&file.display().to_string())
        .await
        .expect_err("the missing permission is refused");

    assert!(
        matches!(error, ReviewError::Client(ClientError::PermissionDenied(_))),
        "R5.5: the refusal keeps its class, got {error:?}"
    );
    assert_eq!(
        engine.review_items().expect("read the review").len(),
        1,
        "R5.5: the item stays in the review"
    );
    assert!(!server.trashed(&asset_id), "nothing changed on the server");
    assert_eq!(
        store
            .item(&file.display().to_string())
            .expect("read the row")
            .expect("the row exists")
            .state,
        ItemState::Missing
    );
    engine.stop();
}

#[tokio::test]
async fn a_trash_without_a_server_copy_is_refused_not_silently_kept() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    add_folder(&store, &photos, true);
    let path = photos.join("never-uploaded.jpg").display().to_string();
    store
        .record(&ItemRecord {
            path: path.clone(),
            size: 12,
            mtime_ns: 1_700_000_000_000_000_000,
            sha1: Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").expect("a hex SHA-1"),
            asset_id: None,
            state: ItemState::Missing,
            error_class: None,
            error_message: None,
            attempts: 0,
            sidecar: None,
        })
        .expect("write the missing row");
    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    wait_for_review(&engine, 1).await;

    let error = engine
        .trash_review_item(&path)
        .await
        .expect_err("a row with no asset cannot be trashed");
    assert!(
        matches!(error, ReviewError::NotInReview(_)),
        "the click must report instead of silently keeping the asset: {error:?}"
    );
    assert_eq!(
        store
            .item(&path)
            .expect("read the row")
            .expect("the row exists")
            .state,
        ItemState::Missing,
        "a refused trash leaves the row in the review"
    );
    assert!(
        server.delete_calls().is_empty(),
        "no request is sent for a row with no server copy"
    );
    assert_eq!(engine.review_items().expect("read the review").len(), 1);
    engine.stop();
}

#[tokio::test]
async fn a_rename_keeps_the_mapping_and_raises_no_review_item() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let old = write_file(&photos, "old.jpg", b"renamed bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(
        &server,
        &store,
        review_config(Duration::from_secs(1), false),
    )
    .await;
    let asset_id = sync_one(&store, &old).await;

    let renamed = photos.join("renamed.jpg");
    std::fs::rename(&old, &renamed).expect("rename the fixture");
    engine.sync_now();
    assert!(
        wait_until(
            || store
                .item(&renamed.display().to_string())
                .ok()
                .flatten()
                .is_some(),
            Duration::from_secs(15)
        )
        .await,
        "R5.2: the item never moved to the renamed path"
    );

    let moved = store
        .item(&renamed.display().to_string())
        .expect("read the new path")
        .expect("the row moved");
    assert_eq!(
        moved.asset_id.as_deref(),
        Some(asset_id.as_str()),
        "R5.2: the item keeps its asset mapping"
    );
    assert!(
        store
            .item(&old.display().to_string())
            .expect("read the old path")
            .is_none(),
        "the old path is no longer tracked"
    );
    assert_eq!(
        store.review_count().expect("count the review"),
        0,
        "R5.2: a rename creates no review item"
    );
    // Later scans walk the same rename; none may raise a review item.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(
        store.review_count().expect("count the review"),
        0,
        "a later scan must not turn the rename into a missing file"
    );
    engine.stop();
}

#[tokio::test]
async fn a_second_copy_of_the_same_content_raises_no_review_item() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let archive = temp.path().join("archive");
    let first = write_file(&photos, "one.jpg", b"the same bytes");
    let second = write_file(&archive, "two.jpg", b"the same bytes");
    add_folder(&store, &photos, true);
    add_folder(&store, &archive, true);
    let engine = start_engine(
        &server,
        &store,
        review_config(Duration::from_secs(1), false),
    )
    .await;
    let asset_id = sync_one(&store, &first).await;
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&second)),
            Duration::from_secs(30)
        )
        .await,
        "the copy never finished"
    );

    std::fs::remove_file(&first).expect("delete the first copy");
    assert!(
        wait_until(
            || store
                .item(&first.display().to_string())
                .ok()
                .flatten()
                .is_some_and(|row| row.state == ItemState::ReviewKeep),
            Duration::from_secs(15)
        )
        .await,
        "the disappeared copy was never retired behind its twin"
    );

    assert_eq!(
        store.review_count().expect("count the review"),
        0,
        "the second-copy acceptance line: the same content in another folder \
         creates no review item"
    );
    assert!(
        !server.trashed(&asset_id),
        "the asset another item still holds is not touched"
    );
    engine.stop();
}

#[tokio::test]
async fn the_watcher_reports_a_deleted_file_without_a_scan_tick() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "watched.jpg", b"watched bytes");
    add_folder(&store, &photos, true);
    // An hour between scans: only the watcher can notice this deletion in
    // time for the test (R2.3, R5.1).
    let engine = start_engine(
        &server,
        &store,
        review_config(Duration::from_secs(60 * 60), true),
    )
    .await;
    sync_one(&store, &file).await;

    std::fs::remove_file(&file).expect("delete the fixture");

    wait_for_review(&engine, 1).await;
    assert_eq!(
        engine.review_items().expect("read the review")[0].path,
        file.display().to_string(),
        "the watcher's disappearance reaches the same review"
    );
    engine.stop();
}

#[tokio::test]
async fn a_file_that_comes_back_leaves_the_review() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "back.jpg", b"restored bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, EngineConfig::default()).await;
    let asset_id = sync_one(&store, &file).await;
    std::fs::remove_file(&file).expect("delete the fixture");
    wait_for_review(&engine, 1).await;

    write_file(&photos, "back.jpg", b"restored bytes");
    assert!(
        wait_until(
            || store.review_count().ok() == Some(0),
            Duration::from_secs(15)
        )
        .await,
        "a restored file never left the review"
    );
    // The item goes back through the pipeline: the server already holds the
    // bytes, so the upload resolves to the same asset (R3.2, R4.5).
    assert!(
        wait_until(
            || store
                .item(&file.display().to_string())
                .ok()
                .flatten()
                .is_some_and(|row| row.state.is_terminal_success()),
            Duration::from_secs(15)
        )
        .await,
        "the restored file never finished"
    );
    let row = store
        .item(&file.display().to_string())
        .expect("read the row")
        .expect("the row exists");
    assert_eq!(
        row.asset_id.as_deref(),
        Some(asset_id.as_str()),
        "the restored file keeps pointing at the asset the server holds"
    );
    engine.stop();
}

#[tokio::test]
async fn a_file_that_comes_back_after_a_trash_decision_restores_the_asset() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "trashed-then-back.jpg", b"trashed then back");
    add_folder(&store, &photos, true);
    let engine = start_engine(
        &server,
        &store,
        review_config(Duration::from_secs(1), false),
    )
    .await;
    let asset_id = sync_one(&store, &file).await;
    std::fs::remove_file(&file).expect("delete the fixture");
    wait_for_review(&engine, 1).await;
    engine
        .trash_review_item(&file.display().to_string())
        .await
        .expect("R5.3: the asset moves to the trash");
    assert!(server.trashed(&asset_id), "the asset is in the trash");

    // The same bytes come back. `review_trash` must not be a settled state,
    // and a trashed checksum must not read as held: the engine restores the
    // asset instead of uploading bytes the server answers `duplicate`.
    write_file(&photos, "trashed-then-back.jpg", b"trashed then back");
    engine.sync_now();
    assert!(
        wait_until(|| !server.trashed(&asset_id), Duration::from_secs(15)).await,
        "R5.3: the trashed asset was not restored"
    );
    assert!(
        wait_until(
            || store
                .item(&file.display().to_string())
                .ok()
                .flatten()
                .is_some_and(|row| row.state.is_terminal_success()),
            Duration::from_secs(15)
        )
        .await,
        "the restored file never settled: {:?}",
        store.item(&file.display().to_string())
    );
    assert_eq!(
        server.upload_requests(),
        1,
        "a restore is not an upload: the checksum is already known"
    );
    assert_eq!(
        store
            .item(&file.display().to_string())
            .expect("read the row")
            .expect("the row exists")
            .asset_id
            .as_deref(),
        Some(asset_id.as_str()),
        "the restored file keeps its asset mapping"
    );
    engine.stop();
}

#[tokio::test]
async fn a_review_item_whose_content_reappears_elsewhere_leaves_the_review() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "moved-after-review.jpg", b"late rename bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(
        &server,
        &store,
        review_config(Duration::from_secs(1), false),
    )
    .await;
    let asset_id = sync_one(&store, &file).await;

    std::fs::remove_file(&file).expect("delete the fixture");
    wait_for_review(&engine, 1).await;

    // The file comes back at another path after the review already listed it.
    let renamed = photos.join("found-again.jpg");
    write_file(&photos, "found-again.jpg", b"late rename bytes");
    assert!(
        wait_until(
            || store
                .item(&renamed.display().to_string())
                .ok()
                .flatten()
                .is_some_and(|row| row.state.is_terminal_success()),
            Duration::from_secs(15)
        )
        .await,
        "the item never reunited with its content"
    );

    let moved = store
        .item(&renamed.display().to_string())
        .expect("read the new path")
        .expect("the row moved");
    assert_eq!(moved.asset_id.as_deref(), Some(asset_id.as_str()));
    assert_eq!(
        store.review_count().expect("count the review"),
        0,
        "R5.4: an item whose content is back leaves the review"
    );
    assert_eq!(engine.status().counts.review, 0);
    engine.stop();
}

#[tokio::test]
async fn a_missing_row_retired_behind_a_restored_twin_leaves_the_review_count() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let first = write_file(&photos, "twin-one.jpg", b"twin bytes");
    let second = write_file(&photos, "twin-two.jpg", b"twin bytes");
    add_folder(&store, &photos, true);
    let engine = start_engine(
        &server,
        &store,
        review_config(Duration::from_secs(1), false),
    )
    .await;
    assert!(
        wait_until(
            || all_terminal(&store, &[first.clone(), second.clone()]),
            Duration::from_secs(30)
        )
        .await,
        "the twins never reached a terminal state"
    );

    // Both copies disappear: both wait for a decision.
    std::fs::remove_file(&first).expect("delete the first twin");
    std::fs::remove_file(&second).expect("delete the second twin");
    wait_for_review(&engine, 2).await;
    assert_eq!(engine.status().counts.review, 2, "both twins are counted");

    // One copy comes back at the same path: the other is retired behind its
    // present twin, which must also take it off the tray count (R5.4).
    write_file(&photos, "twin-two.jpg", b"twin bytes");
    assert!(
        wait_until(
            || engine.review_items().is_ok_and(|items| items.is_empty()),
            Duration::from_secs(20)
        )
        .await,
        "the review never drained"
    );
    assert_eq!(
        engine.status().counts.review,
        0,
        "a retired missing item must leave the tray count"
    );
    engine.stop();
}
