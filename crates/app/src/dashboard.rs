//! The dashboard view (SPEC R8.1): a global status card and one card per
//! source folder.

use crate::status::{ActivityState, DashboardModel, StatusCard};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::tag::Tag;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The dashboard view. It holds the view model and nothing else.
pub struct Dashboard {
    model: DashboardModel,
    paused: bool,
}

impl Dashboard {
    /// Builds the dashboard from a view model.
    #[must_use]
    pub fn new(model: DashboardModel) -> Self {
        Self {
            model,
            paused: false,
        }
    }

    fn sync_now(&mut self, cx: &mut Context<Self>) {
        self.paused = false;
        self.model.global.activity = ActivityState::Syncing;
        cx.notify();
    }

    fn toggle_paused(&mut self, cx: &mut Context<Self>) {
        self.paused = !self.paused;
        self.model.global.activity = if self.paused {
            ActivityState::Paused
        } else {
            ActivityState::Syncing
        };
        cx.notify();
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let (pause_label, pause_name) = if self.paused {
            ("Resume", "Resume syncing")
        } else {
            ("Pause", "Pause syncing")
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
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
                            .child("Immich Sync"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("Upload status for every source folder"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        Button::new("dashboard-sync-now")
                            .primary()
                            .label("Sync now")
                            .accessibility_label("Sync now")
                            .on_click(cx.listener(|this, _, _, cx| this.sync_now(cx))),
                    )
                    .child(
                        Button::new("dashboard-pause")
                            .secondary()
                            .label(pause_label)
                            .accessibility_label(pause_name)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_paused(cx))),
                    ),
            )
    }
}

impl Render for Dashboard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let header = self.header(cx);

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .p_6()
            .gap_4()
            .child(header)
            .child(status_card("status-global".into(), &self.model.global, cx))
            .child(div().text_sm().text_color(muted).child("Source folders"))
            .child(
                div().flex().flex_row().flex_wrap().gap_4().children(
                    self.model
                        .folders
                        .iter()
                        .enumerate()
                        .map(|(index, folder)| {
                            div().w(rems(17.5)).child(status_card(
                                ("status-folder", index).into(),
                                folder,
                                cx,
                            ))
                        }),
                ),
            )
    }
}

/// One status card. The text comes from [`StatusCard::content`], so the view
/// does no formatting.
fn status_card(id: ElementId, card: &StatusCard, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let content = card.content();
    let title = content.title.clone();
    let mut root = div()
        .id(id.clone())
        .test_support()
        .flex()
        .flex_col()
        .gap_3()
        .p_4()
        .w_full()
        .rounded(theme.radius_lg)
        .bg(theme.secondary)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .min_w_0()
                        .child(
                            div()
                                .text_base()
                                .font_weight(FontWeight::MEDIUM)
                                .child(content.title),
                        )
                        .when_some(content.path, |this, path| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(path),
                            )
                        }),
                )
                .child(state_tag(card.activity)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    Progress::new((id, "progress"))
                        .flex_1()
                        .value(content.progress * 100.0)
                        .accessibility_label(format!("{title} progress")),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(content.progress_percent),
                ),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(content.counts),
        );

    if let Some(review) = content.review {
        root = root.child(div().text_xs().text_color(theme.warning).child(review));
    }
    if let Some(error) = content.error {
        root = root.child(
            div()
                .text_xs()
                .text_color(theme.danger)
                .child(format!("Last error: {error}")),
        );
    }
    root
}

/// The activity-state pill on a card.
fn state_tag(state: ActivityState) -> Tag {
    let tag = match state {
        ActivityState::Idle => Tag::secondary(),
        ActivityState::Syncing => Tag::info(),
        ActivityState::Paused => Tag::warning(),
        ActivityState::NeedsAttention => Tag::danger(),
    };
    tag.outline().child(state.label())
}
