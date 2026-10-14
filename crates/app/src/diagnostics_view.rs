//! The Diagnostics view (SPEC R10.2, R10.4): the server, the sqlite state, the
//! last error per class, the throughput, and the rebuild action.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::Button;
use gpui_kit::*;

use crate::dashboard::CardPalette;
use crate::diagnostics::{DiagnosticsModel, Permissions, format_bytes, format_rate};
use crate::settings_view::{field_text, notice, section};
use sync_core::store::SqliteInfo;

/// The Diagnostics view.
pub struct DiagnosticsView {
    /// The model owns every read; the view renders what it last produced.
    model: Entity<DiagnosticsModel>,
}

impl DiagnosticsView {
    /// A view over the diagnostics model.
    pub fn new(model: Entity<DiagnosticsModel>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self { model }
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
                    .child("Diagnostics"),
            )
            .child(div().text_sm().text_color(muted).child(
                "What the client knows about the server and its own state. Refresh re-reads both.",
            ))
    }

    /// One labelled row: the name and help on the left, the value on the right.
    fn value_row(
        label: &'static str,
        help: &'static str,
        value_id: &'static str,
        value: String,
        palette: &CardPalette,
    ) -> AnyElement {
        div()
            .id(ElementId::Name(format!("{value_id}-row").into()))
            .test_support()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_6()
            .min_h(rems(2.25))
            .child(field_text(label, help, palette))
            .child(
                div()
                    .id(value_id)
                    .test_support()
                    .aria_label(value.clone())
                    .min_w_0()
                    .max_w(rems(36.))
                    .text_right()
                    .text_xs()
                    .text_color(palette.meta)
                    .child(value),
            )
            .into_any_element()
    }

    /// The server version, the granted permissions, and the refresh action.
    fn server_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = CardPalette::new(cx.theme());
        let server = self.model.read(cx).server().clone();
        let version = server
            .version
            .clone()
            .unwrap_or_else(|| "Not read yet".to_string());
        let permissions = match &server.permissions {
            Permissions::NotSignedIn => "Not signed in".to_string(),
            Permissions::Account => {
                "Signed in with the account; its role decides the permissions, not a key's list."
                    .to_string()
            }
            Permissions::Granted { name, permissions } if permissions.is_empty() => {
                format!("{name}: no permissions granted")
            }
            Permissions::Granted { name, permissions } => {
                format!("{name}: {}", permissions.join(", "))
            }
            Permissions::Unavailable(reason) => reason.clone(),
        };
        let mut rows = Vec::with_capacity(4);
        if let Some(problem) = &server.problem {
            rows.push(
                notice(
                    "diagnostics-server-problem",
                    problem.clone(),
                    cx.theme().danger,
                    &palette,
                )
                .into_any_element(),
            );
        }
        rows.push(Self::value_row(
            "Server version",
            "GET /server/about with this credential.",
            "diagnostics-server-version-value",
            version,
            &palette,
        ));
        rows.push(Self::value_row(
            "Permissions",
            "An API key's granted permissions, read from GET /api-keys/me.",
            "diagnostics-permissions-value",
            permissions,
            &palette,
        ));
        rows.push(
            div()
                .id("diagnostics-refresh-row")
                .test_support()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_6()
                .min_h(rems(2.25))
                .child(field_text(
                    "Refresh",
                    "Re-reads the state database and re-checks the server.",
                    &palette,
                ))
                .child(
                    Button::new("diagnostics-refresh")
                        .label("Refresh")
                        .accessibility_label("Re-check the server and the state database")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model.update(cx, |model, cx| model.refresh(cx));
                        })),
                )
                .into_any_element(),
        );
        section(
            "diagnostics-server",
            "Server",
            "The version and permissions this client actually holds.",
            rows,
            cx,
        )
    }

    /// The database path, the migration version, the size, the table counts,
    /// the integrity check, and the rebuild action.
    fn state_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = CardPalette::new(cx.theme());
        let model = self.model.read(cx);
        let path = model.db_path(cx).display().to_string();
        let info = model.info().cloned();
        let rebuilding = model.is_rebuilding();
        let problem = model.problem().map(str::to_string);

        let mut rows = Vec::with_capacity(12);
        if let Some(problem) = &problem {
            rows.push(
                notice(
                    "diagnostics-state-problem",
                    problem.clone(),
                    cx.theme().danger,
                    &palette,
                )
                .into_any_element(),
            );
        }
        if rebuilding {
            rows.push(
                notice(
                    "diagnostics-rebuilding",
                    "Rebuilding the state database and rescanning the folders…".to_string(),
                    cx.theme().info,
                    &palette,
                )
                .into_any_element(),
            );
        }
        rows.push(Self::value_row(
            "Database",
            "The state database the client writes.",
            "diagnostics-state-path-value",
            path,
            &palette,
        ));
        match &info {
            Some(info) => {
                rows.push(Self::value_row(
                    "Migration version",
                    "PRAGMA user_version: the schema the client migrated to.",
                    "diagnostics-sqlite-version-value",
                    info.migration_version.to_string(),
                    &palette,
                ));
                rows.push(Self::value_row(
                    "Size",
                    "The pages SQLite holds for this database.",
                    "diagnostics-sqlite-size-value",
                    format_bytes(info.size_bytes),
                    &palette,
                ));
                rows.push(Self::value_row(
                    "Integrity",
                    "PRAGMA quick_check.",
                    "diagnostics-integrity-value",
                    info.integrity.clone(),
                    &palette,
                ));
                rows.extend(table_rows(info, &palette));
            }
            None => rows.push(
                div()
                    .id("diagnostics-state-empty")
                    .test_support()
                    .text_xs()
                    .text_color(palette.meta)
                    .child("The state database is opening.")
                    .into_any_element(),
            ),
        }
        rows.push(
            div()
                .id("diagnostics-rebuild-row")
                .test_support()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_6()
                .min_h(rems(2.25))
                .child(field_text(
                    "Rebuild",
                    "Starts the state over and rescans every folder. The server is re-checked.",
                    &palette,
                ))
                .child(
                    Button::new("diagnostics-rebuild")
                        .label("Rebuild")
                        .accessibility_label("Rebuild the state database and rescan the folders")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model.update(cx, |model, cx| model.rebuild(cx));
                        })),
                )
                .into_any_element(),
        );
        section(
            "diagnostics-state",
            "State database",
            "The local state the client rebuilds from, and its integrity.",
            rows,
            cx,
        )
    }

    /// The last failure of each class the attempt log holds.
    fn errors_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = CardPalette::new(cx.theme());
        let errors = self.model.read(cx).errors().to_vec();
        let rows: Vec<AnyElement> = if errors.is_empty() {
            vec![
                div()
                    .id("diagnostics-errors-empty")
                    .test_support()
                    .aria_label("No failure has been recorded.")
                    .text_xs()
                    .text_color(palette.meta)
                    .child("No failure has been recorded.")
                    .into_any_element(),
            ]
        } else {
            errors
                .iter()
                .map(|error| {
                    let text = match &error.message {
                        Some(message) => format!("{}: {message}", error.class.as_str()),
                        None => error.class.as_str().to_string(),
                    };
                    div()
                        .id(ElementId::Name(
                            format!("diagnostics-error-{}", error.class.as_str()).into(),
                        ))
                        .test_support()
                        .aria_label(text.clone())
                        .min_w_0()
                        .w_full()
                        .text_xs()
                        .text_color(palette.text)
                        .child(text)
                        .into_any_element()
                })
                .collect()
        };
        section(
            "diagnostics-errors",
            "Last error per class",
            "The newest attempt the client recorded of each failure class.",
            rows,
            cx,
        )
    }

    /// The upload rate from the engine's meter, and the live progress.
    fn throughput_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = CardPalette::new(cx.theme());
        let model = self.model.read(cx);
        let rate = format_rate(model.throughput_bytes_per_second());
        let status = model.status().clone();
        let counts = format!(
            "{} uploaded, {} pending, {} failed",
            status.counts.uploaded, status.counts.pending, status.counts.failed
        );
        section(
            "diagnostics-throughput",
            "Throughput",
            "Upload bytes per second, from the same meter the bandwidth limit uses.",
            vec![
                Self::value_row(
                    "Upload rate",
                    "Measured between the last two status polls.",
                    "diagnostics-throughput-value",
                    rate,
                    &palette,
                ),
                Self::value_row(
                    "Activity",
                    "What the engine is doing right now.",
                    "diagnostics-activity-value",
                    status.activity.label().to_string(),
                    &palette,
                ),
                Self::value_row(
                    "Items",
                    "The counts across every folder.",
                    "diagnostics-counts-value",
                    counts,
                    &palette,
                ),
            ],
            cx,
        )
    }
}

/// One row per tracked table, with its row count.
fn table_rows(info: &SqliteInfo, palette: &CardPalette) -> Vec<AnyElement> {
    info.tables
        .iter()
        .map(|count| {
            let id = ElementId::Name(format!("diagnostics-table-{}", count.table).into());
            div()
                .id(id)
                .test_support()
                .aria_label(format!("{}: {}", count.table, count.rows))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_6()
                .min_h(rems(1.75))
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.text)
                        .child(count.table.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(palette.meta)
                        .child(count.rows.to_string()),
                )
                .into_any_element()
        })
        .collect()
}

impl Render for DiagnosticsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let background = theme.background;
        let foreground = theme.foreground;
        let server = self.server_section(cx);
        let state = self.state_section(cx);
        let errors = self.errors_section(cx);
        let throughput = self.throughput_section(cx);

        div()
            .id("diagnostics-view")
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
            .child(server)
            .child(state)
            .child(throughput)
            .child(errors)
    }
}
