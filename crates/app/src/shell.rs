//! The tray shell: one GPUI window, one tray icon, and the actions they share.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_tray::{Icon, Tray};

use crate::connection::ConnectionModel;
use crate::dashboard::Dashboard;
use crate::settings_view::SettingsView;
use sync_core::connection::{ConnectionState, ServerProfile};

actions!(immich_sync, [Show, Hide, Quit]);

/// Global holding the tray icon so [`Quit`] can remove it.
struct TrayHandle(Tray);
impl Global for TrayHandle {}

/// Registers the shell actions.
///
/// Call it once, after [`gpui_kit::init`].
pub fn init(cx: &mut App) {
    // The tray owns the process lifetime: closing the window must not quit.
    // Without this, GPUI quits when the last window closes on Windows and
    // Linux, which would end the tray app.
    cx.set_quit_mode(QuitMode::Explicit);
    cx.on_action(show).on_action(hide).on_action(quit);
}

fn show(_: &Show, cx: &mut App) {
    show_window(cx);
}

fn hide(_: &Hide, cx: &mut App) {
    hide_window(cx);
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
        eprintln!("immich-sync: could not open the main window: {error:#}");
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

/// Opens the main window with the dashboard and settings views.
///
/// # Errors
///
/// Returns an error when the platform cannot create the window.
pub fn open_main_window(cx: &mut App) -> anyhow::Result<()> {
    let (window, _view) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
        // The connection state is one entity both surfaces read, so the
        // dashboard's global card follows a sign-in from the settings form.
        let connection = cx.new(|_| {
            ConnectionModel::new(
                ServerProfile::default(),
                crate::credentials::platform_credential_store(),
            )
        });
        let dashboard = cx.new(|_| Dashboard::new(crate::status::sample()));
        let settings = cx.new(|cx| SettingsView::from_environment(window, cx, connection.clone()));
        cx.new(|cx| AppShell::new(dashboard, settings, connection, cx))
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
}

/// The main window's view: a view switch above the dashboard or the settings.
pub struct AppShell {
    mode: ViewMode,
    dashboard: Entity<Dashboard>,
    settings: Entity<SettingsView>,
    _connection: Entity<ConnectionModel>,
    _connection_subscription: Subscription,
}

impl AppShell {
    /// A shell that starts on the dashboard and mirrors the connection state
    /// onto the global status card (R8.1).
    pub fn new(
        dashboard: Entity<Dashboard>,
        settings: Entity<SettingsView>,
        connection: Entity<ConnectionModel>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&connection, |this, connection, cx| {
            let summary = connection.read(cx).summary();
            let state = connection.read(cx).state().clone();
            this.dashboard.update(cx, |dashboard, cx| {
                dashboard.set_connection(Some(summary), cx)
            });
            show_connection_on_tray(&state, cx);
        });
        let summary = connection.read(cx).summary();
        let state = connection.read(cx).state().clone();
        dashboard.update(cx, |dashboard, cx| {
            dashboard.set_connection(Some(summary), cx);
        });
        show_connection_on_tray(&state, cx);
        Self {
            mode: ViewMode::Dashboard,
            dashboard,
            settings,
            _connection: connection,
            _connection_subscription: subscription,
        }
    }

    /// Switches the visible surface.
    pub fn show(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        self.mode = mode;
        cx.notify();
    }

    /// The view switch. Both buttons are keyboard reachable tab stops.
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
                "nav-settings",
                "Settings",
                ViewMode::Settings,
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
    Ok(())
}

/// The tray menu: show, hide, and quit.
#[must_use]
pub fn tray_menu() -> Vec<MenuItem> {
    vec![
        MenuItem::action("Show", Show),
        MenuItem::action("Hide", Hide),
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

/// Shows a connection state on the tray tooltip, when a tray is installed.
///
/// The tray can be absent (a desktop without one, or a failed install), so a
/// missing tray is not an error.
pub fn show_connection_on_tray(state: &ConnectionState, cx: &mut App) {
    let Some(tray) = cx.try_global::<TrayHandle>().map(|handle| handle.0.clone()) else {
        return;
    };
    let _ = tray.set_tooltip(Some(tray_tooltip(state)), cx);
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
