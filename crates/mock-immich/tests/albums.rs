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
        .post(format!("{}/api/assets", server.base_url()))
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
        .post(format!("{}/api/albums", server.base_url()))
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

/// `PUT /albums/{id}/assets` with one asset id, returning the per-asset result
/// the real server answers with.
async fn add_asset(
    client: &reqwest::Client,
    server: &MockImmich,
    album_id: &str,
    asset: &str,
) -> serde_json::Value {
    let response = client
        .put(format!(
            "{}/api/albums/{album_id}/assets",
            server.base_url()
        ))
        .json(&serde_json::json!({ "ids": [asset] }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let results: serde_json::Value = response.json().await.unwrap();
    assert_eq!(results[0]["id"], asset);
    results
}

#[tokio::test]
async fn add_assets_to_album_links_an_asset() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let asset = upload_asset(&client, &server, 1).await;

    let album: serde_json::Value = client
        .post(format!("{}/api/albums", server.base_url()))
        .json(&serde_json::json!({ "albumName": "Later" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let album_id = album["id"].as_str().unwrap();

    let results = add_asset(&client, &server, album_id, &asset).await;

    assert_eq!(results[0]["success"], true);
    assert_eq!(server.album_for_asset(&asset).as_deref(), Some("Later"));
}

#[tokio::test]
async fn adding_an_asset_twice_leaves_one_membership() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let asset = upload_asset(&client, &server, 1).await;
    let album: serde_json::Value = client
        .post(format!("{}/api/albums", server.base_url()))
        .json(&serde_json::json!({ "albumName": "Later" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let album_id = album["id"].as_str().unwrap();

    let first = add_asset(&client, &server, album_id, &asset).await;
    // The same asset added twice is how a duplicate upload and a fresh upload of
    // the same folder meet the same album: the request still succeeds, and the
    // server does not add a second membership.
    let second = add_asset(&client, &server, album_id, &asset).await;

    assert_eq!(first[0]["success"], true, "the first add links the asset");
    assert_eq!(
        second[0]["success"], false,
        "the album already holds the asset, so the server adds nothing"
    );
    assert_eq!(server.add_album_requests(), 2);
    assert_eq!(
        server.album_for_asset(&asset).as_deref(),
        Some("Later"),
        "the asset is in the album"
    );
}

#[tokio::test]
async fn the_album_counters_and_names_report_what_arrived() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let asset = upload_asset(&client, &server, 1).await;
    assert_eq!(
        (server.create_album_requests(), server.add_album_requests()),
        (0, 0),
        "an upload alone creates no album"
    );

    let mut ids = Vec::new();
    for name in ["Holiday", "Holiday"] {
        let album: serde_json::Value = client
            .post(format!("{}/api/albums", server.base_url()))
            .json(&serde_json::json!({ "albumName": name }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        ids.push(album["id"].as_str().expect("an album id").to_string());
    }
    for _ in 0..2 {
        client
            .put(format!(
                "{}/api/albums/{}/assets",
                server.base_url(),
                ids[0]
            ))
            .json(&serde_json::json!({ "ids": [asset] }))
            .send()
            .await
            .unwrap();
    }

    assert_eq!(server.create_album_requests(), 2);
    assert_eq!(server.add_album_requests(), 2);
    assert_eq!(
        server.album_names(),
        vec!["Holiday", "Holiday"],
        "both albums exist under the same name, which is what merging leaf folders looks like"
    );
    assert_eq!(server.album_for_asset(&asset).as_deref(), Some("Holiday"));
}

#[tokio::test]
async fn add_assets_to_an_unknown_album_is_not_found() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let asset = upload_asset(&client, &server, 1).await;

    let response = client
        .put(format!(
            "{}/api/albums/99999999-9999-4999-8999-999999999999/assets",
            server.base_url()
        ))
        .json(&serde_json::json!({ "ids": [asset] }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 404);
}

/// `PATCH /albums/{id}` with `UpdateAlbumDto.albumName` renames one album and
/// keeps its assets (#47).
#[tokio::test]
async fn updating_an_album_renames_it_and_keeps_its_assets() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let asset = upload_asset(&client, &server, 1).await;
    let album: serde_json::Value = client
        .post(format!("{}/api/albums", server.base_url()))
        .json(&serde_json::json!({ "albumName": "Vacation", "assetIds": [asset] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let album_id = album["id"].as_str().unwrap();

    let response = client
        .patch(format!("{}/api/albums/{album_id}", server.base_url()))
        .json(&serde_json::json!({ "albumName": "Vacation 2024" }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["id"], album_id, "the same album is renamed");
    assert_eq!(body["albumName"], "Vacation 2024");
    assert_eq!(
        server.album_for_asset(&asset).as_deref(),
        Some("Vacation 2024"),
        "the assets stay in the album"
    );
    assert_eq!(
        server.album_names(),
        vec!["Vacation 2024".to_string()],
        "and its name is the one the request carried"
    );
    assert_eq!(
        server.album_rename_calls(),
        vec![mock_immich::AlbumRenameCall {
            album_id: album_id.to_string(),
            album_name: "Vacation 2024".to_string(),
        }]
    );
    assert!(
        server
            .requests()
            .contains(&format!("PATCH /api/albums/{album_id}")),
        "{:?}",
        server.requests()
    );
}

/// #47: an album the server does not hold is a 404, which is what tells the
/// client to create it again rather than retry the rename.
#[tokio::test]
async fn updating_an_unknown_album_is_not_found() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();

    let response = client
        .patch(format!(
            "{}/api/albums/99999999-9999-4999-8999-999999999999",
            server.base_url()
        ))
        .json(&serde_json::json!({ "albumName": "Vacation 2024" }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 404);
    assert!(server.album_names().is_empty());
}

/// #47: Immich's `UpdateAlbumDto` names the field `albumName`; a body without
/// it renames nothing.
#[tokio::test]
async fn updating_an_album_without_a_name_is_refused() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    let album: serde_json::Value = client
        .post(format!("{}/api/albums", server.base_url()))
        .json(&serde_json::json!({ "albumName": "Vacation" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let album_id = album["id"].as_str().unwrap();

    let response = client
        .patch(format!("{}/api/albums/{album_id}", server.base_url()))
        .json(&serde_json::json!({ "name": "Vacation 2024" }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    assert_eq!(server.album_names(), vec!["Vacation".to_string()]);
}
