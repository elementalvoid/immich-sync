//! HTTP server implementation for the mock Immich double.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Multipart, Path, State as AxumState};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// The Immich server version the double reports.
const SERVER_VERSION: &str = "v3.3.1";
/// A stable owner id for the assets the double returns.
const OWNER_ID: &str = "00000000-0000-4000-8000-000000000001";
/// A stable timestamp for the assets and albums the double returns.
const TIMESTAMP: &str = "2026-01-01T00:00:00.000Z";

/// The Immich operation a fault applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    Login,
    ValidateToken,
    About,
    ApiKeyMe,
    BulkUploadCheck,
    Upload,
    GetAsset,
    Copy,
    CreateStack,
    TagAssets,
    DeleteAssets,
    CreateAlbum,
    AddAssetsToAlbum,
}

/// A fault the mock injects for a matching request.
#[derive(Debug, Clone)]
pub enum Fault {
    /// Reply with an HTTP status; `retry_after` adds the `Retry-After` header.
    Status { code: u16, retry_after: Option<u64> },
    /// Reply with an HTTP status and a JSON `message` body, as a broken or
    /// hostile server that echoes the request would.
    ServerMessage { code: u16, message: String },
    /// 400 with the Immich quota message.
    Quota,
    /// 403 with the Immich missing-permission message.
    PermissionDenied,
    /// 415 for an unsupported format.
    UnsupportedFormat,
    /// Delay the response by the given duration.
    Slow(Duration),
    /// Drop the connection without a complete response.
    Disconnect,
}

struct State {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    faults: HashMap<Operation, VecDeque<Fault>>,
    assets: HashMap<String, Asset>,
    checksums: HashMap<Vec<u8>, String>,
    /// The `POST /assets` requests whose body the server read, in arrival
    /// order. A duplicate answered from the `x-immich-checksum` header never
    /// reaches this list, exactly as on the real server.
    uploads: Vec<UploadRecord>,
    /// Checksums `bulk-upload-check` rejects as an unsupported format.
    unsupported: HashSet<Vec<u8>>,
    albums: HashMap<String, Album>,
    /// The album ids in creation order, so `album_names` is deterministic.
    album_order: Vec<String>,
    /// How many `POST /assets/bulk-upload-check` requests arrived.
    bulk_check_requests: u64,
    /// How many `POST /auth/login` requests arrived.
    login_requests: u64,
    /// How many `POST /auth/validateToken` requests arrived (R6.6).
    validate_token_requests: u64,
    /// How many `POST /assets` requests arrived, whether or not one uploaded.
    upload_requests: u64,
    /// How many `POST /assets` requests are being handled right now.
    uploads_in_flight: u64,
    /// The most uploads that were ever in flight at one time.
    peak_uploads: u64,
    /// How long the upload handler waits before it reads a body. A test uses
    /// it to hold uploads open and observe the client's concurrency.
    upload_delay: Option<Duration>,
    /// How many `POST /albums` requests arrived, whether or not one created an
    /// album (R3.4).
    create_album_requests: u64,
    /// How many `PUT /albums/{id}/assets` requests arrived.
    add_album_requests: u64,
    /// Immich's `passwordLogin.enabled` system config.
    password_login_disabled: bool,
    /// The key `x-api-key` must carry, once one is configured.
    api_key: Option<String>,
    /// The permissions the configured key was granted. `None` grants every
    /// permission, mirroring `api_key: None` accepting any key.
    api_key_permissions: Option<Vec<String>>,
    /// How many `GET /server/about` requests arrived.
    about_requests: u64,
    /// The credential the last `GET /server/about` carried: `api_key`,
    /// `bearer`, or `None`.
    about_auth: Option<String>,
    /// How many `GET /api-keys/me` requests arrived.
    api_key_me_requests: u64,
    /// The session tokens this server issued and has not revoked.
    tokens: HashSet<String>,
    /// When set, only an issued, unrevoked token validates.
    require_known_credentials: bool,
    /// The `fileCreatedAt` and `fileModifiedAt` fields of the last upload.
    last_upload_dates: Option<(String, String)>,
    /// Every `PUT /assets/copy` call, in order (R4.2). A test asserts the
    /// request the client sent, not only the outcome it got.
    copy_calls: Vec<AssetCopyCall>,
    /// Every `POST /stacks` call, in order (R4.3).
    stack_calls: Vec<StackCall>,
    /// The stacks the double holds, by stack id, with their members in the
    /// order they joined. The first member is the primary asset.
    stacks: HashMap<String, Vec<String>>,
    /// The tag ids on each asset (R4.2).
    asset_tags: HashMap<String, Vec<String>>,
}

/// One `PUT /assets/copy` call, as the double received it (R4.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetCopyCall {
    /// The asset the associations were copied from.
    pub source_id: String,
    /// The asset they were copied onto.
    pub target_id: String,
    /// Whether album memberships were copied.
    pub albums: bool,
    /// Whether shared links were copied.
    pub shared_links: bool,
    /// Whether favorite state was copied.
    pub favorite: bool,
    /// Whether the sidecar was copied.
    pub sidecar: bool,
    /// Whether stack membership was copied.
    pub stack: bool,
}

/// One `POST /stacks` call, with the stack the double made for it (R4.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackCall {
    /// The assets the stack was asked for, in request order. The first is the
    /// primary asset.
    pub asset_ids: Vec<String>,
    /// The stack the double created or merged into.
    pub stack_id: String,
}

struct Asset {
    id: String,
    checksum: Vec<u8>,
    trashed: bool,
    has_sidecar: bool,
    live_photo_video_id: Option<String>,
    /// The stack this asset is in, when it is in one (R4.3).
    stack_id: Option<String>,
}

struct Album {
    id: String,
    name: String,
    asset_ids: Vec<String>,
}

/// One `POST /assets` request, as the server received it.
///
/// The double keeps the order so a test can prove *when* a request arrived:
/// a live photo's video has to go on the wire before the photo that links it
/// (R3.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadRecord {
    /// The `filename` field of the request.
    pub filename: String,
    /// Whether the request carried a `sidecarData` part (R3.3).
    pub has_sidecar: bool,
    /// The `livePhotoVideoId` field, when the request carried one (R3.3).
    pub live_photo_video_id: Option<String>,
    /// Every part name in the order the body carried them. Immich's multer
    /// filter resolves a `sidecarData` part from `body.filename ||
    /// file.originalName` while it parses the stream, so the order is part of
    /// the request's meaning, not a detail.
    pub parts: Vec<String>,
}

impl Inner {
    fn asset_by_checksum(&self, checksum: &[u8]) -> Option<&Asset> {
        let id = self.checksums.get(checksum)?;
        self.assets.get(id)
    }

    fn asset(&self, id: &str) -> Option<&Asset> {
        self.assets.get(id)
    }

    fn has_asset(&self, id: &str) -> bool {
        self.assets.contains_key(id)
    }

    fn insert_asset(&mut self, asset: Asset) {
        self.checksums
            .insert(asset.checksum.clone(), asset.id.clone());
        self.assets.insert(asset.id.clone(), asset);
    }

    fn mark_trashed(&mut self, id: &str) {
        if let Some(asset) = self.assets.get_mut(id) {
            asset.trashed = true;
        }
    }

    /// The stack of one asset, as `AssetResponseDto` reports it (R4.3).
    fn stack_of(&self, asset_id: &str) -> Option<StackCall> {
        let id = self.assets.get(asset_id)?.stack_id.clone()?;
        let asset_ids = self.stacks.get(&id)?.clone();
        Some(StackCall {
            asset_ids,
            stack_id: id,
        })
    }

    /// Puts `asset_ids` in one stack and returns the stack id (R4.3).
    ///
    /// An asset that is already the primary of a stack merges that stack into
    /// the new one, which is what the real server does; a version flow that is
    /// retried after a lost answer therefore lands in the same stack instead of
    /// splitting the chain.
    fn create_stack(&mut self, asset_ids: &[String]) -> String {
        let merged = asset_ids
            .first()
            .and_then(|id| self.assets.get(id))
            .and_then(|asset| asset.stack_id.clone());
        let id = merged.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let members = self.stacks.entry(id.clone()).or_default();
        for asset_id in asset_ids {
            if !members.contains(asset_id) {
                members.push(asset_id.clone());
            }
            if let Some(asset) = self.assets.get_mut(asset_id) {
                asset.stack_id = Some(id.clone());
            }
        }
        id
    }

    fn tag(&mut self, asset_ids: &[String], tag_ids: &[String]) -> usize {
        let mut count = 0;
        for asset_id in asset_ids {
            if !self.assets.contains_key(asset_id) {
                continue;
            }
            let tags = self.asset_tags.entry(asset_id.clone()).or_default();
            for tag_id in tag_ids {
                if !tags.contains(tag_id) {
                    tags.push(tag_id.clone());
                    count += 1;
                }
            }
        }
        count
    }

    fn remove_asset(&mut self, id: &str) {
        if let Some(asset) = self.assets.remove(id) {
            self.checksums.remove(&asset.checksum);
        }
    }

    fn create_album(&mut self, name: String, asset_ids: Vec<String>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.album_order.push(id.clone());
        self.albums.insert(
            id.clone(),
            Album {
                id: id.clone(),
                name,
                asset_ids,
            },
        );
        id
    }

    /// Add the known, not-yet-listed assets to the album. Returns `None` when
    /// the album is unknown.
    fn add_assets_to_album(&mut self, album_id: &str, asset_ids: &[String]) -> Option<Vec<String>> {
        let album = self.albums.get_mut(album_id)?;
        let mut added = Vec::new();
        for id in asset_ids {
            if self.assets.contains_key(id) && !album.asset_ids.contains(id) {
                album.asset_ids.push(id.clone());
                added.push(id.clone());
            }
        }
        Some(added)
    }

    fn album_for_asset(&self, asset_id: &str) -> Option<String> {
        self.albums
            .values()
            .find(|album| album.asset_ids.iter().any(|id| id == asset_id))
            .map(|album| album.name.clone())
    }

    /// Every album name, in creation order. Two albums can share one name,
    /// which is what two leaf folders with the same folder name look like.
    fn album_names(&self) -> Vec<String> {
        self.album_order
            .iter()
            .filter_map(|id| self.albums.get(id))
            .map(|album| album.name.clone())
            .collect()
    }
}

impl State {
    fn take_fault(&self, operation: Operation) -> Option<Fault> {
        let mut inner = self.inner.lock().expect("mock state poisoned");
        inner.faults.get_mut(&operation)?.pop_front()
    }
}

/// A running mock Immich server bound to an ephemeral loopback port.
pub struct MockImmich {
    base_url: String,
    state: Arc<State>,
}

impl MockImmich {
    /// Start the server with default credentials on an ephemeral loopback port.
    pub async fn start() -> MockImmich {
        Self::spawn().await
    }

    /// The base URL of the running server, for example `http://127.0.0.1:12345`.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Inject a fault that applies to the next request of `operation`.
    pub fn fail(&self, operation: Operation, fault: Fault) {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .faults
            .entry(operation)
            .or_default()
            .push_back(fault);
    }

    /// Remove any faults that have not yet been consumed.
    pub fn reset_faults(&self) {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .faults
            .clear();
    }

    /// Turn off password login, as an Immich admin can (R1.6).
    pub fn disable_password_login(&self) {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .password_login_disabled = true;
    }

    /// Require an issued, unrevoked session token on `validateToken`.
    ///
    /// The default accepts any bearer token, which keeps the older mock tests
    /// simple; session tests turn this on to model a revoked token.
    pub fn require_known_credentials(&self) {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .require_known_credentials = true;
    }

    /// The session tokens this server issued and has not revoked.
    ///
    /// The redaction test greps the log file for these, so it greps the token
    /// the real login actually received rather than a placeholder.
    #[must_use]
    pub fn issued_tokens(&self) -> Vec<String> {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .tokens
            .iter()
            .cloned()
            .collect()
    }

    /// Delete every session, as an admin "log out all devices" does.
    pub fn revoke_tokens(&self) {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .tokens
            .clear();
    }

    /// Reject `checksum` in `bulk-upload-check` as an unsupported format, as
    /// the real server does for a file type it cannot accept (R6.3).
    pub fn reject_format(&self, checksum: &str) {
        if let Some(checksum) = canonical_checksum(checksum) {
            self.state
                .inner
                .lock()
                .expect("mock state poisoned")
                .unsupported
                .insert(checksum);
        }
    }

    /// The number of `POST /assets/bulk-upload-check` requests that arrived.
    #[must_use]
    pub fn bulk_check_requests(&self) -> u64 {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .bulk_check_requests
    }

    /// The number of `POST /auth/login` requests that arrived (R1.4).
    #[must_use]
    pub fn login_requests(&self) -> u64 {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .login_requests
    }

    /// The number of `POST /auth/validateToken` requests that arrived, which is
    /// how many times the client probed the server (R6.6).
    #[must_use]
    pub fn validate_token_requests(&self) -> u64 {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .validate_token_requests
    }

    /// The `fileCreatedAt` and `fileModifiedAt` fields the last upload sent.
    ///
    /// Immich requires both; the double records them so a test can prove the
    /// client sent them (R3.1).
    #[must_use]
    pub fn last_upload_file_dates(&self) -> Option<(String, String)> {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .last_upload_dates
            .clone()
    }

    /// The number of `POST /assets` requests that arrived. A request that the
    /// server refused before reading a body still counts: what a test wants to
    /// prove is that the client never opened the upload at all.
    #[must_use]
    pub fn upload_requests(&self) -> u64 {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .upload_requests
    }

    /// Hold every upload open for `delay` before reading its body, so a test
    /// can watch how many the client runs at once.
    pub fn set_upload_delay(&self, delay: Duration) {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .upload_delay = Some(delay);
    }

    /// How many `POST /assets` requests are being handled right now.
    #[must_use]
    pub fn upload_concurrency(&self) -> u64 {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .uploads_in_flight
    }

    /// The most uploads that were ever in flight at one time.
    #[must_use]
    pub fn peak_upload_concurrency(&self) -> u64 {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .peak_uploads
    }

    /// Forgets the peak recorded so far, so a test can watch the next wave of
    /// uploads alone. The uploads in flight right now count as the new peak.
    pub fn reset_upload_peaks(&self) {
        let mut inner = self.state.inner.lock().expect("mock state poisoned");
        inner.peak_uploads = inner.uploads_in_flight;
    }

    /// Configure the one API key the server accepts. Without one the mock
    /// accepts any non-empty `x-api-key`, as it always has.
    pub fn set_api_key(&self, key: &str) {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .api_key = Some(key.to_string());
    }

    /// Grant the configured API key exactly `permissions`, so a test can prove
    /// a refusal for a permission the key lacks. Without this the key holds
    /// every permission.
    pub fn set_api_key_permissions(&self, permissions: &[&str]) {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .api_key_permissions = Some(permissions.iter().map(|value| value.to_string()).collect());
    }

    /// The number of `GET /server/about` requests that arrived, whether or not
    /// one was answered.
    #[must_use]
    pub fn about_requests(&self) -> u64 {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .about_requests
    }

    /// The credential the last answered `GET /server/about` carried:
    /// `api_key`, `bearer`, or nothing when no request was answered.
    #[must_use]
    pub fn about_auth(&self) -> Option<String> {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .about_auth
            .clone()
    }

    /// The number of `GET /api-keys/me` requests that arrived.
    #[must_use]
    pub fn api_key_me_requests(&self) -> u64 {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .api_key_me_requests
    }

    /// The `POST /assets` requests whose body the server read, in arrival
    /// order.
    ///
    /// A request answered `duplicate` from the `x-immich-checksum` header is
    /// not here: the server answers it before it reads the body, so a second
    /// request for bytes it already holds is counted by
    /// [`Self::upload_requests`] but never appears in this list.
    #[must_use]
    pub fn uploads(&self) -> Vec<UploadRecord> {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .uploads
            .clone()
    }

    /// Whether the asset was uploaded with a sidecar.
    #[must_use]
    pub fn has_sidecar(&self, id: &str) -> bool {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .asset(id)
            .is_some_and(|asset| asset.has_sidecar)
    }

    /// The id the server gave the asset holding `checksum` (base64 or hex),
    /// when the server holds one. A test can name the asset an upload made
    /// without reading the state row that is under test.
    #[must_use]
    pub fn asset_id_for_checksum(&self, checksum: &str) -> Option<String> {
        let inner = self.state.inner.lock().expect("mock state poisoned");
        let bytes = canonical_checksum(checksum)?;
        inner
            .asset_by_checksum(&bytes)
            .map(|asset| asset.id.clone())
    }

    /// The live photo video id linked to an asset, if any.
    #[must_use]
    pub fn live_photo_video_id(&self, id: &str) -> Option<String> {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .asset(id)
            .and_then(|asset| asset.live_photo_video_id.clone())
    }

    /// The name of the album that contains the asset, if any.
    #[must_use]
    pub fn album_for_asset(&self, asset_id: &str) -> Option<String> {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .album_for_asset(asset_id)
    }

    /// Every album name, in the order the albums were created (R3.4).
    #[must_use]
    pub fn album_names(&self) -> Vec<String> {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .album_names()
    }

    /// How many `POST /albums` requests arrived, whether or not one created an
    /// album. A client that must create no album asserts this is 0 (R3.4),
    /// which no album row alone can prove.
    #[must_use]
    pub fn create_album_requests(&self) -> u64 {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .create_album_requests
    }

    /// How many `PUT /albums/{id}/assets` requests arrived.
    #[must_use]
    pub fn add_album_requests(&self) -> u64 {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .add_album_requests
    }

    /// Every `PUT /assets/copy` call, in request order (R4.2).
    ///
    /// The recorded call is what proves the flags and the source and target
    /// the client sent; the outcome alone cannot.
    #[must_use]
    pub fn copy_calls(&self) -> Vec<AssetCopyCall> {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .copy_calls
            .clone()
    }

    /// Every `POST /stacks` call, in request order, with the stack it made.
    #[must_use]
    pub fn stack_calls(&self) -> Vec<StackCall> {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .stack_calls
            .clone()
    }

    /// The tag ids on one asset, in the order they were applied (R4.2).
    #[must_use]
    pub fn tags_of(&self, asset_id: &str) -> Vec<String> {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .asset_tags
            .get(asset_id)
            .cloned()
            .unwrap_or_default()
    }

    /// The stack one asset is in, with its members in stack order (R4.3).
    #[must_use]
    pub fn stack_of(&self, asset_id: &str) -> Option<StackCall> {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .stack_of(asset_id)
    }

    async fn spawn() -> MockImmich {
        let state = Arc::new(State {
            inner: Mutex::new(Inner::default()),
        });
        let router = Router::new()
            .route("/server/about", get(about))
            .route("/api-keys/me", get(api_key_me))
            .route("/auth/login", post(login))
            .route("/auth/validateToken", post(validate_token))
            .route("/assets/bulk-upload-check", post(bulk_upload_check))
            .route("/assets", post(upload).delete(delete_assets))
            .route("/assets/copy", put(copy_asset))
            .route("/assets/{id}", get(get_asset))
            .route("/stacks", post(create_stack))
            .route("/tags/assets", put(tag_assets))
            .route("/albums", post(create_album))
            .route("/albums/{id}/assets", put(add_assets_to_album))
            .with_state(Arc::clone(&state));

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock server address");
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve mock");
        });

        MockImmich {
            base_url: format!("http://{addr}"),
            state,
        }
    }
}

fn message(code: StatusCode, text: &str) -> Response {
    (code, Json(json!({ "message": text }))).into_response()
}

fn duplicate_response(id: &str) -> Response {
    Json(json!({ "id": id, "status": "duplicate" })).into_response()
}

/// Apply the injected fault for `operation`, if any, before the real handler runs.
/// Returns the short-circuit response when a fault applied.
async fn guard(state: &State, operation: Operation) -> Option<Response> {
    let fault = state.take_fault(operation)?;
    match fault {
        Fault::Status { code, retry_after } => {
            let mut response = StatusCode::from_u16(code)
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
            if let Some(seconds) = retry_after {
                response.headers_mut().insert(
                    header::RETRY_AFTER,
                    seconds.to_string().parse().expect("retry-after value"),
                );
            }
            Some(response)
        }
        Fault::ServerMessage {
            code,
            message: text,
        } => Some(message(
            StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            &text,
        )),
        Fault::Quota => Some(message(StatusCode::BAD_REQUEST, "Quota has been exceeded!")),
        Fault::PermissionDenied => Some(message(
            StatusCode::FORBIDDEN,
            "Missing required permission: asset.upload",
        )),
        Fault::UnsupportedFormat => Some(message(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Unsupported file type file.bin",
        )),
        Fault::Slow(delay) => {
            tokio::time::sleep(delay).await;
            None
        }
        // shortcut: drop the response body after the request is read. Immich has
        // no resumable upload, so the client restarts the whole POST either way.
        Fault::Disconnect => Some(
            Response::builder()
                .status(StatusCode::OK)
                .body(Body::from_stream(tokio_stream::iter([Err::<
                    axum::body::Bytes,
                    std::io::Error,
                >(
                    std::io::Error::new(std::io::ErrorKind::ConnectionReset, "mock disconnect"),
                )])))
                .expect("disconnect response"),
        ),
    }
}

async fn about(AxumState(state): AxumState<Arc<State>>, headers: HeaderMap) -> Response {
    state
        .inner
        .lock()
        .expect("mock state poisoned")
        .about_requests += 1;
    if let Some(response) = guard(&state, Operation::About).await {
        return response;
    }
    // The real `GET /server/about` is `@Authenticated({ permission:
    // Permission.ServerAbout })` (server.controller.ts, v3.3.1), so the double
    // refuses a request with no credential the same way.
    let auth = auth_kind(&headers);
    if auth.is_none() {
        return message(StatusCode::UNAUTHORIZED, "Authentication required");
    }
    if let Some(response) = api_key_permission_refusal(&state, &headers, "server.about") {
        return response;
    }
    state
        .inner
        .lock()
        .expect("mock state poisoned")
        .about_auth = auth.map(str::to_string);
    Json(json!({ "version": SERVER_VERSION })).into_response()
}

/// The credential a request carried, as the double saw it.
fn auth_kind(headers: &HeaderMap) -> Option<&'static str> {
    let api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|key| !key.is_empty());
    if api_key {
        return Some("api_key");
    }
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.is_empty())
        .then_some("bearer")
}

/// The configured key's refusal when the request carries an API key that lacks
/// `permission`, or was not the configured one.
///
/// A bearer token has no per-key permission list in the double, so it passes.
fn api_key_permission_refusal(
    state: &State,
    headers: &HeaderMap,
    permission: &str,
) -> Option<Response> {
    let key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .filter(|key| !key.is_empty())?;
    let inner = state.inner.lock().expect("mock state poisoned");
    if let Some(configured) = &inner.api_key
        && configured != key
    {
        return Some(message(
            StatusCode::UNAUTHORIZED,
            "Invalid authentication",
        ));
    }
    let granted = inner
        .api_key_permissions
        .as_ref()
        .is_none_or(|permissions| permissions.iter().any(|granted| granted == permission));
    (!granted).then(|| {
        message(
            StatusCode::FORBIDDEN,
            &format!("Missing required permission: {permission}"),
        )
    })
}

/// `GET /api-keys/me` (Stable, no permission beyond a credential).
///
/// The real server answers a bearer session with 403 `Not authenticated with
/// an API Key` (api-key.service.ts `getMine`, v3.3.1), which is the refusal
/// the diagnostics probe must show as a message.
async fn api_key_me(AxumState(state): AxumState<Arc<State>>, headers: HeaderMap) -> Response {
    state
        .inner
        .lock()
        .expect("mock state poisoned")
        .api_key_me_requests += 1;
    if let Some(response) = guard(&state, Operation::ApiKeyMe).await {
        return response;
    }
    let Some(key) = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .filter(|key| !key.is_empty())
    else {
        return message(
            StatusCode::FORBIDDEN,
            "Not authenticated with an API Key",
        );
    };
    let inner = state.inner.lock().expect("mock state poisoned");
    if let Some(configured) = &inner.api_key
        && configured != key
    {
        return message(
            StatusCode::UNAUTHORIZED,
            "Invalid authentication",
        );
    }
    let permissions = inner
        .api_key_permissions
        .clone()
        .unwrap_or_else(|| vec!["all".to_string()]);
    Json(json!({
        "id": "00000000-0000-4000-8000-0000000000a1",
        "name": "Mock API Key",
        "permissions": permissions,
        "createdAt": TIMESTAMP,
        "updatedAt": TIMESTAMP,
    }))
    .into_response()
}

async fn login(
    AxumState(state): AxumState<Arc<State>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    state
        .inner
        .lock()
        .expect("mock state poisoned")
        .login_requests += 1;
    if let Some(response) = guard(&state, Operation::Login).await {
        return response;
    }
    if state
        .inner
        .lock()
        .expect("mock state poisoned")
        .password_login_disabled
    {
        // The real server throws `UnauthorizedException('Password login has
        // been disabled')`, so the message, not the status, is the signal.
        return message(StatusCode::UNAUTHORIZED, "Password login has been disabled");
    }
    let email = body.get("email").and_then(|value| value.as_str());
    let password = body.get("password").and_then(|value| value.as_str());
    if email != Some("sync@example.com") || password != Some("correct horse") {
        return message(StatusCode::UNAUTHORIZED, "Invalid user credentials");
    }
    let user_id = uuid::Uuid::new_v4().to_string();
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    state
        .inner
        .lock()
        .expect("mock state poisoned")
        .tokens
        .insert(token.clone());
    (
        StatusCode::CREATED,
        Json(json!({
            "accessToken": token,
            "userId": user_id,
            "userEmail": "sync@example.com",
            "name": "Sync Tester",
            "isAdmin": false,
            "isOnboarded": true,
            "profileImagePath": "",
            "shouldChangePassword": false,
        })),
    )
        .into_response()
}

async fn validate_token(AxumState(state): AxumState<Arc<State>>, headers: HeaderMap) -> Response {
    state
        .inner
        .lock()
        .expect("mock state poisoned")
        .validate_token_requests += 1;
    if let Some(response) = guard(&state, Operation::ValidateToken).await {
        return response;
    }
    let valid = {
        let inner = state.inner.lock().expect("mock state poisoned");
        let api_key = headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok());
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
            });
        match (api_key, bearer) {
            (Some(key), _) if !key.is_empty() => match &inner.api_key {
                Some(configured) => key == configured,
                None => true,
            },
            (None, Some(token)) if !token.is_empty() => {
                !inner.require_known_credentials || inner.tokens.contains(token)
            }
            _ => false,
        }
    };
    if !valid {
        return message(StatusCode::UNAUTHORIZED, "Invalid authentication");
    }
    Json(json!({ "authStatus": true })).into_response()
}

async fn bulk_upload_check(
    AxumState(state): AxumState<Arc<State>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    state
        .inner
        .lock()
        .expect("mock state poisoned")
        .bulk_check_requests += 1;
    if let Some(response) = guard(&state, Operation::BulkUploadCheck).await {
        return response;
    }
    let Some(items) = body.get("assets").and_then(|value| value.as_array()) else {
        return message(StatusCode::BAD_REQUEST, "assets is required");
    };
    let inner = state.inner.lock().expect("mock state poisoned");
    let results: Vec<Value> = items
        .iter()
        .map(|item| {
            let id = item
                .get("id")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let checksum = item
                .get("checksum")
                .and_then(|value| value.as_str())
                .and_then(canonical_checksum);
            let unsupported = checksum
                .as_ref()
                .is_some_and(|checksum| inner.unsupported.contains(checksum));
            match checksum.and_then(|checksum| inner.asset_by_checksum(&checksum)) {
                Some(asset) => json!({
                    "id": id,
                    "action": "reject",
                    "reason": "duplicate",
                    "assetId": asset.id,
                    "isTrashed": asset.trashed,
                }),
                None if unsupported => json!({
                    "id": id,
                    "action": "reject",
                    "reason": "unsupported-format",
                }),
                None => json!({ "id": id, "action": "accept" }),
            }
        })
        .collect();
    Json(json!({ "results": results })).into_response()
}

async fn upload(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    // Counted from arrival, so a test sees how many requests the client has
    // open at once even while the bodies are still being read.
    let _in_flight = InFlight::enter(&state);
    let (delay, header_checksum) = {
        let mut inner = state.inner.lock().expect("mock state poisoned");
        inner.upload_requests += 1;
        let delay = inner.upload_delay;
        drop(inner);
        // Immich answers a known checksum from the header before it reads the
        // body: `AssetUploadInterceptor` checks `x-immich-checksum` and runs
        // before `FileUploadInterceptor`, so multer never parses the form of a
        // duplicate.
        let checksum = headers
            .get("x-immich-checksum")
            .and_then(|value| value.to_str().ok())
            .and_then(canonical_checksum);
        (delay, checksum)
    };
    if let Some(checksum) = &header_checksum {
        let inner = state.inner.lock().expect("mock state poisoned");
        if let Some(asset) = inner.asset_by_checksum(checksum) {
            return duplicate_response(&asset.id);
        }
    }
    // A test holds the upload here, before the body is read, so several
    // requests can be open at the same time.
    if let Some(delay) = delay {
        tokio::time::sleep(delay).await;
    }

    // The real server buffers the upload before it validates the format or the
    // quota, so a fault response never races the streaming request body. Only
    // the checksum short-circuit above answers earlier.
    let mut asset_data = Vec::new();
    let mut has_sidecar = false;
    let mut live_photo_video_id = None;
    let mut filename = None;
    let mut parts = Vec::new();
    let mut file_created_at = None;
    let mut file_modified_at = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return message(StatusCode::BAD_REQUEST, &error.to_string()),
        };
        let name = field.name().map(str::to_string);
        parts.push(name.clone().unwrap_or_default());
        match name.as_deref() {
            Some("assetData") => match field.bytes().await {
                Ok(bytes) => asset_data = bytes.to_vec(),
                Err(error) => return message(StatusCode::BAD_REQUEST, &error.to_string()),
            },
            Some("sidecarData") => match field.bytes().await {
                Ok(_) => has_sidecar = true,
                Err(error) => return message(StatusCode::BAD_REQUEST, &error.to_string()),
            },
            Some("livePhotoVideoId") => match field.text().await {
                Ok(text) => live_photo_video_id = Some(text),
                Err(error) => return message(StatusCode::BAD_REQUEST, &error.to_string()),
            },
            Some("filename") => match field.text().await {
                Ok(text) => filename = Some(text),
                Err(error) => return message(StatusCode::BAD_REQUEST, &error.to_string()),
            },
            Some("fileCreatedAt") => match field.text().await {
                Ok(text) => file_created_at = Some(text),
                Err(error) => return message(StatusCode::BAD_REQUEST, &error.to_string()),
            },
            Some("fileModifiedAt") => match field.text().await {
                Ok(text) => file_modified_at = Some(text),
                Err(error) => return message(StatusCode::BAD_REQUEST, &error.to_string()),
            },
            _ => {}
        }
    }
    if let (Some(created), Some(modified)) = (file_created_at, file_modified_at) {
        state
            .inner
            .lock()
            .expect("mock state poisoned")
            .last_upload_dates = Some((created, modified));
    }

    if let Some(response) = guard(&state, Operation::Upload).await {
        return response;
    }

    // Immich stores the checksum it computes from the body, not the header.
    let checksum = checksum_of(&asset_data);
    let mut inner = state.inner.lock().expect("mock state poisoned");
    inner.uploads.push(UploadRecord {
        filename: filename.unwrap_or_default(),
        has_sidecar,
        live_photo_video_id: live_photo_video_id.clone(),
        parts,
    });
    if let Some(asset) = inner.asset_by_checksum(&checksum) {
        return duplicate_response(&asset.id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    inner.insert_asset(Asset {
        id: id.clone(),
        checksum,
        trashed: false,
        has_sidecar,
        live_photo_video_id,
        stack_id: None,
    });
    (
        StatusCode::CREATED,
        Json(json!({ "id": id, "status": "created" })),
    )
        .into_response()
}

/// Counts one upload as in flight until the handler returns, however it
/// returns.
struct InFlight(Arc<State>);

impl InFlight {
    fn enter(state: &Arc<State>) -> Self {
        {
            let mut inner = state.inner.lock().expect("mock state poisoned");
            inner.uploads_in_flight += 1;
            inner.peak_uploads = inner.peak_uploads.max(inner.uploads_in_flight);
        }
        Self(Arc::clone(state))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut inner = self.0.inner.lock().expect("mock state poisoned");
        inner.uploads_in_flight = inner.uploads_in_flight.saturating_sub(1);
    }
}

/// Decode an Immich checksum: 28 characters is base64, anything else is hex.
fn canonical_checksum(encoded: &str) -> Option<Vec<u8>> {
    if encoded.len() == 28 {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()
    } else {
        hex::decode(encoded).ok()
    }
}

fn checksum_of(data: &[u8]) -> Vec<u8> {
    use sha1::Digest as _;
    sha1::Sha1::digest(data).to_vec()
}

fn asset_response(inner: &Inner, asset: &Asset) -> Value {
    let stack = inner.stack_of(&asset.id).map_or(Value::Null, |stack| {
        json!({
            "id": stack.stack_id,
            "primaryAssetId": stack.asset_ids.first(),
            "assetCount": stack.asset_ids.len(),
        })
    });
    // The double keeps no tag table of its own: the ids a tag request carried
    // are the tags, which is all the client's version flow reads back.
    let tags: Vec<Value> = inner
        .asset_tags
        .get(&asset.id)
        .map(|tags| {
            tags.iter()
                .map(|id| json!({ "id": id, "name": id, "value": id }))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": asset.id,
        "checksum": hex::encode(&asset.checksum),
        "type": "IMAGE",
        "originalFileName": "",
        "originalPath": "",
        "ownerId": OWNER_ID,
        "createdAt": TIMESTAMP,
        "updatedAt": TIMESTAMP,
        "fileCreatedAt": TIMESTAMP,
        "fileModifiedAt": TIMESTAMP,
        "localDateTime": TIMESTAMP,
        "thumbhash": "",
        "duration": 0,
        "height": 0,
        "width": 0,
        "isArchived": false,
        "isEdited": false,
        "isFavorite": false,
        "isOffline": false,
        "isTrashed": asset.trashed,
        "hasMetadata": asset.has_sidecar,
        "visibility": "timeline",
        "livePhotoVideoId": asset.live_photo_video_id,
        "stack": stack,
        "tags": tags,
    })
}

fn album_response(album: &Album) -> Value {
    json!({
        "id": album.id,
        "albumName": album.name,
        "description": "",
        "albumThumbnailAssetId": null,
        "albumUsers": [],
        "assetCount": album.asset_ids.len(),
        "createdAt": TIMESTAMP,
        "updatedAt": TIMESTAMP,
        "hasSharedLink": false,
        "isActivityEnabled": true,
        "shared": false,
    })
}

async fn get_asset(AxumState(state): AxumState<Arc<State>>, Path(id): Path<String>) -> Response {
    if let Some(response) = guard(&state, Operation::GetAsset).await {
        return response;
    }
    let inner = state.inner.lock().expect("mock state poisoned");
    match inner.asset(&id) {
        Some(asset) => Json(asset_response(&inner, asset)).into_response(),
        None => message(StatusCode::NOT_FOUND, "Asset not found"),
    }
}

async fn copy_asset(
    AxumState(state): AxumState<Arc<State>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Some(response) = guard(&state, Operation::Copy).await {
        return response;
    }
    let source = body.get("sourceId").and_then(|value| value.as_str());
    let target = body.get("targetId").and_then(|value| value.as_str());
    let flag = |name: &str| {
        body.get(name)
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    };
    let mut inner = state.inner.lock().expect("mock state poisoned");
    let (Some(source), Some(target)) = (source, target) else {
        return message(StatusCode::NOT_FOUND, "Asset not found");
    };
    inner.copy_calls.push(AssetCopyCall {
        source_id: source.to_string(),
        target_id: target.to_string(),
        albums: flag("albums"),
        shared_links: flag("sharedLinks"),
        favorite: flag("favorite"),
        sidecar: flag("sidecar"),
        stack: flag("stack"),
    });
    if !inner.has_asset(source) || !inner.has_asset(target) {
        return message(StatusCode::NOT_FOUND, "Asset not found");
    }
    // shortcut: the double records the flags and models the server's
    // `copyStack` (asset.service.ts, v3.3.1): no source stack is a no-op; a
    // target that is already stacked absorbs the source stack and the source
    // stack is then deleted — which, when both assets share one stack, means
    // that stack is deleted and its assets end up unstacked; only an
    // unstacked target joins the source stack. Model album, shared-link,
    // favorite, and sidecar state when a ticket asserts those states.
    if flag("stack") {
        let source_stack = inner
            .assets
            .get(source)
            .and_then(|asset| asset.stack_id.clone());
        let target_stack = inner
            .assets
            .get(target)
            .and_then(|asset| asset.stack_id.clone());
        match (source_stack, target_stack) {
            // `merge({sourceId, targetId})` moves the source stack's assets
            // onto the target stack — a no-op when they are the same stack —
            // and `delete(sourceId)` then drops the source stack row. The
            // assets the deleted stack still points at are left unstacked.
            (Some(source_id), Some(target_id)) => {
                if source_id != target_id
                    && let Some(members) = inner.stacks.remove(&source_id)
                {
                    for member_id in members {
                        if let Some(asset) = inner.assets.get_mut(&member_id) {
                            asset.stack_id = Some(target_id.clone());
                        }
                        if let Some(target_members) = inner.stacks.get_mut(&target_id)
                            && !target_members.contains(&member_id)
                        {
                            target_members.push(member_id);
                        }
                    }
                }
                inner.stacks.remove(&source_id);
                for asset in inner.assets.values_mut() {
                    if asset.stack_id.as_deref() == Some(source_id.as_str()) {
                        asset.stack_id = None;
                    }
                }
            }
            (Some(source_id), None) => {
                if let Some(members) = inner.stacks.get_mut(&source_id)
                    && !members.contains(&target.to_string())
                {
                    members.push(target.to_string());
                }
                if let Some(target_asset) = inner.assets.get_mut(target) {
                    target_asset.stack_id = Some(source_id);
                }
            }
            // The server returns before touching any stack when the source is
            // in none.
            (None, _) => {}
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn create_stack(
    AxumState(state): AxumState<Arc<State>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Some(response) = guard(&state, Operation::CreateStack).await {
        return response;
    }
    let Some(asset_ids) = body.get("assetIds").and_then(|value| value.as_array()) else {
        return message(StatusCode::BAD_REQUEST, "assetIds is required");
    };
    if asset_ids.len() < 2 {
        return message(StatusCode::BAD_REQUEST, "A stack needs at least two assets");
    }
    let asset_ids = id_list(Some(body.get("assetIds").expect("checked above")));
    let mut inner = state.inner.lock().expect("mock state poisoned");
    if !asset_ids.iter().all(|id| inner.has_asset(id)) {
        return message(StatusCode::NOT_FOUND, "Asset not found");
    }
    let stack_id = inner.create_stack(&asset_ids);
    inner.stack_calls.push(StackCall {
        asset_ids: asset_ids.clone(),
        stack_id: stack_id.clone(),
    });
    let assets: Vec<Value> = asset_ids
        .iter()
        .filter_map(|id| inner.asset(id))
        .map(|asset| asset_response(&inner, asset))
        .collect();
    (
        StatusCode::CREATED,
        Json(json!({
            "id": stack_id,
            "primaryAssetId": asset_ids.first(),
            "assets": assets,
        })),
    )
        .into_response()
}

async fn tag_assets(
    AxumState(state): AxumState<Arc<State>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Some(response) = guard(&state, Operation::TagAssets).await {
        return response;
    }
    let asset_ids = id_list(body.get("assetIds"));
    let tag_ids = id_list(body.get("tagIds"));
    let count = state
        .inner
        .lock()
        .expect("mock state poisoned")
        .tag(&asset_ids, &tag_ids);
    Json(json!({ "count": count })).into_response()
}

async fn delete_assets(
    AxumState(state): AxumState<Arc<State>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Some(response) = guard(&state, Operation::DeleteAssets).await {
        return response;
    }
    let force = body
        .get("force")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let ids = id_list(body.get("ids"));
    let mut inner = state.inner.lock().expect("mock state poisoned");
    for id in ids {
        if force {
            inner.remove_asset(&id);
        } else {
            inner.mark_trashed(&id);
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// The `POST /albums` handler counts its request before the fault runs, the
/// way `upload` does: a client that must create no album is proven by the
/// request count, so a faulted request must not hide from it.
async fn create_album(
    AxumState(state): AxumState<Arc<State>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    state
        .inner
        .lock()
        .expect("mock state poisoned")
        .create_album_requests += 1;
    if let Some(response) = guard(&state, Operation::CreateAlbum).await {
        return response;
    }
    let Some(name) = body.get("albumName").and_then(|value| value.as_str()) else {
        return message(StatusCode::BAD_REQUEST, "albumName is required");
    };
    let asset_ids = id_list(body.get("assetIds"));
    let mut inner = state.inner.lock().expect("mock state poisoned");
    let id = inner.create_album(name.to_string(), asset_ids);
    let album = inner.albums.get(&id).expect("album just created");
    (StatusCode::CREATED, Json(album_response(album))).into_response()
}

/// `PUT /albums/{id}/assets` counts its request the way `create_album` does.
async fn add_assets_to_album(
    AxumState(state): AxumState<Arc<State>>,
    Path(album_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    state
        .inner
        .lock()
        .expect("mock state poisoned")
        .add_album_requests += 1;
    if let Some(response) = guard(&state, Operation::AddAssetsToAlbum).await {
        return response;
    }
    let ids = id_list(body.get("ids"));
    let mut inner = state.inner.lock().expect("mock state poisoned");
    let Some(added) = inner.add_assets_to_album(&album_id, &ids) else {
        return message(StatusCode::NOT_FOUND, "Album not found");
    };
    let results: Vec<Value> = ids
        .iter()
        .map(|id| json!({ "id": id, "success": added.contains(id) }))
        .collect();
    Json(results).into_response()
}

fn id_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(|value| value.as_array())
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
