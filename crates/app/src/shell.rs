//! The tray shell: one GPUI window, one tray icon, and the actions they share.

use gpui_kit::base::Tabs as SurfaceTabs;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::scroll::ScrollableMask;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_tray::{Icon, Tray};

use crate::connection::ConnectionModel;
use crate::dashboard::{Dashboard, DashboardEvent};
use crate::diagnostics::DiagnosticsModel;
use crate::diagnostics_view::DiagnosticsView;
use crate::engine::EngineModel;
use crate::failed_view::FailedView;
use crate::logs_view::LogsView;
use crate::paths::AppPaths;
use crate::review_view::ReviewView;
use crate::settings_view::SettingsView;
use crate::store::StoreModel;
use crate::upload::UploadModel;
use crate::upload_view::UploadView;
use sync_core::connection::{ConnectionState, ServerProfile};
use sync_core::status::Activity;
use sync_core::store::FolderCounts;

use std::sync::Mutex;

actions!(
    immich_sync,
    [Show, Hide, Quit, Pause, Resume, SyncNow, About, Minimize]
);

/// Global holding the tray icon so [`Quit`] can remove it.
struct TrayHandle(Tray);
impl Global for TrayHandle {}

/// Installs the shell: the window actions, the app-level connection model, and
/// the app-scoped tray observer (R1.4).
///
/// Call it once, after [`gpui_kit::init`].
pub fn init(cx: &mut App) {
    // The tray owns the process lifetime: closing the window must not quit.
    // Without this, GPUI quits when the last window closes on Windows and
    // Linux, which would end the tray app.
    cx.set_quit_mode(QuitMode::Explicit);
    // A system notification needs an identity on Windows (the AppUserModelID)
    // and on the test platform, so it is set before any can be posted (R5.4,
    // R10.1). It must match the packager identifier: the NSIS installer stamps
    // shortcuts with that identifier, and a pin or toast only resolves to them
    // when the two agree.
    cx.set_app_identity(crate::BUNDLE_ID, "Immich Sync");
    cx.on_action(show)
        .on_action(hide)
        .on_action(quit)
        .on_action(pause)
        .on_action(resume)
        .on_action(sync_now)
        // The macOS menu bar's own items; nothing else dispatches them, so
        // registering the handlers everywhere leaves Linux and Windows alone.
        .on_action(about)
        .on_action(minimize);
    // The macOS menu bar and the shortcuts it advertises (issue #64). The keys
    // are bound before the bar is built: an item draws its key equivalent from
    // the keymap, and GPUI has no `OsAction` for Quit or Close. With no context
    // they apply everywhere; the Edit items carry the inputs' own actions, so
    // `cmd-x/c/v/a` still dispatch to the focused field's handlers.
    #[cfg(target_os = "macos")]
    {
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-w", Hide, None),
        ]);
        cx.set_menus(app_menu());
    }
    let connection = install_connection_with(cx, show_connection_on_tray);
    install_models(cx, connection);
}

/// The app-level handle to the one connection model (R1.4).
///
/// The model is app state, not window state: hiding the window must not stop
/// the tray tooltip from following state transitions. The engine state that
/// later tickets push to the tray attaches here too.
struct ConnectionHandle(Entity<ConnectionModel>);
impl Global for ConnectionHandle {}

/// The one connection model, created by [`init`].
///
/// # Panics
///
/// Panics when [`init`] has not run.
#[must_use]
pub fn connection(cx: &App) -> Entity<ConnectionModel> {
    cx.global::<ConnectionHandle>().0.clone()
}

/// The app-level handle to the one manual-upload model (issue #5).
struct UploadHandle(Entity<UploadModel>);
impl Global for UploadHandle {}

/// The one manual-upload model, created by [`init`].
///
/// # Panics
///
/// Panics when [`init`] has not run.
#[must_use]
pub fn upload(cx: &App) -> Entity<UploadModel> {
    cx.global::<UploadHandle>().0.clone()
}

/// The app-level handle to the one engine model (issue #6).
struct EngineHandle(Entity<EngineModel>);
impl Global for EngineHandle {}

/// The one engine model, created by [`init`].
///
/// # Panics
///
/// Panics when [`init`] has not run.
#[must_use]
pub fn engine(cx: &App) -> Entity<EngineModel> {
    cx.global::<EngineHandle>().0.clone()
}

/// The app-level handle to the one shared state store.
struct StoreHandle(Entity<StoreModel>);
impl Global for StoreHandle {}

/// The one shared store model, created by [`init`].
///
/// # Panics
///
/// Panics when [`init`] has not run.
#[must_use]
pub fn store(cx: &App) -> Entity<StoreModel> {
    cx.global::<StoreHandle>().0.clone()
}

/// The last tooltip written to the tray, so the 250 ms status poll does not
/// call the platform with an unchanged string.
struct TrayTooltip(Mutex<String>);
impl Global for TrayTooltip {}

/// Creates the shared store, the engine, and the manual-upload model at App
/// scope (N2, N4).
///
/// The database opens off the UI thread in [`StoreModel`]; the engine starts
/// when the store and a credential are both ready.
pub fn install_models(
    cx: &mut App,
    connection: Entity<ConnectionModel>,
) -> (Entity<StoreModel>, Entity<EngineModel>) {
    let paths = AppPaths::resolve().unwrap_or_else(|error| {
        eprintln!("immich-sync: could not resolve the state location: {error:#}");
        AppPaths::fallback()
    });
    let store = cx.new(|cx| StoreModel::open(&paths, cx));
    let engine = cx.new(|cx| EngineModel::new(connection.clone(), store.clone(), &paths, cx));
    let upload = cx.new(|cx| UploadModel::new(connection, store.clone(), cx));
    cx.set_global(StoreHandle(store.clone()));
    cx.set_global(EngineHandle(engine.clone()));
    cx.set_global(UploadHandle(upload.clone()));
    cx.set_global(TrayTooltip(Mutex::new(String::new())));
    // The tooltip follows the engine's live activity as well as the connection.
    cx.observe(&engine, |_, cx| show_status_on_tray(cx))
        .detach();
    (store, engine)
}

/// Creates the connection model and registers the tray observer at App scope.
///
/// `present` is the production [`show_connection_on_tray`]; the parameter lets
/// a test record what the app scope pushes without a native tray.
fn install_connection_with(
    cx: &mut App,
    present: impl Fn(&ConnectionState, &mut App) + 'static,
) -> Entity<ConnectionModel> {
    let connection = cx.new(|_| {
        ConnectionModel::new(
            ServerProfile::default(),
            crate::credentials::CredentialStoreProvider::new(
                crate::credentials::platform_credential_store,
            ),
        )
    });
    cx.set_global(ConnectionHandle(connection.clone()));
    // App scope, not the window's view context: `hide_window` drops the root
    // view, and a window-scoped observer would die with it (R1.4).
    cx.observe(&connection, move |connection, cx| {
        present(&connection.read(cx).state().clone(), cx);
    })
    .detach();
    connection
}

fn show(_: &Show, cx: &mut App) {
    show_window(cx);
}

fn hide(_: &Hide, cx: &mut App) {
    hide_window(cx);
}

fn pause(_: &Pause, cx: &mut App) {
    with_engine(cx, |engine, cx| engine.pause(cx));
}

fn resume(_: &Resume, cx: &mut App) {
    with_engine(cx, |engine, cx| engine.resume(cx));
}

fn sync_now(_: &SyncNow, cx: &mut App) {
    // #56: the tray runs the same model call the dashboard does, so the window
    // reports the click. A native menu cannot paint the reason a click could
    // not scan; the window is where that answer belongs.
    with_engine(cx, |engine, cx| {
        let _ = engine.sync_now(cx);
    });
}

/// Runs `change` on the app-scoped engine, when one exists.
fn with_engine(cx: &mut App, change: impl FnOnce(&mut EngineModel, &mut Context<EngineModel>)) {
    if let Some(handle) = cx.try_global::<EngineHandle>() {
        let engine = handle.0.clone();
        engine.update(cx, change);
    }
}

fn quit(_: &Quit, cx: &mut App) {
    if cx.has_global::<TrayHandle>() {
        let tray = cx.global::<TrayHandle>().0.clone();
        let _ = tray.close(cx);
    }
    cx.quit();
}

/// About opens the Diagnostics surface for the same reason it exists: version,
/// git revision, and paths, without a dialog this app does not have.
fn about(_: &About, cx: &mut App) {
    show_window(cx);
    let Some(window) = cx.windows().first().copied() else {
        return;
    };
    // `gpui_kit::open_window` wraps every window in its `Root`; the shell is
    // the view inside it.
    let _ = window.update(cx, |root, _, cx| {
        let shell = root
            .downcast::<base::Root>()
            .ok()
            .and_then(|root| root.read(cx).view().clone().downcast::<AppShell>().ok());
        if let Some(shell) = shell {
            shell.update(cx, |shell, cx| shell.show(ViewMode::Diagnostics, cx));
        }
    });
}

/// Window > Minimize. [`show_window`] does not undo it: `minimize_window` is
/// `miniaturize:`, and `activate_window()` is only `makeKeyAndOrderFront:`,
/// which does not deminiaturize. gpui exposes no deminiaturize, so after this
/// item, the tray's Show and a Dock click leave the window minimized; only the
/// Dock tile or the Window menu's list restores it (known limit, #64).
fn minimize(_: &Minimize, cx: &mut App) {
    if let Some(window) = cx.windows().first().copied() {
        let _ = window.update(cx, |_, window, _| window.minimize_window());
    }
}

/// Brings the main window to the front, opening it if it was closed.
pub fn show_window(cx: &mut App) {
    if let Some(window) = cx.windows().first().copied() {
        let _ = window.update(cx, |_, window, _| window.activate_window());
        cx.refresh_windows();
    } else if let Err(error) = open_main_window(cx) {
        tracing::error!(
            error = %format!("{error:#}"),
            "could not reopen the main window"
        );
    }
}

/// Closes the main window. The process stays alive in the tray.
///
/// GPUI has no cross-platform window hide, so hide closes the window and
/// `show_window` opens a fresh one. The user sees the same show/hide behaviour.
pub fn hide_window(cx: &mut App) {
    if let Some(window) = cx.windows().first().copied() {
        let _ = window.update(cx, |_, window, _| window.remove_window());
        cx.refresh_windows();
    }
}

/// The main window's bounds and limits: compact by default, resizable from
/// every edge and corner, and never smaller than the narrowest surface still
/// lays out at (issues #26 and #75).
#[must_use]
pub fn main_window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        // Centred on the primary display's visible bounds, so the taskbar
        // cannot cover the window (issue #26).
        window_bounds: Some(WindowBounds::centered(size(px(900.), px(650.)), cx)),
        is_resizable: true,
        // The narrowest surface's layout sets this floor, not the surface
        // switch (#75): the switch scrolls its seven tabs instead of clipping
        // them, so the labels no longer hold the window open. 520 px is the
        // width the widest row of any surface still fits in, the Dashboard's
        // header, whose title block beside the Sync now and Pause buttons needs
        // 509 px; 480 px is the height the Logs surface's cards keep their
        // floors at in their narrowest wrap (#54). `tests/shell.rs` stands at
        // exactly this size, so a layout that outgrows it fails there.
        window_min_size: Some(size(px(520.), px(480.))),
        ..Default::default()
    }
}

/// Opens the main window with the dashboard and settings views.
///
/// # Errors
///
/// Returns an error when the platform cannot create the window.
pub fn open_main_window(cx: &mut App) -> anyhow::Result<()> {
    let Some(connection) = cx
        .try_global::<ConnectionHandle>()
        .map(|handle| handle.0.clone())
    else {
        anyhow::bail!("the shell is not installed; call shell::init before opening a window");
    };
    let Some(upload) = cx
        .try_global::<UploadHandle>()
        .map(|handle| handle.0.clone())
    else {
        anyhow::bail!("the shell is not installed; call shell::init before opening a window");
    };
    let engine = cx
        .try_global::<EngineHandle>()
        .map(|handle| handle.0.clone())
        .ok_or_else(|| anyhow::anyhow!("the engine is not installed"))?;
    let store = cx
        .try_global::<StoreHandle>()
        .map(|handle| handle.0.clone())
        .ok_or_else(|| anyhow::anyhow!("the state store is not installed"))?;
    let (window, _view) = gpui_kit::open_window(main_window_options(cx), cx, |window, cx| {
        // The connection state is one entity both surfaces read, so the
        // dashboard's global card follows a sign-in from the settings form,
        // and reopening the window after a hide reuses the same state.
        let dashboard = cx.new(|cx| Dashboard::with_engine(engine.clone(), window, cx));
        let settings = cx.new(|cx| SettingsView::from_environment(window, cx, connection.clone()));
        // The Limits card pushes a change straight to the running engine
        // (R7.1, R7.2), and the Tagging section follows its tag state (R3.7).
        settings.update(cx, |view, cx| view.set_engine(engine.clone(), cx));
        let upload_view = cx.new(|cx| UploadView::new(upload.clone(), window, cx));
        let failed_view = cx.new(|cx| FailedView::new(engine.clone(), window, cx));
        let diagnostics_model = cx.new(|cx| {
            DiagnosticsModel::new(
                store.clone(),
                engine.clone(),
                connection.clone(),
                crate::debug_bundle::bundle_directory(),
                cx,
            )
        });
        let diagnostics_view = cx.new(|cx| DiagnosticsView::new(diagnostics_model, cx));
        let review_view = cx.new(|cx| ReviewView::new(engine.clone(), window, cx));
        cx.new(|cx| {
            AppShell::new(
                dashboard,
                settings,
                upload_view,
                failed_view,
                diagnostics_view,
                review_view,
                connection,
                cx,
            )
        })
    })?;
    window.update(cx, |_, window, cx| {
        crate::theme::follow_system_theme(window, cx)
    })?;
    Ok(())
}

/// Which surface the main window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode {
    /// The status dashboard.
    Dashboard,
    /// The settings surface.
    Settings,
    /// The manual one-file upload.
    Upload,
    /// The parked items and their manual retry (R6.5).
    Failed,
    /// The missing files and their keep-or-trash decision (R5.3).
    Review,
    /// The log file and its level control (R10.3).
    Logs,
    /// The server, the state database, and the throughput (R10.2, R10.4).
    Diagnostics,
}

impl ViewMode {
    /// Every surface, in the order the switch shows them.
    const ALL: [Self; 7] = [
        Self::Dashboard,
        Self::Upload,
        Self::Failed,
        Self::Review,
        Self::Settings,
        Self::Logs,
        Self::Diagnostics,
    ];

    /// The element id this surface's tab carries.
    ///
    /// The tests that click their way to one surface find the switch by these
    /// ids, so they are part of the surface's contract, not a rendering detail.
    const fn nav_id(self) -> &'static str {
        match self {
            Self::Dashboard => "nav-dashboard",
            Self::Upload => "nav-upload",
            Self::Failed => "nav-failed",
            Self::Review => "nav-review",
            Self::Settings => "nav-settings",
            Self::Logs => "nav-logs",
            Self::Diagnostics => "nav-diagnostics",
        }
    }

    /// The tab's label, which is also what assistive technology announces.
    const fn label(self) -> &'static str {
        match self {
            Self::Dashboard => "Dashboard",
            Self::Upload => "Upload",
            Self::Failed => "Failed items",
            Self::Review => "Review",
            Self::Settings => "Settings",
            Self::Logs => "Logs",
            Self::Diagnostics => "Diagnostics",
        }
    }

    /// This surface's place in the switch, counted from zero.
    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|mode| *mode == self)
            .expect("every ViewMode is in ViewMode::ALL")
    }
}

/// The environment variable that pins a folder detail open (#43).
///
/// Set it to a source folder's stored path and the dashboard opens that
/// folder's detail once the store has loaded it, so a screenshot run captures
/// the dialog. A product run never sets it.
pub const FORCE_DETAIL_ENV: &str = "IMMICH_SYNC_FOLDER_DETAIL";

/// The element id of the strip the surface switch's tabs scroll in.
///
/// It clips the tabs at a narrow window and is what a wheel over the switch
/// scrolls, so it names the switch's viewport rather than any one surface.
const NAV_STRIP: &str = "nav-tabs";

/// The environment variable that pins the visible surface.
///
/// Set it to `dashboard`, `settings`, `upload`, `failed`, `review`, `logs`, or
/// `diagnostics` to open the window on that surface and skip the observer that
/// would otherwise decide. A screenshot run uses it so the capture is
/// reproducible.
pub const FORCE_VIEW_ENV: &str = "IMMICH_SYNC_VIEW";

/// The surface a pinned value names, when one is set.
#[must_use]
pub fn forced_view() -> Option<ViewMode> {
    view_for_name(std::env::var(FORCE_VIEW_ENV).ok()?.as_str())
}

/// The surface a value names, when it names one.
#[must_use]
pub fn view_for_name(value: &str) -> Option<ViewMode> {
    match value {
        "dashboard" => Some(ViewMode::Dashboard),
        "settings" => Some(ViewMode::Settings),
        "upload" => Some(ViewMode::Upload),
        "failed" => Some(ViewMode::Failed),
        "review" => Some(ViewMode::Review),
        "logs" => Some(ViewMode::Logs),
        "diagnostics" => Some(ViewMode::Diagnostics),
        _ => None,
    }
}

/// The main window's view: a view switch above the dashboard, the settings, the
/// upload, the logs, or the diagnostics.
pub struct AppShell {
    mode: ViewMode,
    /// Where the switch's tabs are scrolled to. The strip scrolls instead of
    /// clipping its tabs, so the tab of the surface on screen has to be brought
    /// into sight at a narrow window (#75).
    nav_scroll: ScrollHandle,
    /// The tab the switch last revealed. Compared against, so a frame that only
    /// repaints does not undo the user's own scrolling.
    revealed: Option<usize>,
    dashboard: Entity<Dashboard>,
    settings: Entity<SettingsView>,
    logs: Entity<LogsView>,
    upload: Entity<UploadView>,
    failed: Entity<FailedView>,
    diagnostics: Entity<DiagnosticsView>,
    review: Entity<ReviewView>,
    _connection: Entity<ConnectionModel>,
    _connection_subscription: Subscription,
    _dashboard_subscription: Subscription,
}

impl AppShell {
    /// A shell that starts on the dashboard, or on the surface
    /// [`FORCE_VIEW_ENV`] pins, and mirrors the connection state onto the
    /// global status card (R8.1).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dashboard: Entity<Dashboard>,
        settings: Entity<SettingsView>,
        upload: Entity<UploadView>,
        failed: Entity<FailedView>,
        diagnostics: Entity<DiagnosticsView>,
        review: Entity<ReviewView>,
        connection: Entity<ConnectionModel>,
        cx: &mut Context<Self>,
    ) -> Self {
        // The tray is observed at App scope in `install_connection_with`, so
        // it keeps working while this window (and this observer) is gone.
        let subscription = cx.observe(&connection, |this, connection, cx| {
            let summary = connection.read(cx).summary();
            this.dashboard.update(cx, |dashboard, cx| {
                dashboard.set_connection(Some(summary), cx)
            });
        });
        let summary = connection.read(cx).summary();
        dashboard.update(cx, |dashboard, cx| {
            dashboard.set_connection(Some(summary), cx);
        });
        let logs = cx.new(|cx| LogsView::new(settings.clone(), cx));
        let mode = forced_view().unwrap_or(ViewMode::Dashboard);
        // A pinned Logs surface is on screen from the first frame, so its live
        // tail starts with it.
        if mode == ViewMode::Logs {
            logs.update(cx, |logs, cx| logs.set_visible(true, cx));
        }
        // #43: the folder detail cannot switch surfaces itself, so the
        // dashboard reports the request and the shell opens it.
        let dashboard_subscription = cx.subscribe(
            &dashboard,
            |this, _dashboard, event: &DashboardEvent, cx| {
                let DashboardEvent::OpenView(mode, folder) = event;
                this.show_filtered(*mode, folder.clone(), cx);
            },
        );
        let mut shell = Self {
            mode,
            nav_scroll: ScrollHandle::new(),
            revealed: None,
            dashboard,
            settings,
            logs,
            upload,
            failed,
            diagnostics,
            review,
            _connection: connection,
            _connection_subscription: subscription,
            _dashboard_subscription: dashboard_subscription,
        };
        // A pinned surface (#43) can be one of the last tabs, so the first frame
        // already scrolls its tab into sight.
        shell.reveal(shell.mode);
        shell
    }

    /// The visible surface.
    #[must_use]
    pub fn mode(&self) -> ViewMode {
        self.mode
    }

    /// The dashboard, so a test can drive the folder detail it owns (#43).
    #[must_use]
    pub fn dashboard(&self) -> Entity<Dashboard> {
        self.dashboard.clone()
    }

    /// Switches the visible surface.
    pub fn show(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            // The live tail reads only while its surface is on screen, so the
            // shell starts it on the way in and stops it on the way out
            // (R10.3).
            self.logs
                .update(cx, |logs, cx| logs.set_visible(mode == ViewMode::Logs, cx));
            self.reveal(mode);
        }
        cx.notify();
    }

    /// Scrolls the switch until `mode`'s tab is in sight.
    ///
    /// The strip scrolls rather than clips, so at a window too narrow for the
    /// seven labels the tab of the surface on screen could sit past the strip's
    /// edge; the tray, a pinned surface, and a folder-detail link all switch
    /// without the pointer ever touching the switch (#75).
    fn reveal(&mut self, mode: ViewMode) {
        self.revealed = Some(mode.index());
        self.nav_scroll.scroll_to_item(mode.index());
    }

    /// Opens a list view narrowed to one source folder (#41).
    ///
    /// The folder detail calls this to link into the failed items or the
    /// deletion review for its folder (#43). A surface without a folder filter
    /// ignores the folder and simply opens.
    pub fn show_filtered(&mut self, mode: ViewMode, folder: String, cx: &mut Context<Self>) {
        match mode {
            ViewMode::Failed => self
                .failed
                .update(cx, |view, cx| view.select_folder(folder, cx)),
            ViewMode::Review => self
                .review
                .update(cx, |view, cx| view.select_folder(folder, cx)),
            _ => {}
        }
        self.show(mode, cx);
    }

    /// The Logs surface. Its live tail runs only while [`Self::show`] has it
    /// visible.
    #[must_use]
    pub fn logs(&self) -> Entity<LogsView> {
        self.logs.clone()
    }

    /// The surface switch: one tab per surface, in a strip that scrolls when the
    /// window is too narrow for all seven labels (#75).
    ///
    /// Every tab is its own tab stop, so the keyboard reaches every surface and
    /// Enter or Space shows it (GPUI turns a key on the focused element into a
    /// click). The tab of the surface on screen is filled and set in semibold,
    /// and reports itself as selected, so the selection is not colour alone.
    ///
    /// The tabs ride in the kit's [`SurfaceTabs`] list, but each one is its own
    /// element rather than a `base::Tab`: in 0.8.1 that tab renames itself to
    /// its index and observes itself, so it can carry neither the stable id the
    /// per-surface tests click nor the focus handle that makes it reachable.
    /// The role, the label, the position and the selected state below are the
    /// same ones `base::Tab` writes.
    fn switch(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (background, border) = (theme.background, theme.border);
        let (primary, primary_foreground) = (theme.primary, theme.primary_foreground);
        let (foreground, muted_foreground) = (theme.foreground, theme.muted_foreground);
        let (accent, radius) = (theme.accent, theme.radius);
        // The focus handles are claimed first: the tab loop below needs `cx` for
        // its listeners, and a tab's handle has to outlive that borrow.
        let focus: Vec<FocusHandle> = ViewMode::ALL
            .iter()
            .map(|mode| {
                window
                    .use_keyed_state(mode.nav_id(), cx, |_, cx| cx.focus_handle().tab_stop(true))
                    .read(cx)
                    .clone()
            })
            .collect();
        // A tab that holds the keyboard is scrolled into the strip. The strip
        // clips what does not fit, and a clipped tab would clip its focus ring
        // with it.
        if let Some(ix) = focus.iter().position(|handle| handle.is_focused(window))
            && self.revealed != Some(ix)
        {
            self.revealed = Some(ix);
            self.nav_scroll.scroll_to_item(ix);
        }

        let mut tabs = Vec::with_capacity(ViewMode::ALL.len());
        for (ix, (mode, focus)) in ViewMode::ALL.iter().zip(&focus).enumerate() {
            let mode = *mode;
            let selected = mode == self.mode;
            tabs.push(
                div()
                    .id(mode.nav_id())
                    .test_support()
                    .role(Role::Tab)
                    .aria_label(mode.label())
                    // Both states are always reported, so a tab that is not the
                    // surface on screen says so instead of saying nothing.
                    .aria_selected(selected)
                    .aria_position_in_set(ix + 1)
                    .aria_size_of_set(ViewMode::ALL.len())
                    .track_focus(focus)
                    .on_click(cx.listener(move |this, _, _, cx| this.show(mode, cx)))
                    .flex()
                    .items_center()
                    .px_3()
                    .py_1()
                    .rounded(radius)
                    .text_sm()
                    .text_color(if selected {
                        primary_foreground
                    } else {
                        muted_foreground
                    })
                    .when(selected, |tab| {
                        tab.bg(primary).font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(!selected, |tab| {
                        tab.hover(move |style| style.bg(accent).text_color(foreground))
                    })
                    .child(mode.label())
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .px_6()
            .py_2()
            .bg(background)
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .id(NAV_STRIP)
                    .test_support()
                    .flex()
                    .items_center()
                    .flex_1()
                    .min_w_0()
                    .relative()
                    .child(
                        SurfaceTabs::new("nav-switch")
                            .flex()
                            .items_center()
                            .gap_1()
                            .flex_1()
                            .min_w_0()
                            .overflow_x_scroll()
                            .track_scroll(&self.nav_scroll)
                            .children(tabs),
                    )
                    // Over the strip, not inside it, so a plain wheel's vertical
                    // lines scroll the tabs sideways the way the kit's tab bars
                    // do; tab indices stay the strip's own children.
                    .child(ScrollableMask::vertical_wheel(&self.nav_scroll)),
            )
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let background = theme.background;
        let foreground = theme.foreground;
        let content: AnyElement = match self.mode {
            ViewMode::Dashboard => self.dashboard.clone().into_any_element(),
            ViewMode::Settings => self.settings.clone().into_any_element(),
            ViewMode::Upload => self.upload.clone().into_any_element(),
            ViewMode::Failed => self.failed.clone().into_any_element(),
            ViewMode::Review => self.review.clone().into_any_element(),
            ViewMode::Logs => self.logs.clone().into_any_element(),
            ViewMode::Diagnostics => self.diagnostics.clone().into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(self.switch(window, cx))
            .child(content)
    }
}

/// Builds the tray icon and stores it for later actions.
///
/// # Errors
///
/// Returns an error when the platform cannot register the tray icon.
pub fn install_tray(cx: &mut App) -> gpui_tray::Result<()> {
    let builder = Tray::builder()
        .icon(icon()?)
        .tooltip("Immich Sync")
        .on_activate(Show)
        .menu(|_| tray_menu());
    // The macOS menu bar has no room for a text title beside the icon; Windows
    // and Linux keep theirs (issue #64).
    #[cfg(not(target_os = "macos"))]
    let builder = builder.title("Immich Sync");
    let tray = builder.build(cx)?;
    cx.set_global(TrayHandle(tray));
    // The models are created before the window, so push the state they already
    // hold instead of waiting for the first transition.
    show_status_on_tray(cx);
    Ok(())
}

/// The tray menu: Show, Pause, Resume, Sync now, and Quit (R8.3).
#[must_use]
pub fn tray_menu() -> Vec<MenuItem> {
    vec![
        MenuItem::action("Show", Show),
        MenuItem::separator(),
        MenuItem::action("Pause", Pause),
        MenuItem::action("Resume", Resume),
        MenuItem::action("Sync now", SyncNow),
        MenuItem::separator(),
        MenuItem::action("Quit", Quit),
    ]
}

/// The macOS menu bar: the app menu, Edit, and Window (issue #64).
///
/// GPUI has no default bar and no prebuilt menus, so every item is spelled out
/// from the actions [`tray_menu`] already uses. [`init`] installs it on macOS
/// only.
///
/// Built on macOS and under test only; elsewhere it would be a dead menu.
#[cfg(any(test, target_os = "macos"))]
#[must_use]
fn app_menu() -> Vec<Menu> {
    vec![
        Menu::new("Immich Sync").items([
            MenuItem::action("About Immich Sync", About),
            MenuItem::separator(),
            MenuItem::action("Quit Immich Sync", Quit),
        ]),
        // `os_action` gives AppKit the editing selector while the item still
        // carries the action the text inputs already bind to cmd-x/c/v/a.
        Menu::new("Edit").items([
            MenuItem::os_action("Cut", base::input::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", base::input::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", base::input::Paste, OsAction::Paste),
            MenuItem::os_action("Select All", base::input::SelectAll, OsAction::SelectAll),
        ]),
        // AppKit gives the menu titled "Window" the window roles and the window
        // list; Close is this app's Hide, which removes the window as Cmd+W does.
        Menu::new("Window").items([
            MenuItem::action("Close", Hide),
            MenuItem::action("Minimize", Minimize),
        ]),
    ]
}

/// The tray tooltip for one connection state (R1.4).
///
/// The full message, not just the summary, so a needs-attention state shows its
/// reason from the tray when the window is behind another one.
#[must_use]
pub fn tray_tooltip(state: &ConnectionState) -> String {
    format!("Immich Sync — {}", state.message())
}

/// The tray tooltip for the live activity, the connection, the review count,
/// and the progress (R8.2, R5.4).
#[must_use]
pub fn tray_status_tooltip(
    activity: Activity,
    state: &ConnectionState,
    counts: FolderCounts,
    last_error: Option<&str>,
) -> String {
    let total = counts.uploaded + counts.pending;
    let mut text = format!("Immich Sync — {} — {}", activity.label(), state.message());
    if total > 0 {
        text.push_str(&format!(" — {}/{} uploaded", counts.uploaded, total));
    }
    if counts.review > 0 {
        text.push_str(&format!(" — {} to review", counts.review));
    }
    if let Some(error) = last_error {
        text.push_str(&format!(" — last error: {error}"));
    }
    text
}

/// Shows a connection state on the tray tooltip, when a tray is installed.
///
/// The tray can be absent (a desktop without one, or a failed install), so a
/// missing tray is not an error.
pub fn show_connection_on_tray(_state: &ConnectionState, cx: &mut App) {
    show_status_on_tray(cx);
}

/// Pushes the engine's activity and the connection state to the tray tooltip
/// (R8.2).
///
/// The status poll runs four times a second, so an unchanged string is not
/// written to the platform again.
pub fn show_status_on_tray(cx: &mut App) {
    let Some(tray) = cx.try_global::<TrayHandle>().map(|handle| handle.0.clone()) else {
        return;
    };
    let Some(engine) = cx
        .try_global::<EngineHandle>()
        .map(|handle| handle.0.clone())
    else {
        return;
    };
    let state = cx
        .try_global::<ConnectionHandle>()
        .map(|handle| handle.0.read(cx).state().clone())
        .unwrap_or(ConnectionState::NotConfigured);
    let status = engine.read(cx).status().clone();
    let tooltip = tray_status_tooltip(
        status.activity,
        &state,
        status.counts,
        status.last_error.as_deref(),
    );
    let changed = cx.try_global::<TrayTooltip>().is_none_or(|cache| {
        let mut cached = cache.0.lock().expect("the tray tooltip lock");
        if *cached == tooltip {
            return false;
        }
        *cached = tooltip.clone();
        true
    });
    if changed {
        let _ = tray.set_tooltip(Some(tooltip), cx);
    }
}

/// A solid circular tray icon, drawn in code so the shell needs no asset.
fn icon() -> gpui_tray::Result<Icon> {
    let size = tray_icon_size();
    Icon::from_rgba(tray_icon_rgba(size), size, size)
}

/// The status item's pixel size: an 18 pt macOS menu-bar icon, 32 px elsewhere.
fn tray_icon_size() -> u32 {
    if cfg!(target_os = "macos") { 18 } else { 32 }
}

/// A solid disc centred in a transparent square of `size` pixels.
///
/// One colour and no thin strokes is what keeps it legible on a light and a
/// dark menu bar, because `gpui-tray` 0.1.4 cannot mark the image as a template.
// shortcut: one coloured 18 px icon. gpui-tray 0.1.4 has no `setTemplate` and
// no Retina representation, so a monochrome icon could not invert and a second
// image would have nowhere to go; #64 records the gap and the upstream request
// (a template flag plus a 2x image, then ship an 18 + 36 px template) is still
// manual.
fn tray_icon_rgba(size: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let centre = size as i32 / 2 - 1;
    let radius = centre - 1;
    for y in 0..size {
        for x in 0..size {
            let offset = ((y * size + x) * 4) as usize;
            let dx = x as i32 - centre;
            let dy = y as i32 - centre;
            if dx * dx + dy * dy <= radius * radius {
                rgba[offset..offset + 4].copy_from_slice(&[45, 105, 220, 255]);
            }
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::{
        About, Hide, Minimize, Quit, app_menu, hide_window, install_connection_with,
        install_models, show_window, tray_icon_rgba, tray_icon_size,
    };
    use gpui_kit::{Action, MenuItem, OsAction, TestAppContext, base};
    use std::cell::RefCell;
    use std::rc::Rc;
    use sync_core::connection::{Attention, ConnectionState};

    /// The labels and OS actions of a menu's real items, in order.
    fn menu_items(menu: &gpui_kit::Menu) -> Vec<(&str, Option<OsAction>)> {
        menu.items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action {
                    name, os_action, ..
                } => Some((name.as_ref(), *os_action)),
                _ => None,
            })
            .collect()
    }

    fn carries(item: &MenuItem, expected: &dyn Action) -> bool {
        matches!(item, MenuItem::Action { action, .. } if action.partial_eq(expected))
    }

    /// #64: the macOS menu bar is the app menu, the Edit menu, and the Window
    /// menu, built from the same actions the tray menu uses. AppKit gives the
    /// items below the first menu no OS role, so About and Quit are plain app
    /// actions and Close is this app's Hide.
    #[test]
    fn the_app_menu_is_the_macos_app_edit_and_window_bar() {
        let menus = app_menu();
        let names: Vec<&str> = menus.iter().map(|menu| menu.name.as_ref()).collect();
        assert_eq!(names, ["Immich Sync", "Edit", "Window"]);

        assert!(
            menu_items(&menus[0]) == [("About Immich Sync", None), ("Quit Immich Sync", None)],
            "the app menu holds About and Quit"
        );
        assert!(carries(&menus[0].items[0], &About));
        assert!(carries(&menus[0].items[2], &Quit));

        assert!(
            menu_items(&menus[1])
                == [
                    ("Cut", Some(OsAction::Cut)),
                    ("Copy", Some(OsAction::Copy)),
                    ("Paste", Some(OsAction::Paste)),
                    ("Select All", Some(OsAction::SelectAll)),
                ],
            "the Edit menu carries the four AppKit editing roles"
        );
        // The same actions the text inputs bind: the item sends the AppKit
        // selector and dispatches the action the input already handles.
        assert!(carries(&menus[1].items[0], &base::input::Cut));
        assert!(carries(&menus[1].items[1], &base::input::Copy));
        assert!(carries(&menus[1].items[2], &base::input::Paste));
        assert!(carries(&menus[1].items[3], &base::input::SelectAll));

        assert!(
            menu_items(&menus[2]) == [("Close", None), ("Minimize", None)],
            "the Window menu closes and minimizes"
        );
        assert!(carries(&menus[2].items[0], &Hide));
        assert!(carries(&menus[2].items[1], &Minimize));
    }

    /// #64: the status item is 18 pt on macOS and 32 px elsewhere, and the art
    /// is a solid disc: one colour with no thin strokes, which is what stays
    /// legible on both menu bar appearances because 0.1.4 cannot mark the
    /// image as a template.
    #[test]
    fn the_tray_icon_is_menu_bar_sized_on_macos() {
        #[cfg(target_os = "macos")]
        assert_eq!(tray_icon_size(), 18, "an 18 pt menu-bar icon");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(tray_icon_size(), 32, "a 32 px status icon");

        for size in [18, 32] {
            let rgba = tray_icon_rgba(size);
            assert_eq!(rgba.len(), (size * size * 4) as usize);
            assert_eq!(rgba[0..4], [0, 0, 0, 0], "the corner stays transparent");
            let centre = ((size / 2 * size + size / 2) * 4) as usize;
            assert_eq!(
                rgba[centre..centre + 4],
                [45, 105, 220, 255],
                "the disc covers the centre"
            );
        }
    }

    #[gpui_kit::test]
    fn the_tray_mapping_follows_a_state_change_with_no_window(cx: &mut TestAppContext) {
        let seen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let connection = cx.update(|cx| {
            gpui_kit::init(cx);
            let seen = seen.clone();
            let connection = install_connection_with(cx, move |state, _| {
                seen.borrow_mut().push(state.message());
            });
            // `open_main_window` builds every surface, so the store, engine,
            // and upload models the window needs are installed here too, as
            // `init` does.
            install_models(cx, connection.clone());
            connection
        });

        // A window opens and closes; the observer lives at App scope, not in
        // the root view that `remove_window` drops.
        cx.update(show_window);
        cx.run_until_parked();
        assert_eq!(cx.read(|cx| cx.windows().len()), 1);
        cx.update(hide_window);
        cx.run_until_parked();
        assert_eq!(cx.read(|cx| cx.windows().len()), 0);

        let attention = ConnectionState::NeedsAttention(Attention::Rejected);
        cx.update(|cx| {
            connection.update(cx, |model, cx| model.set_state(attention.clone(), cx));
        });
        cx.run_until_parked();

        assert_eq!(
            seen.borrow().last().map(String::as_str),
            Some(attention.message().as_str()),
            "a state change with no window still reached the tray mapping"
        );
    }
}
