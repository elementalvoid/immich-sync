//! The sync tag's wire contract against the offline Immich double (SPEC R3.7).
//!
//! `PUT /tags` is Immich's upsert: the body names the tags, and the answer
//! carries one tag per name with the id the client caches. The double records
//! each call, so these tests assert the request the client sent and not only
//! the id it kept: the double is more forgiving than the real server.

use immich_client::{AssetUpload, Auth, BandwidthLimit, ImmichClient, SessionToken};
use mock_immich::{MockImmich, TagAssetCall, TagUpsertCall};

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

/// Uploads one small file and returns its asset id.
async fn uploaded_asset(client: &ImmichClient, token: &SessionToken) -> String {
    let directory = tempfile::tempdir().expect("create a temp dir");
    let path = directory.path().join("photo.jpg");
    std::fs::write(&path, b"photo bytes").expect("write the fixture");
    client
        .upload_asset(
            Auth::Token(token),
            &AssetUpload {
                path: &path,
                checksum: "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
                created_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
                modified_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_040),
                sidecar: None,
                live_photo_video_id: None,
                bandwidth: BandwidthLimit::default(),
            },
        )
        .await
        .expect("the upload is created")
        .asset_id
}

#[tokio::test]
async fn the_tag_upsert_sends_the_names_and_returns_the_servers_id() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;

    let id = client
        .upsert_tag(Auth::Token(&token), "immich-sync")
        .await
        .expect("the mock upserts the tag");

    // Immich v3.3.1 `PUT /tags`: `TagUpsertDto { tags: string[] }`, answered
    // with `TagResponseDto[]`, one entry per name.
    assert_eq!(
        server.upsert_calls(),
        vec![TagUpsertCall {
            tags: vec!["immich-sync".to_string()],
        }],
        "R3.7: the upsert names the tags and nothing else"
    );
    assert!(
        server.requests().iter().any(|line| line == "PUT /api/tags"),
        "R3.7: the upsert is PUT /api/tags: {:?}",
        server.requests()
    );
    assert_eq!(
        server.tag_id("immich-sync").as_deref(),
        Some(id.as_str()),
        "R3.7: the id the client keeps is the one the server answered"
    );
}

#[tokio::test]
async fn upserting_the_same_tag_twice_reuses_the_one_tag() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;

    let first = client
        .upsert_tag(Auth::Token(&token), "immich-sync")
        .await
        .expect("the first upsert creates the tag");
    let second = client
        .upsert_tag(Auth::Token(&token), "immich-sync")
        .await
        .expect("the second upsert reuses the tag");

    assert_eq!(first, second, "R3.7: one tag named immich-sync");
    assert_eq!(
        server.tag_names(),
        vec!["immich-sync".to_string()],
        "R3.7: the upsert created exactly one tag"
    );
}

#[tokio::test]
async fn tagging_assets_sends_the_tag_ids_and_the_asset_ids() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let asset_id = uploaded_asset(&client, &token).await;
    let tag_id = client
        .upsert_tag(Auth::Token(&token), "immich-sync")
        .await
        .expect("the mock upserts the tag");

    client
        .bulk_tag_assets(
            Auth::Token(&token),
            std::slice::from_ref(&tag_id),
            std::slice::from_ref(&asset_id),
        )
        .await
        .expect("the mock tags the asset");

    // Immich v3.3.1 `PUT /tags/assets`, `TagBulkAssetsDto { assetIds, tagIds }`.
    assert_eq!(
        server.tag_asset_calls(),
        vec![TagAssetCall {
            tag_ids: vec![tag_id.clone()],
            asset_ids: vec![asset_id.clone()],
        }],
        "R3.7: tagging carries the tag ids and the asset ids"
    );
    assert_eq!(
        server.tags_of(&asset_id),
        vec![tag_id],
        "R3.7: the asset carries the tag"
    );
}
