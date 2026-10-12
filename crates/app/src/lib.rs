//! Immich Sync shell: a GPUI-Kit window, a tray icon, and a single instance.

pub mod instance;
pub mod shell;

use anyhow::Context as _;
use async_channel::Receiver;
use gpui_kit::{App, QuitMode};

/// Runs the client, and returns when the primary instance quits.
///
/// A second launch asks the running instance to show its window and returns
/// without starting a second client.
///
/// # Errors
///
/// Returns an error when the single-instance socket cannot be claimed.
pub fn run() -> anyhow::Result<()> {
    let Some(listener) =
        instance::claim(&instance::socket_name()).context("claim the single-instance socket")?
    else {
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
            cx.update(|cx| cx.dispatch_action(&shell::Show));
        }
    })
    .detach();

    if let Err(error) = shell::open_main_window(cx) {
        eprintln!("immich-sync: could not open the main window: {error:#}");
    }
    if let Err(error) = shell::install_tray(cx) {
        // With no tray there is no Quit menu item, so the app must not keep the
        // tray's explicit quit mode: it would run with no way to quit.
        cx.set_quit_mode(QuitMode::default());
        eprintln!("immich-sync: could not create the tray icon: {error}");
    }
}
