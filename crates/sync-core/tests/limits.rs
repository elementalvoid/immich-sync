//! The live bandwidth and concurrency limits (SPEC R7.1, R7.2).
//!
//! Real timing against the offline double, because the criteria are a rate and
//! a number of simultaneous uploads.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mock_immich::{Fault, MockImmich, Operation};
use sync_core::connection::{AuthMethod, Connection, Secret, SharedSession, SignIn};
use sync_core::credential::MemoryCredentialStore;
use sync_core::engine::{Engine, EngineConfig, Limits};
use sync_core::store::{AlbumScope, SourceFolder, StateStore};

use common::wait_until;

/// The acceptance criterion's rate (R7.1).
const ONE_MEGABYTE_PER_SECOND: u64 = 1_000_000;
/// The bandwidth fixture is 1.5 mebibytes: far more than the limiter's 0.1 s
/// burst, so the pace is what the clock measures.
const BIG_BYTES: usize = 1_572_864;
/// How many small files the concurrency tests sync.
const SMALL_FILES: usize = 4;

/// Starts the mock and signs in, returning the session the engine attaches
/// to every request (R1.4).
async fn signed_in(server: &MockImmich) -> SharedSession {
    let mut connection = Connection::new(server.base_url(), Arc::new(MemoryCredentialStore::new()))
        .expect("a valid URL");
    connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        })
        .await;
    connection.shared()
}

fn store_in(temp: &tempfile::TempDir) -> Arc<StateStore> {
    Arc::new(StateStore::open(temp.path().join("data").join("state.db")).expect("open the store"))
}

fn add_folder(store: &StateStore, path: &Path) {
    store
        .add_folder(&SourceFolder {
            path: path.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
        })
        .expect("add the source folder");
}

async fn start_engine(
    server: &MockImmich,
    store: &Arc<StateStore>,
    config: EngineConfig,
) -> Arc<Engine> {
    let session = signed_in(server).await;
    let engine = Arc::new(Engine::new(store.clone(), session, config).expect("build the engine"));
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

/// Writes `bytes` deterministic bytes to `root/name`. The content differs per
/// name, so the double treats two fixtures as two files instead of one file
/// uploaded twice.
fn write_file(root: &Path, name: &str, bytes: usize) -> PathBuf {
    std::fs::create_dir_all(root).expect("create the fixture directory");
    let path = root.join(name);
    let seed = name
        .bytes()
        .fold(0_usize, |sum, byte| sum + usize::from(byte));
    let content: Vec<u8> = (0..bytes)
        .map(|index| (index.wrapping_add(seed) % 251) as u8)
        .collect();
    std::fs::write(&path, content).expect("write the fixture");
    path
}

/// `count` small distinct files named `<prefix>-<index>.bin`, so each one
/// uploads separately and two waves never collide.
fn small_files(root: &Path, prefix: &str, count: usize) -> Vec<PathBuf> {
    (0..count)
        .map(|index| write_file(root, &format!("{prefix}-{index}.bin"), 64 * 1024))
        .collect()
}

#[tokio::test]
async fn a_one_megabyte_per_second_limit_paces_the_sync() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let path = write_file(&photos, "big.bin", BIG_BYTES);
    add_folder(&store, &photos);

    let config = EngineConfig {
        bandwidth_bytes_per_second: ONE_MEGABYTE_PER_SECOND,
        ..EngineConfig::default()
    };
    let started = Instant::now();
    let engine = start_engine(&server, &store, config).await;
    assert_eq!(
        engine.limits().bandwidth_bytes_per_second,
        ONE_MEGABYTE_PER_SECOND,
        "the engine reads the limit from its config"
    );

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&path)),
            Duration::from_secs(20)
        )
        .await,
        "the paced upload never finished"
    );
    let elapsed = started.elapsed();
    // bytes / limit is 1.57 s, less the 0.1 s burst: 1.47 s expected. The
    // floor (0.65x) cannot be met by any rate above the limit, and the ceiling
    // (2.7x) is far more than hashing and sleeps can add.
    assert!(
        elapsed >= Duration::from_secs(1),
        "1 MB/s for {BIG_BYTES} bytes finished in {elapsed:?}; the limiter did not pace it"
    );
    assert!(
        elapsed <= Duration::from_secs(4),
        "1 MB/s for {BIG_BYTES} bytes took {elapsed:?}; the limiter is far under the limit"
    );
}

#[tokio::test]
async fn zero_bandwidth_means_unlimited() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let path = write_file(&photos, "big.bin", BIG_BYTES);
    add_folder(&store, &photos);

    let config = EngineConfig {
        bandwidth_bytes_per_second: 0,
        ..EngineConfig::default()
    };
    let started = Instant::now();
    let _engine = start_engine(&server, &store, config).await;
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&path)),
            Duration::from_secs(20)
        )
        .await,
        "the unlimited upload never finished"
    );
    let elapsed = started.elapsed();
    // Only the ceiling is an assertion here. The floor cannot be: the whole
    // pipeline (store, watcher, hash, bulk check, upload) shares the clock
    // with the limiter, and a loaded machine moves that overhead by seconds -
    // which is how a 1 s ceiling once cost this suite a day. What matters is
    // that no pacing is left: 1 MB/s would need 1.47 s of the upload alone,
    // while this run lands near 0.3 s unloaded and well inside the ceiling
    // even when the machine is six times oversubscribed.
    assert!(
        elapsed <= Duration::from_secs(3),
        "R7.1: 0 means unlimited, but {BIG_BYTES} bytes took {elapsed:?}"
    );
}

#[tokio::test]
async fn raising_bandwidth_mid_upload_is_applied_to_the_same_upload() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let path = write_file(&photos, "big.bin", BIG_BYTES);
    add_folder(&store, &photos);

    // At 100 kB/s the whole file would take about 15 s, so the timeout below
    // fails unless the change reached the request already on the wire.
    let config = EngineConfig {
        bandwidth_bytes_per_second: 100_000,
        ..EngineConfig::default()
    };
    let started = Instant::now();
    let engine = start_engine(&server, &store, config).await;
    assert!(
        wait_until(|| server.upload_requests() >= 1, Duration::from_secs(10)).await,
        "the upload never reached the server"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    engine
        .set_limits(Limits {
            bandwidth_bytes_per_second: 0,
            concurrency: 4,
        })
        .await;

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&path)),
            Duration::from_secs(5)
        )
        .await,
        "the raised limit did not reach the upload in flight ({:?})",
        started.elapsed()
    );
}

#[tokio::test]
async fn raising_concurrency_mid_sync_starts_new_workers() {
    let server = MockImmich::start().await;
    server.set_upload_delay(Duration::from_millis(400));
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let files = small_files(&photos, "small", SMALL_FILES);
    add_folder(&store, &photos);

    let engine = start_engine(
        &server,
        &store,
        EngineConfig {
            concurrency: 1,
            ..EngineConfig::default()
        },
    )
    .await;
    assert!(
        wait_until(|| server.upload_concurrency() >= 1, Duration::from_secs(10)).await,
        "the first upload never started"
    );
    assert_eq!(
        server.peak_upload_concurrency(),
        1,
        "the pool must start at the configured one"
    );

    engine
        .set_limits(Limits {
            bandwidth_bytes_per_second: 0,
            concurrency: SMALL_FILES,
        })
        .await;

    assert!(
        wait_until(
            || server.peak_upload_concurrency() >= 3,
            Duration::from_secs(10)
        )
        .await,
        "R7.2: raising concurrency must start new workers, saw {}",
        server.peak_upload_concurrency()
    );
    assert!(
        wait_until(|| all_terminal(&store, &files), Duration::from_secs(20)).await,
        "not every file finished"
    );
    assert_eq!(server.upload_requests(), SMALL_FILES as u64);
}

#[tokio::test]
async fn lowering_concurrency_does_not_abandon_an_in_flight_upload() {
    let server = MockImmich::start().await;
    server.set_upload_delay(Duration::from_millis(600));
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let files = small_files(&photos, "small", SMALL_FILES);
    add_folder(&store, &photos);

    let engine = start_engine(
        &server,
        &store,
        EngineConfig {
            concurrency: SMALL_FILES,
            ..EngineConfig::default()
        },
    )
    .await;
    assert!(
        wait_until(
            || server.upload_concurrency() == SMALL_FILES as u64,
            Duration::from_secs(10)
        )
        .await,
        "all four uploads should be on the wire at once"
    );

    // A lower limit takes effect as the in-flight uploads finish; it must not
    // cancel or fail one of them.
    engine
        .set_limits(Limits {
            bandwidth_bytes_per_second: 0,
            concurrency: 1,
        })
        .await;

    assert!(
        wait_until(|| all_terminal(&store, &files), Duration::from_secs(20)).await,
        "lowering concurrency abandoned an upload"
    );
    assert_eq!(server.upload_requests(), SMALL_FILES as u64);
    assert_eq!(
        engine.limits().concurrency,
        1,
        "the new limit is the live one"
    );
}

#[tokio::test]
async fn lowering_concurrency_completes_while_a_retry_sleeps() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    write_file(&photos, "retrying.bin", 64 * 1024);
    add_folder(&store, &photos);
    // A long server-requested wait: a worker that held its permit across the
    // sleep would make the resize below wait the whole 30 s.
    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 503,
            retry_after: Some(30),
        },
    );
    let engine = start_engine(
        &server,
        &store,
        EngineConfig {
            concurrency: 2,
            ..EngineConfig::default()
        },
    )
    .await;
    assert!(
        wait_until(|| server.upload_requests() == 1, Duration::from_secs(10)).await,
        "the first attempt never reached the server"
    );
    // Let the worker reach its backoff sleep.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let changed = tokio::time::timeout(
        Duration::from_secs(3),
        engine.set_limits(Limits {
            bandwidth_bytes_per_second: 0,
            concurrency: 1,
        }),
    )
    .await;
    assert!(
        changed.is_ok(),
        "R7.2: lowering the limit waited on a permit held through the backoff sleep"
    );
    engine.stop();
}

#[tokio::test]
async fn lowering_concurrency_shrinks_the_pool_for_the_next_wave() {
    let server = MockImmich::start().await;
    server.set_upload_delay(Duration::from_millis(200));
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let first_wave = small_files(&photos, "first", SMALL_FILES);
    add_folder(&store, &photos);

    let engine = start_engine(
        &server,
        &store,
        EngineConfig {
            concurrency: SMALL_FILES,
            ..EngineConfig::default()
        },
    )
    .await;
    assert!(
        wait_until(
            || all_terminal(&store, &first_wave),
            Duration::from_secs(20)
        )
        .await,
        "the first wave never finished"
    );
    assert_eq!(
        server.peak_upload_concurrency(),
        SMALL_FILES as u64,
        "the pool must start at the configured four"
    );

    engine
        .set_limits(Limits {
            bandwidth_bytes_per_second: 0,
            concurrency: 1,
        })
        .await;

    // Work that appears after the first wave. The reconciliation scan walks a
    // folder it has already walked, so this does not lean on the watcher
    // re-reporting a folder it is watching (R8.3).
    let second_wave = small_files(&photos, "second", SMALL_FILES);
    // The peak so far belongs to the first wave; what matters now is what the
    // pool admits next.
    server.reset_upload_peaks();
    engine.sync_now();

    assert!(
        wait_until(
            || all_terminal(&store, &second_wave),
            Duration::from_secs(30)
        )
        .await,
        "the second wave never finished"
    );
    assert_eq!(
        server.peak_upload_concurrency(),
        1,
        "R7.2: the pool admitted more uploads at once than the new limit"
    );
    assert_eq!(
        server.upload_requests(),
        2 * SMALL_FILES as u64,
        "the settled first wave must not be uploaded again"
    );
}
