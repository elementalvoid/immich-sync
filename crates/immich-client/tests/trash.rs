//! The trash call against the offline Immich double (SPEC R5.3, R5.5).
//!
//! v1 has no permanent delete (ADR 0002), so the wire contract the client must
//! send is `DELETE /assets` with `force: false` and the asset ids, which the
//! double records and the test asserts.

use immich_client::{AssetUpload, Auth, BandwidthLimit, ClientError, ImmichClient, SessionToken};
use mock_immich::{DeleteCall, Fault, MockImmich, Operation};

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

/// Uploads one small file and returns its asset id.
async fn uploaded_asset(client: &ImmichClient, token: &SessionToken) -> String {
    let directory = tempfile::tempdir().expect("create a temp dir");
    let path = directory.path().join("photo.jpg");
    std::fs::write(&path, b"photo bytes").expect("write the fixture");
    client
        .upload_asset(
            Auth::Token(token),
            &AssetUpload {
                path: &path,
                checksum: "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
                created_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
                modified_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_040),
                sidecar: None,
                live_photo_video_id: None,
                bandwidth: BandwidthLimit::default(),
            },
        )
        .await
        .expect("the upload is created")
        .asset_id
}

#[tokio::test]
async fn trashing_an_asset_sends_the_delete_body_the_server_expects() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let asset_id = uploaded_asset(&client, &token).await;

    client
        .trash_assets(Auth::Token(&token), std::slice::from_ref(&asset_id))
        .await
        .expect("the mock trashes the asset");

    // The exact request, not only its outcome: the method, the path, the body
    // field names, and the force flag (Immich v3.3.1 `DELETE /assets`,
    // `AssetBulkDeleteDto`).
    assert_eq!(
        server.delete_calls(),
        vec![DeleteCall {
            ids: vec![asset_id.clone()],
            force: false,
        }],
        "R5.3: trash is a delete with force false, never a permanent delete"
    );
    assert!(server.trashed(&asset_id), "the asset is in the trash");
}

#[tokio::test]
async fn a_trash_without_the_permission_is_permission_denied() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let asset_id = uploaded_asset(&client, &token).await;
    server.fail(Operation::DeleteAssets, Fault::PermissionDenied);

    let error = client
        .trash_assets(Auth::Token(&token), std::slice::from_ref(&asset_id))
        .await
        .expect_err("the missing permission is refused");

    assert!(
        matches!(error, ClientError::PermissionDenied(_)),
        "R5.5: a missing permission is its own class, got {error:?}"
    );
    assert!(
        !server.trashed(&asset_id),
        "R5.5: a refused trash changes nothing on the server"
    );
}

#[tokio::test]
async fn restoring_an_asset_brings_it_back_from_the_trash() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let asset_id = uploaded_asset(&client, &token).await;
    client
        .trash_assets(Auth::Token(&token), std::slice::from_ref(&asset_id))
        .await
        .expect("the mock trashes the asset");
    assert!(server.trashed(&asset_id));

    client
        .restore_assets(Auth::Token(&token), std::slice::from_ref(&asset_id))
        .await
        .expect("the mock restores the asset");

    assert!(
        !server.trashed(&asset_id),
        "R5.3: a restore is the way back for a trashed checksum"
    );
}

#[tokio::test]
async fn a_restore_without_the_permission_is_permission_denied() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let asset_id = uploaded_asset(&client, &token).await;
    client
        .trash_assets(Auth::Token(&token), std::slice::from_ref(&asset_id))
        .await
        .expect("the mock trashes the asset");
    server.fail(Operation::RestoreAssets, Fault::PermissionDenied);

    let error = client
        .restore_assets(Auth::Token(&token), std::slice::from_ref(&asset_id))
        .await
        .expect_err("the missing permission is refused");

    assert!(
        matches!(error, ClientError::PermissionDenied(_)),
        "R5.5: a missing permission is its own class, got {error:?}"
    );
    assert!(
        server.trashed(&asset_id),
        "R5.5: a refused restore leaves the asset in the trash"
    );
}

#[tokio::test]
async fn an_api_key_trashes_too() {
    let server = MockImmich::start().await;
    let client = client(&server);
    let token = token(&client).await;
    let asset_id = uploaded_asset(&client, &token).await;

    client
        .trash_assets(
            Auth::ApiKey("api-key-1234"),
            std::slice::from_ref(&asset_id),
        )
        .await
        .expect("the mock accepts any non-empty API key");

    assert!(server.trashed(&asset_id));
}
