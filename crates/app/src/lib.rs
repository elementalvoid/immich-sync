//! Immich Sync shell: a GPUI-Kit window, a tray icon, and a single instance.

pub mod autostart;
pub mod connection;
pub mod credentials;
pub mod dashboard;
pub mod engine;
pub mod failed;
pub mod failed_view;
pub mod instance;
pub mod logging;
pub mod logs_view;
pub mod paths;
pub mod settings;
pub mod settings_view;
pub mod shell;
pub mod status;
pub mod store;
pub mod theme;
pub mod upload;
pub mod upload_view;

use anyhow::Context as _;
use async_channel::Receiver;
use gpui_kit::{App, QuitMode};

use crate::logging::{LogPolicy, LogSink};

/// Runs the client, and returns when the primary instance quits.
///
/// A second launch asks the running instance to show its window and returns
/// without starting a second client.
///
/// # Errors
///
/// Returns an error when the single-instance socket cannot be claimed.
pub fn run() -> anyhow::Result<()> {
    // A resolution failure keeps the window usable on the fallback paths, the
    // same way the settings view does, and is recorded once the log is up.
    let (paths, resolution_problem) = match paths::AppPaths::resolve() {
        Ok(paths) => (paths, None),
        Err(error) => (paths::AppPaths::fallback(), Some(format!("{error:#}"))),
    };
    start_client(&paths, resolution_problem.as_deref());
    let result = run_client();
    // Flush what is still queued before the process ends.
    logging::shutdown();
    result
}

/// Starts the rotating log and records how this process booted (R10.3).
///
/// The log file lives in the state database's directory, so the resolved paths
/// decide where it goes before the window exists.
fn start_client(paths: &paths::AppPaths, resolution_problem: Option<&str>) {
    // The persisted level is read before the subscriber exists, so this open
    // cannot log its own failure. The settings view opens the file again once
    // the logger is up and logs the problem then.
    let store = settings::SettingsStore::open(paths.config_file.clone());
    match LogSink::start(
        &paths.log_directory(),
        LogPolicy::default(),
        store.settings().log_level,
    ) {
        Ok(sink) => {
            if let Err(error) = sink.install() {
                // Nothing can log this: the subscriber was refused.
                eprintln!("immich-sync: could not install the log subscriber: {error:#}");
            }
        }
        // Nothing can log this either: the log file is what failed.
        Err(error) => eprintln!("immich-sync: could not start the log file: {error:#}"),
    }

    logging::record_start(paths, resolution_problem);
}

/// Claims the single instance and runs the window and tray loop.
///
/// # Errors
///
/// Returns an error when the single-instance socket cannot be claimed.
fn run_client() -> anyhow::Result<()> {
    let Some(listener) =
        instance::claim(&instance::socket_name()).context("claim the single-instance socket")?
    else {
        tracing::info!("this launch is a handoff: the running client was asked to show its window");
        return Ok(());
    };

    let (requests, incoming) = async_channel::unbounded();
    std::thread::spawn(move || instance::forward(listener, requests));

    gpui_kit::application().run(move |cx| boot(cx, incoming));
    Ok(())
}

fn boot(cx: &mut App, incoming: Receiver<instance::Request>) {
    gpui_kit::init(cx);
    shell::init(cx);

    // A second launch reaches the shell through GPUI's executor, the same way
    // gpui-tray dispatches its native events.
    cx.spawn(async move |cx| {
        while incoming.recv().await.is_ok() {
            tracing::info!("a second launch handed off to this instance; showing the window");
            cx.update(|cx| cx.dispatch_action(&shell::Show));
        }
    })
    .detach();

    // The tray is installed before the window so the shell's first connection
    // state lands on it (R1.4).
    if let Err(error) = shell::install_tray(cx) {
        // With no tray there is no Quit menu item, so the app must not keep the
        // tray's explicit quit mode: it would run with no way to quit.
        cx.set_quit_mode(QuitMode::default());
        tracing::error!(
            error = %format!("{error:#}"),
            "could not create the tray icon, so the client runs without a tray"
        );
    }
    if let Err(error) = shell::open_main_window(cx) {
        tracing::error!(
            error = %format!("{error:#}"),
            "could not open the main window"
        );
    }
}
