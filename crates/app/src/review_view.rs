//! The deletion review view (SPEC R5.3, R5.5): one row per missing file, with
//! keep on server or move to trash.
//!
//! The rows come from [`crate::review`], so the view does no formatting. Every
//! control is a keyboard-reachable tab stop with its own accessible name (N5),
//! and the surface follows the theme tokens like the failed-items view. The
//! folder filter narrows the list, its summary, and its empty state (#41).

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sync_core::paths::PathCase;

use crate::dashboard::CardPalette;
use crate::empty_state::EmptyState;
use crate::engine::EngineModel;
use crate::folder_filter::FolderFilter;
use crate::folder_filter_view::folder_filter_bar;
use crate::notice::{Notice, Severity};
use crate::review::{ReviewRow, rows, summary};

/// The deletion review surface.
pub struct ReviewView {
    engine: Entity<EngineModel>,
    /// The source folder the list shows (#41).
    filter: Entity<FolderFilter>,
    _subscriptions: Vec<Subscription>,
}

impl ReviewView {
    /// Builds the view over the app-scoped engine model.
    ///
    /// The missing files are read off the UI thread (N2), so the first frame
    /// asks for a fresh read instead of showing a stale cache.
    pub fn new(engine: Entity<EngineModel>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        engine.update(cx, |engine, cx| engine.refresh_review(cx));
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
        let mut items = self.engine.read(cx).review_items().to_vec();
        let case = PathCase::for_platform();
        let filter = self.filter.read(cx);
        items.retain(|item| filter.keeps(&item.path, &folders, case));
        let rows = rows(&items);
        let problem = self.engine.read(cx).review_problem().map(str::to_string);
        // R5.3, #41: the empty state names the folder the filter shows, and only
        // the list it stands in for (#77).
        let empty = self
            .filter
            .read(cx)
            .empty_heading("No missing files", "Every tracked file is present");
        // #41 review: an empty filtered list must not be summarised as if the
        // whole client were empty, so the summary only states a count. An empty
        // list is stated once, by the empty state below it (#76's "announced
        // once" rule, #77).
        let summary = (!rows.is_empty()).then(|| summary(&items));

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
                this.child(Notice::new("review-problem", Severity::Error, problem))
            })
            .child(folder_filter_bar(
                "review",
                &self.filter,
                &folders,
                window,
                cx,
            ))
            .when_some(summary, |this, summary| {
                this.child(
                    div()
                        .id("review-summary")
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
                    .id("review-list")
                    .test_support()
                    .role(Role::List)
                    .aria_label("Deletion review")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .when(rows.is_empty(), |this| {
                        this.child(EmptyState::new(
                            "review-empty",
                            empty,
                            "A file that disappears from a source folder appears here, and the \
                             server copy waits for your choice.",
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

/// One missing file: the file, its path, and the two decisions.
fn row_card(
    row: &ReviewRow,
    index: usize,
    palette: &CardPalette,
    radius: Pixels,
    border: Hsla,
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
                    // The row's accessible name states the row and its
                    // reassurance, so the strip names nothing itself (#76).
                    Notice::new(
                        format!("review-row-notice-{index}"),
                        Severity::Warning,
                        "Missing on this computer · the server copy is untouched",
                    )
                    .quiet(),
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
