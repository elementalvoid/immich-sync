//! The login path must not log a secret (SPEC N3, R1.2, R10.3).
//!
//! This test lives in its own binary because it installs the process-wide
//! tracing subscriber: `tracing` caches whether each call site is enabled for
//! the process, so the subscriber must exist before the first log event.

use std::sync::{Arc, Mutex};

use mock_immich::MockImmich;
use sync_core::connection::{AuthMethod, Connection, Secret, SignIn};
use sync_core::credential::MemoryCredentialStore;

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
async fn the_login_path_never_logs_a_secret() {
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

    let log = String::from_utf8(
        buffer
            .0
            .lock()
            .expect("the log buffer is not poisoned")
            .clone(),
    )
    .expect("the log is UTF-8");

    assert!(!log.is_empty(), "the login path logs its outcome");
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
