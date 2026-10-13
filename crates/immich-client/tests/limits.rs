//! The live bandwidth limit on a body stream (SPEC R7.1).
//!
//! Real timing, because the criterion is a rate: the body is streamed to the
//! offline double and the elapsed time is compared with `bytes / limit`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use immich_client::{AssetUpload, Auth, BandwidthLimit, ImmichClient, SessionToken};
use mock_immich::MockImmich;

/// The acceptance criterion's rate (R7.1): one megabyte per second.
const ONE_MEGABYTE_PER_SECOND: u64 = 1_000_000;
/// The fixture is 1.5 mebibytes: far more than the limiter's 0.1 s burst, so
/// the pace is what the clock measures, and under axum's 2 MB body limit in
/// the double.
const FILE_BYTES: usize = 1_572_864;
/// A checksum for the request header. The double stores the checksum it
/// computes from the body, so this one only has to be well-formed.
const CHECKSUM: &str = "0123456789abcdef0123456789abcdef01234567";

/// A file of `bytes` deterministic bytes, with its path.
fn fixture(bytes: usize) -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("create a temp dir");
    let path = directory.path().join("large.bin");
    let mut content = Vec::with_capacity(bytes);
    for index in 0..bytes {
        content.push((index % 251) as u8);
    }
    std::fs::write(&path, content).expect("write the fixture");
    (directory, path)
}

async fn client(server: &MockImmich) -> (ImmichClient, SessionToken) {
    let client = ImmichClient::new(server.base_url()).expect("the mock URL is valid");
    let token = client
        .login("sync@example.com", "correct horse")
        .await
        .expect("the mock accepts these credentials")
        .access_token;
    (client, token)
}

fn upload_request<'a>(path: &'a std::path::Path, limit: &BandwidthLimit) -> AssetUpload<'a> {
    AssetUpload {
        path,
        checksum: CHECKSUM,
        created_at: std::time::UNIX_EPOCH,
        modified_at: std::time::UNIX_EPOCH,
        bandwidth: Some(limit.clone()),
    }
}

#[tokio::test]
async fn a_bandwidth_limit_paces_the_upload_body() {
    let server = MockImmich::start().await;
    let (client, token) = client(&server).await;
    let (_directory, path) = fixture(FILE_BYTES);
    let limit = BandwidthLimit::new(ONE_MEGABYTE_PER_SECOND);

    let started = Instant::now();
    let response = client
        .upload_asset(Auth::Token(&token), &upload_request(&path, &limit))
        .await
        .expect("the mock accepts the upload");
    let elapsed = started.elapsed();

    assert!(!response.duplicate);
    // bytes / limit is 1.57 s; the burst (0.1 s of budget) comes off the
    // front, so the honest expectation is 1.47 s. The floor (0.65x) is below
    // any honest scheduler delay and above any rate over the limit. The
    // ceiling is 3x, which is far more load than the sleeps can add.
    assert!(
        elapsed >= Duration::from_secs(1),
        "1 MB/s for {FILE_BYTES} bytes finished in {elapsed:?}; the limiter did not pace it"
    );
    assert!(
        elapsed <= Duration::from_secs(4),
        "1 MB/s for {FILE_BYTES} bytes took {elapsed:?}; the limiter is far under the limit"
    );
}

#[tokio::test]
async fn raising_the_limit_mid_upload_speeds_the_same_request() {
    let server = MockImmich::start().await;
    let (client, token) = client(&server).await;
    let (_directory, path) = fixture(FILE_BYTES);
    // At 100 kB/s the whole file would take about 15 s, so finishing fast
    // proves the change reached the request already on the wire.
    let limit = BandwidthLimit::new(100_000);

    let started = Instant::now();
    let upload = tokio::spawn({
        let path = path.clone();
        let limit = limit.clone();
        async move {
            let request = upload_request(&path, &limit);
            client.upload_asset(Auth::Token(&token), &request).await
        }
    });
    // The body is on the wire once the server has the request.
    while server.upload_requests() == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    limit.set(0);

    let response = upload
        .await
        .expect("the upload task joins")
        .expect("the mock accepts the upload");
    let elapsed = started.elapsed();

    assert!(!response.duplicate);
    assert_eq!(
        server.upload_requests(),
        1,
        "the live change must not re-issue the request"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "raising the limit to unlimited took {elapsed:?}; the upload kept the old rate"
    );
}
