//! Failure classes and retry classification (SPEC R6.3, R6.4).
//!
//! The class list is the one the spec persists. #7 owns the backoff and the
//! attempt cap; this module only answers "which class is this" and "may another
//! attempt succeed", so the tracer bullet can store a truthful row and stop.

use immich_client::ClientError;

/// The persisted failure class (R6.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureClass {
    /// The credential was refused (401, R6.3).
    AuthFailed,
    /// The credential lacks the required permission (403).
    PermissionDenied,
    /// The account is over its storage quota (400 with Immich's message).
    QuotaExceeded,
    /// The server cannot accept this file type (415, or a bulk-check reject).
    UnsupportedFormat,
    /// A transport, timeout, or rate-limit failure (R6.1).
    NetworkTransient,
    /// The server answered 5xx (R6.1).
    ServerTransient,
    /// The local file could not be read.
    LocalReadError,
    /// The file is larger than the server accepts (413).
    TooLarge,
}

impl FailureClass {
    /// The exact string the state database stores.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AuthFailed => "auth_failed",
            Self::PermissionDenied => "permission_denied",
            Self::QuotaExceeded => "quota_exceeded",
            Self::UnsupportedFormat => "unsupported_format",
            Self::NetworkTransient => "network_transient",
            Self::ServerTransient => "server_transient",
            Self::LocalReadError => "local_read_error",
            Self::TooLarge => "too_large",
        }
    }

    /// Parses the stored string. A class a newer build wrote that this one does
    /// not know reads as `None` rather than as a wrong class.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "auth_failed" => Self::AuthFailed,
            "permission_denied" => Self::PermissionDenied,
            "quota_exceeded" => Self::QuotaExceeded,
            "unsupported_format" => Self::UnsupportedFormat,
            "network_transient" => Self::NetworkTransient,
            "server_transient" => Self::ServerTransient,
            "local_read_error" => Self::LocalReadError,
            "too_large" => Self::TooLarge,
            _ => return None,
        })
    }
}

/// Whether a failed attempt can succeed on a later one (R6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// A later attempt cannot succeed without a change (R6.3).
    Permanent,
    /// A later attempt may succeed (R6.1).
    Transient,
}

/// The class and retry kind for one client failure.
///
/// A permanent status that has no class of its own (404, 409, 422, and a 400
/// that is not the quota refusal) reports `None` as its class; the row still
/// records `failed_permanent`.
#[must_use]
pub fn classify(error: &ClientError) -> (Option<FailureClass>, FailureKind) {
    match error {
        ClientError::Unreachable(_) | ClientError::Stalled => {
            (Some(FailureClass::NetworkTransient), FailureKind::Transient)
        }
        ClientError::LocalRead(_) => (Some(FailureClass::LocalReadError), FailureKind::Permanent),
        ClientError::InvalidToken
        | ClientError::WrongCredentials
        | ClientError::PasswordLoginDisabled
        | ClientError::InvalidApiKey => (Some(FailureClass::AuthFailed), FailureKind::Permanent),
        ClientError::PermissionDenied(_) => {
            (Some(FailureClass::PermissionDenied), FailureKind::Permanent)
        }
        ClientError::QuotaExceeded(_) => {
            (Some(FailureClass::QuotaExceeded), FailureKind::Permanent)
        }
        ClientError::UnsupportedFormat(_) => (
            Some(FailureClass::UnsupportedFormat),
            FailureKind::Permanent,
        ),
        ClientError::TooLarge(_) => (Some(FailureClass::TooLarge), FailureKind::Permanent),
        // A URL or HTTP-client build failure is a configuration problem that a
        // retry cannot fix.
        ClientError::InvalidUrl(_) | ClientError::HttpClient(_) => (None, FailureKind::Permanent),
        ClientError::Protocol { status, .. } => match status {
            408 | 425 | 429 => (Some(FailureClass::NetworkTransient), FailureKind::Transient),
            500..=599 => (Some(FailureClass::ServerTransient), FailureKind::Transient),
            _ => (None, FailureKind::Permanent),
        },
        // `ClientError` is `#[non_exhaustive]`: a newer variant is not retried
        // by accident.
        _ => (None, FailureKind::Permanent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stalled_upload_is_a_transient_network_failure() {
        // The catch-all below turns a variant nobody classified into a
        // permanent failure, so each new one has to say which it is.
        assert_eq!(
            classify(&ClientError::Stalled),
            (Some(FailureClass::NetworkTransient), FailureKind::Transient)
        );
    }
}
