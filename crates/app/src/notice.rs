//! The notice component every surface reports a problem or a confirmation
//! through (#76).
//!
//! Three rules live here so no view can drift from them:
//!
//! - [`Severity`] is the only place a severity becomes a theme colour, and the
//!   only place it becomes an accessibility role: a result that went the right
//!   way is a `Role::Status`, everything else is a `Role::Alert`.
//! - A notice is announced once. The wrapper carries the message as its
//!   accessible name, every visible line inside it is `Text::new_inaccessible`,
//!   and the only other node inside is the severity caption, which exposes the
//!   severity word as a `Role::Label`. A [`Notice::quiet`] notice adds no node
//!   at all, because the container holding it already names the same fact.
//! - The wrapper is a polite live region ([`polite_live_region`]), and the three
//!   adapters read it differently: Windows announces the node when it appears
//!   and when its name changes, AT-SPI announces a changed name on a node that
//!   stays (its `node_added` carries no `Announcement`, so a mount is silent
//!   there), and macOS announces only a node carrying an accessibility value.
//!   A plain notice sets no value, so it is silent on macOS; that is what
//!   [`Notice::announcement`] is for, and it sets the name and the value to the
//!   same words so a repeat is heard on every platform.
//!
//! The severity is also stated in a visible word, so the accent colour is never
//! the only thing that says how urgent a notice is.
//!
//! A headline is visible emphasis, not content: it stays out of the accessible
//! name, so it must not carry information the message lacks.
//!
//! The sibling lesson, and the reason [`crate::a11y::named`] exists: gpui builds
//! a node only for an element that declares a role, so an `aria_label` or an
//! `aria_description` on an element without `.role()` is stored and read by
//! nobody. The snapshots cannot see the difference — they read the declaration —
//! so no test catches it; a name or a description is always written with the
//! role it needs.

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

    /// The role the notice reports as.
    ///
    /// A result that went the right way is a status, not an alert: `Alert` is
    /// the loud half of the assertive/polite split, and the kit uses
    /// `Role::Status` for its own status text (`gpui-component`'s
    /// `marker.rs`).
    fn role(self) -> Role {
        match self {
            Self::Success => Role::Status,
            _ => Role::Alert,
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
    announcement: Option<SharedString>,
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
            announcement: None,
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

    /// Names the notice for assistive technology with `announcement` instead of
    /// the visible message, and repeats it as the node's value.
    ///
    /// The value carries it because the platform adapters gate on different
    /// fields when they decide to announce: Windows and AT-SPI need a changed
    /// name, macOS announces the node's value (`accesskit_macos`'s `event.rs`).
    /// Visible-only emphasis already exists as [`Self::headline`], so
    /// accessible-only wording is symmetric to it.
    pub(crate) fn announcement(mut self, announcement: impl Into<SharedString>) -> Self {
        self.announcement = Some(announcement.into());
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

/// Marks `node` as a polite live region.
///
/// gpui-pre 0.3.10 never sets `Live`, and every AccessKit platform adapter
/// raises a live-region event only when `node.live() != Live::Off`
/// (`accesskit_windows` `adapter.rs`, `accesskit_macos` `event.rs`,
/// `accesskit_atspi_common` `node.rs`), so a notice is silent without this.
/// `Div` has no `live` builder and `Element::write_a11y_info` is only reachable
/// from a custom `Element`; the synthetic-children hook hands out
/// `parent_node()`, the node the element just contributed, which is the
/// bounded app-side way to set the property.
///
/// The hook only runs for an element that contributed a node, which is why a
/// [`Notice::quiet`] notice never reaches it. Ungated, `parent_node()` would not
/// panic (it only panics on an empty stack); it would return the ancestor node
/// still on the stack and make the container politely live.
pub(crate) fn polite_live_region(node: &mut accesskit::Node) {
    node.set_live(accesskit::Live::Polite);
}

/// Stops a live region at one of its children.
///
/// `accesskit_consumer` makes a node inherit its parent's `live` when it sets
/// none itself, and the severity caption is part of the notice's one
/// announcement, not a second one.
fn not_a_live_region(node: &mut accesskit::Node) {
    node.set_live(accesskit::Live::Off);
}

/// The confirmation a copy action shows (#82), or `None` before its first press.
///
/// It is the same notice on every surface that copies: visible beside the
/// control that copied, named for assistive technology, and with no button and
/// no dismiss timer, so nothing has to be dismissed and nothing disappears
/// mid-read. The words are the same everywhere; the surface owns the id and the
/// count.
///
/// One node with one stable id serves every press, and the count rides in the
/// announcement instead of in the identity: a changed id is a node removed and
/// another added (`gpui-pre/src/_accessibility.rs`), and every adapter raises
/// the live-region event from a changed name or value on the node that stayed
/// (`accesskit_windows` `adapter.rs`, `accesskit_macos` `event.rs`,
/// `accesskit_atspi_common` `node.rs`).
pub(crate) fn copy_confirmation(id: impl Into<ElementId>, presses: u32) -> Option<Notice> {
    (presses > 0).then(|| {
        let announcement = match presses {
            1 => "Copied".to_string(),
            presses => format!("Copied {presses} times"),
        };
        Notice::new(id, Severity::Success, "Copied").announcement(announcement)
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
        // A quiet notice names nothing, so it has no role and no label. The
        // announcement also becomes the node's value, because macOS announces a
        // live node by value (`accesskit_macos`'s `event.rs`).
        let announcement = self.announcement.clone();
        let name =
            (!self.quiet).then(|| announcement.clone().unwrap_or_else(|| self.message.clone()));

        div()
            .id(self.id.clone())
            .test_support()
            .when_some(name, |this, name| {
                this.role(self.severity.role())
                    .aria_label(name)
                    .when_some(announcement, |this, value| this.aria_value(value))
                    .a11y_synthetic_children(|builder| polite_live_region(builder.parent_node()))
            })
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
                    .a11y_synthetic_children(|builder| not_a_live_region(builder.parent_node()))
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
    use super::{Severity, copy_confirmation, not_a_live_region, polite_live_region};
    use crate::theme::{SystemTheme, apply_system_theme};
    use gpui_kit::component::Theme;
    use gpui_kit::{Hsla, Role, TestAppContext, accesskit};

    /// #82: the app can set a live region at all, and only through the node the
    /// synthetic-children hook hands out. The kit's snapshot API does not carry
    /// `Live` (`ElementSnapshot` has no such field) and the a11y tree dump does
    /// not either, so the property is pinned where the app decides it.
    #[test]
    fn a_notice_node_is_a_polite_live_region() {
        let mut node = accesskit::Node::new(Role::Status);
        assert_eq!(
            node.live(),
            None,
            "accesskit defaults to no live region, which is why the adapters stayed silent"
        );
        polite_live_region(&mut node);
        assert_eq!(node.live(), Some(accesskit::Live::Polite));

        // A node inherits its parent's live region, so the caption says out
        // loud that it is not one: the notice announces once.
        let mut caption = accesskit::Node::new(Role::Label);
        not_a_live_region(&mut caption);
        assert_eq!(caption.live(), Some(accesskit::Live::Off));
    }

    /// #82: a success is a status, a problem is an alert, and a copy keeps one
    /// element id while its announcement names the press.
    #[test]
    fn a_success_is_a_status_and_a_repeat_names_itself() {
        assert_eq!(Severity::Success.role(), Role::Status);
        for severity in [Severity::Error, Severity::Warning, Severity::Info] {
            assert_eq!(severity.role(), Role::Alert, "{severity:?} interrupts");
        }

        assert!(copy_confirmation("copy", 0).is_none(), "nothing copied yet");
        let first = copy_confirmation("copy", 1).expect("a named confirmation");
        let third = copy_confirmation("copy", 3).expect("a named confirmation");
        assert_eq!(first.id, third.id, "one node, one identity, every press");
        assert_eq!(first.message, "Copied", "the visible word never changes");
        assert_eq!(first.announcement.as_deref(), Some("Copied"));
        assert_eq!(third.announcement.as_deref(), Some("Copied 3 times"));
    }

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
