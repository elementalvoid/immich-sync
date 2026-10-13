//! The app-scoped engine model's start handoff (SPEC R8.7).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{AppContext as _, Entity, TestAppContext};
use immich_sync::connection::ConnectionModel;
use immich_sync::engine::EngineModel;
use immich_sync::paths::AppPaths;
use immich_sync::store::StoreModel;
use mock_immich::MockImmich;
use sync_core::connection::{AuthMethod, Secret, ServerProfile, SignIn};
use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};
use sync_core::engine::Activity;

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

    let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
    let profile = ServerProfile {
        server_url: server.base_url().to_string(),
        email: "sync@example.com".to_string(),
        method: AuthMethod::Password,
    };
    let connection = cx.update(|cx| {
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
    });
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
