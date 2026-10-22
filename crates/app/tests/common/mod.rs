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
