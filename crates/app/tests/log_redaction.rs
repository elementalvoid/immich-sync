//! The credential redaction acceptance (SPEC R10.3, N3): real sign-ins through
//! the real form, against the offline Immich double, with the real log
//! pipeline, and a grep of the log FILES afterwards.
//!
//! The events the app writes never record a secret, and the types that hold one
//! redact their `Debug`. The sign-in runs on the HTTP runtime, while the test
//! runs on the GPUI executor, so the sink must be the process-global
//! [`LogSink::install`]: a thread-local sink would drop the worker's events
//! instead of writing them, and the grep could never catch a secret leaked from
//! the network path. One scenario points the form at a hostile server that
//! echoes the submitted password in its error body, so a server-supplied
//! message cannot slip a secret into the log either.

mod common;

use common::mock_server;

use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

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
use mock_immich::{Fault, Operation};
use sync_core::connection::{AuthMethod, RequestCredential, Secret, SignIn};
use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};

/// The paths one scenario uses, all inside the shared temp directory.
///
/// The settings file is fresh per scenario, so a profile saved by an earlier
/// sign-in cannot pre-fill the next scenario's form. The log directory is
/// shared, so one grep sees every scenario.
fn scenario_paths(dir: &Path, scenario: &str) -> AppPaths {
    AppPaths {
        config_file: dir.join("config").join(format!("settings-{scenario}.toml")),
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

/// Opens Settings, fills the connection form, and clicks Sign in.
fn fill_and_sign_in(
    cx: &mut TestAppContext,
    paths: &AppPaths,
    server_url: &str,
    secret: &str,
    method: AuthMethod,
) -> AnyWindowHandle {
    let window = open_settings_view(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths.clone(),
        Arc::new(MemoryCredentialStore::new()),
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "settings-server-url", cx);
        window.input(server_url, cx);
        tab_to(window, "settings-email", cx);
        window.input("sync@example.com", cx);
        if method == AuthMethod::ApiKey {
            window.click("settings-auth-apikey", cx);
        }
        tab_to(window, "settings-secret", cx);
        window.input(secret, cx);
        window.click("settings-sign-in", cx);
    })
    .expect("fill in the connection form and sign in");
    window
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

/// Polls the connection-state label until `ready` accepts it, then returns.
///
/// A timeout reports the label that was actually shown, which the generic
/// `wait_for` cannot.
async fn wait_for_state(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    ready: impl Fn(&str) -> bool,
) {
    for _ in 0..300 {
        let label = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window
                    .find("settings-connection-state")
                    .label()
                    .unwrap_or("<no state>")
                    .to_string()
            })
            .expect("read the connection state");
        if ready(&label) {
            return;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
    }
    let shown = cx
        .update_window(window, |_, window, _| {
            window
                .find("settings-connection-state")
                .label()
                .unwrap_or("<no state>")
                .to_string()
        })
        .expect("read the connection state");
    panic!("the connection state never matched; it shows {shown:?}");
}

#[gpui_kit::test]
async fn a_real_sign_in_never_writes_a_secret_to_the_log(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (runtime, server) = mock_server();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let dir = temp.path();
    let paths = scenario_paths(dir, "password");
    LogSink::start(
        &paths.log_directory(),
        LogPolicy::default(),
        LogLevel::Trace,
    )
    .expect("start the log")
    .install()
    .expect("install the process log sink");
    cx.update(gpui_kit::init);

    // A password sign-in succeeds and is issued a session token.
    let password = "correct horse";
    let window = fill_and_sign_in(
        cx,
        &paths,
        server.base_url(),
        password,
        AuthMethod::Password,
    );
    wait_for_state(cx, window, |label| label == "Connected").await;
    let tokens = server.issued_tokens();
    assert!(!tokens.is_empty(), "the sign-in was issued a session token");

    // A refused password.
    let refused = "wrong horse";
    let paths = scenario_paths(dir, "refused");
    let window = fill_and_sign_in(cx, &paths, server.base_url(), refused, AuthMethod::Password);
    wait_for_state(cx, window, |label| label.contains("rejected the password")).await;

    // An API key.
    let key = "api-key-abc123";
    server.set_api_key(key);
    let paths = scenario_paths(dir, "api-key");
    let window = fill_and_sign_in(cx, &paths, server.base_url(), key, AuthMethod::ApiKey);
    wait_for_state(cx, window, |label| label == "Connected").await;

    // A hostile server echoes the submitted password in its error body. The
    // user-facing state may repeat it; the log file must not.
    let echoed = "echoed horse";
    server.fail(
        Operation::Login,
        Fault::ServerMessage {
            code: 500,
            message: format!("rejected {echoed}"),
        },
    );
    let paths = scenario_paths(dir, "echoed");
    let window = fill_and_sign_in(cx, &paths, server.base_url(), echoed, AuthMethod::Password);
    wait_for_state(cx, window, |label| label.contains("server answered")).await;

    // The last line of defence behind the events: a `{:?}` dump of the form's
    // request, through the same layer into the same file. The explicit target
    // keeps the test crate under the app's filter.
    tracing::debug!(
        target: "immich_sync",
        request = ?SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new(password),
            method: AuthMethod::Password,
        },
        "a debug dump of the sign-in request cannot leak"
    );
    tracing::debug!(
        target: "immich_sync",
        credential = ?RequestCredential::ApiKey(Secret::new(key)),
        "a debug dump of the credential cannot leak"
    );

    // Flush the queued lines before the grep.
    immich_sync::logging::shutdown();

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
        text.contains("Immich request finished"),
        "the HTTP runtime's own events reach the file: {text}"
    );
    assert!(
        text.contains("rejected the password"),
        "the outcome names the local reason: {text}"
    );
    assert!(
        text.contains("Secret(<redacted>)"),
        "the request dump redacts the secret: {text}"
    );
    assert!(
        text.contains("ApiKey(<redacted>)"),
        "the credential dump redacts the key: {text}"
    );
    assert_log_hides(&text, password, "password");
    assert_log_hides(&text, refused, "refused password");
    assert_log_hides(&text, key, "API key");
    assert_log_hides(&text, echoed, "password echoed by the server");
    for token in &tokens {
        assert_log_hides(&text, token, "session token");
    }
    drop(runtime);
}
