//! The hand-written Immich HTTP client (ADR 0006).

use std::path::Path;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};

use reqwest::multipart::{Form, Part};
use reqwest::{Body, Client, RequestBuilder, Response, StatusCode, Url};
use serde::Serialize;
use tokio_util::io::ReaderStream;

use crate::bandwidth::{MeteredReader, Progress};
use crate::error::ClientError;
use crate::model::{
    Album, AlbumAssetsRequest, AlbumBody, ApiKeyInfo, AssetBulkDeleteRequest, AssetCopyRequest,
    AssetInfo, AssetInfoBody, AssetUpload, Auth, AuthStatus, BulkAction, BulkCheckBody,
    BulkCheckItem, BulkCheckResult, BulkIdsRequest, BulkTagRequest, CreateAlbumRequest,
    CreateStackRequest, LoginResponse, RejectReason, ServerAbout, SessionToken, StackBody,
    UploadBody, UploadResponse,
};

/// The longest error message body kept from the server. A reverse proxy can
/// answer with an HTML page; the client keeps a short, useful prefix.
const MESSAGE_LIMIT: usize = 200;

/// A request that has not finished within this window is unreachable.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The window [`send`] enforces. Production always gets [`REQUEST_TIMEOUT`];
/// only a unit test can shrink it, so the stalled-server test does not spend
/// half a minute on the wall clock.
fn request_timeout() -> Duration {
    #[cfg(test)]
    {
        let millis = TEST_TIMEOUT_MILLIS.load(std::sync::atomic::Ordering::Relaxed);
        if millis > 0 {
            return Duration::from_millis(millis);
        }
    }
    REQUEST_TIMEOUT
}

/// The test seam [`request_timeout`] reads. It exists only in the unit-test
/// build and defaults to zero, which means [`REQUEST_TIMEOUT`].
#[cfg(test)]
static TEST_TIMEOUT_MILLIS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How long an upload may hand the server no byte before it is called stalled.
///
/// The ordinary request window, so an upload meets a server that stops taking
/// its bytes in the 30 s every request always had. The bound is on progress,
/// not on a rate sampled when the request was built: it does not shorten when
/// the user lowers the limit while the upload is on the wire, and it does not
/// grow for a large file at a slow one (R7.1).
///
/// A window that sees bytes move is renewed whole, so the stall is called at
/// the last byte the server took plus this budget: at most twice it, never at
/// one window from the start of the gap.
const STALL_BUDGET: Duration = REQUEST_TIMEOUT;

/// Sends a request under the ordinary window.
///
/// An upload does not come through here: a total deadline cannot tell a slow
/// upload from a stalled server, and one derived from the limit in force when
/// the request was built aborts a healthy upload the moment the limit changes.
async fn send(request: RequestBuilder) -> Result<Response, ClientError> {
    request
        .timeout(request_timeout())
        .send()
        .await
        .map_err(ClientError::Unreachable)
}

/// Sends an upload, failing it when the body has made no progress for
/// [`STALL_BUDGET`].
///
/// The upload carries no total deadline: the client's 30 s would cut off any
/// throttled upload larger than `limit x 30 s`, and a deadline scaled by the
/// limit in force when the request was built grows without bound for a large
/// file at a slow one (R7.1). Progress is watched instead, so the bound holds
/// whatever the limit does while the request is on the wire. A window that saw
/// bytes is renewed in full, so the call comes at the last byte moved plus
/// [`STALL_BUDGET`] - at most twice that budget.
async fn send_upload(
    request: RequestBuilder,
    progress: &Progress,
) -> Result<Response, ClientError> {
    let mut send = Box::pin(request.send());
    // The last byte that had moved when this window opened.
    let mut waited = progress.stamp();
    loop {
        if let Ok(response) =
            tokio::time::timeout(STALL_BUDGET.saturating_sub(progress.idle()), &mut send).await
        {
            return response.map_err(ClientError::Unreachable);
        }
        // The window closed with nothing moving, and a timer that fires a
        // fraction late must not count as progress on its own: the server
        // stopped reading.
        let moved = progress.stamp();
        if moved == waited {
            return Err(ClientError::Stalled);
        }
        // Bytes did move: the request is healthy and waits out a full window.
        waited = moved;
    }
}

/// A typed client for the Immich Stable operations the client uses.
///
/// The client is cheap to clone and holds no credential. The session owns the
/// credential; the client only sends what the caller passes in.
#[derive(Clone, Debug)]
pub struct ImmichClient {
    http: Client,
    base: Url,
}

/// The login request body. It does not derive `Debug`, so a password cannot
/// reach a log through a stray `{:?}`.
#[derive(Serialize)]
struct LoginRequest<'a> {
    email: &'a str,
    password: &'a str,
}

/// The `POST /assets/bulk-upload-check` request body.
#[derive(Serialize)]
struct BulkCheckRequest<'a> {
    assets: &'a [BulkCheckItem],
}

/// Maps a non-success response onto its typed failure (R6.3).
///
/// The named classes are the ones the retry policy acts on. A refusal without
/// a class stays an [`ClientError::Protocol`] with its status, so the caller
/// still knows it is permanent, not transport noise.
async fn failure(response: Response) -> ClientError {
    let status = response.status();
    let retry_after = retry_after(&response);
    let message = server_message(response).await;
    match status {
        StatusCode::UNAUTHORIZED => ClientError::InvalidToken,
        StatusCode::FORBIDDEN => ClientError::PermissionDenied(message),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => ClientError::UnsupportedFormat(message),
        StatusCode::PAYLOAD_TOO_LARGE => ClientError::TooLarge(message),
        StatusCode::BAD_REQUEST if quota_exceeded(&message) => ClientError::QuotaExceeded(message),
        _ => ClientError::Protocol {
            status: status.as_u16(),
            message,
            retry_after,
        },
    }
}

/// The `Retry-After` wait a refusal asked for, in seconds (R6.2).
///
/// The header also allows an HTTP date. A date is not a wait this client can
/// act on without trusting the server's clock, so it reads as no hint and the
/// client's own backoff applies.
fn retry_after(response: &Response) -> Option<Duration> {
    let value = response.headers().get(reqwest::header::RETRY_AFTER)?;
    let seconds = value.to_str().ok()?.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

/// Maps a 2xx body the client could not read as JSON (R6.3).
///
/// A body that was cut short, that timed out, or that does not parse as the
/// schema expects is transport noise: the server proved nothing about the
/// request, so a later attempt may succeed. [`ClientError::Unreachable`] is the
/// transient class the retry policy acts on.
fn body_failure(status: StatusCode, error: reqwest::Error) -> ClientError {
    if error.is_body() || error.is_decode() || error.is_timeout() {
        ClientError::Unreachable(error)
    } else {
        ClientError::Protocol {
            status: status.as_u16(),
            message: error.to_string(),
            retry_after: None,
        }
    }
}

/// Whether a 400 message is Immich's quota refusal.
fn quota_exceeded(message: &str) -> bool {
    message.to_ascii_lowercase().contains("quota")
}

impl ImmichClient {
    /// Builds a client for one server. TLS verification stays on (N3).
    ///
    /// The client carries no deadline of its own: [`send`] puts the ordinary
    /// window on every request but an upload, and an upload is bounded by the
    /// stall guard instead.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::InvalidUrl`] for a URL that is not `http` or
    /// `https`, or that cannot be parsed, and [`ClientError::HttpClient`] when
    /// the TLS backend cannot start.
    pub fn new(base_url: &str) -> Result<Self, ClientError> {
        let mut base = Url::parse(base_url.trim())
            .map_err(|error| ClientError::InvalidUrl(error.to_string()))?;
        // `Url::parse` also accepts `mailto:`, `data:`, `tel:`, and `urn:`
        // URLs. They have no host to join an endpoint onto, and `Url::join`
        // panics on them, so the value from the server URL field and from a
        // hand-edited settings file is refused here instead.
        if !matches!(base.scheme(), "http" | "https") || base.cannot_be_a_base() {
            return Err(ClientError::InvalidUrl(
                "the server URL must start with http:// or https://".to_string(),
            ));
        }
        // A URL can carry `user:password@`. That password would reach the
        // settings file, a log line, and a request error, so it is refused at
        // the one place a server URL enters the client (R10.3, N3).
        if !base.username().is_empty() || base.password().is_some() {
            return Err(ClientError::InvalidUrl(
                "the server URL must not contain a user name or password; sign in with the \
                 credential fields instead"
                    .to_string(),
            ));
        }
        // A base with a path prefix (`https://host/immich`) must keep the
        // prefix when an endpoint is joined to it.
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let http = Client::builder().build().map_err(ClientError::HttpClient)?;
        Ok(Self { http, base })
    }

    /// Logs in with an email address and password (R1.3).
    ///
    /// # Errors
    ///
    /// [`ClientError::WrongCredentials`] when the server rejects the password,
    /// [`ClientError::PasswordLoginDisabled`] when the server has password
    /// login turned off (R1.6), and the transport and protocol variants.
    pub async fn login(&self, email: &str, password: &str) -> Result<LoginResponse, ClientError> {
        let response = send(
            self.http
                .post(self.endpoint("auth/login"))
                .json(&LoginRequest { email, password }),
        )
        .await?;
        let result = self.read_login(response).await;
        log_outcome("login", &result);
        result
    }

    /// Confirms that a session token is still valid (R6.6).
    ///
    /// # Errors
    ///
    /// [`ClientError::InvalidToken`] when the server refuses the token, plus the
    /// transport and protocol variants.
    pub async fn validate_token(&self, token: &SessionToken) -> Result<(), ClientError> {
        let response = send(
            self.http
                .post(self.endpoint("auth/validateToken"))
                .bearer_auth(token.expose()),
        )
        .await?;
        let result = self
            .read_auth_status(response, ClientError::InvalidToken)
            .await;
        log_outcome("validateToken", &result);
        result
    }

    /// Confirms that an API key is still valid (R1.5).
    ///
    /// # Errors
    ///
    /// [`ClientError::InvalidApiKey`] when the server refuses the key, plus the
    /// transport and protocol variants.
    pub async fn validate_api_key(&self, key: &str) -> Result<(), ClientError> {
        let response = send(
            self.http
                .post(self.endpoint("auth/validateToken"))
                .header("x-api-key", key),
        )
        .await?;
        let result = self
            .read_auth_status(response, ClientError::InvalidApiKey)
            .await;
        log_outcome("validateApiKey", &result);
        result
    }

    /// Reads the server version.
    ///
    /// `GET /server/about` is `x-immich-state: Stable` and asks for the
    /// `server.about` permission, so the request carries the credential and a
    /// key without the permission is a [`ClientError::PermissionDenied`] whose
    /// message names it.
    ///
    /// # Errors
    ///
    /// Returns the typed refusal classes (R6.3) and the transport and protocol
    /// variants.
    pub async fn about(&self, auth: Auth<'_>) -> Result<ServerAbout, ClientError> {
        let response =
            send(self.authorize(self.http.get(self.endpoint("server/about")), auth)).await?;
        let status = response.status();
        let result = if status.is_success() {
            response
                .json::<ServerAbout>()
                .await
                .map_err(|error| body_failure(status, error))
        } else {
            Err(failure(response).await)
        };
        log_outcome("about", &result);
        result
    }

    /// Reads the API key a request authenticates with (R10.2).
    ///
    /// `GET /api-keys/me` is `x-immich-state: Stable` and asks for no
    /// permission beyond a credential in the v3.3.1 spec. The real server
    /// still answers a bearer session with 403 `Not authenticated with an API
    /// Key`, so the caller asks only when the credential is an API key and
    /// shows the refusal as a message when it is not.
    ///
    /// # Errors
    ///
    /// [`ClientError::PermissionDenied`] for that session-token refusal, the
    /// other typed refusal classes (R6.3), and the transport and protocol
    /// variants.
    pub async fn api_key_permissions(&self, auth: Auth<'_>) -> Result<ApiKeyInfo, ClientError> {
        let response =
            send(self.authorize(self.http.get(self.endpoint("api-keys/me")), auth)).await?;
        let status = response.status();
        let result = if status.is_success() {
            response
                .json::<ApiKeyInfo>()
                .await
                .map_err(|error| body_failure(status, error))
        } else {
            Err(failure(response).await)
        };
        log_outcome("api-key-permissions", &result);
        result
    }

    /// Looks up SHA-1 checksums before uploading (R3.1).
    ///
    /// The caller sends one item or a batch; the server answers one result per
    /// item, in the same order.
    ///
    /// # Errors
    ///
    /// Returns the typed refusal classes (R6.3), [`ClientError::InvalidToken`]
    /// for a refused credential, and the transport and protocol variants.
    pub async fn bulk_upload_check(
        &self,
        auth: Auth<'_>,
        items: &[BulkCheckItem],
    ) -> Result<Vec<BulkCheckResult>, ClientError> {
        let response = send(
            self.authorize(
                self.http.post(self.endpoint("assets/bulk-upload-check")),
                auth,
            )
            .json(&BulkCheckRequest { assets: items }),
        )
        .await?;
        let result = self.read_bulk_check(response).await;
        log_outcome("bulk-upload-check", &result);
        result
    }

    /// Uploads one asset with its checksum header (R3.1).
    ///
    /// The file streams from disk; it is never buffered in memory. The
    /// checksum must already be known, because `x-immich-checksum` is a request
    /// header and it is what makes the server answer `duplicate` for an asset
    /// it already has.
    ///
    /// The XMP sidecar and the live photo video id the caller resolved ride
    /// along in the same request (R3.3).
    ///
    /// Immich answers such a duplicate from the header, before multer parses
    /// the body, and hangs up while the client is still sending (#19). That
    /// answer can be lost in flight, so a transport failure here is resolved by
    /// asking whether the server kept the asset.
    ///
    /// A `duplicate` answer therefore proves nothing about the `sidecarData`
    /// part that rode along: the server never read the body. It also deletes
    /// the file it just buffered when it finds the body checksum known. There
    /// is no Stable operation that attaches a sidecar to an asset afterwards,
    /// so the caller must not treat a duplicate as "the sidecar arrived" (see
    /// `sync-core`'s upload pipeline).
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::LocalRead`] when the file or its sidecar cannot be
    /// opened, the typed refusal classes (R6.3), and the transport and
    /// protocol variants. A transport failure is returned only when the server
    /// neither holds the asset nor answers the follow-up question.
    pub async fn upload_asset(
        &self,
        auth: Auth<'_>,
        upload: &AssetUpload<'_>,
    ) -> Result<UploadResponse, ClientError> {
        let filename = upload
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("asset")
            .to_string();
        let file = tokio::fs::File::open(upload.path)
            .await
            .map_err(ClientError::LocalRead)?;
        let length = file.metadata().await.map_err(ClientError::LocalRead)?.len();
        // The throttle is below the multipart reader, so it meters the file
        // bytes themselves, and it is live: a settings change is read on the
        // next poll (R7.1).
        let (reader, progress) = MeteredReader::new(file, upload.bandwidth.clone());
        let part = Part::stream_with_length(Body::wrap_stream(ReaderStream::new(reader)), length)
            .file_name(filename.clone());
        // R3.3: the sidecar part goes before every text field. Immich's multer
        // filter resolves the `sidecarData` part's name from
        // `body.filename || file.originalName` while it parses the stream, and
        // `body.filename` is already the media name once the `filename` field
        // is read — `mimeTypes.isSidecar("IMG_0001.jpg")` is false, so the
        // filter answers `Unsupported file type` and every sidecar upload is a
        // permanent 400. Sent first, the part's own `file_name()` is already
        // `IMG_0001.jpg.xmp`, which is what `originalName` then supplies.
        let mut form = Form::new().part("assetData", part);
        if let Some(sidecar) = upload.sidecar {
            // The sidecar streams from disk as its own part, so an asset with
            // a sidecar does not hold the sidecar in memory.
            form = form.part("sidecarData", file_part(sidecar).await?);
        }
        // Immich requires the file's own dates on the asset. The caller reads
        // them from the local file (R3.1).
        form = form
            .text("filename", filename.clone())
            .text("deviceAssetId", upload.path.display().to_string())
            .text("deviceId", "immich-sync")
            .text("fileCreatedAt", iso8601(upload.created_at))
            .text("fileModifiedAt", iso8601(upload.modified_at));
        if let Some(video_id) = upload.live_photo_video_id {
            form = form.text("livePhotoVideoId", video_id.to_string());
        }
        let sent = send_upload(
            self.authorize(self.http.post(self.endpoint("assets")), auth)
                .header("x-immich-checksum", upload.checksum)
                .multipart(form),
            &progress,
        )
        .await;
        let result = match sent {
            Ok(response) => self.read_upload(response).await,
            Err(lost) => {
                match self
                    .asset_for_checksum(auth, &filename, upload.checksum)
                    .await
                {
                    Some(asset_id) => Ok(UploadResponse {
                        asset_id,
                        duplicate: true,
                    }),
                    None => Err(lost),
                }
            }
        };
        log_outcome("upload-asset", &result);
        result
    }

    /// The id of the live asset the server already holds for `checksum`, if
    /// any.
    ///
    /// Only an answer carrying the `id` sent here is about this file; an entry
    /// for another id is some other asset, and a row that carries one is never
    /// uploaded again (#19). A trashed asset is not a held asset: the bytes
    /// are not usable until a restore, so it is not returned here.
    async fn asset_for_checksum(&self, auth: Auth<'_>, id: &str, checksum: &str) -> Option<String> {
        let item = BulkCheckItem {
            id: id.to_string(),
            checksum: checksum.to_string(),
        };
        let results = self
            .bulk_upload_check(auth, std::slice::from_ref(&item))
            .await
            .ok()?;
        match results
            .into_iter()
            .find(|result| result.id == id)
            .map(|result| result.action)?
        {
            BulkAction::Reject {
                reason: RejectReason::Duplicate,
                asset_id: Some(asset_id),
                is_trashed: false,
            } => Some(asset_id),
            _ => None,
        }
    }

    /// Creates one album, optionally with the assets it already holds (R3.4).
    ///
    /// `POST /albums` is `x-immich-state: Stable` in the v3.3.1 spec, which is
    /// what R3.5 allows sync correctness to depend on.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::InvalidToken`] for a refused credential, the typed
    /// refusal classes (R6.3), and the transport and protocol variants.
    pub async fn create_album(
        &self,
        auth: Auth<'_>,
        name: &str,
        asset_ids: &[String],
    ) -> Result<Album, ClientError> {
        let response = send(
            self.authorize(self.http.post(self.endpoint("albums")), auth)
                .json(&CreateAlbumRequest {
                    album_name: name,
                    asset_ids,
                }),
        )
        .await?;
        let result = self.read_album(response).await;
        log_outcome("create-album", &result);
        result
    }

    /// Adds assets to one album (R3.4).
    ///
    /// An asset the album already holds is not an error: Immich answers with
    /// `success: false` for it and 200 for the request, so one call is
    /// idempotent and a retry after a lost answer cannot duplicate a
    /// membership.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::InvalidToken`] for a refused credential, the typed
    /// refusal classes (R6.3), and the transport and protocol variants. An
    /// unknown album id is a [`ClientError::Protocol`] with its 404 status, so
    /// the caller can tell it from transport noise.
    pub async fn add_assets_to_album(
        &self,
        auth: Auth<'_>,
        album_id: &str,
        asset_ids: &[String],
    ) -> Result<(), ClientError> {
        let response = send(
            self.authorize(
                self.http
                    .put(self.endpoint(&format!("albums/{album_id}/assets"))),
                auth,
            )
            .json(&AlbumAssetsRequest { ids: asset_ids }),
        )
        .await?;
        let result = self.read_accepted(response).await;
        log_outcome("add-album-assets", &result);
        result
    }

    /// Reads the tags and the stack of one asset (R4.2, R4.3).
    ///
    /// One request answers both questions a new version asks about the old one,
    /// so the version flow never walks the asset twice.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::InvalidToken`] for a refused credential,
    /// [`ClientError::Protocol`] with its 404 for an asset the server does not
    /// hold, the typed refusal classes (R6.3), and the transport variants.
    pub async fn asset_info(
        &self,
        auth: Auth<'_>,
        asset_id: &str,
    ) -> Result<AssetInfo, ClientError> {
        let response = send(self.authorize(
            self.http.get(self.endpoint(&format!("assets/{asset_id}"))),
            auth,
        ))
        .await?;
        let result = self.read_asset_info(response).await;
        log_outcome("asset-info", &result);
        result
    }

    /// Copies the associations of one asset onto another (R4.2).
    ///
    /// R4.2 names the associations a new version needs, so the album, shared
    /// link, favorite, and sidecar flags are on. `stack` carries whether stack
    /// membership moves too: the version flow turns it off when a retry finds
    /// the new asset already in the old version's stack, because the server
    /// would then merge that stack into itself and delete it. Faces,
    /// descriptions, and custom metadata are not copied by this endpoint; they
    /// stay on the source (ADR 0004).
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::InvalidToken`] for a refused credential, the typed
    /// refusal classes (R6.3), and the transport and protocol variants.
    pub async fn copy_asset(
        &self,
        auth: Auth<'_>,
        source_id: &str,
        target_id: &str,
        stack: bool,
    ) -> Result<(), ClientError> {
        let response = send(
            self.authorize(self.http.put(self.endpoint("assets/copy")), auth)
                .json(&AssetCopyRequest {
                    source_id,
                    target_id,
                    albums: true,
                    shared_links: true,
                    favorite: true,
                    sidecar: true,
                    stack,
                }),
        )
        .await?;
        let result = self.read_accepted(response).await;
        log_outcome("copy-asset", &result);
        result
    }

    /// Creates one stack from `asset_ids` and returns the stack id (R4.3).
    ///
    /// The first asset is the primary one, which is what puts the new version
    /// at the top of the timeline entry. An asset that is already primary of a
    /// stack merges that stack into the new one, which is what the server does
    /// when a version flow is retried.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::InvalidToken`] for a refused credential, the typed
    /// refusal classes (R6.3), and the transport and protocol variants.
    pub async fn create_stack(
        &self,
        auth: Auth<'_>,
        asset_ids: &[String],
    ) -> Result<String, ClientError> {
        let response = send(
            self.authorize(self.http.post(self.endpoint("stacks")), auth)
                .json(&CreateStackRequest { asset_ids }),
        )
        .await?;
        let result = self.read_stack(response).await;
        log_outcome("create-stack", &result);
        result
    }

    /// Adds `tag_ids` to `asset_ids` (R4.2).
    ///
    /// One call carries the whole tag set of the old version onto the new one.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::InvalidToken`] for a refused credential, the typed
    /// refusal classes (R6.3), and the transport and protocol variants.
    pub async fn bulk_tag_assets(
        &self,
        auth: Auth<'_>,
        tag_ids: &[String],
        asset_ids: &[String],
    ) -> Result<(), ClientError> {
        let response = send(
            self.authorize(self.http.put(self.endpoint("tags/assets")), auth)
                .json(&BulkTagRequest { tag_ids, asset_ids }),
        )
        .await?;
        let result = self.read_accepted(response).await;
        log_outcome("bulk-tag-assets", &result);
        result
    }

    /// Moves assets to the trash (R5.3).
    ///
    /// v1 has no permanent delete: the request always carries `force: false`,
    /// so the assets stay in the trash and can be restored (ADR 0002). The
    /// caller records the decision only after this answers.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::PermissionDenied`] when the credential lacks
    /// Immich's `asset.delete` permission (R5.5), the other typed refusal
    /// classes (R6.3), and the transport and protocol variants. A refusal is
    /// permanent: it changes nothing on the server.
    pub async fn trash_assets(
        &self,
        auth: Auth<'_>,
        asset_ids: &[String],
    ) -> Result<(), ClientError> {
        let response = send(
            self.authorize(self.http.delete(self.endpoint("assets")), auth)
                .json(&AssetBulkDeleteRequest {
                    ids: asset_ids,
                    force: false,
                }),
        )
        .await?;
        let result = self.read_accepted(response).await;
        log_outcome("trash-assets", &result);
        result
    }

    /// Brings trashed assets back (R5.3).
    ///
    /// `POST /trash/restore/assets` is `x-immich-state: Stable` in the v3.3.1
    /// spec and asks for the same `asset.delete` permission the trash call
    /// does. This is the way back for a checksum the server holds in the
    /// trash: a re-POST is answered `duplicate` from the `x-immich-checksum`
    /// header before the body is read, and the unique checksum index does not
    /// exclude `deletedAt`.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::PermissionDenied`] when the credential lacks
    /// Immich's `asset.delete` permission (R5.5), the other typed refusal
    /// classes (R6.3), and the transport and protocol variants.
    pub async fn restore_assets(
        &self,
        auth: Auth<'_>,
        asset_ids: &[String],
    ) -> Result<(), ClientError> {
        let response = send(
            self.authorize(self.http.post(self.endpoint("trash/restore/assets")), auth)
                .json(&BulkIdsRequest { ids: asset_ids }),
        )
        .await?;
        let result = self.read_accepted(response).await;
        log_outcome("restore-assets", &result);
        result
    }

    /// Attaches the credential to one request.
    fn authorize(&self, request: RequestBuilder, auth: Auth<'_>) -> RequestBuilder {
        match auth {
            Auth::Token(token) => request.bearer_auth(token.expose()),
            Auth::ApiKey(key) => request.header("x-api-key", key),
        }
    }

    /// Maps a bulk-check response to its result.
    async fn read_bulk_check(
        &self,
        response: Response,
    ) -> Result<Vec<BulkCheckResult>, ClientError> {
        let status = response.status();
        if !status.is_success() {
            return Err(failure(response).await);
        }
        let body = response
            .json::<BulkCheckBody>()
            .await
            .map_err(|error| body_failure(status, error))?;
        body.results
            .into_iter()
            .map(|entry| {
                entry.into_result().ok_or(ClientError::Protocol {
                    status: status.as_u16(),
                    message: "bulk-upload-check answered with an unknown action".to_string(),
                    retry_after: None,
                })
            })
            .collect()
    }

    /// Maps an upload response to its result (R3.6).
    async fn read_upload(&self, response: Response) -> Result<UploadResponse, ClientError> {
        let status = response.status();
        if !status.is_success() {
            return Err(failure(response).await);
        }
        let body = response
            .json::<UploadBody>()
            .await
            .map_err(|error| body_failure(status, error))?;
        if body.id.is_empty() {
            return Err(ClientError::Protocol {
                status: status.as_u16(),
                message: "the server returned no asset id".to_string(),
                retry_after: None,
            });
        }
        Ok(UploadResponse {
            asset_id: body.id,
            // Immich answers 201 for a created asset and 200 for a duplicate.
            duplicate: status == StatusCode::OK,
        })
    }

    /// Maps an album response to its id and name.
    async fn read_album(&self, response: Response) -> Result<Album, ClientError> {
        let status = response.status();
        if !status.is_success() {
            return Err(failure(response).await);
        }
        let body = response
            .json::<AlbumBody>()
            .await
            .map_err(|error| body_failure(status, error))?;
        if body.id.is_empty() || body.album_name.is_empty() {
            return Err(ClientError::Protocol {
                status: status.as_u16(),
                message: "the server returned an album without an id or a name".to_string(),
                retry_after: None,
            });
        }
        Ok(Album {
            id: body.id,
            name: body.album_name,
        })
    }

    /// Maps an asset response to what the version flow reads.
    async fn read_asset_info(&self, response: Response) -> Result<AssetInfo, ClientError> {
        let status = response.status();
        if !status.is_success() {
            return Err(failure(response).await);
        }
        let body = response
            .json::<AssetInfoBody>()
            .await
            .map_err(|error| body_failure(status, error))?;
        Ok(body.into_info())
    }

    /// Maps a stack response to its id.
    async fn read_stack(&self, response: Response) -> Result<String, ClientError> {
        let status = response.status();
        if !status.is_success() {
            return Err(failure(response).await);
        }
        let body = response
            .json::<StackBody>()
            .await
            .map_err(|error| body_failure(status, error))?;
        if body.id.is_empty() {
            return Err(ClientError::Protocol {
                status: status.as_u16(),
                message: "the server returned a stack without an id".to_string(),
                retry_after: None,
            });
        }
        Ok(body.id)
    }

    /// Accepts a 2xx answer whose body the client has no use for.
    async fn read_accepted(&self, response: Response) -> Result<(), ClientError> {
        let status = response.status();
        if !status.is_success() {
            return Err(failure(response).await);
        }
        Ok(())
    }

    /// The absolute URL of one endpoint.
    fn endpoint(&self, path: &str) -> Url {
        self.base
            .join(path)
            .expect("the base URL ends in a slash before a relative path")
    }

    /// Maps a login response to its result.
    async fn read_login(&self, response: Response) -> Result<LoginResponse, ClientError> {
        let status = response.status();
        if status.is_success() {
            return response
                .json::<LoginResponse>()
                .await
                .map_err(|error| body_failure(status, error));
        }
        let retry_after = retry_after(&response);
        let message = server_message(response).await;
        match status {
            // The server answers 401 for both a wrong password and a disabled
            // password login; only Immich's own message separates them (R1.6).
            StatusCode::UNAUTHORIZED if password_login_disabled(&message) => {
                Err(ClientError::PasswordLoginDisabled)
            }
            StatusCode::UNAUTHORIZED => Err(ClientError::WrongCredentials),
            _ => Err(ClientError::Protocol {
                status: status.as_u16(),
                message,
                retry_after,
            }),
        }
    }

    /// Maps a `validateToken` response to its result.
    async fn read_auth_status(
        &self,
        response: Response,
        refusal: ClientError,
    ) -> Result<(), ClientError> {
        let status = response.status();
        let retry_after = retry_after(&response);
        if status == StatusCode::UNAUTHORIZED {
            return Err(refusal);
        }
        if !status.is_success() {
            return Err(ClientError::Protocol {
                status: status.as_u16(),
                message: server_message(response).await,
                retry_after,
            });
        }
        let body = response
            .json::<AuthStatus>()
            .await
            .map_err(|error| body_failure(status, error))?;
        if body.auth_status {
            Ok(())
        } else {
            Err(refusal)
        }
    }
}

/// One streamed multipart file part: the file from disk, never buffered.
///
/// The filename is a hint for the server's own file name; a path without one
/// is not a file the pipeline can upload anyway.
///
/// # Errors
///
/// Returns [`ClientError::LocalRead`] when the file cannot be opened or its
/// length cannot be read.
async fn file_part(path: &Path) -> Result<Part, ClientError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(ClientError::LocalRead)?;
    let length = file.metadata().await.map_err(ClientError::LocalRead)?.len();
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("asset")
        .to_string();
    Ok(
        Part::stream_with_length(Body::wrap_stream(ReaderStream::new(file)), length)
            .file_name(filename),
    )
}

/// One local timestamp in the ISO 8601 form Immich stores, in UTC.
///
/// Immich parses `fileCreatedAt` and `fileModifiedAt` as ISO 8601, so a
/// numeric timestamp is not accepted; the trailing `Z` and millisecond
/// precision match what the server and its own uploaders send.
#[must_use]
fn iso8601(time: SystemTime) -> String {
    let datetime: DateTime<Utc> = time.into();
    datetime.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The server's message from an error response, shortened and without secrets.
async fn server_message(response: Response) -> String {
    let text = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|body| {
            body.get("message")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .unwrap_or(text);
    message.chars().take(MESSAGE_LIMIT).collect()
}

/// Whether a 401 message is Immich's disabled-password-login refusal.
///
/// The real server throws `UnauthorizedException('Password login has been
/// disabled')`. A proxy or WAF 401 that merely contains the word "disabled"
/// is a wrong credential, not this refusal.
fn password_login_disabled(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("password login has been disabled")
}

/// Records the outcome of one request as structured, secret-free fields.
///
/// Every failure variant can carry a server-supplied message — the protocol
/// error, and the typed refusals that quote the body (403, 415, 413, and the
/// quota 400) — and a broken or hostile server can fill it with a submitted
/// credential. Only [`ClientError::redacted_reason`] is recorded (SPEC N3).
fn log_outcome<T>(operation: &'static str, result: &Result<T, ClientError>) {
    match result {
        Ok(_) => tracing::debug!(operation, outcome = "ok", "Immich request finished"),
        Err(error) => tracing::debug!(
            operation,
            outcome = "error",
            reason = %error.redacted_reason(),
            "Immich request finished"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_times_are_formatted_as_iso8601_utc() {
        // 1_700_000_000 seconds after the epoch is 2023-11-14T22:13:20Z.
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(iso8601(time), "2023-11-14T22:13:20.000Z");
        assert_eq!(iso8601(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn only_immichs_quota_message_is_a_quota_refusal() {
        assert!(quota_exceeded("Quota has been exceeded!"));
        assert!(!quota_exceeded("Invalid asset data"));
    }

    #[tokio::test]
    async fn a_request_the_server_never_answers_hits_the_deadline() {
        let server = mock_immich::MockImmich::start().await;
        server.fail(
            mock_immich::Operation::CreateAlbum,
            mock_immich::Fault::Slow(Duration::from_secs(30)),
        );
        TEST_TIMEOUT_MILLIS.store(200, std::sync::atomic::Ordering::Relaxed);
        let client = ImmichClient::new(server.base_url()).expect("a valid URL");

        let error = client
            .create_album(Auth::ApiKey("test-key"), "album", &[])
            .await
            .expect_err("a server that accepts and never answers must fail the call");

        TEST_TIMEOUT_MILLIS.store(0, std::sync::atomic::Ordering::Relaxed);
        assert!(
            matches!(&error, ClientError::Unreachable(inner) if inner.is_timeout()),
            "the ordinary request window must end the call, got {error:?}"
        );
    }

    #[test]
    fn only_immichs_disabled_message_is_a_disabled_server() {
        assert!(password_login_disabled("Password login has been disabled"));
        assert!(password_login_disabled(
            "UnauthorizedException: Password login has been disabled"
        ));
        assert!(!password_login_disabled("Invalid user credentials"));
        assert!(
            !password_login_disabled("access disabled by the proxy"),
            "a proxy 401 is not Immich's refusal"
        );
        assert!(!password_login_disabled("this account is disabled"));
    }
}
