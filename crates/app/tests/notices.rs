//! The notice every surface renders its problems from (#76).
//!
//! Three properties are read off a real dashboard window: a notice is announced
//! once, its severity reaches the reader in words and not only through the
//! accent colour, and a long quoted path wraps instead of clipping at the
//! shell's smallest window.

mod common;

use common::scroll_dashboard_into_view;
use gpui_kit::base::test_support::{ElementSnapshot, snapshots};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, ElementId, Point, TestAppContext, Window,
    WindowBounds, WindowOptions, px, size,
};
use immich_sync::dashboard::Dashboard;
use immich_sync::status::{DashboardModel, sample};

/// The shell's smallest window, where a notice is widest relative to its card.
const NARROWEST: (f32, f32) = (720., 480.);

/// A path long enough to wrap the card's notice onto several lines at 720.
const LONG_PATH: &str = "photos/2024-holiday-in-the-mountains-with-the-whole-family-and-two-\
dogs/exported-from-the-old-phone/camera-roll/DCIM/100APPLE/IMG_20240101_120000_\
HDR-portrait-export-final-v2.jpg";

/// The last error the seeded folder reports, which is what the card quotes.
const ERROR: &str = "permission_denied: the API key cannot write to this album";

fn open_dashboard(
    cx: &mut TestAppContext,
    model: DashboardModel,
    window: (f32, f32),
) -> AnyWindowHandle {
    cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(window.0), px(window.1)),
                })),
                ..Default::default()
            },
            cx,
            |_, cx| cx.new(|_| Dashboard::new(model)),
        )
        .expect("open the dashboard window")
        .0
    })
}

/// The sample data narrowed to one card that quotes `error` and prompts for
/// `review`, so a label belongs to exactly one notice.
fn one_card(error: &str, review: u32) -> DashboardModel {
    let mut model = sample();
    model.global.review_count = 0;
    model.global.last_error = None;
    for folder in &mut model.folders {
        folder.review_count = 0;
        folder.last_error = None;
    }
    model.folders[0].review_count = review;
    model.folders[0].last_error = (!error.is_empty()).then(|| error.to_string());
    model
}

/// Every observed element whose accessible name is exactly `label`.
fn named(window: &Window, label: &str) -> Vec<ElementSnapshot> {
    snapshots(window)
        .into_iter()
        .filter(|element| element.label() == Some(label))
        .collect()
}

/// Every observed element that announces exactly `text`, as a name or as the
/// value a screen reader reads out of a text node.
fn announced(window: &Window, text: &str) -> Vec<ElementSnapshot> {
    snapshots(window)
        .into_iter()
        .filter(|element| element.label() == Some(text) || element.value() == Some(text))
        .collect()
}

/// The id the card gives one of its two notices: `"error"` or `"review"`.
fn card_notice(index: usize, kind: &'static str) -> ElementId {
    (ElementId::from(("status-folder", index)), kind).into()
}

/// #76: the card's error is announced once, by the notice that quotes it.
///
/// The message is the notice's accessible name and the visible copy is
/// unreachable to assistive technology, so exactly one element carries it. The
/// harness sees the elements a view registers for observation, so this also
/// pins that the notice is observed at all: an unregistered notice is silent
/// rather than tidy.
#[gpui_kit::test]
fn the_card_error_is_announced_once(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let message = format!("Last error: {ERROR}");
    let window = open_dashboard(cx, one_card(ERROR, 0), NARROWEST);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let copies = announced(window, &message);
        assert_eq!(
            copies.len(),
            1,
            "#76: the card's error is announced once, not zero times and not twice: {:?}",
            copies
                .iter()
                .map(|element| element.path().to_vec())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            window.find(card_notice(0, "error")).label(),
            Some(message.as_str()),
            "#76: the notice keeps the message as its accessible name"
        );
    })
    .expect("read the dashboard window");
}

/// #76: severity is not carried by the accent colour alone.
///
/// Each severity states its name in a visible word, and the word is its own
/// accessibility node, so a reader who cannot tell the hues apart still learns
/// how urgent the notice is.
#[gpui_kit::test]
fn no_notice_conveys_its_severity_by_colour_alone(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let window = open_dashboard(cx, one_card(ERROR, 2), NARROWEST);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        for (kind, word) in [("error", "Error"), ("review", "Warning")] {
            let notice = card_notice(0, kind);
            scroll_dashboard_into_view(window, cx, notice.clone());
            let words: Vec<_> = snapshots(window)
                .into_iter()
                .filter(|element| element.path().contains(&notice))
                .filter(|element| element.label() == Some(word))
                .collect();
            assert_eq!(
                words.len(),
                1,
                "#76: the {kind} notice names its severity {word:?} in words: {:?}",
                named(window, word)
                    .iter()
                    .map(|element| element.path().to_vec())
                    .collect::<Vec<_>>()
            );
            assert!(
                words[0].visible(),
                "#76: the {word:?} severity word is painted, not only announced: {:?}",
                words[0]
            );
        }
        assert!(
            named(window, "Error").len() == 1,
            "#76: one error notice, one Error word"
        );
    })
    .expect("read the dashboard window");
}

/// #76: a long quoted path wraps rather than clips at the narrowest window.
///
/// The card quotes a 200-character path in its error notice and a short review
/// prompt beside it: the long one must be taller than the short one (it wrapped
/// onto more lines) and stay inside the window's width.
#[gpui_kit::test]
fn a_long_quoted_path_wraps_at_the_narrowest_window(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let window = open_dashboard(
        cx,
        one_card(&format!("permission_denied: {LONG_PATH}"), 2),
        NARROWEST,
    );

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        scroll_dashboard_into_view(window, cx, card_notice(0, "error"));
        let long = window.find(card_notice(0, "error"));
        let short = window.find(card_notice(0, "review"));
        assert!(
            long.label().is_some_and(|label| label.contains(LONG_PATH)),
            "#76: the notice announces the whole path: {:?}",
            long.label()
        );
        assert!(long.visible(), "#76: the notice is on screen: {long:?}");
        assert!(
            long.bounds().size.height > short.bounds().size.height,
            "#76: the long path wraps onto more lines than the short prompt: {:?} vs {:?}",
            long.bounds(),
            short.bounds()
        );
        let viewport = Bounds::new(Point::default(), window.viewport_size());
        assert!(
            long.bounds().size.width <= viewport.size.width,
            "#76: the wrapped notice stays inside the window: {:?} in {viewport:?}",
            long.bounds()
        );
    })
    .expect("read the dashboard window");
}
