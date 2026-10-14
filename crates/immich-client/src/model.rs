//! The wire types of the Stable operations this client covers.

use std::fmt;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::bandwidth::BandwidthLimit;

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

/// How one request authenticates (R1.3, R1.5).
///
/// `Debug` is hand-written because the API-key arm would otherwise print the
/// key; a stray `{:?}` in a log must not leak it (N3).
#[derive(Clone, Copy)]
pub enum Auth<'a> {
    /// A session token from `POST /auth/login`, sent as a bearer token.
    Token(&'a SessionToken),
    /// An API key, sent as `x-api-key`.
    ApiKey(&'a str),
}

impl fmt::Debug for Auth<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Token(token) => write!(formatter, "Auth::Token({token:?})"),
            Self::ApiKey(_) => formatter.write_str("Auth::ApiKey(<redacted>)"),
        }
    }
}

/// One checksum to look up before uploading (R3.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkCheckItem {
    /// The caller's correlation id; the server echoes it back.
    pub id: String,
    /// The base64 or hex SHA-1 checksum.
    pub checksum: String,
}

/// The server's answer for one [`BulkCheckItem`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkCheckResult {
    /// The correlation id the request carried.
    pub id: String,
    /// Whether the server accepts the upload.
    pub action: BulkAction,
}

/// What the server wants the client to do with one checksum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BulkAction {
    /// The server does not have these bytes; upload them.
    Accept,
    /// The server refuses this checksum (R3.2, R6.3).
    Reject {
        /// Why the server refused it.
        reason: RejectReason,
        /// The existing asset, when the reason names one.
        asset_id: Option<String>,
        /// Whether the existing asset is in the trash.
        is_trashed: bool,
    },
}

/// Why the server rejected a checksum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// The content already exists on the server (R3.2).
    Duplicate,
    /// The server cannot accept this file type (R6.3).
    UnsupportedFormat,
    /// A reason this client does not know. It is still a refusal.
    Other(String),
}

/// One asset to upload with `POST /assets` (R3.1).
#[derive(Clone, Debug)]
pub struct AssetUpload<'a> {
    /// The local file, streamed as the `assetData` part. It is never buffered.
    pub path: &'a Path,
    /// The SHA-1 checksum for the `x-immich-checksum` header. Base64 or hex.
    pub checksum: &'a str,
    /// The local creation time, sent as `fileCreatedAt`. Immich requires it.
    pub created_at: SystemTime,
    /// The local modification time, sent as `fileModifiedAt`.
    pub modified_at: SystemTime,
    /// The XMP sidecar, sent as the `sidecarData` part (R3.3). `None` uploads
    /// the asset without one.
    pub sidecar: Option<&'a Path>,
    /// The asset id of the live photo video that is already on the server,
    /// sent as `livePhotoVideoId` (R3.3). `None` uploads the photo unlinked.
    pub live_photo_video_id: Option<&'a str>,
    /// The live bandwidth limit the body is metered against (R7.1). The
    /// default, `BandwidthLimit::default()`, is 0: the network's pace. A limit
    /// is shared by every upload that clones it, so it caps the total rate and
    /// not each upload's.
    pub bandwidth: BandwidthLimit,
}

/// The result of `POST /assets` (R3.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadResponse {
    /// The asset id the server assigned or matched.
    pub asset_id: String,
    /// Whether the server already had these bytes (HTTP 200, R3.2).
    pub duplicate: bool,
}

/// What `GET /assets/{id}` says the version flow needs (R4.2, R4.3).
///
/// Immich's asset response carries a great deal this client does not need, so
/// the response is parsed down to the two questions a new version asks about
/// the old one: which tags does it carry, and is it already in a stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetInfo {
    /// The ids of the tags on the asset, which a new version gets (R4.2).
    pub tag_ids: Vec<String>,
    /// The stack the asset is in, when it is in one (R4.3).
    pub stack_id: Option<String>,
}

/// One Immich album (GLOSSARY).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Album {
    /// The album id the server assigned.
    pub id: String,
    /// The album name, as the server kept it (R3.4).
    pub name: String,
}

/// The `POST /assets` response body.
#[derive(Deserialize)]
pub(crate) struct UploadBody {
    pub(crate) id: String,
}

/// The part of the `GET /assets/{id}` response the version flow reads.
#[derive(Deserialize)]
pub(crate) struct AssetInfoBody {
    /// The stack the asset is in, or nothing when it is in none (R4.3).
    #[serde(default)]
    pub(crate) stack: Option<StackBody>,
    /// The tags on the asset (R4.2).
    #[serde(default)]
    pub(crate) tags: Vec<TagBody>,
}

impl AssetInfoBody {
    /// Maps the response onto what the version flow needs.
    pub(crate) fn into_info(self) -> AssetInfo {
        AssetInfo {
            tag_ids: self.tags.into_iter().map(|tag| tag.id).collect(),
            stack_id: self.stack.map(|stack| stack.id),
        }
    }
}

/// One tag on an asset, as the asset response carries it (R4.2).
#[derive(Deserialize)]
pub(crate) struct TagBody {
    pub(crate) id: String,
}

/// One stack, as the asset and stack responses carry it (R4.3, R4.4).
///
/// Both answers carry it: the `stack` of an asset, and the stack
/// `POST /stacks` creates. The rest of `POST /stacks`'s answer (`primaryAssetId`
/// and the assets) is what the server already applied.
#[derive(Deserialize)]
pub(crate) struct StackBody {
    pub(crate) id: String,
}

/// The `PUT /assets/copy` request body (R4.2).
///
/// Every association R4.2 names is requested: albums, shared links, favorite
/// state, the sidecar, and stack membership. Faces, descriptions, and custom
/// metadata are not copied by this endpoint and stay on the source (ADR 0004).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssetCopyRequest<'a> {
    pub(crate) source_id: &'a str,
    pub(crate) target_id: &'a str,
    pub(crate) albums: bool,
    pub(crate) shared_links: bool,
    pub(crate) favorite: bool,
    pub(crate) sidecar: bool,
    pub(crate) stack: bool,
}

/// The `PUT /tags/assets` request body (R4.2).
#[derive(Serialize)]
pub(crate) struct BulkTagRequest<'a> {
    #[serde(rename = "tagIds")]
    pub(crate) tag_ids: &'a [String],
    #[serde(rename = "assetIds")]
    pub(crate) asset_ids: &'a [String],
}

/// The `POST /stacks` request body (R4.3).
#[derive(Serialize)]
pub(crate) struct CreateStackRequest<'a> {
    #[serde(rename = "assetIds")]
    pub(crate) asset_ids: &'a [String],
}

/// The `POST /albums` request body.
#[derive(Serialize)]
pub(crate) struct CreateAlbumRequest<'a> {
    #[serde(rename = "albumName")]
    pub(crate) album_name: &'a str,
    #[serde(rename = "assetIds")]
    pub(crate) asset_ids: &'a [String],
}

/// The `PUT /albums/{id}/assets` request body.
#[derive(Serialize)]
pub(crate) struct AlbumAssetsRequest<'a> {
    pub(crate) ids: &'a [String],
}

/// The `POST /albums` response body.
#[derive(Deserialize)]
pub(crate) struct AlbumBody {
    pub(crate) id: String,
    #[serde(rename = "albumName")]
    pub(crate) album_name: String,
}

/// The `DELETE /assets` request body (R5.3).
///
/// `force` is always `false`: v1 moves an asset to the trash and never
/// permanently deletes it (ADR 0002). The field is sent rather than omitted so
/// the intent is on the wire, not only in this client.
#[derive(Serialize)]
pub(crate) struct AssetBulkDeleteRequest<'a> {
    pub(crate) ids: &'a [String],
    pub(crate) force: bool,
}

/// The `POST /assets/bulk-upload-check` response body.
#[derive(Deserialize)]
pub(crate) struct BulkCheckBody {
    pub(crate) results: Vec<BulkCheckEntry>,
}

/// One raw entry of the bulk-check response.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BulkCheckEntry {
    pub(crate) id: String,
    pub(crate) action: String,
    pub(crate) reason: Option<String>,
    pub(crate) asset_id: Option<String>,
    #[serde(default)]
    pub(crate) is_trashed: bool,
}

impl BulkCheckEntry {
    /// Maps the raw entry onto the typed result.
    ///
    /// Returns `None` for an `action` this client does not know, so the caller
    /// can report it as a protocol failure instead of guessing.
    pub(crate) fn into_result(self) -> Option<BulkCheckResult> {
        let action = match self.action.as_str() {
            "accept" => BulkAction::Accept,
            "reject" => BulkAction::Reject {
                reason: match self.reason.as_deref() {
                    Some("duplicate") => RejectReason::Duplicate,
                    Some("unsupported-format") => RejectReason::UnsupportedFormat,
                    other => RejectReason::Other(other.unwrap_or_default().to_string()),
                },
                asset_id: self.asset_id,
                is_trashed: self.is_trashed,
            },
            _ => return None,
        };
        Some(BulkCheckResult {
            id: self.id,
            action,
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_debug_redacts_the_api_key() {
        let auth = Auth::ApiKey("api-key-abc123");
        assert_eq!(format!("{auth:?}"), "Auth::ApiKey(<redacted>)");

        let token: SessionToken =
            serde_json::from_str("\"session-token-abc123\"").expect("a token deserializes");
        let auth = Auth::Token(&token);
        assert_eq!(format!("{auth:?}"), "Auth::Token(SessionToken(<redacted>))");
    }

    #[test]
    fn an_unknown_bulk_action_is_not_guessed() {
        let entry = serde_json::from_str::<BulkCheckEntry>(r#"{"id":"a","action":"queue-later"}"#)
            .expect("the entry parses");
        assert!(entry.into_result().is_none());
    }

    #[test]
    fn session_token_debug_is_redacted() {
        let token = SessionToken("eyJhbGciOiJIUzI1NiJ9.secret-token".to_string());

        let formatted = format!("{token:?}");
        assert!(
            !formatted.contains("eyJhbGciOiJIUzI1NiJ9"),
            "the token leaked through Debug: {formatted}"
        );
        assert!(
            !formatted.contains("secret-token"),
            "the token leaked through Debug: {formatted}"
        );
        assert!(
            formatted.contains("redacted"),
            "Debug must show that it is redacted: {formatted}"
        );
    }
}
