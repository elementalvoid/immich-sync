//! The failed-items view (SPEC R6.5): the parked items, why they failed, and a
//! manual retry for each one.
//!
//! The rows come from [`crate::failed`], so the view does no formatting. Every
//! retry control is a keyboard-reachable tab stop with its own accessible name
//! (N5).

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::dashboard::CardPalette;
use crate::engine::EngineModel;
use crate::failed::{FailedRow, rows, summary};

/// The failed-items surface.
pub struct FailedView {
    engine: Entity<EngineModel>,
    _subscription: Subscription,
}

impl FailedView {
    /// Builds the view over the app-scoped engine model.
    ///
    /// The parked items are read off the UI thread (N2), so the first frame
    /// asks for a fresh read instead of showing a stale cache.
    pub fn new(engine: Entity<EngineModel>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        engine.update(cx, |engine, cx| engine.refresh_failed(cx));
        let subscription = cx.observe(&engine, |_, _, cx| cx.notify());
        Self {
            engine,
            _subscription: subscription,
        }
    }

    /// Re-attempts one parked item by hand (R6.5).
    fn retry(&mut self, path: String, cx: &mut Context<Self>) {
        self.engine
            .update(cx, |engine, cx| engine.retry_failed(path, cx));
    }
}

impl Render for FailedView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The tokens are copied, not borrowed from `cx`: building the rows
        // needs the context mutably for their click listeners.
        let palette = CardPalette::new(cx.theme());
        let radius = cx.theme().radius_lg;
        let border = cx.theme().border;
        let danger = cx.theme().danger;
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let items = self.engine.read(cx).failed_items().to_vec();
        let rows = rows(&items);
        let summary = summary(&items);
        let problem = self.engine.read(cx).problem().map(str::to_string);

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .p_6()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Failed items"),
                    )
                    .child(div().text_sm().text_color(muted).child(
                        "Files the client stopped trying. Retry one after you fix the cause.",
                    )),
            )
            .when_some(problem, |this, problem| {
                this.child(div().text_sm().text_color(danger).child(problem))
            })
            .child(
                div()
                    .id("failed-summary")
                    .test_support()
                    .role(Role::Status)
                    .aria_label(summary.clone())
                    .text_sm()
                    .text_color(muted)
                    .child(summary),
            )
            .child(
                div()
                    .id("failed-list")
                    .test_support()
                    .role(Role::List)
                    .aria_label("Failed items")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .when(rows.is_empty(), |this| {
                        this.child(empty_card(&palette, radius, border))
                    })
                    .children(rows.iter().enumerate().map(|(index, row)| {
                        row_card(row, index, &palette, radius, border, danger, cx)
                    })),
            )
    }
}

/// One parked item: the file, its class, the last error, and Retry.
fn row_card(
    row: &FailedRow,
    index: usize,
    palette: &CardPalette,
    radius: Pixels,
    border: Hsla,
    danger: Hsla,
    cx: &mut Context<FailedView>,
) -> impl IntoElement + use<> {
    let path = row.path.clone();
    div()
        .id(format!("failed-row-{index}"))
        .test_support()
        .role(Role::ListItem)
        .aria_label(row.accessible_name.clone())
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .p_3()
        .w_full()
        .rounded(radius)
        .bg(palette.surface)
        .border_1()
        .border_color(border)
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .min_w_0()
                .child(
                    div()
                        .truncate()
                        .text_base()
                        .font_weight(FontWeight::MEDIUM)
                        .child(row.name.clone()),
                )
                .child(
                    div()
                        .truncate()
                        .text_xs()
                        .text_color(palette.meta)
                        .child(row.path.clone()),
                )
                .child(
                    div()
                        .border_l_2()
                        .border_color(danger)
                        .pl_2()
                        .text_xs()
                        .text_color(palette.text)
                        .child(format!("{} — {}", row.class, row.error)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.meta)
                        .child(row.attempts_line()),
                ),
        )
        .child(
            Button::new(format!("failed-retry-{index}"))
                .secondary()
                .label("Retry")
                .accessibility_label(row.retry_label.clone())
                .on_click(cx.listener(move |this, _, _, cx| this.retry(path.clone(), cx))),
        )
}

/// The empty state: the view is a normal place to be, not a broken one.
fn empty_card(palette: &CardPalette, radius: Pixels, border: Hsla) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_4()
        .w_full()
        .rounded(radius)
        .bg(palette.surface)
        .border_1()
        .border_color(border)
        .child(
            div()
                .text_base()
                .font_weight(FontWeight::MEDIUM)
                .child("Nothing has failed"),
        )
        .child(
            div()
                .text_xs()
                .text_color(palette.meta)
                .child("A file the client gives up on appears here, with the reason."),
        )
}
