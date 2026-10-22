//! The three per-folder settings controls, rendered in one place (#43).
//!
//! Folder setup and the folder detail's settings show the same stored row:
//! Recursive, Album mapping, and Album scope. The controls, their layout, and
//! their accessible names live here, so the two surfaces cannot drift apart.
//! The owner supplies the handlers, which is what lets the detail write each
//! change through the engine while the setup dialog keeps its draft.

use std::rc::Rc;

use gpui_kit::component::radio::Radio;
use gpui_kit::component::switch::Switch;
use gpui_kit::*;
use sync_core::store::AlbumScope;

/// The element ids the three controls get on one surface.
pub(crate) struct SettingsIds {
    /// The Recursive switch.
    pub recursive: ElementId,
    /// The Album mapping switch.
    pub album_mapping: ElementId,
    /// The one-album-per-folder radio.
    pub album_leaf: ElementId,
    /// The one-album-for-this-folder radio.
    pub album_whole: ElementId,
}

/// What the three controls show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SettingsValues {
    /// Whether the scan descends into subfolders (R2.1).
    pub recursive: bool,
    /// Whether this folder's files join an album (R3.4).
    pub album_mapping: bool,
    /// What the folder maps to once the mapping is on (R3.4).
    pub album_scope: AlbumScope,
}

/// The controls, the folder they belong to, and what a change asks for.
pub(crate) struct SettingsControls<R, M, S> {
    ids: SettingsIds,
    /// The folder name, so each accessible name says which folder it changes.
    name: String,
    values: SettingsValues,
    on_recursive: R,
    on_album_mapping: M,
    /// Both scope radios answer one handler, so it is shared rather than
    /// moved into the first of them.
    on_album_scope: Rc<S>,
}

impl<R, M, S> SettingsControls<R, M, S>
where
    R: Fn(&bool, &mut Window, &mut App) + 'static,
    M: Fn(&bool, &mut Window, &mut App) + 'static,
    S: Fn(&AlbumScope, &mut Window, &mut App) + 'static,
{
    /// Collects one surface's ids, values, and handlers.
    pub(crate) fn new(
        ids: SettingsIds,
        name: impl Into<String>,
        values: SettingsValues,
        on_recursive: R,
        on_album_mapping: M,
        on_album_scope: S,
    ) -> Self {
        Self {
            ids,
            name: name.into(),
            values,
            on_recursive,
            on_album_mapping,
            on_album_scope: Rc::new(on_album_scope),
        }
    }

    /// The rows: one setting each, label left and control right, so a reader
    /// can see where one setting ends and the next begins (#57).
    ///
    /// The scope stays choosable while the mapping is off: it is the folder's
    /// stored scope (R3.4) and takes effect as soon as the mapping is on, and
    /// the kit's `Radio` does not announce a disabled state, so a disabled radio
    /// would read as an enabled one (N5).
    pub(crate) fn render(self, muted: Hsla) -> AnyElement {
        let label = |text: &'static str| div().text_xs().text_color(muted).child(text);
        // The house settings row: the label on the left and the control on the
        // right, with the row's height from Settings' own rows.
        let row = || {
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_6()
                .min_h(rems(2.25))
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(
                row().child(label("Recursive")).child(
                    Switch::new(self.ids.recursive)
                        .checked(self.values.recursive)
                        .accessibility_label(format!("Sync {} recursively", self.name))
                        .on_click(move |recursive, window, cx| {
                            (self.on_recursive)(recursive, window, cx);
                        }),
                ),
            )
            .child(
                row().child(label("Album mapping")).child(
                    Switch::new(self.ids.album_mapping)
                        .checked(self.values.album_mapping)
                        .accessibility_label(format!("Album mapping for {}", self.name))
                        .on_click(move |enabled, window, cx| {
                            (self.on_album_mapping)(enabled, window, cx);
                        }),
                ),
            )
            .child(
                row().child(label("Album scope")).child(
                    // The two visible labels are long, so the radios stack as one
                    // right-aligned block instead of splitting a line at the
                    // narrow dialog width.
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_2()
                        .child(
                            Radio::new(self.ids.album_leaf)
                                .label(AlbumScope::Leaf.label())
                                .accessibility_label(format!(
                                    "Album scope for {}: one album per folder",
                                    self.name
                                ))
                                .checked(self.values.album_scope == AlbumScope::Leaf)
                                .on_change({
                                    let scope = Rc::clone(&self.on_album_scope);
                                    move |checked, window, cx| {
                                        if *checked {
                                            scope(&AlbumScope::Leaf, window, cx);
                                        }
                                    }
                                }),
                        )
                        .child(
                            Radio::new(self.ids.album_whole)
                                .label(AlbumScope::WholeFolder.label())
                                .accessibility_label(format!(
                                    "Album scope for {}: one album for this folder",
                                    self.name
                                ))
                                .checked(self.values.album_scope == AlbumScope::WholeFolder)
                                .on_change({
                                    let scope = Rc::clone(&self.on_album_scope);
                                    move |checked, window, cx| {
                                        if *checked {
                                            scope(&AlbumScope::WholeFolder, window, cx);
                                        }
                                    }
                                }),
                        ),
                ),
            )
            .into_any_element()
    }
}
