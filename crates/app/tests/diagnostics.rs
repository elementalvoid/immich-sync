//! The Diagnostics view and its model (SPEC R10.2, R10.4).
//!
//! The view shows the server version, the granted permissions, the sqlite
//! state, the last error per class, and the upload throughput; its rebuild
//! action starts the state over and rescans the folders. The headless harness
//! cannot read pixels, so "readable in both themes" is checked by rendering
//! under both and reading the same accessibility properties.

mod common;

use common::mock_server;

use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, Point, Role, ScrollDelta,
    TestAppContext, Window, WindowBounds, WindowOptions, point, px, size,
};
use immich_sync::autostart::{Autostart, UnavailableAutostart};
use immich_sync::connection::ConnectionModel;
use immich_sync::dashboard::Dashboard;
use immich_sync::debug_bundle::BUNDLE_PREFIX;
use immich_sync::diagnostics::{BundleState, DiagnosticsModel, Permissions};
use immich_sync::diagnostics_view::DiagnosticsView;
use immich_sync::engine::EngineModel;
use immich_sync::logging::{LOG_PREFIX, LogLevel, LogPolicy, LogSink};
use immich_sync::paths::AppPaths;
use immich_sync::settings::SettingsStore;
use immich_sync::settings_view::SettingsView;
use immich_sync::shell::{self, AppShell, ViewMode};
use immich_sync::store::StoreModel;
use immich_sync::upload::UploadModel;
use immich_sync::upload_view::UploadView;
use immich_sync::{status, theme};
use mock_immich::MockImmich;
use sync_core::connection::{AuthMethod, ConnectionState, Secret, ServerProfile, SignIn};
use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};
use sync_core::failure::FailureClass;
use sync_core::hash::sha1;
use sync_core::store::{
    AlbumScope, AttemptOutcome, AttemptRecord, ItemRecord, ItemState, SourceFolder, StateStore,
};

const API_KEY: &str = "api-key-abc123";
/// The schema stands at the twelfth migration (#47 added the album title on top
/// of the folder times #43 and the tag backfill #46).
const DB_MIGRATION_VERSION: i64 = 12;

fn temporary_paths(dir: &Path) -> AppPaths {
    AppPaths {
        config_file: dir.join("config").join("settings.toml"),
        state_db: dir.join("data").join("state.db"),
        autostart_dir: dir.join("config").join("autostart"),
    }
}

fn window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            size: size(px(1200.), px(1600.)),
        })),
        ..Default::default()
    }
}

fn autostart() -> Rc<dyn Autostart> {
    Rc::new(UnavailableAutostart::new(
        "the test run has no OS autostart mechanism".to_string(),
    ))
}

fn profile_for(server: &MockImmich, method: AuthMethod) -> ServerProfile {
    ServerProfile {
        server_url: server.base_url().to_string(),
        email: "sync@example.com".to_string(),
        method,
    }
}

/// Builds a connection model without signing in yet, as the shell does before
/// the form is used.
fn connection_model(
    cx: &mut TestAppContext,
    server: &MockImmich,
    method: AuthMethod,
) -> Entity<ConnectionModel> {
    let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
    let profile = profile_for(server, method);
    cx.update(|cx| cx.new(|_| ConnectionModel::new(profile, credentials)))
}

/// Signs in an existing connection model.
fn sign_in(
    cx: &mut TestAppContext,
    connection: &Entity<ConnectionModel>,
    server: &MockImmich,
    method: AuthMethod,
    secret: &str,
) {
    let profile = profile_for(server, method);
    cx.update(|cx| {
        connection.update(cx, |model, cx| {
            model.sign_in(
                SignIn {
                    email: profile.email.clone(),
                    secret: Secret::new(secret),
                    method,
                },
                profile,
                cx,
            );
        });
    })
}

/// Builds and signs in a connection model in one go.
fn signed_in(
    cx: &mut TestAppContext,
    server: &MockImmich,
    method: AuthMethod,
    secret: &str,
) -> Entity<ConnectionModel> {
    let connection = connection_model(cx, server, method);
    sign_in(cx, &connection, server, method, secret);
    connection
}

/// Pumps the executor until `ready` holds, or panics after the timeout.
async fn wait_until(cx: &mut TestAppContext, what: &str, ready: impl FnMut(&mut App) -> bool) {
    wait_until_reported(cx, what, ready, |_| String::new()).await;
}

/// The same wait, with a description of the stuck state on failure.
async fn wait_until_reported(
    cx: &mut TestAppContext,
    what: &str,
    mut ready: impl FnMut(&mut App) -> bool,
    report: impl Fn(&mut App) -> String,
) {
    for _ in 0..900 {
        if cx.update(|cx| ready(cx)) {
            return;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    let report = cx.update(|cx| report(cx));
    panic!("{what} never happened: {report}");
}

/// Opens the store alone, so a test can seed it before the engine exists.
fn open_store(cx: &mut TestAppContext, paths: &AppPaths) -> Entity<StoreModel> {
    let paths = paths.clone();
    cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)))
}

/// Builds the engine and the diagnostics model over an open store.
fn open_models(
    cx: &mut TestAppContext,
    paths: &AppPaths,
    connection: Entity<ConnectionModel>,
    store: Entity<StoreModel>,
) -> (Entity<EngineModel>, Entity<DiagnosticsModel>) {
    let paths = paths.clone();
    cx.update(|cx| {
        let engine = cx.new(|cx| EngineModel::new(connection.clone(), store.clone(), &paths, cx));
        let diagnostics = cx.new(|cx| {
            DiagnosticsModel::new(
                store.clone(),
                engine.clone(),
                connection,
                paths.log_directory(),
                cx,
            )
        });
        (engine, diagnostics)
    })
}

/// Opens the real shell over the real settings and diagnostics views.
fn open_shell(
    cx: &mut TestAppContext,
    store: SettingsStore,
    paths: AppPaths,
    connection: Entity<ConnectionModel>,
) -> (
    AnyWindowHandle,
    Entity<StoreModel>,
    Entity<EngineModel>,
    Entity<DiagnosticsModel>,
) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        let mut opened = None;
        let (window, _) = gpui_kit::open_window(window_options(), cx, |window, cx| {
            let dashboard = cx.new(|_| Dashboard::new(status::sample()));
            let settings = cx.new(|cx| {
                SettingsView::new(
                    store.clone(),
                    paths.clone(),
                    autostart(),
                    connection.clone(),
                    window,
                    cx,
                )
            });
            let store_model = cx.new(|cx| StoreModel::open(&paths, cx));
            let upload_model =
                cx.new(|cx| UploadModel::new(connection.clone(), store_model.clone(), cx));
            let upload = cx.new(|cx| UploadView::new(upload_model, window, cx));
            let engine_model =
                cx.new(|cx| EngineModel::new(connection.clone(), store_model.clone(), &paths, cx));
            let diagnostics_model = cx.new(|cx| {
                DiagnosticsModel::new(
                    store_model.clone(),
                    engine_model.clone(),
                    connection.clone(),
                    paths.log_directory(),
                    cx,
                )
            });
            let diagnostics = cx.new(|cx| DiagnosticsView::new(diagnostics_model.clone(), cx));
            let failed = cx.new(|cx| {
                immich_sync::failed_view::FailedView::new(engine_model.clone(), window, cx)
            });
            let review = cx.new(|cx| {
                immich_sync::review_view::ReviewView::new(engine_model.clone(), window, cx)
            });
            let shell = cx.new(|cx| {
                AppShell::new(
                    dashboard,
                    settings,
                    upload,
                    failed,
                    diagnostics,
                    review,
                    connection.clone(),
                    cx,
                )
            });
            opened = Some((
                store_model.clone(),
                engine_model.clone(),
                diagnostics_model.clone(),
            ));
            shell
        })
        .expect("open the shell window");
        theme::apply_system_theme(theme::SystemTheme::Light, cx);
        let (store_model, engine_model, diagnostics_model) =
            opened.expect("the shell built its models");
        (window, store_model, engine_model, diagnostics_model)
    })
}

/// Moves focus to the next tab stop, as the Root's Tab binding does.
fn tab_to(window: &mut Window, id: &'static str, cx: &mut App) {
    for _ in 0..24 {
        if window.find(id).focused() == Some(true) {
            return;
        }
        window.focus_next(cx);
        window.render_frame(cx);
    }
    assert_eq!(
        window.find(id).focused(),
        Some(true),
        "{id} is not keyboard reachable"
    );
}

/// Switches the shell to the Diagnostics surface.
fn show_diagnostics(window: &mut Window, cx: &mut App) {
    window.render_frame(cx);
    tab_to(window, "nav-diagnostics", cx);
    window.press("enter", cx);
}

/// Adds one source folder straight to the store, before the engine exists.
fn seed_folder(store: &Entity<StoreModel>, cx: &mut App, path: &Path) {
    store
        .read(cx)
        .store()
        .expect("the store is open")
        .add_folder(&SourceFolder {
            path: path.display().to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
        })
        .expect("add the source folder");
}

/// Every log file's text, concatenated, as #17's grep does.
fn log_text(directory: &Path) -> String {
    let mut text = String::new();
    for entry in fs::read_dir(directory).expect("read the log directory") {
        let path = entry.expect("read a log directory entry").path();
        if path.is_file()
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(LOG_PREFIX))
        {
            text.push_str(&fs::read_to_string(&path).expect("read a log file"));
        }
    }
    text
}

#[test]
fn the_screenshot_view_override_names_the_diagnostics_surface() {
    assert_eq!(
        shell::view_for_name("diagnostics"),
        Some(ViewMode::Diagnostics)
    );
}

#[gpui_kit::test]
async fn the_view_shows_the_sqlite_state_the_server_and_the_permissions(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_runtime, server) = mock_server();
    server.set_api_key(API_KEY);
    server.set_api_key_permissions(&["asset.upload", "server.about"]);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = connection_model(cx, &server, AuthMethod::ApiKey);
    let (window, _store, _engine, diagnostics) = open_shell(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths,
        connection.clone(),
    );
    sign_in(cx, &connection, &server, AuthMethod::ApiKey, API_KEY);

    wait_until(cx, "the server probe", |cx| {
        diagnostics.read(cx).server().version.is_some()
    })
    .await;
    wait_until(cx, "the sqlite read", |cx| {
        diagnostics.read(cx).info().is_some()
    })
    .await;
    assert!(
        matches!(
            cx.read(|cx| diagnostics.read(cx).server().permissions.clone()),
            Permissions::Granted { .. }
        ),
        "the API key's permissions were read"
    );

    for system in [theme::SystemTheme::Light, theme::SystemTheme::Dark] {
        cx.update(|cx| theme::apply_system_theme(system, cx));
        cx.update_window(window, |_, window, cx| {
            show_diagnostics(window, cx);
            assert!(
                window.find("diagnostics-view").visible(),
                "the Diagnostics view is shown"
            );
            assert_eq!(
                window.find("diagnostics-server-version-value").label(),
                Some("v3.3.1"),
                "the server version is shown"
            );
            assert_eq!(
                window.find("diagnostics-client-version-value").label(),
                Some(concat!(
                    env!("CARGO_PKG_VERSION"),
                    " (",
                    env!("IMMICH_SYNC_GIT_REVISION"),
                    ")"
                )),
                "the client version row names the running binary"
            );
            let permissions = window
                .find("diagnostics-permissions-value")
                .label()
                .expect("the permissions are shown")
                .to_string();
            assert!(
                permissions.contains("asset.upload") && permissions.contains("server.about"),
                "the granted permissions are listed: {permissions}"
            );
            assert_eq!(
                window.find("diagnostics-sqlite-version-value").label(),
                Some(DB_MIGRATION_VERSION.to_string().as_str()),
                "the migration version is shown"
            );
            assert_eq!(
                window.find("diagnostics-integrity-value").label(),
                Some("ok"),
                "the integrity check is shown"
            );
            assert!(
                window
                    .find("diagnostics-sqlite-size-value")
                    .label()
                    .is_some_and(|size| !size.is_empty()),
                "the database size is shown"
            );
            assert_eq!(
                window.find("diagnostics-table-sync_item").label(),
                Some("sync_item: 0"),
                "every tracked table is counted"
            );
            assert_eq!(
                window.find("diagnostics-throughput-value").label(),
                Some("0 B/s"),
                "the throughput starts at zero"
            );
            assert_eq!(
                window.find("diagnostics-errors-empty").label(),
                Some("No failure has been recorded."),
                "with no recorded failure the section says so"
            );
        })
        .expect("update the shell window");
    }
}

#[gpui_kit::test]
async fn every_diagnostics_control_is_named_and_keyboard_reachable(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_runtime, server) = mock_server();
    server.set_api_key(API_KEY);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = connection_model(cx, &server, AuthMethod::ApiKey);
    let (window, _store, _engine, _diagnostics) = open_shell(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths,
        connection.clone(),
    );
    sign_in(cx, &connection, &server, AuthMethod::ApiKey, API_KEY);

    cx.update_window(window, |_, window, cx| {
        show_diagnostics(window, cx);
        for (id, label) in [
            ("nav-diagnostics", "Diagnostics"),
            (
                "diagnostics-refresh",
                "Re-check the server and the state database",
            ),
            (
                "diagnostics-rebuild",
                "Rebuild the state database and rescan the folders",
            ),
            (
                "diagnostics-bundle-collect",
                "Collect the debug bundle into one archive",
            ),
        ] {
            assert_eq!(window.find(id).label(), Some(label), "{id} is named");
        }
        for id in [
            "nav-diagnostics",
            "diagnostics-refresh",
            "diagnostics-rebuild",
            "diagnostics-bundle-collect",
        ] {
            tab_to(window, id, cx);
        }
    })
    .expect("update the shell window");
}

#[gpui_kit::test]
async fn a_password_session_shows_the_account_permissions_without_a_key_read(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let (_runtime, server) = mock_server();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = connection_model(cx, &server, AuthMethod::Password);
    let store = open_store(cx, &paths);
    let (_engine, diagnostics) = open_models(cx, &paths, connection.clone(), store);
    sign_in(
        cx,
        &connection,
        &server,
        AuthMethod::Password,
        "correct horse",
    );

    wait_until(cx, "the version probe", |cx| {
        diagnostics.read(cx).server().version.is_some()
    })
    .await;

    assert_eq!(
        cx.read(|cx| diagnostics.read(cx).server().permissions.clone()),
        Permissions::Account,
        "a password session has the account's permissions, not a key's list"
    );
    assert_eq!(
        server.api_key_me_requests(),
        0,
        "the key read is skipped for a credential that cannot answer it"
    );
}

#[gpui_kit::test]
async fn a_missing_version_permission_is_an_actionable_message(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_runtime, server) = mock_server();
    server.set_api_key(API_KEY);
    // The key may upload but may not read the server version, which the real
    // server answers with 403 Missing required permission: server.about.
    server.set_api_key_permissions(&["asset.upload"]);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = connection_model(cx, &server, AuthMethod::ApiKey);
    let (window, _store, _engine, diagnostics) = open_shell(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths,
        connection.clone(),
    );
    sign_in(cx, &connection, &server, AuthMethod::ApiKey, API_KEY);

    wait_until(cx, "the refusal", |cx| {
        diagnostics.read(cx).server().problem.is_some()
    })
    .await;
    let problem = cx.read(|cx| diagnostics.read(cx).server().problem.clone().unwrap());
    assert!(
        problem.contains("server.about"),
        "the message names the permission to grant: {problem}"
    );

    cx.update_window(window, |_, window, cx| {
        show_diagnostics(window, cx);
        let shown = window
            .find("diagnostics-server-problem")
            .label()
            .expect("the refusal is shown")
            .to_string();
        assert!(
            shown.contains("server.about"),
            "the view repeats the actionable message: {shown}"
        );
    })
    .expect("update the shell window");
}

// Unix only: this test deletes the database out from under the open store,
// and Windows refuses to remove a file an open handle still maps. Windows
// recovery of a hand-deleted database happens with the client stopped, and
// `StateStore::open` and the `rebuild_store` unit tests cover it there.
#[cfg(unix)]
#[gpui_kit::test]
async fn a_rebuild_recovers_a_deleted_state_database_and_rescans_the_folder(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let (_runtime, server) = mock_server();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    fs::create_dir_all(&photos).expect("create the source folder");
    let file = photos.join("one.jpg");
    fs::write(&file, b"one photo").expect("write the fixture");
    let file_path = file.display().to_string();

    let store_model = open_store(cx, &paths);
    wait_until(cx, "the store", |cx| store_model.read(cx).store().is_some()).await;
    // The folder belongs to the store before the engine exists, exactly as it
    // does when a user adds it in a running client.
    cx.update(|cx| seed_folder(&store_model, cx, &photos));
    let connection = signed_in(cx, &server, AuthMethod::Password, "correct horse");
    let (engine, diagnostics) = open_models(cx, &paths, connection, store_model.clone());

    wait_until_reported(
        cx,
        "the first upload",
        |cx| {
            store_model
                .read(cx)
                .store()
                .and_then(|store| store.item(&file_path).ok().flatten())
                .is_some_and(|row| row.state.is_terminal_success())
        },
        |cx| {
            format!(
                "running={} engine={:?}",
                engine.read(cx).is_running(),
                engine.read(cx).status()
            )
        },
    )
    .await;

    // R10.4's acceptance: the database is deleted by hand.
    for suffix in ["", "-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", paths.state_db.display()));
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove {}: {error}", path.display()),
        }
    }
    assert!(!paths.state_db.exists(), "the database file is gone");

    let probes_before = server.about_requests();
    cx.update(|cx| diagnostics.update(cx, |model, cx| model.rebuild(cx)));

    wait_until_reported(
        cx,
        "the rebuilt store",
        |cx| {
            let store = store_model.read(cx);
            store.store().is_some()
                && store
                    .folders()
                    .iter()
                    .any(|folder| folder.path == photos.display().to_string())
        },
        |cx| {
            let store = store_model.read(cx);
            format!(
                "problem={:?} folders={:?} rebuilding={} db_exists={} generation={}",
                store.problem(),
                store
                    .folders()
                    .iter()
                    .map(|f| f.path.clone())
                    .collect::<Vec<_>>(),
                diagnostics.read(cx).is_rebuilding(),
                paths.state_db.exists(),
                store.generation()
            )
        },
    )
    .await;
    assert!(
        paths.state_db.exists(),
        "the rebuild recreated the database file"
    );
    let store = cx.read(|cx| store_model.read(cx).store().expect("the store reopened"));
    store.quick_check().expect("the rebuilt database is sound");
    assert_eq!(
        store
            .sqlite_info()
            .expect("read the rebuilt state")
            .migration_version,
        DB_MIGRATION_VERSION
    );

    // R10.4: the rebuild rescans the folders and re-checks the server.
    wait_until(cx, "the rescan", |cx| {
        store_model
            .read(cx)
            .store()
            .and_then(|store| store.item(&file_path).ok().flatten())
            .is_some_and(|row| row.state.is_terminal_success())
    })
    .await;
    wait_until(cx, "the server re-check", |_| {
        server.about_requests() > probes_before
    })
    .await;
}

/// R10.4: the first open owns the store until it lands. A rebuild clicked
/// before then must not reset an empty database and wipe the folders.
#[gpui_kit::test]
async fn a_rebuild_during_the_first_open_keeps_the_stored_folders(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    fs::create_dir_all(&photos).expect("create the source folder");
    // A database a previous run left behind, folder and all.
    {
        let store = StateStore::open(&paths.state_db).expect("open the state database");
        store
            .add_folder(&SourceFolder {
                path: photos.display().to_string(),
                recursive: true,
                album_mapping: false,
                album_scope: AlbumScope::Leaf,
                album_title: None,
            })
            .expect("add the source folder");
    }

    let store_model = open_store(cx, &paths);
    // The click lands before the open settles.
    cx.update(|cx| store_model.update(cx, |model, cx| model.rebuild(cx)));
    assert_eq!(
        cx.read(|cx| store_model.read(cx).problem().map(str::to_string)),
        Some("The state database is still opening.".to_string()),
        "the refused rebuild carries the reason the user sees"
    );

    wait_until(cx, "the store", |cx| store_model.read(cx).store().is_some()).await;
    assert!(
        cx.read(|cx| store_model
            .read(cx)
            .folders()
            .iter()
            .any(|folder| folder.path == photos.display().to_string())),
        "the refused rebuild must not wipe the folders the database holds"
    );
    cx.read(|cx| store_model.read(cx).store())
        .expect("the store is open")
        .quick_check()
        .expect("the database is intact");
}

/// R10.2: a broken errors read is reported, not silently dropped.
#[gpui_kit::test]
async fn a_failed_error_read_is_surfaced(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_runtime, server) = mock_server();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = connection_model(cx, &server, AuthMethod::Password);
    let store = open_store(cx, &paths);
    let (_engine, diagnostics) = open_models(cx, &paths, connection, store);
    wait_until(cx, "the sqlite read", |cx| {
        diagnostics.read(cx).info().is_some()
    })
    .await;

    // Break only the query behind the error section: the schema, the size,
    // and the integrity check stay readable.
    {
        let raw = rusqlite::Connection::open(&paths.state_db).expect("open the database directly");
        raw.execute_batch("DROP TABLE attempt")
            .expect("drop the attempt log");
    }
    cx.update(|cx| diagnostics.update(cx, |model, cx| model.refresh(cx)));

    wait_until(cx, "the problem", |cx| {
        diagnostics.read(cx).problem().is_some()
    })
    .await;
    let problem = cx.read(|cx| diagnostics.read(cx).problem().unwrap().to_string());
    assert!(
        problem.contains("failures"),
        "the message says what could not be read: {problem}"
    );
}

/// R10.4: a double click cannot start two rebuilds; the second click is a
/// no-op while the first still runs.
#[gpui_kit::test]
async fn a_second_rebuild_cannot_start_while_one_runs(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_runtime, server) = mock_server();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = connection_model(cx, &server, AuthMethod::Password);
    let (window, store, _engine, diagnostics) = open_shell(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths,
        connection,
    );
    wait_until(cx, "the store", |cx| store.read(cx).store().is_some()).await;
    let generation = cx.read(|cx| store.read(cx).generation());
    cx.update_window(window, |_, window, cx| show_diagnostics(window, cx))
        .expect("update the shell window");

    // A double click: the second click lands while the first rebuild runs. The
    // button is also disabled while rebuilding, and this model guard is what a
    // direct call cannot bypass.
    cx.update_window(window, |_, window, cx| {
        window.click("diagnostics-rebuild", cx);
        window.click("diagnostics-rebuild", cx);
    })
    .expect("update the shell window");

    wait_until(cx, "the rebuild", |cx| {
        !diagnostics.read(cx).is_rebuilding() && store.read(cx).store().is_some()
    })
    .await;
    assert_eq!(
        cx.read(|cx| store.read(cx).generation()),
        generation + 1,
        "the second click must not replace the store a second time"
    );
}

#[gpui_kit::test]
async fn the_throughput_follows_the_engine_meter_while_a_file_uploads(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_runtime, server) = mock_server();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let photos = temp.path().join("photos");
    fs::create_dir_all(&photos).expect("create the source folder");
    // A slow limit, so the upload takes seconds and the sampler sees bytes
    // move between two polls.
    fs::create_dir_all(paths.config_file.parent().expect("the config directory"))
        .expect("create the config directory");
    fs::write(
        &paths.config_file,
        "bandwidth_bytes_per_second = 131072\nconcurrency = 1\n",
    )
    .expect("write the settings");
    let file = photos.join("large.bin");
    fs::write(&file, vec![9_u8; 256 * 1024]).expect("write the fixture");
    let file_path = file.display().to_string();

    let store_model = open_store(cx, &paths);
    wait_until(cx, "the store", |cx| store_model.read(cx).store().is_some()).await;
    cx.update(|cx| seed_folder(&store_model, cx, &photos));
    let connection = signed_in(cx, &server, AuthMethod::Password, "correct horse");
    let (engine, diagnostics) = open_models(cx, &paths, connection, store_model.clone());

    wait_until(cx, "a live throughput", |cx| {
        diagnostics.read(cx).throughput_bytes_per_second() >= 1_000.0
    })
    .await;

    wait_until(cx, "the upload", |cx| {
        store_model
            .read(cx)
            .store()
            .and_then(|store| store.item(&file_path).ok().flatten())
            .is_some_and(|row| row.state.is_terminal_success())
    })
    .await;
    // With nothing moving, the next sample reports zero: the readout follows
    // the meter in both directions.
    wait_until(cx, "the idle throughput", |cx| {
        diagnostics.read(cx).throughput_bytes_per_second() == 0.0
    })
    .await;
    assert!(
        cx.read(|cx| engine.read(cx).uploaded_bytes()) >= 256 * 1024,
        "the engine's meter counted the file"
    );
}

#[gpui_kit::test]
async fn the_probe_logs_its_outcome_without_the_credential(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_runtime, server) = mock_server();
    server.set_api_key(API_KEY);
    server.set_api_key_permissions(&["server.about"]);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    LogSink::start(
        &paths.log_directory(),
        LogPolicy::default(),
        LogLevel::Trace,
    )
    .expect("start the log")
    .install()
    .expect("install the process log sink");

    let connection = signed_in(cx, &server, AuthMethod::ApiKey, API_KEY);
    let store = open_store(cx, &paths);
    let (_engine, diagnostics) = open_models(cx, &paths, connection, store);
    wait_until(cx, "the permissions probe", |cx| {
        matches!(
            diagnostics.read(cx).server().permissions,
            Permissions::Granted { .. }
        )
    })
    .await;

    immich_sync::logging::shutdown();
    let text = log_text(&paths.log_directory());
    assert!(
        text.contains("Immich request finished"),
        "the probe reaches the #17 logging pipeline: {text}"
    );
    assert!(
        text.contains("api-key-permissions"),
        "the permissions read logs its outcome: {text}"
    );
    assert!(
        !text.contains(API_KEY),
        "the log file holds the API key: {text}"
    );
}

/// A credential-like string seeded where the client keeps one (the API key)
/// and where a hostile server's answer would land (a recorded failure
/// message). It must appear nowhere in the archive (issue #25).
const BUNDLE_SENTINEL: &str = "s3cr3t-api-key-9f3c1b2a";

/// One seeded item row for the bundle tests.
fn bundle_item(path: &str, state: ItemState) -> ItemRecord {
    ItemRecord {
        path: path.to_string(),
        size: 4096,
        mtime_ns: 1_700_000_000_000_000_000,
        sha1: sha1(b"bundle fixture"),
        asset_id: None,
        state,
        error_class: None,
        error_message: None,
        attempts: 0,
        sidecar: None,
    }
}

/// The archive's entry names and texts, read with the archive reader.
fn read_archive(path: &Path) -> (Vec<(String, String)>, Vec<u8>) {
    use std::io::Read as _;
    let bytes = fs::read(path).expect("read the archive");
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).expect("open the archive");
    let names: Vec<String> = archive
        .file_names()
        .map(|name| name.expect("a readable entry name").into_owned())
        .collect();
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        let mut body = String::new();
        archive
            .by_name(&name)
            .expect("open an archive entry")
            .read_to_string(&mut body)
            .expect("read an archive entry");
        entries.push((name, body));
    }
    (entries, bytes)
}

/// Issue #25: one action writes one archive off the UI thread, the view shows
/// its path, the copy control puts it on the clipboard, and no credential
/// reaches the archive.
#[gpui_kit::test]
async fn the_bundle_action_saves_an_archive_shows_its_path_and_copies_it(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_runtime, server) = mock_server();
    server.set_api_key(BUNDLE_SENTINEL);
    server.set_api_key_permissions(&["asset.upload", "server.about"]);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    // The real log pipeline writes the archive's logs, so a #17 redaction
    // regression fails the sentinel scan below. It is installed on this thread
    // only: a process installs one subscriber and the test binary shares it.
    let mut sink = LogSink::start(
        &paths.log_directory(),
        LogPolicy::default(),
        LogLevel::Trace,
    )
    .expect("start the log");
    let guard = sink.thread_default();
    // A settings file, as a running client would have written it. The URL
    // carries a password in its userinfo, which #17's pipeline must drop, and
    // the settings file never holds the credential itself.
    fs::create_dir_all(paths.config_file.parent().expect("the config directory"))
        .expect("create the config directory");
    fs::write(
        &paths.config_file,
        format!(
            "server_url = \"https://sync:{BUNDLE_SENTINEL}@immich.example.com\"\n\
             email = \"sync@example.com\"\n"
        ),
    )
    .expect("write the settings");
    // The test shell saves bundles beside the logs; the real shell saves into
    // the user's Downloads folder.
    let bundle_directory = paths.log_directory();

    let connection = connection_model(cx, &server, AuthMethod::ApiKey);
    // `tracing` caches a callsite's interest for the whole process. A test
    // thread with no subscriber can register one of the client's callsites
    // first, while this test's thread-local sink is the only installed
    // dispatcher (`a_missing_version_permission_is_an_actionable_message` does
    // exactly that). The cache then says "never", and the client's event is
    // dropped even on this thread: issue #49's missing log line. If the test
    // rebuilds the cache while its own sink is installed, every callsite is
    // enabled again. The production client installs one process-wide sink, so
    // there is no second dispatcher that could poison the cache. This first
    // rebuild is insurance for what `open_shell` logs before the sign-in; the
    // measured poison window is the store wait, guarded again below.
    tracing::callsite::rebuild_interest_cache();
    let (window, store, _engine, diagnostics) = open_shell(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths.clone(),
        connection.clone(),
    );
    wait_until(cx, "the store", |cx| store.read(cx).store().is_some()).await;
    // The seeded failure's message is the sentinel: the archive must not carry
    // the stored server text even when it looks like a credential.
    cx.read(|cx| {
        let store = store.read(cx).store().expect("the store is open");
        let mut failed = bundle_item("/photos/RAW_0002.cr3", ItemState::FailedPermanent);
        failed.error_class = Some(FailureClass::UnsupportedFormat);
        failed.error_message = Some(format!("the server answered: {BUNDLE_SENTINEL}"));
        store.record(&failed).expect("write the failed item");
        store
            .record_attempt(&AttemptRecord {
                path: failed.path.clone(),
                started_ns: 10,
                outcome: AttemptOutcome::Failed,
                error_class: Some(FailureClass::UnsupportedFormat),
            })
            .expect("record the attempt");
    });
    // The store wait above is a wide window for the same poison, and the sign-in
    // line is the one the archive assertion needs: rebuild again right before it.
    tracing::callsite::rebuild_interest_cache();
    sign_in(
        cx,
        &connection,
        &server,
        AuthMethod::ApiKey,
        BUNDLE_SENTINEL,
    );
    wait_until(cx, "the server probe", |cx| {
        diagnostics.read(cx).server().version.is_some()
    })
    .await;
    // Flush the queued lines before the collector reads the log directory.
    drop(guard);
    drop(sink);

    cx.update_window(window, |_, window, cx| {
        show_diagnostics(window, cx);
        window.click("diagnostics-bundle-collect", cx);
        // The click returned before the archive was written: collection runs
        // off the UI thread.
        assert!(
            matches!(
                diagnostics.read(cx).bundle(),
                BundleState::Collecting | BundleState::Ready(_)
            ),
            "the click starts one collection"
        );
    })
    .expect("update the shell window");
    wait_until(cx, "the debug bundle", |cx| {
        matches!(diagnostics.read(cx).bundle(), BundleState::Ready(_))
    })
    .await;

    let report = match cx.read(|cx| diagnostics.read(cx).bundle().clone()) {
        BundleState::Ready(report) => report,
        other => panic!("the bundle is ready: {other:?}"),
    };
    assert_eq!(
        report.path.parent(),
        Some(bundle_directory.as_path()),
        "the archive lands in the folder the view names"
    );
    assert!(
        report
            .path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(BUNDLE_PREFIX)),
        "the archive is named for the client: {}",
        report.path.display()
    );
    assert!(
        report.notes.is_empty(),
        "every section was collected: {:?}",
        report.notes
    );

    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            window.find("diagnostics-bundle-path-value").label(),
            Some(report.path.display().to_string().as_str()),
            "the view shows the archive's full path"
        );
        window.click("diagnostics-bundle-copy", cx);
        let notice = window.find("diagnostics-bundle-copy-notice");
        assert_eq!(notice.role(), Some(Role::Alert));
        assert_eq!(
            notice.label(),
            Some("Debug bundle path copied to the clipboard.")
        );
    })
    .expect("update the shell window");
    let copied = cx
        .update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .unwrap_or_default();
    assert_eq!(
        copied,
        report.path.display().to_string(),
        "the copy control puts the archive's path on the clipboard"
    );

    cx.update_window(window, |_, window, cx| {
        window.scroll(
            "diagnostics-view",
            ScrollDelta::Pixels(point(px(0.), px(-1200.))),
            cx,
        );
        window.render_frame(cx);
        window.click("diagnostics-bundle-copy-folder", cx);
        let notice = window.find("diagnostics-bundle-copy-notice");
        assert_eq!(
            notice.role(),
            Some(Role::Alert),
            "copy confirmation is announced once"
        );
        assert_eq!(
            notice.label(),
            Some("Debug bundle folder path copied to the clipboard.")
        );
    })
    .expect("update the shell window");
    let copied_folder = cx
        .update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .unwrap_or_default();
    assert_eq!(
        copied_folder,
        report.path.parent().unwrap().display().to_string(),
        "the folder copy control puts the debug bundle folder on the clipboard"
    );

    let (entries, bytes) = read_archive(&report.path);
    for required in [
        "README.txt",
        "versions.txt",
        "database.txt",
        "configuration.txt",
        "settings.toml",
        "sizing.txt",
        "runtime.txt",
    ] {
        assert!(
            entries.iter().any(|(name, _)| name == required),
            "the archive holds {required}: {:?}",
            entries.iter().map(|(name, _)| name).collect::<Vec<_>>()
        );
    }
    let logs: Vec<&str> = entries
        .iter()
        .filter(|(name, _)| name.starts_with("logs/"))
        .map(|(_, body)| body.as_str())
        .collect();
    assert!(
        logs.iter().any(|body| body.contains("sign-in attempt")),
        "the archive holds the log the client wrote: {logs:?}"
    );
    assert!(
        logs.iter()
            .any(|body| body.contains("https://<redacted>@immich.example.com")),
        "the pipeline's line drops the URL's userinfo: {logs:?}"
    );
    for (name, body) in &entries {
        assert!(
            !body.contains(BUNDLE_SENTINEL),
            "{name} carries the credential: {body}"
        );
    }
    assert!(
        !bytes
            .windows(BUNDLE_SENTINEL.len())
            .any(|window| window == BUNDLE_SENTINEL.as_bytes()),
        "the raw archive bytes carry the credential"
    );
    let database = entries
        .iter()
        .find(|(name, _)| name == "database.txt")
        .map(|(_, body)| body.as_str())
        .expect("the database section");
    assert!(
        database.contains("unsupported_format"),
        "the last error per class names the class it recorded: {database}"
    );
}

/// Issue #25: a second collect cannot start while one runs; the model guard is
/// what a direct call cannot bypass, and only one archive is written.
#[gpui_kit::test]
async fn a_second_bundle_collection_cannot_start_while_one_runs(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_runtime, server) = mock_server();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let connection = connection_model(cx, &server, AuthMethod::Password);
    let store = open_store(cx, &paths);
    let (_engine, diagnostics) = open_models(cx, &paths, connection, store);
    wait_until(cx, "the store", |cx| diagnostics.read(cx).info().is_some()).await;

    cx.update(|cx| {
        diagnostics.update(cx, |model, cx| {
            model.collect_bundle(cx);
            assert_eq!(
                *model.bundle(),
                BundleState::Collecting,
                "the first call starts the collector"
            );
            model.collect_bundle(cx);
            assert_eq!(
                *model.bundle(),
                BundleState::Collecting,
                "the second call must not start a second collection"
            );
        });
    });
    wait_until(cx, "the debug bundle", |cx| {
        matches!(diagnostics.read(cx).bundle(), BundleState::Ready(_))
    })
    .await;

    let archives: Vec<_> = fs::read_dir(paths.log_directory())
        .expect("read the bundle directory")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "zip"))
        .collect();
    assert_eq!(
        archives.len(),
        1,
        "one collection writes one archive: {archives:?}"
    );
}

/// Issue #25: a server the client cannot reach makes a partial archive with a
/// note, not an error.
#[gpui_kit::test]
async fn an_unreachable_server_makes_a_partial_bundle_with_a_note(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    // Nothing listens on this port, so the sign-in fails as unreachable.
    let profile = ServerProfile {
        server_url: "http://127.0.0.1:1".to_string(),
        email: "sync@example.com".to_string(),
        method: AuthMethod::Password,
    };
    let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
    let connection = cx.update(|cx| cx.new(|_| ConnectionModel::new(profile.clone(), credentials)));
    let (window, _store, _engine, diagnostics) = open_shell(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths.clone(),
        connection.clone(),
    );
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
    wait_until(cx, "the unreachable server", |cx| {
        matches!(
            connection.read(cx).state(),
            ConnectionState::NeedsAttention(_)
        )
    })
    .await;

    cx.update(|cx| diagnostics.update(cx, |model, cx| model.collect_bundle(cx)));
    wait_until(cx, "the partial bundle", |cx| {
        matches!(diagnostics.read(cx).bundle(), BundleState::Ready(_))
    })
    .await;

    let report = match cx.read(|cx| diagnostics.read(cx).bundle().clone()) {
        BundleState::Ready(report) => report,
        other => panic!("the partial bundle is ready: {other:?}"),
    };
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("server version")),
        "the note says the server version is missing: {:?}",
        report.notes
    );
    assert!(
        report.notes.iter().any(|note| note.contains("No log file")),
        "the note says the client never logged: {:?}",
        report.notes
    );
    let (entries, _) = read_archive(&report.path);
    let versions = entries
        .iter()
        .find(|(name, _)| name == "versions.txt")
        .map(|(_, body)| body.as_str())
        .expect("the versions section");
    assert!(
        versions.contains("Immich server: not read"),
        "the versions section says the server was not read: {versions}"
    );
    // The view shows every note the archive could not collect, each announced
    // on its own.
    cx.update_window(window, |_, window, cx| {
        show_diagnostics(window, cx);
        for (index, note) in report.notes.iter().enumerate() {
            let element = window.find(ElementId::Name(
                format!("diagnostics-bundle-note-{index}").into(),
            ));
            assert_eq!(
                element.role(),
                Some(Role::Alert),
                "the partial bundle's note is announced"
            );
            assert_eq!(
                element.label().map(ToString::to_string),
                Some(note.clone()),
                "the note shows the archive's own text"
            );
        }
    })
    .expect("update the shell window");
}
