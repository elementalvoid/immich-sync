//! XMP sidecars and live photos through the engine (SPEC R3.3).
//!
//! Every test drives the real pipeline: a real directory, the real watcher or
//! reconciliation scan, the real state store, and the mock server's record of
//! what arrived and in which order.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mock_immich::{Fault, MockImmich, Operation};
use sync_core::connection::{AuthMethod, Connection, Secret, SharedSession, SignIn};
use sync_core::credential::MemoryCredentialStore;
use sync_core::engine::{Engine, EngineConfig};
use sync_core::hash::Sha1;
use sync_core::store::{AlbumScope, ItemState, SourceFolder, StateStore};
use sync_core::tracking::Tracking;

use common::wait_until;

/// Starts the mock and signs in, returning the session the engine attaches to
/// every request (R1.4): #7 owns the one re-login of a 401.
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

/// Reopens the store a previous run left behind, at the same path.
fn reopen(temp: &tempfile::TempDir) -> Arc<StateStore> {
    store_in(temp)
}

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

/// The scan-only configuration: the reconciliation scan finds every fixture, so
/// one batch carries the whole folder and the order is the folder's.
fn scan_only() -> EngineConfig {
    EngineConfig {
        watch: false,
        scan_interval: Duration::from_secs(3600),
        ..EngineConfig::default()
    }
}

/// A watcher configuration with a short debounce, for the watch-event path.
fn watching() -> EngineConfig {
    EngineConfig {
        watch: true,
        watch_debounce: Duration::from_millis(200),
        scan_interval: Duration::from_secs(3600),
        ..EngineConfig::default()
    }
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

/// Whether every path reached a terminal success state.
fn all_terminal(store: &StateStore, paths: &[PathBuf]) -> bool {
    paths.iter().all(|path| {
        matches!(
            store.item(&path.display().to_string()),
            Ok(Some(row)) if row.state.is_terminal_success()
        )
    })
}

/// The asset id the store recorded for `path`, once it uploaded.
fn asset_id(store: &StateStore, path: &Path) -> String {
    store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the row exists")
        .asset_id
        .expect("the row carries an asset id")
}

/// The digest of the sidecar fixture, read from the literal rather than
/// recomputed, so the test cannot agree with a wrong hash.
fn sidecar_fixture_sha1() -> Sha1 {
    Sha1::from_hex(SIDECAR_SHA1_HEX).expect("a 40-character hex digest")
}

/// The sidecar fixture, and its SHA-1 as an independent source of truth.
const SIDECAR_XMP: &[u8] = b"<x:xmpmeta>sidecar</x:xmpmeta>";
const SIDECAR_SHA1_HEX: &str = "f8d2e9cfb0466aeebda758acb4dbbdfa7ce77140";

#[tokio::test]
async fn a_photo_and_its_sidecar_upload_as_one_asset() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let photo = write_file(&photos, "IMG_0001.jpg", b"photo bytes");
    let sidecar = write_file(&photos, "IMG_0001.jpg.xmp", SIDECAR_XMP);
    add_folder(&store, &photos, true);

    let engine = start_engine(&server, &store, scan_only()).await;

    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&photo)),
            Duration::from_secs(15)
        )
        .await,
        "the photo did not upload"
    );
    let id = asset_id(&store, &photo);
    assert!(
        server.has_sidecar(&id),
        "R3.3: the sidecarData part arrived"
    );
    assert_eq!(
        server.uploads().len(),
        1,
        "R3.3: the sidecar is not an upload of its own"
    );
    assert_eq!(server.uploads()[0].filename, "IMG_0001.jpg");
    assert_eq!(
        store
            .item(&sidecar.display().to_string())
            .expect("read the sidecar row"),
        None,
        "R3.3: a sidecar is never a sync item"
    );
    let row = store
        .item(&photo.display().to_string())
        .expect("read")
        .expect("the photo row");
    assert_eq!(
        row.sidecar.map(|sidecar| sidecar.sha1),
        Some(sidecar_fixture_sha1()),
        "R4.5: the store remembers which sidecar the asset carries"
    );
    engine.stop();
}

#[tokio::test]
async fn a_late_sidecar_cannot_be_attached_and_the_client_says_so() {
    // R3.3 deviation, recorded rather than papered over: `POST /assets` is the
    // only Stable operation that carries a `sidecarData` part, and Immich
    // answers a checksum it already holds as `duplicate` from the header,
    // before multer parses the form. There is no attach afterwards. So a
    // sidecar that appears after the asset uploaded is sent once, is not
    // recorded as sent, and parks the item where the user can see it.
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let photo = write_file(&photos, "IMG_0001.jpg", b"photo bytes");
    add_folder(&store, &photos, true);

    // The first pass sees the photo alone: the camera writes the sidecar after.
    let engine = start_engine(&server, &store, scan_only()).await;
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&photo)),
            Duration::from_secs(15)
        )
        .await,
        "the photo did not upload"
    );
    let first_asset = asset_id(&store, &photo);
    engine.stop();
    assert_eq!(server.upload_requests(), 1);

    let sidecar = write_file(&photos, "IMG_0001.jpg.xmp", SIDECAR_XMP);
    let engine = start_engine(&server, &store, scan_only()).await;
    assert!(
        wait_until(
            || {
                store
                    .item(&photo.display().to_string())
                    .ok()
                    .flatten()
                    .is_some_and(|row| row.state == ItemState::FailedPermanent)
            },
            Duration::from_secs(15)
        )
        .await,
        "the late sidecar did not park the item"
    );

    let row = store
        .item(&photo.display().to_string())
        .expect("read")
        .expect("the photo row");
    assert_eq!(
        row.sidecar, None,
        "the request was never read, so the asset carries no sidecar"
    );
    assert_eq!(
        row.asset_id.as_deref(),
        Some(first_asset.as_str()),
        "R3.2: the media is on the server, and the row says where"
    );
    assert!(
        !server.has_sidecar(&first_asset),
        "the truth: the sidecar did not reach the asset"
    );
    assert_eq!(
        server.upload_requests(),
        2,
        "the sidecar was sent once, in the only request that can carry one"
    );

    // The folder says why, while the answer is fresh (R8.2).
    let status = engine.status();
    assert!(
        status.folders[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("sidecar")),
        "the folder says why the sidecar is not on the asset: {:?}",
        status.folders[0].last_error
    );

    // A further pass must not keep re-POSTing an answer the server has already
    // given.
    engine.sync_now();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        server.upload_requests(),
        2,
        "R6.3: a permanent answer is not retried on every scan"
    );
    assert_eq!(
        store.item(&sidecar.display().to_string()).expect("read"),
        None,
        "R3.3: the late sidecar is still not an item of its own"
    );
    engine.stop();

    // The parked item is durable: a restarted client still counts it as
    // failed, so the condition does not disappear with the process.
    let engine = start_engine(&server, &store, scan_only()).await;
    assert_eq!(
        engine.status().folders[0].counts.failed,
        1,
        "the store keeps the item in the failed count the user sees"
    );
    assert_eq!(server.upload_requests(), 2, "and it is not sent again");
    engine.stop();
}

#[tokio::test]
async fn an_unchanged_sidecar_is_not_sent_again() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let photo = write_file(&photos, "IMG_0001.jpg", b"photo bytes");
    write_file(&photos, "IMG_0001.jpg.xmp", SIDECAR_XMP);
    add_folder(&store, &photos, true);

    let engine = start_engine(&server, &store, scan_only()).await;
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&photo)),
            Duration::from_secs(15)
        )
        .await
    );
    engine.stop();

    // A second pass over the same folder finds nothing new to do (R4.5).
    let engine = start_engine(&server, &store, scan_only()).await;
    engine.sync_now();
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert_eq!(
        server.upload_requests(),
        1,
        "R4.5: the same sidecar does not upload again"
    );
    engine.stop();
}

#[tokio::test]
async fn the_motion_video_uploads_first_and_the_photo_links_it() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let photo = write_file(&photos, "IMG_0002.jpg", b"photo bytes");
    let video = write_file(&photos, "IMG_0002.mov", b"motion bytes");
    add_folder(&store, &photos, true);

    let engine = start_engine(&server, &store, scan_only()).await;
    assert!(
        wait_until(
            || all_terminal(&store, &[photo.clone(), video.clone()]),
            Duration::from_secs(15)
        )
        .await,
        "the live photo pair did not upload"
    );

    let uploads = server.uploads();
    assert_eq!(uploads.len(), 2, "one request per asset: {uploads:?}");
    assert_eq!(
        (uploads[0].filename.as_str(), uploads[1].filename.as_str()),
        ("IMG_0002.mov", "IMG_0002.jpg"),
        "R3.3: the motion video goes on the wire before the photo that links it"
    );
    let video_asset = asset_id(&store, &video);
    assert_eq!(
        uploads[1].live_photo_video_id.as_deref(),
        Some(video_asset.as_str()),
        "R3.3: the photo request carries livePhotoVideoId"
    );
    assert_eq!(
        server.live_photo_video_id(&asset_id(&store, &photo)),
        Some(video_asset),
        "R3.3: the server links the photo to the video asset"
    );
    engine.stop();
}

#[tokio::test]
async fn a_claimed_video_is_released_and_uploads_on_its_own() {
    // R3.3: a photo claims its motion video, so the video is not an item of its
    // own in that batch. The claim is a borrow, not a deletion: when the photo
    // cannot upload it either, the video still uploads, and the folder's
    // pending count says what is still in flight.
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let photo = write_file(&photos, "IMG_0006.jpg", b"photo bytes");
    let video = write_file(&photos, "IMG_0006.mov", b"motion bytes");
    add_folder(&store, &photos, true);
    // The photo is refused by the checksum check, so it never uploads and
    // never claims the video itself.
    server.reject_format(
        &sync_core::hash::sha1_file(&photo)
            .expect("hash the photo fixture")
            .base64(),
    );

    let engine = start_engine(&server, &store, scan_only()).await;
    assert!(
        wait_until(
            || !store
                .item(&photo.display().to_string())
                .expect("read the photo row")
                .is_some_and(|row| row.state.is_terminal_success()),
            Duration::from_secs(15)
        )
        .await,
        "the refused photo was not parked"
    );
    assert!(
        !all_terminal(&store, std::slice::from_ref(&video)),
        "the video is still pending work: the photo never claimed it for real"
    );

    // The next scan re-prepares the released item (R2.3).
    engine.sync_now();
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&video)),
            Duration::from_secs(15)
        )
        .await,
        "the claimed video was never released"
    );

    let status = engine.status();
    assert_eq!(
        status.folders[0].counts.pending, 0,
        "R8.1: a released item is not pending once it settled: {:?}",
        status.folders[0].counts
    );
    engine.stop();
}

#[tokio::test]
async fn a_video_and_a_photo_without_each_other_upload_as_ordinary_assets() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let lone_video = write_file(&photos, "clip.mov", b"motion bytes");
    let lone_photo = write_file(&photos, "solo.jpg", b"photo bytes");
    add_folder(&store, &photos, true);

    let engine = start_engine(&server, &store, scan_only()).await;
    assert!(
        wait_until(
            || all_terminal(&store, &[lone_video.clone(), lone_photo.clone()]),
            Duration::from_secs(15)
        )
        .await,
        "an unpaired file must not be lost"
    );

    let mut uploads = server.uploads();
    uploads.sort_by(|left, right| left.filename.cmp(&right.filename));
    assert_eq!(
        (uploads[0].filename.as_str(), uploads[1].filename.as_str()),
        ("clip.mov", "solo.jpg"),
        "both files uploaded as assets of their own"
    );
    assert!(
        uploads
            .iter()
            .all(|record| record.live_photo_video_id.is_none()),
        "R3.3: nothing pairs with nothing"
    );
    assert_eq!(
        server.live_photo_video_id(&asset_id(&store, &lone_photo)),
        None
    );
    engine.stop();
}

#[tokio::test]
async fn a_failed_motion_video_does_not_lose_or_mislink_the_photo() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let photo = write_file(&photos, "IMG_0003.jpg", b"photo bytes");
    let video = write_file(&photos, "IMG_0003.mov", b"motion bytes");
    add_folder(&store, &photos, true);
    // The video is the first upload the pair sends, so the fault lands on it.
    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 503,
            retry_after: None,
        },
    );

    let engine = start_engine(&server, &store, scan_only()).await;
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&photo)),
            Duration::from_secs(15)
        )
        .await,
        "the photo must not be lost with its video"
    );

    assert_eq!(
        server.live_photo_video_id(&asset_id(&store, &photo)),
        None,
        "R3.3: a failed video never leaves a link to an asset that does not exist"
    );
    let video_row = store
        .item(&video.display().to_string())
        .expect("read")
        .expect("the video row");
    assert!(
        !video_row.state.is_terminal_success(),
        "R6.1: the failed video stays parked for the retry policy (#7): {video_row:?}"
    );
    engine.stop();
}

#[tokio::test]
async fn the_pairing_resolves_again_after_the_store_is_reopened() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let video = write_file(&photos, "IMG_0004.mov", b"motion bytes");
    add_folder(&store, &photos, true);

    // The camera writes the video first; the photo lands while the client is
    // stopped, as it does when the tray app is closed for the night.
    let engine = start_engine(&server, &store, scan_only()).await;
    assert!(
        wait_until(
            || all_terminal(&store, std::slice::from_ref(&video)),
            Duration::from_secs(15)
        )
        .await,
        "the motion video did not upload"
    );
    engine.stop();
    let video_asset = asset_id(&store, &video);
    drop(store);

    let photo = write_file(&photos, "IMG_0004.jpg", b"photo bytes");
    let reopened = reopen(&temp);
    let engine = start_engine(&server, &reopened, scan_only()).await;
    assert!(
        wait_until(
            || all_terminal(&reopened, std::slice::from_ref(&photo)),
            Duration::from_secs(15)
        )
        .await,
        "the photo did not upload after the restart"
    );

    assert_eq!(
        server.live_photo_video_id(&asset_id(&reopened, &photo)),
        Some(video_asset),
        "R3.3: the video's asset id comes from the store, so it survives a restart"
    );
    assert_eq!(
        server.uploads().len(),
        2,
        "the video was not sent again: {uploads:?}",
        uploads = server.uploads()
    );
    engine.stop();
}

#[tokio::test]
async fn the_watcher_path_pairs_a_sidecar_and_a_live_photo() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture directory");
    add_folder(&store, &photos, true);
    let engine = start_engine(&server, &store, watching()).await;

    // Both files appear while the engine runs, so the watcher reports them.
    let photo = write_file(&photos, "IMG_0005.jpg", b"photo bytes");
    let video = write_file(&photos, "IMG_0005.mov", b"motion bytes");
    let sidecar = write_file(&photos, "IMG_0005.jpg.xmp", SIDECAR_XMP);

    assert!(
        wait_until(
            || all_terminal(&store, &[photo.clone(), video.clone()]),
            Duration::from_secs(15)
        )
        .await,
        "the watched live photo did not upload"
    );
    let photo_asset = asset_id(&store, &photo);
    assert!(
        server.has_sidecar(&photo_asset),
        "R3.3: the watch path carries the sidecar too"
    );
    assert_eq!(
        server.live_photo_video_id(&photo_asset),
        Some(asset_id(&store, &video)),
        "R3.3: the watch path links the live photo too"
    );
    assert_eq!(
        store.item(&sidecar.display().to_string()).expect("read"),
        None,
        "R3.3: a watched sidecar is not an item of its own"
    );
    engine.stop();
}
