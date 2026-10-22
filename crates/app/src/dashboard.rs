//! The dashboard view (SPEC R8.1): a global status card and one card per
//! source folder.
//!
//! A new folder is confirmed in the setup dialog (#40) before the client
//! records and scans it; the dialog's panel lives in [`crate::folder_panel`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::asset_link::{self, LinkOpener};
use crate::engine::{EngineModel, SyncNowOutcome};
use crate::folder_controls::{SettingsControls, SettingsIds, SettingsValues};
use crate::folder_panel::{FolderOpener, FolderPanel, FolderPanelEvent};
use crate::path_picker::{self, Picked};
use crate::shell::ViewMode;
use crate::status::{ActivityState, DashboardModel, StatusCard};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Colorize as _;
use gpui_kit::component::Theme;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::tag::Tag;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use sync_core::store::AlbumScope;

/// What the dashboard asks the shell to do (#43).
///
/// The dashboard cannot switch surfaces itself: the [`crate::shell::AppShell`]
/// owns the visible view. It reports the request and the shell opens it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DashboardEvent {
    /// Open one list view narrowed to one source folder (#41, #43).
    OpenView(ViewMode, String),
}

/// The dashboard view: a status card per source folder, the global card, and
/// the folder controls (R8.1).
///
/// With an [`EngineModel`] the view renders the live engine status and sends
/// every control straight to the engine. Without one (the layout tests) it
/// renders the injected model and keeps the pause state locally.
pub struct Dashboard {
    model: DashboardModel,
    engine: Option<Entity<EngineModel>>,
    folder_input: Option<Entity<InputState>>,
    paused: bool,
    /// The notice a failed Browse… left behind (#30).
    pick_notice: Option<String>,
    /// The reason the last "Sync now" click could not scan (#56). The outcome,
    /// not its sentence: the header drops it once the reason no longer holds.
    sync_notice: Option<SyncNowOutcome>,
    /// The open folder panel: the setup dialog (#40) or a folder's live detail
    /// (#43). The dashboard owns whether it is open; the panel owns the state
    /// inside it.
    panel: Option<Entity<FolderPanel>>,
    panel_subscription: Option<Subscription>,
    /// Where focus goes back to when the detail closes: the card's Details
    /// anchor and the folder's stored path (#43).
    detail_return: Option<(String, FocusHandle)>,
    /// One focus anchor per folder card, keyed by the stored path so a removal
    /// cannot shift a deleted folder's focus onto its neighbour (#43).
    card_focus: HashMap<String, FocusHandle>,
    /// What "Open folder" runs (#43). The product opens the platform's file
    /// manager; a test injects a recorder.
    folder_opener: FolderOpener,
    /// What an asset link opens (#44). The product opens the default browser; a
    /// test injects a recorder.
    link_opener: LinkOpener,
    /// A folder whose detail a capture run pins open (#43), so a screenshot of
    /// the detail needs no pointer.
    capture_detail: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DashboardEvent> for Dashboard {}

impl Dashboard {
    /// Builds the dashboard from a fixed view model, with no engine attached.
    #[must_use]
    pub fn new(model: DashboardModel) -> Self {
        Self {
            model,
            engine: None,
            folder_input: None,
            paused: false,
            pick_notice: None,
            sync_notice: None,
            panel: None,
            panel_subscription: None,
            detail_return: None,
            card_focus: HashMap::new(),
            folder_opener: Arc::new(crate::folder_panel::open_in_file_manager),
            link_opener: asset_link::browser_opener(),
            capture_detail: None,
            _subscriptions: Vec::new(),
        }
    }

    /// Builds the live dashboard over the app-scoped engine (R8.1).
    pub fn with_engine(
        engine: Entity<EngineModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let folder_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("/home/you/Pictures")
                .masked(false)
        });
        let subscriptions = vec![cx.observe(&engine, |this, engine, cx| {
            this.model = engine.read(cx).dashboard_model(cx);
            cx.notify();
        })];
        let model = engine.read(cx).dashboard_model(cx);
        Self {
            model,
            engine: Some(engine),
            folder_input: Some(folder_input),
            paused: false,
            pick_notice: None,
            sync_notice: None,
            panel: None,
            panel_subscription: None,
            detail_return: None,
            card_focus: HashMap::new(),
            folder_opener: Arc::new(crate::folder_panel::open_in_file_manager),
            link_opener: asset_link::browser_opener(),
            capture_detail: std::env::var(crate::shell::FORCE_DETAIL_ENV).ok(),
            _subscriptions: subscriptions,
        }
    }

    /// Replaces the action "Open folder" runs, so a test can assert the path
    /// without starting the platform's file manager (#43). It applies to the
    /// next detail the view opens.
    pub fn set_folder_opener(&mut self, opener: impl Fn(&str) -> std::io::Result<()> + 'static) {
        self.folder_opener = Arc::new(opener);
    }

    /// Replaces the action an asset link runs, so a test can assert the URL
    /// without starting a browser (#44). It applies to the next detail the view
    /// opens.
    pub fn set_link_opener(&mut self, opener: impl Fn(&str, &App) + 'static) {
        self.link_opener = Arc::new(opener);
    }

    /// Whether new uploads are held (R8.7).
    fn is_paused(&self, cx: &App) -> bool {
        match &self.engine {
            Some(engine) => engine.read(cx).is_paused(),
            None => self.paused,
        }
    }

    fn sync_now(&mut self, cx: &mut Context<Self>) {
        match &self.engine {
            Some(engine) => {
                let outcome = engine.update(cx, |model, cx| model.sync_now(cx));
                // #56: a click that cannot scan says why, so the control never
                // swallows it. A click that lands clears the last reason.
                self.sync_notice = (outcome != SyncNowOutcome::Requested).then_some(outcome);
                cx.notify();
            }
            None => {
                self.paused = false;
                self.model.global.activity = ActivityState::Syncing;
                cx.notify();
            }
        }
    }

    fn toggle_paused(&mut self, cx: &mut Context<Self>) {
        match &self.engine {
            Some(engine) => {
                let paused = engine.read(cx).is_paused();
                engine.update(cx, |model, cx| {
                    if paused {
                        model.resume(cx);
                    } else {
                        model.pause(cx);
                    }
                });
                self.model = engine.read(cx).dashboard_model(cx);
                cx.notify();
            }
            None => {
                self.paused = !self.paused;
                self.model.global.activity = if self.paused {
                    ActivityState::Paused
                } else {
                    ActivityState::Syncing
                };
                cx.notify();
            }
        }
    }

    /// Opens the OS folder picker and fills the field with the chosen folder
    /// (R2.1).
    ///
    /// A cancel changes nothing. A dialog that cannot open shows a notice and
    /// leaves the typed path alone (#30).
    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        path_picker::browse(
            PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: None,
            },
            window,
            cx,
            |this, result, window, cx| this.apply_pick(result, window, cx),
        );
    }

    /// Applies one picker result: the chosen folder fills the field, a cancel
    /// changes nothing, and a failure shows the notice (#30).
    ///
    /// This takes the result rather than reading it at the click site, because
    /// the test harness can answer a prompt with a choice or a cancel but never
    /// with an error.
    pub fn apply_pick(
        &mut self,
        result: anyhow::Result<Option<Vec<PathBuf>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_notice = None;
        match path_picker::picked(result) {
            Picked::Path(path) => {
                let path = path.to_string_lossy().into_owned();
                if let Some(input) = self.folder_input.clone() {
                    input.update(cx, |state, cx| state.set_value(path.clone(), window, cx));
                }
                // R2.1: a chosen folder is confirmed in the setup dialog, not
                // recorded straight away.
                self.open_folder_setup(path, window, cx);
            }
            Picked::Cancelled => {}
            Picked::Failed(notice) => self.pick_notice = Some(notice),
        }
        cx.notify();
    }

    /// Opens the setup dialog for the path the field holds (#40, R2.1).
    fn open_setup_from_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self
            .folder_input
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_string())
            .unwrap_or_default();
        self.open_folder_setup(path, window, cx);
    }

    /// Opens the folder setup dialog for `path` (#40).
    ///
    /// Nothing is written here: the panel writes the path and all three
    /// settings in one request when the user presses Add, and a Cancel drops
    /// the draft.
    fn open_folder_setup(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        // One setup at a time: a second Add must not replace a dialog that is
        // already open, whose request may still be in flight.
        if self.panel.is_some() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let panel = cx.new(|cx| FolderPanel::setup(engine, path, window, cx));
        let focus = panel.read(cx).focus_handle(cx);
        let subscription = cx.subscribe_in(
            &panel,
            window,
            |this, _panel, event: &FolderPanelEvent, window, cx| {
                this.panel_event(event, window, cx);
            },
        );
        self.panel = Some(panel);
        self.panel_subscription = Some(subscription);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Opens one folder's live detail (#43).
    ///
    /// Nothing is written: the settings write only when the user changes one,
    /// through the same engine request the card makes.
    fn open_folder_detail(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel.is_some() {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let opener = self.folder_opener.clone();
        let link_opener = self.link_opener.clone();
        let panel =
            cx.new(|cx| FolderPanel::detail(engine, path.clone(), opener, link_opener, window, cx));
        let focus = panel.read(cx).focus_handle(cx);
        let subscription = cx.subscribe_in(
            &panel,
            window,
            |this, _panel, event: &FolderPanelEvent, window, cx| {
                this.panel_event(event, window, cx);
            },
        );
        self.detail_return = Some((path.clone(), self.details_focus(&path, cx).tab_stop(false)));
        self.panel = Some(panel);
        self.panel_subscription = Some(subscription);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Opens the pinned detail once the store has loaded its folder (#43).
    ///
    /// The screenshot recipe sets [`crate::shell::FORCE_DETAIL_ENV`], so the
    /// capture of the detail needs no pointer. It waits for the folder to be in
    /// the model, because the store opens off the UI thread.
    fn open_capture_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.capture_detail.clone() else {
            return;
        };
        if self.panel.is_some()
            || !self
                .model
                .folders
                .iter()
                .any(|card| card.path.as_deref() == Some(path.as_str()))
        {
            return;
        }
        self.capture_detail = None;
        self.open_folder_detail(path, window, cx);
    }

    /// Applies one panel event: the setup lifecycle, the detail's close, or a
    /// link into a filtered list view (#40, #43).
    fn panel_event(
        &mut self,
        event: &FolderPanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            FolderPanelEvent::Added => {
                self.clear_folder_input(window, cx);
                self.close_folder_setup(window, cx);
            }
            FolderPanelEvent::Cancelled => self.close_folder_setup(window, cx),
            FolderPanelEvent::Closed => self.close_folder_detail(window, cx),
            FolderPanelEvent::Open(mode, folder) => {
                self.close_folder_detail(window, cx);
                // #43 follow-up: the new surface opens without the keyboard,
                // because focus moves into a view the dashboard does not own.
                // Moving it into that view's folder filter needs a focus handle
                // per view; recorded, not done here.
                cx.emit(DashboardEvent::OpenView(*mode, folder.clone()));
            }
        }
    }

    /// The focus anchor of one card's Details control (#43).
    ///
    /// The anchor is kept per stored path, not per index a removal can shift.
    /// It is created on demand, because a click handler is not inside a render
    /// pass and cannot use a keyed window state.
    fn details_focus(&mut self, path: &str, cx: &mut Context<Self>) -> FocusHandle {
        self.card_focus
            .entry(path.to_string())
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    /// Closes the detail dialog and puts focus back on the folder's card
    /// (#43), or on the folder field when the card is gone.
    fn close_folder_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel = None;
        self.panel_subscription = None;
        if let Some((path, anchor)) = self.detail_return.take() {
            if self
                .model
                .folders
                .iter()
                .any(|card| card.path.as_deref() == Some(path.as_str()))
            {
                // The anchor is not a tab stop of its own: its first tab stop is
                // the card's Details control.
                window.focus(&anchor, cx);
                window.focus_next(cx);
            } else if let Some(input) = self.folder_input.clone() {
                // A successful Remove took the card with it, so the card's
                // anchor renders nowhere: the keyboard goes to the folder field
                // instead of landing on nothing (#43).
                let focus = input.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            }
        }
        cx.notify();
    }

    /// Closes the setup dialog and puts focus back on the folder field (#40).
    fn close_folder_setup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel = None;
        self.panel_subscription = None;
        if let Some(input) = self.folder_input.clone() {
            let focus = input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Clears the field once its folder is recorded (#40).
    fn clear_folder_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = self.folder_input.clone() {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
    }

    /// Removes one source folder (R2.1).
    fn remove_folder(&mut self, path: String, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.update(cx, |model, cx| model.remove_folder(path, cx));
        }
    }

    /// Turns recursive discovery on or off for one source folder (R2.1).
    ///
    /// The engine model treats this as the folder's own edit, updates the
    /// existing folder, and re-watches and rescans it.
    fn set_folder_recursive(&mut self, path: String, recursive: bool, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.update(cx, |model, cx| model.add_folder(path, recursive, cx));
        }
    }

    /// Turns album mapping on or off for one source folder (R3.4).
    fn set_album_mapping(&mut self, path: String, enabled: bool, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.update(cx, |model, cx| model.set_album_mapping(path, enabled, cx));
        }
    }

    /// Sets the album scope for one source folder (R3.4).
    fn set_album_scope(&mut self, path: String, scope: AlbumScope, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.update(cx, |model, cx| model.set_album_scope(path, scope, cx));
        }
    }

    /// Shows the connection state on the global card (SPEC R8.1).
    pub fn set_connection(&mut self, state: Option<String>, cx: &mut Context<Self>) {
        if self.model.global.connection != state {
            self.model.global.connection = state;
            cx.notify();
        }
    }

    /// The folder add field, its Browse… control, and its button (R2.1).
    fn folder_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let accent = cx.theme().danger;
        let palette = CardPalette::new(cx.theme());
        let muted = cx.theme().muted_foreground;
        let input = self.folder_input.clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("Add a folder to sync"),
                    )
                    .when_some(input, |this, input| {
                        let focus = input.read(cx).focus_handle(cx);
                        this.child(
                            div()
                                .id("dashboard-folder-path")
                                .test_support()
                                .track_focus(&focus)
                                .aria_label("Source folder path")
                                // Enter opens the setup dialog. This raw key
                                // handler is the path that fires: the field
                                // and this wrapper share one focus handle, so
                                // the key reaches the wrapper whatever the
                                // platform does with the input's own context.
                                // The input's `PressEnter` used to open the
                                // dialog a second time; a rendered test covers
                                // the surviving path (#43).
                                .on_key_down(cx.listener(
                                    |this, event: &KeyDownEvent, window, cx| {
                                        if event.keystroke.key == "enter" {
                                            this.open_setup_from_field(window, cx);
                                            cx.stop_propagation();
                                        }
                                    },
                                ))
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&input).aria_label("Source folder path")),
                        )
                        .child(
                            Button::new("dashboard-browse-folder")
                                .secondary()
                                .label("Browse…")
                                .accessibility_label("Browse for a source folder")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.browse(window, cx);
                                })),
                        )
                        .child(
                            Button::new("dashboard-add-folder")
                                .primary()
                                .label("Add folder")
                                .accessibility_label("Add folder")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_setup_from_field(window, cx);
                                })),
                        )
                    }),
            )
            .when_some(self.pick_notice.clone(), |this, notice| {
                this.child(crate::settings_view::notice(
                    "dashboard-browse-notice",
                    notice,
                    accent,
                    &palette,
                ))
            })
    }

    /// The recursion switch, the album mapping switch, and the album scope
    /// choice for one folder card (R2.1, R3.4), through the shared controls
    /// the folder detail uses too (#43).
    ///
    /// Each control stops the click from reaching the card, which opens the
    /// folder's detail (#43).
    fn folder_controls(
        &self,
        index: usize,
        card: &StatusCard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let path = card.path.clone().unwrap_or_default();
        let recursive_path = path.clone();
        let mapping_path = path.clone();
        SettingsControls::new(
            SettingsIds {
                recursive: ("folder-recursive", index).into(),
                album_mapping: ("folder-album", index).into(),
                album_leaf: ("folder-album-leaf", index).into(),
                album_whole: ("folder-album-whole", index).into(),
            },
            card.title.clone(),
            SettingsValues {
                recursive: card.recursive,
                album_mapping: card.album_mapping,
                album_scope: card.album_scope,
            },
            cx.listener(move |this, recursive: &bool, _, cx| {
                this.set_folder_recursive(recursive_path.clone(), *recursive, cx);
                cx.stop_propagation();
            }),
            cx.listener(move |this, enabled: &bool, _, cx| {
                this.set_album_mapping(mapping_path.clone(), *enabled, cx);
                cx.stop_propagation();
            }),
            cx.listener(move |this, scope: &AlbumScope, _, cx| {
                this.set_album_scope(path.clone(), *scope, cx);
                cx.stop_propagation();
            }),
        )
        .render(cx.theme().muted_foreground)
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let muted = cx.theme().muted_foreground;
        let accent = cx.theme().warning;
        let palette = CardPalette::new(cx.theme());
        let paused = self.is_paused(cx);
        let (sync_label, sync_name) = crate::status::sync_now_copy(self.model.scanning);
        // #56: the reason is asked for again on every frame, so a notice that
        // outlived its cause cannot stay on screen beside the card that
        // answered it.
        let sync_notice = match (&self.engine, self.sync_notice) {
            (Some(engine), Some(outcome)) => engine.read(cx).sync_now_reason(outcome, cx),
            _ => None,
        };
        let (pause_label, pause_name) = if paused {
            ("Resume", "Resume syncing")
        } else {
            ("Pause", "Pause syncing")
        };

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_2xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Immich Sync"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(muted)
                                    .child("Upload status for every source folder"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                // #56: the control reports the scan it started,
                                // and a click during a scan is harmless: it
                                // re-notifies the engine.
                                Button::new("dashboard-sync-now")
                                    .primary()
                                    .label(sync_label)
                                    .accessibility_label(sync_name)
                                    .on_click(cx.listener(|this, _, _, cx| this.sync_now(cx))),
                            )
                            .child(
                                Button::new("dashboard-pause")
                                    .secondary()
                                    .label(pause_label)
                                    .accessibility_label(pause_name)
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_paused(cx))),
                            ),
                    ),
            )
            // #56: a click that cannot scan says why, in the same notice idiom
            // the folder bar uses.
            .when_some(sync_notice, |this, notice| {
                this.child(crate::settings_view::notice(
                    "dashboard-sync-notice",
                    notice,
                    accent,
                    &palette,
                ))
            })
    }
}

impl Render for Dashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // #43: a capture run pins a folder's detail open, so a screenshot of
        // the dialog needs no pointer. The store opens off the UI thread, so
        // the dialog waits until the folder is in the model.
        self.open_capture_detail(window, cx);
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let header = self.header(cx);

        let folder_bar = self.folder_bar(cx);
        let removed = self.engine.is_some();

        div()
            .id("dashboard-scroll")
            .test_support()
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
            // A short window scrolls instead of hiding the last card's controls
            // (R8.1: every card stays reachable).
            .overflow_y_scroll()
            .p_6()
            .gap_4()
            .child(header)
            .child(status_card(
                "status-global".into(),
                "status-state-global".into(),
                Some("status-connection-global".into()),
                &self.model.global,
                None,
                None,
                cx,
            ))
            .child(folder_bar)
            .child(div().text_sm().text_color(muted).child("Source folders"))
            .child(
                div().flex().flex_row().flex_wrap().gap_4().children(
                    // The cards are copied out of the model, so the loop can
                    // ask the view for a card's focus anchor (#43).
                    self.model
                        .folders
                        .clone()
                        .iter()
                        .enumerate()
                        .map(|(index, folder)| {
                            let path = folder.path.clone().unwrap_or_default();
                            let title = folder.title.clone();
                            let remove = removed.then(|| {
                                let path = path.clone();
                                Button::new(("folder-remove", index))
                                    .ghost()
                                    .label("Remove")
                                    .accessibility_label(format!("Remove {title}"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        // #43: a card control never opens the
                                        // detail the card itself opens.
                                        cx.stop_propagation();
                                        this.remove_folder(path.clone(), cx);
                                    }))
                                    .into_any_element()
                            });
                            let album = removed.then(|| {
                                self.folder_controls(index, folder, cx).into_any_element()
                            });
                            let details_focus = self.details_focus(&path, cx);
                            let details = removed.then(|| {
                                let path = path.clone();
                                div()
                                    .id(("folder-details-anchor", index))
                                    .test_support()
                                    .track_focus(&details_focus)
                                    .tab_stop(false)
                                    .child(
                                        Button::new(("folder-details", index))
                                            .secondary()
                                            .label("Details")
                                            .accessibility_label(format!("Details for {title}"))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.open_folder_detail(path.clone(), window, cx);
                                            })),
                                    )
                                    .into_any_element()
                            });
                            let footer = removed.then(|| {
                                let remove = remove.unwrap_or_else(|| div().into_any_element());
                                let details = details.unwrap_or_else(|| div().into_any_element());
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(details)
                                    .child(remove)
                                    .into_any_element()
                            });
                            div()
                                .id(("folder-card", index))
                                .test_support()
                                .w(rems(17.5))
                                .flex()
                                // #43: the card opens the folder's detail; its
                                // controls stop the click at themselves.
                                .when(removed, |this| {
                                    let path = path.clone();
                                    this.on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_folder_detail(path.clone(), window, cx);
                                    }))
                                })
                                .child(status_card(
                                    ("status-folder", index).into(),
                                    ("status-state-folder", index).into(),
                                    None,
                                    folder,
                                    album,
                                    footer,
                                    cx,
                                ))
                        }),
                ),
            )
            .when_some(self.panel.clone(), |this, panel| this.child(panel))
    }
}

/// The theme tokens one dashboard card paints with.
///
/// The color choice lives here, so the WCAG contrast test measures the tokens
/// the view actually paints rather than a copy of them.
#[derive(Clone, Copy, Debug)]
pub struct CardPalette {
    /// The card fill.
    pub surface: Hsla,
    /// The card title and notice text.
    pub text: Hsla,
    /// The path, percentage, and counts text.
    pub meta: Hsla,
    /// The state pill fill.
    pub pill_fill: Hsla,
    /// The state pill text.
    pub pill_text: Hsla,
}

impl CardPalette {
    /// Reads the palette from the active theme.
    #[must_use]
    pub fn new(theme: &Theme) -> Self {
        Self {
            surface: theme.popover,
            text: theme.foreground,
            meta: theme.muted_foreground,
            pill_fill: theme.secondary,
            pill_text: theme.secondary_foreground,
        }
    }
}

/// How far a state hue is darkened to read as a 1px border on a card.
///
/// The kit's light accents are too bright for a border: the light `info` and
/// `warning` tokens sit below the WCAG non-text contrast floor of 3:1 on the
/// card surface. This factor clears it in both themes.
const STATE_BORDER_DARKEN: f32 = 0.25;

/// The border color that carries one activity state's hue.
///
/// The pill label stays in the theme's paired pill ink, because the light
/// accents are too bright to read as text on a light card.
#[must_use]
pub fn state_border(state: ActivityState, theme: &Theme) -> Hsla {
    let accent = match state {
        ActivityState::Idle => theme.muted_foreground,
        ActivityState::Syncing => theme.info,
        ActivityState::Paused => theme.warning,
        ActivityState::NeedsAttention => theme.danger,
    };
    accent.darken(STATE_BORDER_DARKEN)
}

/// One status card. The text comes from [`StatusCard::content`], so the view
/// does no formatting. `footer` is the card's controls row — Details and Remove
/// on a folder card, when the dashboard is attached to an engine (#43, R2.1) —
/// and `album` the per-folder settings controls (R3.4).
fn status_card(
    id: ElementId,
    state_id: ElementId,
    connection_id: Option<ElementId>,
    card: &StatusCard,
    album: Option<AnyElement>,
    footer: Option<AnyElement>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let palette = CardPalette::new(theme);
    let content = card.content();
    let title = content.title.clone();
    let mut root = div()
        .id(id.clone())
        .test_support()
        .flex()
        .flex_col()
        .gap_3()
        .p_4()
        .w_full()
        .rounded(theme.radius_lg)
        .bg(palette.surface)
        .text_color(palette.text)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .text_base()
                                .font_weight(FontWeight::MEDIUM)
                                .child(content.title),
                        )
                        .when_some(content.path, |this, path| {
                            this.child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(palette.meta)
                                    .child(path),
                            )
                        }),
                )
                .child(
                    div()
                        .id(state_id)
                        .test_support()
                        .aria_label(content.state.clone())
                        .child(state_tag(card.activity, content.state, &palette, theme)),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    Progress::new((id.clone(), "progress"))
                        .flex_1()
                        .value(content.progress * 100.0)
                        .accessibility_label(format!("{title} progress")),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(palette.meta)
                        .child(content.progress_percent),
                ),
        )
        .child(
            div()
                .text_sm()
                .text_color(palette.meta)
                .child(content.counts),
        )
        .when_some(content.uploads.clone(), |this, uploads| {
            // R8.1: the global card's rate and uploads in flight. Plain meta
            // ink on the card surface, which is the pair the theme contrast
            // test measures, and a label instead of a control: nothing new to
            // operate or to skip.
            this.child(
                div()
                    .id((id.clone(), "uploads"))
                    .test_support()
                    .aria_label(uploads.clone())
                    .text_sm()
                    .text_color(palette.meta)
                    .child(uploads),
            )
        })
        .when_some(content.scan.clone(), |this, scan| {
            // #56: the global card's reconciliation scan, in the folder
            // detail's own words. A label, not a control: nothing new to
            // operate or to skip.
            this.child(
                div()
                    .id((id.clone(), "scan"))
                    .test_support()
                    .aria_label(scan.clone())
                    .text_sm()
                    .text_color(palette.meta)
                    .child(scan),
            )
        })
        .when_some(content.album.clone(), |this, album| {
            this.child(div().text_sm().text_color(palette.meta).child(album))
        });

    if let Some(connection) = content.connection.clone()
        && let Some(id) = connection_id
    {
        root = root.child(
            div()
                .id(id)
                .test_support()
                .aria_label(connection.clone())
                .text_sm()
                .text_color(palette.meta)
                .child(connection),
        );
    }
    if let Some(review) = content.review {
        root = root.child(notice(review, theme.warning, &palette));
    }
    if let Some(error) = content.error {
        root = root.child(notice(
            format!("Last error: {error}"),
            theme.danger,
            &palette,
        ));
    }
    if let Some(album) = album {
        root = root.child(album);
    }
    if let Some(footer) = footer {
        root = root.child(footer);
    }
    root
}

/// The activity-state pill on a card.
///
/// The wrapper owns the accessible name and the visible copy is inaccessible
/// to assistive technology, so the state is announced once, not twice.
pub(crate) fn state_tag(
    state: ActivityState,
    label: String,
    palette: &CardPalette,
    theme: &Theme,
) -> Tag {
    Tag::secondary()
        .bg(palette.pill_fill)
        .text_color(palette.pill_text)
        .border_color(state_border(state, theme))
        .child(Text::new_inaccessible(label.into()))
}

/// A card notice: a colored edge marks the kind, and the text keeps the
/// card's readable ink.
fn notice(text: String, accent: Hsla, palette: &CardPalette) -> impl IntoElement {
    div()
        .border_l_2()
        .border_color(accent)
        .pl_2()
        .text_xs()
        .text_color(palette.text)
        .child(text)
}
