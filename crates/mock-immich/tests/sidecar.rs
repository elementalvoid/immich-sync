use mock_immich::MockImmich;
use reqwest::multipart::{Form, Part};
use sha1::{Digest, Sha1};

fn checksum(body: &[u8]) -> String {
    hex::encode(Sha1::digest(body))
}

async fn upload(
    client: &reqwest::Client,
    server: &MockImmich,
    body: &[u8],
    sidecar: Option<&[u8]>,
    live_photo_video_id: Option<&str>,
) -> serde_json::Value {
    let mut form = Form::new()
        .part("assetData", Part::bytes(body.to_vec()).file_name("a.jpg"))
        .text("filename", "a.jpg")
        .text("fileCreatedAt", "2026-01-01T00:00:00.000Z")
        .text("fileModifiedAt", "2026-01-01T00:00:00.000Z");
    if let Some(sidecar) = sidecar {
        form = form.part(
            "sidecarData",
            Part::bytes(sidecar.to_vec()).file_name("a.jpg.xmp"),
        );
    }
    if let Some(id) = live_photo_video_id {
        form = form.text("livePhotoVideoId", id.to_string());
    }
    client
        .post(format!("{}/assets", server.base_url()))
        .header("x-immich-checksum", checksum(body))
        .multipart(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_sidecar_is_recorded_with_its_asset() {
    let server = MockImmich::start().await;

    let body = upload(
        &reqwest::Client::new(),
        &server,
        b"photo bytes",
        Some(b"<xmp>sidecar</xmp>"),
        None,
    )
    .await;
    let id = body["id"].as_str().unwrap();

    assert!(server.has_sidecar(id));
}

#[tokio::test]
async fn an_asset_without_a_sidecar_is_not_marked() {
    let server = MockImmich::start().await;

    let body = upload(&reqwest::Client::new(), &server, b"photo bytes", None, None).await;
    let id = body["id"].as_str().unwrap();

    assert!(!server.has_sidecar(id));
}

#[tokio::test]
async fn a_live_photo_video_id_is_recorded() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();

    let video = upload(&client, &server, b"video bytes", None, None).await;
    let video_id = video["id"].as_str().unwrap().to_string();

    let photo = upload(&client, &server, b"photo bytes", None, Some(&video_id)).await;
    let photo_id = photo["id"].as_str().unwrap();

    assert_eq!(server.live_photo_video_id(photo_id), Some(video_id));
}
