//! The album operations against the offline Immich double (SPEC R3.4, R3.5,
//! R6.3).
//!
//! Every operation here is `x-immich-state: Stable` in the v3.3.1 spec, which
//! is what R3.5 allows sync correctness to depend on.

use immich_client::{AssetUpload, Auth, BandwidthLimit, ClientError, ImmichClient, SessionToken};
use mock_immich::{AlbumRenameCall, Fault, MockImmich, Operation};

/// `sha1("hello world")`, base64.
const HELLO_WORLD_SHA1_BASE64: &str = "Kq5sNclPz7QV2+lfQIuc6R7oRu0=";

fn client(server: &MockImmich) -> ImmichClient {
    ImmichClient::new(server.base_url()).expect("the mock URL is valid")
}

async fn token(client: &ImmichClient) -> SessionToken {
    client
        .login("sync@example.com", "correct horse")
        .await
        .expect("the mock accepts these credentials")
        .access_token
}

/// Uploads `hello.txt` and returns the asset id the server assigned.
async fn upload_asset(client: &ImmichClient, token: &SessionToken) -> String {
    let directory = tempfile::tempdir().expect("create a temp dir");
    let path = directory.path().join("hello.txt");
    std::fs::write(&path, b"hello world").expect("write the fixture file");
    client
        .upload_asset(
            Auth::Token(token),
            &AssetUpload {
                path: &path,
                checksum: HELLO_WORLD_SHA1_BASE64,
                created_at: std::time::UNIX_EPOCH,
                modified_at: std::time::UNIX_EPOCH,
                sidecar: None,
                live_photo_video_id: None,
                bandwidth: BandwidthLimit::default(),
            },
        )
        .await
        .expect("the mock accepts the upload")
        .asset_id
}

#[tokio::test]
async fn creating_an_album_returns_the_id_and_name_the_server_kept() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let asset = upload_asset(&client, &token).await;

    let album = client
        .create_album(Auth::Token(&token), "Holiday", std::slice::from_ref(&asset))
        .await
        .expect("the mock creates the album");

    assert!(
        !album.id.is_empty(),
        "R3.4: the album id is what the client needs"
    );
    assert_eq!(album.name, "Holiday");
    assert_eq!(server.album_for_asset(&asset).as_deref(), Some("Holiday"));
}

#[tokio::test]
async fn adding_an_asset_twice_leaves_one_membership() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let asset = upload_asset(&client, &token).await;
    let album = client
        .create_album(Auth::Token(&token), "Holiday", &[])
        .await
        .expect("the mock creates the album");

    // The same asset added twice is how a duplicate upload and a fresh upload
    // of the same folder meet the same album.
    for _ in 0..2 {
        client
            .add_assets_to_album(Auth::Token(&token), &album.id, std::slice::from_ref(&asset))
            .await
            .expect("adding an asset the album already holds is not a failure");
    }

    assert_eq!(
        server.album_for_asset(&asset).as_deref(),
        Some("Holiday"),
        "the asset is in the album"
    );
    assert_eq!(server.add_album_requests(), 2);
}

#[tokio::test]
async fn an_unknown_album_is_a_protocol_failure_not_a_transport_one() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let asset = upload_asset(&client, &token).await;

    let error = client
        .add_assets_to_album(
            Auth::Token(&token),
            "99999999-9999-4999-8999-999999999999",
            &[asset],
        )
        .await
        .expect_err("an unknown album is refused");

    assert!(
        matches!(error, ClientError::Protocol { status: 404, .. }),
        "a 404 keeps its status, so the caller knows it is permanent: {error:?}"
    );
}

#[tokio::test]
async fn a_credential_without_album_permission_is_a_permission_failure() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    server.fail(Operation::CreateAlbum, Fault::PermissionDenied);

    let error = client
        .create_album(Auth::Token(&token), "Holiday", &[])
        .await
        .expect_err("the server refuses the album");

    assert!(
        matches!(error, ClientError::PermissionDenied(_)),
        "R6.3: a 403 is the named permission class: {error:?}"
    );
    assert!(
        !error
            .redacted_reason()
            .contains("Missing required permission"),
        "the server's message stays out of the log line (N3)"
    );
}

#[tokio::test]
async fn a_dropped_connection_is_a_transport_failure_the_caller_can_retry() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let asset = upload_asset(&client, &token).await;
    server.fail(Operation::AddAssetsToAlbum, Fault::Disconnect);

    let error = client
        .add_assets_to_album(Auth::Token(&token), "album-1", &[asset])
        .await
        .expect_err("the connection is dropped");

    assert!(
        matches!(error, ClientError::Unreachable(_)),
        "R6.1: a transport failure stays retriable for the caller: {error:?}"
    );
}

/// #47: a rename is `PATCH /albums/{id}` with the title in `albumName`, which
/// the spec's `UpdateAlbumDto` names. The recorded call is the assertion: the
/// album's new name alone cannot prove the endpoint or the field.
#[tokio::test]
async fn renaming_an_album_patches_the_album_with_its_new_name() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let album = client
        .create_album(Auth::Token(&token), "Holiday", &[])
        .await
        .expect("the mock creates the album");

    let renamed = client
        .rename_album(Auth::Token(&token), &album.id, "Holiday 2024")
        .await
        .expect("the mock renames the album");

    assert_eq!(renamed.id, album.id, "the album keeps its identity");
    assert_eq!(renamed.name, "Holiday 2024");
    assert_eq!(
        server.album_rename_calls(),
        vec![AlbumRenameCall {
            album_id: album.id.clone(),
            album_name: "Holiday 2024".to_string(),
        }],
        "#47: the title travels in the body's albumName field, on the album's own path"
    );
    assert_eq!(
        server.album_names(),
        vec!["Holiday 2024".to_string()],
        "the album the server holds carries the new name"
    );
    assert!(
        server
            .requests()
            .contains(&format!("PATCH /api/albums/{}", album.id)),
        "the request is a PATCH on the album: {:?}",
        server.requests()
    );
}

/// #47: an album the user deleted in Immich is a 404 the caller can tell from
/// transport noise, so it can create the album again instead of retrying.
#[tokio::test]
async fn renaming_an_album_the_server_does_not_hold_is_a_protocol_404() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;

    let error = client
        .rename_album(
            Auth::Token(&token),
            "99999999-9999-4999-8999-999999999999",
            "Holiday 2024",
        )
        .await
        .expect_err("an unknown album is refused");

    assert!(
        matches!(error, ClientError::Protocol { status: 404, .. }),
        "a 404 keeps its status, so the caller knows the album is gone: {error:?}"
    );
}

/// #47: an API key without `album.update` is the named permission class, which
/// is what the folder detail tells the user to grant.
#[tokio::test]
async fn a_credential_without_album_update_is_a_permission_failure() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let album = client
        .create_album(Auth::Token(&token), "Holiday", &[])
        .await
        .expect("the mock creates the album");
    server.fail(Operation::UpdateAlbum, Fault::PermissionDenied);

    let error = client
        .rename_album(Auth::Token(&token), &album.id, "Holiday 2024")
        .await
        .expect_err("the key may not rename the album");

    assert!(
        matches!(error, ClientError::PermissionDenied(_)),
        "R6.3: a 403 is the named permission class: {error:?}"
    );
    assert_eq!(
        server.album_rename_calls(),
        vec![AlbumRenameCall {
            album_id: album.id.clone(),
            album_name: "Holiday 2024".to_string(),
        }],
        "#47: the refused request still put the title on the wire"
    );
}

/// #47: a dropped connection stays retriable, so the title is kept and sent
/// again rather than reported as a permanent refusal.
#[tokio::test]
async fn a_dropped_rename_is_a_transport_failure_the_caller_can_retry() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let album = client
        .create_album(Auth::Token(&token), "Holiday", &[])
        .await
        .expect("the mock creates the album");
    server.fail(Operation::UpdateAlbum, Fault::Disconnect);

    let error = client
        .rename_album(Auth::Token(&token), &album.id, "Holiday 2024")
        .await
        .expect_err("the connection is dropped");

    assert!(
        matches!(error, ClientError::Unreachable(_)),
        "a transport failure stays retriable for the caller: {error:?}"
    );
}
