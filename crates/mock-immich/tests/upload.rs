use mock_immich::MockImmich;
use reqwest::multipart::{Form, Part};

const HELLO_WORLD_SHA1: &str = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";
const HELLO_WORLD_BASE64: &str = "Kq5sNclPz7QV2+lfQIuc6R7oRu0=";

fn upload_form() -> Form {
    Form::new()
        .part(
            "assetData",
            Part::bytes(b"hello world".to_vec()).file_name("hello.txt"),
        )
        .text("filename", "hello.txt")
        .text("fileCreatedAt", "2026-01-01T00:00:00.000Z")
        .text("fileModifiedAt", "2026-01-01T00:00:00.000Z")
}

async fn upload(
    client: &reqwest::Client,
    server: &MockImmich,
    checksum: Option<&str>,
) -> reqwest::Response {
    let request = client.post(format!("{}/assets", server.base_url()));
    let request = match checksum {
        Some(checksum) => request.header("x-immich-checksum", checksum),
        None => request,
    };
    request.multipart(upload_form()).send().await.unwrap()
}

async fn bulk(client: &reqwest::Client, server: &MockImmich, checksum: &str) -> serde_json::Value {
    let body: serde_json::Value = client
        .post(format!("{}/assets/bulk-upload-check", server.base_url()))
        .json(&serde_json::json!({
            "assets": [{ "id": "hello.txt", "checksum": checksum }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["results"][0].clone()
}

#[tokio::test]
async fn bulk_upload_check_accepts_an_unknown_checksum() {
    let server = MockImmich::start().await;

    let result = bulk(&reqwest::Client::new(), &server, HELLO_WORLD_SHA1).await;

    assert_eq!(result["id"], "hello.txt");
    assert_eq!(result["action"], "accept");
}

#[tokio::test]
async fn uploading_a_new_checksum_returns_created() {
    let server = MockImmich::start().await;

    let response = upload(&reqwest::Client::new(), &server, Some(HELLO_WORLD_SHA1)).await;

    assert_eq!(response.status(), 201);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], "created");
    assert!(!body["id"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn uploading_a_known_checksum_returns_the_existing_asset_as_duplicate() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();

    let first: serde_json::Value = upload(&client, &server, Some(HELLO_WORLD_SHA1))
        .await
        .json()
        .await
        .unwrap();
    let second = upload(&client, &server, Some(HELLO_WORLD_SHA1)).await;

    assert_eq!(second.status(), 200);
    let second: serde_json::Value = second.json().await.unwrap();
    assert_eq!(second["status"], "duplicate");
    assert_eq!(second["id"], first["id"]);
}

#[tokio::test]
async fn a_bulk_check_sees_an_uploaded_asset_as_a_duplicate() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();

    upload(&client, &server, Some(HELLO_WORLD_SHA1)).await;

    let result = bulk(&client, &server, HELLO_WORLD_SHA1).await;

    assert_eq!(result["action"], "reject");
    assert_eq!(result["reason"], "duplicate");
    assert!(!result["assetId"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn the_server_hashes_the_body_when_no_checksum_header_is_sent() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();

    upload(&client, &server, None).await;

    let result = bulk(&client, &server, HELLO_WORLD_SHA1).await;

    assert_eq!(result["action"], "reject");
}

#[tokio::test]
async fn the_server_stores_the_body_hash_not_the_header() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let wrong_checksum = "0000000000000000000000000000000000000001";

    upload(&client, &server, Some(wrong_checksum)).await;

    let by_body = bulk(&client, &server, HELLO_WORLD_SHA1).await;
    let by_header = bulk(&client, &server, wrong_checksum).await;
    assert_eq!(by_body["action"], "reject");
    assert_eq!(by_header["action"], "accept");
}

#[tokio::test]
async fn a_duplicate_header_short_circuits_without_reading_the_body() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    upload(&client, &server, Some(HELLO_WORLD_SHA1)).await;

    let response = client
        .post(format!("{}/assets", server.base_url()))
        .header("x-immich-checksum", HELLO_WORLD_SHA1)
        .header("content-type", "multipart/form-data; boundary=garbage")
        .body("this is not a parsable multipart body")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], "duplicate");
}

#[tokio::test]
async fn a_base64_checksum_is_accepted() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();

    let first = upload(&client, &server, Some(HELLO_WORLD_BASE64)).await;
    assert_eq!(first.status(), 201);

    let second = upload(&client, &server, Some(HELLO_WORLD_BASE64)).await;
    assert_eq!(second.status(), 200);

    // The body hash and the header decode to the same bytes, so the hex
    // encoding sees the asset as a duplicate too.
    let result = bulk(&client, &server, HELLO_WORLD_SHA1).await;
    assert_eq!(result["action"], "reject");
}
