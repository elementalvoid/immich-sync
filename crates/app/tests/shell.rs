//! Headless tests for the shell's window effects.
//!
//! `cx.quit()` and the tray icon are platform effects that a headless test
//! cannot observe, so those are checked by the probe and the GUI smoke job.
//! The state-to-tooltip mapping is pure, so it is tested here.

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{Action, AppContext as _, MenuItem, TestAppContext};
use immich_sync::shell::{self, Hide, Show};
use sync_core::connection::{Attention, ConnectionState};

fn boot(cx: &mut TestAppContext) {
    // The saved-profile restore runs on the app's Tokio worker, whose result
    // wakes the foreground executor from that thread. Parking mode is how the
    // harness permits a task to be woken off the test thread.
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        shell::init(cx);
    });
}

fn windows(cx: &TestAppContext) -> usize {
    cx.read(|cx| cx.windows().len())
}

#[gpui_kit::test]
fn show_opens_the_window_when_none_is_open(cx: &mut TestAppContext) {
    boot(cx);
    assert_eq!(windows(cx), 0);

    cx.update(|cx| cx.dispatch_action(&Show));
    cx.run_until_parked();

    assert_eq!(windows(cx), 1);
}

#[gpui_kit::test]
fn hide_closes_the_open_window(cx: &mut TestAppContext) {
    boot(cx);
    cx.update(|cx| cx.dispatch_action(&Show));
    cx.run_until_parked();
    assert_eq!(windows(cx), 1);

    cx.update(|cx| cx.dispatch_action(&Hide));
    cx.run_until_parked();

    assert_eq!(windows(cx), 0);
}

#[gpui_kit::test]
fn show_reopens_the_window_after_a_hide(cx: &mut TestAppContext) {
    boot(cx);
    cx.update(|cx| cx.dispatch_action(&Show));
    cx.update(|cx| cx.dispatch_action(&Hide));
    cx.run_until_parked();
    assert_eq!(windows(cx), 0);

    cx.update(|cx| cx.dispatch_action(&Show));
    cx.run_until_parked();

    assert_eq!(windows(cx), 1);
}

#[gpui_kit::test]
fn the_connection_model_outlives_the_window(cx: &mut TestAppContext) {
    boot(cx);
    cx.update(|cx| cx.dispatch_action(&Show));
    cx.run_until_parked();
    assert_eq!(windows(cx), 1);

    // Hide closes the window and drops its root view, which used to drop the
    // model and its tray observation with it.
    cx.update(|cx| cx.dispatch_action(&Hide));
    cx.run_until_parked();
    assert_eq!(windows(cx), 0);

    // The app-level model is still alive and can transition while hidden.
    let connection = cx.read(shell::connection);
    let attention = ConnectionState::NeedsAttention(Attention::Rejected);
    cx.update(|cx| {
        connection.update(cx, |model, cx| model.set_state(attention.clone(), cx));
    });
    cx.run_until_parked();

    // The reopened window renders from that same model, not a fresh one that
    // would have forgotten the transition. With a configured profile (a real
    // settings file, or XDG_CONFIG_HOME in CI), the reopen used to restart the
    // startup restore here and reset the state; the model's once guard keeps
    // it (R1.3).
    cx.update(|cx| cx.dispatch_action(&Show));
    cx.run_until_parked();
    let window = cx
        .read(|cx| cx.windows().first().copied())
        .expect("the window is open again");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("status-connection-global").label(),
            Some(attention.message().as_str()),
            "the reopened window shows the state the app-level model holds"
        );
    })
    .expect("update the reopened window");
}

#[gpui_kit::test]
fn showing_twice_keeps_a_single_window(cx: &mut TestAppContext) {
    boot(cx);
    cx.update(|cx| cx.dispatch_action(&Show));
    cx.update(|cx| cx.dispatch_action(&Show));
    cx.run_until_parked();

    assert_eq!(windows(cx), 1);
}

#[test]
fn tray_menu_labels_match_their_actions() {
    let items = shell::tray_menu();
    let entries: Vec<(String, &dyn Action)> = items
        .iter()
        .filter_map(|item| match item {
            MenuItem::Action { name, action, .. } => Some((name.to_string(), action.as_ref())),
            _ => None,
        })
        .collect();

    assert_eq!(entries.len(), 3, "Show, Hide, and Quit");
    assert_eq!(entries[0].0, "Show");
    assert!(entries[0].1.partial_eq(&Show));
    assert_eq!(entries[1].0, "Hide");
    assert!(entries[1].1.partial_eq(&Hide));
    assert_eq!(entries[2].0, "Quit");
    assert!(entries[2].1.partial_eq(&shell::Quit));
    assert!(
        matches!(items[2], MenuItem::Separator),
        "a separator divides the window actions from Quit"
    );
}

#[test]
fn the_tray_tooltip_shows_every_connection_state() {
    let not_configured = shell::tray_tooltip(&ConnectionState::NotConfigured);
    assert!(not_configured.contains("Immich Sync"), "{not_configured}");
    assert!(not_configured.contains("Not signed in"), "{not_configured}");

    assert!(
        shell::tray_tooltip(&ConnectionState::Connecting).contains("Signing in"),
        "the connecting state is shown"
    );
    assert!(
        shell::tray_tooltip(&ConnectionState::Connected).contains("Connected"),
        "the connected state is shown"
    );
}

#[test]
fn the_needs_attention_tooltip_carries_the_reason() {
    let state = ConnectionState::NeedsAttention(Attention::Rejected);

    let tooltip = shell::tray_tooltip(&state);
    assert!(tooltip.contains("Needs attention"), "{tooltip}");
    assert!(
        tooltip.contains("refused the session again"),
        "a second 401 must be visible without the window open: {tooltip}"
    );
}
