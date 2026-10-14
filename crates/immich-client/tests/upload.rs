//! The upload operations against the offline Immich double (SPEC R3, R6.3).

use immich_client::{
    AssetUpload, Auth, BandwidthLimit, BulkAction, BulkCheckItem, ClientError, ImmichClient,
    RejectReason, SessionToken,
};
use mock_immich::{Fault, MockImmich, Operation};

/// `sha1("hello world")`, hex (a known source of truth, not a recomputation).
const HELLO_WORLD_SHA1_HEX: &str = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";
/// The same digest, base64.
const HELLO_WORLD_SHA1_BASE64: &str = "Kq5sNclPz7QV2+lfQIuc6R7oRu0=";

fn client(server: &MockImmich) -> ImmichClient {
    ImmichClient::new(server.base_url()).expect("the mock URL is valid")
}

async fn token(client: &ImmichClient) -> SessionToken {
    client
        .login("sync@example.com", "correct horse")
        .await
        .expect("the mock accepts these credentials")
        .access_token
}

fn hello_world_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("create a temp dir");
    let path = directory.path().join("hello.txt");
    std::fs::write(&path, b"hello world").expect("write the fixture file");
    (directory, path)
}

async fn check(
    client: &ImmichClient,
    auth: Auth<'_>,
    checksum: &str,
) -> Vec<immich_client::BulkCheckResult> {
    client
        .bulk_upload_check(
            auth,
            &[BulkCheckItem {
                id: "hello.txt".to_string(),
                checksum: checksum.to_string(),
            }],
        )
        .await
        .expect("the mock answers bulk-upload-check")
}

#[tokio::test]
async fn bulk_check_accepts_an_unknown_checksum() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;

    let results = check(&client, Auth::Token(&token), HELLO_WORLD_SHA1_BASE64).await;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "hello.txt");
    assert_eq!(results[0].action, BulkAction::Accept);
}

#[tokio::test]
async fn bulk_check_rejects_a_known_checksum_as_a_duplicate_with_the_asset_id() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let (_directory, path) = hello_world_file();
    let first = upload(&client, &token, &path, HELLO_WORLD_SHA1_BASE64)
        .await
        .expect("the first upload is created");

    let results = check(&client, Auth::Token(&token), HELLO_WORLD_SHA1_BASE64).await;

    assert_eq!(
        results[0].action,
        BulkAction::Reject {
            reason: RejectReason::Duplicate,
            asset_id: Some(first.asset_id.clone()),
            is_trashed: false,
        }
    );
}

#[tokio::test]
async fn bulk_check_reports_a_hex_checksum_too() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let (_directory, path) = hello_world_file();
    upload(&client, &token, &path, HELLO_WORLD_SHA1_HEX)
        .await
        .expect("the first upload is created");

    let results = check(&client, Auth::Token(&token), HELLO_WORLD_SHA1_HEX).await;

    assert!(
        matches!(
            results[0].action,
            BulkAction::Reject {
                reason: RejectReason::Duplicate,
                ..
            }
        ),
        "{:?}",
        results[0].action
    );
}

#[tokio::test]
async fn bulk_check_reports_an_unsupported_format_as_its_own_reason() {
    let server = MockImmich::start().await;
    server.reject_format(HELLO_WORLD_SHA1_BASE64);
    let client = client(&server);
    let token = token(&client).await;

    let results = check(&client, Auth::Token(&token), HELLO_WORLD_SHA1_BASE64).await;

    assert_eq!(
        results[0].action,
        BulkAction::Reject {
            reason: RejectReason::UnsupportedFormat,
            asset_id: None,
            is_trashed: false,
        }
    );
}

#[tokio::test]
async fn a_401_on_bulk_check_is_an_invalid_token() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    server.fail(
        Operation::BulkUploadCheck,
        Fault::Status {
            code: 401,
            retry_after: None,
        },
    );

    let error = client
        .bulk_upload_check(
            Auth::Token(&token),
            &[BulkCheckItem {
                id: "hello.txt".to_string(),
                checksum: HELLO_WORLD_SHA1_BASE64.to_string(),
            }],
        )
        .await
        .expect_err("the mock refuses the token");

    assert!(matches!(error, ClientError::InvalidToken), "{error:?}");
}

/// 2023-11-14T22:13:20Z, a known instant, as the multipart fields carry it.
const FILE_CREATED_AT: &str = "2023-11-14T22:13:20.000Z";
const FILE_MODIFIED_AT: &str = "2023-11-14T22:14:00.000Z";

fn upload_request<'a>(path: &'a std::path::Path, checksum: &'a str) -> AssetUpload<'a> {
    AssetUpload {
        path,
        checksum,
        created_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
        modified_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_040),
        bandwidth: BandwidthLimit::default(),
    }
}

async fn upload(
    client: &ImmichClient,
    token: &SessionToken,
    path: &std::path::Path,
    checksum: &str,
) -> Result<immich_client::UploadResponse, ClientError> {
    client
        .upload_asset(Auth::Token(token), &upload_request(path, checksum))
        .await
}

#[tokio::test]
async fn an_api_key_uploads_too() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let (_directory, path) = hello_world_file();

    let response = client
        .upload_asset(
            Auth::ApiKey("api-key-1234"),
            &upload_request(&path, HELLO_WORLD_SHA1_BASE64),
        )
        .await
        .expect("the mock accepts any non-empty API key");

    assert!(!response.asset_id.is_empty());
    assert!(!response.duplicate);
}

#[tokio::test]
async fn uploading_a_new_file_returns_created_with_the_asset_id() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let (_directory, path) = hello_world_file();

    let response = upload(&client, &token, &path, HELLO_WORLD_SHA1_BASE64)
        .await
        .expect("the mock accepts the upload");

    assert!(
        !response.asset_id.is_empty(),
        "success carries the asset id"
    );
    assert!(!response.duplicate, "a 201 is a created asset");
    assert_eq!(server.upload_requests(), 1);
}

#[tokio::test]
async fn the_upload_carries_the_file_dates_immich_requires() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let (_directory, path) = hello_world_file();

    upload(&client, &token, &path, HELLO_WORLD_SHA1_BASE64)
        .await
        .expect("the mock accepts the upload");

    assert_eq!(
        server.last_upload_file_dates(),
        Some((FILE_CREATED_AT.to_string(), FILE_MODIFIED_AT.to_string())),
        "R3.1: fileCreatedAt and fileModifiedAt must arrive as ISO 8601"
    );
}

#[tokio::test]
async fn uploading_a_known_checksum_returns_the_existing_asset_as_duplicate() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let (_directory, path) = hello_world_file();

    let first = upload(&client, &token, &path, HELLO_WORLD_SHA1_BASE64)
        .await
        .expect("the first upload is created");
    let second = upload(&client, &token, &path, HELLO_WORLD_SHA1_BASE64)
        .await
        .expect("the duplicate is still a success (R3.2)");

    assert!(second.duplicate, "a 200 is the duplicate answer");
    assert_eq!(second.asset_id, first.asset_id);
}

/// A server that never reads a request body, which is what a duplicate
/// short-circuit does to a real Immich: it answers from the header and hangs
/// up while the body is still being sent (#19). One entry per connection:
/// `None` closes without a reply, `Some` answers `200` with that JSON body.
async fn server_that_never_reads_a_body(answers: &'static [Option<&'static str>]) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the misbehaving server");
    let address = listener.local_addr().expect("a loopback address");
    tokio::spawn(async move {
        for answer in answers {
            let (mut socket, _) = listener.accept().await.expect("the client connects");
            let mut head = [0_u8; 4096];
            let _ = socket.read(&mut head).await;
            if let Some(answer) = answer {
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n",
                    answer.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(answer.as_bytes()).await;
            }
            // Closing a socket that never read the request body resets the
            // connection, so an answer can also be lost in flight.
            drop(socket);
        }
    });
    format!("http://{address}")
}

/// `bulk-upload-check` answering that the server holds the asset under
/// `asset_id`.
const SERVER_HOLDS_IT: &str = r#"{"results":[{"id":"hello.txt","action":"reject",
    "reason":"duplicate","assetId":"kept","isTrashed":false}]}"#;

/// The same answer, about a file this upload knows nothing of.
const SERVER_HOLDS_ANOTHER_FILE: &str = r#"{"results":[{"id":"other.txt","action":"reject",
    "reason":"duplicate","assetId":"someone-elses","isTrashed":false}]}"#;

#[tokio::test]
async fn a_cut_off_upload_is_resolved_by_asking_whether_the_server_kept_it() {
    let base_url = server_that_never_reads_a_body(&[None, Some(SERVER_HOLDS_IT)]).await;
    let client = ImmichClient::new(&base_url).expect("a loopback URL is valid");
    let (_directory, path) = hello_world_file();

    let response = client
        .upload_asset(
            Auth::ApiKey("api-key-1234"),
            &upload_request(&path, HELLO_WORLD_SHA1_BASE64),
        )
        .await
        .expect("the server says it already has these bytes");

    assert!(
        response.duplicate,
        "an upload the server hung up on is the duplicate it reported (#19)"
    );
    assert_eq!(response.asset_id, "kept");
}

#[tokio::test]
async fn an_answer_about_another_file_does_not_resolve_the_upload() {
    let base_url = server_that_never_reads_a_body(&[None, Some(SERVER_HOLDS_ANOTHER_FILE)]).await;
    let client = ImmichClient::new(&base_url).expect("a loopback URL is valid");
    let (_directory, path) = hello_world_file();

    let error = client
        .upload_asset(
            Auth::ApiKey("api-key-1234"),
            &upload_request(&path, HELLO_WORLD_SHA1_BASE64),
        )
        .await
        .expect_err("another file's answer says nothing about this upload");

    // A row that already carries an asset id is never uploaded again, so
    // another file's id would be trusted and persisted (#19). What is left is
    // the transport failure the upload started with.
    assert!(
        matches!(error, ClientError::Unreachable(_)),
        "an answer for another id leaves the upload unanswered (#19): {error:?}"
    );
}

#[tokio::test]
async fn a_server_that_answers_nothing_at_all_is_a_transient_failure() {
    let base_url = server_that_never_reads_a_body(&[None, None]).await;
    let client = ImmichClient::new(&base_url).expect("a loopback URL is valid");
    let (_directory, path) = hello_world_file();

    let error = client
        .upload_asset(
            Auth::ApiKey("api-key-1234"),
            &upload_request(&path, HELLO_WORLD_SHA1_BASE64),
        )
        .await
        .expect_err("neither the upload nor the question is answered");

    // Never an invented success: with nothing to go on, the transport failure
    // stays transient, so #7's retry uploads the same bytes again.
    assert!(
        matches!(error, ClientError::Unreachable(_)),
        "an unanswered upload is transient (#19): {error:?}"
    );
}

#[tokio::test]
async fn an_answer_to_an_upload_the_server_never_read_is_only_ever_a_duplicate() {
    let base_url = server_that_never_reads_a_body(&[
        Some(r#"{"id":"hostile","status":"duplicate"}"#),
        Some(SERVER_HOLDS_IT),
    ])
    .await;
    let client = ImmichClient::new(&base_url).expect("a loopback URL is valid");
    let (_directory, path) = hello_world_file();

    // The answer races the upload it never read, and the race can go either
    // way. Both outcomes are safe, and both must be: never an asset the server
    // did not store (#19).
    match client
        .upload_asset(
            Auth::ApiKey("api-key-1234"),
            &upload_request(&path, HELLO_WORLD_SHA1_BASE64),
        )
        .await
    {
        Ok(response) => assert!(
            response.duplicate,
            "only a duplicate claim is believable here (#19): {response:?}"
        ),
        Err(error) => assert!(
            matches!(error, ClientError::Unreachable(_)),
            "a lost answer is transient, so the retry resolves it (#19): {error:?}"
        ),
    }
}

#[tokio::test]
async fn an_unsupported_format_upload_is_its_own_error() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let (_directory, path) = hello_world_file();
    server.fail(Operation::Upload, Fault::UnsupportedFormat);

    let error = upload(&client, &token, &path, HELLO_WORLD_SHA1_BASE64)
        .await
        .expect_err("415 is not a success");

    assert!(
        matches!(error, ClientError::UnsupportedFormat(_)),
        "415 is the permanent unsupported-format class (R6.3): {error:?}"
    );
}

#[tokio::test]
async fn the_permanent_failure_classes_are_typed() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let (_directory, path) = hello_world_file();

    server.fail(Operation::Upload, Fault::Quota);
    let error = upload(&client, &token, &path, HELLO_WORLD_SHA1_BASE64)
        .await
        .expect_err("a quota refusal is an error");
    assert!(matches!(error, ClientError::QuotaExceeded(_)), "{error:?}");

    server.fail(Operation::Upload, Fault::PermissionDenied);
    let error = upload(&client, &token, &path, HELLO_WORLD_SHA1_BASE64)
        .await
        .expect_err("a permission refusal is an error");
    assert!(
        matches!(error, ClientError::PermissionDenied(_)),
        "{error:?}"
    );

    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 413,
            retry_after: None,
        },
    );
    let error = upload(&client, &token, &path, HELLO_WORLD_SHA1_BASE64)
        .await
        .expect_err("a too-large refusal is an error");
    assert!(matches!(error, ClientError::TooLarge(_)), "{error:?}");
}

#[tokio::test]
async fn a_failed_upload_reports_the_server_message() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let (_directory, path) = hello_world_file();
    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 409,
            retry_after: None,
        },
    );

    let error = upload(&client, &token, &path, HELLO_WORLD_SHA1_BASE64)
        .await
        .expect_err("409 is a refusal");

    // 409 has no named class in R6.4; it still reaches the caller as a typed
    // status, not as a transport error.
    assert!(
        matches!(error, ClientError::Protocol { status: 409, .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_missing_local_file_is_a_local_read_error_not_a_network_one() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let missing = std::path::Path::new("/nonexistent/immich-sync/missing.txt");
    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 500,
            retry_after: None,
        },
    );

    let error = upload(&client, &token, missing, HELLO_WORLD_SHA1_BASE64)
        .await
        .expect_err("the file does not exist");

    assert!(matches!(error, ClientError::LocalRead(_)), "{error:?}");
}
