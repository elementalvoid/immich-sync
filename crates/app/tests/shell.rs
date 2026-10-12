//! Headless tests for the shell's window effects.
//!
//! `cx.quit()` and the tray icon are platform effects that a headless test
//! cannot observe, so those are checked by the probe and the GUI smoke job.

use gpui_kit::{Action, MenuItem, TestAppContext};
use immich_sync::shell::{self, Hide, Show};

fn boot(cx: &mut TestAppContext) {
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
