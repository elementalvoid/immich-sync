//! The Upload view: a path field and an Upload action (issue #5).
//!
//! This is the manual tracer bullet, not the folder sync engine (#6): the user
//! types one path and sees one outcome. The view holds the input and renders
//! the model's report; the model owns the app-scoped state.

use gpui_kit::base::Disableable as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::*;

use crate::dashboard::CardPalette;
use crate::upload::UploadModel;

/// The Upload surface.
pub struct UploadView {
    model: Entity<UploadModel>,
    path_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl UploadView {
    /// Builds the view over the app-scoped upload model.
    pub fn new(model: Entity<UploadModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("/home/you/Pictures/photo.jpg")
                .masked(false)
        });
        let mut subscriptions = Vec::with_capacity(2);
        subscriptions.push(cx.subscribe_in(
            &path_input,
            window,
            |this, _, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit(cx);
                }
            },
        ));
        // The report lives on the app-scoped model, so a result that arrives
        // while the window is hidden still reaches the reopened view.
        subscriptions.push(cx.observe(&model, |_, _, cx| cx.notify()));
        Self {
            model,
            path_input,
            _subscriptions: subscriptions,
        }
    }

    /// Hands the typed path to the model.
    fn submit(&mut self, cx: &mut Context<Self>) {
        let path = self.path_input.read(cx).value().trim().to_string();
        self.model.update(cx, |model, cx| model.upload(path, cx));
    }

    /// The outcome notice: the headline, the full message, and the accent for
    /// the result kind.
    fn result(&self, cx: &App) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let palette = CardPalette::new(theme);
        let report = self.model.read(cx).report().clone();
        let accent = match &report {
            crate::upload::UploadReport::Created { .. }
            | crate::upload::UploadReport::Duplicate { .. } => theme.success,
            crate::upload::UploadReport::Failed {
                permanent: true, ..
            } => theme.danger,
            crate::upload::UploadReport::Failed { .. } => theme.warning,
            crate::upload::UploadReport::CannotRun { .. } => theme.danger,
            crate::upload::UploadReport::Idle | crate::upload::UploadReport::Running => {
                theme.muted_foreground
            }
        };
        let message = report.message();
        div()
            .id("upload-result")
            .test_support()
            .role(Role::Alert)
            .aria_label(message.clone())
            .flex()
            .flex_col()
            .gap_0p5()
            .border_l_2()
            .border_color(accent)
            .pl_2()
            .py_1()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(report.headline()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(palette.meta)
                    .child(Text::new_inaccessible(message.into())),
            )
    }
}

impl Render for UploadView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let palette = CardPalette::new(theme);
        let background = theme.background;
        let foreground = theme.foreground;
        let muted = theme.muted_foreground;
        let focus = self.path_input.read(cx).focus_handle(cx);
        let busy = self.model.read(cx).report().is_busy();

        div()
            .id("upload-view")
            .test_support()
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
            // A short window scrolls instead of clipping the upload control
            // (#26).
            .overflow_y_scroll()
            .p_4()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Upload"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("Send one file now to check the upload pipeline end to end."),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_3()
                    .w_full()
                    .rounded(theme.radius_lg)
                    .bg(palette.surface)
                    .text_color(palette.text)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .id("upload-path")
                            .test_support()
                            .track_focus(&focus)
                            .aria_label("File path")
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
                                    .min_w_0()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child("File path"),
                                    )
                                    .child(div().text_xs().text_color(palette.meta).child(
                                        "The file to upload. The path stays on this computer.",
                                    )),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(&self.path_input).aria_label("File path")),
                            ),
                    )
                    .child(
                        div().flex().flex_row().justify_end().child(
                            Button::new("upload-button")
                                .primary()
                                .label("Upload")
                                .accessibility_label("Upload")
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                        ),
                    )
                    .child(self.result(cx)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(palette.meta)
                    .child("The same bytes again report a duplicate and transfer nothing."),
            )
    }
}
