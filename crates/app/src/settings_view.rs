//! The settings surface (SPEC R7, R8.5, R9): General, Limits, Sync, and the
//! read-only Storage locations.
//!
//! Every control is keyboard reachable: `Switch` activates with Enter or Space,
//! `NumberInput` edits with the keyboard and steps with Up and Down, and both
//! draw the kit's focus ring. Each row also observes its control's focus handle
//! so a test can assert reachability.

use std::rc::Rc;

use crate::autostart::Autostart;
use crate::dashboard::CardPalette;
use crate::paths::AppPaths;
use crate::settings::{Settings, SettingsStore};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::{InputEvent, InputState, NumberInput};
use gpui_kit::component::switch::Switch;
use gpui_kit::*;

/// The number of bytes in a displayed megabyte. The stored limit stays bytes
/// per second (SPEC R7.1); the UI shows MB/s with one decimal place.
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
            Self::ScanInterval => "Scan interval",
            Self::AttemptCap => "Retry attempt cap",
        }
    }

    /// One line that says what the value does.
    #[must_use]
    pub fn help(self) -> &'static str {
        match self {
            Self::Bandwidth => "Zero means unlimited. Edit in 0.1 MB/s steps.",
            Self::Concurrency => "Uploads that run at the same time.",
            Self::ScanInterval => "How often a full scan runs, in minutes.",
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
/// One decimal place is the field's granularity: every value the field writes
/// is a multiple of 0.1 MB/s and displays exactly.
fn bytes_per_second(megabytes: f64) -> u64 {
    if megabytes.is_nan() {
        return 0;
    }
    let bytes = (megabytes.clamp(0.0, 10_000.0) * BYTES_PER_MEGABYTE as f64).round();
    // The clamp above keeps the rounded value inside `u64`.
    bytes as u64
}

/// The MB/s text for a stored bytes-per-second limit, with one decimal place
/// when the value has one.
fn megabytes_per_second_text(bytes: u64) -> String {
    const TENTHS_PER_MEGABYTE: u64 = BYTES_PER_MEGABYTE / 10;
    let tenths = bytes.saturating_add(TENTHS_PER_MEGABYTE / 2) / TENTHS_PER_MEGABYTE;
    if tenths % 10 == 0 {
        (tenths / 10).to_string()
    } else {
        format!("{}.{}", tenths / 10, tenths % 10)
    }
}

/// The notice prefix for a settings file that could not be written.
const SAVE_PROBLEM: &str = "Could not save the settings";
/// The notice prefix for an autostart change that could not be applied.
const AUTOSTART_PROBLEM: &str = "Could not update the autostart setting";

/// One numeric row: its key and the input state that edits it.
struct Field {
    key: FieldKey,
    input: Entity<InputState>,
}

/// The settings view.
pub struct SettingsView {
    store: SettingsStore,
    paths: AppPaths,
    autostart: Rc<dyn Autostart>,
    fields: Vec<Field>,
    /// The last failed save or autostart change, shown with the load problem.
    action_error: Option<String>,
    /// A failure before the store opened, such as path resolution. It stays
    /// visible until the process restarts, because a later save cannot fix it.
    environment_problem: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    /// Builds the view over an already-loaded store.
    pub fn new(
        store: SettingsStore,
        paths: AppPaths,
        autostart: Rc<dyn Autostart>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut fields = Vec::with_capacity(FieldKey::ALL.len());
        let mut subscriptions = Vec::with_capacity(FieldKey::ALL.len());
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

        let mut view = Self {
            store,
            paths,
            autostart,
            fields,
            action_error: None,
            environment_problem: None,
            _subscriptions: subscriptions,
        };
        view.reconcile_autostart(cx);
        view
    }

    /// Builds the view from the real platform directories and OS autostart.
    pub fn from_environment(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::from_resolved_paths(AppPaths::resolve(), window, cx)
    }

    /// Builds the view from an already-attempted path resolution.
    ///
    /// A resolution failure keeps the window usable on the fallback paths and
    /// shows the reason through the same notice as a corrupt settings file.
    pub fn from_resolved_paths(
        resolved: anyhow::Result<AppPaths>,
        window: &mut Window,
        cx: &mut Context<Self>,
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
            eprintln!("immich-sync: could not prepare autostart: {error:#}");
            Rc::new(crate::autostart::UnavailableAutostart::new(format!(
                "{error:#}"
            )))
        });
        let mut view = Self::new(store, paths, autostart, window, cx);
        view.environment_problem = environment_problem;
        view
    }

    /// Brings the stored autostart flag in line with the real OS mechanism.
    ///
    /// The Run key or `.desktop` entry can change outside the client. The
    /// switch must show what the OS does, or turning it off would be a no-op
    /// against a file that already said off.
    fn reconcile_autostart(&mut self, cx: &mut Context<Self>) {
        let stored = self.store.settings().autostart;
        let result = match self.autostart.is_enabled() {
            Ok(actual) if actual != stored => self
                .store
                .update(|settings| settings.autostart = actual)
                .map_err(|error| error.context("record the OS autostart state")),
            Ok(_) => Ok(()),
            Err(error) => Err(error.context("read the OS autostart state")),
        };
        self.record(AUTOSTART_PROBLEM, result, cx);
    }

    /// Records one settings change and saves it.
    fn write_field(&mut self, key: FieldKey, value: f64, cx: &mut Context<Self>) {
        let result = self.store.update(|settings| key.set(settings, value));
        self.record(SAVE_PROBLEM, result, cx);
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
        let result = self.store.update(|settings| settings.autostart = enabled);
        let result = match result {
            Ok(()) => Ok(()),
            Err(error) => match self.autostart.set_enabled(!enabled) {
                Ok(()) => Err(error),
                Err(rollback) => Err(error.context(format!(
                    "and rolling the OS autostart change back failed: {rollback:#}"
                ))),
            },
        };
        self.record(AUTOSTART_PROBLEM, result, cx);
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

    /// The General section: autostart.
    fn general(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let palette = CardPalette::new(cx.theme());
        let enabled = self.store.settings().autostart;
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
        let row = |id: &'static str, label: &'static str, path: &std::path::Path| {
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
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_right()
                        .truncate()
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .text_color(palette.meta)
                        .child(path.display().to_string()),
                )
        };
        section(
            "settings-storage",
            "Storage",
            "Read-only. The database stays on this computer; it never lives on a network share or in a cloud-sync folder.",
            vec![
                row(
                    "settings-path-config",
                    "Settings file",
                    &self.paths.config_file,
                )
                .into_any_element(),
                row(
                    "settings-path-state",
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

/// One labelled field row: the name and help on the left, the control on the
/// right.
fn field_text(label: &str, help: &str, palette: &CardPalette) -> impl IntoElement {
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
fn section(
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
fn notice(id: &'static str, text: String, accent: Hsla, palette: &CardPalette) -> impl IntoElement {
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
