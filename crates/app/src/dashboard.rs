//! The dashboard view (SPEC R8.1): a global status card and one card per
//! source folder.
//!
//! Keyboard navigation stays out until the form controls land in #3.

use crate::status::{ActivityState, DashboardModel, StatusCard};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Colorize as _;
use gpui_kit::component::Theme;
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
            .child(status_card(
                "status-global".into(),
                "status-state-global".into(),
                &self.model.global,
                cx,
            ))
            .child(div().text_sm().text_color(muted).child("Source folders"))
            .child(
                div().flex().flex_row().flex_wrap().gap_4().children(
                    self.model
                        .folders
                        .iter()
                        .enumerate()
                        .map(|(index, folder)| {
                            div().w(rems(17.5)).flex().child(status_card(
                                ("status-folder", index).into(),
                                ("status-state-folder", index).into(),
                                folder,
                                cx,
                            ))
                        }),
                ),
            )
    }
}

/// The theme tokens one dashboard card paints with.
///
/// The color choice lives here, so the WCAG contrast test measures the tokens
/// the view actually paints rather than a copy of them.
#[derive(Clone, Copy, Debug)]
pub struct CardPalette {
    /// The card fill.
    pub surface: Hsla,
    /// The card title and notice text.
    pub text: Hsla,
    /// The path, percentage, and counts text.
    pub meta: Hsla,
    /// The state pill fill.
    pub pill_fill: Hsla,
    /// The state pill text.
    pub pill_text: Hsla,
}

impl CardPalette {
    /// Reads the palette from the active theme.
    #[must_use]
    pub fn new(theme: &Theme) -> Self {
        Self {
            surface: theme.popover,
            text: theme.foreground,
            meta: theme.muted_foreground,
            pill_fill: theme.secondary,
            pill_text: theme.secondary_foreground,
        }
    }
}

/// How far a state hue is darkened to read as a 1px border on a card.
///
/// The kit's light accents are too bright for a border: the light `info` and
/// `warning` tokens sit below the WCAG non-text contrast floor of 3:1 on the
/// card surface. This factor clears it in both themes.
const STATE_BORDER_DARKEN: f32 = 0.25;

/// The border color that carries one activity state's hue.
///
/// The pill label stays in the theme's paired pill ink, because the light
/// accents are too bright to read as text on a light card.
#[must_use]
pub fn state_border(state: ActivityState, theme: &Theme) -> Hsla {
    let accent = match state {
        ActivityState::Idle => theme.muted_foreground,
        ActivityState::Syncing => theme.info,
        ActivityState::Paused => theme.warning,
        ActivityState::NeedsAttention => theme.danger,
    };
    accent.darken(STATE_BORDER_DARKEN)
}

/// One status card. The text comes from [`StatusCard::content`], so the view
/// does no formatting.
fn status_card(
    id: ElementId,
    state_id: ElementId,
    card: &StatusCard,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let palette = CardPalette::new(theme);
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
        .bg(palette.surface)
        .text_color(palette.text)
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
                                .truncate()
                                .text_base()
                                .font_weight(FontWeight::MEDIUM)
                                .child(content.title),
                        )
                        .when_some(content.path, |this, path| {
                            this.child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(palette.meta)
                                    .child(path),
                            )
                        }),
                )
                .child(
                    div()
                        .id(state_id)
                        .test_support()
                        .aria_label(content.state.clone())
                        .child(state_tag(card.activity, content.state, &palette, theme)),
                ),
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
                        .text_color(palette.meta)
                        .child(content.progress_percent),
                ),
        )
        .child(
            div()
                .text_sm()
                .text_color(palette.meta)
                .child(content.counts),
        );

    if let Some(review) = content.review {
        root = root.child(notice(review, theme.warning, &palette));
    }
    if let Some(error) = content.error {
        root = root.child(notice(
            format!("Last error: {error}"),
            theme.danger,
            &palette,
        ));
    }
    root
}

/// The activity-state pill on a card.
///
/// The wrapper owns the accessible name and the visible copy is inaccessible
/// to assistive technology, so the state is announced once, not twice.
fn state_tag(state: ActivityState, label: String, palette: &CardPalette, theme: &Theme) -> Tag {
    Tag::secondary()
        .bg(palette.pill_fill)
        .text_color(palette.pill_text)
        .border_color(state_border(state, theme))
        .child(Text::new_inaccessible(label.into()))
}

/// A card notice: a colored edge marks the kind, and the text keeps the
/// card's readable ink.
fn notice(text: String, accent: Hsla, palette: &CardPalette) -> impl IntoElement {
    div()
        .border_l_2()
        .border_color(accent)
        .pl_2()
        .text_xs()
        .text_color(palette.text)
        .child(text)
}
