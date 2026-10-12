//! HTTP server implementation for the mock Immich double.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Multipart;
use axum::extract::State as AxumState;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::json;
use tokio::net::TcpListener;

/// The Immich server version the double reports.
const SERVER_VERSION: &str = "v3.3.1";

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

/// Builder for a [`MockImmich`] instance with non-default credentials.
#[derive(Clone, Default)]
pub struct MockImmichBuilder {
    email: Option<String>,
    password: Option<String>,
}

impl MockImmichBuilder {
    /// Set the email the login endpoint accepts.
    #[must_use]
    pub fn email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }

    /// Set the password the login endpoint accepts.
    #[must_use]
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Start the server on an ephemeral loopback port.
    pub async fn start(self) -> MockImmich {
        let config = Config {
            email: self.email.unwrap_or_else(|| "sync@example.com".into()),
            password: self.password.unwrap_or_else(|| "correct horse".into()),
        };
        MockImmich::spawn(config).await
    }
}

struct Config {
    email: String,
    password: String,
}

struct State {
    config: Config,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    faults: HashMap<Operation, VecDeque<Fault>>,
    assets: Vec<Asset>,
}

struct Asset {
    id: String,
    checksum: Vec<u8>,
    trashed: bool,
}

impl Inner {
    fn asset_by_checksum(&self, checksum: &[u8]) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.checksum == checksum)
    }

    fn has_asset(&self, id: &str) -> bool {
        self.assets.iter().any(|asset| asset.id == id)
    }

    fn mark_trashed(&mut self, id: &str) {
        if let Some(asset) = self.assets.iter_mut().find(|asset| asset.id == id) {
            asset.trashed = true;
        }
    }

    fn remove_asset(&mut self, id: &str) {
        self.assets.retain(|asset| asset.id != id);
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
        Self::builder().start().await
    }

    /// Start building a server with non-default credentials.
    #[must_use]
    pub fn builder() -> MockImmichBuilder {
        MockImmichBuilder::default()
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

    async fn spawn(config: Config) -> MockImmich {
        let state = Arc::new(State {
            config,
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
        Fault::Quota => Some(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": "Quota has been exceeded!" })),
            )
                .into_response(),
        ),
        Fault::PermissionDenied => Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Missing required permission: asset.upload" })),
            )
                .into_response(),
        ),
        Fault::UnsupportedFormat => Some(
            (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                Json(json!({ "message": "Unsupported file type file.bin" })),
            )
                .into_response(),
        ),
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
    let email = body.get("email").and_then(|value| value.as_str());
    let password = body.get("password").and_then(|value| value.as_str());
    if email != Some(state.config.email.as_str())
        || password != Some(state.config.password.as_str())
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "message": "Invalid user credentials" })),
        )
            .into_response();
    }
    let user_id = uuid::Uuid::new_v4().to_string();
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    (
        StatusCode::CREATED,
        Json(json!({
            "accessToken": token,
            "userId": user_id,
            "userEmail": state.config.email,
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
    let has_credential =
        headers.contains_key(header::AUTHORIZATION) || headers.contains_key("x-api-key");
    if !has_credential {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "message": "Invalid authentication" })),
        )
            .into_response();
    }
    Json(json!({ "authStatus": true })).into_response()
}

async fn bulk_upload_check(
    AxumState(state): AxumState<Arc<State>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Some(response) = guard(&state, Operation::BulkUploadCheck).await {
        return response;
    }
    let items = body.get("assets").and_then(|value| value.as_array());
    let Some(items) = items else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "assets is required" })),
        )
            .into_response();
    };
    let inner = state.inner.lock().expect("mock state poisoned");
    let results: Vec<serde_json::Value> = items
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
            match checksum.and_then(|checksum| inner.asset_by_checksum(&checksum)) {
                Some(asset) => json!({
                    "id": id,
                    "action": "reject",
                    "reason": "duplicate",
                    "assetId": asset.id,
                    "isTrashed": asset.trashed,
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
    if let Some(response) = guard(&state, Operation::Upload).await {
        return response;
    }
    let mut asset_data = Vec::new();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "message": error.to_string() })),
                )
                    .into_response();
            }
        };
        if field.name() == Some("assetData") {
            match field.bytes().await {
                Ok(bytes) => asset_data = bytes.to_vec(),
                Err(error) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "message": error.to_string() })),
                    )
                        .into_response();
                }
            }
        }
    }

    let checksum = headers
        .get("x-immich-checksum")
        .and_then(|value| value.to_str().ok())
        .and_then(canonical_checksum)
        .unwrap_or_else(|| checksum_of(&asset_data));

    let mut inner = state.inner.lock().expect("mock state poisoned");
    if let Some(asset) = inner.asset_by_checksum(&checksum) {
        return Json(json!({ "id": asset.id, "status": "duplicate" })).into_response();
    }
    let id = uuid::Uuid::new_v4().to_string();
    inner.assets.push(Asset {
        id: id.clone(),
        checksum,
        trashed: false,
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
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Asset not found" })),
        )
            .into_response(),
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
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "assetIds is required" })),
        )
            .into_response();
    };
    if asset_ids.len() < 2 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "A stack needs at least two assets" })),
        )
            .into_response();
    }
    let inner = state.inner.lock().expect("mock state poisoned");
    let all_known = asset_ids
        .iter()
        .all(|id| id.as_str().is_some_and(|id| inner.has_asset(id)));
    if !all_known {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Asset not found" })),
        )
            .into_response();
    }
    let primary = asset_ids[0].clone();
    let assets: Vec<serde_json::Value> = asset_ids.iter().map(|id| json!({ "id": id })).collect();
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
    let ids: Vec<String> = body
        .get("ids")
        .and_then(|value| value.as_array())
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
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
