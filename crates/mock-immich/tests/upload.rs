use mock_immich::MockImmich;
use reqwest::multipart::{Form, Part};

const HELLO_WORLD_SHA1: &str = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";

fn upload_form(body: &'static [u8]) -> Form {
    Form::new()
        .part("assetData", Part::bytes(body).file_name("hello.txt"))
        .text("filename", "hello.txt")
        .text("fileCreatedAt", "2026-01-01T00:00:00.000Z")
        .text("fileModifiedAt", "2026-01-01T00:00:00.000Z")
}

async fn upload(
    client: &reqwest::Client,
    server: &MockImmich,
    body: &'static [u8],
) -> reqwest::Response {
    client
        .post(format!("{}/assets", server.base_url()))
        .header("x-immich-checksum", HELLO_WORLD_SHA1)
        .multipart(upload_form(body))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn bulk_upload_check_accepts_an_unknown_checksum() {
    let server = MockImmich::start().await;

    let body: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/assets/bulk-upload-check", server.base_url()))
        .json(&serde_json::json!({
            "assets": [{ "id": "hello.txt", "checksum": HELLO_WORLD_SHA1 }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["results"][0]["id"], "hello.txt");
    assert_eq!(body["results"][0]["action"], "accept");
}

#[tokio::test]
async fn uploading_a_new_checksum_returns_created() {
    let server = MockImmich::start().await;

    let response = upload(&reqwest::Client::new(), &server, b"hello world").await;

    assert_eq!(response.status(), 201);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], "created");
    assert!(!body["id"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn uploading_a_known_checksum_returns_the_existing_asset_as_duplicate() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();

    let first: serde_json::Value = upload(&client, &server, b"hello world")
        .await
        .json()
        .await
        .unwrap();
    let second = upload(&client, &server, b"hello world").await;

    assert_eq!(second.status(), 200);
    let second: serde_json::Value = second.json().await.unwrap();
    assert_eq!(second["status"], "duplicate");
    assert_eq!(second["id"], first["id"]);
}

#[tokio::test]
async fn a_bulk_check_sees_an_uploaded_asset_as_a_duplicate() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();

    upload(&client, &server, b"hello world").await;

    let body: serde_json::Value = client
        .post(format!("{}/assets/bulk-upload-check", server.base_url()))
        .json(&serde_json::json!({
            "assets": [{ "id": "hello.txt", "checksum": HELLO_WORLD_SHA1 }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["results"][0]["action"], "reject");
    assert_eq!(body["results"][0]["reason"], "duplicate");
    assert!(!body["results"][0]["assetId"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn the_server_hashes_the_body_when_no_checksum_header_is_sent() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();

    client
        .post(format!("{}/assets", server.base_url()))
        .multipart(upload_form(b"hello world"))
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = client
        .post(format!("{}/assets/bulk-upload-check", server.base_url()))
        .json(&serde_json::json!({
            "assets": [{ "id": "hello.txt", "checksum": HELLO_WORLD_SHA1 }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["results"][0]["action"], "reject");
}
