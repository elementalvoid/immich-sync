//! Headless tests for the settings surface and the shell's view switch.
//!
//! The harness can drive keyboard focus and native accessibility properties; it
//! cannot read pixels, so the focus *ring* itself is not asserted here.

use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Point, TestAppContext, Window,
    WindowBounds, WindowOptions, px, size,
};
use immich_sync::autostart::Autostart;
use immich_sync::dashboard::Dashboard;
use immich_sync::paths::AppPaths;
use immich_sync::settings::{Settings, SettingsStore};
use immich_sync::settings_view::{FieldKey, SettingsView};
use immich_sync::shell::AppShell;
use immich_sync::status;

/// A stand-in for the OS autostart mechanism, so the settings-to-OS wiring is
/// testable on every platform. On Linux the real XDG backend is covered by
/// `tests/settings.rs`; the Windows Run key stays a manual check.
struct FakeAutostart {
    enabled: Cell<bool>,
    refuse: bool,
}

impl FakeAutostart {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            enabled: Cell::new(false),
            refuse: false,
        })
    }

    fn refusing() -> Rc<Self> {
        Rc::new(Self {
            enabled: Cell::new(false),
            refuse: true,
        })
    }
}

impl Autostart for FakeAutostart {
    fn is_enabled(&self) -> anyhow::Result<bool> {
        Ok(self.enabled.get())
    }

    fn set_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        if self.refuse {
            anyhow::bail!("the OS refused the autostart change");
        }
        self.enabled.set(enabled);
        Ok(())
    }
}

fn temporary_paths(dir: &Path) -> AppPaths {
    AppPaths {
        config_file: dir.join("config").join("settings.toml"),
        state_db: dir.join("data").join("state.db"),
        autostart_dir: dir.join("config").join("autostart"),
    }
}

fn window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            size: size(px(1200.), px(900.)),
        })),
        ..Default::default()
    }
}

fn open_settings(
    cx: &mut TestAppContext,
    store: SettingsStore,
    paths: AppPaths,
    autostart: Rc<dyn Autostart>,
) -> AnyWindowHandle {
    cx.update(|cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(window_options(), cx, |window, cx| {
            cx.new(|cx| SettingsView::new(store, paths, autostart, window, cx))
        })
        .expect("open the settings window")
        .0
    })
}

fn open_shell(
    cx: &mut TestAppContext,
    store: SettingsStore,
    paths: AppPaths,
    autostart: Rc<dyn Autostart>,
) -> AnyWindowHandle {
    cx.update(|cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(window_options(), cx, |window, cx| {
            let dashboard = cx.new(|_| Dashboard::new(status::sample()));
            let settings = cx.new(|cx| SettingsView::new(store, paths, autostart, window, cx));
            cx.new(|_| AppShell::new(dashboard, settings))
        })
        .expect("open the shell window")
        .0
    })
}

/// Moves focus to the next tab stop, as the Root's Tab binding does, and
/// redraws so the observed focus state is current.
fn tab(window: &mut Window, cx: &mut App) {
    window.focus_next(cx);
    window.render_frame(cx);
}

/// Tabs until `id` holds the keyboard focus.
fn tab_to(window: &mut Window, id: &'static str, cx: &mut App) {
    for _ in 0..16 {
        if window.find(id).focused() == Some(true) {
            return;
        }
        tab(window, cx);
    }
    assert_eq!(
        window.find(id).focused(),
        Some(true),
        "{id} is not keyboard reachable"
    );
}

#[test]
fn field_keys_map_settings_to_the_displayed_text() {
    let mut settings = Settings::default();

    assert_eq!(FieldKey::Bandwidth.value(&settings), 0, "0 MB/s is unlimited");
    assert_eq!(FieldKey::Concurrency.value(&settings), 4);
    assert_eq!(FieldKey::ScanInterval.value(&settings), 30);
    assert_eq!(FieldKey::AttemptCap.value(&settings), 8);

    assert_eq!(FieldKey::Concurrency.label(), "Concurrent uploads");
    assert_eq!(FieldKey::Concurrency.unit(), "uploads");
    assert_eq!(FieldKey::Bandwidth.unit(), "MB/s");
    assert_eq!(FieldKey::ScanInterval.unit(), "minutes");

    FieldKey::Bandwidth.set(&mut settings, 3);
    assert_eq!(
        settings.bandwidth_bytes_per_second, 3_000_000,
        "the UI's MB/s is stored as bytes per second (R7.1)"
    );
    assert_eq!(FieldKey::Bandwidth.value(&settings), 3);

    FieldKey::Concurrency.set(&mut settings, 999);
    assert_eq!(settings.concurrency, 64, "out-of-range input clamps");
    FieldKey::Concurrency.set(&mut settings, 0);
    assert_eq!(settings.concurrency, 1);
}

#[gpui_kit::test]
fn settings_view_shows_the_stored_values(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let autostart = FakeAutostart::new();
    let window = open_settings(cx, store, paths, autostart);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);

        assert_eq!(
            window.find("settings-autostart").checked(),
            Some(false),
            "autostart defaults to off (R8.5)"
        );
        assert!(window.find("settings-general").visible());
        assert!(window.find("settings-limits").visible());
        assert!(window.find("settings-sync").visible());
        assert!(window.find("settings-storage").visible());
        for key in FieldKey::ALL {
            let row = window.find(key.id());
            assert!(row.visible(), "the {} row is visible", key.label());
            assert!(
                row.bounds().size.width > px(0.) && row.bounds().size.height > px(0.),
                "the {} row has a layout size",
                key.label()
            );
        }
        assert!(
            window.try_find("settings-problem").is_none(),
            "a healthy settings file shows no problem"
        );
        assert!(
            window.find("settings-path-config").visible(),
            "the settings file location is shown"
        );
        assert!(
            window.find("settings-path-state").visible(),
            "the state database location is shown"
        );
    })
    .expect("update the settings window");
}

#[gpui_kit::test]
fn keyboard_reaches_every_control_and_steps_the_concurrency(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths.clone(), FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        for id in [
            "settings-autostart",
            FieldKey::Bandwidth.id(),
            FieldKey::Concurrency.id(),
            FieldKey::ScanInterval.id(),
            FieldKey::AttemptCap.id(),
        ] {
            tab_to(window, id, cx);
            assert_eq!(
                window.find(id).focused(),
                Some(true),
                "{id} holds the keyboard focus"
            );
        }

        // The Tab key itself moves focus between controls.
        tab_to(window, "settings-autostart", cx);
        window.press("tab", cx);
        assert_eq!(
            window.find(FieldKey::Bandwidth.id()).focused(),
            Some(true),
            "Tab moves focus from the switch to the next field"
        );

        // Up and Down step the focused field and save it.
        tab_to(window, FieldKey::Concurrency.id(), cx);
        window.press("up", cx);
        window.press("up", cx);
        window.press("down", cx);
    })
    .expect("update the settings window");

    let reloaded = SettingsStore::open(paths.config_file);
    assert_eq!(
        reloaded.settings().concurrency,
        5,
        "arrow steps changed the setting and wrote it to disk"
    );
}

#[gpui_kit::test]
fn the_autostart_toggle_applies_to_the_os_and_persists(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let backend = FakeAutostart::new();
    let autostart: Rc<dyn Autostart> = backend.clone();
    let window = open_settings(cx, store, paths.clone(), autostart);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "settings-autostart", cx);
        window.press("space", cx);
        assert_eq!(
            window.find("settings-autostart").checked(),
            Some(true),
            "the switch shows the new value"
        );
    })
    .expect("update the settings window");

    assert!(backend.enabled.get(), "the OS mechanism was switched on");
    assert!(
        SettingsStore::open(paths.config_file.clone())
            .settings()
            .autostart,
        "the setting persisted"
    );

    cx.update_window(window, |_, window, cx| {
        tab_to(window, "settings-autostart", cx);
        window.press("enter", cx);
    })
    .expect("update the settings window");

    assert!(!backend.enabled.get(), "the OS mechanism was switched off");
    assert!(
        !SettingsStore::open(paths.config_file).settings().autostart,
        "turning it off persisted too"
    );
}

#[gpui_kit::test]
fn a_refused_autostart_change_is_reported_and_not_recorded(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths.clone(), FakeAutostart::refusing());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "settings-autostart", cx);
        window.press("space", cx);

        let notice = window.find("settings-action-error");
        assert!(notice.visible(), "the failure is visible");
        let message = notice.label().unwrap_or_default();
        assert!(
            message.contains("the OS refused the autostart change"),
            "the notice explains the failure: {message}"
        );
        assert_eq!(
            window.find("settings-autostart").checked(),
            Some(false),
            "the switch stays off when the OS refuses"
        );
    })
    .expect("update the settings window");

    assert!(
        !SettingsStore::open(paths.config_file).settings().autostart,
        "a refused change is not recorded as on"
    );
}

#[gpui_kit::test]
fn corrupt_settings_load_defaults_and_show_a_problem_that_clears_after_a_save(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    fs::create_dir_all(paths.config_file.parent().expect("a config directory"))
        .expect("create the config directory");
    fs::write(&paths.config_file, "concurrency = \"four\"\n").expect("write the corrupt file");
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths.clone(), FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let notice = window.find("settings-problem");
        assert!(notice.visible(), "the corrupt file is reported in the UI");
        let message = notice.label().unwrap_or_default();
        assert!(
            message.contains("settings.toml"),
            "the notice names the file: {message}"
        );
        assert_eq!(
            window.find("settings-autostart").checked(),
            Some(false),
            "the view stays usable on defaults"
        );

        // Acting on the defaults repairs the file. The input change is
        // delivered as an entity effect, so the notice is checked after this
        // window update returns and the effect has run.
        tab_to(window, FieldKey::Concurrency.id(), cx);
        window.press("up", cx);
    })
    .expect("update the settings window");

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("settings-problem").is_none(),
            "a successful save clears the load problem"
        );
    })
    .expect("update the settings window");

    let reloaded = SettingsStore::open(paths.config_file);
    assert_eq!(reloaded.settings().concurrency, 5);
    assert!(reloaded.problem().is_none());
}

#[gpui_kit::test]
fn long_storage_paths_stay_inside_their_card(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let mut paths = temporary_paths(temp.path());
    let deep = "a-very-long-directory-name".repeat(12);
    paths.config_file = temp.path().join(&deep).join("settings.toml");
    paths.state_db = temp.path().join(&deep).join("state.db");
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let storage = window.find("settings-storage").bounds();
        let limits = window.find("settings-limits").bounds();
        assert_eq!(
            storage.size.width, limits.size.width,
            "every section card keeps the same width"
        );

        for id in ["settings-path-config", "settings-path-state"] {
            let row = window.find(id).bounds();
            assert!(
                row.right() <= storage.right() && row.left() >= storage.left(),
                "{id} overflows its card: {row:?} vs {storage:?}"
            );
        }
    })
    .expect("update the settings window");
}

#[gpui_kit::test]
fn the_shell_switch_shows_the_dashboard_and_the_settings(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_shell(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.find("status-global").visible(),
            "the shell starts on the dashboard"
        );
        assert!(
            window.try_find("settings-general").is_none(),
            "settings are not rendered while the dashboard is shown"
        );

        tab_to(window, "nav-settings", cx);
        window.press("enter", cx);
        assert!(
            window.find("settings-general").visible(),
            "the settings surface is shown"
        );
        assert!(
            window.try_find("status-global").is_none(),
            "the dashboard is not rendered while settings are shown"
        );

        tab_to(window, "nav-dashboard", cx);
        window.press("space", cx);
        assert!(
            window.find("status-global").visible(),
            "the dashboard is shown again"
        );
    })
    .expect("update the shell window");
}
