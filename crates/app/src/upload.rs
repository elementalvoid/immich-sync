//! The manual one-file upload (issue #5): the app-scoped state behind the
//! Upload view.
//!
//! The model reads the shared state store and keeps the last result. The
//! network work runs on the same Tokio runtime as the connection, and the
//! result comes back to the model through an `async_channel`, so a hidden
//! window loses nothing.

use std::path::Path;

use gpui_kit::*;
use sync_core::failure::FailureClass;
use sync_core::upload::{UploadOutcome, Uploader};

use crate::connection::{ConnectionModel, worker};
use crate::store::StoreModel;

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
    /// The shared store, opened off the UI thread by [`StoreModel`] (N2).
    store: Entity<StoreModel>,
    report: UploadReport,
    /// Changes with every report, so a result from an upload the user
    /// superseded is dropped instead of overwriting the newer report.
    generation: u64,
}

impl UploadModel {
    /// Reads the shared state database and prepares the model.
    ///
    /// A database that cannot be opened does not stop the window: every upload
    /// then reports why. The store lives for the process, so the startup
    /// integrity check runs once (N4).
    #[must_use]
    pub fn new(
        connection: Entity<ConnectionModel>,
        store: Entity<StoreModel>,
        cx: &mut Context<Self>,
    ) -> Self {
        let report = match store.read(cx).problem() {
            Some(problem) => UploadReport::CannotRun {
                message: format!("The state database could not be opened: {problem}"),
            },
            None => UploadReport::Idle,
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
    ///
    /// A manual upload is deliberately exempt from the sync pause (R8.7): the
    /// user picked this one file and pressed Upload, so it is an explicit
    /// request rather than background sync work.
    pub fn upload(&mut self, path: String, cx: &mut Context<Self>) {
        // The button disables itself while busy; Enter must not slip past it.
        if self.report.is_busy() {
            return;
        }
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
        let store = match self.store.read(cx).store() {
            Some(store) => store,
            None => {
                let message = match self.store.read(cx).problem() {
                    Some(problem) => {
                        format!("The state database could not be opened: {problem}")
                    }
                    None => "The state database is still opening.".to_string(),
                };
                self.set(UploadReport::CannotRun { message }, cx);
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
        // Exempt from the engine's pause gate by design (R8.7); see the method
        // doc comment.
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
        // The manual path never pauses, so this is unreachable here; it still
        // reads as a state rather than a panic.
        UploadOutcome::Deferred => UploadReport::CannotRun {
            message: "Uploads are paused.".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{ConnectionModel, UploadModel, UploadReport};
    use crate::paths::AppPaths;
    use gpui_kit::AppContext as _;
    use gpui_kit::TestAppContext;
    use std::sync::Arc;
    use std::time::Duration;
    use sync_core::connection::{AuthMethod, Secret, ServerProfile, SignIn, Unauthorized};
    use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};

    /// Starts the offline Immich double on its own runtime.
    fn mock_server() -> (tokio::runtime::Runtime, mock_immich::MockImmich) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("start the mock runtime");
        let server = runtime.block_on(mock_immich::MockImmich::start());
        (runtime, server)
    }

    /// Pumps GPUI's test executor until the condition holds.
    async fn wait_until(cx: &mut TestAppContext, mut done: impl FnMut(&mut gpui_kit::App) -> bool) {
        for _ in 0..600 {
            if cx.update(&mut done) {
                return;
            }
            cx.executor().timer(Duration::from_millis(10)).await;
            cx.run_until_parked();
        }
        panic!("the test condition never became true");
    }

    #[gpui_kit::test]
    async fn a_successful_upload_restores_the_re_login_budget(cx: &mut TestAppContext) {
        let (_runtime, server) = mock_server();
        cx.executor().allow_parking();
        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = AppPaths {
            config_file: temp.path().join("config").join("settings.toml"),
            state_db: temp.path().join("data").join("state.db"),
            autostart_dir: temp.path().join("config").join("autostart"),
        };
        let profile = ServerProfile {
            server_url: server.base_url().to_string(),
            email: "sync@example.com".to_string(),
            method: AuthMethod::Password,
        };
        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        let connection = cx.update(|cx| {
            let connection = cx.new(|_| ConnectionModel::new(profile.clone(), credentials));
            let request = SignIn {
                email: profile.email.clone(),
                secret: Secret::new("correct horse"),
                method: AuthMethod::Password,
            };
            connection.update(cx, |model, cx| model.sign_in(request, profile.clone(), cx));
            connection
        });
        wait_until(cx, |cx| connection.read(cx).state().is_connected()).await;

        // A first 401 spends the one re-login of this session.
        server.revoke_tokens();
        let first = cx
            .update(|cx| connection.update(cx, |model, cx| model.handle_unauthorized_for_test(cx)));
        assert_eq!(first, Unauthorized::Retry, "the first 401 re-logs in");

        let store = cx.update(|cx| cx.new(|cx| crate::store::StoreModel::open(&paths, cx)));
        wait_until(cx, |cx| store.read(cx).store().is_some()).await;
        let model = cx.update(|cx| cx.new(|cx| UploadModel::new(connection.clone(), store, cx)));
        let file = temp.path().join("hello.txt");
        std::fs::write(&file, b"hello world").expect("write the fixture");
        let path_text = file.display().to_string();
        cx.update(|cx| {
            model.update(cx, |model, cx| model.upload(path_text.clone(), cx));
        });
        wait_until(cx, |cx| {
            matches!(model.read(cx).report(), UploadReport::Created { .. })
        })
        .await;

        // The successful upload restored the budget, so a later 401 re-logs in
        // again instead of parking the connection in needs-attention.
        server.revoke_tokens();
        let second = cx
            .update(|cx| connection.update(cx, |model, cx| model.handle_unauthorized_for_test(cx)));
        assert_eq!(
            second,
            Unauthorized::Retry,
            "an upload that succeeded must refresh the re-login budget"
        );
        assert!(cx.read(|cx| connection.read(cx).state().is_connected()));
    }
}
