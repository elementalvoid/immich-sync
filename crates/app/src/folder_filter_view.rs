//! The folder filter control the failed-items and deletion-review views share
//! (#41): the trigger, the source-folder options it shows while open, and the
//! chip that clears an active folder.
//!
//! The rule it applies lives in [`crate::folder_filter`]. Rendering it once here
//! is what keeps the two views from growing two dropdowns (N5 keeps it a named,
//! keyboard-reachable tab stop in both).

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Selectable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sync_core::store::SourceFolder;

use crate::dashboard::CardPalette;
use crate::folder_filter::{ALL_FOLDERS, FolderFilter};
use crate::status::folder_name;

/// The filter row for one list view.
///
/// `prefix` names the element ids (`<prefix>-folder-filter`, `-option-<index>`,
/// `-chip`, `-clear`), so the two views never collide. The filter entity owns
/// the selection and the open state; the view that owns it observes the entity
/// and re-renders its list.
pub fn folder_filter_bar(
    prefix: &'static str,
    filter: &Entity<FolderFilter>,
    folders: &[SourceFolder],
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement + use<> {
    let palette = CardPalette::new(cx.theme());
    let radius = cx.theme().radius_lg;
    let border = cx.theme().border;
    let muted = cx.theme().muted_foreground;
    // The row outlives the option list and the chip, so a choice or a clear
    // parks the keyboard focus back on the trigger instead of on the window's
    // first tab stop (N5). A focusable row is how gpui reaches a child: the row
    // is an anchor, not a tab stop of its own, and `focus_next` from it descends
    // to its first tab stop, the trigger.
    let row_focus = window
        .use_keyed_state(
            ElementId::Name(format!("{prefix}-folder-focus").into()),
            cx,
            |_, cx| cx.focus_handle(),
        )
        .read(cx)
        .clone()
        .tab_stop(false);
    let state = filter.read(cx);
    let label = state.label();
    let selected = state.selected().map(str::to_string);
    let open = state.is_open();
    let active = state.is_active();
    let trigger = filter.clone();

    let mut bar = div()
        .id(format!("{prefix}-folder-bar"))
        .test_support()
        .track_focus(&row_focus)
        .flex()
        .flex_row()
        // A narrow window wraps the chip onto its own row.
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(
            Button::new(format!("{prefix}-folder-filter"))
                .secondary()
                .label(label.clone())
                .accessibility_label(format!("Folder filter: {label}"))
                .dropdown_caret(true)
                .toggled(open)
                .on_click(move |_, _, cx| {
                    trigger.update(cx, |filter, cx| {
                        filter.toggle();
                        cx.notify();
                    });
                }),
        )
        .when(active, |this| {
            let clear = filter.clone();
            let clear_focus = row_focus.clone();
            this.child(
                div()
                    .id(format!("{prefix}-folder-chip"))
                    .test_support()
                    .role(Role::Group)
                    .aria_label(format!("Showing {label}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(radius)
                    .bg(palette.surface)
                    .border_1()
                    .border_color(border)
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("Showing {label}")),
                    )
                    .child(
                        Button::new(format!("{prefix}-folder-clear"))
                            .ghost()
                            .label("Clear")
                            .accessibility_label("Clear the folder filter")
                            .on_click(move |_, window, cx| {
                                clear.update(cx, |filter, cx| {
                                    filter.clear();
                                    cx.notify();
                                });
                                focus_trigger(&clear_focus, window, cx);
                            }),
                    ),
            )
        });

    if open {
        let options = std::iter::once(None)
            .chain(folders.iter().map(Some))
            .enumerate()
            .map(|(index, choice)| {
                option(
                    prefix,
                    index,
                    choice,
                    filter,
                    selected.as_deref(),
                    &row_focus,
                )
                .into_any_element()
            });
        bar = bar.child(
            div()
                .id(format!("{prefix}-folder-options"))
                .test_support()
                .role(Role::List)
                .aria_label("Source folders")
                .flex()
                .flex_col()
                .gap_1()
                .p_1()
                .rounded(radius)
                .bg(palette.surface)
                .border_1()
                .border_color(border)
                .children(options),
        );
    }

    bar
}

/// One choice in the option list: "All folders", or one source folder.
///
/// The visible label stays the folder's name; the accessible name carries the
/// full path, because two source folders can share a leaf name, and says when
/// the choice is the current one, so the selection is not colour alone.
fn option(
    prefix: &'static str,
    index: usize,
    choice: Option<&SourceFolder>,
    filter: &Entity<FolderFilter>,
    selected: Option<&str>,
    row_focus: &FocusHandle,
) -> Button {
    let filter = filter.clone();
    let row_focus = row_focus.clone();
    let path = choice.map(|folder| folder.path.clone());
    let is_selected = path.as_deref() == selected;
    let label = choice.map_or_else(
        || ALL_FOLDERS.to_string(),
        |folder| folder_name(&folder.path),
    );
    let accessibility_label = choice.map_or_else(
        || "Show items from all folders".to_string(),
        |folder| format!("Show only {}", folder.path),
    );
    let accessibility_label = if is_selected {
        format!("{accessibility_label}, selected")
    } else {
        accessibility_label
    };
    Button::new(format!("{prefix}-folder-option-{index}"))
        .ghost()
        .label(label)
        .accessibility_label(accessibility_label)
        .selected(is_selected)
        .on_click(move |_, window, cx| {
            let path = path.clone();
            filter.update(cx, |filter, cx| {
                match path {
                    Some(path) => filter.select(path),
                    None => filter.clear(),
                }
                cx.notify();
            });
            focus_trigger(&row_focus, window, cx);
        })
}

/// Puts the focus back on the trigger after a choice closed the control.
///
/// The activated option and the chip both unmount, so without this the keyboard
/// focus would fall back to the window's first tab stop.
fn focus_trigger(row_focus: &FocusHandle, window: &mut Window, cx: &mut App) {
    row_focus.focus(window, cx);
    window.focus_next(cx);
}
