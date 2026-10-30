//! Headless tests for the settings surface and the shell's view switch.
//!
//! The harness can drive keyboard focus and native accessibility properties; it
//! cannot read pixels, so the focus *ring* itself is not asserted here.

mod common;

use common::mock_server;

use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ClipboardItem, ElementId, Entity,
    InputEvent as _, Point, Role, ScrollDelta, ScrollEvent, TestAppContext, Window, WindowBounds,
    WindowOptions, point, px, size,
};
use immich_sync::autostart::Autostart;
use immich_sync::connection::{ConnectionModel, profile_from_settings};
use immich_sync::credentials::CredentialStoreProvider;
use immich_sync::dashboard::Dashboard;
use immich_sync::engine::EngineModel;
use immich_sync::paths::AppPaths;
use immich_sync::settings::{Settings, SettingsStore};
use immich_sync::settings_view::{FieldKey, SettingsView};
use immich_sync::shell::AppShell;
use immich_sync::status;
use immich_sync::store::StoreModel;
use immich_sync::theme;
use immich_sync::upload::UploadModel;
use immich_sync::upload_view::UploadView;
use mock_immich::{Fault, Operation};
use sync_core::connection::{
    Attention, AuthMethod, ConnectionState, Secret, ServerProfile, SignIn,
};
use sync_core::credential::{
    CredentialError, CredentialStore, MemoryCredentialStore, SharedCredentialStore,
};
use sync_core::engine::Limits;
use sync_core::hash::Sha1;
use sync_core::store::{ItemRecord, ItemState, StateStore};

/// A stand-in for the OS autostart mechanism, so the settings-to-OS wiring is
/// testable on every platform. On Linux the real XDG backend is covered by
/// `tests/settings.rs`; the Windows Run key stays a manual check.
struct FakeAutostart {
    enabled: Cell<bool>,
    refuse: bool,
    unreadable: bool,
    /// The hint shown while the OS waits for the user to approve the item.
    approval: Option<&'static str>,
    /// Whether the approval settings page was opened.
    approval_opened: Cell<bool>,
}

impl FakeAutostart {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            enabled: Cell::new(false),
            refuse: false,
            unreadable: false,
            approval: None,
            approval_opened: Cell::new(false),
        })
    }

    /// A backend whose OS entry is already `enabled`.
    fn with_enabled(enabled: bool) -> Rc<Self> {
        Rc::new(Self {
            enabled: Cell::new(enabled),
            refuse: false,
            unreadable: false,
            approval: None,
            approval_opened: Cell::new(false),
        })
    }

    fn refusing() -> Rc<Self> {
        Rc::new(Self {
            enabled: Cell::new(false),
            refuse: true,
            unreadable: false,
            approval: None,
            approval_opened: Cell::new(false),
        })
    }

    /// A backend whose OS entry cannot be read.
    fn unreadable() -> Rc<Self> {
        Rc::new(Self {
            enabled: Cell::new(false),
            refuse: false,
            unreadable: true,
            approval: None,
            approval_opened: Cell::new(false),
        })
    }

    /// A backend whose entry is registered but waits for the user, as macOS
    /// does after `SMAppService.register()`. The registration exists, so the
    /// entry reads as enabled.
    fn awaiting_approval() -> Rc<Self> {
        Rc::new(Self {
            enabled: Cell::new(true),
            refuse: false,
            unreadable: false,
            approval: Some("macOS needs your approval in System Settings › General › Login Items."),
            approval_opened: Cell::new(false),
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

    fn approval_needed(&self) -> Option<&'static str> {
        self.approval
    }

    fn open_approval_settings(&self) {
        self.approval_opened.set(true);
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
            // Tall enough for every settings section to be laid out at once,
            // so a visibility assertion measures the section, not the scroll.
            size: size(px(1200.), px(1400.)),
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
    open_settings_with_connection(cx, store, paths, autostart).0
}

/// Opens the settings and returns the connection model next to the window.
fn open_settings_with_connection(
    cx: &mut TestAppContext,
    store: SettingsStore,
    paths: AppPaths,
    autostart: Rc<dyn Autostart>,
) -> (AnyWindowHandle, Entity<ConnectionModel>) {
    let (window, _view, connection) = open_settings_view(
        cx,
        store,
        paths,
        autostart,
        Arc::new(MemoryCredentialStore::new()),
    );
    (window, connection)
}

/// Opens the settings and returns the view entity too, for a test that reads
/// what the fields show.
fn open_settings_view(
    cx: &mut TestAppContext,
    store: SettingsStore,
    paths: AppPaths,
    autostart: Rc<dyn Autostart>,
    credentials: SharedCredentialStore,
) -> (
    AnyWindowHandle,
    Entity<SettingsView>,
    Entity<ConnectionModel>,
) {
    let profile = profile_from_settings(store.settings());
    let connection = cx.update(|cx| cx.new(|_| ConnectionModel::new(profile, credentials)));
    let (window, view) =
        open_settings_over_connection(cx, store, paths, autostart, connection.clone());
    (window, view, connection)
}

/// Opens the settings view over `connection`, so a test can decide how the
/// model resolves its credential store.
fn open_settings_over_connection(
    cx: &mut TestAppContext,
    store: SettingsStore,
    paths: AppPaths,
    autostart: Rc<dyn Autostart>,
    connection: Entity<ConnectionModel>,
) -> (AnyWindowHandle, Entity<SettingsView>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(window_options(), cx, |window, cx| {
            cx.new(|cx| SettingsView::new(store, paths, autostart, connection.clone(), window, cx))
        })
        .expect("open the settings window")
    })
}

/// Replaces the text in the focused field with `value`.
fn retype(window: &mut Window, value: &str, cx: &mut App) {
    window.press(crate::common::select_all_chord(), cx);
    window.input(value, cx);
}

/// Pumps the executor until the settings file holds a value `check` accepts.
///
/// The settings write now runs on the background executor (N2), so a test
/// that reads the file right after a UI action has to wait for it.
async fn wait_for_settings(
    cx: &mut TestAppContext,
    paths: &AppPaths,
    check: impl Fn(&Settings) -> bool,
) -> Settings {
    for _ in 0..600 {
        let settings = SettingsStore::open(paths.config_file.clone())
            .settings()
            .clone();
        if check(&settings) {
            return settings;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    panic!("the settings file never reached the wanted state");
}

/// Pumps the executor until the settings window shows `id`, then returns its
/// label. A save outcome now lands on a later tick (N2).
async fn wait_for_label(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    id: &'static str,
) -> String {
    for _ in 0..600 {
        cx.run_until_parked();
        let label = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window
                    .try_find(id)
                    .and_then(|found| found.label().map(str::to_string))
            })
            .ok()
            .flatten();
        if let Some(label) = label {
            return label;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
    }
    panic!("the settings window never showed {id}");
}

/// Opens the settings over an explicit path-resolution result.
fn open_settings_over_resolution(
    cx: &mut TestAppContext,
    resolved: anyhow::Result<AppPaths>,
) -> AnyWindowHandle {
    cx.update(|cx| {
        gpui_kit::init(cx);
        let connection = cx.new(|_| {
            let store: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(ServerProfile::default(), store)
        });
        gpui_kit::open_window(window_options(), cx, |window, cx| {
            cx.new(|cx| SettingsView::from_resolved_paths(resolved, window, cx, connection.clone()))
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
) -> (AnyWindowHandle, Entity<ConnectionModel>) {
    let profile = profile_from_settings(store.settings());
    let upload_paths = paths.clone();
    cx.update(|cx| {
        gpui_kit::init(cx);
        let connection = cx.new(|_| {
            let store: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(profile, store)
        });
        let window = gpui_kit::open_window(window_options(), cx, |window, cx| {
            let dashboard = cx.new(|_| Dashboard::new(status::sample()));
            let settings = cx.new(|cx| {
                SettingsView::new(store, paths, autostart, connection.clone(), window, cx)
            });
            let store_model = cx.new(|cx| immich_sync::store::StoreModel::open(&upload_paths, cx));
            let upload_model =
                cx.new(|cx| UploadModel::new(connection.clone(), store_model.clone(), cx));
            let upload = cx.new(|cx| UploadView::new(upload_model, window, cx));
            let engine_model = cx.new(|cx| {
                immich_sync::engine::EngineModel::new(
                    connection.clone(),
                    store_model.clone(),
                    &upload_paths,
                    cx,
                )
            });
            let failed = cx.new(|cx| {
                immich_sync::failed_view::FailedView::new(engine_model.clone(), window, cx)
            });
            let diagnostics_model = cx.new(|cx| {
                immich_sync::diagnostics::DiagnosticsModel::new(
                    store_model.clone(),
                    engine_model.clone(),
                    connection.clone(),
                    upload_paths.log_directory(),
                    cx,
                )
            });
            let diagnostics = cx.new(|cx| {
                immich_sync::diagnostics_view::DiagnosticsView::new(diagnostics_model, cx)
            });
            let review =
                cx.new(|cx| immich_sync::review_view::ReviewView::new(engine_model, window, cx));
            cx.new(|cx| {
                AppShell::new(
                    dashboard,
                    settings,
                    upload,
                    failed,
                    diagnostics,
                    review,
                    connection.clone(),
                    cx,
                )
            })
        })
        .expect("open the shell window")
        .0;
        (window, connection)
    })
}

/// Moves focus to the next tab stop, as the Root's Tab binding does, and
/// redraws so the observed focus state is current.
fn tab(window: &mut Window, cx: &mut App) {
    window.focus_next(cx);
    window.render_frame(cx);
}

/// Tabs until `id` holds the keyboard focus.
///
/// The bound is generous because Tab wraps: walking back from a page's controls
/// to the page strip above them goes round the whole window.
fn tab_to(window: &mut Window, id: &'static str, cx: &mut App) {
    for _ in 0..32 {
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

/// The text input the kit nests inside the row `id`, which must carry `role`.
///
/// The number input the kit renders nests the editable text field, so the row
/// alone is not proof of what assistive technology reaches. The role is
/// asserted, not accepted: gpui-kit 0.8 reports a masked field as
/// `Role::PasswordInput`, so a regression that started masking another field
/// would fail here instead of passing silently.
fn row_text_input(
    window: &Window,
    id: &'static str,
    role: Role,
) -> gpui_kit::base::test_support::ElementSnapshot {
    let snapshot = gpui_kit::base::test_support::snapshots(window)
        .into_iter()
        .find(|snapshot| {
            matches!(snapshot.role(), Some(Role::TextInput | Role::PasswordInput))
                && snapshot.path().contains(&ElementId::Name(id.into()))
        })
        .expect("a text input inside the row");
    assert_eq!(
        snapshot.role(),
        Some(role),
        "{id} carries the expected role"
    );
    snapshot
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
    // The client runs a rate under its floor at the floor, so the field says
    // which one that is instead of silently overriding the value.
    assert!(
        FieldKey::Bandwidth.help().contains("0.001 MB/s"),
        "the bandwidth help does not name the minimum: {}",
        FieldKey::Bandwidth.help()
    );

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
        draw(window, cx);

        // #78: the surface is six pages now, so a stored value is shown on its
        // own page; `every_settings_row_is_named_and_explained_on_its_own_page`
        // pins which page each row belongs to.
        open_page(window, PAGE_GENERAL, cx);
        assert_eq!(
            window.find("settings-autostart").checked(),
            Some(false),
            "autostart defaults to off (R8.5)"
        );
        for (key, page) in [
            (FieldKey::Bandwidth, PAGE_LIMITS),
            (FieldKey::Concurrency, PAGE_LIMITS),
            (FieldKey::ScanInterval, PAGE_SYNC),
            (FieldKey::AttemptCap, PAGE_SYNC),
        ] {
            open_page(window, page, cx);
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
        open_page(window, PAGE_STORAGE, cx);
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
        draw(window, cx);
        for (key, page) in [
            (FieldKey::Bandwidth, PAGE_LIMITS),
            (FieldKey::Concurrency, PAGE_LIMITS),
            (FieldKey::ScanInterval, PAGE_SYNC),
            (FieldKey::AttemptCap, PAGE_SYNC),
        ] {
            open_page(window, page, cx);
            assert_eq!(
                window.find(key.id()).label(),
                Some(key.label()),
                "the {} row carries the field name",
                key.label()
            );
            assert_eq!(
                row_text_input(window, key.id(), Role::TextInput).label(),
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
        draw(window, cx);
        open_page(window, PAGE_LIMITS, cx);
        assert_eq!(
            row_text_input(window, FieldKey::Bandwidth.id(), Role::TextInput).value(),
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
async fn stepping_the_bandwidth_keeps_the_fraction(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut store = SettingsStore::open(paths.config_file.clone());
    store
        .update(|settings| settings.bandwidth_bytes_per_second = 1_500_000)
        .expect("write a hand-edited value");
    let window = open_settings(cx, store, paths.clone(), FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_LIMITS, cx);
        tab_to(window, FieldKey::Bandwidth.id(), cx);
        window.press("up", cx);
        assert_eq!(
            row_text_input(window, FieldKey::Bandwidth.id(), Role::TextInput).value(),
            Some("2.5"),
            "a step keeps the decimal place"
        );
    })
    .expect("update the settings window");

    let saved = wait_for_settings(cx, &paths, |settings| {
        settings.bandwidth_bytes_per_second == 2_500_000
    })
    .await;
    assert_eq!(
        saved.bandwidth_bytes_per_second, 2_500_000,
        "the stepped value was saved in bytes per second"
    );
}

#[gpui_kit::test]
async fn keyboard_walks_every_page_and_every_control(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    // A server URL gives the Tagging page its link, so the walk reaches a
    // Button there as well as the switch beside it.
    let mut seed = SettingsStore::open(paths.config_file.clone());
    seed.update(|settings| settings.server_url = "https://immich.example.com".to_string())
        .expect("seed the server URL");
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths.clone(), FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        // #78: no pointer click opens a page. The strip is the keyboard's path
        // between them: Tab reaches a page's own button, Enter opens it, and Tab
        // then reaches every control that page holds.
        for page in 0..PAGE_COUNT {
            tab_to(window, PAGE_BUTTONS[page], cx);
            assert_eq!(
                window.find(PAGE_BUTTONS[page]).focused(),
                Some(true),
                "the keyboard reaches page {page}'s tab"
            );
            window.press("enter", cx);
            draw(window, cx);
            for id in PAGE_CONTROLS[page] {
                tab_to(window, id, cx);
                assert_eq!(
                    window.find(*id).focused(),
                    Some(true),
                    "{id} holds the keyboard focus on page {page}"
                );
            }
        }

        // Tab walks a page's controls in the order the page lists them.
        tab_to(window, PAGE_BUTTONS[PAGE_LIMITS], cx);
        window.press("enter", cx);
        draw(window, cx);
        tab_to(window, FieldKey::Bandwidth.id(), cx);
        window.press("tab", cx);
        assert_eq!(
            window.find(FieldKey::Concurrency.id()).focused(),
            Some(true),
            "Tab moves focus from one field to the next on the page"
        );

        // Up and Down step the focused field and save it.
        window.press("up", cx);
        window.press("up", cx);
        window.press("down", cx);
    })
    .expect("update the settings window");

    // The three steps queue and coalesce; the last one is what reaches disk.
    let reloaded = wait_for_settings(cx, &paths, |settings| settings.concurrency == 5).await;
    assert_eq!(
        reloaded.concurrency, 5,
        "arrow steps changed the setting and wrote it to disk"
    );
}

#[gpui_kit::test]
async fn the_autostart_toggle_applies_to_the_os_and_persists(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let backend = FakeAutostart::new();
    let autostart: Rc<dyn Autostart> = backend.clone();
    let window = open_settings(cx, store, paths.clone(), autostart);

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_GENERAL, cx);
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
    let saved = wait_for_settings(cx, &paths, |settings| settings.autostart).await;
    assert!(saved.autostart, "the setting persisted");

    cx.update_window(window, |_, window, cx| {
        tab_to(window, "settings-autostart", cx);
        window.press("enter", cx);
    })
    .expect("update the settings window");

    assert!(!backend.enabled.get(), "the OS mechanism was switched off");
    let saved = wait_for_settings(cx, &paths, |settings| !settings.autostart).await;
    assert!(!saved.autostart, "turning it off persisted too");
}

#[gpui_kit::test]
fn a_failed_path_resolution_is_shown_through_the_problem_notice(cx: &mut TestAppContext) {
    let window = open_settings_over_resolution(
        cx,
        Err(anyhow::anyhow!("the platform provided no home directory")),
    );

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_STORAGE, cx);
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
async fn the_view_reconciles_the_autostart_setting_with_the_os(cx: &mut TestAppContext) {
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
        draw(window, cx);
        open_page(window, PAGE_GENERAL, cx);
        assert_eq!(
            window.find("settings-autostart").checked(),
            Some(false),
            "the switch shows the OS state, not the stale file"
        );
    })
    .expect("update the settings window");
    // N2: the correction is written on the background executor, so the file
    // is only in step after the write lands.
    let saved = wait_for_settings(cx, &paths, |settings| !settings.autostart).await;
    assert!(!saved.autostart, "the file was corrected to match the OS");

    // The reverse mismatch: a real OS entry with the file saying off. The
    // switch shows on and the file is corrected, so toggling it off is real.
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let backend = FakeAutostart::with_enabled(true);
    let autostart: Rc<dyn Autostart> = backend.clone();
    let window = open_settings(cx, store, paths.clone(), autostart);

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_GENERAL, cx);
        assert_eq!(
            window.find("settings-autostart").checked(),
            Some(true),
            "the switch shows the OS entry"
        );
    })
    .expect("update the settings window");
    let saved = wait_for_settings(cx, &paths, |settings| settings.autostart).await;
    assert!(saved.autostart, "the file was corrected to match the OS");
    assert!(backend.enabled.get(), "the OS entry was left alone");
}

#[gpui_kit::test]
fn an_unreadable_os_autostart_state_is_reported_as_a_read_failure(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths.clone(), FakeAutostart::unreadable());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_GENERAL, cx);
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
async fn a_failed_settings_write_rolls_the_autostart_change_back(cx: &mut TestAppContext) {
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
        draw(window, cx);
        open_page(window, PAGE_GENERAL, cx);
        tab_to(window, "settings-autostart", cx);
        window.press("space", cx);

        // N2: the save runs off the UI thread, so its outcome is not here yet.
        assert!(
            window.try_find("settings-action-error").is_none(),
            "the failing write did not run on the UI thread"
        );
        assert!(
            backend.enabled.get(),
            "the OS change is applied before the file write"
        );
    })
    .expect("update the settings window");

    let message = wait_for_label(cx, window, "settings-action-error").await;
    assert!(
        message.starts_with("Could not update the autostart setting"),
        "the notice names the failed action: {message}"
    );
    assert!(
        !backend.enabled.get(),
        "the OS change is rolled back when the file cannot be saved"
    );
    let checked = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.find("settings-autostart").checked()
        })
        .expect("read the switch");
    assert_eq!(
        checked,
        Some(false),
        "the switch keeps the value both stores agree on"
    );
    assert!(
        !SettingsStore::open(paths.config_file).settings().autostart,
        "the failed save recorded nothing"
    );
}

#[gpui_kit::test]
async fn a_failed_save_says_the_settings_could_not_be_saved(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    fs::create_dir_all(paths.config_file.parent().expect("a config directory"))
        .expect("create the config directory");
    fs::create_dir(paths.config_file.with_extension("toml.tmp")).expect("block the temporary file");
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_LIMITS, cx);
        tab_to(window, FieldKey::Concurrency.id(), cx);
        window.press("up", cx);
        // N2: the write runs off the UI thread, so its failure is not here
        // yet. Seeing it in this same tick would mean the UI waited on it.
        assert!(
            window.try_find("settings-action-error").is_none(),
            "the failing write did not run on the UI thread"
        );
    })
    .expect("update the settings window");

    let message = wait_for_label(cx, window, "settings-action-error").await;
    assert!(
        message.starts_with("Could not save the settings"),
        "a number change reports a save failure: {message}"
    );
}

/// N2: a no-op edit while a failing write is in flight must not report
/// success on its behalf; the queued write reports the truth.
#[gpui_kit::test]
async fn a_noop_edit_while_a_write_is_in_flight_still_reports_its_failure(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    fs::create_dir_all(paths.config_file.parent().expect("a config directory"))
        .expect("create the config directory");
    fs::create_dir(paths.config_file.with_extension("toml.tmp")).expect("block the temporary file");
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_LIMITS, cx);
        tab_to(window, FieldKey::Bandwidth.id(), cx);
        // The first edit starts a write for 2 MB/s that will fail.
        retype(window, "2", cx);
        assert!(
            window.try_find("settings-action-error").is_none(),
            "the failing write did not run on the UI thread"
        );
        // The second edit stores the same 2_000_000 bytes per second from a
        // different text, so it queues a redundant write instead of claiming
        // success while the first still runs.
        retype(window, "2.0000001", cx);
    })
    .expect("update the settings window");

    let message = wait_for_label(cx, window, "settings-action-error").await;
    assert!(
        message.starts_with("Could not save the settings"),
        "a no-op edit must not hide the failing write: {message}"
    );
}

#[gpui_kit::test]
fn an_unapproved_login_item_shows_where_to_approve_it(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let backend = FakeAutostart::awaiting_approval();
    let autostart: Rc<dyn Autostart> = backend.clone();
    let window = open_settings(cx, store, paths.clone(), autostart);

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_GENERAL, cx);
        assert!(
            window.find("settings-autostart-approval").visible(),
            "the approval row is shown"
        );
        assert_eq!(
            window.find("settings-autostart").checked(),
            Some(true),
            "an item waiting for approval is registered, so it reads as on"
        );
        assert_eq!(
            window.find("settings-autostart-approve").label(),
            Some("Open Login Items settings"),
            "the control to approve it is named"
        );
        window.click("settings-autostart-approve", cx);
    })
    .expect("update the settings window");

    assert!(
        backend.approval_opened.get(),
        "the control opens the approval settings page"
    );

    // A platform with no approval step shows no row and no control.
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths, FakeAutostart::new());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("settings-autostart-approval").is_none(),
            "no approval row without a pending approval"
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
        draw(window, cx);
        open_page(window, PAGE_GENERAL, cx);
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
async fn corrupt_settings_load_defaults_and_show_a_problem_that_clears_after_a_save(
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
        draw(window, cx);
        open_page(window, PAGE_LIMITS, cx);
        let notice = window.find("settings-problem");
        assert!(notice.visible(), "the corrupt file is reported in the UI");
        let message = notice.label().unwrap_or_default();
        assert!(
            message.contains("settings.toml"),
            "the notice names the file: {message}"
        );
        open_page(window, PAGE_GENERAL, cx);
        assert_eq!(
            window.find("settings-autostart").checked(),
            Some(false),
            "the view stays usable on defaults"
        );
        open_page(window, PAGE_LIMITS, cx);

        // Acting on the defaults repairs the file. The input change is
        // delivered as an entity effect, and the save runs on the background
        // executor (N2), so the file is waited for below.
        tab_to(window, FieldKey::Concurrency.id(), cx);
        window.press("up", cx);
    })
    .expect("update the settings window");

    wait_for_settings(cx, &paths, |settings| settings.concurrency == 5).await;
    let reloaded = SettingsStore::open(paths.config_file.clone());
    assert_eq!(reloaded.settings().concurrency, 5);
    assert!(
        reloaded.problem().is_none(),
        "the repaired file loads cleanly"
    );

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("settings-problem").is_none(),
            "a successful save clears the load problem"
        );
    })
    .expect("update the settings window");
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
        draw(window, cx);
        open_page(window, PAGE_GENERAL, cx);
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
    let shown = [
        (
            "settings-path-config",
            "settings-path-config-value",
            paths.config_file.display().to_string(),
        ),
        (
            "settings-path-state",
            "settings-path-state-value",
            paths.state_db.display().to_string(),
        ),
    ];
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_STORAGE, cx);
        // #78: the house cards are gone, so the row's bound is the surface it
        // sits in rather than a card of its own.
        let surface = window.find("settings-scroll").bounds();
        for (id, value_id, path) in shown {
            let row = window.find(id).bounds();
            assert!(
                row.right() <= surface.right() && row.left() >= surface.left(),
                "{id} overflows the settings surface: {row:?} vs {surface:?}"
            );

            // Only the text node itself proves the path was truncated: an
            // unconstrained text child overflows its row while the row fits.
            let value = window.find(value_id).bounds();
            assert!(
                value.right() <= surface.right() && value.left() >= surface.left(),
                "{value_id} overflows the settings surface: {value:?} vs {surface:?}"
            );
            assert!(
                value.size.width < surface.size.width,
                "{value_id} shrank to fit instead of keeping its full width: {value:?} vs {surface:?}"
            );
            // #78: a read-only path a user has to hear needs a name of its own,
            // and the row's title is not the path (S2).
            assert_eq!(
                window.find(value_id).label(),
                Some(path.as_str()),
                "{value_id} is named by the path it shows"
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
    let (window, _connection) = open_shell(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.find("status-global").visible(),
            "the shell starts on the dashboard"
        );
        assert!(
            window.try_find("settings-scroll").is_none(),
            "settings are not rendered while the dashboard is shown"
        );

        tab_to(window, "nav-settings", cx);
        window.press("enter", cx);
        draw(window, cx);
        assert!(
            window.find("settings-scroll").visible(),
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

        tab_to(window, "nav-upload", cx);
        window.press("enter", cx);
        assert!(
            window.find("upload-path").visible(),
            "the upload surface is shown"
        );
        assert!(
            window.try_find("status-global").is_none(),
            "the dashboard is not rendered while the upload is shown"
        );
    })
    .expect("update the shell window");
}

#[gpui_kit::test]
fn the_connection_form_names_its_controls(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);

        assert!(
            window.find("settings-server-url").visible(),
            "the connection page is on the settings surface"
        );
        for (id, label) in [
            ("settings-server-url", "Server URL"),
            ("settings-email", "Email address"),
            ("settings-secret", "Password"),
            ("settings-auth-method", "Credential"),
            ("settings-sign-in", "Sign in"),
        ] {
            let row = window.find(id);
            assert!(row.visible(), "{id} is visible");
            assert_eq!(row.label(), Some(label), "{id} is named");
        }
        // The nested text fields carry the same names to assistive technology.
        for (id, label, role) in [
            ("settings-server-url", "Server URL", Role::TextInput),
            ("settings-email", "Email address", Role::TextInput),
            ("settings-secret", "Password", Role::PasswordInput),
        ] {
            assert_eq!(
                row_text_input(window, id, role).label(),
                Some(label),
                "{id} names its text field"
            );
        }
        // The secret field is masked, so its value is never announced.
        assert!(
            row_text_input(window, "settings-secret", Role::PasswordInput)
                .value()
                .is_none(),
            "the password field is masked"
        );
        assert_eq!(
            window.find("settings-auth-password").label(),
            Some("Sign in with a password"),
            "the password method is named"
        );
        assert_eq!(
            window.find("settings-auth-apikey").label(),
            Some("Sign in with an API key"),
            "the API-key method is named"
        );

        // Switching the credential relabels the secret field (R1.5).
        window.click("settings-auth-apikey", cx);
        assert_eq!(
            row_text_input(window, "settings-secret", Role::PasswordInput).label(),
            Some("API key"),
            "the secret field follows the credential choice"
        );
    })
    .expect("update the settings window");
}

#[gpui_kit::test]
fn the_connection_state_shows_on_the_form_and_the_global_card(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, connection) = open_shell(cx, store, paths, FakeAutostart::new());
    let attention = ConnectionState::NeedsAttention(Attention::PasswordLoginDisabled);

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        assert_eq!(
            window.find("status-connection-global").label(),
            Some(ConnectionState::NotConfigured.message().as_str()),
            "the global card carries the live connection state, not the sample"
        );
    })
    .expect("update the shell window");

    // A state change travels through the shell's observation of the model.
    cx.update(|cx| {
        connection.update(cx, |model, cx| model.set_state(attention.clone(), cx));
    });
    cx.run_until_parked();

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("status-connection-global").label(),
            Some(attention.message().as_str()),
            "the global card follows a state change"
        );

        tab_to(window, "nav-settings", cx);
        window.press("enter", cx);
        assert_eq!(
            window.find("settings-connection-state").label(),
            Some(attention.message().as_str()),
            "the form shows the same state"
        );
        assert!(
            window.find("settings-connection-hint").visible(),
            "a disabled password login points at the API key (R1.6)"
        );
    })
    .expect("update the shell window");
}

#[gpui_kit::test]
fn reopening_the_window_does_not_restart_the_startup_login(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());

    // A saved profile, as a fresh launch finds in the settings file (R1.3).
    let mut store = SettingsStore::open(paths.config_file.clone());
    store
        .update(|settings| {
            settings.server_url = "https://immich.example.com".to_string();
            settings.email = "sync@example.com".to_string();
        })
        .expect("seed a saved profile");
    let connection = cx.update(|cx| {
        cx.new(|_| {
            let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(profile_from_settings(store.settings()), credentials)
        })
    });

    // The first open starts the startup login and puts the model in connecting.
    let _first = open_settings_over_connection(
        cx,
        store.clone(),
        paths.clone(),
        FakeAutostart::new(),
        connection.clone(),
    );
    assert_eq!(
        cx.read(|cx| connection.read(cx).state().clone()),
        ConnectionState::Connecting,
        "the first open starts the saved login"
    );

    // The app-scoped model transitions while the window is hidden, e.g. after
    // the server refused the restore.
    let attention = ConnectionState::NeedsAttention(Attention::Rejected);
    cx.update(|cx| {
        connection.update(cx, |model, cx| model.set_state(attention.clone(), cx));
    });

    // The reopen must not restart the startup login, which would reset the
    // state the tray is showing.
    let window =
        open_settings_over_connection(cx, store, paths, FakeAutostart::new(), connection.clone()).0;
    assert_eq!(
        cx.read(|cx| connection.read(cx).state().clone()),
        attention,
        "a reopened window keeps the state the app-level model holds"
    );
    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        assert_eq!(
            window.find("settings-connection-state").label(),
            Some(attention.message().as_str()),
            "the reopened form shows the surviving state"
        );
    })
    .expect("render the reopened window");
}

#[gpui_kit::test]
async fn a_first_run_does_not_restart_the_login_on_reopen(cx: &mut TestAppContext) {
    let (runtime, server) = mock_server();
    // The sign-in result arrives from the HTTP worker thread.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let server_url = server.base_url().to_string();

    // A genuine first launch: the settings file holds no saved profile (R1.3).
    let store = SettingsStore::open(paths.config_file.clone());
    let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
    let connection = cx.update(|cx| {
        cx.new(|_| ConnectionModel::new(profile_from_settings(store.settings()), credentials))
    });
    let (window, _first) = open_settings_over_connection(
        cx,
        store.clone(),
        paths.clone(),
        FakeAutostart::new(),
        connection.clone(),
    );
    assert_eq!(
        cx.read(|cx| connection.read(cx).state().clone()),
        ConnectionState::NotConfigured,
        "a first launch has no saved login to restore"
    );

    // The user signs in through the form, and the view persists the connected
    // profile, so the settings file now holds the login a later launch reads.
    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        tab_to(window, "settings-server-url", cx);
        window.input(&server_url, cx);
        tab_to(window, "settings-email", cx);
        window.input("sync@example.com", cx);
        tab_to(window, "settings-secret", cx);
        window.input("correct horse", cx);
        window.click("settings-sign-in", cx);
    })
    .expect("fill in the form and sign in");
    cx.wait_for(window, Duration::from_secs(30), |window, _| {
        window.find("settings-connection-state").label() == Some("Connected")
    })
    .await;
    cx.run_until_parked();
    let text = fs::read_to_string(&paths.config_file).expect("the connected profile was saved");
    assert!(text.contains(&server_url), "{text}");

    // Hide and reopen the window: the profile is now saved, but the app-scoped
    // model already owns the live session. The first open consumed the restore
    // guard, so the reopen must not start the saved login again. A later launch
    // reads the persisted profile from the file, not the stale first-run store.
    let store = SettingsStore::open(paths.config_file.clone());
    let reopened =
        open_settings_over_connection(cx, store, paths, FakeAutostart::new(), connection.clone()).0;
    assert_eq!(
        cx.read(|cx| connection.read(cx).state().clone()),
        ConnectionState::Connected,
        "reopening after a first-run sign-in keeps the live session"
    );
    cx.update_window(reopened, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("settings-connection-state").label(),
            Some("Connected"),
            "the reopened form shows the surviving session"
        );
    })
    .expect("render the reopened window");
    drop(runtime);
}

/// A credential store whose drop is observable. The connection model holds one
/// while an attempt is in flight; a result a newer state superseded drops the
/// connection instead of storing it, so the drop proves the branch ran.
struct DropSignalingStore {
    secret: Mutex<Option<Secret>>,
    dropped: Arc<AtomicBool>,
}

impl CredentialStore for DropSignalingStore {
    fn get(&self) -> Result<Option<Secret>, CredentialError> {
        Ok(self
            .secret
            .lock()
            .expect("the store is not poisoned")
            .clone())
    }

    fn set(&self, secret: &Secret) -> Result<(), CredentialError> {
        *self.secret.lock().expect("the store is not poisoned") = Some(secret.clone());
        Ok(())
    }

    fn delete(&self) -> Result<(), CredentialError> {
        *self.secret.lock().expect("the store is not poisoned") = None;
        Ok(())
    }
}

impl Drop for DropSignalingStore {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[gpui_kit::test]
async fn a_superseded_sign_in_result_is_dropped(cx: &mut TestAppContext) {
    let (runtime, server) = mock_server();
    // The sign-in result arrives from the HTTP worker thread.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());

    // Every resolve makes a fresh store whose drop this test observes.
    let dropped = Arc::new(AtomicBool::new(false));
    let provider = CredentialStoreProvider::new({
        let dropped = dropped.clone();
        move || {
            Arc::new(DropSignalingStore {
                secret: Mutex::new(None),
                dropped: dropped.clone(),
            })
        }
    });
    let connection =
        cx.update(|cx| cx.new(|_| ConnectionModel::new(ServerProfile::default(), provider)));
    let window = open_settings_over_connection(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths,
        FakeAutostart::new(),
        connection.clone(),
    )
    .0;

    // A sign-in starts, and a newer state supersedes it while it is in flight.
    let profile = ServerProfile {
        server_url: server.base_url().to_string(),
        email: "sync@example.com".to_string(),
        method: AuthMethod::Password,
    };
    cx.update(|cx| {
        connection.update(cx, |model, cx| {
            model.sign_in(
                SignIn {
                    email: profile.email.clone(),
                    secret: Secret::new("correct horse"),
                    method: AuthMethod::Password,
                },
                profile.clone(),
                cx,
            );
        });
    });
    let attention = ConnectionState::NeedsAttention(Attention::Rejected);
    cx.update(|cx| {
        connection.update(cx, |model, cx| model.set_state(attention.clone(), cx));
    });

    // The result arrives after the newer state. The connection is dropped
    // rather than stored; without the generation check the predicate would see
    // the overwritten state and the assertion would fail.
    cx.wait_for(window, Duration::from_secs(30), |_, cx| {
        dropped.load(Ordering::SeqCst) || connection.read(cx).state().clone() != attention
    })
    .await;
    assert!(
        dropped.load(Ordering::SeqCst),
        "the superseded result must be dropped instead of overwriting the newer state"
    );
    assert_eq!(
        cx.read(|cx| connection.read(cx).state().clone()),
        attention,
        "the newer state survives the superseded result"
    );
    drop(runtime);
}

#[gpui_kit::test]
fn a_wrong_password_shows_an_error_and_the_form_stays_usable(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, connection) =
        open_settings_with_connection(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        connection.update(cx, |model, cx| {
            model.set_state(
                ConnectionState::NeedsAttention(Attention::WrongPassword),
                cx,
            );
        });
        draw(window, cx);

        let notice = window.find("settings-connection-state");
        let label = notice.label();
        assert!(
            label.is_some_and(|text| text.contains("rejected the password")),
            "the wrong password is reported: {label:?}"
        );
        // The form itself stays enabled, so the user can try again.
        assert_ne!(window.find("settings-sign-in").disabled(), Some(true));
        assert_ne!(
            row_text_input(window, "settings-secret", Role::PasswordInput).disabled(),
            Some(true),
            "the secret field stays enabled"
        );
        assert!(
            window.try_find("settings-connection-hint").is_none(),
            "a wrong password does not point at the API key"
        );

        connection.update(cx, |model, cx| {
            model.set_state(ConnectionState::Connected, cx);
        });
        window.render_frame(cx);
        assert_eq!(
            window.find("settings-connection-state").label(),
            Some("Connected"),
            "a successful sign-in clears the error"
        );
    })
    .expect("update the settings window");
}

#[gpui_kit::test]
fn the_connection_form_reports_empty_fields_without_signing_in(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        window.click("settings-sign-in", cx);

        let error = window.find("settings-connection-error");
        let label = error.label();
        assert!(
            label.is_some_and(|text| text.contains("server URL")),
            "the empty form is reported: {label:?}"
        );
    })
    .expect("update the settings window");
}

#[gpui_kit::test]
fn switching_the_credential_updates_the_secret_placeholder(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, view, _connection) = open_settings_view(
        cx,
        store,
        paths,
        FakeAutostart::new(),
        Arc::new(MemoryCredentialStore::new()),
    );

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        assert_eq!(
            view.read(cx).secret_placeholder(cx),
            "Password",
            "the secret field follows the selected credential"
        );

        window.click("settings-auth-apikey", cx);
        assert_eq!(
            view.read(cx).secret_placeholder(cx),
            "API key",
            "the secret field follows the credential choice"
        );

        window.click("settings-auth-password", cx);
        assert_eq!(
            view.read(cx).secret_placeholder(cx),
            "Password",
            "switching back restores the password placeholder"
        );
    })
    .expect("update the settings window");
}

#[gpui_kit::test]
async fn signing_in_through_the_form_never_writes_the_secret(cx: &mut TestAppContext) {
    let (runtime, server) = mock_server();
    // The sign-in result arrives from the HTTP worker thread, so the test
    // scheduler must allow that wake (its hook for I/O tests).
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let server_url = server.base_url().to_string();
    let password = "correct horse";
    let (window, _view, _connection) = open_settings_view(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths.clone(),
        FakeAutostart::new(),
        Arc::new(MemoryCredentialStore::new()),
    );

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        tab_to(window, "settings-server-url", cx);
        window.input(&server_url, cx);
        tab_to(window, "settings-email", cx);
        window.input("sync@example.com", cx);
        tab_to(window, "settings-secret", cx);
        window.input(password, cx);
        window.click("settings-sign-in", cx);
    })
    .expect("fill in the connection form and click Sign in");

    cx.wait_for(window, Duration::from_secs(30), |window, _| {
        window.find("settings-connection-state").label() == Some("Connected")
    })
    .await;
    cx.run_until_parked();

    let text = fs::read_to_string(&paths.config_file).expect("the connected profile was saved");
    assert!(text.contains(&server_url), "{text}");
    assert!(text.contains("sync@example.com"), "{text}");
    assert!(
        !text.contains(password),
        "the settings TOML holds the password: {text}"
    );
    drop(runtime);
}

#[gpui_kit::test]
async fn signing_in_again_resolves_the_credential_store_again(cx: &mut TestAppContext) {
    let (runtime, server) = mock_server();
    // The sign-in result arrives from the HTTP worker thread.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let server_url = server.base_url().to_string();

    // The store is missing on the first sign-in and appears before the second,
    // as when the user starts GNOME Keyring and signs in again.
    let resolves = Arc::new(AtomicUsize::new(0));
    let started_later: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
    let provider = CredentialStoreProvider::new({
        let resolves = resolves.clone();
        let started_later = started_later.clone();
        move || {
            if resolves.fetch_add(1, Ordering::SeqCst) == 0 {
                Arc::new(MemoryCredentialStore::unavailable(
                    "no Secret Service provider yet",
                ))
            } else {
                started_later.clone()
            }
        }
    });
    let connection =
        cx.update(|cx| cx.new(|_| ConnectionModel::new(ServerProfile::default(), provider)));
    let (window, _view) = open_settings_over_connection(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths,
        FakeAutostart::new(),
        connection,
    );

    // The first sign-in resolves a missing provider and reports it.
    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        tab_to(window, "settings-server-url", cx);
        window.input(&server_url, cx);
        tab_to(window, "settings-email", cx);
        window.input("sync@example.com", cx);
        tab_to(window, "settings-secret", cx);
        window.input("correct horse", cx);
        window.click("settings-sign-in", cx);
    })
    .expect("fill in the connection form and click Sign in");
    cx.wait_for(window, Duration::from_secs(30), |window, _| {
        window
            .find("settings-connection-state")
            .label()
            .is_some_and(|label| label.contains("Secret Service"))
    })
    .await;
    assert_eq!(
        resolves.load(Ordering::SeqCst),
        1,
        "the first sign-in resolved the store"
    );

    // The user starts the provider and signs in again, without a restart.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("settings-sign-in", cx);
    })
    .expect("sign in again");
    cx.wait_for(window, Duration::from_secs(30), |window, _| {
        window.find("settings-connection-state").label() == Some("Connected")
    })
    .await;
    assert_eq!(
        resolves.load(Ordering::SeqCst),
        2,
        "the second sign-in resolved the store again"
    );
    drop(runtime);
}

#[gpui_kit::test]
async fn a_failed_sign_in_does_not_replace_the_saved_profile(cx: &mut TestAppContext) {
    let (runtime, server) = mock_server();
    // The restore and the failed sign-in both finish on the HTTP worker thread.
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let working_url = server.base_url().to_string();

    // A working setup: the profile and the credential the server accepts.
    let mut seed = SettingsStore::open(paths.config_file.clone());
    seed.update(|settings| {
        settings.server_url = working_url.clone();
        settings.email = "sync@example.com".to_string();
        settings.auth_method = AuthMethod::Password;
    })
    .expect("seed a saved profile");
    let credentials = Arc::new(MemoryCredentialStore::new());
    credentials
        .set(&Secret::new("correct horse"))
        .expect("store the working credential");

    let (window, _view, _connection) = open_settings_view(
        cx,
        SettingsStore::open(paths.config_file.clone()),
        paths.clone(),
        FakeAutostart::new(),
        credentials,
    );

    // The startup restore of the saved profile succeeds first.
    cx.wait_for(window, Duration::from_secs(30), |window, _| {
        window.find("settings-connection-state").label() == Some("Connected")
    })
    .await;

    // Change the account and give the wrong credential, so the server rejects
    // the sign-in.
    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        tab_to(window, "settings-email", cx);
        retype(window, "someone-else@example.com", cx);
        tab_to(window, "settings-secret", cx);
        retype(window, "wrong password", cx);
        window.click("settings-sign-in", cx);
    })
    .expect("change the profile and sign in");

    cx.wait_for(window, Duration::from_secs(30), |window, _| {
        window
            .find("settings-connection-state")
            .label()
            .is_some_and(|label| label.contains("Needs attention"))
    })
    .await;
    cx.run_until_parked();

    let text = fs::read_to_string(&paths.config_file).expect("the settings file still exists");
    assert!(
        text.contains(&working_url),
        "the working server URL is still saved: {text}"
    );
    assert!(
        text.contains("sync@example.com"),
        "the working email is still saved: {text}"
    );
    assert!(
        !text.contains("someone-else@example.com"),
        "a failed sign-in must not replace the saved profile: {text}"
    );
    drop(runtime);
}

/// The Limits card is the writer of both values, and the running engine
/// follows it without a restart (SPEC R7.1, R7.2, R7.3).
#[gpui_kit::test]
async fn a_limits_change_reaches_the_running_engine(cx: &mut TestAppContext) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut stored = SettingsStore::open(paths.config_file.clone());
    stored
        .update(|settings| {
            settings.bandwidth_bytes_per_second = 2_000_000;
            settings.concurrency = 6;
        })
        .expect("write the stored limits");

    let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
    let (window, view, connection) =
        open_settings_view(cx, stored, paths.clone(), FakeAutostart::new(), credentials);

    // Sign in on the model the view already holds, so the engine can start.
    let profile = ServerProfile {
        server_url: server.base_url().to_string(),
        email: "sync@example.com".to_string(),
        method: AuthMethod::Password,
    };
    cx.update(|cx| {
        connection.update(cx, |model, cx| {
            model.sign_in(
                SignIn {
                    email: profile.email.clone(),
                    secret: Secret::new("correct horse"),
                    method: AuthMethod::Password,
                },
                profile,
                cx,
            );
        });
    });

    let store_model = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    let engine_model = cx.update(|cx| {
        let engine = cx.new(|cx| EngineModel::new(connection, store_model, &paths, cx));
        engine.update(cx, |model, cx| model.start(cx));
        engine
    });
    for _ in 0..600 {
        if cx.read(|cx| engine_model.read(cx).is_running()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| engine_model.read(cx).is_running()),
        "the engine never started"
    );
    assert_eq!(
        cx.read(|cx| engine_model.read(cx).limits()),
        Some(Limits {
            bandwidth_bytes_per_second: 2_000_000,
            concurrency: 6,
        }),
        "R7.1/R7.2/R7.3: the engine starts with the stored limits"
    );

    // The shell hands the view the engine it must push changes to; this is
    // that same call.
    cx.update(|cx| view.update(cx, |view, cx| view.set_engine(engine_model.clone(), cx)));

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_LIMITS, cx);
        tab_to(window, FieldKey::Bandwidth.id(), cx);
        retype(window, "1", cx);
        tab_to(window, FieldKey::Concurrency.id(), cx);
        retype(window, "3", cx);
    })
    .expect("edit the limits");

    for _ in 0..600 {
        if cx.read(|cx| engine_model.read(cx).limits())
            == Some(Limits {
                bandwidth_bytes_per_second: 1_000_000,
                concurrency: 3,
            })
        {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert_eq!(
        cx.read(|cx| engine_model.read(cx).limits()),
        Some(Limits {
            bandwidth_bytes_per_second: 1_000_000,
            concurrency: 3,
        }),
        "R7.1/R7.2: a change in the window reaches the running engine"
    );

    let reloaded = SettingsStore::open(paths.config_file.clone());
    assert_eq!(
        reloaded.settings().bandwidth_bytes_per_second,
        1_000_000,
        "R7.3: the bandwidth limit was written back"
    );
    assert_eq!(
        reloaded.settings().concurrency,
        3,
        "R7.3: the concurrency limit was written back"
    );
}

/// R3.7: the Tagging section shows the fixed tag name and opens Immich's tag
/// page on the configured server, minus the API prefix.
#[gpui_kit::test]
fn the_tagging_section_shows_the_fixed_tag_and_opens_its_immich_page(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    // A reverse-proxy prefix in front of the API prefix, which is the shape
    // the client accepts as a base URL.
    let mut seed = SettingsStore::open(paths.config_file.clone());
    seed.update(|settings| {
        settings.server_url = "https://immich.example.com/immich/api".to_string();
    })
    .expect("seed the server URL");
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths.clone(), FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_TAGGING, cx);
        assert_eq!(
            window.find("settings-tag-name").label(),
            Some("immich-sync"),
            "R3.7: the fixed tag name is shown"
        );
        assert!(
            window.find("settings-tag-prerequisite").visible(),
            "R3.7: the section explains Immich's Tags preference"
        );
        assert_eq!(
            window.find("settings-tag-upload").checked(),
            Some(true),
            "R3.7: a fresh install shows the switch on"
        );
        // #78: the link is one of the rows the kit has no field type for, so it
        // is a custom item and must keep its name and its keyboard reach.
        assert_eq!(
            window.find("settings-tag-link").label(),
            Some("Open the sync tag's page in Immich"),
            "#78: the tag-page link keeps its accessible name"
        );
        tab_to(window, "settings-tag-link", cx);
        window.click("settings-tag-link", cx);
    })
    .expect("open the tag page from the settings window");

    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://immich.example.com/immich/tags?path=immich-sync"),
        "R3.7: the URL is the server URL minus /api, with the tag's path"
    );
}

/// R3.7: no server URL means no View in Immich link.
#[gpui_kit::test]
fn the_tagging_section_has_no_immich_link_without_a_server_url(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths.clone(), FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_TAGGING, cx);
        assert!(
            window.find("settings-tag-name").visible(),
            "the tagging page is there without a server"
        );
        assert!(
            window.try_find("settings-tag-link").is_none(),
            "R3.7: there is no link to open without a server URL"
        );
    })
    .expect("render the settings window");
    assert_eq!(cx.opened_url(), None, "nothing was opened");
}

/// R3.7: a fresh store starts with the tag on, and the switch persists the
/// setting and moves the running engine to it without a restart, in both
/// directions.
#[gpui_kit::test]
async fn the_tag_switch_persists_and_reaches_the_running_engine(cx: &mut TestAppContext) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut stored = SettingsStore::open(paths.config_file.clone());
    stored
        .update(|settings| {
            settings.server_url = server.base_url().to_string();
        })
        .expect("seed the server URL");

    let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
    let (window, view, connection) =
        open_settings_view(cx, stored, paths.clone(), FakeAutostart::new(), credentials);

    // Sign in on the model the view already holds, so the engine can start.
    let profile = ServerProfile {
        server_url: server.base_url().to_string(),
        email: "sync@example.com".to_string(),
        method: AuthMethod::Password,
    };
    cx.update(|cx| {
        connection.update(cx, |model, cx| {
            model.sign_in(
                SignIn {
                    email: profile.email.clone(),
                    secret: Secret::new("correct horse"),
                    method: AuthMethod::Password,
                },
                profile,
                cx,
            );
        });
    });

    let store_model = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    let engine_model = cx.update(|cx| {
        let engine = cx.new(|cx| EngineModel::new(connection, store_model, &paths, cx));
        engine.update(cx, |model, cx| model.start(cx));
        engine
    });
    for _ in 0..600 {
        if cx.read(|cx| engine_model.read(cx).is_running()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| engine_model.read(cx).is_running()),
        "the engine never started"
    );
    assert!(
        cx.read(|cx| engine_model.read(cx).tag_status().enabled),
        "R3.7: a fresh install starts with the tag on"
    );
    // The engine resolved the tag on the worker runtime as part of the start.
    for _ in 0..600 {
        if server.tag_id(sync_core::tags::SYNC_TAG).is_some() {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        server.tag_id(sync_core::tags::SYNC_TAG).is_some(),
        "R3.7: a fresh install created the tag"
    );

    // The shell hands the view the engine it must push changes to; this is
    // that same call.
    cx.update(|cx| view.update(cx, |view, cx| view.set_engine(engine_model.clone(), cx)));

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_TAGGING, cx);
        tab_to(window, "settings-tag-upload", cx);
        window.press("space", cx);
    })
    .expect("turn the tag off");

    let saved = wait_for_settings(cx, &paths, |settings| !settings.tag_uploaded_assets).await;
    assert!(!saved.tag_uploaded_assets, "R3.7: turning it off persisted");
    for _ in 0..600 {
        if !cx.read(|cx| engine_model.read(cx).tag_status().enabled) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        !cx.read(|cx| engine_model.read(cx).tag_status().enabled),
        "R3.7: the engine stopped tagging"
    );

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        tab_to(window, "settings-tag-upload", cx);
        window.press("space", cx);
    })
    .expect("turn the tag on");

    let saved = wait_for_settings(cx, &paths, |settings| settings.tag_uploaded_assets).await;
    assert!(saved.tag_uploaded_assets, "R3.7: turning it on persisted");
    for _ in 0..600 {
        if cx.read(|cx| engine_model.read(cx).tag_status().enabled) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| engine_model.read(cx).tag_status().enabled),
        "R3.7: the switch reached the running engine"
    );
}

/// R3.7: the Tagging section shows where the backfill over the assets uploaded
/// before the switch stands, reaches its completed state through the engine
/// model, and "Tag existing assets now" runs the whole library again.
#[gpui_kit::test]
async fn the_tagging_section_shows_the_backfill_and_tags_the_library_again(
    cx: &mut TestAppContext,
) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    // One asset an earlier run uploaded: the library the backfill covers.
    {
        let store = StateStore::open(paths.state_db.clone()).expect("seed the state database");
        store
            .record(&ItemRecord {
                path: temp.path().join("seeded.jpg").display().to_string(),
                size: 11,
                mtime_ns: 1,
                sha1: Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")
                    .expect("a hex SHA-1"),
                asset_id: Some("asset-seeded".to_string()),
                state: ItemState::Uploaded,
                error_class: None,
                error_message: None,
                attempts: 0,
                sidecar: None,
            })
            .expect("seed an uploaded asset");
    }
    let mut stored = SettingsStore::open(paths.config_file.clone());
    stored
        .update(|settings| {
            settings.server_url = server.base_url().to_string();
            // An install that turned the tag off before the default changed:
            // the flow below turns it on and expects the backfill to run.
            settings.tag_uploaded_assets = false;
        })
        .expect("seed the server URL");

    let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
    let (window, view, connection) =
        open_settings_view(cx, stored, paths.clone(), FakeAutostart::new(), credentials);
    let profile = ServerProfile {
        server_url: server.base_url().to_string(),
        email: "sync@example.com".to_string(),
        method: AuthMethod::Password,
    };
    cx.update(|cx| {
        connection.update(cx, |model, cx| {
            model.sign_in(
                SignIn {
                    email: profile.email.clone(),
                    secret: Secret::new("correct horse"),
                    method: AuthMethod::Password,
                },
                profile,
                cx,
            );
        });
    });

    let store_model = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    let engine_model = cx.update(|cx| {
        let engine = cx.new(|cx| EngineModel::new(connection, store_model, &paths, cx));
        engine.update(cx, |model, cx| model.start(cx));
        engine
    });
    for _ in 0..600 {
        if cx.read(|cx| engine_model.read(cx).is_running()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| engine_model.read(cx).is_running()),
        "the engine never started"
    );
    cx.update(|cx| view.update(cx, |view, cx| view.set_engine(engine_model.clone(), cx)));

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_TAGGING, cx);
        assert!(
            window.try_find("settings-tag-backfill").is_none(),
            "R3.7: a switch that is off shows no backfill progress"
        );
        tab_to(window, "settings-tag-upload", cx);
        window.press("space", cx);
    })
    .expect("turn the tag on");

    // The write lands first, then the engine starts the backfill; the finished
    // state arrives on the status poll.
    for _ in 0..600 {
        if cx.read(|cx| {
            let status = engine_model.read(cx).tag_status();
            status.enabled && status.backfill.completed
        }) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| engine_model.read(cx).tag_status().backfill.completed),
        "R3.7: the backfill never completed: {:?}",
        cx.read(|cx| engine_model.read(cx).tag_status().clone())
    );

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("settings-tag-backfill").label(),
            Some("Existing assets tagged: 1 of 1"),
            "R3.7: the section shows the completed state and its count"
        );
        assert!(
            window.find("settings-tag-now-row").visible(),
            "R3.7: the action is offered while the switch is on"
        );
        window.click("settings-tag-now", cx);
    })
    .expect("run the backfill again");

    // The action cleared the marker on the worker runtime, so the engine tags
    // the library again: one more batch on the wire.
    for _ in 0..600 {
        if server.tag_asset_calls().len() >= 2
            && cx.read(|cx| engine_model.read(cx).tag_status().backfill.completed)
        {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert_eq!(
        server.tag_asset_calls().len(),
        2,
        "R3.7: \"Tag existing assets now\" runs the whole library again: {:?}",
        server.tag_asset_calls()
    );
}

/// R3.7: an API key that lacks a permission gets its name in the Tagging
/// section, through the engine's own probe, and the section repaints itself
/// when the probe answers: nothing else asks it to render in between.
#[gpui_kit::test]
async fn a_missing_tag_permission_is_named_in_the_tagging_section(cx: &mut TestAppContext) {
    let (_runtime, server) = mock_server();
    cx.executor().allow_parking();
    server.set_api_key("test-key");
    // The upload may run, so the only missing permission is the tag's.
    server.set_api_key_permissions(&["asset.upload", "tag.create", "tag.asset"]);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let mut stored = SettingsStore::open(paths.config_file.clone());
    stored
        .update(|settings| {
            settings.server_url = server.base_url().to_string();
            // Start off, so the toggle below is the one that probes the key.
            settings.tag_uploaded_assets = false;
        })
        .expect("seed the server URL");

    let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
    let (window, view, connection) =
        open_settings_view(cx, stored, paths.clone(), FakeAutostart::new(), credentials);

    let profile = ServerProfile {
        server_url: server.base_url().to_string(),
        email: "sync@example.com".to_string(),
        method: AuthMethod::ApiKey,
    };
    cx.update(|cx| {
        connection.update(cx, |model, cx| {
            model.sign_in(
                SignIn {
                    email: profile.email.clone(),
                    secret: Secret::new("test-key"),
                    method: AuthMethod::ApiKey,
                },
                profile,
                cx,
            );
        });
    });

    let store_model = cx.update(|cx| cx.new(|cx| StoreModel::open(&paths, cx)));
    let engine_model = cx.update(|cx| {
        let engine = cx.new(|cx| EngineModel::new(connection, store_model, &paths, cx));
        engine.update(cx, |model, cx| model.start(cx));
        engine
    });
    for _ in 0..600 {
        if cx.read(|cx| engine_model.read(cx).is_running()) {
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(
        cx.read(|cx| engine_model.read(cx).is_running()),
        "the engine never started"
    );
    cx.update(|cx| view.update(cx, |view, cx| view.set_engine(engine_model.clone(), cx)));

    // Hold the probe open, so the frame the toggle itself produces cannot
    // already hold its answer.
    server.fail(Operation::ApiKeyMe, Fault::Slow(Duration::from_secs(1)));
    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        open_page(window, PAGE_TAGGING, cx);
        tab_to(window, "settings-tag-upload", cx);
        window.press("space", cx);
    })
    .expect("turn the tag on");

    // Let the setting land, then draw the frame its write produces: the switch
    // is on and nothing is known against it yet (N2).
    let saved = wait_for_settings(cx, &paths, |settings| settings.tag_uploaded_assets).await;
    assert!(saved.tag_uploaded_assets, "R3.7: the setting persisted");
    cx.update_window(window, |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(
            window.try_find("settings-tag-problem").is_none(),
            "the probe has not answered yet"
        );
    })
    .expect("draw the frame the switch produced");

    // The save outcome lands a tick after the file does (N2): its `record`
    // notifies the view, and its finish hands the switch to the engine, whose
    // next poll (250 ms) publishes the new tag state and notifies the view
    // again. Wait for that state instead of pumping one tick, because a loaded
    // runner can take longer than the fixed pump, and the late notification
    // would then be counted against the probe's answer below; it flaked on CI
    // as `left: 2` while issue #49's CI was being fixed. The probe is held for
    // a second, so the switch is always published before the answer.
    let mut applied = false;
    for _ in 0..600 {
        if cx.read(|cx| engine_model.read(cx).tag_status().enabled) {
            applied = true;
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(applied, "the engine never applied the tag switch");
    cx.update_window(window, |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(
            window.try_find("settings-tag-problem").is_none(),
            "the save outcome arrived before the probe answered"
        );
    })
    .expect("draw the frame the applied switch produced");

    // The harness re-renders the root view on every `draw`, so the notice
    // appearing in a frame does not prove the view asked for one. Watch the
    // view's own notifications instead: from here only its observation of the
    // engine may notify it.
    let repaints = Rc::new(Cell::new(0));
    let _repaint_watch = {
        let repaints = Rc::clone(&repaints);
        cx.update(|cx| cx.observe(&view, move |_, _| repaints.set(repaints.get() + 1)))
    };

    // The probe answers on the engine's worker runtime, and the model's poll
    // publishes it (R3.7).
    let mut settled = false;
    for _ in 0..600 {
        if cx.read(|cx| engine_model.read(cx).tag_status().problem.is_some()) {
            settled = true;
            break;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
        cx.run_until_parked();
    }
    assert!(settled, "the permission probe never settled");
    assert_eq!(
        repaints.get(),
        1,
        "R3.7: the section did not notify when the engine's answer arrived"
    );

    // The observation notified for that answer; this frame shows the state it
    // exposed.
    let label = cx
        .update_window(window, |_, window, cx| {
            window.draw(cx).clear(cx);
            window
                .try_find("settings-tag-problem")
                .and_then(|found| found.label().map(str::to_string))
        })
        .ok()
        .flatten()
        .expect("R3.7: the section never repainted the engine's tag problem");
    assert!(
        label.contains("asset.update"),
        "R3.7: the section names the missing permission: {label}"
    );
    assert!(
        server.upsert_calls().is_empty(),
        "R3.7: a key that may not create the tag sends no upsert"
    );
}

// ---------------------------------------------------------------------------
// #78: the settings surface is six pages of rows, with a search over their
// words. These tests pin what each row promises *before* the layout moves:
// its page, its element id, its accessible name, and its help sentence.
// ---------------------------------------------------------------------------

/// The six settings pages, in the order the sidebar lists them.
const PAGE_CONNECTION: usize = 0;
const PAGE_GENERAL: usize = 1;
const PAGE_LIMITS: usize = 2;
const PAGE_SYNC: usize = 3;
const PAGE_TAGGING: usize = 4;
const PAGE_STORAGE: usize = 5;
/// The number of settings pages.
const PAGE_COUNT: usize = 6;

/// The page strip's tabs, in the order the surface lists them.
const PAGE_BUTTONS: [&str; PAGE_COUNT] = [
    "settings-page-connection",
    "settings-page-general",
    "settings-page-limits",
    "settings-page-sync",
    "settings-page-tagging",
    "settings-page-storage",
];

/// The name each page tab carries, which is the page's own name.
const PAGE_TITLES: [&str; PAGE_COUNT] = [
    "Connection",
    "General",
    "Limits",
    "Sync",
    "Tagging",
    "Storage",
];

/// Every control that takes the keyboard, page by page. A read-only value, such
/// as a resolved path, is not a control and holds no focus, so Storage has none.
const PAGE_CONTROLS: [&[&str]; PAGE_COUNT] = [
    &[
        "settings-server-url",
        "settings-email",
        "settings-auth-password",
        "settings-auth-apikey",
        "settings-secret",
        "settings-sign-in",
    ],
    &["settings-autostart"],
    &["settings-bandwidth", "settings-concurrency"],
    &["settings-scan-interval", "settings-attempt-cap"],
    &["settings-tag-upload", "settings-tag-link"],
    &[],
];

/// One settings row, as the surface promises it.
struct RowPin {
    /// The page the row lives on.
    page: usize,
    /// The control's element id.
    id: &'static str,
    /// The control's accessible name.
    name: &'static str,
    /// The element carrying the help sentence as its description.
    help_id: &'static str,
    /// The help sentence under the row's title.
    help: &'static str,
}

/// A row whose control carries both its name and its help sentence.
const fn row(page: usize, id: &'static str, name: &'static str, help: &'static str) -> RowPin {
    RowPin {
        page,
        id,
        name,
        help_id: id,
        help,
    }
}

/// A row whose control is named but whose help hangs on the row around it.
const fn control_row(
    page: usize,
    id: &'static str,
    name: &'static str,
    help_id: &'static str,
    help: &'static str,
) -> RowPin {
    RowPin {
        page,
        id,
        name,
        help_id,
        help,
    }
}

/// Every row the surface always shows, in page order.
const ROWS: &[RowPin] = &[
    row(
        PAGE_CONNECTION,
        "settings-server-url",
        "Server URL",
        "For example https://immich.example.com. Certificate verification stays on.",
    ),
    row(
        PAGE_CONNECTION,
        "settings-email",
        "Email address",
        "The Immich account this client uploads as.",
    ),
    row(
        PAGE_CONNECTION,
        "settings-auth-method",
        "Credential",
        "A password for the account, or an API key from Immich.",
    ),
    row(
        PAGE_CONNECTION,
        "settings-secret",
        "Password",
        "Stored in the OS credential store, never in the settings file.",
    ),
    control_row(
        PAGE_GENERAL,
        "settings-autostart",
        "Start Immich Sync at login",
        "settings-autostart-row",
        "Start Immich Sync when you log in. Off by default.",
    ),
    row(
        PAGE_LIMITS,
        "settings-bandwidth",
        "Bandwidth limit",
        "Zero means unlimited; under 0.001 MB/s the client uses 0.001 MB/s.",
    ),
    row(
        PAGE_LIMITS,
        "settings-concurrency",
        "Concurrent uploads",
        "Uploads that run at the same time.",
    ),
    row(
        PAGE_SYNC,
        "settings-scan-interval",
        "Reconciliation scan interval",
        "The watcher uploads changes as they happen; this scan finds what the watcher missed.",
    ),
    row(
        PAGE_SYNC,
        "settings-attempt-cap",
        "Retry attempt cap",
        "Attempts per file before it needs attention.",
    ),
    control_row(
        PAGE_TAGGING,
        "settings-tag-upload",
        "Tag uploaded assets with immich-sync",
        "settings-tag-upload-row",
        "Adds the tag to every asset the client uploads. On by default.",
    ),
    control_row(
        PAGE_TAGGING,
        "settings-tag-name",
        "immich-sync",
        "settings-tag-name-row",
        "Fixed. The client always uses this name.",
    ),
    row(
        PAGE_STORAGE,
        "settings-path-config",
        "Settings file",
        "Resolved from the platform.",
    ),
    row(
        PAGE_STORAGE,
        "settings-path-state",
        "State database",
        "Resolved from the platform.",
    ),
];

/// The rows whose field is a text input as wide as the page, so a field that
/// shrank to a sliver is a layout bug rather than the kit's own control size.
const WIDE_TEXT_FIELDS: [&str; 3] = ["settings-server-url", "settings-email", "settings-secret"];

/// Draws twice: a page body is a virtual list, which lays its rows out on the
/// frame after the one that measured it.
fn draw(window: &mut Window, cx: &mut App) {
    window.render_frame(cx);
    window.render_frame(cx);
}

/// Opens one settings page by clicking its page-strip button.
fn open_page(window: &mut Window, page: usize, cx: &mut App) {
    window.click(PAGE_BUTTONS[page], cx);
    draw(window, cx);
}

/// Types `query` into the settings search field; an empty query clears it.
fn search(window: &mut Window, query: &str, cx: &mut App) {
    tab_to(window, "settings-search", cx);
    if query.is_empty() {
        window.press(crate::common::select_all_chord(), cx);
        window.press("backspace", cx);
    } else {
        retype(window, query, cx);
    }
    draw(window, cx);
}

/// Opens the shell at a chosen window size, so a layout test measures the
/// surface in the geometry the app draws it in.
fn open_shell_at(
    cx: &mut TestAppContext,
    paths: AppPaths,
    window_size: (f32, f32),
) -> AnyWindowHandle {
    let store = SettingsStore::open(paths.config_file.clone());
    let profile = profile_from_settings(store.settings());
    let upload_paths = paths.clone();
    cx.update(|cx| {
        gpui_kit::init(cx);
        let connection = cx.new(|_| {
            let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            ConnectionModel::new(profile, credentials)
        });
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(window_size.0), px(window_size.1)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| {
                let dashboard = cx.new(|_| Dashboard::new(status::sample()));
                let settings = cx.new(|cx| {
                    SettingsView::new(
                        store,
                        paths,
                        FakeAutostart::new(),
                        connection.clone(),
                        window,
                        cx,
                    )
                });
                let store_model =
                    cx.new(|cx| immich_sync::store::StoreModel::open(&upload_paths, cx));
                let upload_model =
                    cx.new(|cx| UploadModel::new(connection.clone(), store_model.clone(), cx));
                let upload = cx.new(|cx| UploadView::new(upload_model, window, cx));
                let engine_model = cx.new(|cx| {
                    EngineModel::new(connection.clone(), store_model.clone(), &upload_paths, cx)
                });
                let failed = cx.new(|cx| {
                    immich_sync::failed_view::FailedView::new(engine_model.clone(), window, cx)
                });
                let diagnostics_model = cx.new(|cx| {
                    immich_sync::diagnostics::DiagnosticsModel::new(
                        store_model.clone(),
                        engine_model.clone(),
                        connection.clone(),
                        upload_paths.log_directory(),
                        cx,
                    )
                });
                let diagnostics = cx.new(|cx| {
                    immich_sync::diagnostics_view::DiagnosticsView::new(diagnostics_model, cx)
                });
                let review = cx
                    .new(|cx| immich_sync::review_view::ReviewView::new(engine_model, window, cx));
                cx.new(|cx| {
                    AppShell::new(
                        dashboard,
                        settings,
                        upload,
                        failed,
                        diagnostics,
                        review,
                        connection.clone(),
                        cx,
                    )
                })
            },
        )
        .expect("open the shell window")
        .0
    })
}

/// #78: the surface is six pages, and each row is named and explained on its
/// own page, not on another.
#[gpui_kit::test]
fn every_settings_row_is_named_and_explained_on_its_own_page(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        // Each page's own tab names it, and the kit's sidebar is not drawn:
        // one page switch, not two (N3, S5).
        for (page, id) in PAGE_BUTTONS.iter().enumerate() {
            let button = window
                .try_find(*id)
                .unwrap_or_else(|| panic!("page {page} has no tab"));
            assert_eq!(
                button.label(),
                Some(PAGE_TITLES[page]),
                "{id} carries the page's name"
            );
        }
        assert!(
            window.try_find("0-0").is_none() && window.try_find("0-5").is_none(),
            "#78: the kit's own sidebar entries are gone with the kit sidebar"
        );

        for page in 0..PAGE_COUNT {
            open_page(window, page, cx);
            for pin in ROWS {
                if pin.page == page {
                    let found = window
                        .try_find(pin.id)
                        .unwrap_or_else(|| panic!("page {page} is missing {}", pin.id));
                    assert!(found.visible(), "{} is not on screen", pin.id);
                    assert_eq!(found.label(), Some(pin.name), "{} keeps its name", pin.id);
                    assert!(
                        found.bounds().size.width > px(0.) && found.bounds().size.height > px(0.),
                        "{} has a layout size: {:?}",
                        pin.id,
                        found.bounds()
                    );
                    assert_eq!(
                        window.find(pin.help_id).description(),
                        Some(pin.help),
                        "{} keeps its help sentence",
                        pin.help_id
                    );
                } else {
                    assert!(
                        window.try_find(pin.id).is_none(),
                        "{} belongs to page {}, so page {page} does not show it",
                        pin.id,
                        pin.page
                    );
                }
            }
        }
    })
    .expect("walk the settings pages");
}

/// #78: the strip is a tab list, and every page reports whether it is the page
/// on screen, so the selection reaches assistive technology and not only the
/// eye. It is the word #75's surface switch above it uses, because these are
/// the pages of one surface, not a set of toggles.
#[gpui_kit::test]
fn the_page_strip_reports_every_page_as_a_tab_and_the_selected_one(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        for shown in 0..PAGE_COUNT {
            open_page(window, shown, cx);
            for (page, id) in PAGE_BUTTONS.iter().enumerate() {
                let tab = window.find(*id);
                assert_eq!(tab.role(), Some(Role::Tab), "{id} is announced as a tab");
                assert_eq!(
                    tab.label(),
                    Some(PAGE_TITLES[page]),
                    "{id} is announced by its page's name"
                );
                assert_eq!(
                    tab.selected(),
                    Some(page == shown),
                    "{id} reports whether page {shown} is the page on screen"
                );
            }
        }
    })
    .expect("read the page strip");
}

/// #78: the kit's settings group always draws its resize handle, even with a
/// zero-width sidebar, and 0.8.1 has no switch to leave it out. The handle is
/// therefore covered rather than left live under the pointer: the band paints
/// nothing and, because the cover occludes, it never offers a resize cursor for
/// a drag that resizes nothing.
#[gpui_kit::test]
fn the_dead_resize_handle_is_covered_instead_of_left_live(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        let surface = window.find("settings-scroll").bounds();
        let inside_page = surface.left() + px(16.);
        let handle = window.find(("resizable-handle", 0usize));
        let cover = window.find("settings-handle-cover");
        assert!(
            handle.bounds().left() < inside_page && handle.bounds().right() > inside_page,
            "the kit's handle straddles the page's left edge: {:?}",
            handle.bounds()
        );
        assert!(
            cover.bounds().left() <= inside_page
                && cover.bounds().right() >= handle.bounds().right()
                && cover.bounds().size.height >= handle.bounds().size.height,
            "the cover spans the handle's inside part: {:?} vs {:?}",
            cover.bounds(),
            handle.bounds()
        );
        assert!(
            cover.bounds().right() < window.find("settings-server-url").bounds().left(),
            "the cover stays in the page's padding, clear of the first control"
        );
    })
    .expect("read the settings body");
}

/// #78: the search field is a named, keyboard-reachable control.
#[gpui_kit::test]
fn the_settings_search_field_is_named_and_reachable_from_the_keyboard(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        assert!(
            window.find("settings-search").visible(),
            "#78: the surface shows a search field"
        );
        tab_to(window, "settings-search", cx);
        assert_eq!(
            window.find("settings-search").focused(),
            Some(true),
            "#78: the search field holds the keyboard focus"
        );
        assert_eq!(
            row_text_input(window, "settings-search", Role::TextInput).label(),
            Some("Search settings"),
            "#78: the search field is named"
        );
    })
    .expect("focus the settings search field");
}

/// #78: a query matches a row's title, lands on the page that holds it, and
/// leaves the other rows out.
#[gpui_kit::test]
fn search_filters_by_title_and_lands_on_the_page_that_holds_the_match(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        // The surface opens on the first page, so only a search can show
        // `settings-bandwidth`: it proves both the match and the landing.
        assert!(window.try_find("settings-bandwidth").is_none());

        search(window, "bandwidth", cx);
        assert!(
            window.find("settings-bandwidth").visible(),
            "#78: the title's match is shown"
        );
        assert!(
            window.try_find("settings-concurrency").is_none(),
            "#78: a filtered page shows only the matching row"
        );
        assert!(
            window.try_find("settings-server-url").is_none(),
            "#78: a page without a match is not shown"
        );
    })
    .expect("search the settings surface");
}

/// #78: a query matches the help sentence too, and an empty result says so.
#[gpui_kit::test]
fn search_matches_the_help_sentence_and_an_empty_result_says_so(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let window = open_settings(cx, store, paths, FakeAutostart::new());

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        // "watcher" is only in the scan interval's help sentence.
        search(window, "watcher", cx);
        assert!(
            window.find("settings-scan-interval").visible(),
            "#78: the help sentence's match is shown on its own page"
        );
        assert!(
            window.try_find("settings-attempt-cap").is_none(),
            "#78: the page's other row is filtered out"
        );

        search(window, "zzz-no-such-setting", cx);
        let empty = window.find("settings-search-empty");
        assert!(empty.visible(), "#78: an empty result is on screen");
        assert_eq!(
            empty.label(),
            Some("No setting matches \"zzz-no-such-setting\"."),
            "#78: the empty result says what it is"
        );
        assert!(
            window.try_find("settings-scan-interval").is_none(),
            "#78: nothing from the previous query is left behind"
        );
        assert!(
            window.find("settings-search").visible(),
            "#78: the field stays, so the search can be cleared"
        );

        // Clearing the query brings the pages back.
        search(window, "", cx);
        assert!(window.find("settings-server-url").visible());
        assert!(
            window.try_find("settings-search-empty").is_none(),
            "#78: the empty result goes with the query"
        );
    })
    .expect("search the settings surface");
}

/// #78: at the narrowest supported window, in both themes, every row of every
/// page is reachable and keeps its control inside its group.
///
/// 520×480 is the shell's minimum window (`shell::main_window_options`). The
/// extra 780×480 pass measures the tight band just above the component's
/// stacking breakpoint, where a long label word and a fixed-width field are
/// closest to not fitting.
#[gpui_kit::test]
fn every_page_lays_out_its_rows_at_the_narrowest_window_in_both_themes(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    for (window_size, system) in [
        ((520., 480.), theme::SystemTheme::Light),
        ((520., 480.), theme::SystemTheme::Dark),
        ((780., 480.), theme::SystemTheme::Light),
    ] {
        cx.update(|cx| theme::apply_system_theme(system, cx));
        let window = open_shell_at(cx, paths.clone(), window_size);
        cx.update_window(window, |_, window, cx| {
            draw(window, cx);
            tab_to(window, "nav-settings", cx);
            window.press("enter", cx);
            draw(window, cx);
            // The page body's group is inset by the surface's own `p_4` and the
            // list's `px_4`, so this is the edge every row and control stays
            // inside.
            let surface = window.find("settings-scroll").bounds();
            let group_right = surface.right() - px(32.);
            for id in PAGE_BUTTONS {
                let tab = window.find(id);
                assert!(
                    tab.bounds().right() <= surface.right() - px(16.) + px(1.),
                    "{window_size:?} {system:?}: {id} overflows the surface: {:?} vs {surface:?}",
                    tab.bounds()
                );
            }
            for page in 0..PAGE_COUNT {
                open_page(window, page, cx);
                for pin in ROWS.iter().filter(|pin| pin.page == page) {
                    let row = scroll_page_to(window, pin.id, cx);
                    assert!(
                        row.visible(),
                        "{window_size:?} {system:?}: {} is unreachable",
                        pin.id
                    );
                    let surface = window.find("settings-scroll").bounds();
                    assert!(
                        row.bounds().left() >= surface.left() - px(1.)
                            && row.bounds().right() <= group_right + px(1.),
                        "{window_size:?} {system:?}: {} overflows its group: {:?} vs {surface:?}",
                        pin.id,
                        row.bounds()
                    );
                    let control = window.find(pin.help_id).bounds();
                    assert!(
                        control.size.width > px(0.) && control.size.height > px(0.),
                        "{window_size:?} {system:?}: {} has no layout size: {control:?}",
                        pin.help_id
                    );
                    assert!(
                        control.right() <= group_right + px(1.),
                        "{window_size:?} {system:?}: {} past its group edge: {control:?} vs {group_right:?}",
                        pin.help_id
                    );
                    // A row wrapper that fits can still overflow its own text
                    // field, which is the clip a screenshot shows first.
                    for input in gpui_kit::base::test_support::snapshots(window)
                        .into_iter()
                        .filter(|element| {
                            matches!(element.role(), Some(Role::TextInput | Role::PasswordInput))
                                && element
                                    .path()
                                    .contains(&ElementId::Name(pin.help_id.into()))
                        })
                    {
                        assert!(
                            input.bounds().right() <= surface.right() + px(1.),
                            "{window_size:?} {system:?}: the text field inside {} overflows the surface: {:?} vs {surface:?}",
                            pin.help_id,
                            input.bounds()
                        );
                        if WIDE_TEXT_FIELDS.contains(&pin.help_id) {
                            assert!(
                                input.bounds().size.width >= px(200.),
                                "{window_size:?} {system:?}: the text field inside {} is squeezed to {}",
                                pin.help_id,
                                input.bounds().size.width
                            );
                        }
                    }
                }
            }
        })
        .expect("lay out the settings pages");
    }
}

/// Scrolls the open page until `id` is laid out and on screen.
fn scroll_page_to(
    window: &mut Window,
    id: &'static str,
    cx: &mut App,
) -> gpui_kit::base::test_support::ElementSnapshot {
    for _ in 0..40 {
        if let Some(row) = window.try_find(id)
            && row.visible()
        {
            return row;
        }
        // The wheel goes to the page body, the surface's only scrollable part:
        // above it sits the page header and below it the reachability line.
        let surface = window.find("settings-scroll").bounds();
        let position = point(surface.center().x, surface.bottom() - px(44.));
        window.dispatch_event(
            ScrollEvent {
                position,
                delta: ScrollDelta::Lines(point(0., -4.)),
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        draw(window, cx);
    }
    panic!("{id} never became reachable inside the settings surface");
}

/// #78: a keystroke in the search field asks the surface to render, so the
/// rows are filtered from the field's value on the same frame.
#[gpui_kit::test]
fn typing_in_the_search_field_asks_the_surface_to_render(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, view) = open_settings_over_connection(
        cx,
        store,
        paths,
        FakeAutostart::new(),
        cx.update(|cx| {
            let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials))
        }),
    );

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        tab_to(window, "settings-search", cx);
    })
    .expect("focus the settings search field");

    let repaints = Rc::new(Cell::new(0));
    let _watch = {
        let repaints = Rc::clone(&repaints);
        cx.update(|cx| cx.observe(&view, move |_, _| repaints.set(repaints.get() + 1)))
    };

    cx.update_window(window, |_, window, cx| {
        window.input("b", cx);
    })
    .expect("type one character");

    assert!(
        repaints.get() > 0,
        "#78: a keystroke in the search field did not ask the surface to render"
    );
}

/// #78: a paste into the search field refilters the rows, exactly as a keystroke
/// does.
///
/// The harness re-renders the whole tree every frame, so asserting the rows
/// after a paste proves nothing: they would be filtered by the next frame even
/// with the bug. What is asserted instead is the view's own notification count
/// across the paste, which stays zero until the surface subscribes to the
/// field's `Change` event.
#[gpui_kit::test]
fn pasting_into_the_search_field_asks_the_surface_to_render(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let paths = temporary_paths(temp.path());
    let store = SettingsStore::open(paths.config_file.clone());
    let (window, view) = open_settings_over_connection(
        cx,
        store,
        paths,
        FakeAutostart::new(),
        cx.update(|cx| {
            let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
            cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials))
        }),
    );

    cx.update_window(window, |_, window, cx| {
        draw(window, cx);
        tab_to(window, "settings-search", cx);
    })
    .expect("focus the settings search field");

    let repaints = Rc::new(Cell::new(0));
    let _watch = {
        let repaints = Rc::clone(&repaints);
        cx.update(|cx| cx.observe(&view, move |_, _| repaints.set(repaints.get() + 1)))
    };

    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("bandwidth".into())));
    cx.update_window(window, |_, window, cx| {
        window.dispatch_action(Box::new(gpui_kit::base::input::Paste), cx);
        draw(window, cx);
    })
    .expect("paste into the search field");

    assert!(
        repaints.get() > 0,
        "#78: a paste into the search field did not ask the surface to render"
    );
    cx.update_window(window, |_, window, _| {
        assert_eq!(
            row_text_input(window, "settings-search", Role::TextInput).value(),
            Some("bandwidth"),
            "#78: the pasted text lands in the field"
        );
    })
    .expect("read the search field");
}
