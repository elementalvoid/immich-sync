//! The tag handlers of the double (SPEC R3.7).
//!
//! The double is what the client's tests and the engine's tests observe, so
//! the handlers are pinned on their own: the upsert answers one tag per name
//! and reuses an existing id, and the bulk tag call records the ids the body
//! carried.

use mock_immich::{MockImmich, TagAssetCall, TagUpsertCall};

async fn upsert(client: &reqwest::Client, server: &MockImmich) -> serde_json::Value {
    client
        .put(format!("{}/api/tags", server.base_url()))
        .json(&serde_json::json!({ "tags": ["immich-sync"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_upsert_answers_one_tag_per_name_and_reuses_its_id() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();

    let first = upsert(&client, &server).await;
    // Immich v3.3.1 answers `TagResponseDto[]`; the client reads `id` and
    // `name`, so both are on the answer (`value` is the full path).
    assert_eq!(first.as_array().map(Vec::len), Some(1));
    let id = first[0]["id"]
        .as_str()
        .expect("the answer carries the tag id")
        .to_string();
    assert_eq!(first[0]["name"], "immich-sync");
    assert_eq!(
        server.upsert_calls(),
        vec![TagUpsertCall {
            tags: vec!["immich-sync".to_string()],
        }],
        "the recorded call is the body the client sent"
    );

    let second = upsert(&client, &server).await;
    assert_eq!(
        second[0]["id"], id,
        "an existing tag keeps its id, which is what makes the upsert safe to repeat"
    );
    assert_eq!(
        server.tag_names(),
        vec!["immich-sync".to_string()],
        "the second upsert created no second tag"
    );
}

#[tokio::test]
async fn tagging_assets_records_the_tag_ids_and_asset_ids() {
    let server = MockImmich::start().await;
    let client = reqwest::Client::new();
    // The tag and the asset can be made up: the handler's job is to record and
    // apply them, and an unknown asset id is skipped the way the real server
    // answers a partial result.
    let tag_id = "00000000-0000-4000-8000-000000000001";
    let asset_id = "00000000-0000-4000-8000-000000000002";

    let response = client
        .put(format!("{}/api/tags/assets", server.base_url()))
        .json(&serde_json::json!({
            "tagIds": [tag_id],
            "assetIds": [asset_id],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        server.tag_asset_calls(),
        vec![TagAssetCall {
            tag_ids: vec![tag_id.to_string()],
            asset_ids: vec![asset_id.to_string()],
        }],
        "the body's field names are tagIds and assetIds"
    );
}
