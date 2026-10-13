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
    BulkUploadCheck,
    Upload,
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
    /// Checksums `bulk-upload-check` rejects as an unsupported format.
    unsupported: HashSet<Vec<u8>>,
    albums: HashMap<String, Album>,
    /// How many `POST /assets/bulk-upload-check` requests arrived.
    bulk_check_requests: u64,
    /// How many `POST /assets` requests arrived, whether or not one uploaded.
    upload_requests: u64,
    /// Immich's `passwordLogin.enabled` system config.
    password_login_disabled: bool,
    /// The key `x-api-key` must carry, once one is configured.
    api_key: Option<String>,
    /// The session tokens this server issued and has not revoked.
    tokens: HashSet<String>,
    /// When set, only an issued, unrevoked token validates.
    require_known_credentials: bool,
}

struct Asset {
    id: String,
    checksum: Vec<u8>,
    trashed: bool,
    has_sidecar: bool,
    live_photo_video_id: Option<String>,
}

struct Album {
    id: String,
    name: String,
    asset_ids: Vec<String>,
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

    fn remove_asset(&mut self, id: &str) {
        if let Some(asset) = self.assets.remove(id) {
            self.checksums.remove(&asset.checksum);
        }
    }

    fn create_album(&mut self, name: String, asset_ids: Vec<String>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
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

    /// Configure the one API key the server accepts. Without one the mock
    /// accepts any non-empty `x-api-key`, as it always has.
    pub fn set_api_key(&self, key: &str) {
        self.state
            .inner
            .lock()
            .expect("mock state poisoned")
            .api_key = Some(key.to_string());
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

    async fn spawn() -> MockImmich {
        let state = Arc::new(State {
            inner: Mutex::new(Inner::default()),
        });
        let router = Router::new()
            .route("/server/about", get(about))
            .route("/auth/login", post(login))
            .route("/auth/validateToken", post(validate_token))
            .route("/assets/bulk-upload-check", post(bulk_upload_check))
            .route("/assets", post(upload).delete(delete_assets))
            .route("/assets/copy", put(copy_asset))
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

async fn about(AxumState(state): AxumState<Arc<State>>) -> Response {
    if let Some(response) = guard(&state, Operation::About).await {
        return response;
    }
    Json(json!({ "version": SERVER_VERSION })).into_response()
}

async fn login(
    AxumState(state): AxumState<Arc<State>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
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
    state
        .inner
        .lock()
        .expect("mock state poisoned")
        .upload_requests += 1;

    // Immich answers a known checksum from the header before it reads the body.
    let header_checksum = headers
        .get("x-immich-checksum")
        .and_then(|value| value.to_str().ok())
        .and_then(canonical_checksum);
    if let Some(checksum) = &header_checksum {
        let inner = state.inner.lock().expect("mock state poisoned");
        if let Some(asset) = inner.asset_by_checksum(checksum) {
            return duplicate_response(&asset.id);
        }
    }

    // The real server buffers the upload before it validates the format or the
    // quota, so a fault response never races the streaming request body. Only
    // the checksum short-circuit above answers earlier.
    let mut asset_data = Vec::new();
    let mut has_sidecar = false;
    let mut live_photo_video_id = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return message(StatusCode::BAD_REQUEST, &error.to_string()),
        };
        let name = field.name().map(str::to_string);
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
            _ => {}
        }
    }

    if let Some(response) = guard(&state, Operation::Upload).await {
        return response;
    }

    // Immich stores the checksum it computes from the body, not the header.
    let checksum = checksum_of(&asset_data);
    let mut inner = state.inner.lock().expect("mock state poisoned");
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
    });
    (
        StatusCode::CREATED,
        Json(json!({ "id": id, "status": "created" })),
    )
        .into_response()
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

fn asset_response(asset: &Asset) -> Value {
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

async fn copy_asset(
    AxumState(state): AxumState<Arc<State>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Some(response) = guard(&state, Operation::Copy).await {
        return response;
    }
    let source = body.get("sourceId").and_then(|value| value.as_str());
    let target = body.get("targetId").and_then(|value| value.as_str());
    let inner = state.inner.lock().expect("mock state poisoned");
    match (source, target) {
        (Some(source), Some(target)) if inner.has_asset(source) && inner.has_asset(target) => {
            StatusCode::NO_CONTENT.into_response()
        }
        _ => message(StatusCode::NOT_FOUND, "Asset not found"),
    }
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
    let inner = state.inner.lock().expect("mock state poisoned");
    let all_known = asset_ids
        .iter()
        .all(|id| id.as_str().is_some_and(|id| inner.has_asset(id)));
    if !all_known {
        return message(StatusCode::NOT_FOUND, "Asset not found");
    }
    let primary = asset_ids[0].clone();
    let assets: Vec<Value> = asset_ids
        .iter()
        .filter_map(|id| id.as_str())
        .filter_map(|id| inner.asset(id))
        .map(asset_response)
        .collect();
    (
        StatusCode::CREATED,
        Json(json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "primaryAssetId": primary,
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
    let count = body
        .get("assetIds")
        .and_then(|value| value.as_array())
        .map_or(0, Vec::len);
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

async fn create_album(
    AxumState(state): AxumState<Arc<State>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
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

async fn add_assets_to_album(
    AxumState(state): AxumState<Arc<State>>,
    Path(album_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
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
