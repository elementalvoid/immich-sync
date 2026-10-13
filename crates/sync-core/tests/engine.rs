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
use sync_core::hash::sha1_file;
use sync_core::store::{ItemRecord, ItemState, SourceFolder, StateStore};

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
        })
        .expect("add the source folder");
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
        }]
    );

    engine
        .remove_folder(&photos.display().to_string())
        .expect("remove the folder");
    assert!(engine.status().folders.is_empty());
    assert!(store.folders().expect("read the folders").is_empty());
    engine.stop();
}
