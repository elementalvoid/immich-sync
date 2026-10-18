use mock_immich::MockImmich;
use reqwest::StatusCode;

#[tokio::test]
async fn api_key_me_reports_the_key_and_its_permissions() {
    let server = MockImmich::start().await;
    server.set_api_key("api-key-abc123");
    server.set_api_key_permissions(&["asset.upload", "server.about"]);

    let body: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/api-keys/me", server.base_url()))
        .header("x-api-key", "api-key-abc123")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["name"], "Mock API Key");
    assert_eq!(body["permissions"][0], "asset.upload");
    assert_eq!(body["permissions"][1], "server.about");
    assert_eq!(server.api_key_me_requests(), 1);
}

#[tokio::test]
async fn api_key_me_refuses_a_session_token() {
    let server = MockImmich::start().await;

    let response = reqwest::Client::new()
        .get(format!("{}/api/api-keys/me", server.base_url()))
        .bearer_auth("a-session-token")
        .send()
        .await
        .unwrap();

    // The real server throws `ForbiddenException('Not authenticated with an
    // API Key')` for a bearer session (api-key.service.ts, v3.3.1).
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["message"], "Not authenticated with an API Key");
}

#[tokio::test]
async fn api_key_me_requires_a_credential() {
    let server = MockImmich::start().await;

    let response = reqwest::Client::new()
        .get(format!("{}/api/api-keys/me", server.base_url()))
        .send()
        .await
        .unwrap();

    // The real authentication guard answers a request with no credential
    // before `getMine` can throw its 403 (v3.3.1).
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["message"], "Authentication required");
}
