//! Typed client failures (SPEC N3, R1.4, R1.6).
//!
//! The variants carry the distinctions the session and the retry policy act on:
//! a rejected credential, a server with password login disabled, an unreachable
//! server, and a response the client cannot use. Nothing here carries the
//! password, the API key, or the session token.

use std::borrow::Cow;

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
    /// The server cannot accept this format (HTTP 415, R6.3).
    #[error("the server does not accept this file format: {0}")]
    UnsupportedFormat(String),
    /// The account is over its storage quota (HTTP 400 with Immich's quota
    /// message, R6.4).
    #[error("the account is over its storage quota: {0}")]
    QuotaExceeded(String),
    /// The credential lacks the required permission (HTTP 403, R6.3).
    #[error("the credential lacks the required permission: {0}")]
    PermissionDenied(String),
    /// The file is larger than the server accepts (HTTP 413, R6.3).
    #[error("the file is too large for the server: {0}")]
    TooLarge(String),
    /// The local file could not be read (R6.4). It is not a network failure,
    /// so the retry policy must not treat it like one.
    #[error("the local file could not be read: {0}")]
    LocalRead(#[source] std::io::Error),
    /// The server answered with a status or body the client does not understand.
    /// A permanent 4xx that has no named class of its own (404, 409, 422, and a
    /// 400 that is not Immich's quota refusal) also lands here.
    #[error("the server answered {status} in a form this client does not understand: {message}")]
    Protocol {
        /// The HTTP status the server returned.
        status: u16,
        /// The server's message, or a short description of the unusable body.
        message: String,
    },
}

impl ClientError {
    /// The label a log line records: the variant and, for a protocol failure,
    /// its status.
    ///
    /// The server's own message is never included. Several variants quote it,
    /// and a broken or hostile server can fill it with a submitted credential,
    /// so only this label may reach a log (SPEC N3, R10.3).
    #[must_use]
    pub fn redacted_reason(&self) -> Cow<'static, str> {
        match self {
            Self::InvalidUrl(_) => Cow::Borrowed("the server URL is not valid"),
            Self::HttpClient(_) => Cow::Borrowed("the HTTP client could not be built"),
            Self::Unreachable(_) => Cow::Borrowed("the server could not be reached"),
            Self::WrongCredentials => Cow::Borrowed("the server rejected the password"),
            Self::PasswordLoginDisabled => Cow::Borrowed("password login is disabled"),
            Self::InvalidApiKey => Cow::Borrowed("the server rejected the API key"),
            Self::InvalidToken => Cow::Borrowed("the server rejected the session token"),
            Self::UnsupportedFormat(_) => {
                Cow::Borrowed("the server does not accept this file format")
            }
            Self::QuotaExceeded(_) => Cow::Borrowed("the account is over its storage quota"),
            Self::PermissionDenied(_) => {
                Cow::Borrowed("the credential lacks the required permission")
            }
            Self::TooLarge(_) => Cow::Borrowed("the file is too large for the server"),
            Self::LocalRead(_) => Cow::Borrowed("the local file could not be read"),
            Self::Protocol { status, .. } => Cow::Owned(format!(
                "the server answered {status} in a form this client does not understand"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ClientError;

    #[test]
    fn a_server_message_never_reaches_the_redacted_reason() {
        let secret = "hunter2-echo";
        let errors = [
            ClientError::UnsupportedFormat(secret.to_string()),
            ClientError::QuotaExceeded(secret.to_string()),
            ClientError::PermissionDenied(secret.to_string()),
            ClientError::TooLarge(secret.to_string()),
            ClientError::Protocol {
                status: 500,
                message: secret.to_string(),
            },
        ];
        for error in errors {
            let reason = error.redacted_reason();
            assert!(
                !reason.contains(secret),
                "the reason for an error quotes the server: {reason}"
            );
        }
        assert!(
            ClientError::Protocol {
                status: 503,
                message: String::new(),
            }
            .redacted_reason()
            .contains("503"),
            "the status stays in the reason"
        );
    }
}
