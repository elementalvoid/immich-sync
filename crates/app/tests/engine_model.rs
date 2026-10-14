//! The app-scoped engine model's start handoff (SPEC R8.7).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{AppContext as _, Entity, TestAppContext};
use immich_sync::connection::ConnectionModel;
use immich_sync::engine::EngineModel;
use immich_sync::paths::AppPaths;
use immich_sync::store::StoreModel;
use mock_immich::{Fault, MockImmich, Operation};
use sync_core::connection::{AuthMethod, Secret, ServerProfile, SignIn};
use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};
use sync_core::engine::Activity;
use sync_core::store::SourceFolder;

/// The path layout the other app tests use, so the engine gets a temp
/// database and settings file.
fn temporary_paths(dir: &Path) -> AppPaths {
    AppPaths {
        config_file: dir.join("config").join("settings.toml"),
        state_db: dir.join("data").join("state.db"),
        autostart_dir: dir.join("config").join("autostart"),
    }
}

/// Starts the offline Immich double on its own runtime: a GPUI test runs on
/// the GPUI executor, which is not a Tokio runtime.
fn mock_server() -> (tokio::runtime::Runtime, MockImmich) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("start the mock runtime");
    let server = runtime.block_on(MockImmich::start());
    (runtime, server)
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
    // The checksum check is refused twice: the first 401 re-logs in, and the
    // second leaves the connection needing the user (R1.4).
    for _ in 0..2 {
        server.fail(
            Operation::BulkUploadCheck,
            Fault::Status {
                code: 401,
                retry_after: None,
            },
        );
    }
    cx.update(|cx| engine.update(cx, |model, cx| model.sync_now(cx)));
    for _ in 0..400 {
        if !cx.read(|cx| connection.read(cx).state().is_connected()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(50)).await;
        cx.run_until_parked();
    }
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
    for _ in 0..400 {
        if cx.read(|cx| connection.read(cx).state().is_connected()) && server.upload_requests() >= 2
        {
            break;
        }
        cx.executor().timer(Duration::from_millis(50)).await;
        cx.run_until_parked();
    }
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
    // The checksum check is refused twice: the first 401 re-logs in, and the
    // second leaves the connection needing the user (R1.4).
    for _ in 0..2 {
        server.fail(
            Operation::BulkUploadCheck,
            Fault::Status {
                code: 401,
                retry_after: None,
            },
        );
    }
    // The dashboard's "Sync now": the one scan this test asks for by hand.
    cx.update(|cx| engine.update(cx, |model, cx| model.sync_now(cx)));
    for _ in 0..200 {
        if server.bulk_check_requests() >= 2 {
            break;
        }
        cx.executor().timer(Duration::from_millis(50)).await;
        cx.run_until_parked();
    }
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

    for _ in 0..200 {
        if server.upload_requests() >= 2 {
            break;
        }
        cx.executor().timer(Duration::from_millis(50)).await;
        cx.run_until_parked();
    }
    assert_eq!(
        server.upload_requests(),
        2,
        "R1.4: the re-login must resume what the refused batch left pending"
    );
}
