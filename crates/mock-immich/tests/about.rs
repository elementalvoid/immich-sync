use mock_immich::MockImmich;

#[tokio::test]
async fn about_reports_a_server_version() {
    let server = MockImmich::start().await;

    let about: serde_json::Value = reqwest::get(format!("{}/server/about", server.base_url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(about["version"], "v3.3.1");
}
