//! The network path must not log a secret (SPEC N3, R1.2, R10.3).
//!
//! This test lives in its own binary because it installs the process-wide
//! tracing subscriber: `tracing` caches whether each call site is enabled for
//! the process, so the subscriber must exist before the first log event.

use std::sync::{Arc, Mutex};

use mock_immich::{Fault, MockImmich, Operation};
use sync_core::connection::{AuthMethod, Connection, Secret, SignIn};
use sync_core::credential::MemoryCredentialStore;
use sync_core::store::StateStore;
use sync_core::upload::{UploadOutcome, Uploader};

/// A `MakeWriter` that keeps every log line in memory.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("the log buffer is not poisoned")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn the_network_path_never_logs_a_secret() {
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(buffer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("this test binary installs the only subscriber");

    let server = MockImmich::start().await;
    server.set_api_key("api-key-abc123");
    let store = Arc::new(MemoryCredentialStore::new());

    // A successful password login.
    let mut connection = Connection::new(server.base_url(), store.clone()).expect("a valid URL");
    connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        })
        .await;
    let token = connection.token().expect("a session token").clone();

    // A failed password login.
    let mut failing = Connection::new(server.base_url(), store.clone()).expect("a valid URL");
    failing
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("hunter2-wrong"),
            method: AuthMethod::Password,
        })
        .await;

    // An API-key validation.
    let mut keyed = Connection::new(server.base_url(), store.clone()).expect("a valid URL");
    keyed
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("api-key-abc123"),
            method: AuthMethod::ApiKey,
        })
        .await;

    // A hostile server echoes a marker in the error body of both upload
    // endpoints: the checksum check (415) and the upload itself (403). The
    // client's own events and the pipeline's failure event must each drop it.
    let marker = "hostile-echo-9911";
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = Arc::new(
        StateStore::open(temp.path().join("data").join("state.db")).expect("open the store"),
    );
    let session = keyed.shared();
    let uploader = Uploader::new(session, store.clone());
    let asset = temp.path().join("asset.txt");
    std::fs::write(&asset, b"hello world").expect("write the fixture");

    server.fail(
        Operation::BulkUploadCheck,
        Fault::ServerMessage {
            code: 415,
            message: format!("unsupported: {marker}"),
        },
    );
    let check_outcome = uploader.upload_one(&asset).await;
    assert!(
        matches!(check_outcome, UploadOutcome::Failed(_)),
        "the checksum refusal is a failure: {check_outcome:?}"
    );

    server.fail(
        Operation::Upload,
        Fault::ServerMessage {
            code: 403,
            message: format!("forbidden: {marker}"),
        },
    );
    let upload_outcome = uploader.upload_one(&asset).await;
    assert!(
        matches!(upload_outcome, UploadOutcome::Failed(_)),
        "the upload refusal is a failure: {upload_outcome:?}"
    );

    let log = String::from_utf8(
        buffer
            .0
            .lock()
            .expect("the log buffer is not poisoned")
            .clone(),
    )
    .expect("the log is UTF-8");

    assert!(!log.is_empty(), "the network path logs its outcome");
    assert!(
        !log.contains(marker),
        "the log must not contain the server's own text ({marker:?}):\n{log}"
    );
    for secret in [
        "correct horse",
        "hunter2-wrong",
        "api-key-abc123",
        token.expose(),
    ] {
        assert!(
            !log.contains(secret),
            "the log must not contain a secret ({secret:?}):\n{log}"
        );
    }
}
