//! The tray shell: one GPUI window, one tray icon, and the actions they share.

use gpui_kit::*;
use gpui_tray::{Icon, Tray};

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

/// Opens the main window.
///
/// # Errors
///
/// Returns an error when the platform cannot create the window.
pub fn open_main_window(cx: &mut App) -> anyhow::Result<()> {
    gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Root))?;
    Ok(())
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

struct Root;

impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x101828))
            .text_color(rgb(0xffffff))
            .p_12()
            .gap_6()
            .child(
                div()
                    .text_color(rgb(0x22d3ee))
                    .text_2xl()
                    .child("Immich Sync"),
            )
            .child(
                div()
                    .text_color(rgb(0xfacc15))
                    .text_xl()
                    .child("Tray shell walking skeleton"),
            )
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
