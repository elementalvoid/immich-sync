//! The empty state the failed-items list, the deletion review, and the logs
//! tail render when they have nothing to show (#77).
//!
//! One component, so the three surfaces cannot drift apart: a title, a
//! description, and a slot for the one action a surface can honour.
//!
//! Two rules live here:
//!
//! - The state is one node: one name, one description. The titled box is a
//!   `Role::Status` whose accessible name is the title and whose description
//!   follows it, and the visible lines inside it are `Text::new_inaccessible`,
//!   so the component adds no second node of its own. It is also a polite live
//!   region ([`crate::notice::polite_live_region`]), which decides *when* that
//!   name is heard: the status carries it on every platform, Windows announces
//!   it when it appears and when the name changes, AT-SPI announces a changed
//!   name on a node that stays, and macOS announces only a node carrying a
//!   value, which a status has none of. So filtering a list to empty and
//!   re-titling the state that is already on screen is announced on Windows and
//!   AT-SPI, while a status that mounts is announced on Windows only (AT-SPI's
//!   `node_added` carries no `Announcement`). This is the lesson of
//!   [`crate::notice`]: a bare child string produces no accessibility node at
//!   all, so the status must carry the text. What surrounds the state is the
//!   surface's business: the failed list's summary above it states the same
//!   fact, so that window holds two identical statuses, and only the state can
//!   announce.
//! - A filtered list says which folder it is empty for. The title is the
//!   caller's sentence and already names the folder
//!   ([`crate::folder_filter::FolderFilter::empty_heading`]), so the component
//!   never invents a client-wide claim of its own.
//!
//! The action slot stays empty where the surface has no next step it can carry
//! out; an empty state never offers a control it cannot honour.

use crate::notice::polite_live_region;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// One empty state: what the surface has to show, why it is showing it, and the
/// step it offers from here.
///
/// `title` is the whole statement ("No failed items in Videos"), `description`
/// the reassurance under it, and both are read out: the title is the status
/// name and the description is its accessibility description.
#[derive(IntoElement)]
pub(crate) struct EmptyState {
    id: ElementId,
    title: SharedString,
    description: SharedString,
    action: Option<AnyElement>,
}

impl EmptyState {
    /// An empty state under the stable id `id`.
    pub(crate) fn new(
        id: impl Into<ElementId>,
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: description.into(),
            action: None,
        }
    }

    /// The one action this surface can carry out from here.
    ///
    /// Omit it when the surface has no next step of its own: an empty state
    /// offers no control it cannot honour. The logs tail is the one caller that
    /// re-offers its own refresh, because at the narrowest window the copy in
    /// the card above can be scrolled out while this one is on screen.
    pub(crate) fn action(mut self, action: impl IntoElement) -> Self {
        self.action = Some(action.into_any_element());
        self
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (ink, muted) = (theme.foreground, theme.muted_foreground);
        let action_id = (self.id.clone(), "action");
        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .w_full()
            .min_w_0()
            .rounded(theme.radius_lg)
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            // The title and the description are one node: a screen reader hears
            // the state, and nothing inside repeats it.
            .child(
                div()
                    .id(self.id.clone())
                    .test_support()
                    .role(Role::Status)
                    .aria_label(self.title.clone())
                    .aria_description(self.description.clone())
                    // The same live-region hook the notice uses: the adapters
                    // announce nothing whose live is `Off`.
                    .a11y_synthetic_children(|builder| polite_live_region(builder.parent_node()))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(ink)
                            .child(Text::new_inaccessible(self.title)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(Text::new_inaccessible(self.description)),
                    ),
            )
            .when_some(self.action, |this, action| {
                this.child(
                    div()
                        .id(action_id)
                        .test_support()
                        .flex()
                        .flex_row()
                        .child(action),
                )
            })
    }
}
