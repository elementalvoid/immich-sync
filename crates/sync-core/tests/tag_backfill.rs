//! The sync tag's backfill over a library that was uploaded before the tag was
//! turned on (SPEC R3.7, issue #46).
//!
//! The backfill tags every asset the client ever uploaded: each item's current
//! asset, every old version, and every asset the server answered as a
//! duplicate. These tests drive the real engine over the offline double and
//! assert the requests on the wire as well as the progress the engine
//! publishes, because the double is more forgiving than the real server.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::wait_until;
use mock_immich::{Fault, MockImmich, Operation, TagAssetCall};
use sync_core::backoff::RetryPolicy;
use sync_core::connection::{AuthMethod, Connection, Secret, SharedSession, SignIn};
use sync_core::credential::MemoryCredentialStore;
use sync_core::engine::{Engine, EngineConfig};
use sync_core::store::{AlbumScope, ItemState, SourceFolder, StateStore};
use sync_core::tags::SYNC_TAG;
use sync_core::tracking::Tracking;

/// Starts the double and signs in, returning the session the engine attaches
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

/// Opens a store in a fresh temp directory.
fn store_in(temp: &tempfile::TempDir) -> Arc<StateStore> {
    Arc::new(StateStore::open(temp.path().join("data").join("state.db")).expect("open the store"))
}

/// Adds a folder straight to the store, so the engine sees it at construction.
fn add_folder(store: &StateStore, path: &Path) {
    store
        .add_folder(&SourceFolder {
            path: path.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");
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

/// The retry policy the backfill tests use: fast, deterministic waits.
fn test_retry() -> RetryPolicy {
    RetryPolicy {
        base: Duration::from_millis(20),
        max: Duration::from_millis(80),
        max_retry_after: Duration::from_secs(5),
        seed: Some(7),
    }
}

/// The shared part of both configurations: one engine, fast retries, no
/// watcher.
fn base_config() -> EngineConfig {
    EngineConfig {
        attempt_cap: 3,
        retry: test_retry(),
        breaker_threshold: 3,
        attention_threshold: 3,
        breaker_cooldown: Duration::from_millis(120),
        concurrency: 2,
        watch: false,
        ..EngineConfig::default()
    }
}

/// A config with the sync tag off, which is how the existing library is
/// uploaded before the ticket's feature is turned on (R3.7).
fn untagged_config() -> EngineConfig {
    EngineConfig {
        tag_uploaded_assets: false,
        ..base_config()
    }
}

/// A config with the sync tag on and `batch` ids per `PUT /tags/assets` call
/// (R3.7).
fn backfill_config(batch: usize) -> EngineConfig {
    EngineConfig {
        tag_uploaded_assets: true,
        tag_batch_size: batch,
        ..base_config()
    }
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

/// Whether one item reached a terminal success row.
fn succeeded(store: &StateStore, path: &Path) -> bool {
    matches!(
        store.item(&path.display().to_string()),
        Ok(Some(row)) if row.state.is_terminal_success()
    )
}

/// The number of recorded versions of one item.
fn chain_len(store: &StateStore, file: &Path) -> usize {
    store
        .versions(&file.display().to_string())
        .expect("read the version chain")
        .len()
}

/// Uploads `bytes` straight through the API, so the engine's checksum check
/// answers duplicate for content the server already holds (R3.2).
async fn bytes_already_on_the_server(server: &MockImmich, bytes: &[u8]) -> String {
    let directory = tempfile::tempdir().expect("create a temp dir");
    let path = directory.path().join("already.jpg");
    std::fs::write(&path, bytes).expect("write the fixture");
    sync_core::ImmichClient::new(server.base_url())
        .expect("the mock URL is valid")
        .upload_asset(
            immich_client::Auth::ApiKey("test-key"),
            &immich_client::AssetUpload {
                path: &path,
                checksum: &sync_core::hash::sha1_file(&path)
                    .expect("hash the fixture")
                    .base64(),
                created_at: std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                modified_at: std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_040),
                sidecar: None,
                live_photo_video_id: None,
                bandwidth: immich_client::BandwidthLimit::default(),
            },
        )
        .await
        .expect("the mock uploads the fixture")
        .asset_id
}

/// A library uploaded with the tag off, so the backfill has something to cover:
/// a plain asset, a duplicate the server already held, and both assets of an
/// edited file (R3.7).
struct Library {
    store: Arc<StateStore>,
    /// Every asset id the state holds, in the order the backfill enumerates
    /// them.
    assets: Vec<String>,
}

/// Uploads the library with the tag off and returns it, with the asset ids the
/// backfill has to cover.
async fn library_uploaded_without_the_tag(
    server: &MockImmich,
    temp: &tempfile::TempDir,
) -> Library {
    let store = store_in(temp);
    let photos = temp.path().join("photos");
    let duplicate_bytes = b"already on the server";
    let duplicate = bytes_already_on_the_server(server, duplicate_bytes).await;
    let first = write_file(&photos, "first.jpg", b"uploaded before the tag");
    let edited = write_file(&photos, "edited.jpg", b"edit number one");
    let duplicated = write_file(&photos, "duplicate.jpg", duplicate_bytes);
    add_folder(&store, &photos);

    let engine = start_engine(server, &store, untagged_config()).await;
    assert!(
        wait_until(
            || succeeded(&store, &first)
                && succeeded(&store, &edited)
                && succeeded(&store, &duplicated),
            Duration::from_secs(20)
        )
        .await,
        "the library did not upload"
    );
    // An edit makes a second asset and keeps the first as an old version, so
    // the backfill has to cover both (R4.4).
    std::fs::write(&edited, b"edit number two, which is longer").expect("edit the file");
    engine.sync_now();
    assert!(
        wait_until(|| chain_len(&store, &edited) == 2, Duration::from_secs(20)).await,
        "R4.4: the edit did not become a second version"
    );
    engine.stop();

    let assets = store
        .tag_backfill_batch(None, 1000)
        .expect("read the library's asset ids");
    assert_eq!(
        assets.len(),
        4,
        "the library covers a plain asset, a duplicate, and two versions: {assets:?}"
    );
    assert!(
        assets.contains(&duplicate),
        "the duplicate the server already held is part of the library: {assets:?}"
    );
    Library { store, assets }
}

/// A six-asset library: the four of [`library_uploaded_without_the_tag`] plus
/// two plain uploads, so a paused run has more batches to come back for.
async fn library_of_six(server: &MockImmich, temp: &tempfile::TempDir) -> Library {
    let library = library_uploaded_without_the_tag(server, temp).await;
    let photos = temp.path().join("photos");
    let engine = start_engine(server, &library.store, untagged_config()).await;
    for (name, bytes) in [
        ("fifth.jpg", b"fifth upload".as_slice()),
        ("sixth.jpg", b"sixth upload".as_slice()),
    ] {
        let file = write_file(&photos, name, bytes);
        engine.sync_now();
        assert!(
            wait_until(|| succeeded(&library.store, &file), Duration::from_secs(20)).await,
            "{name} did not upload"
        );
    }
    engine.stop();
    let assets = library
        .store
        .tag_backfill_batch(None, 1000)
        .expect("read the library's asset ids");
    assert_eq!(assets.len(), 6, "six uploads, six assets: {assets:?}");
    Library { assets, ..library }
}

/// R3.7: enabling the tag on a library that was uploaded before it starts one
/// backfill over every current asset, every old version, and every duplicate,
/// a batch of ids per `PUT /tags/assets` call.
#[tokio::test]
async fn the_backfill_tags_every_asset_the_client_ever_uploaded() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let library = library_uploaded_without_the_tag(&server, &temp).await;
    assert!(
        server.tag_asset_calls().is_empty(),
        "the library was uploaded with the tag off"
    );

    let engine = start_engine(&server, &library.store, backfill_config(2)).await;

    assert!(
        wait_until(
            || engine.tag_status().backfill.completed,
            Duration::from_secs(20)
        )
        .await,
        "the backfill did not finish: {:?}",
        engine.tag_status().backfill
    );
    let tag = server
        .tag_id(SYNC_TAG)
        .expect("R3.7: the run created the sync tag");
    assert_eq!(
        server.upsert_calls().len(),
        1,
        "R3.7: one upsert resolves the tag for the run, the backfill's start included: {:?}",
        server.upsert_calls()
    );
    assert_eq!(
        server.tag_asset_calls(),
        vec![
            TagAssetCall {
                tag_ids: vec![tag.clone()],
                asset_ids: library.assets[0..2].to_vec(),
            },
            TagAssetCall {
                tag_ids: vec![tag.clone()],
                asset_ids: library.assets[2..4].to_vec(),
            },
        ],
        "R3.7: the backfill batches the library's ids through PUT /tags/assets, in id order"
    );
    for asset in &library.assets {
        assert_eq!(
            server.tags_of(asset),
            vec![tag.clone()],
            "R3.7: every asset the client ever uploaded carries the tag: {asset}"
        );
    }
    engine.stop();
}

/// R3.7: progress is published while the run is going, with the count the
/// library has and how many assets are already tagged, and it reaches a
/// completed state.
#[tokio::test]
async fn the_backfill_publishes_its_progress_and_completes() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let library = library_uploaded_without_the_tag(&server, &temp).await;
    // Hold the second batch, so the state between the two batches is
    // observable instead of a race.
    server.fail(Operation::TagAssets, Fault::Slow(Duration::ZERO));
    server.fail(
        Operation::TagAssets,
        Fault::Slow(Duration::from_millis(800)),
    );

    let engine = start_engine(&server, &library.store, backfill_config(2)).await;

    assert!(
        wait_until(
            || {
                let status = engine.tag_status().backfill;
                status.running && status.tagged == 2 && status.total == 4
            },
            Duration::from_secs(20)
        )
        .await,
        "R3.7: the run did not publish \"2 of 4\" while it worked: {:?}",
        engine.tag_status().backfill
    );
    assert!(
        wait_until(
            || engine.tag_status().backfill.completed,
            Duration::from_secs(20)
        )
        .await,
        "the backfill did not finish: {:?}",
        engine.tag_status().backfill
    );
    let status = engine.tag_status().backfill;
    assert!(
        status.tagged == 4 && status.total == 4,
        "R3.7: the completed state counts the whole library: {status:?}"
    );
    engine.stop();
}

/// R3.7: a finished backfill is recorded, so the next run over the same
/// library sends no tag requests, starts from its completed state, and keeps
/// the count it tagged even when the library grew since.
#[tokio::test]
async fn a_completed_backfill_is_remembered_across_a_restart() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let library = library_uploaded_without_the_tag(&server, &temp).await;

    let engine = start_engine(&server, &library.store, backfill_config(2)).await;
    assert!(
        wait_until(
            || engine.tag_status().backfill.completed,
            Duration::from_secs(20)
        )
        .await,
        "the first backfill did not finish"
    );
    engine.stop();
    let recorded = library.store.tag_backfill().expect("read the state");
    assert!(
        recorded.completed_ns.is_some(),
        "R3.7: the completion is in the state database"
    );
    let calls = server.tag_asset_calls().len();
    // An asset the library gained after the pass finished, outside the synced
    // folder so the restarted engine's scan leaves it alone: the library is one
    // asset bigger, but that asset is not part of the finished pass.
    library
        .store
        .record(&sync_core::store::ItemRecord {
            path: temp
                .path()
                .join("elsewhere/after.jpg")
                .display()
                .to_string(),
            size: 5,
            mtime_ns: 1,
            sha1: sync_core::hash::Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")
                .expect("a hex SHA-1"),
            asset_id: Some("asset-uploaded-after-the-pass".to_string()),
            state: ItemState::Uploaded,
            error_class: None,
            error_message: None,
            attempts: 0,
            sidecar: None,
        })
        .expect("record an upload that landed after the pass");
    assert_eq!(
        library
            .store
            .tag_backfill_total()
            .expect("count the library"),
        5,
        "the library grew after the pass finished"
    );

    let restarted = start_engine(&server, &library.store, backfill_config(2)).await;
    // The run start resolves the tag, which is the one request a restart may
    // still make.
    assert!(
        wait_until(|| server.upsert_calls().len() == 2, Duration::from_secs(20)).await,
        "the restarted engine did not resolve the tag"
    );
    assert!(
        wait_until(
            || {
                let status = restarted.tag_status().backfill;
                status.completed && status.tagged == 4 && status.total == 4
            },
            Duration::from_secs(20)
        )
        .await,
        "R3.7: the completion keeps its own count, not the library's size now: {:?}",
        restarted.tag_status().backfill
    );
    assert_eq!(
        server.tag_asset_calls().len(),
        calls,
        "R3.7: a restart does not tag the whole library again"
    );

    // R3.7: the completion record survives the toggle too, so turning the
    // setting off and on again does not start the library over.
    restarted.set_tag_uploads(false).await;
    restarted.set_tag_uploads(true).await;
    assert!(
        wait_until(|| server.upsert_calls().len() == 3, Duration::from_secs(20)).await,
        "the toggle did not reach the engine"
    );
    assert_eq!(
        server.tag_asset_calls().len(),
        calls,
        "R3.7: the toggle keeps the finished backfill finished"
    );
    restarted.stop();
}

/// R3.7: a tag the user deleted and recreated in Immich is upserted again when
/// the backfill starts, so the run uses the id the tag has now instead of the
/// one it cached.
#[tokio::test]
async fn a_tag_recreated_in_immich_is_used_by_the_backfill() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let library = library_uploaded_without_the_tag(&server, &temp).await;

    let engine = start_engine(&server, &library.store, backfill_config(100)).await;
    assert!(
        wait_until(
            || engine.tag_status().backfill.completed,
            Duration::from_secs(20)
        )
        .await,
        "the first backfill did not finish"
    );
    let deleted = server
        .tag_id(SYNC_TAG)
        .expect("the first run created the tag");
    // The user deletes the tag in Immich, which drops it from every asset.
    server.delete_tag(&deleted);
    assert_eq!(server.tag_id(SYNC_TAG), None);

    engine.retag_all().expect("run the library again");

    assert!(
        wait_until(
            || {
                server
                    .tag_id(SYNC_TAG)
                    .is_some_and(|recreated| recreated != deleted)
                    && engine.tag_status().backfill.completed
            },
            Duration::from_secs(20)
        )
        .await,
        "R3.7: the backfill never resolved the recreated tag: {:?} {:?}",
        server.tag_id(SYNC_TAG),
        engine.tag_status().backfill
    );
    let recreated = server
        .tag_id(SYNC_TAG)
        .expect("the re-upsert created the tag again");
    for asset in &library.assets {
        assert_eq!(
            server.tags_of(asset),
            vec![recreated.clone()],
            "R3.7: the backfill used the id the tag has now, not the one it cached: {asset}"
        );
    }
    engine.stop();
}

/// R3.7: "Tag existing assets now" clears the marker and tags the whole
/// library again, which is safe because tagging is idempotent.
#[tokio::test]
async fn tagging_the_library_again_re_runs_the_backfill_idempotently() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let library = library_uploaded_without_the_tag(&server, &temp).await;

    let engine = start_engine(&server, &library.store, backfill_config(2)).await;
    assert!(
        wait_until(
            || engine.tag_status().backfill.completed,
            Duration::from_secs(20)
        )
        .await,
        "the first backfill did not finish"
    );
    assert_eq!(server.tag_asset_calls().len(), 2, "two batches of two ids");

    engine.retag_all().expect("clear the recorded completion");

    assert!(
        wait_until(
            || { engine.tag_status().backfill.completed && server.tag_asset_calls().len() == 4 },
            Duration::from_secs(20)
        )
        .await,
        "R3.7: \"Tag existing assets now\" did not run the whole library again: {:?}",
        engine.tag_status().backfill
    );
    let tag = server
        .tag_id(SYNC_TAG)
        .expect("the tag still exists")
        .clone();
    for asset in &library.assets {
        assert_eq!(
            server.tags_of(asset),
            vec![tag.clone()],
            "R3.7: re-tagging is safe, the asset carries the tag once: {asset}"
        );
    }
    assert_eq!(
        engine.tag_status().backfill.tagged,
        engine.tag_status().backfill.total,
        "the second run counts the same library"
    );
    engine.stop();
}

/// R3.7: a run that was interrupted after its last completed batch resumes
/// from that cursor instead of tagging the library from the start.
#[tokio::test]
async fn an_interrupted_backfill_resumes_after_its_last_completed_batch() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let library = library_uploaded_without_the_tag(&server, &temp).await;
    // The state a crash after the first of two batches leaves behind.
    library
        .store
        .record_tag_batch(&library.assets[1], 2)
        .expect("record the interrupted batch");

    let engine = start_engine(&server, &library.store, backfill_config(2)).await;

    assert!(
        wait_until(
            || engine.tag_status().backfill.completed,
            Duration::from_secs(20)
        )
        .await,
        "the resumed backfill did not finish: {:?}",
        engine.tag_status().backfill
    );
    let tag = server.tag_id(SYNC_TAG).expect("the tag exists");
    assert_eq!(
        server.tag_asset_calls(),
        vec![TagAssetCall {
            tag_ids: vec![tag],
            asset_ids: library.assets[2..4].to_vec(),
        }],
        "R3.7: the resumed run starts after the last completed batch, not at the beginning"
    );
    assert!(
        library.assets[..2]
            .iter()
            .all(|asset| server.tags_of(asset).is_empty()),
        "the interrupted batch is not re-sent by this run"
    );
    assert_eq!(
        engine.tag_status().backfill.tagged,
        4,
        "R3.7: the count continues from the recorded progress"
    );
    engine.stop();
}

/// R3.7: "Tag existing assets now" pressed while a batch is in flight stops
/// that pass at the batch boundary instead of letting it record the cursor it
/// read before the clear, so the queued pass starts at the library's first id
/// and no asset before the cleared cursor is skipped.
#[tokio::test]
async fn tag_now_during_a_batch_restarts_the_pass_at_the_first_id() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let library = library_of_six(&server, &temp).await;
    // A pass that already got past its first two assets, so the batch in flight
    // below is not the head of the library.
    library
        .store
        .record_tag_batch(&library.assets[1], 2)
        .expect("record the interrupted batch");
    // Hold the resumed pass's next batch, so "tag now" lands while it is the
    // work in flight.
    server.fail(
        Operation::TagAssets,
        Fault::Slow(Duration::from_millis(700)),
    );

    let engine = start_engine(&server, &library.store, backfill_config(2)).await;
    assert!(
        wait_until(
            || server
                .requests()
                .iter()
                .any(|request| request.ends_with("/tags/assets")),
            Duration::from_secs(20)
        )
        .await,
        "the resumed pass never put a batch on the wire"
    );
    engine.retag_all().expect("clear the recorded progress");

    assert!(
        wait_until(
            || engine.tag_status().backfill.completed && server.tag_asset_calls().len() == 4,
            Duration::from_secs(20)
        )
        .await,
        "R3.7: the queued pass did not finish the whole library: {:?} {:?}",
        engine.tag_status().backfill,
        server.tag_asset_calls()
    );
    let tag = server.tag_id(SYNC_TAG).expect("the tag exists");
    assert_eq!(
        server.tag_asset_calls(),
        vec![
            TagAssetCall {
                tag_ids: vec![tag.clone()],
                asset_ids: library.assets[2..4].to_vec(),
            },
            TagAssetCall {
                tag_ids: vec![tag.clone()],
                asset_ids: library.assets[0..2].to_vec(),
            },
            TagAssetCall {
                tag_ids: vec![tag.clone()],
                asset_ids: library.assets[2..4].to_vec(),
            },
            TagAssetCall {
                tag_ids: vec![tag],
                asset_ids: library.assets[4..6].to_vec(),
            },
        ],
        "R3.7: the cleared pass starts at the first id, so nothing before the old cursor is skipped"
    );
    assert_eq!(
        engine.tag_status().backfill.tagged,
        6,
        "R3.7: the re-run counts the whole library"
    );
    engine.stop();
}

/// R3.7: a batch that keeps failing stops the run at the retry cap, records the
/// reason, keeps the progress that already landed, and does not record a
/// completion, so a later attempt continues from the cursor.
#[tokio::test]
async fn a_batch_that_keeps_failing_stops_at_the_retry_cap() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let library = library_uploaded_without_the_tag(&server, &temp).await;
    // A pass that already tagged its first batch, then met a server that keeps
    // answering 500. The fault is queued past the cap, so a run that did not
    // stop would keep spending attempts instead of meeting a success.
    library
        .store
        .record_tag_batch(&library.assets[1], 2)
        .expect("record the interrupted batch");
    for _ in 0..4 {
        server.fail(
            Operation::TagAssets,
            Fault::Status {
                code: 500,
                retry_after: None,
            },
        );
    }
    let config = EngineConfig {
        attempt_cap: 2,
        ..backfill_config(2)
    };

    let engine = start_engine(&server, &library.store, config).await;

    assert!(
        wait_until(
            || {
                let status = engine.tag_status();
                !status.backfill.running && !status.backfill.completed && status.problem.is_some()
            },
            Duration::from_secs(20)
        )
        .await,
        "R3.7: the run did not stop after the retry cap: {:?} {:?}",
        engine.tag_status().backfill,
        engine.tag_status().problem
    );
    let attempts = server
        .requests()
        .iter()
        .filter(|request| request.ends_with("/tags/assets"))
        .count();
    assert_eq!(
        attempts,
        2,
        "R3.7: the batch was tried attempt_cap times and no more: {:?}",
        server.requests()
    );
    let problem = engine
        .tag_status()
        .problem
        .expect("R3.7: the failure's reason is recorded");
    assert!(
        problem.contains("500"),
        "R3.7: the reason names what the server answered: {problem}"
    );
    let status = engine.tag_status().backfill;
    assert_eq!(
        (status.tagged, status.completed),
        (2, false),
        "a run that stopped at the cap keeps the progress it recorded"
    );
    let recorded = library.store.tag_backfill().expect("read the state");
    assert_eq!(
        recorded.cursor.as_deref(),
        Some(library.assets[1].as_str()),
        "R3.7: the cursor stays where the last completed batch left it"
    );
    assert_eq!(recorded.tagged, 2, "the progress is preserved");
    assert!(
        recorded.completed_ns.is_none(),
        "R3.7: a run that stopped at the retry cap is not recorded as finished"
    );
    engine.stop();
}

/// R3.7: a transient batch failure is retried with the retry policy, and the
/// batch lands.
#[tokio::test]
async fn a_transient_batch_failure_is_retried() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let library = library_uploaded_without_the_tag(&server, &temp).await;
    server.fail(
        Operation::TagAssets,
        Fault::Status {
            code: 500,
            retry_after: None,
        },
    );

    let engine = start_engine(&server, &library.store, backfill_config(100)).await;

    assert!(
        wait_until(
            || engine.tag_status().backfill.completed,
            Duration::from_secs(20)
        )
        .await,
        "R3.7: the transient failure stopped the backfill: {:?} {:?}",
        engine.tag_status().backfill,
        engine.tag_status().problem
    );
    let tag = server.tag_id(SYNC_TAG).expect("the tag exists");
    for asset in &library.assets {
        assert_eq!(
            server.tags_of(asset),
            vec![tag.clone()],
            "R3.7: the retried batch tagged every asset: {asset}"
        );
    }
    let attempts = server
        .requests()
        .iter()
        .filter(|request| request.ends_with("/tags/assets"))
        .count();
    assert_eq!(
        attempts, 2,
        "R3.7: the refused batch was sent again, not dropped"
    );
    assert!(
        engine.tag_status().problem.is_none(),
        "a later success clears the recorded failure: {:?}",
        engine.tag_status().problem
    );
    engine.stop();
}

/// R3.7: a permanent refusal records its reason, stops the backfill, and keeps
/// the batches that already landed, so a later retry continues from there.
#[tokio::test]
async fn a_permanent_batch_failure_stops_the_backfill_and_keeps_its_progress() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let library = library_uploaded_without_the_tag(&server, &temp).await;
    // The first batch lands; the second is refused for good.
    server.fail(Operation::TagAssets, Fault::Slow(Duration::ZERO));
    server.fail(
        Operation::TagAssets,
        Fault::Status {
            code: 400,
            retry_after: None,
        },
    );

    let engine = start_engine(&server, &library.store, backfill_config(2)).await;

    assert!(
        wait_until(
            || {
                let status = engine.tag_status().backfill;
                !status.running && !status.completed && status.tagged == 2
            },
            Duration::from_secs(20)
        )
        .await,
        "R3.7: the permanent refusal did not stop the backfill: {:?}",
        engine.tag_status().backfill
    );
    let problem = engine
        .tag_status()
        .problem
        .expect("R3.7: the refusal's reason is recorded");
    assert!(
        problem.contains("400"),
        "R3.7: the reason names what the server answered: {problem}"
    );
    let recorded = library.store.tag_backfill().expect("read the state");
    assert_eq!(
        recorded.tagged, 2,
        "R3.7: the completed batch keeps its progress"
    );
    assert_eq!(
        recorded.cursor.as_deref(),
        Some(library.assets[1].as_str()),
        "R3.7: the cursor is where the run stopped"
    );
    assert!(
        recorded.completed_ns.is_none(),
        "a stopped run is not recorded as finished"
    );

    // The user fixes what the server refused; turning the tag off and on
    // resumes the run where it stopped.
    server.reset_faults();
    engine.set_tag_uploads(false).await;
    engine.set_tag_uploads(true).await;
    assert!(
        wait_until(
            || engine.tag_status().backfill.completed,
            Duration::from_secs(20)
        )
        .await,
        "R3.7: the resumed run did not finish: {:?} {:?}",
        engine.tag_status().backfill,
        engine.tag_status().problem
    );
    let tag = server.tag_id(SYNC_TAG).expect("the tag exists").clone();
    assert_eq!(
        server.tag_asset_calls().len(),
        2,
        "R3.7: the retry continues after the recorded batch instead of starting over"
    );
    for asset in &library.assets[2..] {
        assert_eq!(
            server.tags_of(asset),
            vec![tag.clone()],
            "R3.7: the retry tags the assets the stopped run never reached: {asset}"
        );
    }
    engine.stop();
}

/// R3.7: one asset the server no longer holds does not block the rest of its
/// batch. Real Immich filters inaccessible ids and answers `count: 0`.
#[tokio::test]
async fn an_asset_the_server_no_longer_holds_does_not_block_its_batch() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let library = library_uploaded_without_the_tag(&server, &temp).await;
    // The state still points at an asset the server dropped, which is what a
    // deletion made in Immich leaves behind.
    library
        .store
        .record(&sync_core::store::ItemRecord {
            path: temp.path().join("gone.jpg").display().to_string(),
            size: 11,
            mtime_ns: 1,
            sha1: sync_core::hash::Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")
                .expect("a hex SHA-1"),
            asset_id: Some("asset-the-server-does-not-hold".to_string()),
            state: ItemState::Uploaded,
            error_class: None,
            error_message: None,
            attempts: 0,
            sidecar: None,
        })
        .expect("record the removed asset");

    let engine = start_engine(&server, &library.store, backfill_config(100)).await;

    assert!(
        wait_until(
            || engine.tag_status().backfill.completed,
            Duration::from_secs(20)
        )
        .await,
        "R3.7: one unavailable asset stopped the backfill: {:?} {:?}",
        engine.tag_status().backfill,
        engine.tag_status().problem
    );
    let tag = server.tag_id(SYNC_TAG).expect("the tag exists").clone();
    for asset in &library.assets {
        assert_eq!(
            server.tags_of(asset),
            vec![tag.clone()],
            "R3.7: the assets the server holds are still tagged: {asset}"
        );
    }
    let calls = server.tag_asset_calls();
    assert_eq!(calls.len(), 1, "one batch carried the whole library");
    assert!(
        calls[0]
            .asset_ids
            .contains(&"asset-the-server-does-not-hold".to_string()),
        "the unavailable id went out in the batch the server answered 200 for"
    );
    assert_eq!(
        engine.tag_status().backfill.tagged,
        5,
        "the batch counts every id it carried"
    );
    engine.stop();
}

/// R3.7: turning the toggle off pauses the backfill at the batch boundary and
/// turning it on again resumes it, without losing the batches that landed.
#[tokio::test]
async fn the_toggle_pauses_the_backfill_and_turning_it_on_resumes_it() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let library = library_of_six(&server, &temp).await;
    // Hold the first batch, so the toggle lands while it is the work in
    // flight.
    server.fail(
        Operation::TagAssets,
        Fault::Slow(Duration::from_millis(700)),
    );

    let engine = start_engine(&server, &library.store, backfill_config(2)).await;
    assert!(
        wait_until(
            || engine.tag_status().backfill.running,
            Duration::from_secs(20)
        )
        .await,
        "the backfill did not start"
    );
    engine.set_tag_uploads(false).await;
    assert!(
        wait_until(
            || {
                let status = engine.tag_status().backfill;
                !status.running && status.tagged == 2 && !status.completed
            },
            Duration::from_secs(20)
        )
        .await,
        "R3.7: the backfill did not pause after the batch in flight: {:?}",
        engine.tag_status().backfill
    );
    assert_eq!(
        server.tag_asset_calls().len(),
        1,
        "R3.7: a paused backfill sends no further batch"
    );
    let paused = library.store.tag_backfill().expect("read the state");
    assert_eq!(
        paused.cursor.as_deref(),
        Some(library.assets[1].as_str()),
        "the paused run records the batch it completed"
    );
    assert!(paused.completed_ns.is_none(), "nothing finished");

    engine.set_tag_uploads(true).await;
    assert!(
        wait_until(
            || engine.tag_status().backfill.completed,
            Duration::from_secs(20)
        )
        .await,
        "R3.7: turning the toggle on did not resume the backfill: {:?}",
        engine.tag_status().backfill
    );
    assert_eq!(
        engine.tag_status().backfill.tagged,
        6,
        "the resumed run finishes the library"
    );
    assert_eq!(
        server.tag_asset_calls().len(),
        3,
        "two more batches picked up where the paused run stopped"
    );
    engine.stop();
}

/// R3.7: the backfill runs off the upload path, so an upload settles while a
/// tag batch is still on the wire.
#[tokio::test]
async fn uploads_settle_while_the_backfill_runs() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let library = library_uploaded_without_the_tag(&server, &temp).await;
    server.fail(Operation::TagAssets, Fault::Slow(Duration::from_secs(3)));

    let engine = start_engine(&server, &library.store, backfill_config(100)).await;
    assert!(
        wait_until(
            || {
                server
                    .requests()
                    .iter()
                    .any(|request| request.ends_with("/tags/assets"))
            },
            Duration::from_secs(20)
        )
        .await,
        "the backfill never put a batch on the wire"
    );
    let late = write_file(
        &temp.path().join("photos"),
        "late.jpg",
        b"uploaded meanwhile",
    );
    engine.sync_now();
    assert!(
        wait_until(|| succeeded(&library.store, &late), Duration::from_secs(20)).await,
        "R3.7: the upload waited behind the backfill"
    );
    assert!(
        engine.tag_status().backfill.running,
        "the sleep must be the slow batch, not a finished backfill"
    );
    assert!(
        wait_until(
            || engine.tag_status().backfill.completed,
            Duration::from_secs(20)
        )
        .await,
        "the backfill did not finish after the slow batch"
    );
    engine.stop();
}
