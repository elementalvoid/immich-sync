//! The Diagnostics view (SPEC R10.2, R10.4): the client and server versions,
//! the sqlite state, the last error per class, the throughput, and the rebuild
//! action.

use gpui_kit::base::Disableable as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::description_list::DescriptionItem;
use gpui_kit::*;

use crate::dashboard::CardPalette;
use crate::debug_bundle::{LOG_BYTES, LOG_FILES};
use crate::diagnostics::{BundleState, DiagnosticsModel, Permissions, format_bytes, format_rate};
use crate::folder_panel::FolderOpener;
use crate::notice::{Notice, Severity, copy_confirmation};
use crate::rows::{Row, mono_value_item, value_item, value_list};
use crate::settings_view::section;
use crate::version;
use std::rc::Rc;
use std::sync::Arc;
use sync_core::store::SqliteInfo;

/// The Diagnostics view.
pub struct DiagnosticsView {
    /// The model owns every read; the view renders what it last produced.
    model: Entity<DiagnosticsModel>,
    /// The archive path the last copy put on the clipboard, and how many presses
    /// it has had since. Both are dropped once that path is no longer the last
    /// archive, so the confirmation never describes a stale copy (#82).
    copied: Option<(String, u32)>,
    /// What "Show in folder" runs (#87): the file manager in the product, a
    /// recorder in a test.
    revealer: FolderOpener,
    /// The archive path the last "Show in folder" could not open, and why. It
    /// is shown only for that archive, so it never describes a newer one (#87).
    reveal_problem: Option<(String, String)>,
}

impl DiagnosticsView {
    /// A view over the diagnostics model.
    pub fn new(model: Entity<DiagnosticsModel>, cx: &mut Context<Self>) -> Self {
        Self::with_revealer(
            model,
            Arc::new(crate::folder_panel::reveal_in_file_manager),
            cx,
        )
    }

    /// A view whose "Show in folder" hands the archive to `revealer` instead of
    /// the platform's file manager, so a test can record the path (#87).
    pub fn with_revealer(
        model: Entity<DiagnosticsModel>,
        revealer: FolderOpener,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&model, |this, model, cx| {
            // The confirmation names the path it copied, so it goes when the last
            // archive is no longer that path (#82). A failure to show an archive
            // goes the same way, for the same reason (#87).
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
            let still_refused = this
                .reveal_problem
                .as_ref()
                .is_some_and(|(refused, _)| current.as_deref() == Some(refused.as_str()));
            if !still_refused {
                this.reveal_problem = None;
            }
            cx.notify();
        })
        .detach();
        Self {
            model,
            copied: None,
            revealer,
            reveal_problem: None,
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
    fn server_section(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
                &palette,
            ),
            value_item(
                "Server version",
                Some("GET /server/about with this credential."),
                "diagnostics-server-version-value",
                version,
                &palette,
            ),
            value_item(
                "Permissions",
                Some("An API key's granted permissions, read from GET /api-keys/me."),
                "diagnostics-permissions-value",
                permissions,
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
        rows.push(value_list("diagnostics-server-values", items).into_any_element());
        let refresh = Rc::new(cx.listener(|this, _, _, cx| {
            this.model.update(cx, |model, cx| model.refresh(cx));
        }));
        rows.push(
            Row::wrapped(
                "Refresh",
                "Re-reads the state database and re-checks the server.",
                "diagnostics-refresh-row",
                move |_, _| {
                    let refresh = Rc::clone(&refresh);
                    Button::new("diagnostics-refresh")
                        .label("Refresh")
                        .accessibility_label("Re-check the server and the state database")
                        .on_click(move |event, window, cx| refresh(event, window, cx))
                        .into_any_element()
                },
            )
            .render(window, cx),
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
    fn state_section(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
            &palette,
        ));
        if let Some(info) = &info {
            items.push(value_item(
                "Migration version",
                Some("PRAGMA user_version: the schema the client migrated to."),
                "diagnostics-sqlite-version-value",
                info.migration_version.to_string(),
                &palette,
            ));
            items.push(value_item(
                "Size",
                Some("The pages SQLite holds for this database."),
                "diagnostics-sqlite-size-value",
                format_bytes(info.size_bytes),
                &palette,
            ));
            items.push(value_item(
                "Integrity",
                Some("PRAGMA quick_check."),
                "diagnostics-integrity-value",
                info.integrity.clone(),
                &palette,
            ));
            items.extend(table_items(info, &palette));
        }
        rows.push(value_list("diagnostics-state-values", items).into_any_element());
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
        let rebuild = Rc::new(cx.listener(|this, _, _, cx| {
            this.model.update(cx, |model, cx| model.rebuild(cx));
        }));
        rows.push(
            Row::wrapped(
                "Rebuild",
                "Starts the state over and rescans every folder. The server is re-checked.",
                "diagnostics-rebuild-row",
                move |_, _| {
                    let rebuild = Rc::clone(&rebuild);
                    Button::new("diagnostics-rebuild")
                        .label("Rebuild")
                        .accessibility_label("Rebuild the state database and rescan the folders")
                        .disabled(rebuilding)
                        .on_click(move |event, window, cx| rebuild(event, window, cx))
                        .into_any_element()
                },
            )
            .render(window, cx),
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
                    // The term is the class and the value is the reason, so the
                    // row does not name the class a second time (F1).
                    value_item(
                        error.class.as_str(),
                        None,
                        &format!("diagnostics-error-{}", error.class.as_str()),
                        error.message.clone().unwrap_or_default(),
                        &palette,
                    )
                })
                .collect();
            vec![value_list("diagnostics-errors-values", items).into_any_element()]
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
    fn bundle_section(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let palette = CardPalette::new(cx.theme());
        let mono = cx.theme().mono_font_family.clone();
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
        let collect = Rc::new(cx.listener(|this, _, _, cx| {
            this.model.update(cx, |model, cx| model.collect_bundle(cx));
        }));
        rows.push(
            Row::wrapped(
                "Collect",
                format!(
                    "Writes one .zip into {directory}: the newest {LOG_FILES} log files, \
                     {} each at most, plus the versions, database, configuration, sizing, and \
                     runtime sections.",
                    format_bytes(LOG_BYTES)
                ),
                "diagnostics-bundle-collect-row",
                move |_, _| {
                    let collect = Rc::clone(&collect);
                    Button::new("diagnostics-bundle-collect")
                        .label("Collect bundle")
                        .accessibility_label("Collect the debug bundle into one archive")
                        .disabled(collecting)
                        .on_click(move |event, window, cx| collect(event, window, cx))
                        .into_any_element()
                },
            )
            .render(window, cx),
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
                    value_list(
                        "diagnostics-bundle-values",
                        vec![mono_value_item(
                            "Archive",
                            Some("Attach this file to your support message."),
                            "diagnostics-bundle-path-value",
                            report.path.display().to_string(),
                            &palette,
                            mono.clone(),
                        )],
                    )
                    .into_any_element(),
                );
                let copy_path = Rc::new(cx.listener(|this, _, _, cx| {
                    this.copy_bundle_path(cx);
                }));
                let presses = self.presses();
                rows.push(
                    Row::wrapped(
                        "Copy path",
                        "Puts the archive's full path on the clipboard, so it can be pasted \
                         into a message or a file manager.",
                        "diagnostics-bundle-copy-row",
                        move |_, _| {
                            let copy_path = Rc::clone(&copy_path);
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
                                            presses,
                                        )),
                                )
                                .child(
                                    Button::new("diagnostics-bundle-copy")
                                        .label("Copy path")
                                        .accessibility_label("Copy the debug bundle's path")
                                        .on_click(move |event, window, cx| {
                                            copy_path(event, window, cx)
                                        }),
                                )
                                .into_any_element()
                        },
                    )
                    .render(window, cx),
                );
                let reveal = Rc::new(cx.listener(|this, _, _, cx| {
                    this.reveal_bundle(cx);
                }));
                rows.push(
                    Row::wrapped(
                        "Show in folder",
                        "Opens the folder that holds the archive in your file manager, with \
                         the archive selected where the platform allows it.",
                        "diagnostics-bundle-reveal-row",
                        move |_, _| {
                            let reveal = Rc::clone(&reveal);
                            Button::new("diagnostics-bundle-reveal")
                                .label("Show in folder")
                                .accessibility_label("Show the debug bundle in the file manager")
                                .on_click(move |event, window, cx| reveal(event, window, cx))
                                .into_any_element()
                        },
                    )
                    .render(window, cx),
                );
                if let Some((_, problem)) = self
                    .reveal_problem
                    .as_ref()
                    .filter(|(refused, _)| *refused == report.path.display().to_string())
                {
                    rows.push(
                        Notice::new(
                            "diagnostics-bundle-reveal-problem",
                            Severity::Error,
                            problem.clone(),
                        )
                        .into_any_element(),
                    );
                }
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

    /// Shows the last archive in the file manager (#87). It writes nothing to
    /// the clipboard and confirms nothing: the file manager opening is the
    /// feedback. A refusal is not feedback, so it is shown as an error beside
    /// the control and logged.
    fn reveal_bundle(&mut self, cx: &mut Context<Self>) {
        let path = match self.model.read(cx).bundle() {
            BundleState::Ready(report) => report.path.display().to_string(),
            _ => return,
        };
        self.reveal_problem = match (self.revealer)(&path) {
            Ok(()) => None,
            Err(error) => {
                tracing::warn!(%error, "could not show the debug bundle in the file manager");
                Some((
                    path,
                    format!("The debug bundle could not be shown in the file manager: {error}"),
                ))
            }
        };
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
                &palette,
            ),
            value_item(
                "Activity",
                Some("What the engine is doing right now."),
                "diagnostics-activity-value",
                status.activity.label().to_string(),
                &palette,
            ),
            value_item(
                "Items",
                Some("The counts across every folder."),
                "diagnostics-counts-value",
                counts,
                &palette,
            ),
        ];
        section(
            "diagnostics-throughput",
            "Throughput",
            "Upload bytes per second, from the same meter the bandwidth limit uses.",
            vec![value_list("diagnostics-throughput-values", items).into_any_element()],
            cx,
        )
    }
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
                palette,
            )
        })
        .collect()
}

impl Render for DiagnosticsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let background = theme.background;
        let foreground = theme.foreground;
        let server = self.server_section(window, cx);
        let state = self.state_section(window, cx);
        let errors = self.errors_section(cx);
        let throughput = self.throughput_section(cx);
        let bundle = self.bundle_section(window, cx);

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
