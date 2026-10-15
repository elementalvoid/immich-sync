//! The Logs view (SPEC R10.3): where the log lives, the recent lines, the
//! refresh action, and the persisted level control.
//!
//! The level is stored by the settings surface's store, because one file must
//! have one writer. The view renders the tail once when it is built and on
//! demand, so reading the file never happens on every frame.

use std::path::PathBuf;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::radio::Radio;
use gpui_kit::*;

use crate::dashboard::CardPalette;
use crate::logging::{self, LogLevel, TAIL_LINES};
use crate::settings_view::{SettingsView, field_text, notice, section};

/// The Logs view.
pub struct LogsView {
    /// The settings surface owns the settings file and the level it persists.
    settings: Entity<SettingsView>,
    /// The directory the rotating log writes to (the state directory).
    directory: PathBuf,
    /// The bounded tail read when the view was built or last refreshed.
    lines: Vec<String>,
    /// The newest log file's name, read with the tail.
    current_file: Option<String>,
    /// Why the tail could not be read, when it could not.
    read_problem: Option<String>,
    /// Why the level could not be saved, when it could not.
    save_problem: Option<String>,
    /// Keeps the level notice in step with the settings surface's save
    /// failures, which now land after the worker write (N2).
    _subscriptions: Vec<Subscription>,
}

impl LogsView {
    /// A view over the log directory the settings paths resolved.
    pub fn new(settings: Entity<SettingsView>, cx: &mut Context<Self>) -> Self {
        let directory = settings.read(cx).paths().log_directory();
        let save_problem = settings.read(cx).save_problem().map(str::to_string);
        let subscription = cx.observe(&settings, |this, settings, cx| {
            this.save_problem = settings.read(cx).save_problem().map(str::to_string);
            cx.notify();
        });
        let mut view = Self {
            settings,
            directory,
            lines: Vec::new(),
            current_file: None,
            read_problem: None,
            save_problem,
            _subscriptions: vec![subscription],
        };
        view.refresh(cx);
        view
    }

    /// Re-reads the newest log file's name and the tail of its lines.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.current_file = logging::newest_file(&self.directory)
            .ok()
            .flatten()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            });
        match logging::tail(&self.directory, TAIL_LINES) {
            Ok(lines) => {
                self.lines = lines;
                self.read_problem = None;
            }
            Err(error) => {
                self.lines.clear();
                self.read_problem = Some(format!("Could not read the log file: {error:#}"));
            }
        }
        cx.notify();
    }

    /// The level the settings hold, which is also the level the subscriber runs
    /// with.
    #[must_use]
    pub fn level(&self, cx: &App) -> LogLevel {
        self.settings.read(cx).log_level()
    }

    /// Persists the level and applies it to the running subscriber. The
    /// settings surface reports the outcome through its own subscription.
    fn set_level(&mut self, level: LogLevel, cx: &mut Context<Self>) {
        self.settings
            .update(cx, |settings, cx| settings.set_log_level(level, cx));
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
                    .child("Logs"),
            )
            .child(div().text_sm().text_color(muted).child(
                "The client writes a rotating log file here. Passwords and API keys never reach it.",
            ))
    }

    /// The log location and the refresh action.
    fn location(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = CardPalette::new(cx.theme());
        let theme = cx.theme();
        let current = self
            .current_file
            .clone()
            .unwrap_or_else(|| "No log file yet.".to_string());
        let path_row = |id: &'static str,
                        value_id: &'static str,
                        label: &'static str,
                        help: &'static str,
                        value: String| {
            div()
                .id(id)
                .test_support()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_6()
                .min_h(rems(2.25))
                .child(field_text(label, help, &palette))
                .child(
                    div().flex_1().min_w_0().text_right().child(
                        // Wrapped, not truncated: the whole path stays visible.
                        div()
                            .id(value_id)
                            .test_support()
                            .aria_label(value.clone())
                            .min_w_0()
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .text_color(palette.meta)
                            .child(value),
                    ),
                )
        };
        section(
            "logs-location",
            "Log file",
            "Same directory as the state database. The file rotates and old files are pruned.",
            vec![
                path_row(
                    "logs-path-directory",
                    "logs-path-directory-value",
                    "Log folder",
                    "Resolved from the platform data directory.",
                    self.directory.display().to_string(),
                )
                .into_any_element(),
                path_row(
                    "logs-path-current",
                    "logs-path-current-value",
                    "Current file",
                    "Reads from the newest file. Refresh to re-read it.",
                    current,
                )
                .into_any_element(),
                div()
                    .id("logs-refresh-row")
                    .test_support()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_6()
                    .min_h(rems(2.25))
                    .child(field_text(
                        "Refresh",
                        "Re-reads the tail of the newest log file.",
                        &palette,
                    ))
                    .child(
                        Button::new("logs-refresh")
                            .label("Refresh")
                            .accessibility_label("Refresh the log lines")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .into_any_element(),
            ],
            cx,
        )
    }

    /// The level control. The change applies immediately and persists.
    fn level_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.level(cx);
        let mut rows = Vec::with_capacity(LogLevel::ALL.len() + 1);
        if let Some(problem) = &self.save_problem {
            let palette = CardPalette::new(cx.theme());
            rows.push(
                notice(
                    "logs-level-problem",
                    problem.clone(),
                    cx.theme().danger,
                    &palette,
                )
                .into_any_element(),
            );
        }
        for level in LogLevel::ALL {
            let id = level.id();
            rows.push(
                div()
                    .id(ElementId::Name(format!("{}-row", level.id()).into()))
                    .test_support()
                    .flex()
                    .flex_row()
                    .items_center()
                    .min_h(rems(2.25))
                    .child(
                        Radio::new(id)
                            .label(level.label())
                            .accessibility_label(format!("Log level: {}", level.label()))
                            .checked(level == current)
                            .on_change(cx.listener(move |this, checked, _, cx| {
                                if *checked {
                                    this.set_level(level, cx);
                                }
                            })),
                    )
                    .into_any_element(),
            );
        }
        section(
            "logs-level",
            "Log level",
            "How much detail the file holds. The change applies immediately and is saved.",
            rows,
            cx,
        )
    }

    /// The bounded tail of the newest log file.
    fn lines_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let palette = CardPalette::new(theme);
        let mut body: Vec<AnyElement> = Vec::with_capacity(2);
        if let Some(problem) = &self.read_problem {
            body.push(
                notice("logs-problem", problem.clone(), theme.danger, &palette).into_any_element(),
            );
        }
        if self.lines.is_empty() {
            body.push(
                div()
                    .id("logs-empty")
                    .test_support()
                    .text_xs()
                    .text_color(palette.meta)
                    .child("No log lines yet.")
                    .into_any_element(),
            );
        } else {
            let lines: Vec<AnyElement> = self
                .lines
                .iter()
                .enumerate()
                .map(|(index, line)| {
                    div()
                        .id(ElementId::Name(format!("logs-line-{index}").into()))
                        .test_support()
                        .aria_label(line.clone())
                        .min_w_0()
                        .w_full()
                        .truncate()
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .text_color(palette.text)
                        .child(Text::new_inaccessible(line.clone().into()))
                        .into_any_element()
                })
                .collect();
            body.push(
                div()
                    .id("logs-lines")
                    .test_support()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .max_h(rems(24.))
                    .overflow_y_scroll()
                    .children(lines)
                    .into_any_element(),
            );
        }
        section(
            "logs-lines-section",
            "Recent lines",
            "The newest file's last lines. A new session appends to the same file.",
            body,
            cx,
        )
    }
}

impl Render for LogsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let background = theme.background;
        let foreground = theme.foreground;
        let location = self.location(cx);
        let level = self.level_section(cx);
        let lines = self.lines_section(cx);

        div()
            .id("logs-view")
            .test_support()
            .flex()
            .flex_col()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .overflow_y_scroll()
            .p_4()
            .gap_2()
            .child(Self::header(cx))
            .child(location)
            .child(level)
            .child(lines)
    }
}
