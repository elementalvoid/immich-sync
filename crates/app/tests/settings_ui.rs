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
    AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Point, Role, TestAppContext, Window,
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
    unreadable: bool,
}

impl FakeAutostart {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            enabled: Cell::new(false),
            refuse: false,
            unreadable: false,
        })
    }

    /// A backend whose OS entry is already `enabled`.
    fn with_enabled(enabled: bool) -> Rc<Self> {
        Rc::new(Self {
            enabled: Cell::new(enabled),
            refuse: false,
            unreadable: false,
        })
    }

    fn refusing() -> Rc<Self> {
        Rc::new(Self {
            enabled: Cell::new(false),
            refuse: true,
            unreadable: false,
        })
    }

    /// A backend whose OS entry cannot be read.
    fn unreadable() -> Rc<Self> {
        Rc::new(Self {
            enabled: Cell::new(false),
            refuse: false,
            unreadable: true,
        })
    }
}

impl Autostart for FakeAutostart {
    fn is_enabled(&self) -> anyhow::Result<bool> {
        if self.unreadable {
            anyhow::bail!("the OS autostart state could not be read");
        }
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

/// Opens the settings over an explicit path-resolution result.
fn open_settings_over_resolution(
    cx: &mut TestAppContext,
    resolved: anyhow::Result<AppPaths>,
) -> AnyWindowHandle {
    cx.update(|cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(window_options(), cx, |window, cx| {
            cx.new(|cx| SettingsView::from_resolved_paths(resolved, window, cx))
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

/// The text input the kit nests inside the row `id`.
///
/// The number input the kit renders nests the editable text field, so the row
/// alone is not proof of what assistive technology reaches.
fn row_text_input(
    window: &Window,
    id: &'static str,
) -> gpui_kit::base::test_support::ElementSnapshot {
    gpui_kit::base::test_support::snapshots(window)
        .into_iter()
        .find(|snapshot| {
            snapshot.role() == Some(Role::TextInput)
                && snapshot.path().contains(&ElementId::Name(id.into()))
        })
        .expect("a text input inside the row")
}

#[test]
fn field_keys_map_settings_to_the_displayed_text() {
    let mut settings = Settings::default();

    assert_eq!(
        FieldKey::Bandwidth.display(&settings),
        "0",
        "0 MB/s is unlimited"
    );
    assert_eq!(FieldKey::Concurrency.display(&settings), "4");
    assert_eq!(FieldKey::ScanInterval.display(&settings), "30");
    assert_eq!(FieldKey::AttemptCap.display(&settings), "8");

    assert_eq!(FieldKey::Concurrency.label(), "Concurrent uploads");
    assert_eq!(FieldKey::Concurrency.unit(), "uploads");
    assert_eq!(FieldKey::Bandwidth.unit(), "MB/s");
    assert_eq!(FieldKey::ScanInterval.unit(), "minutes");

    FieldKey::Bandwidth.set(&mut settings, 3.0);
    assert_eq!(
        settings.bandwidth_bytes_per_second, 3_000_000,
        "the UI's MB/s is stored as bytes per second (R7.1)"
    );
    assert_eq!(FieldKey::Bandwidth.display(&settings), "3");

    FieldKey::Concurrency.set(&mut settings, 999.0);
    assert_eq!(settings.concurrency, 64, "out-of-range input clamps");
    FieldKey::Concurrency.set(&mut settings, 0.0);
    assert_eq!(settings.concurrency, 1);
}

#[test]
fn the_bandwidth_field_round_trips_a_fractional_value() {
    let mut settings = Settings {
        bandwidth_bytes_per_second: 1_500_000,
        ..Settings::default()
    };

    assert_eq!(
        FieldKey::Bandwidth.display(&settings),
        "1.5",
        "bytes per second display as MB/s without losing the fraction (R7.1)"
    );

    // What the field shows must store back as the same number, byte for byte.
    FieldKey::Bandwidth.set(&mut settings, 1.5);
    assert_eq!(settings.bandwidth_bytes_per_second, 1_500_000);

    settings.bandwidth_bytes_per_second = 1_234_567;
    assert_eq!(FieldKey::Bandwidth.display(&settings), "1.234567");
    FieldKey::Bandwidth.set(&mut settings, 1.234567);
    assert_eq!(settings.bandwidth_bytes_per_second, 1_234_567);

    // A hand-edited extreme value is shown, not silently replaced.
    settings.bandwidth_bytes_per_second = u64::MAX;
    assert_eq!(
        FieldKey::Bandwidth.display(&settings),
        "18446744073709.551615"
    );
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
fn every_numeric_field_has_an_accessible_name(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        for key in FieldKey::ALL {
            assert_eq!(
                window.find(key.id()).label(),
                Some(key.label()),
                "the {} row carries the field name",
                key.label()
            );
            assert_eq!(
                row_text_input(window, key.id()).label(),
                Some(key.label()),
                "the {} text field is named for assistive technology",
                key.label()
            );
        }
    })
    .expect("update the settings window");
}

#[gpui_kit::test]
fn opening_the_view_leaves_a_fractional_bandwidth_alone(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut store = SettingsStore::open(paths.config_file.clone());
    store
        .update(|settings| settings.bandwidth_bytes_per_second = 1_500_000)
        .expect("write a hand-edited value");
    let before = fs::read_to_string(&paths.config_file).expect("read the settings file");

    let window = open_settings(cx, store, paths.clone(), FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            row_text_input(window, FieldKey::Bandwidth.id()).value(),
            Some("1.5"),
            "the field shows the stored fraction, not a truncated whole number"
        );
    })
    .expect("update the settings window");

    assert_eq!(
        fs::read_to_string(&paths.config_file).expect("read the settings file again"),
        before,
        "opening the view does not rewrite a hand-edited value"
    );
}

#[gpui_kit::test]
fn stepping_the_bandwidth_keeps_the_fraction(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut store = SettingsStore::open(paths.config_file.clone());
    store
        .update(|settings| settings.bandwidth_bytes_per_second = 1_500_000)
        .expect("write a hand-edited value");
    let window = open_settings(cx, store, paths.clone(), FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, FieldKey::Bandwidth.id(), cx);
        window.press("up", cx);
        assert_eq!(
            row_text_input(window, FieldKey::Bandwidth.id()).value(),
            Some("2.5"),
            "a step keeps the decimal place"
        );
    })
    .expect("update the settings window");

    assert_eq!(
        SettingsStore::open(paths.config_file)
            .settings()
            .bandwidth_bytes_per_second,
        2_500_000,
        "the stepped value was saved in bytes per second"
    );
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
fn a_failed_path_resolution_is_shown_through_the_problem_notice(cx: &mut TestAppContext) {
    let window = open_settings_over_resolution(
        cx,
        Err(anyhow::anyhow!("the platform provided no home directory")),
    );

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let notice = window.find("settings-problem");
        assert!(notice.visible(), "the resolution failure is shown");
        let message = notice.label().unwrap_or_default();
        assert!(
            message.contains("the platform provided no home directory"),
            "the notice names the reason: {message}"
        );
        assert!(
            window.find("settings-path-config").visible(),
            "the fallback location stays visible"
        );
    })
    .expect("update the settings window");
}

#[gpui_kit::test]
fn the_view_reconciles_the_autostart_setting_with_the_os(cx: &mut TestAppContext) {
    // The file says the client starts at login, but the OS has no entry, as if
    // the user removed it outside the client. The file is corrected to off.
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut store = SettingsStore::open(paths.config_file.clone());
    store
        .update(|settings| settings.autostart = true)
        .expect("seed the settings file");
    let window = open_settings(cx, store, paths.clone(), FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("settings-autostart").checked(),
            Some(false),
            "the switch shows the OS state, not the stale file"
        );
    })
    .expect("update the settings window");
    assert!(
        !SettingsStore::open(paths.config_file).settings().autostart,
        "the file was corrected to match the OS"
    );

    // The reverse mismatch: a real OS entry with the file saying off. The
    // switch shows on and the file is corrected, so toggling it off is real.
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let backend = FakeAutostart::with_enabled(true);
    let autostart: Rc<dyn Autostart> = backend.clone();
    let window = open_settings(cx, store, paths.clone(), autostart);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("settings-autostart").checked(),
            Some(true),
            "the switch shows the OS entry"
        );
    })
    .expect("update the settings window");
    assert!(
        SettingsStore::open(paths.config_file).settings().autostart,
        "the file was corrected to match the OS"
    );
    assert!(backend.enabled.get(), "the OS entry was left alone");
}

#[gpui_kit::test]
fn an_unreadable_os_autostart_state_is_reported_as_a_read_failure(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths.clone(), FakeAutostart::unreadable());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let notice = window.find("settings-action-error");
        assert!(notice.visible(), "the read failure is visible");
        let message = notice.label().unwrap_or_default();
        assert!(
            message.starts_with("Could not read the autostart setting"),
            "the notice names the failed read, not an update: {message}"
        );
    })
    .expect("update the settings window");
}

#[gpui_kit::test]
fn a_failed_settings_write_rolls_the_autostart_change_back(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    // A directory where the atomic write puts its temporary file makes every
    // save fail after the OS mechanism already changed.
    fs::create_dir_all(paths.config_file.parent().expect("a config directory"))
        .expect("create the config directory");
    fs::create_dir(paths.config_file.with_extension("toml.tmp")).expect("block the temporary file");
    let backend = FakeAutostart::new();
    let autostart: Rc<dyn Autostart> = backend.clone();
    let window = open_settings(cx, store, paths.clone(), autostart);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "settings-autostart", cx);
        window.press("space", cx);

        assert!(
            !backend.enabled.get(),
            "the OS change is rolled back when the file cannot be saved"
        );
        assert_eq!(
            window.find("settings-autostart").checked(),
            Some(false),
            "the switch keeps the value both stores agree on"
        );
        let notice = window.find("settings-action-error");
        assert!(notice.visible(), "the failure is visible");
        let message = notice.label().unwrap_or_default();
        assert!(
            message.starts_with("Could not update the autostart setting"),
            "the notice names the failed action: {message}"
        );
    })
    .expect("update the settings window");

    assert!(
        !SettingsStore::open(paths.config_file).settings().autostart,
        "the failed save recorded nothing"
    );
}

#[gpui_kit::test]
fn a_failed_save_says_the_settings_could_not_be_saved(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    fs::create_dir_all(paths.config_file.parent().expect("a config directory"))
        .expect("create the config directory");
    fs::create_dir(paths.config_file.with_extension("toml.tmp")).expect("block the temporary file");
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, FieldKey::Concurrency.id(), cx);
        window.press("up", cx);
    })
    .expect("update the settings window");

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let notice = window.find("settings-action-error");
        let message = notice.label().unwrap_or_default();
        assert!(
            message.starts_with("Could not save the settings"),
            "a number change reports a save failure: {message}"
        );
    })
    .expect("update the settings window");
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
fn corrupt_settings_load_defaults_and_show_a_problem_that_clears_after_a_save(
    cx: &mut TestAppContext,
) {
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
fn corrupt_settings_are_left_alone_when_the_os_autostart_is_enabled(cx: &mut TestAppContext) {
    // The defaults a corrupt file falls back to are not a stored value, so
    // reconciling them against an enabled OS entry must not rewrite the file:
    // that would destroy the bytes and hide why they could not be loaded.
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let corrupt = "concurrency = \"four\"\n";
    fs::create_dir_all(paths.config_file.parent().expect("a config directory"))
        .expect("create the config directory");
    fs::write(&paths.config_file, corrupt).expect("write the corrupt file");
    let store = SettingsStore::open(paths.config_file.clone());
    let backend = FakeAutostart::with_enabled(true);
    let window = open_settings(cx, store, paths.clone(), backend);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let notice = window
            .try_find("settings-problem")
            .expect("the corrupt file is still reported");
        assert!(notice.visible(), "the problem notice stays visible");
        assert!(
            notice.label().unwrap_or_default().contains("settings.toml"),
            "the notice names the file"
        );
        assert_eq!(
            window.find("settings-autostart").checked(),
            Some(false),
            "the switch shows the defaults the store holds"
        );
    })
    .expect("update the settings window");

    assert_eq!(
        fs::read_to_string(&paths.config_file).expect("read the file back"),
        corrupt,
        "reconciling left the corrupt file's bytes untouched"
    );
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

        for (id, value_id) in [
            ("settings-path-config", "settings-path-config-value"),
            ("settings-path-state", "settings-path-state-value"),
        ] {
            let row = window.find(id).bounds();
            assert!(
                row.right() <= storage.right() && row.left() >= storage.left(),
                "{id} overflows its card: {row:?} vs {storage:?}"
            );

            // Only the text node itself proves the path was truncated: an
            // unconstrained text child overflows its row while the row fits.
            let value = window.find(value_id).bounds();
            assert!(
                value.right() <= storage.right() && value.left() >= storage.left(),
                "{value_id} overflows its card: {value:?} vs {storage:?}"
            );
            assert!(
                value.size.width < storage.size.width,
                "{value_id} shrank to fit instead of keeping its full width: {value:?} vs {storage:?}"
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
