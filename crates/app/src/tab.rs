//! The tab both strips draw: the shell's surface switch (#75) and the settings
//! page strip (#78).
//!
//! A strip lists the surfaces or pages of one thing, so each entry is a
//! `Role::Tab` that reports whether it is the one on screen. A kit `Button`
//! cannot carry that state, and a page is a tab, not a toggle.

use gpui_kit::base::ObservedElement;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::ThemeStyled as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// One tab of a strip, ready for the strip's own click listener.
///
/// The id is the app's, not the kit's: the pointer and the tests find a tab by
/// the surface or page it opens, so adding one renumbers nothing.
pub(crate) struct Tab<'a> {
    id: &'static str,
    label: &'static str,
    selected: bool,
    focus: &'a FocusHandle,
    /// This tab's place in the strip, counted from one, and the strip's size.
    position: (usize, usize),
}

impl<'a> Tab<'a> {
    pub(crate) fn new(
        id: &'static str,
        label: &'static str,
        selected: bool,
        focus: &'a FocusHandle,
        position: usize,
        count: usize,
    ) -> Self {
        Self {
            id,
            label,
            selected,
            focus,
            position: (position, count),
        }
    }

    pub(crate) fn render(self, window: &Window, cx: &App) -> ObservedElement<Stateful<Div>> {
        let theme = cx.theme();
        let (primary, primary_foreground) = (theme.primary, theme.primary_foreground);
        let (foreground, muted_foreground) = (theme.foreground, theme.muted_foreground);
        let (accent, radius) = (theme.accent, theme.radius);
        let (position, count) = self.position;
        let focus = self.focus;
        div()
            .id(self.id)
            .test_support()
            .role(Role::Tab)
            .aria_label(self.label)
            // Both states are always reported, so a tab that is not the one on
            // screen says so instead of saying nothing.
            .aria_selected(self.selected)
            .aria_position_in_set(position)
            .aria_size_of_set(count)
            .track_focus(focus)
            .flex()
            .items_center()
            // 10 px of side padding keeps the six page names on one line at the
            // shell's 520 px minimum.
            .px_2p5()
            .py_1()
            .rounded(radius)
            .text_sm()
            .text_color(if self.selected {
                primary_foreground
            } else {
                muted_foreground
            })
            .when(self.selected, |tab| {
                tab.bg(primary).font_weight(FontWeight::SEMIBOLD)
            })
            .when(!self.selected, |tab| {
                tab.hover(move |style| style.bg(accent).text_color(foreground))
            })
            .when(focus.is_focused(window), |tab| {
                tab.focus_ring_style(window, cx)
            })
            .child(self.label)
    }
}
