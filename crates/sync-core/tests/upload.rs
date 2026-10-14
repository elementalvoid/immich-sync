//! The one-file pipeline against the offline Immich double (R3.1, R3.2, R4.5).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use immich_client::BandwidthLimit;
use mock_immich::{Fault, MockImmich, Operation};
use sync_core::connection::{AuthMethod, Connection, Secret, SharedSession, SignIn};
use sync_core::credential::MemoryCredentialStore;
use sync_core::failure::{FailureClass, FailureKind};
use sync_core::hash::Sha1;
use sync_core::store::{ItemState, StateStore};
use sync_core::upload::{PreparedItem, UploadGate, UploadOutcome, Uploader};

/// `sha1("hello world")`, hex.
const HELLO_WORLD_SHA1_HEX: &str = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";

/// A fixture file whose bytes have a known digest.
fn hello_world_file(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, b"hello world").expect("write the fixture");
    path
}

async fn uploader(
    server: &MockImmich,
) -> (Uploader, SharedSession, Arc<StateStore>, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = Arc::new(
        StateStore::open(temp.path().join("data").join("state.db")).expect("open the store"),
    );
    let mut connection = Connection::new(server.base_url(), Arc::new(MemoryCredentialStore::new()))
        .expect("a valid URL");
    connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new("correct horse"),
            method: AuthMethod::Password,
        })
        .await;
    let session = connection.shared();
    (
        Uploader::new(session.clone(), store.clone()),
        session,
        store,
        temp,
    )
}

/// The mtime the filesystem reports, converted independently of the pipeline.
fn expected_mtime_ns(path: &Path) -> i64 {
    let modified = std::fs::metadata(path)
        .expect("read the fixture metadata")
        .modified()
        .expect("the platform reports a modification time");
    i64::try_from(
        modified
            .duration_since(UNIX_EPOCH)
            .expect("the fixture is newer than 1970")
            .as_nanos(),
    )
    .expect("the mtime fits an i64")
}

fn success_asset_id(outcome: &UploadOutcome) -> &str {
    match outcome {
        UploadOutcome::Created { asset_id } | UploadOutcome::Duplicate { asset_id } => asset_id,
        other => panic!("expected a success, got {other:?}"),
    }
}

/// The bytes [`sidecar_of`] writes, whose digest is [`SIDECAR_SHA1_HEX`].
const SIDECAR: &[u8] = b"<x:xmpmeta>sidecar</x:xmpmeta>";

/// The XMP sidecar of `media`, written next to it.
fn sidecar_of(media: &Path) -> PathBuf {
    let sidecar = media.with_extension("txt.xmp");
    std::fs::write(&sidecar, SIDECAR).expect("write the sidecar");
    sidecar
}

/// The digest of [`sidecar_of`]'s fixture, as an independent literal.
const SIDECAR_SHA1_HEX: &str = "f8d2e9cfb0466aeebda758acb4dbbdfa7ce77140";

#[tokio::test]
async fn uploading_a_new_file_returns_created_and_records_the_row() {
    let server = MockImmich::start().await;
    let (uploader, _session, store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.txt");

    let outcome = uploader.upload_one(&path).await;

    let UploadOutcome::Created { asset_id } = &outcome else {
        panic!("expected created, got {outcome:?}");
    };
    assert!(!asset_id.is_empty(), "R3.6: success carries the asset id");
    let row = store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the upload was recorded");
    assert_eq!(row.path, path.display().to_string());
    assert_eq!(row.size, 11, "the row holds the file size");
    assert_eq!(
        row.mtime_ns,
        expected_mtime_ns(&path),
        "the row holds the file mtime"
    );
    assert_eq!(
        row.sha1,
        Sha1::from_hex(HELLO_WORLD_SHA1_HEX).expect("a hex SHA-1"),
        "the row holds the file's SHA-1 literal"
    );
    assert_eq!(row.asset_id.as_deref(), Some(asset_id.as_str()));
    assert_eq!(row.state, ItemState::Uploaded);
    assert_eq!(row.error_class, None);
    assert_eq!(server.upload_requests(), 1);
}

#[tokio::test]
async fn the_same_file_again_is_a_duplicate_and_transfers_no_body() {
    let server = MockImmich::start().await;
    let (uploader, _session, store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.txt");
    let first = uploader.upload_one(&path).await;

    let second = uploader.upload_one(&path).await;

    let UploadOutcome::Duplicate { asset_id } = &second else {
        panic!("expected a duplicate, got {second:?}");
    };
    assert_eq!(
        asset_id,
        success_asset_id(&first),
        "the same asset is reported"
    );
    assert_eq!(
        server.upload_requests(),
        1,
        "R4.5: an unchanged SHA-1 must not transfer a body again"
    );
    assert_eq!(
        server.bulk_check_requests(),
        1,
        "the local row short-circuits even the check"
    );
    let row = store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the row exists");
    assert_eq!(row.state, ItemState::Uploaded, "the successful row is kept");
}

#[tokio::test]
async fn an_unchanged_file_that_was_touched_does_not_upload_again() {
    let server = MockImmich::start().await;
    let (uploader, _session, _store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.txt");
    uploader.upload_one(&path).await;

    // Same bytes, new mtime: the decision must come from the SHA-1, not the
    // metadata (R4.5).
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&path, b"hello world").expect("rewrite the same bytes");
    let outcome = uploader.upload_one(&path).await;

    assert!(
        matches!(outcome, UploadOutcome::Duplicate { .. }),
        "{outcome:?}"
    );
    assert_eq!(server.upload_requests(), 1);
}

#[tokio::test]
async fn a_touched_sidecar_does_not_offer_the_asset_again() {
    // R3.3, R4.5: the asset already carries these bytes. A `touch` or a backup
    // restore moves the sidecar's mtime without changing its content, and
    // offering it again is answered `duplicate` from the checksum header before
    // the server reads the body — which parks the item claiming the sidecar
    // could not be attached, although it is attached.
    let server = MockImmich::start().await;
    let (uploader, _session, store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.txt");
    let sidecar = sidecar_of(&path);

    let first = uploader.upload_one(&path).await;
    let asset_id = success_asset_id(&first).to_string();
    assert!(
        server.has_sidecar(&asset_id),
        "R3.3: the sidecar rode along with the first attempt"
    );

    // Same bytes, new mtime.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&sidecar, SIDECAR).expect("rewrite the same bytes");
    let second = uploader.upload_one(&path).await;

    assert!(
        matches!(second, UploadOutcome::Duplicate { .. }),
        "the sidecar is already attached, so there is nothing to offer: {second:?}"
    );
    assert_eq!(success_asset_id(&second), asset_id);
    assert_eq!(
        server.upload_requests(),
        1,
        "a touched sidecar must not go on the wire again"
    );
    let row = store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the row exists");
    assert_eq!(
        row.state,
        ItemState::Uploaded,
        "the item stays uploaded rather than parked: {row:?}"
    );
}

#[tokio::test]
async fn content_the_server_already_has_is_a_duplicate_without_a_second_upload() {
    let server = MockImmich::start().await;
    let (uploader, _session, store, temp) = uploader(&server).await;
    let first_path = hello_world_file(temp.path(), "first.txt");
    let second_path = hello_world_file(temp.path(), "second.txt");
    let first = uploader.upload_one(&first_path).await;

    let second = uploader.upload_one(&second_path).await;

    let UploadOutcome::Duplicate { asset_id } = &second else {
        panic!("expected the server's duplicate answer, got {second:?}");
    };
    assert_eq!(asset_id, success_asset_id(&first));
    assert_eq!(
        server.upload_requests(),
        1,
        "a rejected checksum must not upload"
    );
    let row = store
        .item(&second_path.display().to_string())
        .expect("read the row")
        .expect("the duplicate was recorded");
    assert_eq!(row.state, ItemState::Duplicate);
    assert_eq!(row.asset_id.as_deref(), Some(asset_id.as_str()));
}

#[tokio::test]
async fn a_415_is_exactly_one_permanent_failure_and_no_retry() {
    let server = MockImmich::start().await;
    server.fail(Operation::Upload, Fault::UnsupportedFormat);
    let (uploader, _session, store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.bin");

    let outcome = uploader.upload_one(&path).await;

    let UploadOutcome::Failed(failure) = &outcome else {
        panic!("expected a failure, got {outcome:?}");
    };
    assert_eq!(failure.class, Some(FailureClass::UnsupportedFormat));
    assert_eq!(failure.kind, FailureKind::Permanent);
    assert_eq!(
        server.upload_requests(),
        1,
        "a permanent failure must not retry (R6.3)"
    );
    let row = store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the failure was recorded");
    assert_eq!(row.state, ItemState::FailedPermanent);
    assert_eq!(row.error_class, Some(FailureClass::UnsupportedFormat));
    assert_eq!(row.asset_id, None);
    assert_eq!(row.sha1.hex(), HELLO_WORLD_SHA1_HEX);
}

#[tokio::test]
async fn a_rejected_unsupported_format_never_uploads() {
    let server = MockImmich::start().await;
    server.reject_format(
        Sha1::from_hex(HELLO_WORLD_SHA1_HEX)
            .expect("a hex SHA-1")
            .base64()
            .as_str(),
    );
    let (uploader, _session, store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.bin");

    let outcome = uploader.upload_one(&path).await;

    let UploadOutcome::Failed(failure) = &outcome else {
        panic!("expected a failure, got {outcome:?}");
    };
    assert_eq!(failure.class, Some(FailureClass::UnsupportedFormat));
    assert_eq!(server.upload_requests(), 0, "the check refused it first");
    let row = store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the failure was recorded");
    assert_eq!(row.state, ItemState::FailedPermanent);
    assert_eq!(row.error_class, Some(FailureClass::UnsupportedFormat));
}

#[tokio::test]
async fn a_transient_server_failure_waits_for_a_later_attempt() {
    let server = MockImmich::start().await;
    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 503,
            retry_after: None,
        },
    );
    let (uploader, _session, store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.txt");

    let outcome = uploader.upload_one(&path).await;

    let UploadOutcome::Failed(failure) = &outcome else {
        panic!("expected a failure, got {outcome:?}");
    };
    assert_eq!(failure.class, Some(FailureClass::ServerTransient));
    assert_eq!(failure.kind, FailureKind::Transient);
    let row = store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the failure was recorded");
    assert_eq!(row.state, ItemState::RetryWait);
    assert_eq!(row.error_class, Some(FailureClass::ServerTransient));
}

#[tokio::test]
async fn a_upload_the_server_hangs_up_on_is_a_transient_network_failure() {
    let server = MockImmich::start().await;
    // `Fault::Disconnect` fails the request at `.send()`: the server drops the
    // connection before it answers, so no status ever arrives (#19).
    server.fail(Operation::Upload, Fault::Disconnect);
    let (uploader, _session, store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.txt");

    let outcome = uploader.upload_one(&path).await;

    let UploadOutcome::Failed(failure) = &outcome else {
        panic!("expected a failure, got {outcome:?}");
    };
    assert_eq!(
        failure.class,
        Some(FailureClass::NetworkTransient),
        "a dropped connection during the upload is transport noise: {outcome:?}"
    );
    assert_eq!(failure.kind, FailureKind::Transient);
    let row = store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the failure was recorded");
    assert_eq!(
        row.state,
        ItemState::RetryWait,
        "a cut-off body must not become a permanent verdict"
    );
    assert_eq!(row.error_class, Some(FailureClass::NetworkTransient));
}

#[tokio::test]
async fn a_failed_upload_leaves_the_sidecar_owed_and_a_later_pass_sends_it() {
    // R4.5: the digest is what says the asset carries the sidecar, so it may
    // only be written once the request carrying it was accepted. A transient
    // failure sends nothing, and a row that claims otherwise drops the
    // sidecar for good.
    let server = MockImmich::start().await;
    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 503,
            retry_after: None,
        },
    );
    let (uploader, _session, store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.txt");
    sidecar_of(&path);

    let failed = uploader.upload_one(&path).await;
    assert!(
        matches!(failed, UploadOutcome::Failed(_)),
        "expected the faulted upload to fail, got {failed:?}"
    );
    let row = store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the failure was recorded");
    assert_eq!(
        row.sidecar, None,
        "the request failed, so the asset carries no sidecar yet"
    );

    server.reset_faults();
    let retried = uploader.upload_one(&path).await;
    let asset_id = success_asset_id(&retried).to_string();
    assert!(
        server.has_sidecar(&asset_id),
        "R3.3: the sidecar rides along with the later attempt"
    );
    assert_eq!(
        store
            .item(&path.display().to_string())
            .expect("read the row")
            .expect("the row exists")
            .sidecar
            .map(|sidecar| sidecar.sha1),
        Some(Sha1::from_hex(SIDECAR_SHA1_HEX).expect("a hex SHA-1")),
        "R4.5: the digest is recorded once the asset carries the sidecar"
    );
}

#[tokio::test]
async fn a_paused_upload_leaves_the_sidecar_owed() {
    // R8.7: a paused upload sends nothing at all, so it can claim no sidecar.
    let server = MockImmich::start().await;
    let (uploader, _session, store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.txt");
    let sidecar = sidecar_of(&path);
    let gate = UploadGate::new();
    gate.pause();

    let prepared = PreparedItem {
        path: path.clone(),
        size: 11,
        mtime_ns: expected_mtime_ns(&path),
        sha1: Sha1::from_hex(HELLO_WORLD_SHA1_HEX).expect("a hex SHA-1"),
        sidecar: Some(sync_core::upload::SidecarFile {
            digest: sync_core::store::SidecarDigest {
                sha1: Sha1::from_hex(SIDECAR_SHA1_HEX).expect("a hex SHA-1"),
                size: std::fs::metadata(&sidecar).expect("read the sidecar").len(),
                mtime_ns: expected_mtime_ns(&sidecar),
            },
            path: sidecar,
        }),
        live_photo_video: None,
    };
    let outcome = uploader
        .upload_prepared(&prepared, &gate, &BandwidthLimit::default())
        .await;

    assert_eq!(outcome, UploadOutcome::Deferred);
    assert_eq!(server.upload_requests(), 0, "nothing was sent");
    let row = store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the deferred item was recorded");
    assert_eq!(
        row.sidecar, None,
        "the request never went out, so the asset carries no sidecar yet"
    );
}

#[tokio::test]
async fn a_missing_file_is_a_local_read_error_and_writes_no_row() {
    let server = MockImmich::start().await;
    let (uploader, _session, store, temp) = uploader(&server).await;
    let path = temp.path().join("missing.txt");

    let outcome = uploader.upload_one(&path).await;

    let UploadOutcome::Failed(failure) = &outcome else {
        panic!("expected a failure, got {outcome:?}");
    };
    assert_eq!(failure.class, Some(FailureClass::LocalReadError));
    assert_eq!(
        store.item(&path.display().to_string()).expect("read"),
        None,
        "a file that was never hashed leaves no row"
    );
    assert_eq!(server.upload_requests(), 0);
}

#[tokio::test]
async fn a_failed_row_is_attempted_again_instead_of_short_circuiting() {
    let server = MockImmich::start().await;
    server.fail(Operation::Upload, Fault::UnsupportedFormat);
    let (uploader, _session, _store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.bin");
    let first = uploader.upload_one(&path).await;
    assert!(matches!(first, UploadOutcome::Failed(_)), "{first:?}");

    // The fault is consumed; a later run (a manual retry, in #7) must try
    // again rather than report the failed row as a duplicate.
    let second = uploader.upload_one(&path).await;

    assert!(
        matches!(second, UploadOutcome::Created { .. }),
        "the retry uploads: {second:?}"
    );
    assert_eq!(server.upload_requests(), 2);
}

#[tokio::test]
async fn a_401_on_the_checksum_check_re_logs_in_once_and_retries_the_check() {
    let server = MockImmich::start().await;
    server.fail(
        Operation::BulkUploadCheck,
        Fault::Status {
            code: 401,
            retry_after: None,
        },
    );
    let (uploader, session, store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.txt");

    let outcome = uploader.upload_one(&path).await;

    // R1.4: a 401 on the request path re-logs in once and the request is
    // retried, so one refusal is not yet a permanent verdict.
    assert!(
        matches!(outcome, UploadOutcome::Created { .. }),
        "the check was retried after the re-login: {outcome:?}"
    );
    assert_eq!(server.bulk_check_requests(), 2, "one retry, no more");
    assert_eq!(server.login_requests(), 2, "exactly one re-login");
    assert!(session.state().is_connected());
    let row = store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the outcome was recorded");
    assert!(row.state.is_terminal_success());
}

#[tokio::test]
async fn an_empty_checksum_check_body_is_a_transient_network_failure() {
    let server = MockImmich::start().await;
    server.fail(
        Operation::BulkUploadCheck,
        Fault::Status {
            code: 200,
            retry_after: None,
        },
    );
    let (uploader, _session, store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.txt");

    let outcome = uploader.upload_one(&path).await;

    // A 200 whose body was cut short before the JSON arrived proved nothing
    // about the file, so a later attempt may succeed.
    let UploadOutcome::Failed(failure) = &outcome else {
        panic!("expected a failure, got {outcome:?}");
    };
    assert_eq!(failure.class, Some(FailureClass::NetworkTransient));
    assert_eq!(failure.kind, FailureKind::Transient);
    let row = store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the failure was recorded");
    assert_eq!(row.state, ItemState::RetryWait);
    assert_eq!(row.error_class, Some(FailureClass::NetworkTransient));
    assert_eq!(server.upload_requests(), 0);
}

#[tokio::test]
async fn an_empty_upload_response_body_is_a_transient_network_failure() {
    let server = MockImmich::start().await;
    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 200,
            retry_after: None,
        },
    );
    let (uploader, _session, store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.txt");

    let outcome = uploader.upload_one(&path).await;

    let UploadOutcome::Failed(failure) = &outcome else {
        panic!("expected a failure, got {outcome:?}");
    };
    assert_eq!(failure.class, Some(FailureClass::NetworkTransient));
    assert_eq!(failure.kind, FailureKind::Transient);
    let row = store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the failure was recorded");
    assert_eq!(row.state, ItemState::RetryWait);
    assert_eq!(row.error_class, Some(FailureClass::NetworkTransient));
}

// --- The one re-login of R1.4 on the upload path ---

#[tokio::test]
async fn a_401_re_logs_in_once_and_the_retried_upload_succeeds() {
    let server = MockImmich::start().await;
    server.fail(
        Operation::Upload,
        Fault::Status {
            code: 401,
            retry_after: None,
        },
    );
    let (uploader, session, _store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.txt");

    let outcome = uploader.upload_one(&path).await;

    assert!(
        matches!(outcome, UploadOutcome::Created { .. }),
        "R1.4: the upload was retried after the re-login: {outcome:?}"
    );
    assert_eq!(server.upload_requests(), 2, "one retry, no more");
    assert_eq!(server.login_requests(), 2, "exactly one re-login");
    assert!(
        session.state().is_connected(),
        "the connection stayed usable: {:?}",
        session.state()
    );
}

#[tokio::test]
async fn a_second_401_on_the_upload_path_needs_attention() {
    let server = MockImmich::start().await;
    for _ in 0..2 {
        server.fail(
            Operation::Upload,
            Fault::Status {
                code: 401,
                retry_after: None,
            },
        );
    }
    let (uploader, session, store, temp) = uploader(&server).await;
    let path = hello_world_file(temp.path(), "hello.txt");

    let outcome = uploader.upload_one(&path).await;

    let UploadOutcome::Failed(failure) = &outcome else {
        panic!("expected a failure, got {outcome:?}");
    };
    assert_eq!(failure.class, Some(FailureClass::AuthFailed));
    assert_eq!(failure.kind, FailureKind::Permanent);
    assert_eq!(
        server.upload_requests(),
        2,
        "R1.4: one re-login, then the second 401 stops the upload"
    );
    assert_eq!(server.login_requests(), 2);
    assert!(
        matches!(
            session.state(),
            sync_core::connection::ConnectionState::NeedsAttention(
                sync_core::connection::Attention::Rejected
            )
        ),
        "R1.4: the connection needs the user: {:?}",
        session.state()
    );
    let row = store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the failure was recorded");
    assert_eq!(row.state, ItemState::FailedPermanent);
    assert_eq!(row.error_class, Some(FailureClass::AuthFailed));
}
