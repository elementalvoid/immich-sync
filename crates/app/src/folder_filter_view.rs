//! The folder filter control the failed-items and deletion-review views share
//! (#41, #79): the kit select that chooses the folder, and the chip that names
//! the active one.
//!
//! The rule it applies lives in [`crate::folder_filter`]. Rendering it once here
//! is what keeps the two views from growing two selects (N5 keeps it a named,
//! keyboard-reachable tab stop in both).
//!
//! #79 replaced the hand-built trigger, option list and focus workaround with
//! [`Select`], so the open state, the option list, arrows, Enter, Escape and the
//! focus return are the kit's contract instead of ours. The select is a
//! controlled view of [`FolderFilter`]: the filter stays the owner of the
//! choice, and every way of changing it (a chosen option, the chip's Clear, the
//! folder-detail seam) is pushed into the select on the next frame.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::IndexPath;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::searchable_list::{SearchableListDelegate, SearchableListItem};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sync_core::store::SourceFolder;

use crate::dashboard::CardPalette;
use crate::folder_filter::{ALL_FOLDERS, FolderFilter};
use crate::status::folder_name;

/// The filter row for one list view.
///
/// `prefix` names the element ids (`<prefix>-folder-filter`, `-option-<row>`,
/// `-chip`, `-clear`), so the two views never collide. The filter entity owns
/// the selection; the view that owns it observes the entity and re-renders its
/// list.
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
    let label = filter.read(cx).label();
    let selected = filter.read(cx).selected().map(str::to_string);
    let active = filter.read(cx).is_active();
    let binding = folder_select(prefix, filter, folders, selected.as_deref(), window, cx);
    let choices = binding.read(cx).choices.clone();
    // The trigger shows the filter's own label. A folder that is no longer a
    // source folder is not in the list, so it arrives as the placeholder
    // instead — the control still names what the list is narrowed to.
    let placeholder = label.clone();

    div()
        .id(format!("{prefix}-folder-bar"))
        .test_support()
        .flex()
        .flex_row()
        // A narrow window wraps the chip onto its own row.
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(
            div().w(px(200.)).flex_shrink_0().child(
                Select::new(&choices)
                    .id(format!("{prefix}-folder-filter"))
                    .accessibility_label("Folder filter")
                    .placeholder(placeholder),
            ),
        )
        .when(active, |this| {
            let clear = filter.clone();
            let trigger = choices.clone();
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
                                // The Clear button unmounts with the chip, so
                                // the focus goes to the control that survives
                                // instead of the window's first tab stop.
                                trigger.update(cx, |state, cx| state.focus(window, cx));
                            }),
                    ),
            )
        })
}

/// The select state the bar renders, created once per prefix and kept in sync
/// with the filter and the source folders.
///
/// Keyed element state, so both views get their own select without owning it,
/// and the state survives re-renders the way a widget's does.
fn folder_select(
    prefix: &'static str,
    filter: &Entity<FolderFilter>,
    folders: &[SourceFolder],
    selected: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<FolderSelect> {
    let binding = window.use_keyed_state(
        ElementId::Name(format!("{prefix}-folder-select").into()),
        cx,
        |window, cx| {
            let choices = FolderChoices::new(prefix, folders);
            let chosen = choices.index_of(selected);
            let filter = filter.clone();
            let choices = cx.new(|cx| SelectState::new(choices, chosen, window, cx));
            let confirmed = cx.subscribe(
                &choices,
                move |this: &mut FolderSelect,
                      _: Entity<SelectState<FolderChoices>>,
                      event: &SelectEvent<FolderChoices>,
                      cx| {
                    let SelectEvent::Confirm(value) = event;
                    let Some(value) = value else {
                        // The kit's own clear button, which the bar does not
                        // show: the chip's Clear owns that job.
                        return;
                    };
                    this.chosen = value.clone();
                    filter.update(cx, |filter, cx| {
                        match value {
                            Some(path) => filter.select(path.clone()),
                            None => filter.clear(),
                        }
                        cx.notify();
                    });
                },
            );
            FolderSelect {
                prefix,
                choices,
                folders: folders.to_vec(),
                chosen: selected.map(str::to_string),
                _confirmed: confirmed,
            }
        },
    );
    binding.update(cx, |this, cx| this.sync(folders, selected, window, cx));
    binding
}

/// One select control's whole state: the kit state, the folders it was built
/// from, and the choice last read from (or written to) the filter.
struct FolderSelect {
    /// Names this control's element ids; each list surface has its own.
    prefix: &'static str,
    choices: Entity<SelectState<FolderChoices>>,
    /// The folder list the items were built from.
    folders: Vec<SourceFolder>,
    /// The filter's selection as the select last saw it, so a frame that changed
    /// nothing does not push a value back into the control.
    chosen: Option<String>,
    _confirmed: Subscription,
}

impl FolderSelect {
    /// Pushes the filter and the folders into the control when they differ.
    ///
    /// Only a real difference touches the state: a kit setter notifies, so
    /// calling one every frame would schedule a frame every frame.
    fn sync(
        &mut self,
        folders: &[SourceFolder],
        selected: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.folders != folders {
            self.folders = folders.to_vec();
            let choices = FolderChoices::new(self.prefix, folders);
            self.choices
                .update(cx, |state, cx| state.set_items(choices, window, cx));
        }
        if self.chosen.as_deref() != selected {
            self.chosen = selected.map(str::to_string);
            let value = self.chosen.clone();
            self.choices.update(cx, |state, cx| {
                state.set_selected_value(&value, window, cx);
            });
        }
    }
}

/// The select's items: "All folders" first, then one row per source folder.
struct FolderChoices {
    prefix: &'static str,
    items: Vec<FolderChoice>,
}

/// One choice in the list.
#[derive(Clone)]
struct FolderChoice {
    /// `None` is [`ALL_FOLDERS`].
    path: Option<String>,
    /// What the row and the trigger show: the folder's name, not its path.
    label: SharedString,
    /// The accessible name: the full path, because two source folders can share
    /// a leaf name, and what the choice means when it is the current one.
    accessible: SharedString,
}

impl FolderChoices {
    fn new(prefix: &'static str, folders: &[SourceFolder]) -> Self {
        let all = FolderChoice {
            path: None,
            label: ALL_FOLDERS.into(),
            accessible: "Show items from all folders".into(),
        };
        let items = std::iter::once(all)
            .chain(folders.iter().map(|folder| FolderChoice {
                path: Some(folder.path.clone()),
                label: folder_name(&folder.path).into(),
                accessible: format!("Show only {}", folder.path).into(),
            }))
            .collect();
        Self { prefix, items }
    }

    /// The row that holds `path`, so the control opens on the current choice.
    fn index_of(&self, path: Option<&str>) -> Option<IndexPath> {
        self.items
            .iter()
            .position(|item| item.path.as_deref() == path)
            .map(IndexPath::new)
    }
}

impl SearchableListDelegate for FolderChoices {
    type Item = FolderChoice;

    fn items_count(&self, section: usize) -> usize {
        if section == 0 { self.items.len() } else { 0 }
    }

    fn item(&self, ix: IndexPath) -> Option<&Self::Item> {
        self.items.get(ix.row)
    }

    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        Self::Item: SearchableListItem<Value = V>,
        V: PartialEq,
    {
        self.items
            .iter()
            .position(|item| item.value() == value)
            .map(IndexPath::new)
    }

    /// The row the kit paints for each choice.
    ///
    /// The kit's default row is text only, so the accessible name and the check
    /// that marks the current choice are supplied here. The name says
    /// "selected" as well as drawing the check, so the state is never colour
    /// alone.
    fn render_item(
        &self,
        ix: IndexPath,
        item: &Self::Item,
        checked: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let id = format!("{}-folder-option-{}", self.prefix, ix.row);
        let accessible = if checked {
            format!("{}, selected", item.accessible)
        } else {
            item.accessible.to_string()
        };
        Some(
            div()
                .id(id.clone())
                .test_support()
                .role(Role::ListBoxOption)
                .aria_label(accessible)
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .w_full()
                .min_w_0()
                .child(div().min_w_0().truncate().child(item.label.clone()))
                .when(checked, |this| {
                    // A word, not the component family's checkmark: this
                    // application installs no icon assets, so an icon would
                    // draw nothing. The name says it too, for the same reason.
                    this.child(
                        div()
                            .id(format!("{id}-selected"))
                            .test_support()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Selected"),
                    )
                })
                .into_any_element(),
        )
    }
}

impl SearchableListItem for FolderChoice {
    type Value = Option<String>;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.path
    }
}
