//! The folder filter control the failed-items and deletion-review views share
//! (#41): the trigger, the source-folder options it shows while open, and the
//! chip that clears an active folder.
//!
//! The rule it applies lives in [`crate::folder_filter`]. Rendering it once here
//! is what keeps the two views from growing two dropdowns (N5 keeps it a named,
//! keyboard-reachable tab stop in both).

use gpui_kit::component::ActiveTheme as _;
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
    cx: &App,
) -> impl IntoElement + use<> {
    let palette = CardPalette::new(cx.theme());
    let radius = cx.theme().radius_lg;
    let border = cx.theme().border;
    let muted = cx.theme().muted_foreground;
    let state = filter.read(cx);
    let label = state.label();
    let open = state.is_open();
    let active = state.is_active();
    let trigger = filter.clone();

    let mut bar = div()
        .id(format!("{prefix}-folder-bar"))
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
                            .on_click(move |_, _, cx| {
                                clear.update(cx, |filter, cx| {
                                    filter.clear();
                                    cx.notify();
                                });
                            }),
                    ),
            )
        });

    if open {
        let options = std::iter::once(None)
            .chain(folders.iter().map(Some))
            .enumerate()
            .map(|(index, choice)| option(prefix, index, choice, filter).into_any_element());
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
fn option(
    prefix: &'static str,
    index: usize,
    choice: Option<&SourceFolder>,
    filter: &Entity<FolderFilter>,
) -> Button {
    let filter = filter.clone();
    let path = choice.map(|folder| folder.path.clone());
    let (label, accessibility_label) = choice.map_or_else(
        || {
            (
                ALL_FOLDERS.to_string(),
                "Show items from all folders".to_string(),
            )
        },
        |folder| {
            let name = folder_name(&folder.path);
            (name.clone(), format!("Show only {name}"))
        },
    );
    Button::new(format!("{prefix}-folder-option-{index}"))
        .ghost()
        .label(label)
        .accessibility_label(accessibility_label)
        .on_click(move |_, _, cx| {
            let path = path.clone();
            filter.update(cx, |filter, cx| {
                match path {
                    Some(path) => filter.select(path),
                    None => filter.clear(),
                }
                cx.notify();
            });
        })
}
