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

#[tokio::test]
async fn a_server_with_password_login_disabled_rejects_the_password() {
    let server = MockImmich::start().await;
    server.disable_password_login();

    let response = reqwest::Client::new()
        .post(format!("{}/auth/login", server.base_url()))
        .json(&serde_json::json!({
            "email": "sync@example.com",
            "password": "correct horse",
        }))
        .send()
        .await
        .unwrap();

    // Immich answers with `UnauthorizedException('Password login has been
    // disabled')`, so the status is 401 and only the message tells the two
    // 401 responses apart.
    assert_eq!(response.status(), 401);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["message"], "Password login has been disabled");
}

#[tokio::test]
async fn strict_auth_rejects_a_token_the_server_did_not_issue() {
    let server = MockImmich::start().await;
    server.require_known_credentials();

    let response = reqwest::Client::new()
        .post(format!("{}/auth/validateToken", server.base_url()))
        .bearer_auth("stale-session-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    let login: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/auth/login", server.base_url()))
        .json(&serde_json::json!({
            "email": "sync@example.com",
            "password": "correct horse",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = login["accessToken"].as_str().unwrap();

    let response = reqwest::Client::new()
        .post(format!("{}/auth/validateToken", server.base_url()))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    // An admin can revoke every session; the next probe must fail again.
    server.revoke_tokens();
    let response = reqwest::Client::new()
        .post(format!("{}/auth/validateToken", server.base_url()))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn a_configured_api_key_is_the_only_accepted_key() {
    let server = MockImmich::start().await;
    server.set_api_key("api-key-1234");

    let response = reqwest::Client::new()
        .post(format!("{}/auth/validateToken", server.base_url()))
        .header("x-api-key", "api-key-1234")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let response = reqwest::Client::new()
        .post(format!("{}/auth/validateToken", server.base_url()))
        .header("x-api-key", "wrong-key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}
