//! The notice component every surface reports a problem or a confirmation
//! through (#76).
//!
//! Two rules live here so no view can drift from them:
//!
//! - [`Severity`] is the only place a severity becomes a theme colour.
//! - A notice is announced once. The wrapper carries the message as its
//!   accessible name, every visible line inside it is `Text::new_inaccessible`,
//!   and the only other node inside is the severity caption, which exposes the
//!   severity word as a `Role::Label`. A [`Notice::quiet`] notice adds no node
//!   at all, because the container holding it already names the same fact.
//!
//! The severity is also stated in a visible word, so the accent colour is never
//! the only thing that says how urgent a notice is.
//!
//! A headline is visible emphasis, not content: it stays out of the accessible
//! name, so it must not carry information the message lacks.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Theme;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// How urgent a notice is. It is the whole vocabulary of the accent colour:
/// every surface that shows a problem names one of these instead of picking a
/// theme token itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Severity {
    /// Something the user must act on, or a failure that stopped work.
    Error,
    /// Something to look at, with the work still going.
    Warning,
    /// A neutral status worth stating.
    Info,
    /// A result that went the right way.
    Success,
}

impl Severity {
    /// The theme colour that marks this severity. The one severity-to-theme
    /// mapping in the app.
    fn accent(self, theme: &Theme) -> Hsla {
        match self {
            Self::Error => theme.danger,
            Self::Warning => theme.warning,
            Self::Info => theme.info,
            Self::Success => theme.success,
        }
    }

    /// The word that carries this severity without relying on the colour.
    fn label(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::Warning => "Warning",
            Self::Info => "Info",
            Self::Success => "Done",
        }
    }
}

/// One notice: a message, its severity, and a short visible summary.
///
/// `message` is both the visible sentence and the accessible name, so it is
/// also where a long quoted path belongs: it wraps instead of clipping, and it
/// stays whole for assistive technology even when the surface clamps the box.
#[derive(IntoElement)]
pub(crate) struct Notice {
    id: ElementId,
    severity: Severity,
    message: SharedString,
    headline: Option<SharedString>,
    quiet: bool,
}

impl Notice {
    /// A notice named by `id`, carrying `message` and marked by `severity`.
    pub(crate) fn new(
        id: impl Into<ElementId>,
        severity: Severity,
        message: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            severity,
            message: message.into(),
            headline: None,
            quiet: false,
        }
    }

    /// Adds the short emphasis line above the message.
    ///
    /// It stays out of the accessible name: the message already states the
    /// outcome, and the summary would be read twice.
    pub(crate) fn headline(mut self, headline: impl Into<SharedString>) -> Self {
        self.headline = Some(headline.into());
        self
    }

    /// Makes the notice decoration for a container that already names the same
    /// fact, such as a list row whose accessible name carries the class and the
    /// error.
    ///
    /// A quiet notice paints the accent edge and the severity word but adds no
    /// accessibility node at all: the container's own name is the one
    /// announcement, and a list of N rows stays N names instead of N alerts.
    /// Use it only where the container's accessible name says everything the
    /// message says.
    pub(crate) fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }
}

/// The confirmation a copy action shows (#82), or `None` before its first press.
///
/// It is the same notice on every surface that copies: visible beside the
/// control that copied, named for assistive technology, and with no button and
/// no dismiss timer, so nothing has to be dismissed and nothing disappears
/// mid-read. The words are the same everywhere; the surface only names the id.
///
/// The id carries `pressed`, the number of copies so far. GPUI's accessibility
/// guide is explicit that a node's identity is its element id, and a node that
/// arrives under a new id is a new node to assistive technology
/// (`gpui-pre/src/_accessibility.rs`), so a repeated copy is announced again
/// instead of being silent behind an unchanged element.
pub(crate) fn copy_confirmation(control: &str, pressed: u32) -> Option<Notice> {
    (pressed > 0).then(|| {
        Notice::new(
            ElementId::Name(format!("{control}-copy-confirmation-{pressed}").into()),
            Severity::Success,
            "Copied",
        )
    })
}

impl RenderOnce for Notice {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (accent, ink, muted) = (
            self.severity.accent(theme),
            theme.foreground,
            theme.muted_foreground,
        );
        let word = self.severity.label();
        // A notice with a summary line states the outcome there, so the message
        // below it is the supporting detail.
        let message_ink = if self.headline.is_some() { muted } else { ink };
        // A quiet notice names nothing, so it is no alert and has no label.
        let name = (!self.quiet).then(|| self.message.clone());

        div()
            .id(self.id.clone())
            .test_support()
            .when_some(name, |this, name| this.role(Role::Alert).aria_label(name))
            .flex()
            .flex_row()
            .items_start()
            .gap_1p5()
            .min_w_0()
            .border_l_2()
            .border_color(accent)
            .pl_2()
            .py_1()
            .child(
                div()
                    .id((self.id.clone(), "severity"))
                    .test_support()
                    .when(!self.quiet, |this| this.role(Role::Label).aria_label(word))
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(muted)
                    .child(Text::new_inaccessible(word.into())),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .when_some(self.headline, |this, headline| {
                        this.child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(ink)
                                .child(Text::new_inaccessible(headline)),
                        )
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(message_ink)
                            .child(Text::new_inaccessible(self.message)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::Severity;
    use crate::theme::{SystemTheme, apply_system_theme};
    use gpui_kit::component::Theme;
    use gpui_kit::{Hsla, TestAppContext};

    /// WCAG 2.1 relative luminance.
    fn luminance(color: Hsla) -> f32 {
        let rgb = color.to_rgb();
        let linear = |channel: f32| {
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b)
    }

    /// The WCAG 2.1 contrast ratio, 1.0 (identical) to 21.0.
    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// #76: each severity reads one semantic theme token, and both themes keep
    /// the notice's two inks readable on the card surface it paints. The two
    /// inks are the theme's `foreground` and `muted_foreground`; the surface is
    /// the card's `popover` fill.
    #[gpui_kit::test]
    fn every_severity_reads_its_own_token_and_both_themes_stay_readable(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        for system in [SystemTheme::Light, SystemTheme::Dark] {
            cx.update(|cx| apply_system_theme(system, cx));
            cx.read(|cx| {
                let theme = Theme::global(cx);
                for (severity, token) in [
                    (Severity::Error, theme.danger),
                    (Severity::Warning, theme.warning),
                    (Severity::Info, theme.info),
                    (Severity::Success, theme.success),
                ] {
                    assert_eq!(
                        severity.accent(theme),
                        token,
                        "{system:?}: {severity:?} reads its own token"
                    );
                }
                for (what, ink) in [
                    ("the message", theme.foreground),
                    ("the severity word", theme.muted_foreground),
                ] {
                    let ratio = contrast(ink, theme.popover);
                    assert!(
                        ratio >= 4.5,
                        "{system:?}: {what} on the card surface is {ratio:.2}:1, below 4.5:1"
                    );
                }
            });
        }
    }
}
