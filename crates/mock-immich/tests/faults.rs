use std::time::{Duration, Instant};

use mock_immich::{Fault, MockImmich, Operation};
use reqwest::multipart::{Form, Part};

fn upload_form() -> Form {
    Form::new()
        .part(
            "assetData",
            Part::bytes(b"hello world".to_vec()).file_name("a.txt"),
        )
        .text("filename", "a.txt")
        .text("fileCreatedAt", "2026-01-01T00:00:00.000Z")
        .text("fileModifiedAt", "2026-01-01T00:00:00.000Z")
}

async fn upload(client: &reqwest::Client, server: &MockImmich) -> reqwest::Response {
    client
        .post(format!("{}/assets", server.base_url()))
        .multipart(upload_form())
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_429_fault_carries_retry_after() {
    let server = MockImmich::start().await;
    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 429,
            retry_after: Some(7),
        },
    );

    let response = upload(&reqwest::Client::new(), &server).await;

    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["retry-after"], "7");
}

#[tokio::test]
async fn a_503_fault_is_returned() {
    let server = MockImmich::start().await;
    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 503,
            retry_after: None,
        },
    );

    let response = upload(&reqwest::Client::new(), &server).await;

    assert_eq!(response.status(), 503);
}

#[tokio::test]
async fn a_quota_fault_returns_400() {
    let server = MockImmich::start().await;
    server.fail(Operation::Upload, Fault::Quota);

    let response = upload(&reqwest::Client::new(), &server).await;

    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["message"].as_str().unwrap().contains("Quota"));
}

#[tokio::test]
async fn a_permission_fault_returns_403() {
    let server = MockImmich::start().await;
    server.fail(Operation::Upload, Fault::PermissionDenied);

    let response = upload(&reqwest::Client::new(), &server).await;

    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn an_unsupported_format_fault_returns_415() {
    let server = MockImmich::start().await;
    server.fail(Operation::Upload, Fault::UnsupportedFormat);

    let response = upload(&reqwest::Client::new(), &server).await;

    assert_eq!(response.status(), 415);
}

#[tokio::test]
async fn a_slow_fault_delays_the_response() {
    let server = MockImmich::start().await;
    let delay = Duration::from_millis(200);
    server.fail(Operation::About, Fault::Slow(delay));

    let started = Instant::now();
    reqwest::get(format!("{}/server/about", server.base_url()))
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert!(elapsed >= Duration::from_millis(150), "elapsed {elapsed:?}");
}

#[tokio::test]
async fn a_disconnect_fault_breaks_the_response() {
    let server = MockImmich::start().await;
    server.fail(Operation::Upload, Fault::Disconnect);

    let result = reqwest::Client::new()
        .post(format!("{}/assets", server.base_url()))
        .multipart(upload_form())
        .send()
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn a_fault_is_consumed_by_the_next_request() {
    let server = MockImmich::start().await;
    server.fail(
        Operation::About,
        Fault::Status {
            code: 503,
            retry_after: None,
        },
    );

    let first = reqwest::get(format!("{}/server/about", server.base_url()))
        .await
        .unwrap();
    let second = reqwest::get(format!("{}/server/about", server.base_url()))
        .await
        .unwrap();

    assert_eq!(first.status(), 503);
    assert_eq!(second.status(), 200);
}

#[tokio::test]
async fn reset_faults_removes_pending_faults() {
    let server = MockImmich::start().await;
    server.fail(
        Operation::About,
        Fault::Status {
            code: 503,
            retry_after: None,
        },
    );
    server.reset_faults();

    let response = reqwest::get(format!("{}/server/about", server.base_url()))
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
}
