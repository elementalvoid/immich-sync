//! The upload meter the dashboard reads (SPEC R8.1; GLOSSARY "Upload
//! throughput", "Upload in flight").
//!
//! Real uploads against the offline double, because the criteria are a rate and
//! a number of requests on the wire. The double's upload delay is what keeps a
//! request in flight long enough to be sampled.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mock_immich::{Fault, MockImmich, Operation};
use sync_core::connection::{AuthMethod, Connection, Secret, SharedSession, SignIn};
use sync_core::credential::MemoryCredentialStore;
use sync_core::engine::{Engine, EngineConfig};
use sync_core::meter::UploadTraffic;
use sync_core::store::{AlbumScope, SourceFolder, StateStore};
use sync_core::tracking::Tracking;
use sync_core::upload::Uploader;

use common::wait_until;

/// Long enough that several status polls land while the request is in flight.
const UPLOAD_DELAY: Duration = Duration::from_millis(500);
/// Big enough that the body takes more than one poll to stream.
const BIG_BYTES: usize = 1_572_864;

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

fn add_folder(store: &StateStore, path: &Path, recursive: bool) {
    store
        .add_folder(&SourceFolder {
            path: path.display().to_string(),
            recursive,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");
}

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

fn built(store: &Arc<StateStore>, session: SharedSession) -> Arc<Engine> {
    Arc::new(
        Engine::new(
            Arc::new(Tracking::new(store.clone())),
            session,
            EngineConfig::default(),
        )
        .expect("build the engine"),
    )
}

async fn start_engine(server: &MockImmich, store: &Arc<StateStore>) -> Arc<Engine> {
    let engine = built(store, signed_in(server).await);
    engine.start();
    engine
}

fn terminal(store: &StateStore, path: &Path) -> bool {
    matches!(
        store.item(&path.display().to_string()),
        Ok(Some(row)) if row.state.is_terminal_success()
    )
}

#[tokio::test]
async fn the_status_rates_a_upload_while_it_is_on_the_wire() {
    let server = MockImmich::start().await;
    server.set_upload_delay(UPLOAD_DELAY);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let path = write_file(&photos, "big.bin", BIG_BYTES);
    add_folder(&store, &photos, true);

    let engine = built(&store, signed_in(&server).await);
    engine.start();

    let seen = std::sync::Arc::new(std::sync::Mutex::new(UploadTraffic::default()));
    assert!(
        wait_until(
            || {
                let uploads = engine.status().uploads;
                let ready = uploads.in_flight == 1 && uploads.bytes_per_second > 0;
                if ready {
                    *seen.lock().expect("the sample") = uploads;
                }
                ready
            },
            Duration::from_secs(20)
        )
        .await,
        "no poll ever saw the upload on the wire with a rate: {:?}",
        engine.status().uploads
    );
    let on_the_wire = *seen.lock().expect("the sample");
    assert!(
        on_the_wire.bytes > 0 && on_the_wire.bytes <= BIG_BYTES as u64,
        "the meter counts the upload body's own bytes, not the response's: {on_the_wire:?}"
    );
    assert!(
        on_the_wire.bytes_per_second > 0 && on_the_wire.bytes_per_second < BIG_BYTES as u64,
        "the rate is the window's average, not the whole body: {on_the_wire:?}"
    );
    assert_eq!(
        engine.status().folders[0].uploads.bytes,
        on_the_wire.bytes,
        "one folder's reading is the global reading"
    );

    assert!(
        wait_until(|| terminal(&store, &path), Duration::from_secs(20)).await,
        "the upload never finished"
    );
    let finished = engine.status().uploads;
    assert_eq!(
        finished.in_flight, 0,
        "a finished upload is not on the wire"
    );
    assert_eq!(
        finished.bytes, BIG_BYTES as u64,
        "the meter counted every body byte and nothing else"
    );
}

#[tokio::test]
async fn a_paused_engine_never_puts_an_upload_on_the_wire() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let path = write_file(&photos, "photo.bin", 64 * 1024);
    add_folder(&store, &photos, true);

    let engine = built(&store, signed_in(&server).await);
    // R8.7: paused before the first scan, so nothing may reach the wire.
    engine.pause();
    engine.start();

    // The scan finds the file either way; what the pause holds is the request.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        engine.status().uploads,
        UploadTraffic::default(),
        "an attempt that pauses before the wire is not an upload in flight"
    );
    assert_eq!(server.upload_requests(), 0, "nothing was sent");

    // The item was queued all along, which is what the resume proves: it is the
    // gate, not the discovery, that kept the request off the wire.
    engine.resume();
    assert!(
        wait_until(|| terminal(&store, &path), Duration::from_secs(20)).await,
        "the resumed engine never uploaded the queued file"
    );
    assert_eq!(
        server.upload_requests(),
        1,
        "the resume sent the held upload"
    );
}

#[tokio::test]
async fn a_failed_upload_rises_and_falls_but_leaves_nothing_on_the_wire() {
    let server = MockImmich::start().await;
    server.set_upload_delay(UPLOAD_DELAY);
    server.fail(Operation::Upload, Fault::UnsupportedFormat);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let path = write_file(&photos, "bad.bin", 64 * 1024);
    add_folder(&store, &photos, true);

    let engine = start_engine(&server, &store).await;

    let rose = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let peak = Arc::clone(&rose);
    assert!(
        wait_until(
            || {
                let uploads = engine.status().uploads;
                peak.fetch_max(uploads.in_flight, std::sync::atomic::Ordering::AcqRel);
                matches!(
                    store.item(&path.display().to_string()),
                    Ok(Some(ref row)) if row.state == sync_core::store::ItemState::FailedPermanent
                )
            },
            Duration::from_secs(20)
        )
        .await,
        "the refused upload never parked"
    );
    assert!(
        rose.load(std::sync::atomic::Ordering::Acquire) >= 1,
        "the refused attempt was never counted as on the wire"
    );
    assert_eq!(
        engine.status().uploads.in_flight,
        0,
        "a failed upload leaves nothing on the wire"
    );
}

#[tokio::test]
async fn per_folder_bytes_sum_to_the_global_reading() {
    let server = MockImmich::start().await;
    server.set_upload_delay(UPLOAD_DELAY);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let trips = photos.join("trips");
    let parent_file = write_file(&photos, "parent.bin", 64 * 1024);
    let nested_file = write_file(&trips, "nested.bin", 96 * 1024);
    // R2.1: a folder and one of its subfolders are both source folders, one
    // recursive and one not.
    add_folder(&store, &photos, true);
    add_folder(&store, &trips, false);

    let engine = start_engine(&server, &store).await;

    assert!(
        wait_until(
            || terminal(&store, &parent_file) && terminal(&store, &nested_file),
            Duration::from_secs(20)
        )
        .await,
        "the two files never uploaded"
    );
    let status = engine.status();
    let mut summed = UploadTraffic::default();
    for folder in &status.folders {
        summed.add(folder.uploads);
    }
    assert_eq!(
        status.uploads, summed,
        "the global card is the sum of the folder cards"
    );
    assert_eq!(
        status.uploads.bytes,
        (parent_file.metadata().expect("stat").len() + nested_file.metadata().expect("stat").len()),
        "every upload body was metered once"
    );
    // The nested file is metered to the folder the engine takes it from, which
    // is the same first-match rule the pending count uses.
    assert_eq!(
        status.folders[0].uploads.bytes, status.uploads.bytes,
        "the recursive folder owns the nested file's bytes too"
    );
    assert_eq!(
        status.folders[1].uploads.bytes, 0,
        "the subfolder is a source folder but did not run the upload"
    );
    assert_eq!(status.uploads.in_flight, 0);
}

#[tokio::test]
async fn a_paused_engine_keeps_rating_an_upload_already_on_the_wire() {
    let server = MockImmich::start().await;
    server.set_upload_delay(UPLOAD_DELAY);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let path = write_file(&photos, "big.bin", BIG_BYTES);
    add_folder(&store, &photos, true);

    let engine = start_engine(&server, &store).await;
    assert!(
        wait_until(
            || engine.status().uploads.in_flight == 1,
            Duration::from_secs(20)
        )
        .await,
        "the upload never reached the wire"
    );

    engine.pause();

    // R8.7: the request on the wire finishes, and it keeps the rate while it
    // does; no second one starts.
    assert!(
        wait_until(|| terminal(&store, &path), Duration::from_secs(20)).await,
        "the paused engine dropped the upload it was already sending"
    );
    assert_eq!(
        server.upload_requests(),
        1,
        "pause started no second upload"
    );
    assert_eq!(engine.status().uploads.in_flight, 0);
}

#[tokio::test]
async fn a_manual_upload_changes_neither_the_rate_nor_the_counts() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    add_folder(&store, &photos, true);
    // The manual file is outside every source folder, so the engine never
    // queues it: only the Upload view's own path uploads it (R7.4).
    let manual = write_file(&temp.path().join("manual"), "one.bin", 256 * 1024);

    let engine = start_engine(&server, &store).await;
    assert!(
        wait_until(
            || matches!(store.item(&manual.display().to_string()), Ok(None)),
            Duration::from_millis(200)
        )
        .await,
        "the engine queued a file outside its folders"
    );

    let outcome = Uploader::new(engine.session().clone(), Arc::clone(engine.store()))
        .upload_one(&manual)
        .await;
    assert!(
        matches!(
            outcome,
            sync_core::upload::UploadOutcome::Created { .. }
                | sync_core::upload::UploadOutcome::Duplicate { .. }
        ),
        "the manual upload did not succeed: {outcome:?}"
    );
    assert_eq!(
        server.upload_requests(),
        1,
        "the manual upload reached the server"
    );

    assert_eq!(
        engine.status().uploads,
        UploadTraffic::default(),
        "R7.4: a manual upload is exempt from the dashboard's numbers"
    );
    assert_eq!(
        engine.uploaded_bytes(),
        0,
        "the engine's own byte total did not move either"
    );
}

#[tokio::test]
async fn a_live_photos_motion_video_counts_as_an_upload_too() {
    let server = MockImmich::start().await;
    server.set_upload_delay(UPLOAD_DELAY);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    // R3.3: the video uploads first and the photo links it. Both are uploads,
    // and both bodies move bytes.
    let photo = write_file(&photos, "IMG_0001.jpg", 64 * 1024);
    let video = write_file(&photos, "IMG_0001.mov", 128 * 1024);
    add_folder(&store, &photos, true);

    let engine = start_engine(&server, &store).await;
    assert!(
        wait_until(
            || terminal(&store, &photo) && terminal(&store, &video),
            Duration::from_secs(20)
        )
        .await,
        "the live photo never finished"
    );
    let bytes = photo.metadata().expect("stat").len() + video.metadata().expect("stat").len();
    assert_eq!(
        engine.status().uploads.bytes,
        bytes,
        "a video is an upload, and its body is metered like the photo's"
    );
    assert_eq!(
        engine.uploaded_bytes(),
        bytes,
        "R10.2: the client's own total counts the video too, so it agrees with the dashboard"
    );
}

#[tokio::test]
async fn a_rebuilt_engine_starts_the_meter_over() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let path = write_file(&photos, "photo.bin", 64 * 1024);
    add_folder(&store, &photos, true);

    let first = built(&store, signed_in(&server).await);
    first.start();
    assert!(
        wait_until(|| terminal(&store, &path), Duration::from_secs(20)).await,
        "the first engine never uploaded the file"
    );
    assert!(
        first.status().uploads.bytes > 0,
        "the first engine metered its upload"
    );
    first.stop();

    let second = built(&store, signed_in(&server).await);
    second.start();
    assert_eq!(
        second.status().uploads,
        UploadTraffic::default(),
        "a rebuilt engine starts at zero instead of borrowing the old total"
    );
}

#[tokio::test]
async fn stopping_the_engine_releases_an_upload_on_the_wire() {
    let server = MockImmich::start().await;
    server.set_upload_delay(UPLOAD_DELAY);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("photos");
    let _path = write_file(&photos, "big.bin", BIG_BYTES);
    add_folder(&store, &photos, true);

    let engine = start_engine(&server, &store).await;
    assert!(
        wait_until(
            || engine.status().uploads.in_flight == 1,
            Duration::from_secs(20)
        )
        .await,
        "the upload never reached the wire"
    );

    // R8.1: the worker's future is dropped when its task is aborted, so its
    // in-flight guard goes with it. A stale count would leave the dashboard
    // reading one upload on the wire for the life of the shell.
    engine.stop();
    assert!(
        wait_until(
            || engine.status().uploads.in_flight == 0,
            Duration::from_secs(5)
        )
        .await,
        "a stopped engine still counts an upload on the wire: {:?}",
        engine.status().uploads
    );
}
