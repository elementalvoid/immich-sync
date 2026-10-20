//! The tray shell: one GPUI window, one tray icon, and the actions they share.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_tray::{Icon, Tray};

use crate::connection::ConnectionModel;
use crate::dashboard::Dashboard;
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

actions!(immich_sync, [Show, Hide, Quit, Pause, Resume, SyncNow]);

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
    cx.set_app_identity("com.elementalvoid.immich-sync", "Immich Sync");
    cx.on_action(show)
        .on_action(hide)
        .on_action(quit)
        .on_action(pause)
        .on_action(resume)
        .on_action(sync_now);
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
    with_engine(cx, |engine, cx| engine.sync_now(cx));
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
/// every edge and corner, and never smaller than the shell still works at
/// (issue #26).
#[must_use]
pub fn main_window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        // Centred on the primary display's visible bounds, so the taskbar
        // cannot cover the window (issue #26).
        window_bounds: Some(WindowBounds::centered(size(px(900.), px(650.)), cx)),
        is_resizable: true,
        // Narrower and the seven view tabs no longer fit on one row.
        window_min_size: Some(size(px(720.), px(480.))),
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
        // (R7.1, R7.2).
        settings.update(cx, |view, _| view.set_engine(engine.clone()));
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
    dashboard: Entity<Dashboard>,
    settings: Entity<SettingsView>,
    logs: Entity<LogsView>,
    upload: Entity<UploadView>,
    failed: Entity<FailedView>,
    diagnostics: Entity<DiagnosticsView>,
    review: Entity<ReviewView>,
    _connection: Entity<ConnectionModel>,
    _connection_subscription: Subscription,
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
        Self {
            mode: forced_view().unwrap_or(ViewMode::Dashboard),
            dashboard,
            settings,
            logs,
            upload,
            failed,
            diagnostics,
            review,
            _connection: connection,
            _connection_subscription: subscription,
        }
    }

    /// Switches the visible surface.
    pub fn show(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        self.mode = mode;
        cx.notify();
    }

    /// The view switch. Every button is a keyboard reachable tab stop.
    fn switch(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let background = theme.background;
        let border = theme.border;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_6()
            .py_2()
            .bg(background)
            .border_b_1()
            .border_color(border)
            .child(view_button(
                "nav-dashboard",
                "Dashboard",
                ViewMode::Dashboard,
                self.mode,
                cx,
            ))
            .child(view_button(
                "nav-upload",
                "Upload",
                ViewMode::Upload,
                self.mode,
                cx,
            ))
            .child(view_button(
                "nav-failed",
                "Failed items",
                ViewMode::Failed,
                self.mode,
                cx,
            ))
            .child(view_button(
                "nav-review",
                "Review",
                ViewMode::Review,
                self.mode,
                cx,
            ))
            .child(view_button(
                "nav-settings",
                "Settings",
                ViewMode::Settings,
                self.mode,
                cx,
            ))
            .child(view_button(
                "nav-logs",
                "Logs",
                ViewMode::Logs,
                self.mode,
                cx,
            ))
            .child(view_button(
                "nav-diagnostics",
                "Diagnostics",
                ViewMode::Diagnostics,
                self.mode,
                cx,
            ))
    }
}

/// One view-switch button: the active surface is prominent, the other is not.
fn view_button(
    id: &'static str,
    label: &'static str,
    mode: ViewMode,
    current: ViewMode,
    cx: &mut Context<AppShell>,
) -> impl IntoElement + use<> {
    let active = mode == current;
    Button::new(id)
        .label(label)
        .when(active, |button| button.primary())
        .when(!active, |button| button.ghost())
        .on_click(cx.listener(move |this, _, _, cx| this.show(mode, cx)))
}

impl Render for AppShell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(self.switch(cx))
            .child(content)
    }
}

/// Builds the tray icon and stores it for later actions.
///
/// # Errors
///
/// Returns an error when the platform cannot register the tray icon.
pub fn install_tray(cx: &mut App) -> gpui_tray::Result<()> {
    let tray = Tray::builder()
        .icon(icon()?)
        .title("Immich Sync")
        .tooltip("Immich Sync")
        .on_activate(Show)
        .menu(|_| tray_menu())
        .build(cx)?;
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
    const SIZE: u32 = 32;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let offset = ((y * SIZE + x) * 4) as usize;
            let dx = x as i32 - 15;
            let dy = y as i32 - 15;
            if dx * dx + dy * dy <= 14 * 14 {
                rgba[offset..offset + 4].copy_from_slice(&[45, 105, 220, 255]);
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE)
}

#[cfg(test)]
mod tests {
    use super::{hide_window, install_connection_with, install_models, show_window};
    use gpui_kit::TestAppContext;
    use std::cell::RefCell;
    use std::rc::Rc;
    use sync_core::connection::{Attention, ConnectionState};

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
