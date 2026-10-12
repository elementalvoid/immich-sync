use mock_immich::MockImmich;
use reqwest::multipart::{Form, Part};
use sha1::{Digest, Sha1};

fn checksum(n: u64) -> String {
    hex::encode(Sha1::digest(format!("album-asset-{n}")))
}

async fn upload_asset(client: &reqwest::Client, server: &MockImmich, n: u64) -> String {
    let body = format!("album-asset-{n}");
    let form = Form::new()
        .part(
            "assetData",
            Part::bytes(body.into_bytes()).file_name("a.txt"),
        )
        .text("filename", "a.txt")
        .text("fileCreatedAt", "2026-01-01T00:00:00.000Z")
        .text("fileModifiedAt", "2026-01-01T00:00:00.000Z");
    let response: serde_json::Value = client
        .post(format!("{}/assets", server.base_url()))
        .header("x-immich-checksum", checksum(n))
        .multipart(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    response["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn create_album_links_its_initial_assets() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let asset = upload_asset(&client, &server, 1).await;

    let response = client
        .post(format!("{}/albums", server.base_url()))
        .json(&serde_json::json!({
            "albumName": "Vacation",
            "assetIds": [asset],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 201);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["albumName"], "Vacation");
    assert_eq!(body["assetCount"], 1);
    assert!(!body["id"].as_str().unwrap().is_empty());
    assert_eq!(server.album_for_asset(&asset).as_deref(), Some("Vacation"));
}

#[tokio::test]
async fn add_assets_to_album_links_an_asset() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let asset = upload_asset(&client, &server, 1).await;

    let album: serde_json::Value = client
        .post(format!("{}/albums", server.base_url()))
        .json(&serde_json::json!({ "albumName": "Later" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let album_id = album["id"].as_str().unwrap();

    let response = client
        .put(format!("{}/albums/{album_id}/assets", server.base_url()))
        .json(&serde_json::json!({ "ids": [asset] }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let results: serde_json::Value = response.json().await.unwrap();
    assert_eq!(results[0]["id"], asset);
    assert_eq!(results[0]["success"], true);
    assert_eq!(server.album_for_asset(&asset).as_deref(), Some("Later"));
}

#[tokio::test]
async fn add_assets_to_an_unknown_album_is_not_found() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let asset = upload_asset(&client, &server, 1).await;

    let response = client
        .put(format!(
            "{}/albums/99999999-9999-4999-8999-999999999999/assets",
            server.base_url()
        ))
        .json(&serde_json::json!({ "ids": [asset] }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 404);
}
