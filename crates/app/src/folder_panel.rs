//! The folder panel: one source folder's settings, confirmed before it syncs
//! (#40).
//!
//! The panel owns the draft, the reason the last Add did not write, and the
//! modal host. The dashboard owns which panel is open: it creates one when the
//! add field or Browse asks for setup, and drops it when the panel reports that
//! the folder was added or the user cancelled.

use std::path::Path;

use gpui_kit::base::Disableable as _;
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::radio::Radio;
use gpui_kit::component::switch::Switch;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sync_core::store::{AlbumScope, SourceFolder};

use crate::dashboard::CardPalette;
use crate::engine::EngineModel;
use crate::settings_view::notice;

/// The reason the setup dialog shows when the folder is already tracked (#40).
pub(crate) const ALREADY_ADDED: &str = "Already added.";

/// What the platform's filesystem holds at a draft path (#40).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathProbe {
    /// Nothing is there.
    Missing,
    /// A file or another non-directory sits there.
    File,
    /// A directory this process cannot list.
    Unreadable,
    /// A directory this process can list.
    Folder,
}

/// The reason a draft path cannot be a source folder, or `None` when it can.
fn path_problem(probe: PathProbe) -> Option<&'static str> {
    match probe {
        PathProbe::Missing => Some("That path does not exist."),
        PathProbe::File => Some("That path is a file, not a folder."),
        PathProbe::Unreadable => Some("That folder cannot be read."),
        PathProbe::Folder => None,
    }
}

/// Probes one path: it must be a directory this process can list (R2.1).
fn probe(path: &Path) -> PathProbe {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => match std::fs::read_dir(path) {
            Ok(_) => PathProbe::Folder,
            Err(_) => PathProbe::Unreadable,
        },
        Ok(_) => PathProbe::File,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => PathProbe::Missing,
        Err(_) => PathProbe::Unreadable,
    }
}

/// Validates one setup-dialog draft and returns the row to write (#40).
///
/// This runs on the worker, so the filesystem probe never blocks the UI thread
/// (N2). Nothing is written here: the caller writes the returned folder, and an
/// error is the reason the dialog shows while it stays open.
pub(crate) fn confirmed(draft: &SourceFolder) -> Result<SourceFolder, String> {
    let path = draft.path.trim();
    if path.is_empty() {
        return Err("Enter the path of a folder to add.".to_string());
    }
    if let Some(problem) = path_problem(probe(Path::new(path))) {
        return Err(problem.to_string());
    }
    Ok(SourceFolder {
        path: path.to_string(),
        ..draft.clone()
    })
}

/// Which lifecycle the panel shows.
///
/// The setup dialog is the draft. The path, the three settings, and the modal
/// host below are shared with any later folder mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderPanelMode {
    /// A folder that is not tracked yet: Add records the draft and Cancel
    /// discards it (#40).
    Draft,
}

/// What the panel asks its owner to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderPanelEvent {
    /// The folder was written: close the panel and show its card.
    Added,
    /// The user cancelled: close the panel; nothing was written.
    Cancelled,
}

/// The folder panel: a draft's settings, the Add/Cancel lifecycle, and the
/// focus-trapped modal host.
pub struct FolderPanel {
    engine: Entity<EngineModel>,
    mode: FolderPanelMode,
    /// The draft: the path plus the three settings the user confirms.
    draft: SourceFolder,
    /// Why the last Add did not write. The panel stays open while it is set.
    problem: Option<String>,
    /// The engine request this panel waits for, so a slower earlier result
    /// cannot answer a newer dialog (#40).
    pending: Option<u64>,
    focus: FocusHandle,
    _subscription: Subscription,
}

impl EventEmitter<FolderPanelEvent> for FolderPanel {}

impl FolderPanel {
    /// Opens the setup dialog for `path` (#40).
    pub fn setup(engine: Entity<EngineModel>, path: String, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let subscription = cx.observe(&engine, |this, engine, cx| this.engine_changed(&engine, cx));
        Self {
            engine,
            mode: FolderPanelMode::Draft,
            draft: SourceFolder {
                path: path.trim().to_string(),
                recursive: true,
                album_mapping: false,
                album_scope: AlbumScope::Leaf,
            },
            problem: None,
            pending: None,
            focus,
            _subscription: subscription,
        }
    }

    /// Applies the newest engine result when it answers this panel's request.
    fn engine_changed(&mut self, engine: &Entity<EngineModel>, cx: &mut Context<Self>) {
        let Some(result) = engine.read(cx).folder_add_result().cloned() else {
            return;
        };
        if self.pending != Some(result.request) {
            return;
        }
        self.pending = None;
        match result.problem {
            None => cx.emit(FolderPanelEvent::Added),
            Some(problem) => {
                self.problem = Some(problem);
                cx.notify();
            }
        }
    }

    /// Writes the draft through the engine model (#40).
    ///
    /// One request at a time: a second Add while the first is in flight is
    /// ignored, so a slow store cannot write the folder twice.
    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        self.problem = None;
        let draft = self.draft.clone();
        let engine = self.engine.clone();
        let request = engine.update(cx, |engine, cx| engine.add_folder_settings(draft, cx));
        self.pending = Some(request);
        // Cancel is disabled while the request runs, and a disabled control
        // drops focus to the window root, which would stop the modal host
        // receiving Escape. Keep the host focused so the dialog stays
        // cancellable from the keyboard.
        window.focus(&self.focus, cx);
        // A failed worker reports synchronously, before `pending` was set;
        // read the result back so that failure cannot be missed.
        self.engine_changed(&engine, cx);
        cx.notify();
    }

    /// Cancels the draft. While an Add is in flight this is a no-op: its result
    /// (a written folder or a refusal) must be shown, so the dialog waits for
    /// it instead of closing under it.
    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        cx.emit(FolderPanelEvent::Cancelled);
    }

    fn set_recursive(&mut self, recursive: bool, cx: &mut Context<Self>) {
        self.draft.recursive = recursive;
        cx.notify();
    }

    fn set_album_mapping(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.draft.album_mapping = enabled;
        cx.notify();
    }

    fn set_album_scope(&mut self, scope: AlbumScope, cx: &mut Context<Self>) {
        self.draft.album_scope = scope;
        cx.notify();
    }

    /// The heading the mode gives the panel.
    fn title(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let text = match self.mode {
            FolderPanelMode::Draft => "Add a source folder",
        };
        div()
            .text_base()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().foreground)
            .child(text)
    }

    /// The path the dialog confirms (R2.1), shown as one line.
    fn path(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let shown = if self.draft.path.is_empty() {
            "No path yet.".to_string()
        } else {
            self.draft.path.clone()
        };
        div()
            .id("folder-panel-path")
            .test_support()
            .aria_label(format!("Folder path: {shown}"))
            .truncate()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(shown)
    }

    /// The three settings the setup dialog confirms (R2.1, R3.4).
    fn settings(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let muted = cx.theme().muted_foreground;
        let leaf = self.draft.album_scope == AlbumScope::Leaf;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().text_xs().text_color(muted).child("Recursive"))
                    .child(
                        Switch::new("folder-panel-recursive")
                            .checked(self.draft.recursive)
                            .accessibility_label("Sync this folder recursively")
                            .on_click(cx.listener(|this, recursive, _, cx| {
                                this.set_recursive(*recursive, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().text_xs().text_color(muted).child("Album mapping"))
                    .child(
                        Switch::new("folder-panel-album")
                            .checked(self.draft.album_mapping)
                            .accessibility_label("Album mapping for this folder")
                            .on_click(cx.listener(|this, enabled, _, cx| {
                                this.set_album_mapping(*enabled, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(div().text_xs().text_color(muted).child("Album scope"))
                    .child(
                        Radio::new("folder-panel-album-leaf")
                            .label(AlbumScope::Leaf.label())
                            .accessibility_label(
                                "Album scope for this folder: one album per folder",
                            )
                            .checked(leaf)
                            .on_change(cx.listener(|this, checked, _, cx| {
                                if *checked {
                                    this.set_album_scope(AlbumScope::Leaf, cx);
                                }
                            })),
                    )
                    .child(
                        Radio::new("folder-panel-album-whole")
                            .label(AlbumScope::WholeFolder.label())
                            .accessibility_label(
                                "Album scope for this folder: one album for this folder",
                            )
                            .checked(!leaf)
                            .on_change(cx.listener(|this, checked, _, cx| {
                                if *checked {
                                    this.set_album_scope(AlbumScope::WholeFolder, cx);
                                }
                            })),
                    ),
            )
    }

    /// The draft footer: Cancel writes nothing, Add records the four values.
    fn actions(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pending = self.pending.is_some();
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .gap_2()
            .child(
                Button::new("folder-panel-cancel")
                    .secondary()
                    .disabled(pending)
                    .label("Cancel")
                    .accessibility_label("Cancel adding the folder")
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
            )
            .child(
                Button::new("folder-panel-add")
                    .primary()
                    // The visible label shows the wait so a slow worker is not
                    // silence; the control stays enabled so focus does not move.
                    .label(if pending { "Adding…" } else { "Add" })
                    .accessibility_label("Add the folder")
                    .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
            )
    }

    /// The modal host: centered over a backdrop, focus trapped, Escape cancels.
    fn modal(&self, card: AnyElement, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let focus = self.focus.clone();
        let viewport = window.viewport_size();
        deferred(
            anchored().position(point(px(0.), px(0.))).child(
                div()
                    .id("folder-panel-host")
                    .test_support()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w(viewport.width)
                    .h(viewport.height)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme.background.opacity(0.75))
                    .track_focus(&focus)
                    .focus_trap("folder-panel-trap", &focus)
                    // The backdrop swallows pointer and wheel events, so the
                    // dashboard behind the modal stays inert.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "escape" {
                            this.cancel(cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(card),
            ),
        )
        .with_priority(10)
        .into_any_element()
    }
}

impl Focusable for FolderPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FolderPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The theme is copied before the controls borrow the context mutably.
        let theme = cx.theme().clone();
        let palette = CardPalette::new(&theme);
        let card = div()
            .id("folder-panel")
            .test_support()
            .role(Role::Dialog)
            .aria_label("Folder setup")
            .flex()
            .flex_col()
            .gap_3()
            .w(rems(30.))
            .p_5()
            .rounded(theme.radius_lg)
            .bg(theme.popover)
            .text_color(theme.foreground)
            .border_1()
            .border_color(theme.border)
            .child(self.title(cx))
            .child(self.path(cx))
            .child(self.settings(cx))
            .when_some(self.problem.clone(), |this, problem| {
                this.child(notice(
                    "folder-panel-problem",
                    problem,
                    theme.danger,
                    &palette,
                ))
            })
            .child(self.actions(cx))
            .into_any_element();
        self.modal(card, window, cx)
    }
}

#[cfg(test)]
mod tests {
    // GPUI's glob imports its own `test` macro, so this module names what it
    // uses and keeps Rust's `#[test]`.
    use super::{PathProbe, confirmed, path_problem};
    use gpui_kit::TestAppContext;
    use sync_core::store::{AlbumScope, SourceFolder};

    fn draft(path: &str) -> SourceFolder {
        SourceFolder {
            path: path.to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
        }
    }

    #[test]
    fn every_probe_has_its_own_reason_and_only_a_folder_passes() {
        assert_eq!(
            path_problem(PathProbe::Missing),
            Some("That path does not exist.")
        );
        assert_eq!(
            path_problem(PathProbe::File),
            Some("That path is a file, not a folder.")
        );
        assert_eq!(
            path_problem(PathProbe::Unreadable),
            Some("That folder cannot be read.")
        );
        assert_eq!(path_problem(PathProbe::Folder), None);
    }

    #[test]
    fn an_empty_draft_names_the_missing_path() {
        let error = confirmed(&draft("  ")).expect_err("an empty path is refused");
        assert_eq!(error, "Enter the path of a folder to add.");
    }

    #[test]
    fn a_missing_path_is_refused_with_its_own_reason() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let missing = temp.path().join("not there");

        let error = confirmed(&draft(&missing.display().to_string()))
            .expect_err("a missing path is refused");
        assert_eq!(error, "That path does not exist.");
    }

    #[test]
    fn a_file_is_refused_as_not_a_folder() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let file = temp.path().join("photo.jpg");
        std::fs::write(&file, b"not a folder").expect("create the file");

        let error = confirmed(&draft(&file.display().to_string())).expect_err("a file is refused");
        assert_eq!(error, "That path is a file, not a folder.");
    }

    /// #40 (R2.1): a directory this process cannot list is refused with its
    /// own reason. The test skips itself when the process can still list a
    /// mode-000 directory, which is what running as root does.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_is_refused_with_its_own_reason() {
        use super::probe;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("create a temp dir");
        let locked = temp.path().join("locked");
        std::fs::create_dir(&locked).expect("create the folder");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("lock the folder");

        let probed = probe(&locked);
        let refused = confirmed(&draft(&locked.display().to_string()));
        // Restore the mode before the tempdir drops, so its cleanup can empty
        // the directory.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("unlock the folder");

        if probed == PathProbe::Folder {
            eprintln!(
                "skipping an_unreadable_folder_is_refused_with_its_own_reason: \
                 this process lists a mode-000 directory, so it is root"
            );
            return;
        }
        assert_eq!(probed, PathProbe::Unreadable);
        let error = refused.expect_err("an unreadable folder is refused");
        assert_eq!(error, "That folder cannot be read.");
    }

    #[test]
    fn a_readable_folder_passes_with_its_path_trimmed() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let photos = temp.path().join("photos");
        std::fs::create_dir(&photos).expect("create the folder");
        let spelled = format!("  {}  ", photos.display());

        let folder = confirmed(&draft(&spelled)).expect("a readable folder is accepted");
        assert_eq!(folder.path, photos.display().to_string());
        assert!(folder.recursive, "the draft's settings are kept");
        assert!(!folder.album_mapping);
        assert_eq!(folder.album_scope, AlbumScope::Leaf);
    }

    /// #40 (S1): a worker answer that never arrives must not strand the setup
    /// dialog. The panel stops waiting and shows the reason, so Cancel is no
    /// longer refused and the dialog can be closed.
    #[gpui_kit::test]
    fn a_lost_worker_answer_ends_the_pending_add(cx: &mut TestAppContext) {
        use crate::connection::{ConnectionModel, WORKER_UNAVAILABLE};
        use crate::engine::EngineModel;
        use crate::paths::AppPaths;
        use crate::store::StoreModel;
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{AppContext as _, Bounds, Point, WindowBounds, WindowOptions, px, size};
        use std::sync::Arc;
        use sync_core::connection::ServerProfile;
        use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};

        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = AppPaths {
            config_file: temp.path().join("config").join("settings.toml"),
            state_db: temp.path().join("data").join("state.db"),
            autostart_dir: temp.path().join("config").join("autostart"),
        };
        // The add runs on the shared HTTP runtime, a real thread.
        cx.executor().allow_parking();
        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        let (window, panel, engine) = cx.update(|cx| {
            gpui_kit::init(cx);
            let connection =
                cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials));
            let store = cx.new(|cx| StoreModel::open(&paths, cx));
            let engine = cx.new(|cx| EngineModel::new(connection, store, &paths, cx));
            let panel = cx.new(|cx| {
                super::FolderPanel::setup(engine.clone(), "/no such folder".to_string(), cx)
            });
            let root = panel.clone();
            let (window, _root) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1200.), px(800.)),
                    })),
                    ..Default::default()
                },
                cx,
                |_, _| root,
            )
            .expect("open the panel's window");
            (window, panel, engine)
        });

        // Add starts a request and this test drops its answer, as a panicking
        // worker drops it. Both happen in one update, so the real answer cannot
        // land in between.
        cx.update_window(window, |_, window, cx| {
            panel.update(cx, |panel, cx| panel.add(window, cx));
            let request = panel.read(cx).pending.expect("Add waits for its answer");
            engine.update(cx, |engine, cx| engine.folder_add_lost(request, cx));
        })
        .expect("add and then lose the answer");

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                panel.read(cx).pending.is_none(),
                "#40: a lost answer ends the wait instead of stranding the dialog"
            );
            assert_eq!(
                window.find("folder-panel-problem").label(),
                Some(WORKER_UNAVAILABLE),
                "#40: the dialog shows why nothing was written"
            );
            assert_eq!(
                engine.read(cx).problem(),
                None,
                "#40: the reason belongs to the dialog, not the engine"
            );
        })
        .expect("read the dialog after the lost answer");
    }
}
