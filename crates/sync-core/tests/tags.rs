//! The sync tag against the offline Immich double (SPEC R3.7).
//!
//! The sync tag is the one tag `immich-sync` the client adds to the assets it
//! syncs (GLOSSARY). These tests drive the real engine and assert both sides:
//! the tag the double holds on an asset, and the requests the client put on the
//! wire, because the double is more forgiving than the real server.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::wait_until;
use mock_immich::{Fault, MockImmich, Operation, TagAssetCall, TagUpsertCall};
use sync_core::backoff::RetryPolicy;
use sync_core::connection::{AuthMethod, Connection, Secret, SharedSession, SignIn};
use sync_core::credential::MemoryCredentialStore;
use sync_core::engine::{Engine, EngineConfig};
use sync_core::store::{AlbumScope, ItemState, SourceFolder, StateStore};
use sync_core::tags::SYNC_TAG;
use sync_core::tracking::Tracking;

/// Starts the mock and signs in, returning the session the engine attaches to
/// every request (R1.4).
async fn signed_in(server: &MockImmich) -> SharedSession {
    signed_in_with(server, AuthMethod::Password, "correct horse").await
}

/// Signs in with the credential the test names, so a test can drive the
/// API-key permission path of R3.7.
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

/// The retry policy the tag tests use: fast, deterministic waits.
fn test_retry() -> RetryPolicy {
    RetryPolicy {
        base: Duration::from_millis(20),
        max: Duration::from_millis(80),
        max_retry_after: Duration::from_secs(5),
        seed: Some(7),
    }
}

/// A config with the sync tag on, one worker, and no watcher (R3.7).
fn tagged_config() -> EngineConfig {
    EngineConfig {
        tag_uploaded_assets: true,
        attempt_cap: 3,
        retry: test_retry(),
        breaker_threshold: 3,
        attention_threshold: 3,
        breaker_cooldown: Duration::from_millis(120),
        concurrency: 1,
        watch: false,
        ..EngineConfig::default()
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

/// The state one item is in.
fn state_of(store: &StateStore, path: &Path) -> ItemState {
    store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the row exists")
        .state
}

/// The asset id one item currently points at.
fn asset_of(store: &StateStore, path: &Path) -> String {
    store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the row exists")
        .asset_id
        .expect("a recorded asset id (R3.6)")
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

/// The number of recorded versions of one item.
fn chain_len(store: &StateStore, file: &Path) -> usize {
    store
        .versions(&file.display().to_string())
        .expect("read the version chain")
        .len()
}

/// A config with the sync tag off, set explicitly rather than read from the
/// library default: `engine.rs`'s `the_default_config_matches_the_spec` pins that
/// default (R3.7). The app's stored setting is on by default; these tests cover
/// the off path.
fn untagged_config() -> EngineConfig {
    EngineConfig {
        tag_uploaded_assets: false,
        ..tagged_config()
    }
}

/// R3.7: an upload the server creates gets the sync tag.
#[tokio::test]
async fn a_created_upload_gets_the_sync_tag() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "first.jpg", b"tagged bytes");
    add_folder(&store, &photos);

    let engine = start_engine(&server, &store, tagged_config()).await;

    assert!(
        wait_until(|| succeeded(&store, &file), Duration::from_secs(20)).await,
        "the upload did not finish"
    );
    let asset = asset_of(&store, &file);
    let tag = server
        .tag_id(SYNC_TAG)
        .expect("R3.7: the run created the sync tag");
    // The item is recorded as uploaded before the tag call runs (R3.7: the tag
    // is auxiliary), so the wait is on the tag itself.
    assert!(
        wait_until(
            || !server.tags_of(&asset).is_empty(),
            Duration::from_secs(10)
        )
        .await,
        "R3.7: the sync tag was never applied to the uploaded asset"
    );
    assert_eq!(
        server.upsert_calls(),
        vec![TagUpsertCall {
            tags: vec![SYNC_TAG.to_string()],
        }],
        "R3.7: one upsert resolves the tag for the whole run"
    );
    assert_eq!(
        server.tag_asset_calls(),
        vec![TagAssetCall {
            tag_ids: vec![tag.clone()],
            asset_ids: vec![asset.clone()],
        }],
        "R3.7: the tag is applied to the asset the upload settled"
    );
    assert_eq!(
        server.tags_of(&asset),
        vec![tag],
        "R3.7: the uploaded asset carries the sync tag"
    );
    assert_eq!(
        state_of(&store, &file),
        ItemState::Uploaded,
        "R3.2: the upload is a success"
    );
    engine.stop();
}

/// R3.7: a duplicate the server already held gets the sync tag too, so an
/// asset found by checksum is as findable as one this run uploaded.
#[tokio::test]
async fn a_duplicate_upload_gets_the_sync_tag() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let asset = bytes_already_on_the_server(&server, b"already here").await;
    let file = write_file(&photos, "duplicate.jpg", b"already here");
    add_folder(&store, &photos);

    let engine = start_engine(&server, &store, tagged_config()).await;

    assert!(
        wait_until(|| succeeded(&store, &file), Duration::from_secs(20)).await,
        "the duplicate did not settle"
    );
    assert_eq!(
        state_of(&store, &file),
        ItemState::Duplicate,
        "R3.2: the server already held these bytes"
    );
    let tag = server
        .tag_id(SYNC_TAG)
        .expect("R3.7: the run created the sync tag");
    assert!(
        wait_until(
            || server.tags_of(&asset) == vec![tag.clone()],
            Duration::from_secs(10)
        )
        .await,
        "R3.7: the duplicate never got the sync tag: {:?}",
        server.tags_of(&asset)
    );
    assert_eq!(
        server.tag_asset_calls(),
        vec![TagAssetCall {
            tag_ids: vec![tag],
            asset_ids: vec![asset],
        }],
        "R3.7: the tag is applied to the asset the checksum matched"
    );
    engine.stop();
}

/// R3.7 + R4.2: an edited file's new version gets the tag, and the version it
/// replaces keeps its own.
#[tokio::test]
async fn a_new_version_gets_the_sync_tag_and_the_old_version_keeps_it() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "edited.jpg", b"first bytes");
    add_folder(&store, &photos);

    let engine = start_engine(&server, &store, tagged_config()).await;

    assert!(
        wait_until(|| succeeded(&store, &file), Duration::from_secs(20)).await,
        "the first upload did not finish"
    );
    let first = asset_of(&store, &file);
    let tag = server
        .tag_id(SYNC_TAG)
        .expect("R3.7: the run created the sync tag");
    assert!(
        wait_until(
            || server.tags_of(&first) == vec![tag.clone()],
            Duration::from_secs(10)
        )
        .await,
        "the first version never got the sync tag"
    );

    std::fs::write(&file, b"second bytes, longer").expect("edit the file in place");
    engine.sync_now();
    assert!(
        wait_until(|| chain_len(&store, &file) == 2, Duration::from_secs(20)).await,
        "R4.4: the edit did not become a second version: {:?}",
        store.versions(&file.display().to_string())
    );
    let newest = asset_of(&store, &file);
    assert_ne!(newest, first);
    assert!(
        wait_until(
            || server.tags_of(&newest) == vec![tag.clone()],
            Duration::from_secs(10)
        )
        .await,
        "R3.7: the new version never got the sync tag: {:?}",
        server.tags_of(&newest)
    );
    assert_eq!(
        server.tags_of(&first),
        vec![tag],
        "R3.7: the old version keeps the tag; tagging is not a move"
    );
    engine.stop();
}

/// R3.7: the setting off means no tag is created and no asset is tagged.
#[tokio::test]
async fn the_sync_tag_is_not_created_while_the_setting_is_off() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "plain.jpg", b"untagged bytes");
    add_folder(&store, &photos);

    let engine = start_engine(&server, &store, untagged_config()).await;

    assert!(
        wait_until(|| succeeded(&store, &file), Duration::from_secs(20)).await,
        "the upload did not finish"
    );
    assert!(!engine.tag_status().enabled, "the setting is off");
    assert!(
        server.upsert_calls().is_empty(),
        "R3.7: an off setting creates no tag: {:?}",
        server.upsert_calls()
    );
    assert!(
        server.tag_asset_calls().is_empty(),
        "R3.7: an off setting tags no asset"
    );
    engine.stop();
}

/// R3.7: a tag failure leaves the item uploaded and records the reason.
#[tokio::test]
async fn a_tag_failure_leaves_the_upload_successful_and_records_the_reason() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "tagrefused.jpg", b"still uploaded");
    add_folder(&store, &photos);
    server.fail(
        Operation::TagAssets,
        Fault::ServerMessage {
            code: 500,
            message: "the tag service is down".to_string(),
        },
    );

    let engine = start_engine(&server, &store, tagged_config()).await;

    assert!(
        wait_until(|| succeeded(&store, &file), Duration::from_secs(20)).await,
        "the upload did not finish"
    );
    assert!(
        wait_until(
            || engine.tag_status().problem.is_some(),
            Duration::from_secs(10)
        )
        .await,
        "R3.7: the tag failure was not recorded"
    );
    let problem = engine.tag_status().problem.expect("the recorded reason");
    assert!(
        problem.contains("500") && problem.contains("the tag service is down"),
        "R3.7: the reason names what happened: {problem}"
    );
    assert_eq!(
        state_of(&store, &file),
        ItemState::Uploaded,
        "R3.7: a tag failure does not turn a successful upload into a failed item"
    );
    assert!(
        engine
            .failed_items()
            .expect("read the parked list")
            .is_empty(),
        "R3.7: a tag failure parks nothing"
    );
    assert!(
        server.tags_of(&asset_of(&store, &file)).is_empty(),
        "the tag really did not land"
    );
    engine.stop();
}

/// R3.7: an API key that lacks one of the three permissions tags nothing, and
/// the reason names the permission.
#[tokio::test]
async fn an_api_key_missing_a_permission_gets_no_tag_and_a_named_reason() {
    let server = MockImmich::start().await;
    server.set_api_key("test-key");
    // The upload permission is granted, so the item can upload: the tag alone
    // is refused, which is the state the Settings section has to explain.
    server.set_api_key_permissions(&["asset.upload", "tag.create", "tag.asset"]);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let file = write_file(&photos, "keyed.jpg", b"keyed bytes");
    add_folder(&store, &photos);

    let session = signed_in_with(&server, AuthMethod::ApiKey, "test-key").await;
    let engine = Arc::new(
        Engine::new(
            Arc::new(Tracking::new(store.clone())),
            session,
            tagged_config(),
        )
        .expect("build the engine"),
    );
    engine.start();

    assert!(
        wait_until(|| succeeded(&store, &file), Duration::from_secs(20)).await,
        "the upload did not finish"
    );
    assert!(
        wait_until(
            || engine.tag_status().problem.is_some(),
            Duration::from_secs(10)
        )
        .await,
        "R3.7: the missing permission was not reported"
    );
    let problem = engine.tag_status().problem.expect("the recorded reason");
    assert!(
        problem.contains("asset.update"),
        "R3.7: the message names the missing permission: {problem}"
    );
    assert!(
        server.upsert_calls().is_empty(),
        "R3.7: a key that may not create the tag does not send the upsert"
    );
    assert!(server.tag_asset_calls().is_empty());
    assert_eq!(
        state_of(&store, &file),
        ItemState::Uploaded,
        "R3.7: the upload stands without its tag"
    );
    engine.stop();
}

/// R3.7: turning the setting off stops new tagging and leaves the tags that
/// are already applied alone.
#[tokio::test]
async fn turning_the_tag_off_stops_new_tagging_and_keeps_existing_tags() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let first = write_file(&photos, "first.jpg", b"tagged first");
    add_folder(&store, &photos);

    let engine = start_engine(&server, &store, tagged_config()).await;

    assert!(
        wait_until(|| succeeded(&store, &first), Duration::from_secs(20)).await,
        "the first upload did not finish"
    );
    let tagged = asset_of(&store, &first);
    let tag = server
        .tag_id(SYNC_TAG)
        .expect("R3.7: the run created the sync tag");
    assert!(
        wait_until(
            || server.tags_of(&tagged) == vec![tag.clone()],
            Duration::from_secs(10)
        )
        .await,
        "the first asset never got the tag"
    );

    engine.set_tag_uploads(false).await;
    let second = write_file(&photos, "second.jpg", b"untagged second");
    engine.sync_now();
    assert!(
        wait_until(|| succeeded(&store, &second), Duration::from_secs(20)).await,
        "the second upload did not finish"
    );
    assert!(!engine.tag_status().enabled, "the live setting is off");
    assert!(
        server.tags_of(&asset_of(&store, &second)).is_empty(),
        "R3.7: no new asset is tagged after the setting is turned off"
    );
    assert_eq!(
        server.tags_of(&tagged),
        vec![tag],
        "R3.7: turning the setting off does not untag what is already tagged"
    );
    assert_eq!(
        server.tag_asset_calls().len(),
        1,
        "only the first upload was tagged"
    );
    engine.stop();
}

/// R3.7: a tag the user deleted in Immich is created again on the next run.
#[tokio::test]
async fn a_deleted_tag_is_created_again_on_the_next_run() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let first = write_file(&photos, "first.jpg", b"tagged first");
    add_folder(&store, &photos);

    let engine = start_engine(&server, &store, tagged_config()).await;
    assert!(
        wait_until(|| succeeded(&store, &first), Duration::from_secs(20)).await,
        "the first upload did not finish"
    );
    let first_asset = asset_of(&store, &first);
    let deleted = server
        .tag_id(SYNC_TAG)
        .expect("R3.7: the run created the sync tag");
    assert!(
        wait_until(
            || !server.tags_of(&first_asset).is_empty(),
            Duration::from_secs(10)
        )
        .await,
        "the first asset never got the tag"
    );
    engine.stop();

    // The user deletes the tag in Immich, which drops it from the assets too.
    server.delete_tag(&deleted);
    assert_eq!(server.tag_id(SYNC_TAG), None);

    let second = write_file(&photos, "second.jpg", b"tagged second");
    let restarted = start_engine(&server, &store, tagged_config()).await;
    assert!(
        wait_until(|| succeeded(&store, &second), Duration::from_secs(20)).await,
        "the second upload did not finish"
    );
    let second_asset = asset_of(&store, &second);
    let recreated = server
        .tag_id(SYNC_TAG)
        .expect("R3.7: the second run created the tag again");
    assert_ne!(recreated, deleted, "the recreated tag is a new tag");
    assert!(
        wait_until(
            || server.tags_of(&second_asset) == vec![recreated.clone()],
            Duration::from_secs(10)
        )
        .await,
        "the second asset never got the recreated tag"
    );
    assert_eq!(
        server.upsert_calls().len(),
        2,
        "R3.7: one upsert per run, not one per upload"
    );
    restarted.stop();
}

/// R3.7: a refused tag call drops the cached id, so the next asset resolves
/// the tag again instead of reusing an id the server no longer took.
#[tokio::test]
async fn a_refused_tag_call_resolves_the_tag_again_for_the_next_asset() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    add_folder(&store, &photos);

    // The run resolves the tag once at start, before any asset needs it.
    let engine = start_engine(&server, &store, tagged_config()).await;
    assert!(
        wait_until(|| server.upsert_calls().len() == 1, Duration::from_secs(10)).await,
        "R3.7: the start did not resolve the tag: {:?}",
        server.upsert_calls()
    );
    server.fail(
        Operation::TagAssets,
        Fault::ServerMessage {
            code: 500,
            message: "the tag service is down".to_string(),
        },
    );

    let first = write_file(&photos, "first.jpg", b"tagrefused first");
    engine.sync_now();
    assert!(
        wait_until(|| succeeded(&store, &first), Duration::from_secs(20)).await,
        "the first upload did not finish"
    );
    assert!(
        wait_until(
            || engine.tag_status().problem.is_some(),
            Duration::from_secs(10)
        )
        .await,
        "R3.7: the refused tag call was not recorded"
    );

    // The second asset must resolve the tag again: the refused call dropped
    // the id the server answered for this run.
    let second = write_file(&photos, "second.jpg", b"tagrefused second");
    engine.sync_now();
    assert!(
        wait_until(|| succeeded(&store, &second), Duration::from_secs(20)).await,
        "the second upload did not finish"
    );
    assert!(
        wait_until(
            || server.tags_of(&asset_of(&store, &second)).len() == 1,
            Duration::from_secs(10)
        )
        .await,
        "R3.7: the second asset never got the tag"
    );
    assert_eq!(
        server.upsert_calls().len(),
        2,
        "R3.7: a refused tag call drops the cached id, so the next asset upserts again"
    );
    let attempts = server
        .requests()
        .iter()
        .filter(|request| request.ends_with("/tags/assets"))
        .count();
    assert_eq!(attempts, 2, "R3.7: both tag attempts reached the server");
    engine.stop();
}

/// R3.7: one settled permission refusal covers the run, so a folder of uploads
/// costs one probe of the key, not one per asset.
#[tokio::test]
async fn a_settled_permission_refusal_is_not_probed_again_for_each_upload() {
    let server = MockImmich::start().await;
    server.set_api_key("test-key");
    // The upload permission is granted, so the items upload: the tag alone is
    // refused, which is the state the run has to remember.
    server.set_api_key_permissions(&["asset.upload", "tag.create", "tag.asset"]);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    add_folder(&store, &photos);

    let session = signed_in_with(&server, AuthMethod::ApiKey, "test-key").await;
    let engine = Arc::new(
        Engine::new(
            Arc::new(Tracking::new(store.clone())),
            session,
            tagged_config(),
        )
        .expect("build the engine"),
    );
    engine.start();
    assert!(
        wait_until(
            || engine.tag_status().problem.is_some(),
            Duration::from_secs(10)
        )
        .await,
        "R3.7: the start did not settle the key's refusal"
    );
    let reads = server.api_key_me_requests();
    assert_eq!(
        reads, 1,
        "R3.7: the start probe is the run's one permission read"
    );

    let first = write_file(&photos, "first.jpg", b"refused first");
    engine.sync_now();
    assert!(
        wait_until(|| succeeded(&store, &first), Duration::from_secs(20)).await,
        "the first upload did not finish"
    );
    let second = write_file(&photos, "second.jpg", b"refused second");
    engine.sync_now();
    assert!(
        wait_until(|| succeeded(&store, &second), Duration::from_secs(20)).await,
        "the second upload did not finish"
    );
    // Each tag call runs right after its own item settles, on the same worker.
    // The short window is what lets a repeated probe show up here instead of
    // after the assertions.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        server.api_key_me_requests(),
        reads,
        "R3.7: a settled refusal is replayed, so two uploads cost no second probe"
    );
    assert!(
        server.upsert_calls().is_empty(),
        "R3.7: the refused key creates no tag: {:?}",
        server.upsert_calls()
    );
    assert!(
        server.tag_asset_calls().is_empty(),
        "R3.7: the refused key tags no asset"
    );
    engine.stop();
}

/// R3.7: a sign-in that fixes the key's permissions clears the refusal the old
/// credential settled on, without a new engine: the next upload probes once and
/// is tagged.
#[tokio::test]
async fn a_new_sign_in_clears_the_settled_permission_refusal() {
    let server = MockImmich::start().await;
    server.set_api_key("test-key");
    // The upload permission is granted, so the item uploads: the tag alone is
    // refused, which is the refusal the run has to remember.
    server.set_api_key_permissions(&["asset.upload", "tag.create", "tag.asset"]);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    add_folder(&store, &photos);

    let session = signed_in_with(&server, AuthMethod::ApiKey, "test-key").await;
    let engine = Arc::new(
        Engine::new(
            Arc::new(Tracking::new(store.clone())),
            session.clone(),
            tagged_config(),
        )
        .expect("build the engine"),
    );
    engine.start();
    assert!(
        wait_until(
            || engine.tag_status().problem.is_some(),
            Duration::from_secs(10)
        )
        .await,
        "R3.7: the start did not settle the key's refusal"
    );
    assert_eq!(
        server.api_key_me_requests(),
        1,
        "R3.7: the start probe is the credential's one permission read"
    );

    // The user grants the missing permission and signs in again with the same
    // key; the running engine keeps the tagger it was built with.
    server.set_api_key_permissions(&["asset.upload", "tag.create", "tag.asset", "asset.update"]);
    session
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("test-key"),
            method: AuthMethod::ApiKey,
        })
        .await;

    let file = write_file(&photos, "regranted.jpg", b"regranted bytes");
    engine.sync_now();
    assert!(
        wait_until(|| succeeded(&store, &file), Duration::from_secs(20)).await,
        "the upload did not finish"
    );
    let asset = asset_of(&store, &file);
    assert!(
        wait_until(
            || !server.tags_of(&asset).is_empty(),
            Duration::from_secs(10)
        )
        .await,
        "R3.7: the asset never got the tag after the key was fixed: {:?}",
        server.tags_of(&asset)
    );
    let tag = server
        .tag_id(SYNC_TAG)
        .expect("R3.7: the fixed key created the sync tag");
    assert_eq!(
        server.tags_of(&asset),
        vec![tag],
        "R3.7: the asset carries the tag the fixed key created"
    );
    assert_eq!(
        server.api_key_me_requests(),
        2,
        "R3.7: the new sign-in costs exactly one new permission probe"
    );
    assert_eq!(
        server.upsert_calls().len(),
        1,
        "R3.7: the fixed key creates the tag once"
    );
    assert!(
        wait_until(
            || engine.tag_status().problem.is_none(),
            Duration::from_secs(10)
        )
        .await,
        "R3.7: the notice cleared once the tag landed"
    );
    engine.stop();
}
