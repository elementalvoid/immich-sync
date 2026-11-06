use mock_immich::MockImmich;
use reqwest::StatusCode;

#[tokio::test]
async fn about_reports_a_server_version() {
    let server = MockImmich::start().await;

    // The real `GET /server/about` is permission-checked (`server.about`), so
    // the double requires a credential the same way.
    let about: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/server/about", server.base_url()))
        .header("x-api-key", "api-key-abc123")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(about["version"], "v3.3.1");
    assert_eq!(
        server.about_auth().as_deref(),
        Some("api_key"),
        "the double saw the credential the request carried"
    );
}

#[tokio::test]
async fn about_refuses_a_request_with_no_credential() {
    let server = MockImmich::start().await;

    let response = reqwest::get(format!("{}/api/server/about", server.base_url()))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(server.about_auth(), None);
}

#[tokio::test]
async fn about_refuses_a_key_that_lacks_the_server_about_permission() {
    let server = MockImmich::start().await;
    server.set_api_key("api-key-abc123");
    server.set_api_key_permissions(&["asset.upload"]);

    let response = reqwest::Client::new()
        .get(format!("{}/api/server/about", server.base_url()))
        .header("x-api-key", "api-key-abc123")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["message"], "Missing required permission: server.about");
    assert_eq!(server.about_requests(), 1);
}
