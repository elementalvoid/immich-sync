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
#[cfg(test)]
use sync_core::connection::Unauthorized;
use sync_core::connection::{
    Attention, AuthMethod, Connection, ConnectionState, RequestCredential, ServerProfile,
    SharedSession, SignIn,
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
pub(crate) fn worker() -> Result<&'static tokio::runtime::Runtime, String> {
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
    /// The live session, shared with the sync engine so a 401 on an upload is
    /// answered by the same one re-login rule the form uses (R1.4).
    session: Option<SharedSession>,
    provider: CredentialStoreProvider,
    /// Whether the startup restore already ran. The model outlives every
    /// window, so a reopen must not reset the session (R1.3).
    startup_restore_started: bool,
    /// Changes with every state change, so a result from an attempt that a
    /// newer state superseded is dropped instead of overwriting it.
    generation: u64,
}

impl ConnectionModel {
    /// A model with no session yet. A configured profile starts in the
    /// connecting state, because [`Self::restore_once`] is about to run.
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
            session: None,
            provider: store.into(),
            startup_restore_started: false,
            generation: 0,
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
    ///
    /// The generation moves with it, so an attempt still in flight cannot
    /// overwrite this state when its result arrives.
    pub fn set_state(&mut self, state: ConnectionState, cx: &mut Context<Self>) {
        if state.is_connected() {
            self.form_error = None;
        }
        self.state = state;
        self.generation = self.generation.wrapping_add(1);
        cx.notify();
    }

    /// The HTTP client of the live session, for an upload request.
    #[must_use]
    pub fn client(&self) -> Option<sync_core::ImmichClient> {
        self.session.as_ref().map(SharedSession::client)
    }

    /// The credential the live session attaches to a request.
    #[must_use]
    pub fn credential(&self) -> Option<RequestCredential> {
        self.session.as_ref().and_then(SharedSession::credential)
    }

    /// The live session, for a request path that answers its own 401s (R1.4).
    #[must_use]
    pub fn session(&self) -> Option<SharedSession> {
        self.session.clone()
    }

    /// Adopts the session's state after a request path changed it (R1.4).
    ///
    /// The engine's 401 handling runs off this model's thread, so the model
    /// pulls the state it produced on its own poll instead of being notified.
    pub fn refresh_state(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            let state = session.state();
            if state != self.state {
                self.set_state(state, cx);
            }
        }
    }

    /// Records that an authenticated request succeeded with the current
    /// credential, which restores the one re-login budget (R1.4).
    pub fn note_authenticated(&mut self) {
        if let Some(session) = &self.session {
            session.note_authenticated();
        }
    }

    /// A test seam onto the session's own 401 handling, so the upload tests can
    /// exhaust the re-login budget and then observe it restored. No request
    /// path calls it yet; the retried request belongs to #7.
    #[cfg(test)]
    pub(crate) fn handle_unauthorized_for_test(&mut self, cx: &mut Context<Self>) -> Unauthorized {
        let session = self.session.clone().expect("a live session before the 401");
        let outcome = worker()
            .expect("the worker runtime starts in tests")
            .block_on(session.handle_unauthorized());
        self.set_state(session.state(), cx);
        outcome
    }

    /// Logs in at startup with the saved profile and the stored credential, with
    /// no user input (R1.3), at most once per model.
    ///
    /// The window's root view is rebuilt on every show, so this is called from
    /// the settings view's constructor. The once guard lives on the app-scoped
    /// model: a reopen must not restart the saved login, which would reset a
    /// state the tray is already showing.
    pub fn restore_once(&mut self, profile: ServerProfile, cx: &mut Context<Self>) {
        if self.startup_restore_started {
            return;
        }
        self.startup_restore_started = true;
        self.profile = profile.clone();
        if !profile.is_configured() {
            self.session = None;
            self.set_state(ConnectionState::NotConfigured, cx);
            return;
        }
        self.set_state(ConnectionState::Connecting, cx);
        tracing::info!(
            server = %crate::logging::without_userinfo(&profile.server_url),
            email = %profile.email,
            method = ?profile.method,
            "restoring the saved sign-in"
        );
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
            tracing::debug!("sign-in not attempted: the server URL or the email is missing");
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
            tracing::debug!(
                method = ?profile.method,
                "sign-in not attempted: the secret field is empty"
            );
            cx.notify();
            return;
        }
        self.profile = profile.clone();
        // The attempt names the account, never the secret (R10.3, N3).
        tracing::info!(
            server = %crate::logging::without_userinfo(&profile.server_url),
            email = %profile.email,
            method = ?profile.method,
            "sign-in attempt"
        );
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
        // The attempt belongs to the state the caller just set. A later state
        // change supersedes it, so its result must not overwrite that state.
        let generation = self.generation;
        // Resolving per request makes the recovery step real: starting the OS
        // credential store and signing in again works without a restart.
        let store = self.provider.resolve();
        let (results, incoming) = async_channel::bounded(1);
        runtime.spawn(async move {
            let (state, session) = match Connection::new(&server_url, store) {
                Ok(connection) => {
                    let session = connection.shared();
                    let state = match request {
                        Some(request) => session.sign_in(request).await,
                        None => session.restore(&profile).await,
                    };
                    (state, Some(session))
                }
                Err(error) => (
                    ConnectionState::NeedsAttention(Attention::from(&error)),
                    None,
                ),
            };
            let _ = results.send((state, session)).await;
        });
        cx.spawn(async move |this, cx| {
            let Ok((state, session)) = incoming.recv().await else {
                return;
            };
            // The model can be gone while the request runs; the result is then
            // simply dropped.
            let _ = this.update(cx, |model, cx| {
                if model.generation != generation {
                    return;
                }
                model.session = session;
                log_outcome(&state);
                model.set_state(state, cx);
            });
        })
        .detach();
    }
}

/// Records a sign-in outcome. The state holds no secret, and a locally
/// classified reason is safe to write; a server-supplied message is not,
/// because a broken or hostile server can echo a submitted credential in it
/// (R10.3, N3).
fn log_outcome(state: &ConnectionState) {
    match state {
        ConnectionState::NeedsAttention(Attention::Server(_)) => tracing::warn!(
            state = state.summary(),
            reason = "the server answered an error",
            "sign-in outcome"
        ),
        ConnectionState::NeedsAttention(Attention::Unreachable(_)) => tracing::warn!(
            state = state.summary(),
            reason = "the server could not be reached",
            "sign-in outcome"
        ),
        ConnectionState::NeedsAttention(attention) => tracing::warn!(
            state = state.summary(),
            reason = %attention.message(),
            "sign-in outcome"
        ),
        other => tracing::info!(state = other.summary(), "sign-in outcome"),
    }
}
