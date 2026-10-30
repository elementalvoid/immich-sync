//! The failed-items view (SPEC R6.5): the parked items, why they failed, and a
//! manual retry for each one.
//!
//! The rows come from [`crate::failed`], so the view does no formatting. Every
//! retry control is a keyboard-reachable tab stop with its own accessible name
//! (N5). The folder filter narrows the list, its summary, and its empty state
//! (#41).

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sync_core::paths::PathCase;

use crate::dashboard::CardPalette;
use crate::empty_state::EmptyState;
use crate::engine::EngineModel;
use crate::failed::{FailedRow, rows, summary};
use crate::folder_filter::FolderFilter;
use crate::folder_filter_view::folder_filter_bar;
use crate::notice::{Notice, Severity};

/// The failed-items surface.
pub struct FailedView {
    engine: Entity<EngineModel>,
    /// The source folder the list shows (#41).
    filter: Entity<FolderFilter>,
    _subscriptions: Vec<Subscription>,
}

impl FailedView {
    /// Builds the view over the app-scoped engine model.
    ///
    /// The parked items are read off the UI thread (N2), so the first frame
    /// asks for a fresh read instead of showing a stale cache.
    pub fn new(engine: Entity<EngineModel>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        engine.update(cx, |engine, cx| engine.refresh_failed(cx));
        let filter = cx.new(|_| FolderFilter::new());
        let subscriptions = vec![
            cx.observe(&engine, |_, _, cx| cx.notify()),
            cx.observe(&filter, |_, _, cx| cx.notify()),
        ];
        Self {
            engine,
            filter,
            _subscriptions: subscriptions,
        }
    }

    /// Shows one source folder only (#41).
    ///
    /// This is the seam the folder detail calls to open this view on a folder
    /// (#43); the filter's clear control undoes it.
    pub fn select_folder(&mut self, path: String, cx: &mut Context<Self>) {
        // The view observes the filter, so the selection repaints it.
        self.filter.update(cx, |filter, cx| {
            filter.select(path);
            cx.notify();
        });
    }

    /// Re-attempts one parked item by hand (R6.5).
    fn retry(&mut self, path: String, cx: &mut Context<Self>) {
        self.engine
            .update(cx, |engine, cx| engine.retry_failed(path, cx));
    }
}

impl Render for FailedView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The tokens are copied, not borrowed from `cx`: building the rows
        // needs the context mutably for their click listeners.
        let palette = CardPalette::new(cx.theme());
        let radius = cx.theme().radius_lg;
        let border = cx.theme().border;
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let folders = self.engine.read(cx).folders(cx);
        let mut items = self.engine.read(cx).failed_items().to_vec();
        let case = PathCase::for_platform();
        let filter = self.filter.read(cx);
        items.retain(|item| filter.keeps(&item.path, &folders, case));
        let rows = rows(&items);
        let problem = self.engine.read(cx).problem().map(str::to_string);
        // R6.5, #41: the empty state names the folder the filter shows, and only
        // the list it stands in for (#77).
        let empty = self
            .filter
            .read(cx)
            .empty_heading("No failed items", "Nothing has failed");
        // #41 review: an empty filtered list must not be summarised as if the
        // whole client were empty, so the summary only states a count. An empty
        // list is stated once, by the empty state below it (#76's "announced
        // once" rule, #77).
        let summary = (!rows.is_empty()).then(|| summary(&items));

        div()
            .id("failed-view")
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
                            .child("Failed items"),
                    )
                    .child(div().text_sm().text_color(muted).child(
                        "Files the client stopped trying. Retry one after you fix the cause.",
                    )),
            )
            .when_some(problem, |this, problem| {
                this.child(Notice::new("failed-problem", Severity::Error, problem))
            })
            .child(folder_filter_bar(
                "failed",
                &self.filter,
                &folders,
                window,
                cx,
            ))
            .when_some(summary, |this, summary| {
                this.child(
                    div()
                        .id("failed-summary")
                        .test_support()
                        .role(Role::Status)
                        .aria_label(summary.clone())
                        .text_sm()
                        .text_color(muted)
                        .child(summary),
                )
            })
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
                        this.child(EmptyState::new(
                            "failed-empty",
                            empty,
                            "A file the client gives up on appears here, with the reason.",
                        ))
                    })
                    .children(
                        rows.iter()
                            .enumerate()
                            .map(|(index, row)| row_card(row, index, &palette, radius, border, cx)),
                    ),
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
                // The row's accessible name already states the class and the
                // error, so the strip names nothing itself (#76).
                .child(
                    Notice::new(
                        format!("failed-row-notice-{index}"),
                        Severity::Error,
                        format!("{} — {}", row.class, row.error),
                    )
                    .quiet(),
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
