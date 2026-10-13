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
            ClientError::Protocol { status, message } => {
                Self::Server(format!("HTTP {status}: {message}"))
            }
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
    /// The connection needs attention; do not retry.
    Stop,
}

/// The connection session: login at startup, one re-login after a 401, and the
/// needs-attention state after a second 401 (R1.3, R1.4).
pub struct Connection {
    client: ImmichClient,
    store: Arc<dyn CredentialStore>,
    profile: ServerProfile,
    state: ConnectionState,
    credential: Option<RequestCredential>,
    /// Whether the one re-login of this session has already been used.
    relogin_used: bool,
}

impl Connection {
    /// Builds a session for one server. The state stays `NotConfigured` until
    /// [`Self::restore`] or [`Self::sign_in`] runs.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError`] when the server URL cannot be used.
    pub fn new(server_url: &str, store: Arc<dyn CredentialStore>) -> Result<Self, ClientError> {
        let client = ImmichClient::new(server_url)?;
        Ok(Self {
            client,
            store,
            profile: ServerProfile {
                server_url: server_url.trim().to_string(),
                ..ServerProfile::default()
            },
            state: ConnectionState::NotConfigured,
            credential: None,
            relogin_used: false,
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
        self.relogin_used = false;
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
        self.relogin_used = false;
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
    /// second 401 before any request succeeded returns [`Unauthorized::Stop`]
    /// and leaves the session in the needs-attention state.
    pub async fn handle_unauthorized(&mut self) -> Unauthorized {
        if self.relogin_used || self.credential.is_none() {
            self.state = ConnectionState::NeedsAttention(Attention::Rejected);
            return Unauthorized::Stop;
        }
        match self.profile.method {
            AuthMethod::Password => match self.store.get() {
                Ok(Some(secret)) => self.authenticate(AuthMethod::Password, &secret).await,
                Ok(None) => self.state = ConnectionState::NeedsAttention(Attention::Rejected),
                Err(error) => self.need_attention(&error),
            },
            AuthMethod::ApiKey => match self.credential.clone() {
                Some(RequestCredential::ApiKey(key)) => {
                    self.authenticate(AuthMethod::ApiKey, &key).await;
                }
                _ => {
                    self.state = ConnectionState::NeedsAttention(Attention::InvalidApiKey);
                }
            },
        }
        if self.state.is_connected() {
            self.relogin_used = true;
            Unauthorized::Retry
        } else {
            Unauthorized::Stop
        }
    }

    /// Records that a request with the current credential succeeded, which
    /// restores the one re-login budget (R1.4).
    pub fn note_authenticated(&mut self) {
        self.relogin_used = false;
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
