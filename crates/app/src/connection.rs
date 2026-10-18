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

/// The runtime failure every worker-backed edit reports to its model.
pub(crate) const WORKER_UNAVAILABLE: &str = "The network runtime could not start.";

/// The reason a store-backed edit cannot run yet.
pub(crate) const STORE_OPENING: &str = "The state database is still opening.";

/// Runs `work` on the shared HTTP runtime and applies its result on the UI
/// thread (N2).
///
/// This is the one shape behind every worker-backed edit: the bounded result
/// channel, `runtime.spawn` for the work, and `cx.spawn` for the result. `apply`
/// keeps its caller's generation check; the caller reports a runtime that could
/// not start, because each model names that failure in its own place.
///
/// # Errors
///
/// Returns the reason [`worker`] gave when the runtime could not start.
pub(crate) fn run_on_worker<T, R, Fut, Work, Apply>(
    cx: &mut Context<T>,
    work: Work,
    apply: Apply,
) -> Result<(), String>
where
    T: 'static,
    R: Send + 'static,
    Fut: std::future::Future<Output = R> + Send + 'static,
    Work: FnOnce() -> Fut + Send + 'static,
    Apply: FnOnce(&mut T, R, &mut Context<T>) + 'static,
{
    let runtime = worker()?;
    let (results, incoming) = async_channel::bounded(1);
    runtime.spawn(async move {
        let result = work().await;
        let _ = results.send(result).await;
    });
    cx.spawn(async move |this, cx| {
        let Ok(result) = incoming.recv().await else {
            return;
        };
        let _ = this.update(cx, |model, cx| apply(model, result, cx));
    })
    .detach();
    Ok(())
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
    /// answered by the same one re-login rule the form uses (R1.4). The form
    /// signs in on this handle rather than building another one, so the engine
    /// never keeps a session the user has already replaced.
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
                // The session's own state, not a new attempt, so the generation
                // must not move: an attempt still in flight would be dropped as
                // superseded by the very sign-in it is waiting for (R1.4).
                self.state = state;
                cx.notify();
            }
        }
    }

    /// A test seam onto the session's own 401 handling, so the upload tests can
    /// exhaust the re-login budget and then observe it restored. Real requests
    /// answer their own 401s through the session (R1.4).
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
        let server_url = profile.server_url.clone();
        // The attempt belongs to the state the caller just set. A later state
        // change supersedes it, so its result must not overwrite that state.
        let generation = self.generation;
        // Resolving per request makes the recovery step real: starting the OS
        // credential store and signing in again works without a restart.
        let store = self.provider.resolve();
        // A live session is repointed in place, so the sign-in that lands here
        // is the one the sync engine and the uploader hold (R1.4).
        let live = self.session.clone();
        let result = run_on_worker(
            cx,
            move || async move {
                match live {
                    Some(session) => match session.repoint(&server_url, store).await {
                        Ok(()) => {
                            let state = authenticate(session.clone(), profile, request).await;
                            (state, Some(session))
                        }
                        // The session keeps the handle the engine holds, so it
                        // says why the attempt failed instead of the model
                        // alone.
                        Err(error) => (
                            ConnectionState::NeedsAttention(Attention::from(&error)),
                            Some(session),
                        ),
                    },
                    None => match Connection::new(&server_url, store) {
                        Ok(connection) => {
                            let session = connection.shared();
                            let state = authenticate(session.clone(), profile, request).await;
                            (state, Some(session))
                        }
                        Err(error) => (
                            ConnectionState::NeedsAttention(Attention::from(&error)),
                            None,
                        ),
                    },
                }
            },
            // The model can be gone while the request runs; the result is
            // then simply dropped.
            move |model, (state, session), cx| {
                if model.generation != generation {
                    return;
                }
                model.session = session;
                log_outcome(&state);
                model.set_state(state, cx);
            },
        );
        if let Err(message) = result {
            self.form_error = Some(message.clone());
            self.set_state(
                ConnectionState::NeedsAttention(Attention::Server(message)),
                cx,
            );
        }
    }
}

/// Signs in, or restores the saved profile with no user input, on the one
/// session the app, the engine, and the uploader share (R1.3, R1.4).
async fn authenticate(
    session: SharedSession,
    profile: ServerProfile,
    request: Option<SignIn>,
) -> ConnectionState {
    match request {
        Some(request) => session.sign_in(request).await,
        None => session.restore(&profile).await,
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

#[cfg(test)]
mod tests {
    use super::{ConnectionModel, ServerProfile, worker};
    use gpui_kit::AppContext as _;
    use gpui_kit::TestAppContext;
    use std::sync::Arc;
    use std::time::Duration;
    use sync_core::connection::{Attention, AuthMethod, ConnectionState, Secret, SignIn};
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

    /// The one bridge from a refusal the engine answered on its own thread back
    /// to the model the tray and the dashboard read (R1.4).
    #[gpui_kit::test]
    async fn a_refusal_the_engine_answered_reaches_the_model_on_refresh(cx: &mut TestAppContext) {
        let (_runtime, server) = mock_server();
        cx.executor().allow_parking();
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

        // What the engine's 401 handling does, off this model's thread: the one
        // re-login, then the refusal that needs the user.
        let session = cx
            .read(|cx| connection.read(cx).session())
            .expect("a live session");
        for _ in 0..2 {
            let _ = cx.update(|_| {
                worker()
                    .expect("the worker runtime starts in tests")
                    .block_on(session.handle_unauthorized())
            });
        }
        let before = cx.read(|cx| connection.read(cx).state().clone());
        assert!(
            before.is_connected(),
            "the model has not heard about the refusal yet: {before:?}"
        );

        cx.update(|cx| connection.update(cx, |model, cx| model.refresh_state(cx)));

        assert_eq!(
            cx.read(|cx| connection.read(cx).state().clone()),
            ConnectionState::NeedsAttention(Attention::Rejected),
            "R1.4: the tray reads the refusal the engine answered as needing attention"
        );
    }
}
