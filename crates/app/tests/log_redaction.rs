//! The credential redaction acceptance (SPEC R10.3, N3): real sign-ins through
//! the real form, against the offline Immich double, with the real log
//! pipeline, and a grep of the log FILE afterwards.
//!
//! The events the app writes never record a secret, and the types that hold one
//! redact their `Debug`. These tests drive the whole path and then read the
//! bytes that were actually written, so a future event that formats a secret
//! fails here.

use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Point, TestAppContext, Window, WindowBounds,
    WindowOptions, px, size,
};
use immich_sync::autostart::{Autostart, UnavailableAutostart};
use immich_sync::connection::{ConnectionModel, profile_from_settings};
use immich_sync::logging::{LOG_PREFIX, LogLevel, LogPolicy, LogSink};
use immich_sync::paths::AppPaths;
use immich_sync::settings::SettingsStore;
use immich_sync::settings_view::SettingsView;
use mock_immich::MockImmich;
use sync_core::connection::{AuthMethod, RequestCredential, Secret, SignIn};
use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};

/// The paths a test run uses, all inside one temp directory.
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
            // Tall enough for the whole settings form, so the sign-in button
            // and the fields are laid out.
            size: size(px(1200.), px(1400.)),
        })),
        ..Default::default()
    }
}

/// Opens the real settings view, exactly as the shell builds it.
fn open_settings_view(
    cx: &mut TestAppContext,
    store: SettingsStore,
    paths: AppPaths,
    credentials: SharedCredentialStore,
) -> AnyWindowHandle {
    cx.update(|cx| {
        gpui_kit::init(cx);
        let profile = profile_from_settings(store.settings());
        let connection = cx.new(|_| ConnectionModel::new(profile, credentials));
        let autostart: Rc<dyn Autostart> = Rc::new(UnavailableAutostart::new(
            "the test run has no OS autostart mechanism".to_string(),
        ));
        gpui_kit::open_window(window_options(), cx, |window, cx| {
            cx.new(|cx| SettingsView::new(store, paths, autostart, connection, window, cx))
        })
        .expect("open the settings window")
        .0
    })
}

/// Starts the offline Immich double on its own runtime: a GPUI test runs on the
/// GPUI executor, which is not a Tokio runtime.
fn mock_server() -> (tokio::runtime::Runtime, MockImmich) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("start the mock runtime");
    let server = runtime.block_on(MockImmich::start());
    (runtime, server)
}

/// Moves focus to the next tab stop, as the Root's Tab binding does.
fn tab_to(window: &mut Window, id: &'static str, cx: &mut App) {
    for _ in 0..16 {
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

/// Every log file's text, concatenated. A rotation cannot hide a leak from the
/// grep.
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

/// Fails when the log text contains `secret`.
fn assert_log_hides(text: &str, secret: &str, what: &str) {
    assert!(
        !text.contains(secret),
        "the log file holds the {what}: {text}"
    );
}

#[gpui_kit::test]
async fn a_password_login_writes_no_password_and_no_session_token_to_the_log(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let (runtime, server) = mock_server();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut sink = LogSink::start(
        &paths.log_directory(),
        LogPolicy::default(),
        LogLevel::Trace,
    )
    .expect("start the log");
    let guard = sink.thread_default();

    let password = "correct horse";
    let window = open_settings_view(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths.clone(),
        Arc::new(MemoryCredentialStore::new()),
    );

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "settings-server-url", cx);
        window.input(server.base_url(), cx);
        tab_to(window, "settings-email", cx);
        window.input("sync@example.com", cx);
        tab_to(window, "settings-secret", cx);
        window.input(password, cx);
        window.click("settings-sign-in", cx);
    })
    .expect("fill in the connection form and sign in");

    cx.wait_for(window, Duration::from_secs(30), |window, _| {
        window.find("settings-connection-state").label() == Some("Connected")
    })
    .await;
    cx.run_until_parked();

    let tokens = server.issued_tokens();
    assert!(!tokens.is_empty(), "the sign-in was issued a session token");

    // The last line of defence behind the events: a `{:?}` dump of the form's
    // request, through the same layer into the same file.
    tracing::debug!(
        request = ?SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new(password),
            method: AuthMethod::Password,
        },
        "a debug dump of the sign-in request cannot leak"
    );

    drop(guard);
    drop(sink);

    let text = log_text(&paths.log_directory());
    assert!(
        text.contains("sign-in attempt"),
        "the attempt is logged: {text}"
    );
    assert!(
        text.contains("sign-in outcome"),
        "the outcome is logged: {text}"
    );
    assert!(
        text.contains("Secret(<redacted>)"),
        "the debug dump redacts the secret: {text}"
    );
    assert_log_hides(&text, password, "password");
    for token in &tokens {
        assert_log_hides(&text, token, "session token");
    }
    drop(runtime);
}

#[gpui_kit::test]
async fn a_refused_password_login_writes_no_password_to_the_log(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (runtime, server) = mock_server();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut sink = LogSink::start(
        &paths.log_directory(),
        LogPolicy::default(),
        LogLevel::Trace,
    )
    .expect("start the log");
    let guard = sink.thread_default();

    let password = "wrong horse";
    let window = open_settings_view(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths.clone(),
        Arc::new(MemoryCredentialStore::new()),
    );

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "settings-server-url", cx);
        window.input(server.base_url(), cx);
        tab_to(window, "settings-email", cx);
        window.input("sync@example.com", cx);
        tab_to(window, "settings-secret", cx);
        window.input(password, cx);
        window.click("settings-sign-in", cx);
    })
    .expect("fill in the connection form and sign in");

    cx.wait_for(window, Duration::from_secs(30), |window, _| {
        window
            .find("settings-connection-state")
            .label()
            .is_some_and(|label| label.contains("rejected the password"))
    })
    .await;
    cx.run_until_parked();

    drop(guard);
    drop(sink);

    let text = log_text(&paths.log_directory());
    assert!(
        text.contains("sign-in attempt"),
        "the attempt is logged: {text}"
    );
    assert!(
        text.contains("sign-in outcome"),
        "the refusal is logged: {text}"
    );
    assert_log_hides(&text, password, "password");
    assert!(
        text.contains("rejected the password"),
        "the outcome names the reason: {text}"
    );
    drop(runtime);
}

#[gpui_kit::test]
async fn an_api_key_login_writes_no_key_to_the_log(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (runtime, server) = mock_server();
    let key = "api-key-abc123";
    server.set_api_key(key);

    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut sink = LogSink::start(
        &paths.log_directory(),
        LogPolicy::default(),
        LogLevel::Trace,
    )
    .expect("start the log");
    let guard = sink.thread_default();

    let window = open_settings_view(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths.clone(),
        Arc::new(MemoryCredentialStore::new()),
    );

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "settings-server-url", cx);
        window.input(server.base_url(), cx);
        tab_to(window, "settings-email", cx);
        window.input("sync@example.com", cx);
        window.click("settings-auth-apikey", cx);
        tab_to(window, "settings-secret", cx);
        window.input(key, cx);
        window.click("settings-sign-in", cx);
    })
    .expect("fill in the connection form and sign in");

    cx.wait_for(window, Duration::from_secs(30), |window, _| {
        window.find("settings-connection-state").label() == Some("Connected")
    })
    .await;
    cx.run_until_parked();

    tracing::debug!(
        credential = ?RequestCredential::ApiKey(Secret::new(key)),
        "a debug dump of the credential cannot leak"
    );

    drop(guard);
    drop(sink);

    let text = log_text(&paths.log_directory());
    assert!(
        text.contains("sign-in attempt"),
        "the attempt is logged: {text}"
    );
    assert!(
        text.contains("method=ApiKey"),
        "the attempt names the method: {text}"
    );
    assert!(
        text.contains("ApiKey(<redacted>)"),
        "the debug dump redacts the key: {text}"
    );
    assert_log_hides(&text, key, "API key");
    drop(runtime);
}
