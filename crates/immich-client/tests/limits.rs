//! The live bandwidth limit on a body stream (SPEC R7.1).
//!
//! Real timing, because the criterion is a rate: the body is streamed to the
//! offline double and the elapsed time is compared with `bytes / limit`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use immich_client::{AssetUpload, Auth, BandwidthLimit, ClientError, ImmichClient, SessionToken};
use mock_immich::{Fault, MockImmich, Operation};
use tokio::io::AsyncReadExt;

/// The acceptance criterion's rate (R7.1): one megabyte per second.
const ONE_MEGABYTE_PER_SECOND: u64 = 1_000_000;
/// The fixture is 1.5 mebibytes: far more than the limiter's 0.1 s burst, so
/// the pace is what the clock measures, and under axum's 2 MB body limit in
/// the double.
const FILE_BYTES: usize = 1_572_864;
/// A checksum for the request header. The double stores the checksum it
/// computes from the body, so this one only has to be well-formed.
const CHECKSUM: &str = "0123456789abcdef0123456789abcdef01234567";

/// The body the backpressure test sends: more than the two socket buffers can
/// ever hold (Linux caps them at a few megabytes), so the client cannot get
/// it all out while nobody reads.
const BACKPRESSURE_BYTES: usize = 16 * 1024 * 1024;

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
        bandwidth: limit.clone(),
    }
}

#[tokio::test]
async fn zero_is_unlimited() {
    let server = MockImmich::start().await;
    let (client, token) = client(&server).await;
    let (_directory, path) = fixture(FILE_BYTES);
    let limit = BandwidthLimit::new(0);

    let started = Instant::now();
    client
        .upload_asset(Auth::Token(&token), &upload_request(&path, &limit))
        .await
        .expect("the mock accepts the upload");
    let elapsed = started.elapsed();

    // The tight half of R7.1. 1 MB/s needs 1.47 s of the upload on its own,
    // and an unlimited one takes milliseconds; the ceiling leaves a wide
    // margin for a loaded machine without measuring anything but the body.
    assert!(
        elapsed < Duration::from_secs(1),
        "R7.1: 0 means unlimited, but {FILE_BYTES} bytes took {elapsed:?}"
    );
}

#[tokio::test]
async fn a_rate_too_low_to_pay_out_one_read_still_completes_an_upload() {
    let server = MockImmich::start().await;
    let (client, token) = client(&server).await;
    // One read's worth of body: at the floor it costs 4.1 s of streaming, and
    // at 100 B/s it would cost 41 s, which is the case under test.
    let (_directory, path) = fixture(4_096);
    // 100 B/s is a value the settings field accepts - six decimals of MB/s is
    // one byte, so it is 0.0001 in the field. The stall guard waits 30 s for
    // the first byte and the limiter needs `read / rate` for it, so this used
    // to call a healthy, eager server `Stalled` after 30 s.
    let limit = BandwidthLimit::new(100);

    let started = Instant::now();
    let response = client
        .upload_asset(Auth::Token(&token), &upload_request(&path, &limit))
        .await
        .expect("a low rate must not be reported as a dead server");
    let elapsed = started.elapsed();

    assert!(!response.duplicate);
    assert_eq!(
        limit.bytes_per_second(),
        1_000,
        "a rate under the floor is used at the floor"
    );
    // The upload ran at the floor, not at the 100 B/s that was asked for.
    assert!(
        elapsed <= Duration::from_secs(20),
        "4 KiB at the floor took {elapsed:?}; the rate is lower than the floor"
    );
}

#[tokio::test]
async fn a_server_that_stops_answering_fails_the_upload_within_the_stall_budget() {
    let server = MockImmich::start().await;
    // The double takes the whole body and then holds the answer: a server that
    // accepted the upload and stopped talking is exactly this. Nothing is in
    // flight on the socket by then, so the only bound left is the client's.
    server.fail(Operation::Upload, Fault::Slow(Duration::from_secs(300)));
    let (client, token) = client(&server).await;
    let (_directory, path) = fixture(FILE_BYTES);
    // 1.5 MiB at 200 kB/s is 7.9 s of streaming, so the deadline this replaces
    // - three times the bytes' own time plus the client window - was 53 s, and
    // nothing on the wire would have ended the upload inside 30 s.
    let limit = BandwidthLimit::new(200_000);

    let started = Instant::now();
    let error = client
        .upload_asset(Auth::Token(&token), &upload_request(&path, &limit))
        .await
        .expect_err("a server that never answers cannot finish the upload");
    let elapsed = started.elapsed();

    assert!(matches!(error, ClientError::Stalled), "{error:?}");
    assert!(
        elapsed <= Duration::from_secs(60),
        "a stalled upload took {elapsed:?}; the bound has to be the stall budget"
    );
}

#[tokio::test]
async fn an_unlimited_upload_still_gives_up_at_the_client_wide_window() {
    let server = MockImmich::start().await;
    server.fail(Operation::Upload, Fault::Slow(Duration::from_secs(300)));
    let (client, token) = client(&server).await;
    let (_directory, path) = fixture(FILE_BYTES);
    let limit = BandwidthLimit::new(0);

    let started = Instant::now();
    let error = client
        .upload_asset(Auth::Token(&token), &upload_request(&path, &limit))
        .await
        .expect_err("a server that never answers cannot finish the upload");
    let elapsed = started.elapsed();

    assert!(matches!(error, ClientError::Stalled), "{error:?}");
    // An unlimited upload carries no total deadline any more, but it must not
    // wait any longer than the 30 s window every request has always had.
    assert!(
        elapsed >= Duration::from_secs(29) && elapsed <= Duration::from_secs(45),
        "an unlimited upload to a stalled server failed after {elapsed:?}"
    );
}

/// A server that reads the head of the request and then stops reading, keeping
/// the connection open, and can say how many bytes ever arrived.
///
/// The double cannot say this. `MockImmich::set_upload_delay` holds a handler
/// that will read the whole body afterwards, so whether the client's send
/// blocks comes down to the socket buffers rather than to the client, and
/// nothing in there counts what arrived. Nothing can accept the connection
/// before the request comes either, so this hands its answers back over
/// channels.
struct StoppedReading {
    url: String,
    read_the_rest: tokio::sync::oneshot::Sender<()>,
    arrived: tokio::sync::oneshot::Receiver<usize>,
}

impl StoppedReading {
    /// Reads what the kernel still holds and returns every byte that arrived.
    async fn arrived(self) -> usize {
        let _ = self.read_the_rest.send(());
        self.arrived.await.expect("the server counted its bytes")
    }
}

async fn server_that_stops_reading() -> StoppedReading {
    let (read_the_rest, stop_reading) = tokio::sync::oneshot::channel();
    let (count, counted) = tokio::sync::oneshot::channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the server");
    let address = listener.local_addr().expect("a loopback address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("the client connects");
        let mut buf = [0_u8; 4096];
        // One read, so the client's first bytes have somewhere to go, then
        // nothing: the window fills and the send blocks with the body still to
        // come. The socket stays open, so the client waits on a full window
        // instead of meeting a closed connection.
        let mut arrived = socket.read(&mut buf).await.unwrap_or(0);
        if stop_reading.await.is_ok() {
            while let Ok(read) = socket.read(&mut buf).await {
                if read == 0 {
                    break;
                }
                arrived += read;
            }
        }
        let _ = count.send(arrived);
    });
    StoppedReading {
        url: format!("http://{address}"),
        read_the_rest,
        arrived: counted,
    }
}

#[tokio::test]
async fn a_server_that_stops_reading_mid_body_fails_the_upload_within_the_stall_budget() {
    let server = server_that_stops_reading().await;
    let client = ImmichClient::new(&server.url).expect("a loopback URL is valid");
    let (_directory, path) = fixture(BACKPRESSURE_BYTES);
    // Unlimited, so the only thing that can stop the body is the socket.
    let limit = BandwidthLimit::new(0);

    let started = Instant::now();
    let error = client
        .upload_asset(Auth::ApiKey("api-key-1234"), &upload_request(&path, &limit))
        .await
        .expect_err("a server that stops reading cannot finish the upload");
    let elapsed = started.elapsed();

    assert!(matches!(error, ClientError::Stalled), "{error:?}");
    // The guard can only notice at the end of a window, so it needs at least
    // one and at most two of them: it saw bytes still moving in the first.
    assert!(
        elapsed >= Duration::from_secs(29) && elapsed <= Duration::from_secs(65),
        "an upload blocked mid-body gave up after {elapsed:?}"
    );
    // The client could not get the whole body out, so it was blocked with
    // bytes still to send when the guard gave up - a server that stopped
    // reading, not one that stopped answering.
    let arrived = server.arrived().await;
    assert!(
        arrived < BACKPRESSURE_BYTES,
        "{arrived} of {BACKPRESSURE_BYTES} bytes arrived: the server read the body"
    );
}

#[tokio::test]
async fn lowering_the_limit_mid_upload_does_not_abort_a_healthy_upload() {
    let server = MockImmich::start().await;
    let (client, token) = client(&server).await;
    let (_directory, path) = fixture(FILE_BYTES);
    // The engine starts unlimited, so the request is built with no limit in
    // force and used to carry the client's 30 s total deadline. At 20 kB/s
    // the rest of the file needs well over a minute of streaming, which that
    // deadline cut short however well the server was behaving.
    let limit = BandwidthLimit::new(0);

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
    limit.set(20_000);

    let response = upload
        .await
        .expect("the upload task joins")
        .expect("a healthy upload survives the limit that was set on it");
    let elapsed = started.elapsed();

    assert!(!response.duplicate);
    assert_eq!(
        server.upload_requests(),
        1,
        "the change must not re-issue the request"
    );
    // No floor on the elapsed time: how much of the body left the socket before
    // the limit landed is the machine's business, and the proof is that the
    // upload finishes at all rather than dying on a deadline nobody re-read.
    assert!(
        elapsed <= Duration::from_secs(150),
        "a slow but progressing upload took {elapsed:?}"
    );
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
