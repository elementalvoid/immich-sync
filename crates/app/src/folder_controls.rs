//! The per-folder settings controls, rendered in one place (#43, #93).
//!
//! Folder setup and the folder detail's settings show the same stored row:
//! Recursive, Album mapping, Album scope, how each album below the folder is
//! named, and whether the folder's album title comes first. The controls, their
//! layout, and their accessible names live here, so the two surfaces cannot
//! drift apart. The owner supplies the handlers, which is what lets the detail
//! write each change through the engine while the setup dialog keeps its draft.

use std::rc::Rc;

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::radio::Radio;
use gpui_kit::component::switch::Switch;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sync_core::store::{AlbumPath, AlbumScope};

/// The sentence the prefix control carries (#93): what turning it on does, in
/// the same words the status card uses.
///
/// "Album title" is the field above it, so the sentence points at a word the
/// user has already read instead of introducing a second name for it.
pub(crate) const ALBUM_PREFIX_LABEL: &str = "Start names with this folder's album title";

/// The element ids the settings controls get on one surface.
pub(crate) struct SettingsIds {
    /// The Recursive switch.
    pub recursive: ElementId,
    /// The Album mapping switch.
    pub album_mapping: ElementId,
    /// The one-album-per-folder radio.
    pub album_leaf: ElementId,
    /// The one-album-for-this-folder radio.
    pub album_whole: ElementId,
    /// The folder-path album name radio.
    pub album_path: ElementId,
    /// The folder-name album name radio.
    pub album_name: ElementId,
    /// The source-album prefix checkbox.
    pub album_prefix: ElementId,
}

/// What the settings controls show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SettingsValues {
    /// Whether the scan descends into subfolders (R2.1).
    pub recursive: bool,
    /// Whether this folder's files join an album (R3.4).
    pub album_mapping: bool,
    /// What the folder maps to once the mapping is on (R3.4).
    pub album_scope: AlbumScope,
    /// How each album below the folder is named (#93).
    pub album_path: AlbumPath,
    /// Whether the folder's album title comes first (#93).
    pub album_prefix: bool,
}

/// What a change to one control asks for, one handler per control.
pub(crate) struct SettingsHandlers<R, M, S, P, X> {
    /// The Recursive switch.
    pub recursive: R,
    /// The Album mapping switch.
    pub album_mapping: M,
    /// Both scope radios.
    pub album_scope: S,
    /// Both album-name radios.
    pub album_path: P,
    /// The album-prefix checkbox.
    pub album_prefix: X,
}

/// The controls, the folder they belong to, and what a change asks for.
pub(crate) struct SettingsControls<R, M, S, P, X> {
    ids: SettingsIds,
    /// The folder name, so each accessible name says which folder it changes.
    name: String,
    values: SettingsValues,
    on_recursive: R,
    on_album_mapping: M,
    on_album_prefix: X,
    /// Both scope radios answer one handler, so it is shared rather than
    /// moved into the first of them.
    on_album_scope: Rc<S>,
    /// Both album-name radios answer one handler, for the same reason.
    on_album_path: Rc<P>,
}

impl<R, M, S, P, X> SettingsControls<R, M, S, P, X>
where
    R: Fn(&bool, &mut Window, &mut App) + 'static,
    M: Fn(&bool, &mut Window, &mut App) + 'static,
    S: Fn(&AlbumScope, &mut Window, &mut App) + 'static,
    P: Fn(&AlbumPath, &mut Window, &mut App) + 'static,
    X: Fn(&bool, &mut Window, &mut App) + 'static,
{
    /// Collects one surface's ids, values, and handlers.
    pub(crate) fn new(
        ids: SettingsIds,
        name: impl Into<String>,
        values: SettingsValues,
        handlers: SettingsHandlers<R, M, S, P, X>,
    ) -> Self {
        let on_album_scope = Rc::new(handlers.album_scope);
        let on_album_path = Rc::new(handlers.album_path);
        Self {
            ids,
            name: name.into(),
            values,
            on_recursive: handlers.recursive,
            on_album_mapping: handlers.album_mapping,
            on_album_prefix: handlers.album_prefix,
            on_album_scope,
            on_album_path,
        }
    }

    /// The rows: one setting each, label left and control right, so a reader
    /// can see where one setting ends and the next begins (#57).
    ///
    /// The scope stays choosable while the mapping is off: it is the folder's
    /// stored scope (R3.4) and takes effect as soon as the mapping is on, and
    /// the kit's `Radio` does not announce a disabled state, so a disabled radio
    /// would read as an enabled one (N5).
    ///
    /// The two leaf naming rows are shown only under the per-folder scope
    /// (#93): a whole-folder album has no leaf names, so they would be inert.
    /// They are conditional content, not disabled controls, so nothing is left
    /// on screen that a screen reader cannot act on.
    pub(crate) fn render(self, muted: Hsla) -> AnyElement {
        let Self {
            ids,
            name,
            values,
            on_recursive,
            on_album_mapping,
            on_album_prefix,
            on_album_scope,
            on_album_path,
        } = self;
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
        // The scope labels are long, so that radio pair stacks as one
        // right-aligned block instead of splitting a line at the narrow dialog
        // width.
        let radios = || div().flex().flex_col().items_start().gap_2();
        // The album-name labels are two words each, and the row label says
        // "Album names", so the pair fits one line.
        let short_radios = div().flex().flex_row().items_center().gap_4();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(
                row().child(label("Recursive")).child(
                    Switch::new(ids.recursive)
                        .checked(values.recursive)
                        .accessibility_label(format!("Sync {} recursively", name))
                        .on_click(move |recursive, window, cx| {
                            on_recursive(recursive, window, cx);
                        }),
                ),
            )
            .child(
                row().child(label("Album mapping")).child(
                    Switch::new(ids.album_mapping)
                        .checked(values.album_mapping)
                        .accessibility_label(format!("Album mapping for {}", name))
                        .on_click(move |enabled, window, cx| {
                            on_album_mapping(enabled, window, cx);
                        }),
                ),
            )
            .child(
                row().child(label("Album scope")).child(
                    radios()
                        .child(
                            Radio::new(ids.album_leaf)
                                .label(AlbumScope::Leaf.label())
                                .accessibility_label(format!(
                                    "Album scope for {}: one album per folder",
                                    name
                                ))
                                .checked(values.album_scope == AlbumScope::Leaf)
                                .on_change({
                                    let scope = Rc::clone(&on_album_scope);
                                    move |checked, window, cx| {
                                        if *checked {
                                            scope(&AlbumScope::Leaf, window, cx);
                                        }
                                    }
                                }),
                        )
                        .child(
                            Radio::new(ids.album_whole)
                                .label(AlbumScope::WholeFolder.label())
                                .accessibility_label(format!(
                                    "Album scope for {}: one album for this folder",
                                    name
                                ))
                                .checked(values.album_scope == AlbumScope::WholeFolder)
                                .on_change({
                                    let scope = Rc::clone(&on_album_scope);
                                    move |checked, window, cx| {
                                        if *checked {
                                            scope(&AlbumScope::WholeFolder, window, cx);
                                        }
                                    }
                                }),
                        ),
                ),
            )
            .when(values.album_scope == AlbumScope::Leaf, |this| {
                this.child(
                    row().child(label("Album names")).child(
                        short_radios
                            .child(
                                Radio::new(ids.album_path)
                                    .label(AlbumPath::Relative.label())
                                    .accessibility_label(format!(
                                        "Album names for {}: folder path",
                                        name
                                    ))
                                    .checked(values.album_path == AlbumPath::Relative)
                                    .on_change({
                                        let album_path = Rc::clone(&on_album_path);
                                        move |checked, window, cx| {
                                            if *checked {
                                                album_path(&AlbumPath::Relative, window, cx);
                                            }
                                        }
                                    }),
                            )
                            .child(
                                Radio::new(ids.album_name)
                                    .label(AlbumPath::LeafName.label())
                                    .accessibility_label(format!(
                                        "Album names for {}: folder name",
                                        name
                                    ))
                                    .checked(values.album_path == AlbumPath::LeafName)
                                    .on_change({
                                        let album_path = Rc::clone(&on_album_path);
                                        move |checked, window, cx| {
                                            if *checked {
                                                album_path(&AlbumPath::LeafName, window, cx);
                                            }
                                        }
                                    }),
                            ),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .min_h(rems(2.25))
                        .child(
                            Checkbox::new(ids.album_prefix)
                                // The sentence is long, so the box takes the
                                // row's width and the label wraps inside it
                                // instead of running past the dialog at the
                                // narrowest window.
                                .w_full()
                                .min_w_0()
                                // The visible label is the accessible name: it
                                // already says "this folder", so a folder-name
                                // suffix would repeat it.
                                .label(ALBUM_PREFIX_LABEL)
                                .checked(values.album_prefix)
                                .on_click(move |prefixed, window, cx| {
                                    on_album_prefix(prefixed, window, cx);
                                }),
                        ),
                )
            })
            .into_any_element()
    }
}
