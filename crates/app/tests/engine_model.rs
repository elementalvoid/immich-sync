//! The app-scoped engine model's start handoff (SPEC R8.7).

mod common;

use common::mock_server;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{AppContext as _, Entity, TestAppContext};
use immich_sync::connection::ConnectionModel;
use immich_sync::engine::EngineModel;
use immich_sync::paths::AppPaths;
use immich_sync::store::StoreModel;
use mock_immich::{Fault, MockImmich, Operation};
use sync_core::connection::{
    Attention, AuthMethod, ConnectionState, Secret, ServerProfile, SignIn,
};
use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};
use sync_core::hash::Sha1;
use sync_core::status::Activity;
use sync_core::store::{FolderCounts, ItemRecord, ItemState, SourceFolder, StateStore};

/// The path layout the other app tests use, so the engine gets a temp
/// database and settings file.
fn temporary_paths(dir: &Path) -> AppPaths {
    AppPaths {
        config_file: dir.join("config").join("settings.toml"),
        state_db: dir.join("data").join("state.db"),
        autostart_dir: dir.join("config").join("autostart"),
    }
}

/// A permanently failed row, written straight to the store.
fn failed(path: &str) -> ItemRecord {
    ItemRecord {
        path: path.to_string(),
        size: 1024,
        mtime_ns: 1_700_000_000_000_000_000,
        sha1: Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").expect("a hex SHA-1"),
        asset_id: None,
        state: ItemState::FailedPermanent,
        error_class: None,
        error_message: Some("the server refused the upload".to_string()),
        attempts: 1,
        sidecar: None,
    }
}

/// One row in `state`, written straight to the store, for the tests that only
/// need the card's buckets (#52).
fn row(path: &Path, state: ItemState) -> ItemRecord {
    ItemRecord {
        path: path.display().to_string(),
        size: 1024,
        mtime_ns: 1_700_000_000_000_000_000,
        sha1: Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").expect("a hex SHA-1"),
        asset_id: Some("asset-1".to_string()),
        state,
        error_class: None,
        error_message: None,
        attempts: 0,
        sidecar: None,
    }
}

/// The card counts, named so an expectation reads like the card (#52).
fn counts_of(pending: u32, uploaded: u32, failed: u32, review: u32) -> FolderCounts {
    FolderCounts {
        pending,
        uploaded,
        failed,
        review,
    }
}

/// Seeds one source folder and `names` permanent failures under it.
fn seed_failed(paths: &AppPaths, root: &Path, names: &[&str]) {
    let store = StateStore::open(&paths.state_db).expect("open the state database");
    store
        .add_folder(&SourceFolder {
            path: root.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: sync_core::store::AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");
    for name in names {
        store
            .record(&failed(&root.join(name).display().to_string()))
            .expect("write the failed row");
    }
}

/// Signs in against `server` and returns the connection model.
fn signed_in(cx: &mut TestAppContext, server: &MockImmich) -> Entity<ConnectionModel> {
    let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
    let profile = ServerProfile {
        server_url: server.base_url().to_string(),
        email: "sync@example.com".to_string(),
        method: AuthMethod::Password,
    };
    cx.update(|cx| {
        let connection = cx.new(|_| ConnectionModel::new(profile.clone(), credentials));
        connection.update(cx, |model, cx| {
            model.sign_in(
                SignIn {
                    email: profile.email.clone(),
                    secret: Secret::new("correct horse"),
                    method: AuthMethod::Password,
                },
                profile,
                cx,
            );
        });
        connection
    })
}

/// Pumps the executor until both inputs the engine needs are ready.
async fn wait_for_inputs(
    cx: &mut TestAppContext,
    connection: &Entity<ConnectionModel>,
    store: &Entity<StoreModel>,
) {
    for _ in 0..600 {
        if cx.read(|cx| {
            connection.read(cx).state().is_connected() && store.read(cx).store().is_some()
        }) {
            return;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    panic!("the store and the connection never became ready");
}

/// The live connection state, read from the session the engine holds rather
/// than from the model's 250 ms poll of it.
fn live_state(cx: &TestAppContext, connection: &Entity<ConnectionModel>) -> ConnectionState {
    cx.read(|cx| {
        connection
            .read(cx)
            .session()
            .expect("a live session")
            .state()
    })
}

/// Advances the test clock and lets real time pass until `ready` holds.
///
/// The engine's HTTP work runs on its own runtime, so a wait must give it real
/// time; advancing the clock runs the model's own poll without parking the
/// test thread on a timer future. A parked timer only advances while the test
/// thread parks, which is what let the old `timer(..).await` loops outlast
/// every budget.
fn pump_until(cx: &TestAppContext, mut ready: impl FnMut(&TestAppContext) -> bool) -> bool {
    // Twenty real seconds in the shape of the timer loops this replaces.
    const STEPS: usize = 10_000;
    for _ in 0..STEPS {
        if ready(cx) {
            return true;
        }
        cx.executor().advance_clock(Duration::from_millis(5));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

/// Drives the engine's checksum check into refusals until the connection's
/// failure run reaches the attention threshold (R1.4, R6.6).
///
/// The state is read from the live session, not from the model, whose copy
/// only moves on its 250 ms poll. The first round needs two 401s (the one
/// re-login and the refusal that starts the run); every later round needs one,
/// because the re-login budget stays spent until a request succeeds.
fn refuse_checks_to_attention(
    cx: &mut TestAppContext,
    engine: &Entity<EngineModel>,
    connection: &Entity<ConnectionModel>,
    server: &MockImmich,
) {
    let mut refused = 0_u64;
    for _ in 0..10 {
        if !live_state(cx, connection).is_connected() {
            settle_refused_batch(cx, server);
            return;
        }
        let attempts = if refused == 0 { 2 } else { 1 };
        for _ in 0..attempts {
            server.fail(
                Operation::BulkUploadCheck,
                Fault::Status {
                    code: 401,
                    retry_after: None,
                },
            );
        }
        let before = server.bulk_check_requests();
        cx.update(|cx| engine.update(cx, |model, cx| model.sync_now(cx)));
        let consumed = pump_until(cx, |cx| {
            server.bulk_check_requests() >= before + attempts
                || !live_state(cx, connection).is_connected()
        });
        assert!(
            consumed,
            "the refused check never reached the server: {} of {attempts}",
            server.bulk_check_requests() - before
        );
        refused += attempts;
    }
    panic!("the refusal run never reached the engine's attention threshold");
}

/// Lets the batch that produced the refusal finish, then clears the faults a
/// round queued but the refusal stopped before it sent.
///
/// The claim on the refused files is released by the batch's own worker after
/// the session flips; a scan that wins that race would skip them as in flight
/// and nothing would trigger another scan. The leftover faults would be
/// bookkeeping, not the state under test: consumed after a sign-in they would
/// refuse the very batch the test wants to see resume.
fn settle_refused_batch(cx: &TestAppContext, server: &MockImmich) {
    for _ in 0..50 {
        cx.executor().advance_clock(Duration::from_millis(10));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(2));
    }
    server.reset_faults();
}

#[gpui_kit::test]
async fn a_pause_during_engine_startup_still_holds(cx: &mut TestAppContext) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());

    let connection = signed_in(cx, &server);
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    // The engine model is built last, after both inputs, so its own `start` is
    // the only start in flight.
    wait_for_inputs(cx, &connection, &store).await;
    let engine = cx.update(|cx| cx.new(|cx| EngineModel::new(connection, store, &paths, cx)));

    // The start captures "not paused"; the click lands before the engine
    // exists, which is the window R8.7 must survive.
    cx.update(|cx| {
        engine.update(cx, |model, cx| {
            model.start(cx);
            model.pause(cx);
        });
    });

    for _ in 0..600 {
        if cx.read(|cx| engine.read(cx).is_running()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| engine.read(cx).is_running()),
        "the engine never started"
    );
    assert_eq!(
        cx.read(|cx| engine.read(cx).status().activity),
        Activity::Paused,
        "R8.7: a pause clicked while the engine was being built must hold"
    );
}

#[gpui_kit::test]
async fn a_resume_during_engine_startup_leaves_uploads_flowing(cx: &mut TestAppContext) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");

    let connection = signed_in(cx, &server);
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    wait_for_inputs(cx, &connection, &store).await;
    // The folder is in the store before any engine exists, so the engine this
    // start builds watches it.
    cx.read(|cx| store.read(cx).store().expect("the store is open"))
        .add_folder(&SourceFolder {
            path: photos.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: sync_core::store::AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");

    let engine = cx.update(|cx| cx.new(|cx| EngineModel::new(connection, store, &paths, cx)));
    // The start captures "paused", so the engine is built paused. The resume
    // lands before the handoff, and only the handoff can lift that pause.
    cx.update(|cx| {
        engine.update(cx, |model, cx| {
            model.pause(cx);
            model.start(cx);
            model.resume(cx);
        });
    });

    for _ in 0..600 {
        if cx.read(|cx| engine.read(cx).is_running()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| engine.read(cx).is_running()),
        "the engine never started"
    );
    assert_ne!(
        cx.read(|cx| engine.read(cx).status().activity),
        Activity::Paused,
        "R8.7: a resume clicked while the engine was being built must hold"
    );

    std::fs::write(
        photos.join("resumed.jpg"),
        b"the resume must reach the engine",
    )
    .expect("write the file");
    for _ in 0..600 {
        if server.upload_requests() >= 1 {
            break;
        }
        cx.executor().timer(Duration::from_millis(20)).await;
        cx.run_until_parked();
    }
    assert_eq!(
        server.upload_requests(),
        1,
        "R8.7: uploads must flow once the resume reaches the engine"
    );
    // R3.7: the fresh settings store leaves the tag on, so the upload that just
    // flowed is tagged as well.
    assert!(
        pump_until(cx, |_| !server.tag_asset_calls().is_empty()),
        "R3.7: the default-on sync tag never reached the upload"
    );
}

#[gpui_kit::test]
async fn a_superseded_engine_start_leaves_exactly_one_engine_watching(cx: &mut TestAppContext) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");

    let connection = signed_in(cx, &server);
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    wait_for_inputs(cx, &connection, &store).await;
    // The folder is in the store before any engine exists, so every racing
    // engine watches it from its first start.
    cx.read(|cx| store.read(cx).store().expect("the store is open"))
        .add_folder(&SourceFolder {
            path: photos.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: sync_core::store::AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");

    let engine = cx.update(|cx| cx.new(|cx| EngineModel::new(connection, store, &paths, cx)));
    // Two starts overlap: both see no engine yet, so both build one. Only the
    // newer attempt may keep running; the discarded engine must be stopped, or
    // its watcher discovers every file a second time.
    cx.update(|cx| {
        engine.update(cx, |model, cx| {
            model.start(cx);
            model.start(cx);
        });
    });
    for _ in 0..1200 {
        if cx.read(|cx| engine.read(cx).is_running()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| engine.read(cx).is_running()),
        "the engine never started"
    );
    // Let both start results land, so a superseded engine has been discarded.
    for _ in 0..10 {
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }

    std::fs::write(photos.join("arrived.jpg"), b"exactly one engine").expect("write the file");
    for _ in 0..1000 {
        if server.bulk_check_requests() >= 1 {
            break;
        }
        cx.executor().timer(Duration::from_millis(20)).await;
        cx.run_until_parked();
    }
    // Well past the 2 s watcher debounce: an orphaned watcher would report the
    // same file in a second engine by now.
    cx.executor().timer(Duration::from_secs(4)).await;
    cx.run_until_parked();

    assert_eq!(
        server.bulk_check_requests(),
        1,
        "only the surviving engine may discover the new file"
    );
    assert_eq!(
        server.upload_requests(),
        1,
        "the superseded engine's watcher and workers must be stopped"
    );
}

/// R10.4: an engine start already in flight when a rebuild lands must not be
/// installed over the store the rebuild replaces.
///
/// Unix only: the test makes the rebuild fail by replacing the database file
/// with a directory, and Windows refuses to remove a file an open handle still
/// maps. The failing rebuild is what makes the interleaving observable: with
/// the start discarded no engine may exist afterwards, while an installed
/// pre-rebuild engine has nothing that could ever replace it.
#[cfg(unix)]
#[gpui_kit::test]
async fn a_rebuild_discards_an_engine_start_already_in_flight(cx: &mut TestAppContext) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());

    let connection = signed_in(cx, &server);
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    wait_for_inputs(cx, &connection, &store).await;

    let engine =
        cx.update(|cx| cx.new(|cx| EngineModel::new(connection, store.clone(), &paths, cx)));
    // The start is in flight when the rebuild lands.
    cx.update(|cx| {
        engine.update(cx, |model, cx| {
            model.start(cx);
            model.rebuild(cx);
        });
    });
    // Make that rebuild fail, so the fresh store never arrives: a directory
    // where the database file goes.
    std::fs::remove_file(&paths.state_db).expect("remove the database file");
    for suffix in ["-wal", "-shm"] {
        let sidecar = std::path::PathBuf::from(format!("{}{suffix}", paths.state_db.display()));
        match std::fs::remove_file(&sidecar) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove {}: {error}", sidecar.display()),
        }
    }
    std::fs::create_dir(&paths.state_db).expect("put a directory at the database path");

    for _ in 0..600 {
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
        if !cx.read(|cx| engine.read(cx).is_rebuilding()) {
            break;
        }
    }
    assert!(
        cx.read(|cx| store.read(cx).problem().is_some()),
        "the rebuild fails, so no fresh store arrives"
    );
    assert!(
        !cx.read(|cx| engine.read(cx).is_running()),
        "the engine start that predates the rebuild must not be installed"
    );
}

/// The documented recovery from a connection that needs the user: re-enter the
/// credentials in Settings and sign in. The dashboard and the tray recover from
/// the form's own state, so the engine's queue must recover from the same
/// sign-in, without waiting for a "Sync now" (R1.4).
#[gpui_kit::test]
async fn a_form_sign_in_resumes_the_engine_a_refused_session_stalled(cx: &mut TestAppContext) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    // The folder is registered before it exists, so the engine installs no
    // watcher and only a reconciliation scan finds the files below.
    let connection = signed_in(cx, &server);
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    wait_for_inputs(cx, &connection, &store).await;
    cx.read(|cx| store.read(cx).store().expect("the store is open"))
        .add_folder(&SourceFolder {
            path: photos.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: sync_core::store::AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");

    let engine =
        cx.update(|cx| cx.new(|cx| EngineModel::new(connection.clone(), store, &paths, cx)));
    cx.update(|cx| engine.update(cx, |model, cx| model.start(cx)));
    for _ in 0..600 {
        if cx.read(|cx| engine.read(cx).is_running()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| engine.read(cx).is_running()),
        "the engine never started"
    );

    std::fs::create_dir_all(&photos).expect("create the source folder");
    for name in ["one.jpg", "two.jpg"] {
        std::fs::write(photos.join(name), b"stalled bytes").expect("write the file");
    }
    // The checksum check is refused until the connection's run of failures
    // reaches the engine's attention threshold (R1.4, R6.6).
    refuse_checks_to_attention(cx, &engine, &connection, &server);
    // The engine's own poll pulls the session state into the model, and so
    // into the tray; refresh it here rather than wait out the 250 ms poll.
    cx.update(|cx| connection.update(cx, |model, cx| model.refresh_state(cx)));
    assert!(
        !cx.read(|cx| connection.read(cx).state().is_connected()),
        "R1.4: the refused batch must leave the connection needing the user"
    );
    assert_eq!(
        server.upload_requests(),
        0,
        "R1.4: nothing could authenticate, so nothing may be uploaded"
    );

    // What the user does next: the Settings form, signing in again.
    let profile = ServerProfile {
        server_url: server.base_url().to_string(),
        email: "sync@example.com".to_string(),
        method: AuthMethod::Password,
    };
    cx.update(|cx| {
        connection.update(cx, |model, cx| {
            model.sign_in(
                SignIn {
                    email: profile.email.clone(),
                    secret: Secret::new("correct horse"),
                    method: AuthMethod::Password,
                },
                profile,
                cx,
            );
        });
    });
    let resumed = pump_until(cx, |cx| {
        live_state(cx, &connection).is_connected() && server.upload_requests() >= 2
    });
    assert!(
        resumed,
        "R1.4: the sign-in did not resume the queue: {} uploads",
        server.upload_requests()
    );
    assert_eq!(
        server.upload_requests(),
        2,
        "R1.4: the sign-in that fixes the credential must resume the engine's \
         queue without the user pressing Sync now"
    );
}

/// The invariant behind the recovery above: the Settings form and the running
/// engine observe one session, so a sign-in cannot leave the engine behind on
/// the state it rejected (R1.4).
#[gpui_kit::test]
async fn the_engine_and_the_form_observe_the_same_session(cx: &mut TestAppContext) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());

    let connection = signed_in(cx, &server);
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    wait_for_inputs(cx, &connection, &store).await;
    let engine =
        cx.update(|cx| cx.new(|cx| EngineModel::new(connection.clone(), store, &paths, cx)));
    cx.update(|cx| engine.update(cx, |model, cx| model.start(cx)));
    for _ in 0..600 {
        if cx.read(|cx| engine.read(cx).is_running()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| engine.read(cx).is_running()),
        "the engine never started"
    );

    let form_session = cx
        .read(|cx| connection.read(cx).session())
        .expect("a live session");
    assert!(
        form_session.shares_state_with(
            &cx.read(|cx| engine.read(cx).session())
                .expect("the engine's session")
        ),
        "R1.4: the engine must send its requests through the form's session"
    );

    // The second sign-in is the one that splits them when the form builds a
    // new session per attempt.
    let profile = ServerProfile {
        server_url: server.base_url().to_string(),
        email: "sync@example.com".to_string(),
        method: AuthMethod::Password,
    };
    cx.update(|cx| {
        connection.update(cx, |model, cx| {
            model.sign_in(
                SignIn {
                    email: profile.email.clone(),
                    secret: Secret::new("correct horse"),
                    method: AuthMethod::Password,
                },
                profile,
                cx,
            );
        });
    });
    for _ in 0..600 {
        if cx.read(|cx| connection.read(cx).state().is_connected()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| connection.read(cx).state().is_connected()),
        "the form sign-in did not connect"
    );
    assert!(
        cx.read(|cx| connection.read(cx).session())
            .expect("a live session after the sign-in")
            .shares_state_with(
                &cx.read(|cx| engine.read(cx).session())
                    .expect("the engine's session")
            ),
        "R1.4: a form sign-in must land on the session the engine already holds"
    );
}

/// R10.1: the first transition into attention raises one notification, and
/// the status poll does not repeat it while the episode lasts.
#[gpui_kit::test]
async fn the_first_attention_transition_raises_one_notification(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    // The folder is stored but does not exist, so the client starts needing
    // attention with a reason other than the deletion review.
    let unavailable = temp.path().join("gone");
    let store = StateStore::open(&paths.state_db).expect("open the state database");
    store
        .add_folder(&SourceFolder {
            path: unavailable.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: sync_core::store::AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");
    drop(store);

    let paths_for_model = paths.clone();
    let _engine = cx.update(|cx| {
        gpui_kit::init(cx);
        // The platform records a notification only with an identity set.
        cx.set_app_identity("com.elementalvoid.immich-sync", "Immich Sync");
        let connection = cx.new(|_| {
            let store: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(ServerProfile::default(), store)
        });
        let store_model = cx.new(|cx| StoreModel::open(&paths_for_model, cx));
        cx.new(|cx| EngineModel::new(connection, store_model, &paths_for_model, cx))
    });

    let mut shown = Vec::new();
    for _ in 0..600 {
        cx.run_until_parked();
        shown = cx.shown_system_notifications();
        if !shown.is_empty() {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
    }
    assert_eq!(
        shown.len(),
        1,
        "R10.1: an unavailable folder raises one notification"
    );
    assert_eq!(shown[0].tag.as_ref(), "needs-attention");
    assert_eq!(shown[0].title.as_ref(), "Immich Sync");
    assert!(
        shown[0].body.contains("unavailable"),
        "the notification names the reason: {}",
        shown[0].body
    );

    // The status poll keeps running; the same episode must stay quiet.
    for _ in 0..20 {
        cx.executor().timer(Duration::from_millis(100)).await;
        cx.run_until_parked();
    }
    assert_eq!(
        cx.shown_system_notifications().len(),
        1,
        "R10.1: the attention latch holds while the state does not change"
    );
}

/// R10.1: a folder-level permanent error raises the attention notification
/// even when no item is parked. An album-mapping failure is the reachable
/// case: the upload itself succeeded, so the failure count stays zero.
#[gpui_kit::test]
async fn a_folder_error_with_zero_parked_items_raises_one_attention_notification(
    cx: &mut TestAppContext,
) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    std::fs::write(photos.join("mapped.jpg"), b"mapped bytes").expect("write the file");

    let connection = signed_in(cx, &server);
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    wait_for_inputs(cx, &connection, &store).await;
    cx.read(|cx| store.read(cx).store().expect("the store is open"))
        .add_folder(&SourceFolder {
            path: photos.display().to_string(),
            recursive: true,
            album_mapping: true,
            album_scope: sync_core::store::AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");
    // The album call is refused, but the upload still lands, so nothing is
    // parked and the folder error is the only reason to need attention.
    server.fail(Operation::CreateAlbum, Fault::PermissionDenied);

    let engine = cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_app_identity("com.elementalvoid.immich-sync", "Immich Sync");
        cx.new(|cx| EngineModel::new(connection, store, &paths, cx))
    });
    cx.update(|cx| engine.update(cx, |model, cx| model.start(cx)));
    for _ in 0..600 {
        if cx.read(|cx| engine.read(cx).is_running()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| engine.read(cx).is_running()),
        "the engine never started"
    );

    let mut shown = Vec::new();
    for _ in 0..600 {
        cx.run_until_parked();
        shown = cx.shown_system_notifications();
        if !shown.is_empty() {
            break;
        }
        cx.executor().timer(Duration::from_millis(20)).await;
    }
    assert_eq!(
        shown.len(),
        1,
        "R10.1: a folder error with no parked item raises one notification"
    );
    assert_eq!(shown[0].tag.as_ref(), "needs-attention");
    assert!(
        shown[0].body.contains("album mapping failed"),
        "the notification names the folder error: {}",
        shown[0].body
    );
    assert_eq!(
        cx.read(|cx| engine.read(cx).status().counts.failed),
        0,
        "the upload succeeded, so no item is parked"
    );

    // The status poll keeps running; the same episode must stay quiet.
    for _ in 0..20 {
        cx.executor().timer(Duration::from_millis(100)).await;
        cx.run_until_parked();
    }
    assert_eq!(
        cx.shown_system_notifications().len(),
        1,
        "R10.1: the attention latch holds while the folder error stays"
    );
}

/// R10.1: a refused connection raises the attention notification with the
/// reason the user needs to act on.
#[gpui_kit::test]
async fn a_refused_connection_raises_one_attention_notification(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let paths_for_model = paths.clone();
    let (connection, _engine) = cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_app_identity("com.elementalvoid.immich-sync", "Immich Sync");
        let connection = cx.new(|_| {
            let store: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(ServerProfile::default(), store)
        });
        let store_model = cx.new(|cx| StoreModel::open(&paths_for_model, cx));
        let engine =
            cx.new(|cx| EngineModel::new(connection.clone(), store_model, &paths_for_model, cx));
        (connection, engine)
    });

    cx.update(|cx| {
        connection.update(cx, |model, cx| {
            model.set_state(ConnectionState::NeedsAttention(Attention::Rejected), cx);
        });
    });

    let mut shown = Vec::new();
    for _ in 0..600 {
        cx.run_until_parked();
        shown = cx.shown_system_notifications();
        if !shown.is_empty() {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
    }
    assert_eq!(
        shown.len(),
        1,
        "R10.1: a refused connection raises one notification"
    );
    assert_eq!(shown[0].tag.as_ref(), "needs-attention");
    assert!(
        shown[0].body.contains("refused the session"),
        "the notification carries the reason: {}",
        shown[0].body
    );

    for _ in 0..20 {
        cx.executor().timer(Duration::from_millis(100)).await;
        cx.run_until_parked();
    }
    assert_eq!(
        cx.shown_system_notifications().len(),
        1,
        "R10.1: the latch holds while the connection stays refused"
    );
}

/// R10.1: an item that parks during the session raises its own notification,
/// and a second one raises another while the first still waits. An item that
/// was already parked when the process started stays quiet, so a relaunch
/// does not announce the dead letter again.
#[gpui_kit::test]
async fn only_items_that_park_during_the_session_raise_a_failure_notification(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    seed_failed(&paths, &photos, &["old.jpg"]);

    let paths_for_model = paths.clone();
    let engine = cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_app_identity("com.elementalvoid.immich-sync", "Immich Sync");
        let connection = cx.new(|_| {
            let store: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(ServerProfile::default(), store)
        });
        let store_model = cx.new(|cx| StoreModel::open(&paths_for_model, cx));
        cx.new(|cx| EngineModel::new(connection, store_model, &paths_for_model, cx))
    });

    // The first read of the parked list is the one the process starts with;
    // wait for it so the assertion is about behaviour, not timing.
    for _ in 0..600 {
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
        if !cx.read(|cx| engine.read(cx).failed_items().is_empty()) {
            break;
        }
    }
    assert!(
        cx.shown_system_notifications().is_empty(),
        "R10.1: an item parked before startup raises no notification"
    );

    // A permanent failure appears while the process runs.
    let store = cx.read(|cx| engine.read(cx).store(cx).expect("the store is open"));
    store
        .record(&failed(&photos.join("one.jpg").display().to_string()))
        .expect("write the new failed row");
    let mut shown = Vec::new();
    for _ in 0..600 {
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
        shown = cx.shown_system_notifications();
        if !shown.is_empty() {
            break;
        }
    }
    assert_eq!(
        shown.len(),
        1,
        "R10.1: one newly failed item raises one notification"
    );
    assert_eq!(shown[0].tag.as_ref(), "failed-item");
    assert!(
        shown[0].body.contains("one.jpg"),
        "the notification names the file: {}",
        shown[0].body
    );

    // A second permanent failure appears while the first still waits.
    store
        .record(&failed(&photos.join("two.jpg").display().to_string()))
        .expect("write the second failed row");
    for _ in 0..600 {
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
        shown = cx.shown_system_notifications();
        if shown.len() >= 2 {
            break;
        }
    }
    assert_eq!(
        shown.len(),
        2,
        "R10.1: a second failure raises a second notification"
    );
    assert_eq!(shown[1].tag.as_ref(), "failed-item");
    assert!(
        shown[1].body.contains("two.jpg"),
        "the second notification names the new file: {}",
        shown[1].body
    );
}

#[gpui_kit::test]
async fn a_re_login_resumes_the_batch_a_refused_connection_left_pending(cx: &mut TestAppContext) {
    let (runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    // The folder is registered before it exists, so the engine installs no
    // watcher for it and every file below is found by a reconciliation scan.
    // Nothing but a scan can reach them, which is what makes this test about
    // the scan a re-login starts.
    let connection = signed_in(cx, &server);
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    wait_for_inputs(cx, &connection, &store).await;
    cx.read(|cx| store.read(cx).store().expect("the store is open"))
        .add_folder(&SourceFolder {
            path: photos.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: sync_core::store::AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");

    let engine =
        cx.update(|cx| cx.new(|cx| EngineModel::new(connection.clone(), store, &paths, cx)));
    // Both inputs were ready before this model existed, so nothing has
    // notified it yet.
    cx.update(|cx| engine.update(cx, |model, cx| model.start(cx)));
    for _ in 0..600 {
        if cx.read(|cx| engine.read(cx).is_running()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| engine.read(cx).is_running()),
        "the engine never started"
    );

    std::fs::create_dir_all(&photos).expect("create the source folder");
    for name in ["one.jpg", "two.jpg"] {
        std::fs::write(photos.join(name), b"pending bytes").expect("write the file");
    }
    // The checksum check is refused until the connection's run of failures
    // reaches the engine's attention threshold (R1.4, R6.6). Nothing but a
    // scan reaches the files, so the refusals also cost a scan each.
    refuse_checks_to_attention(cx, &engine, &connection, &server);
    assert_eq!(
        server.upload_requests(),
        0,
        "R1.4: nothing could authenticate, so nothing may be uploaded"
    );

    // R1.4: the sign-in that fixes the credential resumes the rows the refused
    // batch left pending, without waiting out a 30-minute reconciliation scan
    // or a manual "Sync now".
    let session = cx.read(|cx| connection.read(cx).session().expect("a live session"));
    // The engine's own poll pulls the state it produced into the model, and so
    // into the dashboard and the tray.
    cx.update(|cx| connection.update(cx, |model, cx| model.refresh_state(cx)));
    assert!(
        !cx.read(|cx| connection.read(cx).state().is_connected()),
        "R1.4: the refused batch must leave the connection needing the user"
    );
    let state = runtime.block_on(session.sign_in(SignIn {
        email: "sync@example.com".to_string(),
        secret: Secret::new("correct horse"),
        method: AuthMethod::Password,
    }));
    assert!(state.is_connected(), "{state:?}");
    cx.update(|cx| connection.update(cx, |model, cx| model.refresh_state(cx)));

    let resumed = pump_until(cx, |_| server.upload_requests() >= 2);
    assert!(
        resumed,
        "R1.4: the re-login did not resume the queue: {} uploads",
        server.upload_requests()
    );
    assert_eq!(
        server.upload_requests(),
        2,
        "R1.4: the re-login must resume what the refused batch left pending"
    );
}

/// R3.4: an edit made while signed out goes straight to the store. Re-adding a
/// folder that is already album-mapped must keep its album settings, even when
/// the user spells the path differently (R2.4): the store's upsert replaces
/// the whole row, so the lookup must compare path keys, not raw strings.
#[gpui_kit::test]
async fn a_signed_out_re_add_of_a_mapped_folder_keeps_its_album_settings(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    // The connection stays signed out, so the add takes the store-only path.
    let connection = cx.update(|cx| {
        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials))
    });
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    for _ in 0..600 {
        if cx.read(|cx| store.read(cx).store().is_some()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    cx.read(|cx| store.read(cx).store().expect("the store is open"))
        .add_folder(&SourceFolder {
            path: photos.display().to_string(),
            recursive: true,
            album_mapping: true,
            album_scope: sync_core::store::AlbumScope::WholeFolder,
            album_title: None,
        })
        .expect("seed the mapped folder");

    let engine =
        cx.update(|cx| cx.new(|cx| EngineModel::new(connection, store.clone(), &paths, cx)));
    cx.update(|cx| {
        engine.update(cx, |model, cx| {
            model.add_folder(format!("{}/", photos.display()), true, cx);
        });
    });

    let mut readded = None;
    for _ in 0..600 {
        let folders = cx.read(|cx| store.read(cx).folders().to_vec());
        if let Some(folder) = folders.first()
            && folder.path.ends_with('/')
        {
            readded = Some(folder.clone());
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    let readded = readded.expect("the signed-out re-add reached the store");
    assert!(
        readded.album_mapping,
        "R3.4: the re-add must keep the mapping on"
    );
    assert_eq!(
        readded.album_scope,
        sync_core::store::AlbumScope::WholeFolder,
        "R3.4: the re-add must keep the stored scope"
    );
}

/// R2.4, R8.1: a re-add that spells the path differently keeps the counts the
/// dashboard cached at open. The cache used to match the raw string, so the
/// re-spelled folder fell back to zero counts for the rest of the session.
#[gpui_kit::test]
async fn a_re_spelled_re_add_keeps_the_cached_counts(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    {
        let store = StateStore::open(&paths.state_db).expect("open the state database");
        store
            .add_folder(&SourceFolder {
                path: photos.display().to_string(),
                recursive: true,
                album_mapping: false,
                album_scope: sync_core::store::AlbumScope::Leaf,
                album_title: None,
            })
            .expect("seed the folder");
        store
            .record(&ItemRecord {
                path: photos.join("one.jpg").display().to_string(),
                size: 1024,
                mtime_ns: 1_700_000_000_000_000_000,
                sha1: Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")
                    .expect("a hex SHA-1"),
                asset_id: Some("asset-1".to_string()),
                state: ItemState::Uploaded,
                error_class: None,
                error_message: None,
                attempts: 0,
                sidecar: None,
            })
            .expect("seed the uploaded row");
    }

    let connection = cx.update(|cx| {
        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials))
    });
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    for _ in 0..600 {
        if cx.read(|cx| store.read(cx).store().is_some()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert_eq!(
        cx.read(|cx| store.read(cx).folder_counts()[0].uploaded),
        1,
        "the seeded count is the one the dashboard starts with"
    );

    let engine =
        cx.update(|cx| cx.new(|cx| EngineModel::new(connection, store.clone(), &paths, cx)));
    cx.update(|cx| {
        engine.update(cx, |model, cx| {
            model.add_folder(format!("{}/", photos.display()), true, cx);
        });
    });

    let mut respelled = false;
    for _ in 0..600 {
        if cx.read(|cx| {
            store
                .read(cx)
                .folders()
                .first()
                .is_some_and(|folder| folder.path.ends_with('/'))
        }) {
            respelled = true;
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(respelled, "the re-spelled re-add reached the store");
    assert_eq!(
        cx.read(|cx| store.read(cx).folder_counts()[0].uploaded),
        1,
        "R8.1: the re-add reset the cached counts to zero"
    );
}

/// R8.1, #52: the store-backed cards own a row the way the engine's meters do.
/// The nested folder is the one the client knows first and the outer folder is
/// added later, which makes the outer folder the first owner of every row
/// under it: the nested card must stop counting them, and the global card must
/// stay the sum of the folder cards.
#[gpui_kit::test]
async fn adding_a_nested_outer_folder_moves_the_rows_to_its_card(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let outer = temp.path().join("photos");
    let inner = outer.join("2024");
    std::fs::create_dir_all(&inner).expect("create the source folder");
    {
        let store = StateStore::open(&paths.state_db).expect("open the state database");
        store
            .add_folder(&SourceFolder {
                path: inner.display().to_string(),
                recursive: true,
                album_mapping: false,
                album_scope: sync_core::store::AlbumScope::Leaf,
                album_title: None,
            })
            .expect("seed the nested folder");
        for (name, state) in [
            ("one.jpg", ItemState::Uploaded),
            ("two.jpg", ItemState::Pending),
            ("three.jpg", ItemState::FailedPermanent),
        ] {
            store
                .record(&row(&inner.join(name), state))
                .expect("seed the row");
        }
    }

    let connection = cx.update(|cx| {
        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials))
    });
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    for _ in 0..600 {
        if cx.read(|cx| store.read(cx).store().is_some()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    let engine =
        cx.update(|cx| cx.new(|cx| EngineModel::new(connection, store.clone(), &paths, cx)));
    assert_eq!(
        cx.read(|cx| store.read(cx).folder_counts().to_vec()),
        vec![counts_of(1, 1, 1, 0)],
        "one source folder counts every row under it"
    );

    // The add lands on the worker and the model replaces the cached list, the
    // path a signed-out client takes.
    cx.update(|cx| {
        engine.update(cx, |model, cx| {
            model.add_folder(outer.display().to_string(), true, cx);
        });
    });
    let settled = pump_until(cx, |cx| {
        cx.read(|cx| {
            store.read(cx).folder_counts().to_vec()
                == vec![counts_of(1, 1, 1, 0), FolderCounts::default()]
        })
    });
    assert!(
        settled,
        "the added outer folder never took the rows from the nested card: {:?}",
        cx.read(|cx| store.read(cx).folder_counts().to_vec())
    );

    // The store-backed status a signed-out client reads: no engine runs, so
    // these are the cached counts over the store's folder order.
    cx.update(|cx| engine.update(cx, |model, cx| model.sync_now(cx)));
    let status = cx.read(|cx| engine.read(cx).status().clone());
    assert_eq!(status.folders.len(), 2, "the store-backed cards arrived");
    assert_eq!(
        status.folders[0].counts,
        counts_of(1, 1, 1, 0),
        "the outer folder is first in path order and owns every row under it"
    );
    assert_eq!(
        status.folders[1].counts,
        FolderCounts::default(),
        "#52: the nested folder counts nothing the outer folder owns"
    );
    assert_eq!(
        status.counts,
        counts_of(1, 1, 1, 0),
        "the global card is the sum of the folder cards, each row once"
    );
}

/// R8.2: with no engine and no session, the derived status is idle, not
/// attention. The connection is a status input now, so a client that has not
/// signed in does not read as a problem.
#[gpui_kit::test]
async fn a_signed_out_client_shows_idle_not_attention(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the source folder");
    {
        let store = StateStore::open(&paths.state_db).expect("open the state database");
        store
            .add_folder(&SourceFolder {
                path: photos.display().to_string(),
                recursive: true,
                album_mapping: false,
                album_scope: sync_core::store::AlbumScope::Leaf,
                album_title: None,
            })
            .expect("seed the folder");
    }

    let connection = cx.update(|cx| {
        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials))
    });
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    for _ in 0..600 {
        if cx.read(|cx| store.read(cx).store().is_some()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    let engine =
        cx.update(|cx| cx.new(|cx| EngineModel::new(connection, store.clone(), &paths, cx)));

    // No engine starts: `sync_now` republishes the store-backed status and
    // returns, which is the path a signed-out client uses.
    cx.update(|cx| engine.update(cx, |model, cx| model.sync_now(cx)));
    assert_eq!(
        cx.read(|cx| engine.read(cx).status().folders.len()),
        1,
        "the store-backed status never arrived"
    );
    assert_eq!(
        cx.read(|cx| engine.read(cx).status().activity),
        Activity::Idle,
        "R8.2: a client that has not signed in must not ask for attention"
    );
}

/// #40: the setup dialog's Add reaches the running engine. One write records
/// the path and all three settings, and the folder's existing files upload
/// without a manual Sync now.
#[gpui_kit::test]
async fn a_confirmed_folder_add_scans_the_folder_through_the_running_engine(
    cx: &mut TestAppContext,
) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");
    std::fs::write(photos.join("existing.jpg"), b"existing").expect("write the fixture file");

    let connection = signed_in(cx, &server);
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    wait_for_inputs(cx, &connection, &store).await;
    let engine =
        cx.update(|cx| cx.new(|cx| EngineModel::new(connection, store.clone(), &paths, cx)));
    cx.update(|cx| engine.update(cx, |model, cx| model.start(cx)));
    assert!(
        pump_until(cx, |cx| cx.read(|cx| engine.read(cx).is_running())),
        "the engine never started"
    );

    let confirmed = SourceFolder {
        path: photos.display().to_string(),
        recursive: false,
        album_mapping: true,
        album_scope: sync_core::store::AlbumScope::WholeFolder,
        album_title: None,
    };
    let request = cx.update(|cx| {
        engine.update(cx, |model, cx| {
            model.add_folder_settings(confirmed.clone(), cx)
        })
    });
    assert!(
        pump_until(cx, |cx| cx
            .read(|cx| engine.read(cx).folder_add_result().is_some())),
        "#40: the add result never reached the dialog"
    );
    let result = cx
        .read(|cx| engine.read(cx).folder_add_result().cloned())
        .expect("the add came back");
    assert_eq!(result.request, request, "#40: the result is this request's");
    assert_eq!(result.problem, None, "#40: the folder was added");
    assert_eq!(
        cx.read(|cx| {
            store
                .read(cx)
                .store()
                .expect("the store is open")
                .folders()
                .expect("read the folders")
        }),
        vec![confirmed.clone()],
        "#40: one write records the path and all three settings"
    );
    assert_eq!(
        cx.read(|cx| engine.read(cx).status().folders.len()),
        1,
        "R8.1: the engine's card holds the new folder"
    );
    assert!(
        pump_until(cx, |_| server.upload_requests() >= 1),
        "R2.1: a confirmed folder starts scanning and uploading its files"
    );

    // #40: the running engine's duplicate refusal reaches the dialog by name,
    // and the tracked row keeps the settings it already had. The second draft
    // differs, so an upsert would show up below.
    let duplicate = SourceFolder {
        recursive: true,
        ..confirmed.clone()
    };
    let refused =
        cx.update(|cx| engine.update(cx, |model, cx| model.add_folder_settings(duplicate, cx)));
    assert!(
        pump_until(cx, |cx| cx.read(|cx| engine
            .read(cx)
            .folder_add_result()
            .is_some_and(|result| result.request == refused))),
        "#40: the duplicate add's answer never reached the dialog"
    );
    let result = cx
        .read(|cx| engine.read(cx).folder_add_result().cloned())
        .expect("the duplicate add came back");
    assert_eq!(
        result.problem.as_deref(),
        Some("Already added."),
        "#40: a folder the running engine tracks is refused by name"
    );
    assert_eq!(
        cx.read(|cx| {
            store
                .read(cx)
                .store()
                .expect("the store is open")
                .folders()
                .expect("read the folders")
        }),
        vec![confirmed.clone()],
        "#40: a duplicate add leaves the tracked row as it was"
    );
}

/// #40: an Add that arrives before the state store opens comes back with that
/// reason, so the dialog stays open and the user can try again.
#[gpui_kit::test]
async fn an_add_before_the_store_opens_reports_that_reason(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    std::fs::create_dir_all(&photos).expect("create the fixture folder");

    let connection = cx.update(|cx| {
        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials))
    });
    let store = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    assert!(
        cx.read(|cx| store.read(cx).store().is_none()),
        "the store must still be opening when the add is sent"
    );
    let engine =
        cx.update(|cx| cx.new(|cx| EngineModel::new(connection, store.clone(), &paths, cx)));
    let draft = SourceFolder {
        path: photos.display().to_string(),
        recursive: true,
        album_mapping: false,
        album_scope: sync_core::store::AlbumScope::Leaf,
        album_title: None,
    };
    cx.update(|cx| {
        engine.update(cx, |model, cx| {
            model.add_folder_settings(draft, cx);
        })
    });
    assert!(
        pump_until(cx, |cx| cx
            .read(|cx| engine.read(cx).folder_add_result().is_some())),
        "#40: the store-opening result never reached the dialog"
    );
    let result = cx
        .read(|cx| engine.read(cx).folder_add_result().cloned())
        .expect("the add came back");
    assert_eq!(
        result.problem.as_deref(),
        Some("The state database is still opening."),
        "#40: the dialog shows why nothing was written"
    );
    assert!(
        cx.read(|cx| store.read(cx).folders().is_empty()),
        "#40: a store-opening add writes nothing"
    );
    assert_eq!(
        cx.read(|cx| engine.read(cx).problem().map(str::to_string)),
        None,
        "#40: a refused add is the dialog's reason, not an engine problem"
    );
}
