//! The connection session (SPEC R1.1 to R1.6).
//!
//! The session owns the credential and the in-memory session token, and it
//! models the transitions as plain data so the shell, the tray, and tests share
//! one state machine. The shell never sees the credential; a request path asks
//! the session for a token (or the API key) through [`Connection::token`] and
//! [`Connection::credential`], and reports a 401 back through
//! [`Connection::handle_unauthorized`].
//!
//! The session token stays in memory only. It is never written to the settings
//! file and never handed to the credential store.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use immich_client::{Auth, ClientError, ImmichClient, SessionToken};
use serde::{Deserialize, Serialize};

pub use crate::credential::Secret;
use crate::credential::{CredentialError, CredentialStore};

/// How the client authenticates (R1.1, R1.5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// Email address and password (R1.1).
    #[default]
    Password,
    /// A long-lived Immich API key (R1.5).
    ApiKey,
}

/// The saved connection settings. It holds no secret: the password or the API
/// key lives in the credential store (R1.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerProfile {
    /// The Immich server base URL.
    pub server_url: String,
    /// The account's email address.
    pub email: String,
    /// Which credential the account uses.
    pub method: AuthMethod,
}

impl ServerProfile {
    /// Whether the profile names a server and an account.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.server_url.trim().is_empty() && !self.email.trim().is_empty()
    }
}

/// Why the connection needs the user's attention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attention {
    /// The server rejected the email and password.
    WrongPassword,
    /// The server has password login disabled; an API key is the way in (R1.6).
    PasswordLoginDisabled,
    /// The server rejected the API key.
    InvalidApiKey,
    /// The server refused the session again after the one re-login (R1.4).
    Rejected,
    /// The server could not be reached.
    Unreachable(String),
    /// The server answered in a form the client cannot use.
    Server(String),
    /// The OS credential store is missing or failed.
    CredentialStore(String),
}

impl Attention {
    /// The user-facing message for this state.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::WrongPassword => {
                "The server rejected the password. Check the email address and the password, then \
                 sign in again."
                    .to_string()
            }
            Self::PasswordLoginDisabled => {
                "This server has password login disabled. Sign in with an API key instead (admin \
                 settings, then API keys in Immich)."
                    .to_string()
            }
            Self::InvalidApiKey => {
                "The server rejected the API key. Create a new key in Immich and sign in again."
                    .to_string()
            }
            Self::Rejected => {
                "The server refused the session again after a fresh sign-in. Check the credential \
                 and the server, then sign in again."
                    .to_string()
            }
            Self::Unreachable(detail) => {
                format!("The server could not be reached: {detail}")
            }
            Self::Server(detail) => {
                format!("The server answered with an error: {detail}")
            }
            Self::CredentialStore(detail) => detail.clone(),
        }
    }

    /// Whether the message points the user at the API-key path (R1.6).
    #[must_use]
    pub fn needs_api_key(&self) -> bool {
        matches!(self, Self::PasswordLoginDisabled)
    }
}

impl From<&ClientError> for Attention {
    fn from(error: &ClientError) -> Self {
        match error {
            ClientError::WrongCredentials => Self::WrongPassword,
            ClientError::PasswordLoginDisabled => Self::PasswordLoginDisabled,
            ClientError::InvalidApiKey => Self::InvalidApiKey,
            ClientError::InvalidToken => Self::Rejected,
            ClientError::Unreachable(detail) => Self::Unreachable(detail.to_string()),
            ClientError::Protocol {
                status, message, ..
            } => Self::Server(format!("HTTP {status}: {message}")),
            ClientError::InvalidUrl(detail) => Self::Server(detail.clone()),
            ClientError::HttpClient(detail) => Self::Server(detail.to_string()),
            // `ClientError` is `#[non_exhaustive]`: a newer client error still
            // reaches the user instead of failing to compile.
            other => Self::Server(other.to_string()),
        }
    }
}

impl From<&CredentialError> for Attention {
    fn from(error: &CredentialError) -> Self {
        Self::CredentialStore(error.to_string())
    }
}

/// The connection state the tray, the dashboard, and the settings surface show
/// (R8.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ConnectionState {
    /// No server or credential is saved yet.
    #[default]
    NotConfigured,
    /// A sign-in is running.
    Connecting,
    /// The client holds a usable credential.
    Connected,
    /// The user must act before the client can connect.
    NeedsAttention(Attention),
}

impl ConnectionState {
    /// A short label for a status card or a tray tooltip.
    #[must_use]
    pub fn summary(&self) -> &'static str {
        match self {
            Self::NotConfigured => "Not signed in",
            Self::Connecting => "Signing in…",
            Self::Connected => "Connected",
            Self::NeedsAttention(_) => "Needs attention",
        }
    }

    /// The full user-facing line: the summary, plus the reason when there is one.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::NeedsAttention(attention) => {
                format!("{}: {}", self.summary(), attention.message())
            }
            _ => self.summary().to_string(),
        }
    }

    /// Whether the client holds a usable credential.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// The credential a request path attaches to an authenticated request.
#[derive(Clone, PartialEq, Eq)]
pub enum RequestCredential {
    /// A session token from `POST /auth/login` (R1.3).
    Session(SessionToken),
    /// An API key, sent as `x-api-key` (R1.5).
    ApiKey(Secret),
}

impl RequestCredential {
    /// The credential for one Immich client request (R1.3, R1.5).
    #[must_use]
    pub fn auth(&self) -> Auth<'_> {
        match self {
            Self::Session(token) => Auth::Token(token),
            Self::ApiKey(secret) => Auth::ApiKey(secret.expose()),
        }
    }
}

impl std::fmt::Debug for RequestCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session(token) => write!(formatter, "Session({token:?})"),
            Self::ApiKey(_) => formatter.write_str("ApiKey(<redacted>)"),
        }
    }
}

/// The user's sign-in input, straight from the connection form.
#[derive(Clone, Debug)]
pub struct SignIn {
    /// The account's email address.
    pub email: String,
    /// The password or the API key. `Debug` is redacted.
    pub secret: Secret,
    /// Which credential the secret is.
    pub method: AuthMethod,
}

/// What a request path does after a 401.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unauthorized {
    /// The session re-authenticated; retry the request once.
    Retry,
    /// The session cannot answer the refusal; do not retry. The connection
    /// shows the needs-attention state once the run of failed requests reaches
    /// the attention threshold (R1.4, R6.6).
    Stop,
}

/// How many failed requests in a row a connection waits out before it needs
/// the user (R1.4, R6.6).
///
/// The run is the same notion the
/// [`CircuitBreaker`](crate::breaker::CircuitBreaker) counts: a failure
/// extends it, a success resets it to zero. A session no engine owns keeps
/// this value of one: a sign-in form has no queue to protect and must report
/// the refusal it is waiting on. The engine raises the length to its
/// `EngineConfig::attention_threshold`, the tray's own run; that is separate
/// from the breaker's longer `breaker_threshold`, because the breaker gates
/// the queue, not the tray.
const DEFAULT_ATTENTION_THRESHOLD: u32 = 1;

/// The connection session: login at startup, one re-login after a 401, and the
/// needs-attention state once a run of failed requests reaches the attention
/// threshold (R1.3, R1.4, R6.6).
pub struct Connection {
    client: ImmichClient,
    store: Arc<dyn CredentialStore>,
    profile: ServerProfile,
    state: ConnectionState,
    credential: Option<RequestCredential>,
    /// Whether the one re-login of this session has already been used.
    ///
    /// It is shared with [`SharedSession`], so a successful request can restore
    /// the budget without waiting for a request that is in flight (R1.4).
    relogin_used: Arc<AtomicBool>,
    /// The failed requests no success has reset, shared with [`SharedSession`]
    /// so the run can be read and reset without the lock (R1.4, R6.6).
    failure_run: Arc<AtomicU32>,
    /// How many failures in a row reach the user; see
    /// [`SharedSession::set_attention_threshold`].
    attention_threshold: Arc<AtomicU32>,
}

impl Connection {
    /// Builds a session for one server. The state stays `NotConfigured` until
    /// [`Self::restore`] or [`Self::sign_in`] runs.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] when the server URL cannot be used.
    pub fn new(server_url: &str, store: Arc<dyn CredentialStore>) -> Result<Self, ClientError> {
        Self::with_budget(
            server_url,
            store,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU32::new(DEFAULT_ATTENTION_THRESHOLD)),
        )
    }

    /// Builds a session that spends a re-login budget a handle already shares,
    /// so a session replaced inside a live [`SharedSession`] keeps one budget
    /// with it (R1.4).
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] when the server URL cannot be used.
    fn with_budget(
        server_url: &str,
        store: Arc<dyn CredentialStore>,
        relogin_used: Arc<AtomicBool>,
        failure_run: Arc<AtomicU32>,
        attention_threshold: Arc<AtomicU32>,
    ) -> Result<Self, ClientError> {
        let client = ImmichClient::new(server_url)?;
        relogin_used.store(false, Ordering::SeqCst);
        failure_run.store(0, Ordering::SeqCst);
        Ok(Self {
            client,
            store,
            profile: ServerProfile {
                server_url: server_url.trim().to_string(),
                ..ServerProfile::default()
            },
            state: ConnectionState::NotConfigured,
            credential: None,
            relogin_used,
            failure_run,
            attention_threshold,
        })
    }

    /// The client, for the operations the sync engine sends.
    #[must_use]
    pub fn client(&self) -> &ImmichClient {
        &self.client
    }

    /// The saved profile this session was restored from.
    #[must_use]
    pub fn profile(&self) -> &ServerProfile {
        &self.profile
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    /// The credential a request path attaches, when one is available.
    #[must_use]
    pub fn credential(&self) -> Option<&RequestCredential> {
        self.credential.as_ref()
    }

    /// The session token, when the session uses password login (R1.3).
    #[must_use]
    pub fn token(&self) -> Option<&SessionToken> {
        match &self.credential {
            Some(RequestCredential::Session(token)) => Some(token),
            _ => None,
        }
    }

    /// Restores the session at startup from stored settings and the stored
    /// credential, with no user input (R1.3).
    pub async fn restore(&mut self, profile: &ServerProfile) -> ConnectionState {
        self.profile = profile.clone();
        self.credential = None;
        self.relogin_used.store(false, Ordering::SeqCst);
        self.failure_run.store(0, Ordering::SeqCst);
        if !profile.is_configured() {
            self.state = ConnectionState::NotConfigured;
            return self.state.clone();
        }
        self.state = ConnectionState::Connecting;
        match self.store.get() {
            Ok(Some(secret)) => {
                self.authenticate(profile.method, &secret).await;
            }
            Ok(None) => {
                self.state = ConnectionState::NotConfigured;
            }
            Err(error) => self.need_attention(&error),
        }
        self.state.clone()
    }

    /// Signs in with the form's email and secret, and stores the secret in the
    /// OS credential store only after the server accepts it (R1.2, R1.5).
    pub async fn sign_in(&mut self, sign_in: SignIn) -> ConnectionState {
        self.profile.email = sign_in.email.clone();
        self.profile.method = sign_in.method;
        self.credential = None;
        self.relogin_used.store(false, Ordering::SeqCst);
        self.failure_run.store(0, Ordering::SeqCst);
        self.state = ConnectionState::Connecting;
        self.authenticate(sign_in.method, &sign_in.secret).await;
        if self.state.is_connected()
            && let Err(error) = self.store.set(&sign_in.secret)
        {
            // The credential is not persisted, so a restart could not log in
            // again. Say so instead of pretending the sign-in finished.
            self.credential = None;
            self.need_attention(&error);
        }
        self.state.clone()
    }

    /// Answers a 401 from a request the session authenticated (R1.4).
    ///
    /// The first 401 re-logs in once and returns [`Unauthorized::Retry`]; a
    /// later 401 before any request succeeded returns [`Unauthorized::Stop`].
    /// The refusal reaches the needs-attention state only once the run of
    /// failed requests reaches the attention threshold, so one unlucky refusal
    /// does not worry the user (R6.6).
    pub async fn handle_unauthorized(&mut self) -> Unauthorized {
        // Every 401 is one failed request in the run; the first success
        // resets it. The run is only read when the client must give up, so a
        // 401 the re-login can answer is not by itself an alarm.
        let run = self.record_failure();
        if self.relogin_used.load(Ordering::SeqCst) || self.credential.is_none() {
            self.refuse(run);
            return Unauthorized::Stop;
        }
        match self.profile.method {
            AuthMethod::Password => match self.store.get() {
                Ok(Some(secret)) => self.authenticate(AuthMethod::Password, &secret).await,
                Ok(None) => self.refuse(run),
                Err(error) => self.need_attention(&error),
            },
            AuthMethod::ApiKey => match self.credential.clone() {
                Some(RequestCredential::ApiKey(key)) => {
                    self.authenticate(AuthMethod::ApiKey, &key).await;
                }
                _ => self.refuse(run),
            },
        }
        if self.state.is_connected() {
            self.relogin_used.store(true, Ordering::SeqCst);
            Unauthorized::Retry
        } else {
            Unauthorized::Stop
        }
    }

    /// Records that a request with the current credential succeeded, which
    /// restores the one re-login budget and ends the failure run (R1.4, R6.6).
    pub fn note_authenticated(&self) {
        self.relogin_used.store(false, Ordering::SeqCst);
        self.failure_run.store(0, Ordering::SeqCst);
    }

    /// Extends the run of failed requests and returns its new length (R1.4,
    /// R6.6).
    fn record_failure(&self) -> u32 {
        self.failure_run
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1)
    }

    /// Whether a run of failed requests has reached the point the user must be
    /// told (R1.4, R6.6).
    fn threshold_reached(&self, run: u32) -> bool {
        run >= self.attention_threshold.load(Ordering::SeqCst)
    }

    /// Enters the needs-attention state once the run of failed requests reaches
    /// the threshold (R1.4, R6.6).
    ///
    /// Below the threshold the credential is kept and the connection stays
    /// usable: the refusal has not become a run yet.
    fn refuse(&mut self, run: u32) {
        if self.threshold_reached(run) {
            self.credential = None;
            self.state = ConnectionState::NeedsAttention(Attention::Rejected);
        }
    }

    /// Enters the needs-attention state with the transport reason (R6.6,
    /// R8.2).
    ///
    /// The credential is kept: the server may come back, and the probe needs
    /// it. A refusal or a sign-in failure is the stronger reason and is not
    /// overwritten.
    fn need_unreachable(&mut self, detail: &str) {
        if matches!(
            self.state,
            ConnectionState::Connected | ConnectionState::NeedsAttention(Attention::Unreachable(_))
        ) {
            self.state =
                ConnectionState::NeedsAttention(Attention::Unreachable(detail.to_string()));
        }
    }

    /// Records a request the server answered: the run resets, and a connection
    /// that only needed attention because the server was unreachable recovers
    /// (R1.4, R6.6).
    pub fn note_reachable(&mut self) {
        self.note_authenticated();
        if matches!(
            self.state,
            ConnectionState::NeedsAttention(Attention::Unreachable(_))
        ) {
            self.state = ConnectionState::Connected;
        }
    }

    /// Records one more refusal against the run (R1.4, R6.6), for a probe
    /// that learns the refusal outside [`Connection::handle_unauthorized`].
    pub(crate) fn reject(&mut self) {
        let run = self.record_failure();
        self.refuse(run);
    }

    /// The same session, shared between the app's surfaces and the sync engine
    /// (R1.4).
    ///
    /// The engine sends its requests through [`SharedSession::request`], so a
    /// 401 on an upload is answered by this one state machine rather than by a
    /// second copy of the rule.
    #[must_use]
    pub fn shared(self) -> SharedSession {
        let snapshot = SessionSnapshot {
            state: self.state.clone(),
            credential: self.credential.clone(),
            client: self.client.clone(),
        };
        SharedSession {
            snapshot: Arc::new(std::sync::Mutex::new(snapshot)),
            relogin_used: Arc::clone(&self.relogin_used),
            failure_run: Arc::clone(&self.failure_run),
            attention_threshold: Arc::clone(&self.attention_threshold),
            auth_epoch: Arc::new(AtomicU32::new(0)),
            connection: Arc::new(tokio::sync::Mutex::new(self)),
        }
    }

    /// Authenticates with `secret` and records the outcome.
    async fn authenticate(&mut self, method: AuthMethod, secret: &Secret) {
        match method {
            AuthMethod::Password => {
                match self
                    .client
                    .login(&self.profile.email, secret.expose())
                    .await
                {
                    Ok(login) => {
                        self.credential = Some(RequestCredential::Session(login.access_token));
                        self.state = ConnectionState::Connected;
                    }
                    Err(error) => self.need_attention(&error),
                }
            }
            AuthMethod::ApiKey => match self.client.validate_api_key(secret.expose()).await {
                Ok(()) => {
                    self.credential = Some(RequestCredential::ApiKey(secret.clone()));
                    self.state = ConnectionState::Connected;
                }
                Err(error) => self.need_attention(&error),
            },
        }
    }

    /// Moves to the needs-attention state with the reason mapped for the user.
    fn need_attention(&mut self, error: &impl AttentionSource) {
        self.credential = None;
        self.state = ConnectionState::NeedsAttention(error.attention());
    }
}

/// An error that maps to an [`Attention`] reason.
trait AttentionSource {
    fn attention(&self) -> Attention;
}

impl AttentionSource for ClientError {
    fn attention(&self) -> Attention {
        Attention::from(self)
    }
}

impl AttentionSource for CredentialError {
    fn attention(&self) -> Attention {
        Attention::from(self)
    }
}

/// The state and credential of a session as of its last change.
#[derive(Clone, Debug)]
struct SessionSnapshot {
    state: ConnectionState,
    credential: Option<RequestCredential>,
    /// The client of the connection the snapshot describes, so a request path
    /// reaches the server this session points at.
    client: ImmichClient,
}

/// One connection session, shared between the app's surfaces and the sync
/// engine (R1.4).
///
/// The engine needs the credential a request attaches, and it needs a 401 to be
/// answered by the same one-re-login rule the settings form uses. Both live
/// here, behind one lock, and a snapshot lets a synchronous reader (the tray,
/// the dashboard) see the state without waiting on a request in flight.
#[derive(Clone)]
pub struct SharedSession {
    connection: Arc<tokio::sync::Mutex<Connection>>,
    /// The one re-login budget, so a successful request restores it without
    /// taking the lock (R1.4).
    relogin_used: Arc<AtomicBool>,
    /// The failed requests no success has reset (R1.4, R6.6).
    failure_run: Arc<AtomicU32>,
    /// How many failures in a row reach the user; see
    /// [`Self::set_attention_threshold`].
    attention_threshold: Arc<AtomicU32>,
    /// How many sign-ins, restores, and re-logins this handle has made
    /// (R1.4). A reader that settled an answer against a credential, such as
    /// the sync tag's permission probe (R3.7), compares it to learn when that
    /// answer is stale.
    auth_epoch: Arc<AtomicU32>,
    snapshot: Arc<std::sync::Mutex<SessionSnapshot>>,
}

impl std::fmt::Debug for SharedSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The session holds a credential; only its state is safe to print.
        formatter
            .debug_struct("SharedSession")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl SharedSession {
    /// The HTTP client, which is cheap to clone.
    #[must_use]
    pub fn client(&self) -> ImmichClient {
        self.snapshot().client
    }

    /// Whether both handles wrap the same session state, so a form and the
    /// engine cannot drift into two session state machines (R1.4).
    #[must_use]
    pub fn shares_state_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.connection, &other.connection)
    }

    /// Points this session at another server and credential store, in place.
    ///
    /// The handle is never swapped, because the sync engine holds the handle it
    /// was constructed with: a form sign-in has to land on that session or the
    /// engine's queue keeps the rejected state and uploads nothing (R1.4).
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] when the server URL cannot be used. The
    /// session then reads as needing attention, so the tray cannot keep
    /// reporting the state this attempt replaced.
    pub async fn repoint(
        &self,
        server_url: &str,
        store: Arc<dyn CredentialStore>,
    ) -> Result<(), ClientError> {
        let mut connection = self.connection.lock().await;
        let outcome = match Connection::with_budget(
            server_url,
            store,
            Arc::clone(&self.relogin_used),
            Arc::clone(&self.failure_run),
            Arc::clone(&self.attention_threshold),
        ) {
            Ok(fresh) => {
                *connection = fresh;
                Ok(())
            }
            Err(error) => {
                connection.need_attention(&error);
                Err(error)
            }
        };
        self.mirror(&connection);
        self.auth_epoch.fetch_add(1, Ordering::SeqCst);
        outcome
    }

    /// The connection state as of the last change (R1.4).
    #[must_use]
    pub fn state(&self) -> ConnectionState {
        self.snapshot().state
    }

    /// The credential a request attaches right now (R1.3, R1.5).
    #[must_use]
    pub fn credential(&self) -> Option<RequestCredential> {
        self.snapshot().credential
    }

    /// Signs in with the form's email and secret (R1.2, R1.5).
    pub async fn sign_in(&self, request: SignIn) -> ConnectionState {
        let mut connection = self.connection.lock().await;
        let state = connection.sign_in(request).await;
        self.mirror(&connection);
        self.auth_epoch.fetch_add(1, Ordering::SeqCst);
        state
    }

    /// Restores the session at startup with no user input (R1.3).
    pub async fn restore(&self, profile: &ServerProfile) -> ConnectionState {
        let mut connection = self.connection.lock().await;
        let state = connection.restore(profile).await;
        self.mirror(&connection);
        self.auth_epoch.fetch_add(1, Ordering::SeqCst);
        state
    }

    /// Answers a 401 with the one re-login of this session (R1.4).
    pub async fn handle_unauthorized(&self) -> Unauthorized {
        let mut connection = self.connection.lock().await;
        let outcome = connection.handle_unauthorized().await;
        self.mirror(&connection);
        self.auth_epoch.fetch_add(1, Ordering::SeqCst);
        outcome
    }

    /// How many times an authentication established a credential on this
    /// handle (R1.4). A sign-in reuses the running session, so a reader that
    /// settled an answer against the previous credential compares this value
    /// to know the answer is stale.
    #[must_use]
    pub fn auth_epoch(&self) -> u32 {
        self.auth_epoch.load(Ordering::SeqCst)
    }

    /// Records that a request with the current credential succeeded, which
    /// restores the one re-login budget and ends the failure run (R1.4, R6.6).
    pub fn note_authenticated(&self) {
        self.relogin_used.store(false, Ordering::SeqCst);
        self.failure_run.store(0, Ordering::SeqCst);
    }

    /// Sets how many failed requests in a row the connection waits out before
    /// it needs the user (R1.4, R6.6).
    ///
    /// The engine sets its `EngineConfig::attention_threshold` here, its own
    /// run for the tray. The circuit breaker keeps its separate
    /// `EngineConfig::breaker_threshold`: the two decide different things, so
    /// they no longer share a length. A session no engine owns keeps
    /// `DEFAULT_ATTENTION_THRESHOLD`.
    pub fn set_attention_threshold(&self, threshold: u32) {
        self.attention_threshold
            .store(threshold.max(1), Ordering::SeqCst);
    }

    /// Records one request the server could not answer (R6.6).
    ///
    /// The run ends at the next success; when it reaches the threshold the
    /// connection needs the user, with `detail` as the transport reason. The
    /// credential is kept, so a server that comes back can be probed and
    /// resume without a sign-in.
    pub async fn note_unreachable(&self, detail: &str) {
        let mut connection = self.connection.lock().await;
        let run = connection.record_failure();
        if connection.threshold_reached(run) {
            connection.need_unreachable(detail);
        }
        self.mirror(&connection);
    }

    /// Records a request the server answered: the run resets, and a connection
    /// that only needed attention because the server was unreachable recovers
    /// (R1.4, R6.6). A refused credential is not cleared: only a sign-in fixes
    /// that.
    pub async fn note_reachable(&self) {
        self.failure_run.store(0, Ordering::SeqCst);
        if !matches!(
            self.state(),
            ConnectionState::NeedsAttention(Attention::Unreachable(_))
        ) {
            return;
        }
        let mut connection = self.connection.lock().await;
        connection.note_reachable();
        self.mirror(&connection);
    }

    /// Runs one authenticated request, answering a 401 with the session's one
    /// re-login and retrying it once (R1.4).
    ///
    /// A second 401 returns the refusal, one more failure in the run (R1.4).
    /// The connection enters the needs-attention state only once the run
    /// reaches the attention threshold, so one refusal below it leaves the
    /// credential in place. A request that succeeds restores the re-login
    /// budget and resets the run.
    ///
    /// # Errors
    ///
    /// Returns the request's own failure. A refusal the session cannot answer
    /// comes back as [`ClientError::InvalidToken`].
    pub async fn request<T, F, Fut>(&self, mut run: F) -> Result<T, ClientError>
    where
        F: FnMut(RequestCredential) -> Fut,
        Fut: std::future::Future<Output = Result<T, ClientError>>,
    {
        let Some(credential) = self.credential() else {
            // No credential is not a server refusal; the session is already in
            // the needs-attention state, so it reads as one to the caller.
            let _ = self.handle_unauthorized().await;
            return Err(ClientError::InvalidToken);
        };
        match run(credential).await {
            Ok(value) => {
                self.note_authenticated();
                Ok(value)
            }
            Err(ClientError::InvalidToken) => {
                if self.handle_unauthorized().await != Unauthorized::Retry {
                    return Err(ClientError::InvalidToken);
                }
                let Some(fresh) = self.credential() else {
                    return Err(ClientError::InvalidToken);
                };
                match run(fresh).await {
                    Ok(value) => {
                        self.note_authenticated();
                        Ok(value)
                    }
                    Err(ClientError::InvalidToken) => {
                        // The second 401 of R1.4: the one re-login is spent, so
                        // the refusal is recorded against the run. The
                        // connection needs the user only once that run reaches
                        // the attention threshold.
                        let _ = self.handle_unauthorized().await;
                        Err(ClientError::InvalidToken)
                    }
                    Err(error) => {
                        self.note_request_error(&error).await;
                        Err(error)
                    }
                }
            }
            Err(error) => {
                self.note_request_error(&error).await;
                Err(error)
            }
        }
    }

    /// Records a request the server could not answer, before the caller sees
    /// the error (R6.6).
    ///
    /// Every request path funnels through [`Self::request`], so this is the one
    /// place a connection failure extends the run of failures that reaches the
    /// attention threshold. A refusal is not a connection failure: the 401
    /// path feeds the run through [`Connection::handle_unauthorized`].
    async fn note_request_error(&self, error: &ClientError) {
        if let ClientError::Unreachable(detail) = error {
            self.note_unreachable(&detail.to_string()).await;
        }
    }

    /// Probes the server with the credential-validation endpoint (R6.6).
    ///
    /// The probe is not an upload attempt: it does not spend the re-login
    /// budget and it does not answer a 401 with a re-login. It is how the
    /// circuit breaker asks whether the server is back before the queue
    /// resumes.
    ///
    /// # Errors
    ///
    /// Returns the transport and protocol variants, and
    /// [`ClientError::InvalidToken`] when no credential is available.
    pub async fn validate(&self) -> Result<(), ClientError> {
        let snapshot = self.snapshot();
        let client = snapshot.client;
        let probe = match snapshot.credential {
            Some(RequestCredential::Session(token)) => client.validate_token(&token).await,
            Some(RequestCredential::ApiKey(key)) => client.validate_api_key(key.expose()).await,
            None => Err(ClientError::InvalidToken),
        };
        // The probe is the one path that learns a refusal without going through
        // `request`, so it records it here: the snapshot the tray reads must
        // not keep saying Connected while nothing can authenticate (R1.4).
        if let Err(error) = &probe
            && matches!(
                error,
                ClientError::InvalidToken | ClientError::InvalidApiKey
            )
        {
            let mut connection = self.connection.lock().await;
            connection.reject();
            self.mirror(&connection);
        }
        probe
    }

    /// The snapshot of the last change.
    fn snapshot(&self) -> SessionSnapshot {
        self.snapshot
            .lock()
            .expect("the session snapshot lock")
            .clone()
    }

    /// Copies the session's state into the snapshot the UI reads.
    fn mirror(&self, connection: &Connection) {
        let mut snapshot = self.snapshot.lock().expect("the session snapshot lock");
        snapshot.state = connection.state().clone();
        snapshot.credential = connection.credential().cloned();
        snapshot.client = connection.client().clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_credential_debug_is_redacted() {
        // The exact string, not `contains`: `Secret` and `SessionToken` redact
        // their own `Debug`, so a derived `RequestCredential` would still pass
        // a substring check while leaking the shape of the credential.
        let api_key = RequestCredential::ApiKey(Secret::new("api-key-abc123"));
        assert_eq!(format!("{api_key:?}"), "ApiKey(<redacted>)");

        let token: SessionToken =
            serde_json::from_str("\"session-token-abc123\"").expect("a token deserializes");
        let session = RequestCredential::Session(token);
        assert_eq!(format!("{session:?}"), "Session(SessionToken(<redacted>))");
    }

    #[test]
    fn sign_in_debug_is_redacted() {
        // The form's request is the one value that holds the secret and could
        // reach a log through a `{:?}` in a future event.
        let request = SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        };

        let text = format!("{request:?}");
        assert!(
            !text.contains("correct horse"),
            "a debug dump of the sign-in request leaks the secret: {text}"
        );
        assert!(text.contains("Secret(<redacted>)"), "{text}");
    }
}
