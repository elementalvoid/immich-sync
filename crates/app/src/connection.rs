//! The connection form's state model (SPEC R1.1 to R1.6, R8.1).
//!
//! The model owns the connection state the settings form edits and the
//! dashboard shows. The network work runs on a dedicated Tokio runtime, because
//! GPUI's executor is not one (the pattern in the GPUI Kit documentation), and
//! the result comes back to the model through an `async_channel`.

use std::sync::OnceLock;

use crate::credentials::CredentialStoreProvider;
use crate::settings::Settings;
use gpui_kit::*;
use sync_core::connection::{
    Attention, AuthMethod, Connection, ConnectionState, ServerProfile, SignIn,
};

/// The saved connection profile, from the settings file. It holds no secret.
#[must_use]
pub fn profile_from_settings(settings: &Settings) -> ServerProfile {
    ServerProfile {
        server_url: settings.server_url.clone(),
        email: settings.email.clone(),
        method: settings.auth_method,
    }
}

/// Writes a profile into the settings. The secret is not part of the profile.
pub fn apply_profile(settings: &mut Settings, profile: &ServerProfile) {
    settings.server_url = profile.server_url.clone();
    settings.email = profile.email.clone();
    settings.auth_method = profile.method;
}

/// The Tokio runtime that carries HTTP work, or the reason it could not start.
fn worker() -> Result<&'static tokio::runtime::Runtime, String> {
    static WORKER: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("immich-sync-http")
                .enable_all()
                .build()
                .map_err(|error| format!("Could not start the network runtime: {error}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The connection state the settings form and the dashboard read.
pub struct ConnectionModel {
    profile: ServerProfile,
    state: ConnectionState,
    form_error: Option<String>,
    connection: Option<Connection>,
    provider: CredentialStoreProvider,
}

impl ConnectionModel {
    /// A model with no session yet. A configured profile starts in the
    /// connecting state, because [`Self::restore`] is about to run.
    ///
    /// The credential store arrives as a provider, because the platform store
    /// can appear after the client started (SPEC R1.2, R1.5).
    #[must_use]
    pub fn new(profile: ServerProfile, store: impl Into<CredentialStoreProvider>) -> Self {
        let state = if profile.is_configured() {
            ConnectionState::Connecting
        } else {
            ConnectionState::NotConfigured
        };
        Self {
            profile,
            state,
            form_error: None,
            connection: None,
            provider: store.into(),
        }
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    /// The saved profile the form starts from.
    #[must_use]
    pub fn profile(&self) -> &ServerProfile {
        &self.profile
    }

    /// The one-line state a status card or a tray tooltip shows.
    #[must_use]
    pub fn summary(&self) -> String {
        self.state.message()
    }

    /// A form problem, such as an empty field.
    #[must_use]
    pub fn form_error(&self) -> Option<&str> {
        self.form_error.as_deref()
    }

    /// Replaces the state and notifies the views.
    pub fn set_state(&mut self, state: ConnectionState, cx: &mut Context<Self>) {
        if state.is_connected() {
            self.form_error = None;
        }
        self.state = state;
        cx.notify();
    }

    /// Remembers a profile without signing in, for a first run.
    pub fn set_profile(&mut self, profile: ServerProfile, cx: &mut Context<Self>) {
        self.profile = profile;
        cx.notify();
    }

    /// Logs in at startup with the saved profile and the stored credential, with
    /// no user input (R1.3).
    pub fn restore(&mut self, profile: ServerProfile, cx: &mut Context<Self>) {
        self.profile = profile.clone();
        if !profile.is_configured() {
            self.connection = None;
            self.set_state(ConnectionState::NotConfigured, cx);
            return;
        }
        self.set_state(ConnectionState::Connecting, cx);
        self.begin(profile, None, cx);
    }

    /// Signs in from the form.
    ///
    /// The caller persists the profile once the model reaches connected, so a
    /// refused sign-in never replaces a working profile with a mismatched one.
    pub fn sign_in(&mut self, request: SignIn, profile: ServerProfile, cx: &mut Context<Self>) {
        self.form_error = None;
        if !profile.is_configured() {
            self.form_error = Some("Enter the server URL and the email address.".to_string());
            cx.notify();
            return;
        }
        if request.secret.expose().trim().is_empty() {
            self.form_error = Some(
                match profile.method {
                    AuthMethod::Password => "Enter the password.",
                    AuthMethod::ApiKey => "Enter the API key.",
                }
                .to_string(),
            );
            cx.notify();
            return;
        }
        self.profile = profile.clone();
        self.set_state(ConnectionState::Connecting, cx);
        self.begin(profile, Some(request), cx);
    }

    /// Runs the sign-in or the restore on the worker runtime and applies the
    /// result back on the UI thread.
    fn begin(&mut self, profile: ServerProfile, request: Option<SignIn>, cx: &mut Context<Self>) {
        let runtime = match worker() {
            Ok(runtime) => runtime,
            Err(message) => {
                self.form_error = Some(message.clone());
                self.set_state(
                    ConnectionState::NeedsAttention(Attention::Server(message)),
                    cx,
                );
                return;
            }
        };
        let server_url = profile.server_url.clone();
        // Resolving per request makes the recovery step real: starting the OS
        // credential store and signing in again works without a restart.
        let store = self.provider.resolve();
        let (results, incoming) = async_channel::bounded(1);
        runtime.spawn(async move {
            let (state, connection) = match Connection::new(&server_url, store) {
                Ok(mut connection) => {
                    let state = match request {
                        Some(request) => connection.sign_in(request).await,
                        None => connection.restore(&profile).await,
                    };
                    (state, Some(connection))
                }
                Err(error) => (
                    ConnectionState::NeedsAttention(Attention::from(&error)),
                    None,
                ),
            };
            let _ = results.send((state, connection)).await;
        });
        cx.spawn(async move |this, cx| {
            let Ok((state, connection)) = incoming.recv().await else {
                return;
            };
            // The model can be gone while the request runs; the result is then
            // simply dropped.
            let _ = this.update(cx, |model, cx| {
                model.connection = connection;
                model.set_state(state, cx);
            });
        })
        .detach();
    }
}
