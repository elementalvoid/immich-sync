//! The settings surface (SPEC R7, R8.5, R9): General, Limits, Sync, and the
//! read-only Storage locations.
//!
//! Every control is keyboard reachable: the page strip puts all six pages in
//! the Tab order, `Switch` activates with Enter or Space, `NumberInput` edits
//! with the keyboard and steps with Up and Down, and both draw the kit's focus
//! ring. Each row also observes its control's focus handle so a test can assert
//! reachability.

use std::rc::Rc;

use crate::autostart::Autostart;
use crate::connection::{ConnectionModel, apply_profile, profile_from_settings};
use crate::dashboard::CardPalette;
use crate::engine::EngineModel;
use crate::logging::LogLevel;
use crate::notice::{Notice, Severity};
use crate::paths::AppPaths;
use crate::settings::{Settings, SettingsStore};
use crate::tab::Tab;

use gpui_kit::base::ObservedElement;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, NumberInput};
use gpui_kit::component::radio::Radio;
use gpui_kit::component::setting::{
    SelectIndex, SettingField, SettingGroup, SettingItem, SettingPage, Settings as SettingsPages,
};
use gpui_kit::component::switch::Switch;
use gpui_kit::*;
use sync_core::connection::{AuthMethod, ConnectionState, ServerProfile, SignIn};
use sync_core::credential::Secret;
use sync_core::tags::{BackfillStatus, SYNC_TAG, TagStatus};

/// The number of bytes in a displayed megabyte. The stored limit stays bytes
/// per second (SPEC R7.1); the UI shows MB/s with up to six decimals, which is
/// one byte.
pub const BYTES_PER_MEGABYTE: u64 = 1_000_000;

/// One settings page: the element id of the tab that opens it, its name, and the
/// sentence under the name.
struct Page {
    id: &'static str,
    title: &'static str,
    description: &'static str,
}

/// The pages, in the order the surface lists them. The page strip and the page
/// a tab opens read the same entry, so a name cannot drift from its tab.
const PAGES: [Page; 6] = [
    Page {
        id: "settings-page-connection",
        title: "Connection",
        description: "The server and the account this client uploads to.",
    },
    Page {
        id: "settings-page-general",
        title: "General",
        description: "How the client starts.",
    },
    Page {
        id: "settings-page-limits",
        title: "Limits",
        description: "How much the client uses while it syncs.",
    },
    Page {
        id: "settings-page-sync",
        title: "Sync",
        description: "How the client scans and retries.",
    },
    Page {
        id: "settings-page-tagging",
        title: "Tagging",
        description: "Adds one tag to the assets this client uploads, so they can be found through \
                      Immich's tag search.",
    },
    Page {
        id: "settings-page-storage",
        title: "Storage",
        description: "Read-only. The database stays on this computer; it never lives on a network \
                      share or in a cloud-sync folder.",
    },
];

/// The index of each page in `PAGES`, so a page builder names the page it builds
/// instead of repeating its title.
const PAGE_CONNECTION: usize = 0;
const PAGE_GENERAL: usize = 1;
const PAGE_LIMITS: usize = 2;
const PAGE_SYNC: usize = 3;
const PAGE_TAGGING: usize = 4;
const PAGE_STORAGE: usize = 5;

/// One numeric setting the view edits, its text, and its bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKey {
    /// The bandwidth limit, in the UI's megabytes per second.
    Bandwidth,
    /// The number of parallel uploads.
    Concurrency,
    /// The reconciliation scan interval in minutes.
    ScanInterval,
    /// The retry attempt cap.
    AttemptCap,
}

impl FieldKey {
    /// Every numeric field, in the order the view shows them.
    pub const ALL: [Self; 4] = [
        Self::Bandwidth,
        Self::Concurrency,
        Self::ScanInterval,
        Self::AttemptCap,
    ];

    /// The row's element id, which a test can find.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Bandwidth => "settings-bandwidth",
            Self::Concurrency => "settings-concurrency",
            Self::ScanInterval => "settings-scan-interval",
            Self::AttemptCap => "settings-attempt-cap",
        }
    }

    /// The field's name.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Bandwidth => "Bandwidth limit",
            Self::Concurrency => "Concurrent uploads",
            Self::ScanInterval => "Reconciliation scan interval",
            Self::AttemptCap => "Retry attempt cap",
        }
    }

    /// One line that says what the value does.
    #[must_use]
    pub fn help(self) -> &'static str {
        match self {
            Self::Bandwidth => "Zero means unlimited; under 0.001 MB/s the client uses 0.001 MB/s.",
            Self::Concurrency => "Uploads that run at the same time.",
            Self::ScanInterval => {
                "The watcher uploads changes as they happen; this scan finds what the watcher missed."
            }
            Self::AttemptCap => "Attempts per file before it needs attention.",
        }
    }

    /// The unit the field shows.
    #[must_use]
    pub fn unit(self) -> &'static str {
        match self {
            Self::Bandwidth => "MB/s",
            Self::Concurrency => "uploads",
            Self::ScanInterval => "minutes",
            Self::AttemptCap => "attempts",
        }
    }

    /// The smallest value the field accepts.
    #[must_use]
    pub fn min(self) -> u32 {
        match self {
            Self::Bandwidth => 0,
            Self::Concurrency | Self::ScanInterval | Self::AttemptCap => 1,
        }
    }

    /// The largest value the field accepts.
    #[must_use]
    pub fn max(self) -> u32 {
        match self {
            Self::Bandwidth => 10_000,
            Self::Concurrency => 64,
            Self::ScanInterval => 1440,
            Self::AttemptCap => 100,
        }
    }

    /// The Up/Down arrow step.
    #[must_use]
    pub fn step(self) -> u32 {
        match self {
            Self::ScanInterval => 5,
            Self::Bandwidth | Self::Concurrency | Self::AttemptCap => 1,
        }
    }

    /// The text the field shows for `settings`.
    #[must_use]
    pub fn display(self, settings: &Settings) -> String {
        match self {
            Self::Bandwidth => megabytes_per_second_text(settings.bandwidth_bytes_per_second),
            Self::Concurrency => settings.concurrency.to_string(),
            Self::ScanInterval => settings.scan_interval_minutes.to_string(),
            Self::AttemptCap => settings.attempt_cap.to_string(),
        }
    }

    /// Stores a displayed value in `settings`, clamped to the field's bounds.
    pub fn set(self, settings: &mut Settings, value: f64) {
        match self {
            Self::Bandwidth => {
                settings.bandwidth_bytes_per_second = bytes_per_second(value);
            }
            Self::Concurrency => {
                settings.concurrency = rounded_clamp(value, 1, 64);
            }
            Self::ScanInterval => {
                settings.scan_interval_minutes = rounded_clamp(value, 1, 1440);
            }
            Self::AttemptCap => {
                settings.attempt_cap = rounded_clamp(value, 1, 100);
            }
        }
    }
}

/// `value` rounded to a whole number inside `[min, max]`.
///
/// The bounds check makes the cast exact and keeps NaN and infinities from
/// becoming an out-of-range stored value.
fn rounded_clamp(value: f64, min: u32, max: u32) -> u32 {
    let rounded = value.round();
    if rounded.is_nan() || rounded <= f64::from(min) {
        min
    } else if rounded >= f64::from(max) {
        max
    } else {
        rounded as u32
    }
}

/// The stored bytes per second for a displayed MB/s value.
///
/// The value is rounded to the nearest whole byte, which is the stored unit.
fn bytes_per_second(megabytes: f64) -> u64 {
    if megabytes.is_nan() {
        return 0;
    }
    let bytes = (megabytes.clamp(0.0, 10_000.0) * BYTES_PER_MEGABYTE as f64).round();
    // The clamp above keeps the rounded value inside `u64`.
    bytes as u64
}

/// The MB/s text for a stored bytes-per-second limit.
///
/// Six decimals are one byte, so a stored value up to the field's 10,000 MB/s
/// bound (10,000,000,000 bytes per second) displays exactly and round-trips
/// through the field. Trailing zeros are trimmed.
fn megabytes_per_second_text(bytes: u64) -> String {
    let whole = bytes / BYTES_PER_MEGABYTE;
    let fraction = bytes % BYTES_PER_MEGABYTE;
    if fraction == 0 {
        return whole.to_string();
    }
    let mut fraction_text = format!("{fraction:06}");
    while fraction_text.ends_with('0') {
        fraction_text.pop();
    }
    format!("{whole}.{fraction_text}")
}

/// The notice prefix for a settings file that could not be written.
const SAVE_PROBLEM: &str = "Could not save the settings";
/// The notice prefix for an autostart change that could not be applied.
const AUTOSTART_PROBLEM: &str = "Could not update the autostart setting";
/// The notice prefix for an autostart state that could not be read.
const AUTOSTART_READ_PROBLEM: &str = "Could not read the autostart setting";

/// One numeric row: its key and the input state that edits it.
struct Field {
    key: FieldKey,
    input: Entity<InputState>,
}

/// What runs on the UI thread when one queued settings write lands (N2).
///
/// `Ok` means the file was written and the view's store now holds the new
/// values; `Err` leaves the previous store in place.
type SettingsWriteDone =
    Box<dyn FnOnce(&mut SettingsView, Result<(), anyhow::Error>, &mut Context<SettingsView>)>;

/// The settings view.
pub struct SettingsView {
    store: SettingsStore,
    paths: AppPaths,
    autostart: Rc<dyn Autostart>,
    /// The connection state shared with the dashboard (R8.1).
    connection: Entity<ConnectionModel>,
    /// The search field over the pages' rows (#78).
    search: Entity<InputState>,
    /// The page the strip shows (#78). The component is keyed on the page this
    /// selects, so re-keying is what moves it.
    page: usize,
    server_url_input: Entity<InputState>,
    email_input: Entity<InputState>,
    secret_input: Entity<InputState>,
    /// The credential kind the form signs in with (R1.1, R1.5).
    auth_method: AuthMethod,
    fields: Vec<Field>,
    /// The engine the Limits card pushes a change to (R7.1, R7.2), once the
    /// shell has installed one. A view with no engine still saves the values.
    engine: Option<Entity<EngineModel>>,
    /// The tag state the engine last published for the Tagging section (R3.7).
    /// It is cached here because the engine notifies on every status poll, and
    /// the section only changes when this value does.
    tag: TagStatus,
    /// The last failed save or autostart change, shown with the load problem.
    action_error: Option<String>,
    /// The approval hint the autostart backend last reported. It is read when the
    /// OS state changes, not on render: on macOS it is a login-item status call.
    approval_hint: Option<&'static str>,
    /// A failure before the store opened, such as path resolution. It stays
    /// visible until the process restarts, because a later save cannot fix it.
    environment_problem: Option<String>,
    /// The newest settings edit, so a slow write cannot install older values
    /// over a newer one (N2).
    settings_generation: u64,
    /// The newest settings waiting to be written; a burst of edits coalesces
    /// into one write.
    pending_settings: Option<(u64, Settings)>,
    /// What runs when `pending_settings` lands.
    pending_finish: Option<SettingsWriteDone>,
    /// The settings a write is currently writing, so a further edit is based
    /// on them instead of the older store while the write is in flight. Its
    /// presence also keeps one atomic save's temporary file from being raced
    /// (N2).
    in_flight_settings: Option<(u64, Settings)>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    /// Builds the view over an already-loaded store.
    pub fn new(
        store: SettingsStore,
        paths: AppPaths,
        autostart: Rc<dyn Autostart>,
        connection: Entity<ConnectionModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial = store.settings().clone();
        let mut fields = Vec::with_capacity(FieldKey::ALL.len());
        let mut subscriptions = Vec::with_capacity(FieldKey::ALL.len() + 2);
        for key in FieldKey::ALL {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    // The kit names the text field from the placeholder, and
                    // the row's own name covers the whole control.
                    .placeholder(key.label())
                    .default_value(key.display(store.settings()))
                    .step(f64::from(key.step()))
                    .min(f64::from(key.min()))
                    .max(f64::from(key.max()))
            });
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |this, input, event: &InputEvent, _window, cx| {
                    if matches!(event, InputEvent::Change)
                        && let Ok(value) = input.read(cx).value().trim().parse::<f64>()
                    {
                        this.write_field(key, value, cx);
                    }
                },
            ));
            fields.push(Field { key, input });
        }

        let server_url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://immich.example.com")
                .default_value(initial.server_url.clone())
        });
        let email_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("you@example.com")
                .default_value(initial.email.clone())
        });
        let secret_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(secret_placeholder_for(initial.auth_method))
                .masked(true)
        });
        subscriptions.push(cx.subscribe_in(
            &secret_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.sign_in(window, cx);
                }
            },
        ));
        // #78: the surface filters its own rows from this field's value, the
        // way the kit's own settings search does: the value is read when the
        // surface renders, not kept as a second copy that an event can miss.
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search settings"));
        // Every text replacement emits `Change`, a paste included, and a paste
        // comes with no keystroke for a key listener to catch (S1).
        subscriptions.push(
            cx.subscribe_in(&search, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        );
        // The profile is persisted only once the model reaches connected, so
        // a refused sign-in leaves the saved profile and credential in step.
        subscriptions.push(cx.observe(&connection, |this, _, cx| {
            this.save_connected_profile(cx);
        }));

        let mut view = Self {
            store,
            paths,
            autostart,
            connection,
            search,
            page: 0,
            server_url_input,
            email_input,
            secret_input,
            auth_method: initial.auth_method,
            fields,
            engine: None,
            tag: TagStatus::default(),
            action_error: None,
            approval_hint: None,
            environment_problem: None,
            settings_generation: 0,
            pending_settings: None,
            pending_finish: None,
            in_flight_settings: None,
            _subscriptions: subscriptions,
        };
        view.reconcile_autostart(cx);
        // A saved profile logs in at startup with no user input (R1.3).
        // Reopening the window rebuilds this view, so the model's once guard
        // keeps that restore from restarting the saved session. A first run
        // has nothing to restore, but it must consume the guard too, or the
        // first reopen would restore the profile the first sign-in just saved.
        let profile = profile_from_settings(view.store.settings());
        view.connection
            .update(cx, |model, cx| model.restore_once(profile, cx));
        view
    }

    /// Builds the view from the real platform directories and OS autostart.
    pub fn from_environment(
        window: &mut Window,
        cx: &mut Context<Self>,
        connection: Entity<ConnectionModel>,
    ) -> Self {
        Self::from_resolved_paths(AppPaths::resolve(), window, cx, connection)
    }

    /// Builds the view from an already-attempted path resolution.
    ///
    /// A resolution failure keeps the window usable on the fallback paths and
    /// shows the reason through the same notice as a corrupt settings file.
    pub fn from_resolved_paths(
        resolved: anyhow::Result<AppPaths>,
        window: &mut Window,
        cx: &mut Context<Self>,
        connection: Entity<ConnectionModel>,
    ) -> Self {
        let (store, paths, environment_problem) = match resolved {
            Ok(paths) => (SettingsStore::open(paths.config_file.clone()), paths, None),
            Err(error) => {
                let paths = AppPaths::fallback();
                let problem = format!(
                    "Could not resolve the settings location, so the client uses {}: {error:#}",
                    paths.config_file.display()
                );
                (
                    SettingsStore::open(paths.config_file.clone()),
                    paths,
                    Some(problem),
                )
            }
        };
        let autostart = crate::autostart::platform_autostart(&paths).unwrap_or_else(|error| {
            tracing::error!(
                error = %format!("{error:#}"),
                "could not prepare the autostart mechanism; the setting is unavailable"
            );
            Rc::new(crate::autostart::UnavailableAutostart::new(format!(
                "{error:#}"
            )))
        });
        let mut view = Self::new(store, paths, autostart, connection, window, cx);
        view.environment_problem = environment_problem;
        view
    }

    /// The resolved paths, so the Logs view can show where the log lives (R9.3).
    #[must_use]
    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    /// The persisted log level (R10.3).
    #[must_use]
    pub fn log_level(&self) -> LogLevel {
        self.store.settings().log_level
    }

    /// Points the Limits card at the engine, so a change to either limit is
    /// applied to a running sync without a restart (R7.1, R7.2).
    ///
    /// The Tagging section follows the engine's tag state too (R3.7). The
    /// engine notifies on every status poll, so the view repaints only when the
    /// state it renders actually changes.
    pub fn set_engine(&mut self, engine: Entity<EngineModel>, cx: &mut Context<Self>) {
        self.tag = engine.read(cx).tag_status().clone();
        self._subscriptions
            .push(cx.observe(&engine, |this, engine, cx| {
                let tag = engine.read(cx).tag_status().clone();
                if tag != this.tag {
                    this.tag = tag;
                    cx.notify();
                }
            }));
        self.engine = Some(engine);
    }

    /// The newest intended settings: what a pending write will hold, or the
    /// store's values when nothing is queued.
    fn intended_settings(&self) -> Settings {
        self.pending_settings
            .as_ref()
            .or(self.in_flight_settings.as_ref())
            .map_or_else(
                || self.store.settings().clone(),
                |(_, settings)| settings.clone(),
            )
    }

    /// The last save failure, for the Logs view's own notice (R10.3).
    #[must_use]
    pub fn save_problem(&self) -> Option<&str> {
        self.action_error
            .as_deref()
            .filter(|error| error.starts_with(SAVE_PROBLEM))
    }

    /// Applies one settings change and writes it on the background executor
    /// (N2).
    ///
    /// The change is computed against the newest intended state, so a second
    /// edit before the first write lands is not lost. `finish` runs on the UI
    /// thread after the write; a superseded failure cannot roll back a newer
    /// choice, because only the newest edit's finish runs.
    fn save_settings<F>(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Settings) + Send + 'static,
        finish: F,
    ) where
        F: FnOnce(&mut Self, Result<(), anyhow::Error>, &mut Context<Self>) + 'static,
    {
        let current = self.intended_settings();
        let mut next = current.clone();
        change(&mut next);
        self.settings_generation = self.settings_generation.wrapping_add(1);
        let generation = self.settings_generation;
        if next == current && self.pending_settings.is_none() && self.in_flight_settings.is_none() {
            // The file already holds this value and no write can still fail,
            // so there is nothing to write. While a write is in flight, queue
            // a redundant write instead: its finish reports the truth about
            // that write, which a separate no-op success would mask.
            finish(self, Ok(()), cx);
            return;
        }
        self.pending_settings = Some((generation, next));
        self.pending_finish = Some(Box::new(finish));
        self.pump_settings_write(cx);
    }

    /// Writes the newest pending settings, one write at a time (N2).
    ///
    /// A write that is superseded while it runs still installs the store it
    /// wrote, so the next write starts from what the file holds; only its
    /// finish is dropped.
    fn pump_settings_write(&mut self, cx: &mut Context<Self>) {
        if self.in_flight_settings.is_some() {
            return;
        }
        let Some((generation, settings)) = self.pending_settings.take() else {
            return;
        };
        let Some(finish) = self.pending_finish.take() else {
            return;
        };
        self.in_flight_settings = Some((generation, settings.clone()));
        let store = self.store.clone();
        let write = cx.background_executor().spawn(async move {
            let mut store = store;
            store.update(|current| *current = settings).map(|()| store)
        });
        cx.spawn(async move |this, cx| {
            let result = write.await;
            let _ = this.update(cx, |view, cx| {
                view.in_flight_settings = None;
                match result {
                    Ok(store) => {
                        view.store = store;
                        if view.settings_generation == generation {
                            finish(view, Ok(()), cx);
                        }
                    }
                    Err(error) => {
                        if view.settings_generation == generation {
                            finish(view, Err(error), cx);
                        }
                    }
                }
                view.pump_settings_write(cx);
            });
        })
        .detach();
    }

    /// Persists a log level and applies it to the running subscriber.
    ///
    /// The Logs view calls this, so the settings file keeps its one writer.
    /// The subscriber keeps its last saved value until the write lands, so the
    /// level, the settings file, and the subscriber never disagree.
    pub fn set_log_level(&mut self, level: LogLevel, cx: &mut Context<Self>) {
        self.save_settings(
            cx,
            move |settings| settings.log_level = level,
            move |view, result, cx| {
                if result.is_ok() {
                    crate::logging::set_level(level);
                }
                view.record(SAVE_PROBLEM, result, cx);
            },
        );
    }

    /// Signs in with the form's values. The profile is saved only once the
    /// session reaches connected (see [`Self::save_connected_profile`]), and
    /// the secret itself never reaches the settings file (R1.2).
    fn sign_in(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let profile = ServerProfile {
            server_url: self.server_url_input.read(cx).value().trim().to_string(),
            email: self.email_input.read(cx).value().trim().to_string(),
            method: self.auth_method,
        };
        let request = SignIn {
            email: profile.email.clone(),
            secret: Secret::new(self.secret_input.read(cx).value().to_string()),
            method: profile.method,
        };
        self.connection
            .update(cx, |model, cx| model.sign_in(request, profile, cx));
    }

    /// Saves the profile the session actually connected with.
    ///
    /// Saving it when the form was merely valid would persist a new server or
    /// email while the credential store still held the old credential, so the
    /// next start would retry a mismatched pair and break a working setup
    /// (R1.2). The session reaches connected only after the credential store
    /// accepted the new secret (R1.3).
    fn save_connected_profile(&mut self, cx: &mut Context<Self>) {
        if self.connection.read(cx).state() != &ConnectionState::Connected {
            return;
        }
        let profile = self.connection.read(cx).profile().clone();
        if profile_from_settings(&self.intended_settings()) == profile {
            return;
        }
        self.save_settings(
            cx,
            move |settings| apply_profile(settings, &profile),
            |view, result, cx| view.record(SAVE_PROBLEM, result, cx),
        );
    }

    /// Switches the credential kind the form signs in with.
    fn set_auth_method(&mut self, method: AuthMethod, window: &mut Window, cx: &mut Context<Self>) {
        self.auth_method = method;
        self.secret_input.update(cx, |input, cx| {
            input.set_placeholder(secret_placeholder_for(method), window, cx);
        });
        cx.notify();
    }

    /// The placeholder the secret field currently shows, for a test and for
    /// assistive technology.
    #[must_use]
    pub fn secret_placeholder(&self, cx: &App) -> String {
        self.secret_input
            .read(cx)
            .presentation()
            .placeholder()
            .to_string()
    }

    /// Brings the stored autostart flag in line with the real OS mechanism.
    ///
    /// The Run key or `.desktop` entry can change outside the client. The
    /// switch must show what the OS does, or turning it off would be a no-op
    /// against a file that already said off. The correction goes through the
    /// background writer, like every other settings save (N2), so a slow disk
    /// never blocks the UI thread at startup.
    fn reconcile_autostart(&mut self, cx: &mut Context<Self>) {
        // The approval hint has its own source, so it is read even when the
        // settings file has a problem.
        self.approval_hint = self.autostart.approval_needed();
        // A corrupt file holds defaults, so its flag is not a stored value. A
        // write here would replace the unparsable bytes and clear the load
        // problem, so leave the file and the notice alone until the user acts.
        if self.store.problem().is_some() {
            return;
        }
        let stored = self.store.settings().autostart;
        let actual = match self.autostart.is_enabled() {
            Ok(actual) => actual,
            Err(error) => {
                self.record(
                    AUTOSTART_READ_PROBLEM,
                    Err(error.context("read the OS autostart state")),
                    cx,
                );
                return;
            }
        };
        if actual != stored {
            self.save_settings(
                cx,
                move |settings| settings.autostart = actual,
                |view, result, cx| view.record(AUTOSTART_PROBLEM, result, cx),
            );
        }
    }

    /// Records one settings change and saves it (N2).
    fn write_field(&mut self, key: FieldKey, value: f64, cx: &mut Context<Self>) {
        self.save_settings(
            cx,
            move |settings| key.set(settings, value),
            move |view, result, cx| {
                // The stored value is what the engine gets, so a failed save
                // cannot move the engine away from the file.
                if result.is_ok()
                    && matches!(key, FieldKey::Bandwidth | FieldKey::Concurrency)
                    && let Some(engine) = view.engine.clone()
                {
                    let settings = view.store.settings();
                    let bandwidth = settings.bandwidth_bytes_per_second;
                    let concurrency = usize::try_from(settings.concurrency.max(1)).unwrap_or(4);
                    engine.update(cx, |model, _| model.set_limits(bandwidth, concurrency));
                }
                view.record(SAVE_PROBLEM, result, cx);
            },
        );
    }

    /// Saves the sync tag setting and moves the running engine to it (R3.7).
    ///
    /// The engine is moved only after the write lands, so a failed save cannot
    /// leave the engine tagging while the file says off. Turning it off stops
    /// new tagging and pauses the backfill at its next batch; nothing untags an
    /// asset and the tag is not deleted. Turning it on again resumes the
    /// backfill where it stopped.
    fn toggle_tag_uploads(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.save_settings(
            cx,
            move |settings| settings.tag_uploaded_assets = enabled,
            move |view, result, cx| {
                if result.is_ok()
                    && let Some(engine) = view.engine.clone()
                {
                    engine.update(cx, |model, _| model.set_tag_uploads(enabled));
                }
                view.record(SAVE_PROBLEM, result, cx);
            },
        );
    }

    /// Asks the running engine to tag every existing asset again (R3.7).
    ///
    /// The action goes through the engine model, which owns the engine and its
    /// runtime: the view never opens the database or sends a request itself
    /// (N2).
    fn tag_existing_assets(&mut self, cx: &mut Context<Self>) {
        if let Some(engine) = self.engine.clone() {
            engine.update(cx, |model, cx| model.tag_existing_assets(cx));
        }
    }

    /// Applies the autostart setting to the OS, then records it.
    ///
    /// The OS and the settings file are two stores. A failed file write rolls
    /// the OS change back, so the switch never claims a state one store does
    /// not hold.
    fn toggle_autostart(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let applied = self.autostart.set_enabled(enabled);
        // One status read per user action, never per render.
        self.approval_hint = self.autostart.approval_needed();
        if let Err(error) = applied {
            let error = error.context("apply the OS autostart change");
            self.record(AUTOSTART_PROBLEM, Err(error), cx);
            return;
        }
        self.save_settings(
            cx,
            move |settings| settings.autostart = enabled,
            move |view, result, cx| {
                let result = match result {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        // The file still holds the previous value; bring the
                        // OS back to it, so the two stores agree.
                        let stored = view.store.settings().autostart;
                        let rolled_back = view.autostart.set_enabled(stored);
                        view.approval_hint = view.autostart.approval_needed();
                        match rolled_back {
                            Ok(()) => Err(error),
                            Err(rollback) => Err(error.context(format!(
                                "and rolling the OS autostart change back failed: {rollback:#}"
                            ))),
                        }
                    }
                };
                view.record(AUTOSTART_PROBLEM, result, cx);
            },
        );
    }

    /// Shows a failure, or clears the previous one after a success.
    fn record(&mut self, prefix: &'static str, result: anyhow::Result<()>, cx: &mut Context<Self>) {
        self.action_error = result.err().map(|error| format!("{prefix}: {error:#}"));
        cx.notify();
    }

    /// The surface header: what the surface is, and the search over its rows.
    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let muted = cx.theme().muted_foreground;
        let focus = self.search.read(cx).focus_handle(cx);
        div()
            .flex()
            .flex_row()
            .justify_between()
            .items_start()
            .gap_6()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    // The field beside this is a fixed width, so the copy has to
                    // wrap in what the minimum window leaves rather than push
                    // the field past the edge (#75's minimum-window test).
                    .min_w_0()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("Defaults for the sync client. Changes save immediately."),
                    ),
            )
            .child(
                // #78: the surface owns the query, because it also owns the
                // result when nothing matches. The component still owns the
                // pages, the groups, the rows, and the sidebar; its own search
                // field is dropped by `hidden()`.
                div()
                    .id("settings-search")
                    .test_support()
                    .track_focus(&focus)
                    .w_64()
                    .flex_none()
                    .child(Input::new(&self.search).aria_label("Search settings")),
            )
    }

    /// The page strip: one tab per page, the shell's own view switch and this
    /// strip draw the same element (#75, #78).
    ///
    /// A kit sidebar entry has no focus handle and no key binding, so the strip
    /// is the only keyboard path between the pages (#78).
    fn page_strip(
        &self,
        selected: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let mut strip = div().flex().flex_row().flex_wrap().items_center().gap_1();
        for (ix, page) in PAGES.iter().enumerate() {
            // The handles are claimed in the loop that needs `cx` for its
            // listeners, so each one is read out before that borrow.
            let focus = window
                .use_keyed_state(page.id, cx, |_, cx| cx.focus_handle().tab_stop(true))
                .read(cx)
                .clone();
            strip = strip.child(
                Tab::new(
                    page.id,
                    page.title,
                    ix == selected,
                    &focus,
                    ix + 1,
                    PAGES.len(),
                )
                .render(window, cx)
                .on_click(cx.listener(move |this, _, _, cx| this.show_page(ix, cx))),
            );
        }
        strip
    }

    /// Opens one page from the strip.
    fn show_page(&mut self, page: usize, cx: &mut Context<Self>) {
        self.page = page;
        cx.notify();
    }

    /// The problem notices, when the file failed to load or an action failed.
    fn notices(&self) -> Vec<AnyElement> {
        let mut notices = Vec::new();
        if let Some(problem) = self.store.problem() {
            notices.push(
                Notice::new("settings-problem", Severity::Warning, problem.to_string())
                    .into_any_element(),
            );
        } else if let Some(problem) = &self.environment_problem {
            notices.push(
                Notice::new("settings-problem", Severity::Warning, problem.clone())
                    .into_any_element(),
            );
        }
        if let Some(error) = &self.action_error {
            notices.push(
                Notice::new("settings-action-error", Severity::Error, error.clone())
                    .into_any_element(),
            );
        }
        notices
    }

    /// What the surface shows when no row matches the search, so an empty
    /// result says so instead of leaving an empty pane.
    fn no_matches(query: &str) -> impl IntoElement + use<> {
        Notice::new(
            "settings-search-empty",
            Severity::Info,
            format!("No setting matches \"{}\".", query.trim()),
        )
    }

    /// Every page that survives the search query, with the index it has in the
    /// unfiltered set.
    ///
    /// A page keeps the rows whose title or help sentence contain the query,
    /// and a page or group left without one is dropped.
    fn pages(&self, query: &str, cx: &mut Context<Self>) -> Vec<(usize, SettingPage)> {
        let query = query.trim().to_lowercase();
        [
            self.connection_page(&query, cx),
            self.general_page(&query, cx),
            self.limits_page(&query, cx),
            self.sync_page(&query, cx),
            self.tagging_page(&query, cx),
            self.storage_page(&query, cx),
        ]
        .into_iter()
        .enumerate()
        .filter_map(|(index, page)| page.map(|page| (index, page)))
        .collect()
    }

    /// The Connection page: the server, the account, and the sign-in form
    /// (R1.1 to R1.6).
    fn connection_page(&self, query: &str, cx: &mut Context<Self>) -> Option<SettingPage> {
        let (state, form_error) = {
            let model = self.connection.read(cx);
            (
                model.state().clone(),
                model.form_error().map(str::to_string),
            )
        };
        let needs_api_key = matches!(
            &state,
            ConnectionState::NeedsAttention(attention) if attention.needs_api_key()
        );
        let state_severity = connection_severity(&state);

        let server = vec![
            Self::text_row(
                &self.server_url_input,
                "settings-server-url",
                "Server URL",
                "For example https://immich.example.com. Certificate verification stays on.",
                cx,
            ),
            Self::text_row(
                &self.email_input,
                "settings-email",
                "Email address",
                "The Immich account this client uploads as.",
                cx,
            ),
        ];

        let sign_in_click = Rc::new(cx.listener(|this, _, window, cx| this.sign_in(window, cx)));
        let mut sign_in = vec![
            self.method_row(cx),
            self.secret_row(cx),
            note(move |_, _| {
                let click = Rc::clone(&sign_in_click);
                div()
                    .id("settings-sign-in-row")
                    .test_support()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .child(
                        Button::new("settings-sign-in")
                            .primary()
                            .label("Sign in")
                            .accessibility_label("Sign in")
                            .on_click(move |event, window, cx| click(event, window, cx)),
                    )
                    .into_any_element()
            }),
            note(move |_, _| {
                Notice::new("settings-connection-state", state_severity, state.message())
                    .into_any_element()
            }),
        ];
        if needs_api_key {
            sign_in.push(note(|_, _| {
                Notice::new(
                    "settings-connection-hint",
                    Severity::Info,
                    "Switch the credential to API key and paste a key from Immich (Account \
                     settings, then API keys).",
                )
                .into_any_element()
            }));
        }
        if let Some(error) = form_error {
            sign_in.push(note(move |_, _| {
                Notice::new("settings-connection-error", Severity::Error, error.clone())
                    .into_any_element()
            }));
        }

        page(
            PAGE_CONNECTION,
            vec![
                group(Some("Server"), server, query),
                group(Some("Sign in"), sign_in, query),
            ],
        )
    }

    /// One text input row (R1.1).
    fn text_row(
        input: &Entity<InputState>,
        id: &'static str,
        title: &'static str,
        help: &'static str,
        cx: &App,
    ) -> Row {
        let input = input.clone();
        let focus = input.read(cx).focus_handle(cx);
        Row::stacked(title, help, move |_, _| {
            div()
                .id(id)
                .test_support()
                .aria_label(title)
                .aria_description(help)
                .track_focus(&focus)
                .min_w_0()
                .w_full()
                .child(Input::new(&input).aria_label(title))
                .into_any_element()
        })
    }

    /// The credential kind: password or API key (R1.1, R1.5).
    fn method_row(&self, cx: &mut Context<Self>) -> Row {
        let password = Rc::new(cx.listener(|this, checked: &bool, window, cx| {
            if *checked {
                this.set_auth_method(AuthMethod::Password, window, cx);
            }
        }));
        let api_key = Rc::new(cx.listener(|this, checked: &bool, window, cx| {
            if *checked {
                this.set_auth_method(AuthMethod::ApiKey, window, cx);
            }
        }));
        let method = self.auth_method;
        Row::stacked(
            "Credential",
            "A password for the account, or an API key from Immich.",
            move |_, _| {
                let password = Rc::clone(&password);
                let api_key = Rc::clone(&api_key);
                div()
                    .id("settings-auth-method")
                    .test_support()
                    .aria_label("Credential")
                    .aria_description("A password for the account, or an API key from Immich.")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .child(
                        Radio::new("settings-auth-password")
                            .label("Password")
                            .accessibility_label("Sign in with a password")
                            .checked(method == AuthMethod::Password)
                            .on_change(move |checked, window, cx| password(checked, window, cx)),
                    )
                    .child(
                        Radio::new("settings-auth-apikey")
                            .label("API key")
                            .accessibility_label("Sign in with an API key")
                            .checked(method == AuthMethod::ApiKey)
                            .on_change(move |checked, window, cx| api_key(checked, window, cx)),
                    )
                    .into_any_element()
            },
        )
    }

    /// The password or API key input. The value is masked and never saved to
    /// the settings file (R1.2).
    fn secret_row(&self, cx: &mut Context<Self>) -> Row {
        let label = secret_placeholder_for(self.auth_method);
        Self::text_row(
            &self.secret_input,
            "settings-secret",
            label,
            "Stored in the OS credential store, never in the settings file.",
            cx,
        )
    }

    /// The General page: how the client starts.
    fn general_page(&self, query: &str, cx: &mut Context<Self>) -> Option<SettingPage> {
        // The switch shows the user's newest intent, so a queued or in-flight
        // write is reflected before it lands (N2).
        let enabled = self.intended_settings().autostart;
        let toggle = Rc::new(cx.listener(|this, enabled: &bool, _, cx| {
            this.toggle_autostart(*enabled, cx);
        }));
        let mut rows = vec![Row::wrapped(
            "Autostart at login",
            "Start Immich Sync when you log in. Off by default.",
            "settings-autostart-row",
            move |_, _| {
                let toggle = Rc::clone(&toggle);
                Switch::new("settings-autostart")
                    .checked(enabled)
                    .accessibility_label("Start Immich Sync at login")
                    .on_change(move |enabled, window, cx| toggle(enabled, window, cx))
                    .into_any_element()
            },
        )];
        // macOS registers the item and leaves the last step to the user; the
        // setting stays on. The backend supplies the text, and the view caches
        // it, so no platform detail or OS call lives on the render path.
        if let Some(hint) = self.approval_hint {
            let open = Rc::new(cx.listener(|this, _, _, _| {
                this.autostart.open_approval_settings();
            }));
            rows.push(Row::wrapped(
                "Approval needed",
                hint,
                "settings-autostart-approval",
                move |_, _| {
                    let open = Rc::clone(&open);
                    Button::new("settings-autostart-approve")
                        .label("Open Login Items")
                        .accessibility_label("Open Login Items settings")
                        .on_click(move |event, window, cx| open(event, window, cx))
                        .into_any_element()
                },
            ));
        }
        page(PAGE_GENERAL, vec![group(None, rows, query)])
    }

    /// The Limits page: bandwidth and concurrency (SPEC R7).
    fn limits_page(&self, query: &str, cx: &App) -> Option<SettingPage> {
        page(
            PAGE_LIMITS,
            vec![group(
                None,
                [FieldKey::Bandwidth, FieldKey::Concurrency]
                    .into_iter()
                    .filter_map(|key| self.numeric_row(key, cx))
                    .collect(),
                query,
            )],
        )
    }

    /// The Sync page: scan interval and attempt cap (SPEC R2.3, R6.1).
    fn sync_page(&self, query: &str, cx: &App) -> Option<SettingPage> {
        page(
            PAGE_SYNC,
            vec![group(
                None,
                [FieldKey::ScanInterval, FieldKey::AttemptCap]
                    .into_iter()
                    .filter_map(|key| self.numeric_row(key, cx))
                    .collect(),
                query,
            )],
        )
    }

    /// The Tagging page: the one sync tag, its switch, and where Immich shows
    /// it (SPEC R3.7).
    fn tagging_page(&self, query: &str, cx: &mut Context<Self>) -> Option<SettingPage> {
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        let muted = theme.muted_foreground;
        // The switch shows the user's newest intent, so a queued or in-flight
        // write is reflected before it lands (N2).
        let settings = self.intended_settings();
        let enabled = settings.tag_uploaded_assets;
        let link = sync_tag_url(&settings.server_url);
        // R3.7: the engine ran the one permission probe; this page only
        // reports what it found, so opening Settings costs no request.
        let problem = self.tag.enabled.then(|| self.tag.problem.clone()).flatten();

        let toggle = Rc::new(cx.listener(|this, enabled: &bool, _, cx| {
            this.toggle_tag_uploads(*enabled, cx);
        }));
        let mut rows = vec![
            Row::wrapped(
                "Tag uploaded assets",
                "Adds the tag to every asset the client uploads. On by default.",
                "settings-tag-upload-row",
                move |_, _| {
                    let toggle = Rc::clone(&toggle);
                    Switch::new("settings-tag-upload")
                        .checked(enabled)
                        .accessibility_label("Tag uploaded assets with immich-sync")
                        .on_change(move |enabled, window, cx| toggle(enabled, window, cx))
                        .into_any_element()
                },
            ),
            Row::wrapped(
                "Tag",
                "Fixed. The client always uses this name.",
                "settings-tag-name-row",
                move |_, _| {
                    div()
                        .id("settings-tag-name")
                        .test_support()
                        .aria_label(SYNC_TAG)
                        .font_family(mono.clone())
                        .text_sm()
                        .child(SYNC_TAG)
                        .into_any_element()
                },
            ),
            note(move |_, _| {
                div()
                    .id("settings-tag-prerequisite")
                    .test_support()
                    .text_xs()
                    .text_color(muted)
                    .child(
                        "Immich hides tags until you enable Tags in your Immich preferences \
                         (Account Settings, then Features).",
                    )
                    .into_any_element()
            }),
        ];
        if let Some(url) = link {
            rows.push(Row::wrapped(
                "View in Immich",
                "Opens the tag's page in your browser.",
                "settings-tag-link-row",
                move |_, _| {
                    let url = url.clone();
                    Button::new("settings-tag-link")
                        .label("View in Immich")
                        .accessibility_label("Open the sync tag's page in Immich")
                        .on_click(move |_, _, cx| cx.open_url(&url))
                        .into_any_element()
                },
            ));
        }
        if let Some(problem) = problem {
            rows.push(note(move |_, _| {
                Notice::new(
                    "settings-tag-problem",
                    Severity::Warning,
                    format!("The sync tag is not being applied: {problem}"),
                )
                .into_any_element()
            }));
        }
        // R3.7: the backfill's progress arrives on the engine's status poll, so
        // the page has no timer of its own; the counts only change when a batch
        // lands.
        if let Some(progress) = backfill_text(&self.tag.backfill, enabled) {
            rows.push(note(move |_, _| {
                div()
                    .id("settings-tag-backfill")
                    .test_support()
                    .aria_label(progress.clone())
                    .text_xs()
                    .text_color(muted)
                    .child(progress.clone())
                    .into_any_element()
            }));
        }
        if enabled && self.engine.is_some() {
            let tag_now = Rc::new(cx.listener(|this, _, _, cx| this.tag_existing_assets(cx)));
            rows.push(Row::wrapped(
                "Tag existing assets now",
                "Tags every asset the client has uploaded, including the ones that were \
                 uploaded before the switch was on.",
                "settings-tag-now-row",
                move |_, _| {
                    let tag_now = Rc::clone(&tag_now);
                    Button::new("settings-tag-now")
                        .label("Tag existing assets now")
                        .accessibility_label("Tag every existing asset with immich-sync")
                        .on_click(move |event, window, cx| tag_now(event, window, cx))
                        .into_any_element()
                },
            ));
        }
        page(PAGE_TAGGING, vec![group(None, rows, query)])
    }

    /// The Storage page: the resolved locations, read-only (SPEC R9).
    fn storage_page(&self, query: &str, cx: &App) -> Option<SettingPage> {
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        let muted = theme.muted_foreground;
        page(
            PAGE_STORAGE,
            vec![group(
                None,
                vec![
                    storage_row(
                        "settings-path-config",
                        "settings-path-config-value",
                        "Settings file",
                        self.paths.config_file.display().to_string(),
                        mono.clone(),
                        muted,
                    ),
                    storage_row(
                        "settings-path-state",
                        "settings-path-state-value",
                        "State database",
                        self.paths.state_db.display().to_string(),
                        mono,
                        muted,
                    ),
                ],
                query,
            )],
        )
    }

    /// One numeric row: the field, its unit, and its focus handle.
    fn numeric_row(&self, key: FieldKey, cx: &App) -> Option<Row> {
        let input = self
            .fields
            .iter()
            .find(|field| field.key == key)?
            .input
            .clone();
        let focus = input.read(cx).focus_handle(cx);
        let muted = cx.theme().muted_foreground;
        let unit = key.unit();
        Some(Row::stacked(key.label(), key.help(), move |_, _| {
            div()
                .id(key.id())
                .test_support()
                .aria_label(key.label())
                .aria_description(key.help())
                .track_focus(&focus)
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    // The component's root is `flex_1`, so the fixed width
                    // belongs on a non-growing wrapper.
                    div().w_32().flex_none().child(NumberInput::new(&input)),
                )
                .child(
                    div()
                        .w_16()
                        .flex_none()
                        .text_xs()
                        .text_color(muted)
                        .child(unit),
                )
                .into_any_element()
        }))
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let background = theme.background;
        let foreground = theme.foreground;
        let muted = theme.muted_foreground;
        let notices = self.notices();
        let header = self.header(cx);
        let query = self.search.read(cx).value().to_string();
        let pages = self.pages(&query, cx);
        // The page the surface shows: the one the strip picked while the query
        // leaves it a row, otherwise the first page the query landed on.
        let shown = if pages.iter().any(|(index, _)| *index == self.page) {
            self.page
        } else {
            pages.first().map_or(self.page, |(index, _)| *index)
        };
        // The component keys its selection on its own id, so the page it shows is
        // seeded and re-keyed from here (S5).
        let selected = pages
            .iter()
            .position(|(index, _)| *index == shown)
            .unwrap_or(0);
        let strip = self.page_strip(shown, window, cx);
        let body = if pages.is_empty() {
            div()
                .flex_1()
                .min_h_0()
                .child(Self::no_matches(&query))
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .relative()
                // The component's sidebar is collapsed to nothing but still draws
                // its resize handle, half of it inside this box; see
                // `handle_cover`.
                .pl(px(1.))
                .overflow_hidden()
                .child(RenderOnce::render(
                    SettingsPages::new(("settings-pages", selected))
                        .pages(pages.into_iter().map(|(_, page)| page))
                        .default_selected_index(SelectIndex {
                            page_ix: selected,
                            group_ix: None,
                        })
                        .sidebar_width(px(0.))
                        .sidebar_size_range(px(0.)..px(0.))
                        .sidebar_style(&hidden())
                        .header_style(&hidden()),
                    window,
                    cx,
                ))
                .child(handle_cover(background))
                .into_any_element()
        };

        div()
            .id("settings-scroll")
            .test_support()
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .p_4()
            .gap_2()
            .child(header)
            .children(notices)
            .child(strip)
            .child(body)
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Every control is reachable with the Tab key."),
            )
    }
}

/// One row of the settings surface: the words the search compares, and the item
/// the kit lays out.
///
/// The id, name and help attributes stay on the wrapper each row builder creates
/// around its control, because that wrapper is also what tracks the control's
/// focus handle: `text_row` and `numeric_row` forward the kit input's handle, and
/// a `Switch` or `Button` names itself. A row cannot own that wrapper without
/// also owning a handle it does not have (S4, recorded for #80).
struct Row {
    /// The row's title, which names its control.
    title: &'static str,
    /// The help sentence under the title.
    help: &'static str,
    /// The setting item the kit renders.
    item: SettingItem,
}

impl Row {
    /// A row whose control is the element the row itself names.
    fn setting(
        title: &'static str,
        help: &'static str,
        control: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        let field = SettingField::<SharedString>::render(move |_, window, cx| control(window, cx));
        Self {
            title,
            help,
            item: SettingItem::new(title, field).description(help),
        }
    }

    /// A row whose field is as wide as the page: the label and help sit above
    /// it at every window size.
    ///
    /// A long label word and a fixed-width field do not fit side by side on a
    /// page just wider than the component's stacking breakpoint, and the kit's
    /// field slot sizes to its content, so the field would draw past the page.
    fn stacked(
        title: &'static str,
        help: &'static str,
        control: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        let field = SettingField::<SharedString>::render(move |_, window, cx| control(window, cx));
        Self {
            title,
            help,
            item: SettingItem::new(title, field)
                .description(help)
                .layout(Axis::Vertical),
        }
    }

    /// A row whose control is its own widget: the row around it states the
    /// help sentence, because the widget names itself.
    fn wrapped(
        title: &'static str,
        help: &'static str,
        row_id: &'static str,
        control: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self::setting(title, help, move |window, cx| {
            div()
                .id(row_id)
                .test_support()
                .aria_description(help)
                .child(control(window, cx))
                .into_any_element()
        })
    }

    /// Whether the row's own words contain `query`, which is already lower case.
    fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || self.title.to_lowercase().contains(query)
            || self.help.to_lowercase().contains(query)
    }

    /// Whether the row carries no words of its own: a notice, a button, a line
    /// of copy. It follows the group it sits in and never keeps one.
    fn is_note(&self) -> bool {
        self.title.is_empty()
    }
}

/// A row that is not a setting: a notice, a button, or a line of copy.
fn note(build: impl Fn(&mut Window, &mut App) -> AnyElement + 'static) -> Row {
    Row {
        title: "",
        help: "",
        item: SettingItem::render(move |_, window, cx| build(window, cx)),
    }
}

/// One group of rows, kept only when a row of its own matches `query`.
fn group(title: Option<&'static str>, rows: Vec<Row>, query: &str) -> Option<SettingGroup> {
    let keep: Vec<bool> = rows.iter().map(|row| row.matches(query)).collect();
    if !keep.iter().any(|kept| *kept) {
        return None;
    }
    let mut group = SettingGroup::new();
    if let Some(title) = title {
        group = group.title(title);
    }
    Some(
        group.items(
            rows.into_iter()
                .zip(keep)
                .filter(|(row, kept)| *kept || row.is_note())
                .map(|(row, _)| row.item),
        ),
    )
}

/// One page, kept only when a group of it survives the search.
fn page(index: usize, groups: Vec<Option<SettingGroup>>) -> Option<SettingPage> {
    let groups: Vec<SettingGroup> = groups.into_iter().flatten().collect();
    if groups.is_empty() {
        return None;
    }
    let page = &PAGES[index];
    Some(
        SettingPage::new(page.title)
            .description(page.description)
            .groups(groups),
    )
}

/// Covers the resize handle the kit's settings group draws with a zero-width
/// sidebar.
///
/// `Settings` always builds its resizable group's handle, and 0.8.1 has no
/// option to leave the sidebar or its handle out. Drawn over the band, the
/// handle paints nothing, receives no pointer and never offers a resize cursor
/// for a drag that resizes nothing. The cover sits in the page's own left
/// padding, clear of every control.
fn handle_cover(background: Hsla) -> impl IntoElement {
    div()
        .id("settings-handle-cover")
        .test_support()
        .occlude()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .w(px(9.))
        .bg(background)
}

/// One read-only storage row (R9).
fn storage_row(
    id: &'static str,
    value_id: &'static str,
    title: &'static str,
    path: String,
    mono: SharedString,
    muted: Hsla,
) -> Row {
    const HELP: &str = "Resolved from the platform.";
    Row::stacked(title, HELP, move |_, _| {
        div()
            .id(id)
            .test_support()
            .aria_label(title)
            .aria_description(HELP)
            .min_w_0()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .child(
                div().flex_1().min_w_0().text_right().child(
                    // The text node itself must shrink, or the path draws past
                    // the row with no ellipsis.
                    div()
                        .id(value_id)
                        .test_support()
                        .aria_label(path.clone())
                        .min_w_0()
                        .truncate()
                        .font_family(mono.clone())
                        .text_xs()
                        .text_color(muted)
                        .child(path.clone()),
                ),
            )
            .into_any_element()
    })
}

/// A style that leaves an element out of the layout: the kit's own page sidebar
/// duplicates the strip and its search field duplicates the surface's.
fn hidden() -> StyleRefinement {
    StyleRefinement {
        display: Some(Display::None),
        ..StyleRefinement::default()
    }
}
/// The placeholder the secret field shows for one credential kind.
///
/// The field follows the radio, so the empty password field does not advertise
/// an API key while the password radio is selected (R1.1, R1.5).
fn secret_placeholder_for(method: AuthMethod) -> &'static str {
    match method {
        AuthMethod::Password => "Password",
        AuthMethod::ApiKey => "API key",
    }
}

/// The accent that marks one connection state on the settings notice.
fn connection_severity(state: &ConnectionState) -> Severity {
    match state {
        ConnectionState::Connected => Severity::Success,
        ConnectionState::NeedsAttention(_) => Severity::Error,
        ConnectionState::Connecting => Severity::Info,
        ConnectionState::NotConfigured => Severity::Info,
    }
}

/// One labelled field row: the name and help on the left, the control on the
/// right.
///
/// This and the card helpers below stay for the diagnostics and logs surfaces,
/// which still borrow them; #78 took the settings surface off them.
///
/// `field_text` survives #78 with two callers, `diagnostics_view` and
/// `logs_view`. #80 owns deleting it, and it owns the logs view's four uses
/// too: no ticket in this spec covers that surface's share of the helper.
pub(crate) fn field_text(label: &str, help: &str, palette: &CardPalette) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_0p5()
        .min_w_0()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(label.to_string()),
        )
        .child(
            div()
                .text_xs()
                .text_color(palette.meta)
                .child(help.to_string()),
        )
}

/// One card of related settings.
pub(crate) fn section(
    id: &'static str,
    title: &'static str,
    description: &'static str,
    rows: Vec<AnyElement>,
    cx: &App,
) -> AnyElement {
    card(id, title, description, rows, cx).into_any_element()
}

/// One card of related settings that takes the page height the cards above it
/// leave, for a card whose body is the page's scroll area (the Logs tail, #54).
///
/// The explicit floor is what a window too short for the card keeps. An explicit
/// floor replaces the card's automatic minimum, which is its body's whole
/// content height, so `floor_rem` must leave the body its own minimum on top of
/// the rows the card always shows. It is a parameter because the rows change
/// with the view's state: the Logs tail adds a paused row below the lines.
pub(crate) fn filling_section(
    id: &'static str,
    title: &'static str,
    description: &'static str,
    rows: Vec<AnyElement>,
    floor_rem: f32,
    cx: &App,
) -> AnyElement {
    card(id, title, description, rows, cx)
        .flex_1()
        .min_h(rems(floor_rem))
        .into_any_element()
}

fn card(
    id: &'static str,
    title: &'static str,
    description: &'static str,
    rows: Vec<AnyElement>,
    cx: &App,
) -> ObservedElement<Stateful<Div>> {
    let theme = cx.theme();
    let palette = CardPalette::new(theme);
    div()
        .id(id)
        .test_support()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .w_full()
        .rounded(theme.radius_lg)
        .bg(palette.surface)
        .text_color(palette.text)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::MEDIUM)
                        .child(title),
                )
                .child(div().text_xs().text_color(palette.meta).child(description)),
        )
        .children(rows)
}

/// The backfill progress line the Tagging section shows (R3.7).
///
/// `None` when there is nothing to say: the switch is off, or the library has
/// no asset to tag yet. The three states are the ones a user acts on: the run
/// working through the library, the run stopped with the batches that already
/// landed, and the run finished.
#[must_use]
pub fn backfill_text(status: &BackfillStatus, enabled: bool) -> Option<String> {
    if !enabled || status.total == 0 {
        return None;
    }
    let counts = format!(
        "{} of {}",
        group_digits(status.tagged),
        group_digits(status.total)
    );
    Some(if status.completed {
        format!("Existing assets tagged: {counts}")
    } else if status.running {
        format!("Tagging existing assets: {counts}")
    } else {
        format!("Tagging stopped: {counts}")
    })
}

/// `48320` as `48,320`, so a library the size of a photo collection reads at a
/// glance (R3.7).
fn group_digits(value: u32) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// The Immich web address of the sync tag's page (R3.7).
///
/// The configured value is the API base, so a trailing `/api` is dropped
/// before the web page's path is added; a reverse-proxy prefix in front of it
/// stays. Only an `http` or `https` base produces a URL, so the button cannot
/// open a `file:` or `mailto:` target from a hand-edited settings file.
#[must_use]
pub fn sync_tag_url(server_url: &str) -> Option<String> {
    Some(format!(
        "{}/tags?path={SYNC_TAG}",
        crate::asset_link::web_root(server_url)?
    ))
}

#[cfg(test)]
mod tests {
    use super::{backfill_text, group_digits, sync_tag_url};
    use sync_core::tags::BackfillStatus;

    /// R3.7: the Tagging section says where the backfill stands: working, at
    /// rest, or finished, with the counts grouped.
    #[test]
    fn the_backfill_line_reports_progress_and_completion() {
        let running = BackfillStatus {
            tagged: 12_340,
            total: 48_000,
            running: true,
            completed: false,
        };
        assert_eq!(
            backfill_text(&running, true).as_deref(),
            Some("Tagging existing assets: 12,340 of 48,000")
        );
        assert_eq!(
            backfill_text(&running, false),
            None,
            "R3.7: the switch off hides the progress it paused"
        );
        assert_eq!(
            backfill_text(
                &BackfillStatus {
                    tagged: 2,
                    total: 4,
                    running: false,
                    completed: false,
                },
                true,
            )
            .as_deref(),
            Some("Tagging stopped: 2 of 4"),
            "R3.7: a stopped run keeps and shows what it reached"
        );
        assert_eq!(
            backfill_text(
                &BackfillStatus {
                    tagged: 4,
                    total: 4,
                    running: false,
                    completed: true,
                },
                true,
            )
            .as_deref(),
            Some("Existing assets tagged: 4 of 4"),
            "R3.7: the finished state"
        );
        assert_eq!(
            backfill_text(&BackfillStatus::default(), true),
            None,
            "an empty library has nothing to report"
        );
    }

    #[test]
    fn the_count_is_grouped_in_thousands() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(48_320), "48,320");
        assert_eq!(group_digits(1_234_567), "1,234,567");
    }

    #[test]
    fn the_tag_url_drops_a_trailing_api_and_keeps_a_prefix() {
        // R3.7: the configured server URL is the API base; the tag page lives
        // on the web root beside it.
        assert_eq!(
            sync_tag_url("https://immich.example.com/api").as_deref(),
            Some("https://immich.example.com/tags?path=immich-sync")
        );
        assert_eq!(
            sync_tag_url("https://immich.example.com/immich/api/").as_deref(),
            Some("https://immich.example.com/immich/tags?path=immich-sync"),
            "a reverse-proxy path prefix stays"
        );
        assert_eq!(
            sync_tag_url("https://immich.example.com").as_deref(),
            Some("https://immich.example.com/tags?path=immich-sync")
        );
    }

    #[test]
    fn there_is_no_tag_url_without_a_web_url() {
        assert_eq!(sync_tag_url("  "), None, "no server URL means no link");
        // Only http(s) is ever opened, whatever a hand-edited file holds.
        assert_eq!(sync_tag_url("file:///tmp/immich"), None);
        assert_eq!(sync_tag_url("mailto:someone@example.com"), None);
        assert_eq!(sync_tag_url("https:///api"), None, "no host to open");
    }
}
