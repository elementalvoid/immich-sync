use mock_immich::MockImmich;

#[tokio::test]
async fn login_returns_a_session_token() {
    let server = MockImmich::start().await;

    let response = reqwest::Client::new()
        .post(format!("{}/auth/login", server.base_url()))
        .json(&serde_json::json!({
            "email": "sync@example.com",
            "password": "correct horse",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 201);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(!body["accessToken"].as_str().unwrap().is_empty());
    assert_eq!(body["userEmail"], "sync@example.com");
}

#[tokio::test]
async fn a_wrong_password_is_rejected() {
    let server = MockImmich::start().await;

    let response = reqwest::Client::new()
        .post(format!("{}/auth/login", server.base_url()))
        .json(&serde_json::json!({
            "email": "sync@example.com",
            "password": "wrong",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn validate_token_reports_the_token_as_valid() {
    let server = MockImmich::start().await;

    let response = reqwest::Client::new()
        .post(format!("{}/auth/validateToken", server.base_url()))
        .bearer_auth("some-session-token")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["authStatus"], true);
}
