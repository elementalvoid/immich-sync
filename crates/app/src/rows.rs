//! The row shapes a card renders, shared by every surface (#89).
//!
//! Two kit shapes cover every row in the app:
//!
//! - [`Row`] — a label, the help sentence under it, and a control. It is one
//!   contract with two renderings: the settings surface groups, searches and
//!   hands it to the kit through [`Row::item`], and the kit draws it, while a
//!   Logs or Diagnostics card draws it itself through [`Row::render`]. The
//!   words, the id, the name and the help are the same on both; the frame is
//!   not, and the column rules differ where the kit has no rule for a card
//!   (the 60% label cap, the 480 px stacking breakpoint, the field wrapper —
//!   ADR 0001, "rows.rs mirrors `SettingItem`").
//! - [`value_list`] with [`value_item`] — a term and a read-only value with its
//!   help, for a row whose second column is a fact rather than a control (#80).
//!
//! Both live here because three views render them. A view that imported them
//! from `settings_view` was importing another view's chrome.
//!
//! The words are `SharedString` rather than `&'static str`: a settings row is a
//! fixed sentence, but a row that quotes the log directory or the bundle's path
//! is built per render. A settings call site passes its `&'static str` unchanged.
//!
//! Only the rendering subset lives here. The settings search reads a row's words
//! through the crate-visible fields; the query, the group and the page filtering
//! stay in `settings_view`, because no other view searches.

use std::rc::Rc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::description_list::{DescriptionItem, DescriptionList};
use gpui_kit::component::label::Label;
use gpui_kit::component::setting::{SettingField, SettingItem};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::a11y;
use crate::dashboard::CardPalette;

/// A row's control: the widget the field column renders. Shared, because both
/// renderings of a row call the same one.
type Control = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// One row: its words, its control, and the layout that puts them together.
///
/// The id, name and help attributes stay on the element each row builder wraps
/// around its control, because that wrapper is also what tracks the control's
/// focus handle: `text_row` and `numeric_row` forward the kit input's handle, and
/// a `Switch` or `Button` names itself. A row cannot own that wrapper without
/// also owning a handle it does not have (S4).
pub(crate) struct Row {
    /// The row's title, which names its control.
    pub(crate) title: SharedString,
    /// The help sentence under the title.
    pub(crate) help: SharedString,
    /// Where the label sits relative to the control.
    layout: Axis,
    /// The control, held as a shared closure so both renderings of the row use
    /// the same one.
    control: Control,
}

impl Row {
    /// A row whose control is the element the row itself names.
    pub(crate) fn setting(
        title: impl Into<SharedString>,
        help: impl Into<SharedString>,
        control: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self::with_layout(title, help, Axis::Horizontal, control)
    }

    /// A row whose field takes the whole width: the label and help sit above it
    /// at every window size.
    ///
    /// A long label word and a fixed-width field do not fit side by side on a
    /// page just wider than the component's stacking breakpoint, and the kit's
    /// field slot sizes to its content, so the field would draw past the page.
    pub(crate) fn stacked(
        title: impl Into<SharedString>,
        help: impl Into<SharedString>,
        control: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self::with_layout(title, help, Axis::Vertical, control)
    }

    /// A row whose control is its own widget: the row around it states the
    /// help sentence, because the widget names itself.
    pub(crate) fn wrapped(
        title: impl Into<SharedString>,
        help: impl Into<SharedString>,
        row_id: &'static str,
        control: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        let title = title.into();
        let help = help.into();
        let (name, description) = (title.clone(), help.clone());
        Self::setting(title, help, move |window, cx| {
            a11y::named(Role::Group, row_id, name.clone(), Some(description.clone()))
                .child(control(window, cx))
                .into_any_element()
        })
    }

    fn with_layout(
        title: impl Into<SharedString>,
        help: impl Into<SharedString>,
        layout: Axis,
        control: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            help: help.into(),
            layout,
            control: Rc::new(control),
        }
    }

    /// The item the settings surface groups, searches and renders.
    pub(crate) fn item(&self) -> SettingItem {
        let control = Rc::clone(&self.control);
        if self.is_note() {
            return SettingItem::render(move |_, window, cx| control(window, cx));
        }
        let field = SettingField::<SharedString>::render(move |_, window, cx| control(window, cx));
        SettingItem::new(self.title.clone(), field)
            .description(self.help.clone())
            .layout(self.layout)
    }

    /// The row itself, for a card that is not the kit's `Settings` surface.
    ///
    /// 0.8.1 renders a `SettingItem` only inside `Settings`: `render_item` is
    /// `pub(super)` (`setting/item.rs:252`), so a card cannot hand it one. This
    /// draws the item's own layout instead — the label and help column, the
    /// `gap_3` row, the control in the field column — from the same row, so the
    /// two renderings cannot disagree about the words, the ids or the layout
    /// choice.
    pub(crate) fn render(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let control = (self.control)(window, cx);
        if self.is_note() {
            return control;
        }
        // The label takes what the control leaves. `min_w_0` is what lets it
        // shrink there: gpui measures a sentence's minimum as its whole line, so
        // without it a long help sentence draws past the card. The kit's item
        // caps the column at 60% (`max_w_3_5`) instead, a rule for a page of
        // fixed-width fields; a card's field is a control that keeps its own
        // content width, and the cap would only shorten the help's line. The
        // help needs no `size_full` either: the column stretches it.
        let label = div()
            .flex()
            .flex_col()
            .map(|this| match self.layout {
                Axis::Horizontal => this.flex_1().min_w_0(),
                Axis::Vertical => this.w_full(),
            })
            .child(Label::new(self.title.clone()).text_sm())
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.help.clone()),
            );
        div()
            .w_full()
            .map(|this| match self.layout {
                Axis::Horizontal => this
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .gap_3(),
                Axis::Vertical => this.flex().flex_col().gap_3(),
            })
            .child(label)
            .child(div().child(control))
            .into_any_element()
    }

    /// Whether the row carries no words of its own: a notice, a button, a line
    /// of copy. It follows the group it sits in and never keeps one.
    pub(crate) fn is_note(&self) -> bool {
        self.title.is_empty()
    }
}

/// A row that is not a setting: a notice, a button, or a line of copy.
pub(crate) fn note(build: impl Fn(&mut Window, &mut App) -> AnyElement + 'static) -> Row {
    Row {
        title: SharedString::default(),
        help: SharedString::default(),
        layout: Axis::Vertical,
        control: Rc::new(build),
    }
}

/// The label column every value row shares, in rems: wide enough for the
/// longest term, and fixed, so a long value wraps inside its own column instead
/// of taking the term's width (#80).
const LABEL_COLUMN_REMS: f32 = 12.;

/// The most lines a value may take before it is cut (#80).
///
/// A value can be as long as a server or a filesystem makes it — a failure
/// message is the raw error text — and an unclamped one grows its card by that
/// many lines. The cut is visual only: the value's accessible name still carries
/// the whole text.
const VALUE_LINE_CLAMP: usize = 6;

/// The description list a card's value rows share (#80).
///
/// `columns(1)` puts one row in each line. `bordered(false)` leaves the card's
/// own surface and border to carry the box: the rows are a grid inside the
/// card, not a second card inside the first. The kit's list is a `RenderOnce`
/// with no role, so the wrapper is what gives the pairs their collection (F4).
pub(crate) fn value_list(id: &'static str, items: Vec<DescriptionItem>) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .role(Role::DescriptionList)
        .child(
            DescriptionList::new()
                .columns(1)
                .bordered(false)
                .label_width(rems(LABEL_COLUMN_REMS))
                .children(items),
        )
}

/// One value row: the term in the label cell, and the value with its help
/// sentence in the description cell.
///
/// The value names itself and the term beside it states the label, so a row
/// reads its term once and has no name to compose (F1).
pub(crate) fn value_item(
    label: &str,
    help: Option<&str>,
    value_id: &str,
    value: String,
    palette: &CardPalette,
) -> DescriptionItem {
    value_item_with_font(label, help, value_id, value, palette, None)
}

/// The archive path's row: [`value_item`] with its value set in the mono font,
/// as every other read-only path in the app is, so `l`/`1` and `O`/`0` stay
/// apart (#80).
pub(crate) fn mono_value_item(
    label: &str,
    help: Option<&str>,
    value_id: &str,
    value: String,
    palette: &CardPalette,
    mono: SharedString,
) -> DescriptionItem {
    value_item_with_font(label, help, value_id, value, palette, Some(mono))
}

/// The one row shape both constructors build, with `font` as the value's family
/// when it is not the UI's (#80).
fn value_item_with_font(
    label: &str,
    help: Option<&str>,
    value_id: &str,
    value: String,
    palette: &CardPalette,
    font: Option<SharedString>,
) -> DescriptionItem {
    DescriptionItem::new(
        crate::a11y::named(
            Role::Term,
            ElementId::Name(format!("{value_id}-label").into()),
            label,
            None,
        )
        .min_w_0()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .child(label.to_string())
        .into_any_element(),
    )
    .value(
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .min_w_0()
            .child(
                // The description-list value of the term above it, so the two
                // reach the tree as a pair and not as flat text (#80).
                crate::a11y::named(
                    Role::Definition,
                    ElementId::Name(value_id.into()),
                    value.clone(),
                    None,
                )
                .min_w_0()
                .line_clamp(VALUE_LINE_CLAMP)
                .text_xs()
                .text_color(palette.text)
                .when_some(font, |this, font| this.font_family(font))
                .child(value),
            )
            .when_some(help, |this, help| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(palette.meta)
                        .child(help.to_string()),
                )
            })
            .into_any_element(),
    )
}
