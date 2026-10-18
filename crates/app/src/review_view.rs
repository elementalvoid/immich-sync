//! The deletion review view (SPEC R5.3, R5.5): one row per missing file, with
//! keep on server or move to trash.
//!
//! The rows come from [`crate::review`], so the view does no formatting. Every
//! control is a keyboard-reachable tab stop with its own accessible name (N5),
//! and the surface follows the theme tokens like the failed-items view.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::dashboard::CardPalette;
use crate::engine::EngineModel;
use crate::review::{ReviewRow, rows, summary};

/// The deletion review surface.
pub struct ReviewView {
    engine: Entity<EngineModel>,
    _subscription: Subscription,
}

impl ReviewView {
    /// Builds the view over the app-scoped engine model.
    ///
    /// The missing files are read off the UI thread (N2), so the first frame
    /// asks for a fresh read instead of showing a stale cache.
    pub fn new(engine: Entity<EngineModel>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        engine.update(cx, |engine, cx| engine.refresh_review(cx));
        let subscription = cx.observe(&engine, |_, _, cx| cx.notify());
        Self {
            engine,
            _subscription: subscription,
        }
    }

    /// Leaves the server asset alone and clears the row (R5.3).
    fn keep(&mut self, path: String, cx: &mut Context<Self>) {
        self.engine
            .update(cx, |engine, cx| engine.keep_review(path, cx));
    }

    /// Moves the server asset to the trash and clears the row (R5.3).
    fn trash(&mut self, path: String, cx: &mut Context<Self>) {
        self.engine
            .update(cx, |engine, cx| engine.trash_review(path, cx));
    }
}

impl Render for ReviewView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The tokens are copied, not borrowed from `cx`: building the rows
        // needs the context mutably for their click listeners.
        let palette = CardPalette::new(cx.theme());
        let radius = cx.theme().radius_lg;
        let border = cx.theme().border;
        let danger = cx.theme().danger;
        let warning = cx.theme().warning;
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let items = self.engine.read(cx).review_items().to_vec();
        let rows = rows(&items);
        let summary = summary(&items);
        let problem = self.engine.read(cx).review_problem().map(str::to_string);

        div()
            .id("review-view")
            .test_support()
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
            // A short window scrolls instead of clipping the last row (#26).
            .overflow_y_scroll()
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
                            .child("Deletion review"),
                    )
                    .child(div().text_sm().text_color(muted).child(
                        "Files that disappeared from your source folders. Keep the server copy, \
                         or move it to the trash. Nothing changes until you choose.",
                    )),
            )
            .when_some(problem, |this, problem| {
                this.child(
                    div()
                        .id("review-problem")
                        .test_support()
                        .role(Role::Status)
                        .aria_label(problem.clone())
                        .text_sm()
                        .text_color(danger)
                        .child(problem),
                )
            })
            .child(
                div()
                    .id("review-summary")
                    .test_support()
                    .role(Role::Status)
                    .aria_label(summary.clone())
                    .text_sm()
                    .text_color(muted)
                    .child(summary),
            )
            .child(
                div()
                    .id("review-list")
                    .test_support()
                    .role(Role::List)
                    .aria_label("Deletion review")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .when(rows.is_empty(), |this| {
                        this.child(empty_card(&palette, radius, border))
                    })
                    .children(rows.iter().enumerate().map(|(index, row)| {
                        row_card(row, index, &palette, radius, border, warning, cx)
                    })),
            )
    }
}

/// One missing file: the file, its path, and the two decisions.
fn row_card(
    row: &ReviewRow,
    index: usize,
    palette: &CardPalette,
    radius: Pixels,
    border: Hsla,
    warning: Hsla,
    cx: &mut Context<ReviewView>,
) -> impl IntoElement + use<> {
    let keep_path = row.path.clone();
    let trash_path = row.path.clone();
    div()
        .id(format!("review-row-{index}"))
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
                        .border_color(warning)
                        .pl_2()
                        .text_xs()
                        .text_color(palette.text)
                        .child("Missing on this computer · the server copy is untouched"),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    Button::new(format!("review-keep-{index}"))
                        .secondary()
                        .label("Keep")
                        .accessibility_label(row.keep_label.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.keep(keep_path.clone(), cx);
                        })),
                )
                .child(
                    Button::new(format!("review-trash-{index}"))
                        .danger()
                        .label("Trash")
                        .accessibility_label(row.trash_label.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.trash(trash_path.clone(), cx);
                        })),
                ),
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
                .child("Every tracked file is present"),
        )
        .child(
            div()
                .text_xs()
                .text_color(palette.meta)
                .child("A file that disappears from a source folder appears here, and the server copy waits for your choice."),
        )
}
