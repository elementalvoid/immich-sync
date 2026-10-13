//! Typed client failures (SPEC N3, R1.4, R1.6).
//!
//! The variants carry the distinctions the session and the retry policy act on:
//! a rejected credential, a server with password login disabled, an unreachable
//! server, and a response the client cannot use. Nothing here carries the
//! password, the API key, or the session token.

use thiserror::Error;

/// Why a request to the Immich server failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClientError {
    /// The server URL could not be parsed.
    #[error("the server URL is not valid: {0}")]
    InvalidUrl(String),
    /// The HTTP client could not be built.
    #[error("could not create the HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),
    /// The server could not be reached: connection, TLS, timeout, or transport.
    #[error("the server could not be reached: {0}")]
    Unreachable(#[source] reqwest::Error),
    /// The server rejected the email and password.
    #[error("the server rejected the password")]
    WrongCredentials,
    /// The server has password login disabled; an API key is the way in (R1.6).
    #[error("the server has password login disabled; sign in with an API key instead")]
    PasswordLoginDisabled,
    /// The server rejected the API key.
    #[error("the server rejected the API key")]
    InvalidApiKey,
    /// The server rejected the session token.
    #[error("the server rejected the session token")]
    InvalidToken,
    /// The server answered with a status or body the client does not understand.
    #[error("the server answered {status} in a form this client does not understand: {message}")]
    Protocol {
        /// The HTTP status the server returned.
        status: u16,
        /// The server's message, or a short description of the unusable body.
        message: String,
    },
}
