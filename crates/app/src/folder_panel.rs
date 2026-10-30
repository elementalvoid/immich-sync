//! The folder panel: one source folder's settings, confirmed before it syncs
//! (#40) and opened as its live detail (#43).
//!
//! One panel serves both: the draft is the setup dialog, and the detail is the
//! same card in live mode. The title, the path block, the three settings
//! controls, the modal host, the focus trap, and Escape are shared, so the two
//! modes cannot drift apart. The dashboard owns which panel is open: it creates
//! one when the add field, Browse, a card, or its Details control asks for it.
//!
//! In detail mode the panel reads the folder's live status from the engine on
//! every engine notification, so counts, state, progress, the last error, and
//! the two timestamps follow the client while the dialog stays open. Its uploaded
//! assets are read a page at a time through the state store (#44), on the
//! background executor: the UI thread never waits behind the database.

use std::path::Path;
use std::sync::Arc;

use gpui_kit::base::Disableable as _;
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::base::SelectableText;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Theme;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{
    DialogAction, DialogClose, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sync_core::meter::UploadTraffic;
use sync_core::store::{AlbumScope, FolderAsset, SourceFolder, StoreError};

use crate::asset_link::{self, LinkOpener};
use crate::dashboard::{CardPalette, state_tag};
use crate::diagnostics::format_bytes;
use crate::engine::EngineModel;
use crate::folder_controls::{SettingsControls, SettingsIds, SettingsValues};
use crate::notice::{Notice, Severity};
use crate::shell::ViewMode;
use crate::status::{ActivityState, StatusCard, folder_name};
use crate::times;

/// The reason the setup dialog shows when the folder is already tracked (#40).
pub(crate) const ALREADY_ADDED: &str = "Already added.";

/// How many of a folder's uploaded assets one page of the detail's list holds
/// (#44): the first page, every Load more, and the floor a live refresh keeps.
const ASSET_PAGE: usize = 50;

/// The action "Open folder" runs (#43).
///
/// The product opens the platform's file manager; a test injects a recorder, so
/// the path can be asserted without launching anything.
pub type FolderOpener = Arc<dyn Fn(&str) -> std::io::Result<()>>;

/// The program "Open folder" launches for this platform (R8.1 folder detail,
/// #43).
///
/// The one place the platform picks it: Windows has no `open`, and macOS has no
/// `xdg-open`. `cfg!` keeps every name in the same function, so the test can
/// read the table on any host.
#[must_use]
pub(crate) fn file_manager_command() -> &'static str {
    if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// Opens `path` in the platform's file manager (R8.1 folder detail, #43).
///
/// The child is reaped on a thread of its own: the dialog must not wait for a
/// file manager the user may keep open, and a client that runs for days must not
/// collect one zombie per launch.
///
/// # Errors
///
/// Returns the platform's error when the file manager cannot be started.
pub fn open_in_file_manager(path: &str) -> std::io::Result<()> {
    let mut child = std::process::Command::new(file_manager_command())
        .arg(path)
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

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
pub(crate) fn confirmed_folder(draft: &SourceFolder) -> Result<SourceFolder, String> {
    let path = draft.path.trim();
    if path.is_empty() {
        return Err("Enter the path of a folder to add.".to_string());
    }
    if let Some(problem) = path_problem(probe(Path::new(path))) {
        return Err(problem.to_string());
    }
    Ok(SourceFolder {
        path: path.to_string(),
        // #47: a blank field means the folder's name, which the placeholder
        // showed.
        album_title: title_from(draft.album_title.as_deref().unwrap_or_default()),
        ..draft.clone()
    })
}

/// Which lifecycle the panel shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderPanelMode {
    /// A folder that is not tracked yet: Add records the draft and Cancel
    /// discards it (#40).
    Draft,
    /// A folder that is already tracked: its live detail (#43).
    Detail,
}

/// What the panel asks its owner to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderPanelEvent {
    /// The folder was written: close the panel and show its card.
    Added,
    /// The user cancelled: close the panel; nothing was written.
    Cancelled,
    /// The detail closed, or its Remove completed: close the panel and return
    /// focus to the card.
    Closed,
    /// Open the failed-items or the deletion-review view on this folder
    /// (#41, #43).
    Open(ViewMode, String),
}

/// The stored form of the album title field: blank means the folder's name
/// (#47).
fn title_from(value: &str) -> Option<String> {
    let title = value.trim();
    (!title.is_empty()).then(|| title.to_string())
}

/// The album title input (#47).
///
/// It starts on the stored title, or blank when the album takes the folder's
/// name, and the folder's name is the placeholder either way.
fn title_field(
    path: &str,
    title: Option<&str>,
    window: &mut Window,
    cx: &mut Context<FolderPanel>,
) -> Entity<InputState> {
    let placeholder = if path.is_empty() {
        "Album title".to_string()
    } else {
        folder_name(path)
    };
    let stored = title.unwrap_or_default().to_string();
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(stored)
    })
}

/// Saves the title when Enter is pressed in the field (#47).
///
/// A draft has nothing to save yet, so its subscription does nothing until the
/// same panel is in detail mode.
fn subscribe_title(
    input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut Context<FolderPanel>,
) -> Subscription {
    cx.subscribe_in(input, window, |this, _, event: &InputEvent, _, cx| {
        if matches!(event, InputEvent::PressEnter { .. }) {
            this.save_album_title(cx);
        }
    })
}

/// The folder panel: a draft's settings or one folder's live detail, inside the
/// focus-trapped modal host.
pub struct FolderPanel {
    engine: Entity<EngineModel>,
    mode: FolderPanelMode,
    /// The draft: the path plus the three settings. In detail mode the path is
    /// the folder's identity and the settings are the live values, so one
    /// renderer serves both.
    draft: SourceFolder,
    /// Why the last action failed. The panel stays open while it is set.
    problem: Option<String>,
    /// The engine request a draft Add waits for, so a slower earlier result
    /// cannot answer a newer dialog (#40).
    pending: Option<u64>,
    /// The folder's live card, in detail mode (#43).
    card: Option<StatusCard>,
    /// Whether the folder is no longer a source folder while the detail is
    /// open. Its last known settings stay usable.
    gone: bool,
    /// Whether the Remove confirmation alert is up, so the body dims behind
    /// it (#43, #73).
    confirming: bool,
    /// Whether the removal is in flight: the dialog closes when the folder
    /// leaves the status, so a failed write keeps it open.
    removing: bool,
    /// The removal request this dialog waits for, so a status poll that lands
    /// while the write is in flight is not read as its answer (#43).
    remove_request: Option<u64>,
    /// The album title field (#47). It is the editing surface in both modes, so
    /// an engine refresh cannot type over what the user is entering.
    title_input: Entity<InputState>,
    /// What "Open folder" runs (#43).
    opener: FolderOpener,
    /// The configured server URL the asset links are built from (#44).
    server_url: String,
    /// What an asset link opens (#44): the default browser in the product, a
    /// recorder in a test.
    link_opener: LinkOpener,
    /// One page of the folder's current assets, newest first (#44).
    assets: Vec<FolderAsset>,
    /// Whether the last page came back full, so there may be another one.
    assets_more: bool,
    /// Why the last asset read failed, if it did. It replaces the list.
    assets_problem: Option<String>,
    /// Whether an asset read is in flight, so the list is not called empty
    /// before it has answered. One read at a time: a page that is in flight
    /// keeps every other read waiting, so no answer can overtake another.
    assets_loading: bool,
    /// The folder's uploaded count and recursion setting the loaded rows were
    /// read at, so the next status poll that changes either reloads them.
    assets_read_at: Option<(u32, bool)>,
    focus: FocusHandle,
    _subscription: Subscription,
    _title_subscription: Subscription,
}

impl EventEmitter<FolderPanelEvent> for FolderPanel {}

impl FolderPanel {
    /// Opens the setup dialog for `path` (#40, #47).
    ///
    /// The album title starts blank, with the folder's name as its placeholder,
    /// so the common case is no typing at all.
    pub fn setup(
        engine: Entity<EngineModel>,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let subscription = cx.observe(&engine, |this, engine, cx| this.engine_changed(&engine, cx));
        let path = path.trim().to_string();
        let title_input = title_field(&path, None, window, cx);
        let title_subscription = subscribe_title(&title_input, window, cx);
        Self {
            engine,
            mode: FolderPanelMode::Draft,
            draft: SourceFolder {
                path,
                recursive: true,
                album_mapping: false,
                album_scope: AlbumScope::Leaf,
                album_title: None,
            },
            problem: None,
            pending: None,
            card: None,
            gone: false,
            confirming: false,
            removing: false,
            remove_request: None,
            title_input,
            opener: Arc::new(open_in_file_manager),
            server_url: String::new(),
            link_opener: asset_link::browser_opener(),
            assets: Vec::new(),
            assets_more: false,
            assets_problem: None,
            assets_loading: false,
            assets_read_at: None,
            focus,
            _subscription: subscription,
            _title_subscription: title_subscription,
        }
    }

    /// Opens the live detail for one tracked folder (#43).
    ///
    /// The card the dashboard clicked is the first frame's data; every later
    /// engine notification refreshes it, so the dialog follows the client
    /// without a poll of its own. The first page of the folder's uploaded
    /// assets is read straight away, off the UI thread (#44).
    pub fn detail(
        engine: Entity<EngineModel>,
        path: String,
        opener: FolderOpener,
        link_opener: LinkOpener,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let subscription = cx.observe(&engine, |this, engine, cx| this.engine_changed(&engine, cx));
        let card = folder_card(&engine, &path, cx);
        let server_url = engine.read(cx).server_url(cx);
        let draft = SourceFolder {
            path: path.clone(),
            recursive: card.as_ref().is_some_and(|card| card.recursive),
            album_mapping: card.as_ref().is_some_and(|card| card.album_mapping),
            album_scope: card
                .as_ref()
                .map_or(AlbumScope::Leaf, |card| card.album_scope),
            album_title: card.as_ref().and_then(|card| card.album_title.clone()),
        };
        let stored_title = draft.album_title.clone();
        let title_input = title_field(&path, stored_title.as_deref(), window, cx);
        let title_subscription = subscribe_title(&title_input, window, cx);
        let uploaded = card.as_ref().map_or(0, |card| card.counts.uploaded);
        let mut panel = Self {
            engine,
            mode: FolderPanelMode::Detail,
            draft,
            problem: None,
            pending: None,
            card,
            gone: false,
            confirming: false,
            removing: false,
            remove_request: None,
            title_input,
            opener,
            server_url,
            link_opener,
            assets: Vec::new(),
            assets_more: false,
            assets_problem: None,
            assets_loading: false,
            assets_read_at: None,
            focus,
            _subscription: subscription,
            _title_subscription: title_subscription,
        };
        panel.read_assets(0, ASSET_PAGE, uploaded, cx);
        panel
    }

    /// The folder's live card, when the engine's status holds it.
    fn refresh_detail(&mut self, engine: &Entity<EngineModel>, cx: &mut Context<Self>) {
        // #44: a URL corrected while the detail is open reaches the links it
        // has not drawn yet, so the dialog need not be reopened.
        self.server_url = engine.read(cx).server_url(cx);
        match folder_card(engine, &self.draft.path, cx) {
            Some(card) => {
                // #43: the live values are the card's, so the settings the
                // dialog shows are the ones the store holds.
                self.draft.recursive = card.recursive;
                self.draft.album_mapping = card.album_mapping;
                self.draft.album_scope = card.album_scope;
                self.draft.album_title = card.album_title.clone();
                self.card = Some(card);
                self.gone = false;
            }
            None => {
                // The row left the status: the last known settings stay
                // usable, and Open folder is disabled.
                self.gone = true;
                if let Some(card) = self.card.as_mut() {
                    card.available = false;
                }
            }
        }
    }

    /// Applies the newest engine result.
    fn engine_changed(&mut self, engine: &Entity<EngineModel>, cx: &mut Context<Self>) {
        match self.mode {
            FolderPanelMode::Draft => self.add_result(engine, cx),
            FolderPanelMode::Detail => {
                self.refresh_detail(engine, cx);
                // #44: a status poll is the list's only clock. A folder whose
                // uploaded count or recursion setting moved reloads the rows it
                // has loaded; nothing else touches the store.
                self.reload_assets_if_changed(cx);
                // #43: a completed Remove closes the dialog once the store no
                // longer holds the folder, so a refused write keeps it open.
                if self.removing && self.gone {
                    cx.emit(FolderPanelEvent::Closed);
                    return;
                }
                if self.removing {
                    self.apply_remove_result(engine, cx);
                }
                cx.notify();
            }
        }
    }

    /// Reads one page of this folder's current assets (#44).
    ///
    /// The read runs on the background executor, never the UI thread. One
    /// read is in flight at a time, so its answer needs no guard against a
    /// newer one. Nothing is written: paging is two reads of the same list.
    fn read_assets(&mut self, offset: usize, count: usize, uploaded: u32, cx: &mut Context<Self>) {
        let Some(store) = self.engine.read(cx).store(cx) else {
            return;
        };
        let folder = self.draft.path.clone();
        let recursive = self.draft.recursive;
        self.assets_loading = true;
        self.assets_read_at = Some((uploaded, recursive));
        let read = cx
            .background_executor()
            .spawn(async move { store.folder_assets(&folder, recursive, count, offset) });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |panel, cx| {
                panel.apply_assets(offset, count, result, cx);
            });
        })
        .detach();
    }

    /// Applies one asset page.
    fn apply_assets(
        &mut self,
        offset: usize,
        count: usize,
        result: Result<Vec<FolderAsset>, StoreError>,
        cx: &mut Context<Self>,
    ) {
        self.assets_loading = false;
        match result {
            Ok(rows) => {
                // A full page may have a successor; a short one is the end.
                self.assets_more = rows.len() == count;
                if offset == 0 {
                    self.assets = rows;
                } else {
                    self.assets.extend(rows);
                }
                self.assets_problem = None;
            }
            Err(error) => {
                // #44: a read error replaces the list rather than leaving rows
                // that no longer describe the folder.
                self.assets.clear();
                self.assets_more = false;
                self.assets_problem = Some(format!(
                    "The folder's uploaded assets could not be read: {error}"
                ));
            }
        }
        // A count that moved while this read ran reloads the rows it kept.
        self.reload_assets_if_changed(cx);
        cx.notify();
    }

    /// Reloads the loaded rows when the folder's uploaded count or recursion
    /// setting changed under them (#44).
    ///
    /// One read at a time: while a page is in flight the refresh waits for it,
    /// and the read that lands re-checks, so a busy sync converges instead of
    /// queueing reads.
    fn reload_assets_if_changed(&mut self, cx: &mut Context<Self>) {
        if self.mode != FolderPanelMode::Detail || self.gone || self.assets_loading {
            return;
        }
        let Some(uploaded) = self.card.as_ref().map(|card| card.counts.uploaded) else {
            return;
        };
        if self.assets_read_at == Some((uploaded, self.draft.recursive)) {
            return;
        }
        // Keep the rows the user already loaded; Load more is what grows it.
        let count = self.assets.len().max(ASSET_PAGE);
        self.read_assets(0, count, uploaded, cx);
    }

    /// Load more: appends the next page without losing the loaded rows (#44).
    fn load_more_assets(&mut self, cx: &mut Context<Self>) {
        if self.mode != FolderPanelMode::Detail || self.assets_loading {
            return;
        }
        let uploaded = self.card.as_ref().map_or(0, |card| card.counts.uploaded);
        self.read_assets(self.assets.len(), ASSET_PAGE, uploaded, cx);
        cx.notify();
    }

    /// Opens one asset's Immich page in the default browser (#44).
    ///
    /// Only a web address reaches the opener, so a hand-edited settings file
    /// cannot make the client launch anything else.
    fn open_asset(&mut self, url: String, cx: &mut Context<Self>) {
        asset_link::open_asset(&url, &self.link_opener, cx);
    }

    /// Applies the newest draft Add result when it answers this panel's request.
    fn add_result(&mut self, engine: &Entity<EngineModel>, cx: &mut Context<Self>) {
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
        if self.mode != FolderPanelMode::Draft || self.pending.is_some() {
            return;
        }
        self.problem = None;
        let mut draft = self.draft.clone();
        draft.album_title = title_from(self.title_input.read(cx).value().as_str());
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

    /// In detail mode, writes one setting immediately through the engine
    /// model, the same request the folder's card makes (#43, R2.1, R3.4).
    fn write_recursive(&mut self, recursive: bool, cx: &mut Context<Self>) {
        let path = self.draft.path.clone();
        self.engine
            .clone()
            .update(cx, |model, cx| model.add_folder(path, recursive, cx));
    }

    fn write_album_mapping(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let path = self.draft.path.clone();
        self.engine
            .clone()
            .update(cx, |model, cx| model.set_album_mapping(path, enabled, cx));
    }

    fn write_album_scope(&mut self, scope: AlbumScope, cx: &mut Context<Self>) {
        let path = self.draft.path.clone();
        self.engine
            .clone()
            .update(cx, |model, cx| model.set_album_scope(path, scope, cx));
    }

    /// Saves the album title the detail's field holds (#47).
    ///
    /// The engine stores the title and an album that already exists is renamed
    /// by its retry, so this reports nothing: a rename that cannot reach the
    /// server is a pending state the dialog shows, not a lost edit.
    fn save_album_title(&mut self, cx: &mut Context<Self>) {
        if self.mode != FolderPanelMode::Detail {
            return;
        }
        let path = self.draft.path.clone();
        let title = self.title_input.read(cx).value().to_string();
        self.engine
            .clone()
            .update(cx, |model, cx| model.set_album_title(path, title, cx));
    }

    /// Opens the stored path in the platform's file manager (#43).
    ///
    /// A file manager that cannot start is the dialog's problem to show, the
    /// same way a refused Add is.
    fn open_folder(&mut self, cx: &mut Context<Self>) {
        self.problem = match (self.opener)(&self.draft.path) {
            Ok(()) => None,
            Err(error) => Some(format!("The folder could not be opened: {error}")),
        };
        cx.notify();
    }

    /// Applies this dialog's own removal answer (#43).
    ///
    /// The engine answers the request that asked, not the newest status, so a
    /// status poll that lands while the write is in flight cannot be mistaken
    /// for the refusal. A refused Remove would otherwise leave `removing` set
    /// forever: the button would stay disabled and say nothing.
    fn apply_remove_result(&mut self, engine: &Entity<EngineModel>, cx: &Context<Self>) {
        let Some(request) = self.remove_request else {
            return;
        };
        let Some(result) = engine.read(cx).folder_remove_result() else {
            return;
        };
        if result.request != request {
            return;
        }
        self.problem = result.problem.clone();
        self.remove_request = None;
        self.removing = false;
    }

    /// The first Remove: opens the confirmation alert and dims the dialog
    /// behind it (#43, #73).
    ///
    /// The alert is a window-level layer, so the answer is on screen whatever
    /// the detail's body scrolls to. The layer may rebuild the builder, so it
    /// captures the name and a handle instead of reading the panel.
    fn ask_remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirming = true;
        cx.notify();
        let title = self.folder_name();
        let panel = cx.entity().downgrade();
        let cancelled = panel.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            // The layer may rebuild the builder, so each run gets its own
            // copies of what the callbacks and the content move into place.
            let title = title.clone();
            let cancelled = cancelled.clone();
            let removed = panel.clone();
            alert
                // The buttons go in the dialog's own footer: an alert builds
                // its default OK/Cancel row only when no footer is set, so a
                // footer supplied through `content` rendered both (#73).
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new().child(
                                Button::new("folder-detail-remove-cancel")
                                    .flex_1()
                                    .outline()
                                    .label("Cancel")
                                    .accessibility_label("Keep the folder"),
                            ),
                        )
                        .child(
                            DialogAction::new().child(
                                Button::new("folder-detail-remove-confirm")
                                    .flex_1()
                                    .danger()
                                    .label("Remove folder")
                                    .accessibility_label(format!("Remove {title} now")),
                            ),
                        ),
                )
                .content(move |content, _, _| {
                    content.child(
                        div()
                            .id("folder-detail-confirm")
                            .test_support()
                            .role(Role::Alert)
                            .aria_label(format!("Remove {title}?"))
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                DialogHeader::new()
                                    .child(
                                        DialogTitle::new()
                                            .child(format!("Remove “{title}” from Immich Sync?")),
                                    )
                                    .child(DialogDescription::new().child(
                                        "Its uploaded assets stay on the server. The client \
                                         stops syncing this folder and forgets the items it \
                                         tracked.",
                                    )),
                            ),
                    )
                })
                .on_cancel(move |_, _, cx| {
                    // The panel can be gone already: closing the detail closes this
                    // alert with it, so there is nothing left to undim (#73).
                    let _ = cancelled.update(cx, |panel, cx| panel.confirmation_closed(cx));
                    true
                })
                .on_ok(move |_, _, cx| {
                    let _ = removed.update(cx, |panel, cx| panel.remove_confirmed(cx));
                    true
                })
        });
    }

    /// The second Remove: asks the engine to drop the folder and waits for the
    /// status to lose it, so a refused write leaves the dialog open (#43).
    fn remove_confirmed(&mut self, cx: &mut Context<Self>) {
        if self.removing {
            return;
        }
        self.removing = true;
        self.confirming = false;
        let path = self.draft.path.clone();
        let engine = self.engine.clone();
        self.remove_request = Some(engine.update(cx, |model, cx| model.remove_folder(path, cx)));
        // A failed worker reports synchronously, before the request was
        // recorded; read the result back so that failure cannot be missed.
        self.apply_remove_result(&engine, cx);
        cx.notify();
    }

    /// The confirmation alert is gone: the body stops dimming (#73).
    ///
    /// Focus returns to the panel by itself: the alert layer puts it back on
    /// the control it took it from.
    fn confirmation_closed(&mut self, cx: &mut Context<Self>) {
        self.confirming = false;
        cx.notify();
    }

    /// Escape, the backdrop, and the close control, each respecting what may
    /// still be in flight.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        match self.mode {
            FolderPanelMode::Draft => self.cancel(cx),
            FolderPanelMode::Detail => cx.emit(FolderPanelEvent::Closed),
        }
    }

    /// Escape inside the dialog (#43). While the confirmation alert is up,
    /// focus is in that layer and its own Escape closes only the alert (#73).
    fn escape(&mut self, cx: &mut Context<Self>) {
        self.dismiss(cx);
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

    /// The name the dialog announces (N5).
    fn aria_label(&self) -> &'static str {
        match self.mode {
            FolderPanelMode::Draft => "Folder setup",
            FolderPanelMode::Detail => "Folder detail",
        }
    }

    /// The visible heading the setup dialog gives the panel; the detail's
    /// heading is the folder's own name (#43).
    fn draft_title(&self) -> &'static str {
        "Add a source folder"
    }

    /// The name the path's last component gives the folder.
    fn folder_name(&self) -> String {
        self.card
            .as_ref()
            .map_or_else(|| folder_name(&self.draft.path), |card| card.title.clone())
    }

    /// The path block: the whole path, wrapped and selectable (#43).
    fn path(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let shown = if self.draft.path.is_empty() {
            "No path yet.".to_string()
        } else {
            self.draft.path.clone()
        };
        let muted = cx.theme().muted_foreground;
        let selectable = self.mode == FolderPanelMode::Detail;
        div()
            .id("folder-panel-path")
            .test_support()
            .aria_label(format!("Folder path: {shown}"))
            .min_w_0()
            .w_full()
            .text_sm()
            .text_color(muted)
            .when(!selectable, |this| this.truncate())
            // The detail shows the path whole: a long one wraps instead of
            // being cut, and it can be selected and copied.
            .when(selectable, |this| {
                this.child(
                    SelectableText::new("folder-panel-path-text", shown.clone()).document_order(0),
                )
            })
            .when(!selectable, |this| this.child(shown))
    }

    /// The album title field (#47): the user's own name for this folder's
    /// album, blank while the album takes the folder's name.
    ///
    /// The placeholder is the folder's name, so a blank field says what the
    /// album will be called without the user typing it. In detail mode the row
    /// saves the title; the setup dialog's Add carries it with the rest of the
    /// draft.
    fn album_title_field(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let detail = self.mode == FolderPanelMode::Detail;
        let name = self.folder_name();
        let focus = self.title_input.read(cx).focus_handle(cx);
        let row = div()
            .id("folder-panel-title")
            .test_support()
            .track_focus(&focus)
            .aria_label(format!("Album title for {name}"))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Album title"),
            )
            .child(div().flex_1().min_w_0().child(
                Input::new(&self.title_input).aria_label(format!("Album title for {name}")),
            ));
        row.when(detail, |this| {
            this.child(
                Button::new("folder-panel-title-save")
                    .secondary()
                    .label("Save title")
                    .accessibility_label(format!("Save the album title for {name}"))
                    .on_click(cx.listener(|this, _, _, cx| this.save_album_title(cx))),
            )
        })
        .into_any_element()
    }

    /// The three settings controls, shared with the folder cards (#43).
    fn settings(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let values = SettingsValues {
            recursive: self.draft.recursive,
            album_mapping: self.draft.album_mapping,
            album_scope: self.draft.album_scope,
        };
        let name = match self.mode {
            FolderPanelMode::Draft => "this folder".to_string(),
            FolderPanelMode::Detail => self.folder_name(),
        };
        let detail = self.mode == FolderPanelMode::Detail;
        SettingsControls::new(
            SettingsIds {
                recursive: "folder-panel-recursive".into(),
                album_mapping: "folder-panel-album".into(),
                album_leaf: "folder-panel-album-leaf".into(),
                album_whole: "folder-panel-album-whole".into(),
            },
            name,
            values,
            cx.listener(move |this, recursive: &bool, _, cx| {
                if detail {
                    this.write_recursive(*recursive, cx);
                } else {
                    this.draft.recursive = *recursive;
                    cx.notify();
                }
            }),
            cx.listener(move |this, enabled: &bool, _, cx| {
                if detail {
                    this.write_album_mapping(*enabled, cx);
                } else {
                    this.draft.album_mapping = *enabled;
                    cx.notify();
                }
            }),
            cx.listener(move |this, scope: &AlbumScope, _, cx| {
                if detail {
                    this.write_album_scope(*scope, cx);
                } else {
                    this.draft.album_scope = *scope;
                    cx.notify();
                }
            }),
        )
        .render(cx.theme().muted_foreground)
    }

    /// The draft footer: Cancel writes nothing, Add records the four values.
    fn draft_actions(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
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

    /// The setup dialog's body: title, path, settings, and the Add footer.
    fn draft_body(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(self.draft_title()),
            )
            .child(self.path(cx))
            .child(self.album_title_field(theme, cx))
            .child(self.settings(cx))
            .when_some(self.problem.clone(), |this, problem| {
                this.child(Notice::new(
                    "folder-panel-problem",
                    Severity::Error,
                    problem,
                ))
            })
            .child(self.draft_actions(cx))
            .into_any_element()
    }

    /// The detail body: the live state, the settings that write immediately,
    /// the timestamps, the links into the filtered views, and Remove (#43).
    fn detail_body(
        &self,
        palette: &CardPalette,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = theme.muted_foreground;
        let available = !self.gone && self.card.as_ref().is_some_and(|card| card.available);
        let activity = match &self.card {
            Some(card) if card.available => card.activity,
            // A folder that is no longer readable is a folder needing the user
            // (R2.5), and one that left the list reads the same way.
            _ => ActivityState::NeedsAttention,
        };
        let progress = self.card.as_ref().map_or(0.0, |card| card.progress);
        let (uploaded, pending, failed, review) = self.card.as_ref().map_or((0, 0, 0, 0), |card| {
            (
                card.counts.uploaded,
                card.counts.pending,
                card.counts.failed,
                card.review_count,
            )
        });
        let tracked = uploaded + pending + failed + review;
        let last_error = self.card.as_ref().and_then(|card| card.last_error.clone());
        let now_ns = times::now_ns();
        let upload_line = times::moment_line(
            "Last successful upload",
            self.card.as_ref().and_then(|card| card.last_upload_ns),
            now_ns,
        );
        let scan_line = times::scan_line(
            self.card.as_ref().and_then(|card| card.last_scan_ns),
            now_ns,
        );
        // #44: this folder's own share of the traffic, in the global card's
        // own words. It rides the 250 ms status poll, never a timer of its own.
        // A folder that left the status has no live rate, so "Folder
        // unavailable" is never paired with one.
        let activity_line = crate::status::upload_line(
            self.card
                .as_ref()
                .filter(|card| card.available)
                .map_or(UploadTraffic::default(), |card| card.uploads),
        );
        let title = self.folder_name();
        let path = self.draft.path.clone();
        let counts = format!(
            "{uploaded} uploaded · {pending} pending · {failed} failed · {review} to review · \
             {tracked} tracked"
        );
        let percent = format!("{}%", (progress.clamp(0.0, 1.0) * 100.0).round() as u32);

        // #43: while the Remove confirmation shows, the normal body dims. The
        // opacity sits on this inner wrapper, never on the whole body, so the
        // answer is never dimmed with the panel (#73 moved the answer into its
        // own window-level layer). Opacity is not one of the things the test
        // harness observes, so no test pins this; the flag is pinned by the
        // alert opening and closing.
        let inner = div()
            .id("folder-detail-body-dim")
            .test_support()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .when(self.confirming, |this| this.opacity(0.4))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .id("folder-detail-title")
                            .test_support()
                            .aria_label(title.clone())
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title.clone()),
                    )
                    .child(
                        Button::new("folder-detail-close")
                            .ghost()
                            .label("✕")
                            .accessibility_label("Close the folder detail")
                            .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx))),
                    ),
            )
            .child(self.path(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .id("folder-detail-state")
                            .test_support()
                            .aria_label(activity.label().to_string())
                            .child(state_tag(
                                activity,
                                activity.label().to_string(),
                                palette,
                                theme,
                            )),
                    )
                    .child(
                        Progress::new("folder-detail-progress")
                            .flex_1()
                            .value(progress.clamp(0.0, 1.0) * 100.0)
                            .accessibility_label(format!("{title} progress")),
                    )
                    .child(
                        div()
                            .id("folder-detail-percent")
                            .test_support()
                            .aria_label(percent.clone())
                            .text_sm()
                            .text_color(muted)
                            .child(percent),
                    ),
            )
            .child(
                div()
                    .id("folder-detail-counts")
                    .test_support()
                    .aria_label(counts.clone())
                    .text_sm()
                    .text_color(muted)
                    .child(counts),
            )
            .child(
                div()
                    .id("folder-detail-activity")
                    .test_support()
                    .aria_label(activity_line.clone())
                    .text_sm()
                    .text_color(muted)
                    .child(activity_line),
            )
            .when(!available, |this| {
                this.child(Notice::new(
                    "folder-detail-unavailable",
                    Severity::Warning,
                    format!("Folder unavailable: {path}"),
                ))
            })
            .child(self.album_title_field(theme, cx))
            .when_some(
                self.card
                    .as_ref()
                    .and_then(|card| card.album_rename.clone()),
                |this, rename| {
                    // #47: the field keeps the desired title, and this says why
                    // the server does not hold it yet.
                    this.child(Notice::new(
                        "folder-detail-album-rename",
                        Severity::Warning,
                        format!("Album title pending: {}.", rename.reason()),
                    ))
                },
            )
            .child(self.settings(cx))
            .when_some(last_error, |this, error| {
                this.child(Notice::new(
                    "folder-detail-error",
                    Severity::Error,
                    format!("Last error: {error}"),
                ))
            })
            .child(
                div()
                    .id("folder-detail-upload")
                    .test_support()
                    .aria_label(upload_line.clone())
                    .text_sm()
                    .text_color(muted)
                    .child(upload_line),
            )
            .child(
                div()
                    .id("folder-detail-scan")
                    .test_support()
                    .aria_label(scan_line.clone())
                    .text_sm()
                    .text_color(muted)
                    .child(scan_line),
            )
            .child(self.assets_section(palette, theme, cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("folder-detail-open")
                            .secondary()
                            .disabled(!available)
                            .label("Open folder")
                            .accessibility_label(format!("Open {path} in the file manager"))
                            .on_click(cx.listener(|this, _, _, cx| this.open_folder(cx))),
                    )
                    .child(
                        Button::new("folder-detail-failed")
                            .ghost()
                            .label(format!("{failed} failed"))
                            .accessibility_label(format!("View {failed} failed items in {title}"))
                            .on_click(cx.listener({
                                let position = path.clone();
                                move |_, _, _, cx| {
                                    cx.emit(FolderPanelEvent::Open(
                                        ViewMode::Failed,
                                        position.clone(),
                                    ));
                                }
                            })),
                    )
                    .child(
                        Button::new("folder-detail-review")
                            .ghost()
                            .label(format!("{review} to review"))
                            .accessibility_label(format!("View {review} missing files in {title}"))
                            .on_click(cx.listener({
                                let position = path.clone();
                                move |_, _, _, cx| {
                                    cx.emit(FolderPanelEvent::Open(
                                        ViewMode::Review,
                                        position.clone(),
                                    ));
                                }
                            })),
                    )
                    .child(
                        Button::new("folder-detail-remove")
                            .danger()
                            .disabled(self.removing)
                            .label(if self.removing {
                                "Removing…"
                            } else {
                                "Remove"
                            })
                            .accessibility_label(format!("Remove {title}"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.ask_remove(window, cx);
                            })),
                    ),
            );

        div()
            .id("folder-detail-body")
            .test_support()
            .flex()
            .flex_col()
            .gap_3()
            .w_full()
            .child(inner)
            // The problem notice is the dialog's answer, not body content: it
            // stays outside the dimmed wrapper, so a refused Remove is readable
            // while the confirmation is up.
            .when_some(self.problem.clone(), |this, problem| {
                this.child(Notice::new(
                    "folder-panel-problem",
                    Severity::Error,
                    problem,
                ))
            })
            .into_any_element()
    }

    /// The folder's uploaded assets: one row per current asset, newest first,
    /// with the states that are not a list (#44).
    ///
    /// The rows are the pages the panel already read; this only renders them,
    /// so a dialog left open never reads the store from the render path.
    fn assets_section(
        &self,
        palette: &CardPalette,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let now_ns = times::now_ns();
        let heading = div()
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(palette.text)
            .child("Uploaded assets");
        let state = if let Some(problem) = self.assets_problem.clone() {
            Notice::new("folder-assets-problem", Severity::Error, problem).into_any_element()
        } else if self.assets.is_empty() {
            let (id, text) = if self.assets_loading {
                ("folder-assets-loading", "Reading the folder's assets…")
            } else {
                (
                    "folder-assets-empty",
                    "No uploaded assets in this folder yet.",
                )
            };
            div()
                .id(id)
                .test_support()
                .aria_label(text)
                .text_xs()
                .text_color(palette.meta)
                .child(text)
                .into_any_element()
        } else {
            // The list scrolls inside the dialog, so a folder with hundreds of
            // assets does not push Load more or the actions out of reach.
            let mut rows = div()
                .id("folder-assets-list")
                .test_support()
                .flex()
                .flex_col()
                .w_full()
                .max_h(rems(18.))
                .overflow_y_scroll();
            for (index, asset) in self.assets.iter().enumerate() {
                rows = rows.child(self.asset_row(index, asset, now_ns, theme, cx));
            }
            rows.into_any_element()
        };

        div()
            .id("folder-detail-assets")
            .test_support()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(heading)
            .child(state)
            .when(self.assets_more, |this| {
                this.child(
                    Button::new("folder-assets-more")
                        .secondary()
                        .label("Load more")
                        .accessibility_label(format!("Load the next {ASSET_PAGE} uploaded assets"))
                        .on_click(cx.listener(|this, _, _, cx| this.load_more_assets(cx))),
                )
            })
    }

    /// One uploaded asset: its name, size, upload time, and Immich link (#44).
    fn asset_row(
        &self,
        index: usize,
        asset: &FolderAsset,
        now_ns: i64,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = folder_name(&asset.path);
        let size = format_bytes(asset.size);
        let when = times::moment(asset.created_at_ns, now_ns);
        let described = format!("{name} · {size} · uploaded {when}");
        // #44: no configured server URL hides the link rather than opening a
        // wrong one; the row stays readable either way.
        let url = asset_link::asset_url(&self.server_url, &asset.asset_id);
        // Two lines: the file's name gets the row's width, and the metadata
        // shares the second line with the link. One line squeezed the name to
        // an ellipsis, which is the one thing the row is for.
        div()
            .id(("folder-asset", index))
            .test_support()
            .aria_label(described)
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(name.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .w_full()
                    .child(
                        div()
                            .min_w_0()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("{size} · uploaded {when}")),
                    )
                    .when_some(url, |this, url| {
                        this.child(
                            Button::new(("folder-asset-link", index))
                                .ghost()
                                .label("View in Immich")
                                .accessibility_label(format!("Open {name} in Immich"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_asset(url.clone(), cx);
                                })),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The modal host: centered over a backdrop, focus trapped, Escape cancels.
    ///
    /// The backdrop is the host's sibling rather than an ancestor of the card,
    /// so a click on the dialog never reaches it (#43).
    fn modal(&self, card: AnyElement, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let focus = self.focus.clone();
        let viewport = window.viewport_size();
        let backdrop = div()
            .id("folder-panel-backdrop")
            .test_support()
            .absolute()
            .top_0()
            .left_0()
            .w(viewport.width)
            .h(viewport.height)
            .bg(theme.background.opacity(0.75))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.dismiss(cx);
                    cx.stop_propagation();
                }),
            );
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
                    .track_focus(&focus)
                    .focus_trap("folder-panel-trap", &focus)
                    // The host swallows pointer and wheel events, so the
                    // dashboard behind the modal stays inert.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_scroll(|_, _, cx| cx.stop_propagation())
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "escape" {
                            this.escape(cx);
                        }
                    }))
                    .child(backdrop)
                    .child(card),
            ),
        )
        .with_priority(10)
        .into_any_element()
    }
}

/// The live card the engine's status holds for `path`, when it holds one.
fn folder_card(
    engine: &Entity<EngineModel>,
    path: &str,
    cx: &mut Context<FolderPanel>,
) -> Option<StatusCard> {
    engine
        .read(cx)
        .dashboard_model(cx)
        .folders
        .into_iter()
        .find(|card| card.path.as_deref() == Some(path))
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
        let viewport = window.viewport_size();
        // The detail carries more than the setup, so it gets the wider card; a
        // short window scrolls the body instead of clipping its last control.
        let width = match self.mode {
            FolderPanelMode::Draft => rems(30.),
            FolderPanelMode::Detail => rems(34.),
        };
        let body = match self.mode {
            FolderPanelMode::Draft => self.draft_body(&theme, cx),
            FolderPanelMode::Detail => self.detail_body(&palette, &theme, cx),
        };
        let card = div()
            .id("folder-panel")
            .test_support()
            .role(Role::Dialog)
            .aria_label(self.aria_label())
            .flex()
            .flex_col()
            .gap_3()
            .w(width)
            .max_h(viewport.height - px(96.))
            .overflow_y_scroll()
            .p_5()
            .rounded(theme.radius_lg)
            .bg(theme.popover)
            .text_color(theme.foreground)
            .border_1()
            .border_color(theme.border)
            // The card is a mouse barrier above the backdrop: a press inside
            // the dialog never reaches the backdrop that dismisses it, and it
            // is what lets the path's text selection see the drag (#43).
            .block_mouse_except_scroll()
            .child(body)
            .into_any_element();
        self.modal(card, window, cx)
    }
}

#[cfg(test)]
mod tests {
    // GPUI's glob imports its own `test` macro, so this module names what it
    // uses and keeps Rust's `#[test]`.
    use super::{PathProbe, confirmed_folder, path_problem};
    use gpui_kit::TestAppContext;
    use sync_core::store::{AlbumScope, SourceFolder};

    fn draft(path: &str) -> SourceFolder {
        SourceFolder {
            path: path.to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
        }
    }

    /// #65: "Open folder" launches this platform's own file manager. The
    /// macOS arm is the one only the macOS CI job can run.
    #[test]
    fn open_folder_launches_the_platforms_own_file_manager() {
        let expected = if cfg!(windows) {
            "explorer"
        } else if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };

        assert_eq!(super::file_manager_command(), expected);
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
        let error = confirmed_folder(&draft("  ")).expect_err("an empty path is refused");
        assert_eq!(error, "Enter the path of a folder to add.");
    }

    #[test]
    fn a_missing_path_is_refused_with_its_own_reason() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let missing = temp.path().join("not there");

        let error = confirmed_folder(&draft(&missing.display().to_string()))
            .expect_err("a missing path is refused");
        assert_eq!(error, "That path does not exist.");
    }

    #[test]
    fn a_file_is_refused_as_not_a_folder() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let file = temp.path().join("photo.jpg");
        std::fs::write(&file, b"not a folder").expect("create the file");

        let error =
            confirmed_folder(&draft(&file.display().to_string())).expect_err("a file is refused");
        assert_eq!(error, "That path is a file, not a folder.");
    }

    /// #40 (R2.1): a directory this process cannot list is refused with its
    /// own reason. Running as root can list a mode-000 directory, so the name
    /// says the test skips itself then, and the skip prints a `SKIP` line.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_is_refused_with_its_own_reason_unless_run_as_root() {
        use super::probe;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("create a temp dir");
        let locked = temp.path().join("locked");
        std::fs::create_dir(&locked).expect("create the folder");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("lock the folder");

        let probed = probe(&locked);
        let refused = confirmed_folder(&draft(&locked.display().to_string()));
        // Restore the mode before the tempdir drops, so its cleanup can empty
        // the directory.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("unlock the folder");

        if probed == PathProbe::Folder {
            eprintln!(
                "SKIP an_unreadable_folder_is_refused_with_its_own_reason_unless_run_as_root: \
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

        let folder = confirmed_folder(&draft(&spelled)).expect("a readable folder is accepted");
        assert_eq!(folder.path, photos.display().to_string());
        assert!(folder.recursive, "the draft's settings are kept");
        assert!(!folder.album_mapping);
        assert_eq!(folder.album_scope, AlbumScope::Leaf);
    }

    /// #43: the detail renders the folder's full last error. In the product it
    /// comes from the engine's folder status, which a signed-out client has
    /// none of, so the rendering is pinned on an injected card.
    #[gpui_kit::test]
    fn a_folders_last_error_renders_in_full(cx: &mut TestAppContext) {
        use crate::connection::ConnectionModel;
        use crate::engine::EngineModel;
        use crate::paths::AppPaths;
        use crate::status::DashboardModel;
        use crate::store::StoreModel;
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{
            AppContext as _, Bounds, Point, Role, WindowBounds, WindowOptions, px, size,
        };
        use std::sync::Arc;
        use sync_core::connection::ServerProfile;
        use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};
        use sync_core::status::{Activity, FolderStatus, Status};
        use sync_core::store::FolderCounts;

        const ERROR: &str = "the server answered 503 in a form this client does not understand: \
                             Service Unavailable";
        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = AppPaths {
            config_file: temp.path().join("config").join("settings.toml"),
            state_db: temp.path().join("data").join("state.db"),
            autostart_dir: temp.path().join("config").join("autostart"),
        };
        let status = Status {
            activity: Activity::NeedsAttention,
            folders: vec![FolderStatus {
                path: "/photos".to_string(),
                recursive: true,
                album_mapping: false,
                album_scope: AlbumScope::Leaf,
                album_title: None,
                album_rename: None,
                available: true,
                counts: FolderCounts::default(),
                last_error: Some(ERROR.to_string()),
                last_scan_ns: None,
                last_upload_ns: None,
                uploads: sync_core::meter::UploadTraffic::default(),
            }],
            counts: FolderCounts::default(),
            last_error: Some(ERROR.to_string()),
            uploads: sync_core::meter::UploadTraffic::default(),
            scanning: false,
        };
        let mut card = DashboardModel::from_engine(&status, None).folders.remove(0);

        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        let (window, panel) = cx.update(|cx| {
            gpui_kit::init(cx);
            let connection =
                cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials));
            let store = cx.new(|cx| StoreModel::open(&paths, cx));
            let engine = cx.new(|cx| EngineModel::new(connection, store, &paths, cx));
            let path = card.path.clone().expect("the card is a folder");
            let (window, panel) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1200.), px(800.)),
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        super::FolderPanel::detail(
                            engine,
                            path,
                            Arc::new(|_| Ok(())),
                            crate::asset_link::browser_opener(),
                            window,
                            cx,
                        )
                    })
                },
            )
            .expect("open the panel's window");
            // The engine has no status for a folder it never read, so the card
            // the view tests is injected.
            card.available = true;
            panel.update(cx, |panel, _| panel.card = Some(card));
            (window, panel)
        });

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("folder-detail-error").label(),
                Some(format!("Last error: {ERROR}").as_str()),
                "#43: the whole last error is announced"
            );
            assert_eq!(window.find("folder-detail-error").role(), Some(Role::Alert));
            assert_eq!(
                window.find("folder-detail-state").label(),
                Some("Needs attention")
            );
            assert!(
                panel
                    .read(cx)
                    .card
                    .as_ref()
                    .is_some_and(|card| card.available),
                "the fixture card is the one rendered"
            );
        })
        .expect("read the rendered error");
    }

    /// #44: the activity line reads this folder's own meter, in the same words
    /// the global card uses, and says `Idle` when nothing is on the wire.
    ///
    /// The meter itself is the engine's (#39); this pins what the detail does
    /// with the reading the status poll carries.
    #[gpui_kit::test]
    fn the_activity_line_reads_the_folders_meter(cx: &mut TestAppContext) {
        use crate::connection::ConnectionModel;
        use crate::engine::EngineModel;
        use crate::paths::AppPaths;
        use crate::status::DashboardModel;
        use crate::store::StoreModel;
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{AppContext as _, Bounds, Point, WindowBounds, WindowOptions, px, size};
        use std::sync::Arc;
        use sync_core::connection::ServerProfile;
        use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};
        use sync_core::meter::UploadTraffic;
        use sync_core::status::{Activity, FolderStatus, Status};
        use sync_core::store::FolderCounts;

        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = AppPaths {
            config_file: temp.path().join("config").join("settings.toml"),
            state_db: temp.path().join("data").join("state.db"),
            autostart_dir: temp.path().join("config").join("autostart"),
        };
        let status = Status {
            activity: Activity::Syncing,
            folders: vec![FolderStatus {
                path: "/photos".to_string(),
                recursive: true,
                album_mapping: false,
                album_scope: AlbumScope::Leaf,
                album_title: None,
                album_rename: None,
                available: true,
                counts: FolderCounts::default(),
                last_error: None,
                last_scan_ns: None,
                last_upload_ns: None,
                uploads: UploadTraffic {
                    bytes: 41_943_040,
                    bytes_per_second: 2_516_582,
                    in_flight: 3,
                },
            }],
            counts: FolderCounts::default(),
            last_error: None,
            uploads: UploadTraffic::default(),
            scanning: false,
        };
        let mut card = DashboardModel::from_engine(&status, None).folders.remove(0);

        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        let (window, panel) = cx.update(|cx| {
            gpui_kit::init(cx);
            let connection =
                cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials));
            let store = cx.new(|cx| StoreModel::open(&paths, cx));
            let engine = cx.new(|cx| EngineModel::new(connection, store, &paths, cx));
            // The engine has no status for a folder it never read, so the card
            // the view tests is injected.
            card.available = true;
            let path = card.path.clone().expect("the card is a folder");
            let (window, panel) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1200.), px(800.)),
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        super::FolderPanel::detail(
                            engine,
                            path,
                            Arc::new(|_| Ok(())),
                            crate::asset_link::browser_opener(),
                            window,
                            cx,
                        )
                    })
                },
            )
            .expect("open the panel's window");
            panel.update(cx, |panel, _| panel.card = Some(card));
            (window, panel)
        });

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("folder-detail-activity").label(),
                Some("2.4 MiB/s · 3 uploading"),
                "#44: the folder's rate and its uploads on the wire, in the global card's words"
            );
            panel.update(cx, |panel, _| {
                if let Some(card) = panel.card.as_mut() {
                    card.uploads = UploadTraffic::default();
                }
            });
            window.render_frame(cx);
            assert_eq!(
                window.find("folder-detail-activity").label(),
                Some("Idle"),
                "#44: nothing on the wire reads Idle"
            );
            panel.update(cx, |panel, _| {
                if let Some(card) = panel.card.as_mut() {
                    card.available = false;
                    card.uploads = UploadTraffic {
                        bytes: 41_943_040,
                        bytes_per_second: 2_516_582,
                        in_flight: 3,
                    };
                }
            });
            window.render_frame(cx);
            assert_eq!(
                window.find("folder-detail-activity").label(),
                Some("Idle"),
                "#44: a folder that left the status keeps no live rate"
            );
        })
        .expect("read the rendered activity line");
    }

    /// #47: the setup dialog's title field starts blank and names the folder,
    /// so the placeholder says what the album is called without one.
    #[gpui_kit::test]
    fn the_setup_dialog_offers_the_folder_name_as_the_album_title(cx: &mut TestAppContext) {
        use crate::connection::ConnectionModel;
        use crate::engine::EngineModel;
        use crate::paths::AppPaths;
        use crate::store::StoreModel;
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
        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        cx.update(|cx| {
            gpui_kit::init(cx);
            let connection =
                cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials));
            let store = cx.new(|cx| StoreModel::open(&paths, cx));
            let engine = cx.new(|cx| EngineModel::new(connection, store, &paths, cx));
            let (_, panel) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1200.), px(800.)),
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        super::FolderPanel::setup(
                            engine,
                            "/photos/Pictures".to_string(),
                            window,
                            cx,
                        )
                    })
                },
            )
            .expect("open the panel's window");
            let panel = panel.read(cx);
            assert_eq!(
                panel.title_input.read(cx).value().as_str(),
                "",
                "#47: the title starts blank"
            );
            assert_eq!(
                panel
                    .title_input
                    .read(cx)
                    .presentation()
                    .placeholder()
                    .as_ref(),
                "Pictures",
                "#47: the folder's name is the placeholder"
            );
        });
    }

    /// #47: a title the server does not hold yet says why the detail is showing
    /// it as pending. In the product the state comes from the engine's album
    /// row; a signed-out client has none, so the rendering is pinned on an
    /// injected card.
    #[gpui_kit::test]
    fn a_pending_album_title_shows_its_reason(cx: &mut TestAppContext) {
        use crate::connection::ConnectionModel;
        use crate::engine::EngineModel;
        use crate::paths::AppPaths;
        use crate::status::DashboardModel;
        use crate::store::StoreModel;
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{
            AppContext as _, Bounds, Point, Role, WindowBounds, WindowOptions, px, size,
        };
        use std::sync::Arc;
        use sync_core::connection::ServerProfile;
        use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};
        use sync_core::status::{Activity, AlbumRename, FolderStatus, Status};
        use sync_core::store::FolderCounts;

        const REASON: &str = "the API key is missing the album.update permission";
        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = AppPaths {
            config_file: temp.path().join("config").join("settings.toml"),
            state_db: temp.path().join("data").join("state.db"),
            autostart_dir: temp.path().join("config").join("autostart"),
        };
        let status = Status {
            activity: Activity::Idle,
            folders: vec![FolderStatus {
                path: "/photos".to_string(),
                recursive: true,
                album_mapping: true,
                album_scope: AlbumScope::WholeFolder,
                album_title: Some("Holiday 2024".to_string()),
                album_rename: Some(AlbumRename::Failed(REASON.to_string())),
                available: true,
                counts: FolderCounts::default(),
                last_error: None,
                last_scan_ns: None,
                last_upload_ns: None,
                uploads: sync_core::meter::UploadTraffic::default(),
            }],
            counts: FolderCounts::default(),
            last_error: None,
            uploads: sync_core::meter::UploadTraffic::default(),
            scanning: false,
        };
        let card = DashboardModel::from_engine(&status, None).folders.remove(0);

        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        let (window, panel) = cx.update(|cx| {
            gpui_kit::init(cx);
            let connection =
                cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials));
            let store = cx.new(|cx| StoreModel::open(&paths, cx));
            let engine = cx.new(|cx| EngineModel::new(connection, store, &paths, cx));
            let path = card.path.clone().expect("the card is a folder");
            let (window, panel) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1200.), px(800.)),
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        super::FolderPanel::detail(
                            engine,
                            path,
                            Arc::new(|_| Ok(())),
                            crate::asset_link::browser_opener(),
                            window,
                            cx,
                        )
                    })
                },
            )
            .expect("open the panel's window");
            panel.update(cx, |panel, _| panel.card = Some(card));
            (window, panel)
        });

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("folder-detail-album-rename").role(),
                Some(Role::Alert),
                "#47: a pending title is an alert"
            );
            let notice = window.find("folder-detail-album-rename");
            let shown = notice.label().unwrap_or_default().to_string();
            assert!(
                shown.contains(REASON),
                "#47: the reason the title is pending is shown: {shown:?}"
            );
            assert!(
                panel.read(cx).card.is_some(),
                "the fixture card is the one rendered"
            );
        })
        .expect("read the rendered reason");
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
            let (window, panel) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1200.), px(800.)),
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        super::FolderPanel::setup(
                            engine.clone(),
                            "/no such folder".to_string(),
                            window,
                            cx,
                        )
                    })
                },
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

    /// #43 (LOW): a worker that reports before the panel records its request
    /// must not leave `removing` set. The panel reads its own answer back, as
    /// the setup dialog's Add does, so the reason lands either way.
    #[gpui_kit::test]
    fn a_remove_answered_before_its_request_is_recorded_clears_removing(cx: &mut TestAppContext) {
        use crate::connection::{ConnectionModel, WORKER_UNAVAILABLE};
        use crate::engine::EngineModel;
        use crate::paths::AppPaths;
        use crate::store::StoreModel;
        use gpui_kit::{AppContext as _, Bounds, Point, WindowBounds, WindowOptions, px, size};
        use std::sync::Arc;
        use sync_core::connection::ServerProfile;
        use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};

        // The store opens on a worker thread, a real thread.
        cx.executor().allow_parking();
        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = AppPaths {
            config_file: temp.path().join("config").join("settings.toml"),
            state_db: temp.path().join("data").join("state.db"),
            autostart_dir: temp.path().join("config").join("autostart"),
        };
        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        cx.update(|cx| {
            gpui_kit::init(cx);
            let connection =
                cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials));
            let store = cx.new(|cx| StoreModel::open(&paths, cx));
            let engine = cx.new(|cx| EngineModel::new(connection, store, &paths, cx));
            let (_, panel) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1200.), px(800.)),
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        super::FolderPanel::detail(
                            engine.clone(),
                            "/photos".to_string(),
                            Arc::new(|_| Ok(())),
                            crate::asset_link::browser_opener(),
                            window,
                            cx,
                        )
                    })
                },
            )
            .expect("open the panel's window");
            // The engine answers the request it is about to return, as a worker
            // that cannot start does, before the panel can record it.
            engine.update(cx, |engine, cx| engine.lose_next_remove_for_test(cx));
            panel.update(cx, |panel, cx| panel.remove_confirmed(cx));
            let panel = panel.read(cx);
            assert!(
                !panel.removing,
                "#43: a synchronously reported loss does not leave `removing` set"
            );
            assert_eq!(
                panel.problem.as_deref(),
                Some(WORKER_UNAVAILABLE),
                "#43: the detail shows why nothing was removed"
            );
        });
    }
}
