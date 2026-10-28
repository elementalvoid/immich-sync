//! Helpers shared by the app's integration tests.
//!
//! Cargo compiles this module into every test binary that declares
//! `mod common;`, so it only uses the dev-dependencies those binaries
//! already enable, and each binary uses a part of it. The per-item
//! `dead_code` allows say which parts that binary left out.

use gpui_kit::{App, ElementId, Window};
use mock_immich::MockImmich;

/// Starts the offline Immich double on its own runtime: a GPUI test runs on
/// the GPUI executor, which is not a Tokio runtime.
#[allow(dead_code)]
pub fn mock_server() -> (tokio::runtime::Runtime, MockImmich) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("start the mock runtime");
    let server = runtime.block_on(MockImmich::start());
    (runtime, server)
}

/// Scrolls the dashboard until the element with `id` is on screen.
///
/// #56: the global card carries the reconciliation scan line, so on a short
/// window a card's footer sits below the fold; a control is clicked only after
/// it is really visible.
#[allow(dead_code)]
pub fn scroll_dashboard_into_view(window: &mut Window, cx: &mut App, id: impl Into<ElementId>) {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{ScrollDelta, point, px};

    let id = id.into();
    window.render_frame(cx);
    for _ in 0..60 {
        if window
            .try_find(id.clone())
            .is_some_and(|target| target.visible())
        {
            return;
        }
        window.scroll(
            "dashboard-scroll",
            ScrollDelta::Pixels(point(px(0.), px(-120.))),
            cx,
        );
    }
    panic!("{id:?} never became visible on the dashboard");
}

/// Waits for an alert dialog's entrance to settle before pressing it.
///
/// The surface slides in for 250 ms, and a press is dispatched from the bounds
/// read a moment earlier: mid-flight the control has moved on, so under load the
/// press lands beside it and the button looks dead. The entrance runs on the
/// wall clock, which the test executor's `advance_clock` does not move, hence
/// the sleep. A person aims at the control they can see, so this is the
/// harness's problem, not the dialog's (#73).
#[allow(dead_code)]
pub fn settle_alert(window: &mut Window, cx: &mut App) {
    use gpui_kit::test::TestWindowExt as _;

    std::thread::sleep(std::time::Duration::from_millis(350));
    window.render_frame(cx);
}

/// The select-all chord the GPUI text input binds on this platform.
///
/// `ctrl-a` is select-all on Linux and Windows, but macOS binds `ctrl-a` to
/// "move to the start of the line" and select-all to `cmd-a` (gpui-base's
/// `input/base/state.rs`), so a test that hardcodes `ctrl-a` types over the
/// start of the old value on macOS instead of replacing it.
#[allow(dead_code)]
pub fn select_all_chord() -> &'static str {
    if cfg!(target_os = "macos") {
        "cmd-a"
    } else {
        "ctrl-a"
    }
}
