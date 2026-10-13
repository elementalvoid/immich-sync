//! The wire types of the Stable operations this client covers.

use std::fmt;

use serde::Deserialize;

/// The opaque Immich session token (GLOSSARY).
///
/// The token is a secret. Its `Debug` output is redacted, and the value is only
/// reachable through [`SessionToken::expose`], so a stray log cannot leak it.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct SessionToken(String);

impl SessionToken {
    /// The token value, for the `Authorization` header.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionToken(<redacted>)")
    }
}

/// The response of `POST /auth/login`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    /// The session token (R1.3).
    pub access_token: SessionToken,
    /// The Immich user id.
    pub user_id: String,
    /// The account's email address.
    pub user_email: String,
    /// The account's display name.
    pub name: String,
    /// Whether the account is an admin.
    pub is_admin: bool,
    /// Whether the account finished onboarding.
    pub is_onboarded: bool,
}

/// The response of `GET /server/about`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ServerAbout {
    /// The Immich server version, for example `v3.3.1`.
    pub version: String,
}

/// The response of `POST /auth/validateToken`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthStatus {
    pub(crate) auth_status: bool,
}
