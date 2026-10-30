//! The Diagnostics view (SPEC R10.2, R10.4): the client and server versions,
//! the sqlite state, the last error per class, the throughput, and the rebuild
//! action.

use gpui_kit::base::Disableable as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::description_list::{DescriptionItem, DescriptionList};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::dashboard::CardPalette;
use crate::debug_bundle::{LOG_BYTES, LOG_FILES};
use crate::diagnostics::{BundleState, DiagnosticsModel, Permissions, format_bytes, format_rate};
use crate::notice::{Notice, Severity, copy_confirmation};
use crate::settings_view::{field_text, section};
use crate::version;
use sync_core::store::SqliteInfo;

/// The Diagnostics view.
pub struct DiagnosticsView {
    /// The model owns every read; the view renders what it last produced.
    model: Entity<DiagnosticsModel>,
    /// The archive path the last copy put on the clipboard, and how many presses
    /// it has had since. Both are dropped once that path is no longer the last
    /// archive, so the confirmation never describes a stale copy (#82).
    copied: Option<(String, u32)>,
}

impl DiagnosticsView {
    /// A view over the diagnostics model.
    pub fn new(model: Entity<DiagnosticsModel>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |this, model, cx| {
            // The confirmation names the path it copied, so it goes when the last
            // archive is no longer that path (#82).
            let current = match model.read(cx).bundle() {
                BundleState::Ready(report) => Some(report.path.display().to_string()),
                _ => None,
            };
            let still_copied = this
                .copied
                .as_ref()
                .is_some_and(|(copied, _)| current.as_deref() == Some(copied.as_str()));
            if !still_copied {
                this.copied = None;
            }
            cx.notify();
        })
        .detach();
        Self {
            model,
            copied: None,
        }
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
        let items = vec![
            value_item(
                "Client version",
                Some("The running binary: the package version and the short build revision."),
                "diagnostics-client-version-value",
                version::client_version().to_string(),
                None,
                &palette,
            ),
            value_item(
                "Server version",
                Some("GET /server/about with this credential."),
                "diagnostics-server-version-value",
                version,
                None,
                &palette,
            ),
            value_item(
                "Permissions",
                Some("An API key's granted permissions, read from GET /api-keys/me."),
                "diagnostics-permissions-value",
                permissions,
                None,
                &palette,
            ),
        ];
        let mut rows = Vec::with_capacity(3);
        if let Some(problem) = &server.problem {
            rows.push(
                Notice::new(
                    "diagnostics-server-problem",
                    Severity::Error,
                    problem.clone(),
                )
                .into_any_element(),
            );
        }
        rows.push(value_list(items).into_any_element());
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
                Notice::new(
                    "diagnostics-state-problem",
                    Severity::Error,
                    problem.clone(),
                )
                .into_any_element(),
            );
        }
        if rebuilding {
            rows.push(
                Notice::new(
                    "diagnostics-rebuilding",
                    Severity::Info,
                    "Rebuilding the state database and rescanning the folders…",
                )
                .into_any_element(),
            );
        }
        let mut items = Vec::with_capacity(12);
        items.push(value_item(
            "Database",
            Some("The state database the client writes."),
            "diagnostics-state-path-value",
            path,
            None,
            &palette,
        ));
        if let Some(info) = &info {
            items.push(value_item(
                "Migration version",
                Some("PRAGMA user_version: the schema the client migrated to."),
                "diagnostics-sqlite-version-value",
                info.migration_version.to_string(),
                None,
                &palette,
            ));
            items.push(value_item(
                "Size",
                Some("The pages SQLite holds for this database."),
                "diagnostics-sqlite-size-value",
                format_bytes(info.size_bytes),
                None,
                &palette,
            ));
            items.push(value_item(
                "Integrity",
                Some("PRAGMA quick_check."),
                "diagnostics-integrity-value",
                info.integrity.clone(),
                None,
                &palette,
            ));
            items.extend(table_items(info, &palette));
        }
        rows.push(value_list(items).into_any_element());
        if info.is_none() {
            rows.push(
                div()
                    .id("diagnostics-state-empty")
                    .test_support()
                    .text_xs()
                    .text_color(palette.meta)
                    .child("The state database is opening.")
                    .into_any_element(),
            );
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
                        .disabled(rebuilding)
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
                crate::a11y::named(
                    Role::Label,
                    "diagnostics-errors-empty",
                    "No failure has been recorded.",
                    None,
                )
                .text_xs()
                .text_color(palette.meta)
                .child("No failure has been recorded.")
                .into_any_element(),
            ]
        } else {
            let items = errors
                .iter()
                .map(|error| {
                    // The row keeps the whole sentence as its accessible name, so
                    // the class is heard once with its reason.
                    let name = match &error.message {
                        Some(message) => format!("{}: {message}", error.class.as_str()),
                        None => error.class.as_str().to_string(),
                    };
                    value_item(
                        error.class.as_str(),
                        None,
                        &format!("diagnostics-error-{}", error.class.as_str()),
                        error.message.clone().unwrap_or_default(),
                        Some(name),
                        &palette,
                    )
                })
                .collect();
            vec![value_list(items).into_any_element()]
        };
        section(
            "diagnostics-errors",
            "Last error per class",
            "The newest attempt the client recorded of each failure class.",
            rows,
            cx,
        )
    }

    /// The debug bundle action and the archive it last wrote (issue #25).
    ///
    /// The privacy note stays visible whether or not a bundle exists: it is
    /// the one thing a user needs to read before attaching the archive.
    fn bundle_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let palette = CardPalette::new(theme);
        let state = self.model.read(cx).bundle().clone();
        let directory = self.model.read(cx).bundle_directory().display().to_string();
        let collecting = matches!(state, BundleState::Collecting);
        let mut rows: Vec<AnyElement> = Vec::with_capacity(6);
        rows.push(
            Notice::new(
                "diagnostics-bundle-privacy",
                Severity::Info,
                "The archive holds your folder paths, account email, server URL, and the log \
                 files. It never holds your password, an API key, or a session token: the log \
                 files are already redacted. Review it before you attach it to a message.",
            )
            .into_any_element(),
        );
        rows.push(
            div()
                .id("diagnostics-bundle-collect-row")
                .test_support()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_6()
                .min_h(rems(2.25))
                .child(field_text(
                    "Collect",
                    &format!(
                        "Writes one .zip into {directory}: the newest {LOG_FILES} log files, \
                         {} each at most, plus the versions, database, configuration, sizing, and \
                         runtime sections.",
                        format_bytes(LOG_BYTES)
                    ),
                    &palette,
                ))
                .child(
                    Button::new("diagnostics-bundle-collect")
                        .label("Collect bundle")
                        .accessibility_label("Collect the debug bundle into one archive")
                        .disabled(collecting)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.model.update(cx, |model, cx| model.collect_bundle(cx));
                        })),
                )
                .into_any_element(),
        );
        match &state {
            BundleState::Idle => {}
            BundleState::Collecting => rows.push(
                Notice::new(
                    "diagnostics-bundle-collecting",
                    Severity::Info,
                    "Collecting the debug bundle…",
                )
                .into_any_element(),
            ),
            BundleState::Ready(report) => {
                rows.push(
                    value_list(vec![mono_value_item(
                        "Archive",
                        Some("Attach this file to your support message."),
                        "diagnostics-bundle-path-value",
                        report.path.display().to_string(),
                        &palette,
                        theme.mono_font_family.clone(),
                    )])
                    .into_any_element(),
                );
                rows.push(
                    div()
                        .id("diagnostics-bundle-copy-row")
                        .test_support()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap_6()
                        .min_h(rems(2.25))
                        .child(field_text(
                            "Copy path",
                            "Puts the archive's full path on the clipboard, so it can be pasted \
                             into a message or a file manager.",
                            &palette,
                        ))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_3()
                                // #82: the confirmation sits beside the control
                                // it belongs to, in the row, so it opens no
                                // surface and adds no row above the control.
                                .flex_shrink_0()
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .justify_end()
                                        // #82: its slot is reserved whether or not
                                        // it is showing, so the confirmation
                                        // cannot rewrap the help text and slide
                                        // the button out from under the pointer.
                                        .w(rems(6.))
                                        .children(copy_confirmation(
                                            "diagnostics-copy-confirmation",
                                            self.presses(),
                                        )),
                                )
                                .child(
                                    Button::new("diagnostics-bundle-copy")
                                        .label("Copy path")
                                        .accessibility_label("Copy the debug bundle's path")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.copy_bundle_path(cx);
                                        })),
                                ),
                        )
                        .into_any_element(),
                );
                // A partial archive says what it could not hold, in the same
                // words its README uses.
                for (index, note) in report.notes.iter().enumerate() {
                    rows.push(
                        Notice::new(
                            ElementId::Name(format!("diagnostics-bundle-note-{index}").into()),
                            Severity::Warning,
                            note.clone(),
                        )
                        .into_any_element(),
                    );
                }
            }
            BundleState::Failed(problem) => rows.push(
                Notice::new(
                    "diagnostics-bundle-problem",
                    Severity::Error,
                    problem.clone(),
                )
                .into_any_element(),
            ),
        }
        section(
            "diagnostics-bundle",
            "Debug bundle",
            "One archive with the logs, versions, database info, configuration, and sizing, \
             ready to attach to a support message. Collecting runs in the background.",
            rows,
            cx,
        )
    }

    /// Puts the last archive's path on the clipboard.
    ///
    /// Only a ready archive can be copied: the control renders only for one, and
    /// a press with nothing behind it confirms nothing (#82).
    fn copy_bundle_path(&mut self, cx: &mut Context<Self>) {
        let path = match self.model.read(cx).bundle() {
            BundleState::Ready(report) => report.path.display().to_string(),
            _ => return,
        };
        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
        self.copied = Some(match self.copied.take() {
            Some((copied, presses)) if copied == path => (copied, presses + 1),
            _ => (path, 1),
        });
        cx.notify();
    }

    /// How many times the archive now on the clipboard has been copied (#82).
    fn presses(&self) -> u32 {
        self.copied.as_ref().map_or(0, |(_, presses)| *presses)
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
        let items = vec![
            value_item(
                "Upload rate",
                Some("Measured between the last two status polls."),
                "diagnostics-throughput-value",
                rate,
                None,
                &palette,
            ),
            value_item(
                "Activity",
                Some("What the engine is doing right now."),
                "diagnostics-activity-value",
                status.activity.label().to_string(),
                None,
                &palette,
            ),
            value_item(
                "Items",
                Some("The counts across every folder."),
                "diagnostics-counts-value",
                counts,
                None,
                &palette,
            ),
        ];
        section(
            "diagnostics-throughput",
            "Throughput",
            "Upload bytes per second, from the same meter the bandwidth limit uses.",
            vec![value_list(items).into_any_element()],
            cx,
        )
    }
}

/// The label column every value row shares, in rems: wide enough for the
/// longest term, and fixed, so a long value wraps inside its own column instead
/// of taking the term's width (#80).
const LABEL_COLUMN_REMS: f32 = 12.;

/// The most lines a value may take before it is cut (#80).
///
/// A value can be as long as a server or a filesystem makes it — a failure
/// message is the raw error text — and an unclamped one grows its card by that
/// many lines. The cut is visual only: the value's accessible name still carries
/// the whole text.
const VALUE_LINE_CLAMP: usize = 6;

/// The description list a card's value rows share (#80).
///
/// `columns(1)` puts one row in each line. `bordered(false)` leaves the card's
/// own surface and border to carry the box: the rows are a grid inside the
/// card, not a second card inside the first.
fn value_list(items: Vec<DescriptionItem>) -> impl IntoElement {
    DescriptionList::new()
        .columns(1)
        .bordered(false)
        .label_width(rems(LABEL_COLUMN_REMS))
        .children(items)
}

/// One value row: the term in the label cell, and the value with its help
/// sentence in the description cell.
///
/// `name` is the value's accessible name when it is not the value itself: the
/// per-table rows keep the `table: rows` name their hand-written row had.
fn value_item(
    label: &str,
    help: Option<&str>,
    value_id: &str,
    value: String,
    name: Option<String>,
    palette: &CardPalette,
) -> DescriptionItem {
    value_item_with_font(label, help, value_id, value, name, palette, None)
}

/// The archive path's row: [`value_item`] with its value set in the mono font,
/// as every other read-only path in the app is, so `l`/`1` and `O`/`0` stay
/// apart (#80).
fn mono_value_item(
    label: &str,
    help: Option<&str>,
    value_id: &str,
    value: String,
    palette: &CardPalette,
    mono: SharedString,
) -> DescriptionItem {
    value_item_with_font(label, help, value_id, value, None, palette, Some(mono))
}

/// The one row shape both constructors build, with `font` as the value's family
/// when it is not the UI's (#80).
fn value_item_with_font(
    label: &str,
    help: Option<&str>,
    value_id: &str,
    value: String,
    name: Option<String>,
    palette: &CardPalette,
    font: Option<SharedString>,
) -> DescriptionItem {
    DescriptionItem::new(
        crate::a11y::named(
            Role::Term,
            ElementId::Name(format!("{value_id}-label").into()),
            label,
            None,
        )
        .min_w_0()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .child(label.to_string())
        .into_any_element(),
    )
    .value(
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .min_w_0()
            .child(
                // The description-list value of the term above it, so the two
                // reach the tree as a pair and not as flat text (#80).
                crate::a11y::named(
                    Role::Definition,
                    ElementId::Name(value_id.into()),
                    name.unwrap_or_else(|| value.clone()),
                    None,
                )
                .min_w_0()
                .line_clamp(VALUE_LINE_CLAMP)
                .text_xs()
                .text_color(palette.text)
                .when_some(font, |this, font| this.font_family(font))
                .child(value),
            )
            .when_some(help, |this, help| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(palette.meta)
                        .child(help.to_string()),
                )
            })
            .into_any_element(),
    )
}

/// One row per tracked table, with its row count (#80).
fn table_items(info: &SqliteInfo, palette: &CardPalette) -> Vec<DescriptionItem> {
    info.tables
        .iter()
        .map(|count| {
            value_item(
                &count.table,
                None,
                &format!("diagnostics-table-{}", count.table),
                count.rows.to_string(),
                Some(format!("{}: {}", count.table, count.rows)),
                palette,
            )
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
        let bundle = self.bundle_section(cx);

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
            .child(bundle)
    }
}
