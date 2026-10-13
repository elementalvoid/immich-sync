//! The manual one-file upload (issue #5): the app-scoped state behind the
//! Upload view.
//!
//! The model owns the state store and the last result. The network work runs on
//! the same Tokio runtime as the connection, and the result comes back to the
//! model through an `async_channel`, so a hidden window loses nothing.

use std::path::Path;
use std::sync::Arc;

use gpui_kit::*;
use sync_core::failure::FailureClass;
use sync_core::store::StateStore;
use sync_core::upload::{UploadOutcome, Uploader};

use crate::connection::{ConnectionModel, worker};
use crate::paths::AppPaths;

/// What the window shows for the last manual upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadReport {
    /// Nothing has been uploaded in this run.
    Idle,
    /// An upload is in flight.
    Running,
    /// The server created the asset (R3.2, R3.6).
    Created {
        /// The asset id the server returned.
        asset_id: String,
    },
    /// The content was already on the server, or the stored SHA-1 is unchanged
    /// (R3.2, R4.5).
    Duplicate {
        /// The existing asset id.
        asset_id: String,
    },
    /// The upload failed.
    Failed {
        /// The persisted class, when one fits (R6.4).
        class: Option<FailureClass>,
        /// Whether the failure is permanent (R6.3).
        permanent: bool,
        /// The user-facing reason.
        message: String,
    },
    /// The upload could not start: not signed in, no path, or no database.
    CannotRun {
        /// The reason to show.
        message: String,
    },
}

impl UploadReport {
    /// The short label the notice shows.
    #[must_use]
    pub fn headline(&self) -> &'static str {
        match self {
            Self::Idle => "No upload yet",
            Self::Running => "Uploading",
            Self::Created { .. } => "Uploaded",
            Self::Duplicate { .. } => "Duplicate",
            Self::Failed {
                permanent: true, ..
            } => "Permanent error",
            Self::Failed {
                permanent: false, ..
            } => "Temporary error",
            Self::CannotRun { .. } => "Upload unavailable",
        }
    }

    /// The full outcome line, including the asset id or the failure class.
    ///
    /// This is the accessible name of the result notice, so it states the whole
    /// outcome once.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Idle => "No file has been uploaded yet.".to_string(),
            Self::Running => "Uploading the file…".to_string(),
            Self::Created { asset_id } => format!("Uploaded. Asset id: {asset_id}"),
            Self::Duplicate { asset_id } => {
                format!(
                    "Already on the server (duplicate). Asset id: {asset_id}. Nothing was transferred."
                )
            }
            Self::Failed {
                class: Some(class),
                permanent: true,
                message,
            } => format!("Permanent error ({}). {message}", class.as_str()),
            Self::Failed {
                class: None,
                permanent: true,
                message,
            } => format!("Permanent error. {message}"),
            Self::Failed {
                class: Some(class),
                message,
                ..
            } => format!("Temporary error ({}). {message}", class.as_str()),
            Self::Failed {
                class: None,
                message,
                ..
            } => format!("Temporary error. {message}"),
            Self::CannotRun { message } => message.clone(),
        }
    }

    /// Whether an upload is in flight, for a control that must not start a
    /// second one.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// The app-scoped manual upload.
pub struct UploadModel {
    connection: Entity<ConnectionModel>,
    /// The state store, or why it could not be opened.
    store: Result<Arc<StateStore>, String>,
    report: UploadReport,
    /// Changes with every report, so a result from an upload the user
    /// superseded is dropped instead of overwriting the newer report.
    generation: u64,
}

impl UploadModel {
    /// Opens the state database and prepares the model.
    ///
    /// A database that cannot be opened does not stop the window: every upload
    /// then reports why. The store lives for the process, so the startup
    /// integrity check runs once (N4).
    #[must_use]
    pub fn new(connection: Entity<ConnectionModel>, paths: &AppPaths) -> Self {
        let store = StateStore::open(&paths.state_db)
            .map(Arc::new)
            .map_err(|error| error.to_string());
        let report = match &store {
            Ok(_) => UploadReport::Idle,
            Err(message) => UploadReport::CannotRun {
                message: format!("The state database could not be opened: {message}"),
            },
        };
        Self {
            connection,
            store,
            report,
            generation: 0,
        }
    }

    /// The last result, for the view and the tray.
    #[must_use]
    pub fn report(&self) -> &UploadReport {
        &self.report
    }

    /// Uploads one file (R3.1).
    ///
    /// The request runs on the HTTP worker; the result comes back on the UI
    /// thread. A successful request restores the one re-login budget (R1.4).
    pub fn upload(&mut self, path: String, cx: &mut Context<Self>) {
        let path = path.trim().to_string();
        if path.is_empty() {
            self.set(
                UploadReport::CannotRun {
                    message: "Enter the path of a file to upload.".to_string(),
                },
                cx,
            );
            return;
        }
        let store = match &self.store {
            Ok(store) => store.clone(),
            Err(message) => {
                self.set(
                    UploadReport::CannotRun {
                        message: message.clone(),
                    },
                    cx,
                );
                return;
            }
        };
        let (client, credential) = {
            let connection = self.connection.read(cx);
            (connection.client(), connection.credential())
        };
        let (Some(client), Some(credential)) = (client, credential) else {
            self.set(
                UploadReport::CannotRun {
                    message: "Sign in to a server before uploading a file.".to_string(),
                },
                cx,
            );
            return;
        };
        let Ok(runtime) = worker() else {
            self.set(
                UploadReport::CannotRun {
                    message: "The network runtime could not start.".to_string(),
                },
                cx,
            );
            return;
        };

        self.set(UploadReport::Running, cx);
        // The attempt belongs to the report the caller just set; a newer report
        // supersedes it.
        let generation = self.generation;
        let (results, incoming) = async_channel::bounded(1);
        runtime.spawn(async move {
            let outcome = Uploader::new(client, store)
                .upload_one(Path::new(&path), &credential)
                .await;
            let _ = results.send(outcome).await;
        });
        cx.spawn(async move |this, cx| {
            let Ok(outcome) = incoming.recv().await else {
                return;
            };
            let _ = this.update(cx, |model, cx| {
                if model.generation != generation {
                    return;
                }
                // R1.4: a request that succeeded with the current credential
                // restores the re-login budget.
                if matches!(
                    outcome,
                    UploadOutcome::Created { .. } | UploadOutcome::Duplicate { .. }
                ) {
                    model
                        .connection
                        .update(cx, |connection, _| connection.note_authenticated());
                }
                model.set(report_from_outcome(outcome), cx);
            });
        })
        .detach();
    }

    /// Replaces the report and notifies the views.
    fn set(&mut self, report: UploadReport, cx: &mut Context<Self>) {
        self.report = report;
        self.generation = self.generation.wrapping_add(1);
        cx.notify();
    }
}

/// Maps the engine's outcome onto the window's state.
#[must_use]
pub fn report_from_outcome(outcome: UploadOutcome) -> UploadReport {
    match outcome {
        UploadOutcome::Created { asset_id } => UploadReport::Created { asset_id },
        UploadOutcome::Duplicate { asset_id } => UploadReport::Duplicate { asset_id },
        UploadOutcome::Failed(failure) => UploadReport::Failed {
            class: failure.class,
            permanent: failure.is_permanent(),
            message: failure.message,
        },
    }
}
