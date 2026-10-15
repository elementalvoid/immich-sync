//! The settings surface (SPEC R7, R8.5, R9): General, Limits, Sync, and the
//! read-only Storage locations.
//!
//! Every control is keyboard reachable: `Switch` activates with Enter or Space,
//! `NumberInput` edits with the keyboard and steps with Up and Down, and both
//! draw the kit's focus ring. Each row also observes its control's focus handle
//! so a test can assert reachability.

use std::rc::Rc;

use crate::autostart::Autostart;
use crate::connection::{ConnectionModel, apply_profile, profile_from_settings};
use crate::dashboard::CardPalette;
use crate::engine::EngineModel;
use crate::logging::LogLevel;
use crate::paths::AppPaths;
use crate::settings::{Settings, SettingsStore};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Theme;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, NumberInput};
use gpui_kit::component::radio::Radio;
use gpui_kit::component::switch::Switch;
use gpui_kit::*;
use sync_core::connection::{AuthMethod, ConnectionState, ServerProfile, SignIn};
use sync_core::credential::Secret;

/// The number of bytes in a displayed megabyte. The stored limit stays bytes
/// per second (SPEC R7.1); the UI shows MB/s with up to six decimals, which is
/// one byte.
pub const BYTES_PER_MEGABYTE: u64 = 1_000_000;

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
    server_url_input: Entity<InputState>,
    email_input: Entity<InputState>,
    secret_input: Entity<InputState>,
    /// The credential kind the form signs in with (R1.1, R1.5).
    auth_method: AuthMethod,
    fields: Vec<Field>,
    /// The engine the Limits card pushes a change to (R7.1, R7.2), once the
    /// shell has installed one. A view with no engine still saves the values.
    engine: Option<Entity<EngineModel>>,
    /// The last failed save or autostart change, shown with the load problem.
    action_error: Option<String>,
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
        let mut subscriptions = Vec::with_capacity(FieldKey::ALL.len() + 1);
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
            server_url_input,
            email_input,
            secret_input,
            auth_method: initial.auth_method,
            fields,
            engine: None,
            action_error: None,
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
    pub fn set_engine(&mut self, engine: Entity<EngineModel>) {
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

    /// Applies the autostart setting to the OS, then records it.
    ///
    /// The OS and the settings file are two stores. A failed file write rolls
    /// the OS change back, so the switch never claims a state one store does
    /// not hold.
    fn toggle_autostart(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if let Err(error) = self.autostart.set_enabled(enabled) {
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
                        match view.autostart.set_enabled(stored) {
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

    /// The page header.
    fn header(cx: &App) -> impl IntoElement + use<> {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_col()
            .gap_0p5()
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
            )
    }

    /// The problem notices, when the file failed to load or an action failed.
    fn notices(&self, cx: &App) -> Vec<AnyElement> {
        let theme = cx.theme();
        let palette = CardPalette::new(theme);
        let mut notices = Vec::new();
        if let Some(problem) = self.store.problem() {
            notices.push(
                notice(
                    "settings-problem",
                    problem.to_string(),
                    theme.warning,
                    &palette,
                )
                .into_any_element(),
            );
        } else if let Some(problem) = &self.environment_problem {
            notices.push(
                notice("settings-problem", problem.clone(), theme.warning, &palette)
                    .into_any_element(),
            );
        }
        if let Some(error) = &self.action_error {
            notices.push(
                notice(
                    "settings-action-error",
                    error.clone(),
                    theme.danger,
                    &palette,
                )
                .into_any_element(),
            );
        }
        notices
    }

    /// The Connection section: server, account, credential, and sign-in
    /// (R1.1 to R1.6).
    fn connection(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = CardPalette::new(cx.theme());
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
        // The accents are read before the row builders borrow `cx` mutably.
        let (state_accent, hint_accent, error_accent) = {
            let theme = cx.theme();
            (connection_accent(&state, theme), theme.info, theme.danger)
        };

        let mut rows = vec![
            self.text_row(
                "settings-server-url",
                "Server URL",
                "For example https://immich.example.com. Certificate verification stays on.",
                &self.server_url_input,
                cx,
            ),
            self.text_row(
                "settings-email",
                "Email address",
                "The Immich account this client uploads as.",
                &self.email_input,
                cx,
            ),
            self.method_row(cx),
            self.secret_row(cx),
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
                        .on_click(cx.listener(|this, _, window, cx| this.sign_in(window, cx))),
                )
                .into_any_element(),
        ];

        rows.push(
            notice(
                "settings-connection-state",
                state.message(),
                state_accent,
                &palette,
            )
            .into_any_element(),
        );
        if needs_api_key {
            rows.push(
                notice(
                    "settings-connection-hint",
                    "Switch the credential to API key and paste a key from Immich (Account \
                     settings, then API keys)."
                        .to_string(),
                    hint_accent,
                    &palette,
                )
                .into_any_element(),
            );
        }
        if let Some(error) = form_error {
            rows.push(
                notice("settings-connection-error", error, error_accent, &palette)
                    .into_any_element(),
            );
        }

        section(
            "settings-connection",
            "Connection",
            "The server and the account this client uploads to.",
            rows,
            cx,
        )
    }

    /// One connection text input row.
    fn text_row(
        &self,
        id: &'static str,
        label: &'static str,
        help: &'static str,
        input: &Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = CardPalette::new(cx.theme());
        let focus = input.read(cx).focus_handle(cx);
        div()
            .id(id)
            .test_support()
            .track_focus(&focus)
            .aria_label(label)
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_6()
            .min_h(rems(2.25))
            .child(field_text(label, help, &palette))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(input).aria_label(label)),
            )
            .into_any_element()
    }

    /// The credential kind: password or API key (R1.1, R1.5).
    fn method_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = CardPalette::new(cx.theme());
        div()
            .id("settings-auth-method")
            .test_support()
            .aria_label("Credential")
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_6()
            .min_h(rems(2.25))
            .child(field_text(
                "Credential",
                "A password for the account, or an API key from Immich.",
                &palette,
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .child(
                        Radio::new("settings-auth-password")
                            .label("Password")
                            .accessibility_label("Sign in with a password")
                            .checked(self.auth_method == AuthMethod::Password)
                            .on_change(cx.listener(|this, checked, window, cx| {
                                if *checked {
                                    this.set_auth_method(AuthMethod::Password, window, cx);
                                }
                            })),
                    )
                    .child(
                        Radio::new("settings-auth-apikey")
                            .label("API key")
                            .accessibility_label("Sign in with an API key")
                            .checked(self.auth_method == AuthMethod::ApiKey)
                            .on_change(cx.listener(|this, checked, window, cx| {
                                if *checked {
                                    this.set_auth_method(AuthMethod::ApiKey, window, cx);
                                }
                            })),
                    ),
            )
            .into_any_element()
    }

    /// The password or API key input. The value is masked and never saved to
    /// the settings file (R1.2).
    fn secret_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = CardPalette::new(cx.theme());
        let label = match self.auth_method {
            AuthMethod::Password => "Password",
            AuthMethod::ApiKey => "API key",
        };
        let focus = self.secret_input.read(cx).focus_handle(cx);
        div()
            .id("settings-secret")
            .test_support()
            .track_focus(&focus)
            .aria_label(label)
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_6()
            .min_h(rems(2.25))
            .child(field_text(
                label,
                "Stored in the OS credential store, never in the settings file.",
                &palette,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.secret_input).aria_label(label)),
            )
            .into_any_element()
    }

    /// The General section: autostart.
    fn general(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = CardPalette::new(cx.theme());
        // The switch shows the user's newest intent, so a queued or in-flight
        // write is reflected before it lands (N2).
        let enabled = self.intended_settings().autostart;
        section(
            "settings-general",
            "General",
            "How the client starts.",
            vec![
                div()
                    .id("settings-autostart-row")
                    .test_support()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_6()
                    .min_h(rems(2.25))
                    .child(field_text(
                        "Autostart at login",
                        "Start Immich Sync when you log in. Off by default.",
                        &palette,
                    ))
                    .child(
                        Switch::new("settings-autostart")
                            .checked(enabled)
                            .accessibility_label("Start Immich Sync at login")
                            .on_change(cx.listener(|this, enabled, _, cx| {
                                this.toggle_autostart(*enabled, cx);
                            })),
                    )
                    .into_any_element(),
            ],
            cx,
        )
    }

    /// The Limits section: bandwidth and concurrency (SPEC R7).
    fn limits(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let rows = self.numeric_rows(&[FieldKey::Bandwidth, FieldKey::Concurrency], cx);
        section(
            "settings-limits",
            "Limits",
            "How much the client uses while it syncs.",
            rows,
            cx,
        )
    }

    /// The Sync section: scan interval and attempt cap (SPEC R2.3, R6.1).
    fn sync(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let rows = self.numeric_rows(&[FieldKey::ScanInterval, FieldKey::AttemptCap], cx);
        section(
            "settings-sync",
            "Sync",
            "How the client scans and retries.",
            rows,
            cx,
        )
    }

    /// The Storage section: the resolved locations, read-only (SPEC R9).
    fn storage(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = CardPalette::new(cx.theme());
        let theme = cx.theme();
        let row = |id: &'static str,
                   value_id: &'static str,
                   label: &'static str,
                   path: &std::path::Path| {
            div()
                .id(id)
                .test_support()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_6()
                .min_h(rems(2.25))
                .child(field_text(label, "Resolved from the platform.", &palette))
                .child(
                    div().flex_1().min_w_0().text_right().child(
                        // The text node itself must shrink, or the path draws
                        // past the card with no ellipsis.
                        div()
                            .id(value_id)
                            .test_support()
                            .min_w_0()
                            .truncate()
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .text_color(palette.meta)
                            .child(path.display().to_string()),
                    ),
                )
        };
        section(
            "settings-storage",
            "Storage",
            "Read-only. The database stays on this computer; it never lives on a network share or in a cloud-sync folder.",
            vec![
                row(
                    "settings-path-config",
                    "settings-path-config-value",
                    "Settings file",
                    &self.paths.config_file,
                )
                .into_any_element(),
                row(
                    "settings-path-state",
                    "settings-path-state-value",
                    "State database",
                    &self.paths.state_db,
                )
                .into_any_element(),
            ],
            cx,
        )
    }

    /// One row per key, with its input and focus observation.
    fn numeric_rows(&self, keys: &[FieldKey], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let palette = CardPalette::new(cx.theme());
        let muted = cx.theme().muted_foreground;
        keys.iter()
            .filter_map(|key| {
                let field = self.fields.iter().find(|field| field.key == *key)?;
                let focus = field.input.read(cx).focus_handle(cx);
                Some(
                    div()
                        .id(key.id())
                        .test_support()
                        .track_focus(&focus)
                        .aria_label(key.label())
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap_6()
                        .min_h(rems(2.25))
                        .child(field_text(key.label(), key.help(), &palette))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_3()
                                .child(
                                    // The component's root is `flex_1`, so the
                                    // fixed width belongs on a non-growing
                                    // wrapper.
                                    div()
                                        .w_32()
                                        .flex_none()
                                        .child(NumberInput::new(&field.input)),
                                )
                                .child(
                                    div()
                                        .w_16()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(key.unit()),
                                ),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let palette = CardPalette::new(theme);
        let background = theme.background;
        let foreground = theme.foreground;
        let notices = self.notices(cx);
        let connection = self.connection(cx);
        let general = self.general(cx);
        let limits = self.limits(cx);
        let sync = self.sync(cx);
        let storage = self.storage(cx);

        div()
            .id("settings-scroll")
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .overflow_y_scroll()
            .p_4()
            .gap_2()
            .child(Self::header(cx))
            .children(notices)
            .child(connection)
            .child(general)
            .child(limits)
            .child(sync)
            .child(storage)
            .child(
                div()
                    .text_xs()
                    .text_color(palette.meta)
                    .child("Every control is reachable with the Tab key."),
            )
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
fn connection_accent(state: &ConnectionState, theme: &Theme) -> Hsla {
    match state {
        ConnectionState::Connected => theme.success,
        ConnectionState::NeedsAttention(_) => theme.danger,
        ConnectionState::Connecting => theme.info,
        ConnectionState::NotConfigured => theme.muted_foreground,
    }
}

/// One labelled field row: the name and help on the left, the control on the
/// right.
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
        .into_any_element()
}

/// A notice with a colored edge. The accessible name carries the message, so
/// the visible copy is not announced a second time.
pub(crate) fn notice(
    id: &'static str,
    text: String,
    accent: Hsla,
    palette: &CardPalette,
) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .role(Role::Alert)
        .aria_label(text.clone())
        .border_l_2()
        .border_color(accent)
        .pl_2()
        .py_1()
        .text_xs()
        .text_color(palette.text)
        .child(Text::new_inaccessible(text.into()))
}
