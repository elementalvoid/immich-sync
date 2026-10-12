use mock_immich::MockImmich;
use reqwest::multipart::{Form, Part};
use sha1::{Digest, Sha1};

fn body_for(n: u64) -> Vec<u8> {
    format!("asset-{n}").into_bytes()
}

fn checksum_for(n: u64) -> String {
    hex::encode(Sha1::digest(body_for(n)))
}

async fn upload(client: &reqwest::Client, server: &MockImmich, n: u64) -> String {
    let form = Form::new()
        .part("assetData", Part::bytes(body_for(n)).file_name("a.txt"))
        .text("filename", "a.txt")
        .text("fileCreatedAt", "2026-01-01T00:00:00.000Z")
        .text("fileModifiedAt", "2026-01-01T00:00:00.000Z");
    let body: serde_json::Value = client
        .post(format!("{}/assets", server.base_url()))
        .header("x-immich-checksum", checksum_for(n))
        .multipart(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["id"].as_str().unwrap().to_string()
}

async fn bulk_action(
    client: &reqwest::Client,
    server: &MockImmich,
    checksum: &str,
) -> serde_json::Value {
    let body: serde_json::Value = client
        .post(format!("{}/assets/bulk-upload-check", server.base_url()))
        .json(&serde_json::json!({
            "assets": [{ "id": "a.txt", "checksum": checksum }]
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
async fn copy_accepts_two_known_assets() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let source = upload(&client, &server, 1).await;
    let target = upload(&client, &server, 2).await;

    let response = client
        .put(format!("{}/assets/copy", server.base_url()))
        .json(&serde_json::json!({
            "sourceId": source,
            "targetId": target,
            "albums": true,
            "favorite": true,
            "sharedLinks": true,
            "sidecar": true,
            "stack": true,
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 204);
}

#[tokio::test]
async fn copy_rejects_an_unknown_asset() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let source = upload(&client, &server, 1).await;

    let response = client
        .put(format!("{}/assets/copy", server.base_url()))
        .json(&serde_json::json!({
            "sourceId": source,
            "targetId": "99999999-9999-4999-8999-999999999999",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn create_stack_returns_the_primary_asset() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let first = upload(&client, &server, 1).await;
    let second = upload(&client, &server, 2).await;

    let response = client
        .post(format!("{}/stacks", server.base_url()))
        .json(&serde_json::json!({ "assetIds": [first, second] }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 201);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["primaryAssetId"], first);
    assert_eq!(body["assets"][0]["id"], first);
    assert_eq!(body["assets"][1]["id"], second);
}

#[tokio::test]
async fn create_stack_returns_conformant_asset_objects() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let first = upload(&client, &server, 1).await;
    let second = upload(&client, &server, 2).await;

    let body: serde_json::Value = client
        .post(format!("{}/stacks", server.base_url()))
        .json(&serde_json::json!({ "assetIds": [first, second] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let asset = &body["assets"][0];
    for field in [
        "checksum",
        "createdAt",
        "duration",
        "fileCreatedAt",
        "fileModifiedAt",
        "hasMetadata",
        "height",
        "id",
        "isArchived",
        "isEdited",
        "isFavorite",
        "isOffline",
        "isTrashed",
        "localDateTime",
        "originalFileName",
        "originalPath",
        "ownerId",
        "thumbhash",
        "type",
        "updatedAt",
        "visibility",
        "width",
    ] {
        assert!(!asset[field].is_null(), "missing field {field}");
    }
    assert_eq!(asset["id"], first);
}

#[tokio::test]
async fn tag_assets_reports_the_tagged_count() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let first = upload(&client, &server, 1).await;
    let second = upload(&client, &server, 2).await;

    let response = client
        .put(format!("{}/tags/assets", server.base_url()))
        .json(&serde_json::json!({
            "assetIds": [first, second],
            "tagIds": ["11111111-1111-4111-8111-111111111111"],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["count"], 2);
}

#[tokio::test]
async fn delete_moves_an_asset_to_the_trash() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let id = upload(&client, &server, 1).await;

    let response = client
        .delete(format!("{}/assets", server.base_url()))
        .json(&serde_json::json!({ "ids": [id], "force": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);

    let result = bulk_action(&client, &server, &checksum_for(1)).await;
    assert_eq!(result["action"], "reject");
    assert_eq!(result["isTrashed"], true);
}

#[tokio::test]
async fn force_delete_removes_an_asset() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let id = upload(&client, &server, 1).await;

    client
        .delete(format!("{}/assets", server.base_url()))
        .json(&serde_json::json!({ "ids": [id], "force": true }))
        .send()
        .await
        .unwrap();

    let result = bulk_action(&client, &server, &checksum_for(1)).await;
    assert_eq!(result["action"], "accept");
}
