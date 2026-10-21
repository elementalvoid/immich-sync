//! The album title (#47, SPEC R3.4, GLOSSARY "Album title").
//!
//! Every test drives the real pipeline: a real directory, the real scan, the
//! real state database, and the double's record of what reached the wire. The
//! title is one stored value; the tests assert where it lands, not how the
//! mapper holds it on the way.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mock_immich::{AlbumRenameCall, Fault, MockImmich, Operation};
use sync_core::connection::{AuthMethod, Connection, Secret, SharedSession, SignIn};
use sync_core::credential::MemoryCredentialStore;
use sync_core::engine::{Engine, EngineConfig};
use sync_core::paths::path_key;
use sync_core::status::AlbumRename;
use sync_core::store::{Album, AlbumScope, ItemState, SourceFolder, StateStore};
use sync_core::tracking::Tracking;

use common::wait_until;

/// Signs in with the credential the test names, so a test can drive the
/// API-key path (#47).
async fn signed_in_with(server: &MockImmich, method: AuthMethod, secret: &str) -> SharedSession {
    let mut connection = Connection::new(server.base_url(), Arc::new(MemoryCredentialStore::new()))
        .expect("a valid URL");
    connection
        .sign_in(SignIn {
            email: "sync@example.com".to_string(),
            secret: Secret::new(secret),
            method,
        })
        .await;
    connection.shared()
}

/// Opens a store in a fresh temp directory.
fn store_in(temp: &tempfile::TempDir) -> Arc<StateStore> {
    Arc::new(StateStore::open(temp.path().join("data").join("state.db")).expect("open the store"))
}

/// Writes one fixture file below `root`.
fn write_file(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create the fixture directory");
    }
    std::fs::write(&path, bytes).expect("write the fixture");
    path
}

/// One mapped folder, with the title the user chose (#47).
fn mapped_folder(path: &Path, scope: AlbumScope, title: Option<&str>) -> SourceFolder {
    SourceFolder {
        path: path.display().to_string(),
        recursive: true,
        album_mapping: true,
        album_scope: scope,
        album_title: title.map(str::to_string),
    }
}

/// Starts an engine over `store`, signed in with `session`, with the watcher
/// off so the reconciliation scan is the one driver.
async fn start(store: &Arc<StateStore>, session: SharedSession) -> (Arc<Engine>, Arc<Tracking>) {
    let tracking = Arc::new(Tracking::new(store.clone()));
    let engine = Arc::new(
        Engine::new(
            tracking.clone(),
            session,
            EngineConfig {
                watch: false,
                ..EngineConfig::default()
            },
        )
        .expect("build the engine"),
    );
    engine.start();
    (engine, tracking)
}

/// The album row of the folder at `path`.
fn album_of(store: &StateStore, path: &Path) -> Album {
    store
        .album(&path_key(path))
        .expect("read the album row")
        .unwrap_or_default()
}

/// The asset id the store recorded for one file.
fn asset_id_of(store: &StateStore, path: &Path) -> String {
    store
        .item(&path.display().to_string())
        .expect("read the row")
        .expect("the upload was recorded")
        .asset_id
        .expect("a finished row carries an asset id (R3.6)")
}

/// Waits until every file has a terminal success row and its album holds it.
async fn wait_for_albums(store: &StateStore, server: &MockImmich, files: &[PathBuf]) -> bool {
    wait_until(
        || {
            files.iter().all(|file| {
                let row = store.item(&file.display().to_string()).ok().flatten();
                let Some(asset) = row.and_then(|row| row.asset_id) else {
                    return false;
                };
                store
                    .item(&file.display().to_string())
                    .ok()
                    .flatten()
                    .is_some_and(|row| row.state == ItemState::Uploaded)
                    && server.album_for_asset(&asset).is_some()
            })
        },
        Duration::from_secs(20),
    )
    .await
}

/// The two facts a settled rename leaves behind: the server holds `title` for
/// the album of `file`, and the store no longer owes the rename.
///
/// The server applies the rename before the client can record the answer, so a
/// test that watched only the server would read a store row that is still
/// pending (#51).
fn rename_settled(
    store: &StateStore,
    server: &MockImmich,
    photos: &Path,
    file: &Path,
    title: &str,
) -> bool {
    !album_of(store, photos).rename_pending
        && server.album_for_asset(&asset_id_of(store, file)).as_deref() == Some(title)
}

/// #47: a new album is created with the title the user chose.
#[tokio::test]
async fn a_new_album_is_created_with_the_custom_title() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let file = write_file(&photos, "cover.jpg", b"cover bytes");
    store
        .add_folder(&mapped_folder(
            &photos,
            AlbumScope::WholeFolder,
            Some("Holiday 2024"),
        ))
        .expect("add the mapped folder");

    let (engine, _tracking) = start(
        &store,
        signed_in_with(&server, AuthMethod::Password, "correct horse").await,
    )
    .await;
    assert!(
        wait_for_albums(&store, &server, std::slice::from_ref(&file)).await,
        "the upload did not join an album: {:?}",
        engine.status()
    );

    assert_eq!(
        server.album_names(),
        vec!["Holiday 2024".to_string()],
        "#47: the create carries the user's title, not the folder's name"
    );
    assert_eq!(
        server
            .album_for_asset(&asset_id_of(&store, &file))
            .as_deref(),
        Some("Holiday 2024")
    );
    assert_eq!(
        album_of(&store, &photos),
        Album {
            title: Some("Holiday 2024".to_string()),
            album_id: album_of(&store, &photos).album_id,
            rename_pending: false,
            rename_problem: None,
        },
        "#47: the created album owes the server no rename"
    );
    assert!(
        server.album_rename_calls().is_empty(),
        "a create is not a rename"
    );
    engine.stop();
}

/// #47: a blank title means the folder's name, which is the placeholder the
/// setup dialog shows.
#[tokio::test]
async fn a_blank_title_creates_the_album_with_the_folder_name() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let file = write_file(&photos, "cover.jpg", b"cover bytes");
    store
        .add_folder(&mapped_folder(&photos, AlbumScope::WholeFolder, None))
        .expect("add the mapped folder");

    let (engine, _tracking) = start(
        &store,
        signed_in_with(&server, AuthMethod::Password, "correct horse").await,
    )
    .await;
    assert!(wait_for_albums(&store, &server, &[file]).await);

    assert_eq!(
        server.album_names(),
        vec!["Pictures".to_string()],
        "#47: a blank title falls back to the folder's name"
    );
    assert_eq!(album_of(&store, &photos).title, None);
    engine.stop();
}

/// #47: changing the title of an existing album sends `PATCH /albums/{id}` and
/// the stored title follows the answer.
#[tokio::test]
async fn changing_the_title_renames_the_existing_album() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let file = write_file(&photos, "cover.jpg", b"cover bytes");
    store
        .add_folder(&mapped_folder(&photos, AlbumScope::WholeFolder, None))
        .expect("add the mapped folder");

    let (engine, _tracking) = start(
        &store,
        signed_in_with(&server, AuthMethod::Password, "correct horse").await,
    )
    .await;
    assert!(wait_for_albums(&store, &server, std::slice::from_ref(&file)).await);
    let album_id = album_of(&store, &photos).album_id.expect("the album id");

    engine
        .set_album_title(&photos.display().to_string(), "Holiday 2024")
        .expect("set the title");

    assert!(
        wait_until(
            || album_of(&store, &photos).title.as_deref() == Some("Holiday 2024")
                && !album_of(&store, &photos).rename_pending
                && server
                    .album_for_asset(&asset_id_of(&store, &file))
                    .as_deref()
                    == Some("Holiday 2024"),
            Duration::from_secs(15)
        )
        .await,
        "the rename did not reach the server: {:?} {:?}",
        server.album_rename_calls(),
        engine.status()
    );
    assert_eq!(
        server.album_rename_calls(),
        vec![AlbumRenameCall {
            album_id: album_id.clone(),
            album_name: "Holiday 2024".to_string(),
        }],
        "#47: the title travels on the album the client already holds"
    );
    assert_eq!(
        album_of(&store, &photos).album_id.as_deref(),
        Some(album_id.as_str()),
        "a rename keeps the album"
    );
    assert_eq!(
        engine.status().folders[0].album_title.as_deref(),
        Some("Holiday 2024"),
        "#47: the folder detail reads the stored title"
    );
    engine.stop();
}

/// #47: clearing the title renames the album back to the folder's name.
#[tokio::test]
async fn clearing_the_title_renames_the_album_back_to_the_folder_name() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let file = write_file(&photos, "cover.jpg", b"cover bytes");
    store
        .add_folder(&mapped_folder(
            &photos,
            AlbumScope::WholeFolder,
            Some("Holiday 2024"),
        ))
        .expect("add the mapped folder");

    let (engine, _tracking) = start(
        &store,
        signed_in_with(&server, AuthMethod::Password, "correct horse").await,
    )
    .await;
    assert!(wait_for_albums(&store, &server, std::slice::from_ref(&file)).await);
    assert_eq!(server.album_names(), vec!["Holiday 2024".to_string()]);

    // Whitespace is what the field holds once the user clears it.
    engine
        .set_album_title(&photos.display().to_string(), "   ")
        .expect("clear the title");

    assert!(
        wait_until(
            || server
                .album_for_asset(&asset_id_of(&store, &file))
                .as_deref()
                == Some("Pictures"),
            Duration::from_secs(15)
        )
        .await,
        "the album was not renamed back: {:?}",
        server.album_rename_calls()
    );
    assert_eq!(
        album_of(&store, &photos),
        Album {
            title: None,
            album_id: album_of(&store, &photos).album_id,
            rename_pending: false,
            rename_problem: None,
        },
        "#47: a cleared title means the folder's name, and it landed"
    );
    assert_eq!(
        server.album_rename_calls()[0].album_name,
        "Pictures",
        "the server is told the fallback name"
    );
    engine.stop();
}

/// #47: an offline rename keeps the title and its reason, and the next scan
/// sends it again. The field shows the desired title meanwhile.
#[tokio::test]
async fn an_offline_rename_is_retried_on_the_next_scan_and_the_title_stays() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let file = write_file(&photos, "cover.jpg", b"cover bytes");
    store
        .add_folder(&mapped_folder(&photos, AlbumScope::WholeFolder, None))
        .expect("add the mapped folder");

    let (engine, _tracking) = start(
        &store,
        signed_in_with(&server, AuthMethod::Password, "correct horse").await,
    )
    .await;
    assert!(wait_for_albums(&store, &server, std::slice::from_ref(&file)).await);
    let album_id = album_of(&store, &photos).album_id.expect("the album id");
    server.fail(Operation::UpdateAlbum, Fault::Disconnect);

    engine
        .set_album_title(&photos.display().to_string(), "Holiday 2024")
        .expect("set the title");

    assert!(
        wait_until(
            || {
                let album = album_of(&store, &photos);
                album.rename_pending && album.rename_problem.is_some()
            },
            Duration::from_secs(15)
        )
        .await,
        "the failed rename was not kept pending: {:?}",
        engine.status()
    );
    let album = album_of(&store, &photos);
    assert_eq!(
        album.title.as_deref(),
        Some("Holiday 2024"),
        "#47: a failed rename does not lose the desired title"
    );
    assert!(
        album
            .rename_problem
            .as_deref()
            .is_some_and(|reason| reason.contains("could not be reached")),
        "the reason says the server was unreachable: {:?}",
        album.rename_problem
    );
    let folder = &engine.status().folders[0];
    assert_eq!(
        folder.album_title.as_deref(),
        Some("Holiday 2024"),
        "#47: the detail shows the desired title while the rename is pending"
    );
    assert!(
        matches!(&folder.album_rename, Some(AlbumRename::Failed(reason)) if reason.contains("could not be reached")),
        "the detail has the reason: {:?}",
        folder.album_rename
    );
    assert_eq!(
        server
            .album_for_asset(&asset_id_of(&store, &file))
            .as_deref(),
        Some("Pictures"),
        "the server still holds the old name"
    );

    // A scan is what a reconnect and a restart both reach, and it sends the
    // title again.
    engine.sync_now();
    assert!(
        wait_until(
            || rename_settled(&store, &server, &photos, &file, "Holiday 2024"),
            Duration::from_secs(15)
        )
        .await,
        "the retry did not confirm the rename: {:?} {:?}",
        server.album_rename_calls(),
        album_of(&store, &photos)
    );
    let album = album_of(&store, &photos);
    assert!(!album.rename_pending, "the retry settled the rename");
    assert_eq!(album.rename_problem, None, "and cleared the reason");
    assert_eq!(
        album.album_id.as_deref(),
        Some(album_id.as_str()),
        "the retry renames the same album"
    );
    assert_eq!(server.album_rename_calls().len(), 2);
    engine.stop();
}

/// #47: a pending title is sent on the next start, without the user doing
/// anything.
#[tokio::test]
async fn a_pending_title_is_sent_on_the_next_start() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let file = write_file(&photos, "cover.jpg", b"cover bytes");
    store
        .add_folder(&mapped_folder(&photos, AlbumScope::WholeFolder, None))
        .expect("add the mapped folder");

    let (engine, tracking) = start(
        &store,
        signed_in_with(&server, AuthMethod::Password, "correct horse").await,
    )
    .await;
    assert!(wait_for_albums(&store, &server, std::slice::from_ref(&file)).await);
    // The client is stopped when the title is stored: nothing can be sent.
    engine.stop();
    drop(engine);
    tracking
        .set_album_title(&photos.display().to_string(), "Holiday 2024")
        .expect("store the title while the engine is stopped");
    assert!(album_of(&store, &photos).rename_pending);

    let (engine, _tracking) = start(
        &store,
        signed_in_with(&server, AuthMethod::Password, "correct horse").await,
    )
    .await;
    assert!(
        wait_until(
            || rename_settled(&store, &server, &photos, &file, "Holiday 2024"),
            Duration::from_secs(20)
        )
        .await,
        "#47: a start retries the stored title: {:?} {:?}",
        server.album_rename_calls(),
        album_of(&store, &photos)
    );
    assert!(!album_of(&store, &photos).rename_pending);
    engine.stop();
}

/// #47: an album deleted in Immich is created again by the rename, and the new
/// id replaces the stale one.
#[tokio::test]
async fn a_rename_for_a_deleted_album_recreates_it_with_the_title() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let file = write_file(&photos, "cover.jpg", b"cover bytes");
    store
        .add_folder(&mapped_folder(&photos, AlbumScope::WholeFolder, None))
        .expect("add the mapped folder");

    let (engine, _tracking) = start(
        &store,
        signed_in_with(&server, AuthMethod::Password, "correct horse").await,
    )
    .await;
    assert!(wait_for_albums(&store, &server, std::slice::from_ref(&file)).await);
    let stale_id = album_of(&store, &photos).album_id.expect("the album id");
    // The user deleted the album in Immich.
    server.delete_album(&stale_id);

    engine
        .set_album_title(&photos.display().to_string(), "Holiday 2024")
        .expect("set the title");

    assert!(
        wait_until(
            || {
                let album = album_of(&store, &photos);
                !album.rename_pending
                    && album.album_id.as_deref().is_some_and(|id| id != stale_id)
                    && album.title.as_deref() == Some("Holiday 2024")
            },
            Duration::from_secs(15)
        )
        .await,
        "the deleted album was not created again: {:?} {:?}",
        album_of(&store, &photos),
        server.album_rename_calls()
    );
    assert_eq!(
        server.album_rename_calls(),
        vec![AlbumRenameCall {
            album_id: stale_id,
            album_name: "Holiday 2024".to_string(),
        }],
        "#47: the stale id is what the client tried before recreating"
    );
    assert_eq!(
        server.album_names(),
        vec!["Holiday 2024".to_string()],
        "the new album carries the title"
    );
    let recreated = album_of(&store, &photos).album_id;
    assert!(
        recreated.as_deref().is_some_and(|id| !id.is_empty()),
        "#47: the row holds the id the recreate returned"
    );
    assert_eq!(
        server.album_names().len(),
        1,
        "the stale album was replaced, not duplicated"
    );
    engine.stop();
}

/// #47: an API key without `album.update` names the missing permission and
/// keeps the title pending.
#[tokio::test]
async fn an_api_key_without_album_update_keeps_the_title_pending_and_names_it() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let file = write_file(&photos, "cover.jpg", b"cover bytes");
    store
        .add_folder(&mapped_folder(&photos, AlbumScope::WholeFolder, None))
        .expect("add the mapped folder");

    let (engine, _tracking) = start(
        &store,
        signed_in_with(&server, AuthMethod::ApiKey, "test-key").await,
    )
    .await;
    assert!(wait_for_albums(&store, &server, std::slice::from_ref(&file)).await);
    server.fail(Operation::UpdateAlbum, Fault::PermissionDenied);

    engine
        .set_album_title(&photos.display().to_string(), "Holiday 2024")
        .expect("set the title");

    assert!(
        wait_until(
            || {
                album_of(&store, &photos)
                    .rename_problem
                    .as_deref()
                    .is_some_and(|reason| reason.contains("album.update"))
            },
            Duration::from_secs(15)
        )
        .await,
        "the refusal was not recorded: {:?}",
        engine.status()
    );
    let album = album_of(&store, &photos);
    assert!(
        album.rename_pending,
        "#47: the change is kept until it lands"
    );
    assert!(
        album
            .rename_problem
            .as_deref()
            .is_some_and(|reason| reason.contains("API key") && reason.contains("album.update")),
        "the reason names the credential and the permission: {:?}",
        album.rename_problem
    );
    assert_eq!(
        album.title.as_deref(),
        Some("Holiday 2024"),
        "the desired title is not lost"
    );
    assert_eq!(
        server
            .album_for_asset(&asset_id_of(&store, &file))
            .as_deref(),
        Some("Pictures"),
        "the server kept its name"
    );
    engine.stop();
}

/// #47: under the leaf scope only the source folder's own album takes the
/// title; a subfolder's album keeps the subfolder's name.
#[tokio::test]
async fn a_leaf_subfolder_album_keeps_its_folder_name() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let at_root = write_file(&photos, "cover.jpg", b"cover bytes");
    let nested = write_file(&photos, "Trips/beach.jpg", b"beach bytes");
    store
        .add_folder(&mapped_folder(
            &photos,
            AlbumScope::Leaf,
            Some("Holiday 2024"),
        ))
        .expect("add the mapped folder");

    let (engine, _tracking) = start(
        &store,
        signed_in_with(&server, AuthMethod::Password, "correct horse").await,
    )
    .await;
    assert!(
        wait_for_albums(&store, &server, &[at_root.clone(), nested.clone()]).await,
        "both files must join an album: {:?}",
        engine.status()
    );

    let mut names = server.album_names();
    names.sort();
    assert_eq!(
        names,
        vec!["Holiday 2024".to_string(), "Trips".to_string()],
        "#47: the source folder's album takes the title, the subfolder's keeps its name"
    );
    assert_eq!(
        server
            .album_for_asset(&asset_id_of(&store, &at_root))
            .as_deref(),
        Some("Holiday 2024"),
        "a file directly in the source folder joins the titled album"
    );
    assert_eq!(
        server
            .album_for_asset(&asset_id_of(&store, &nested))
            .as_deref(),
        Some("Trips")
    );
    engine.stop();
}

/// #47: the title survives a restart, toggling the mapping, and removing and
/// adding the folder again once the album exists.
#[tokio::test]
async fn the_title_survives_a_restart_a_toggle_and_a_re_add() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let photos = temp.path().join("Pictures");
    let file = write_file(&photos, "cover.jpg", b"cover bytes");
    let path = photos.display().to_string();
    store
        .add_folder(&mapped_folder(&photos, AlbumScope::WholeFolder, None))
        .expect("add the mapped folder");

    let (engine, tracking) = start(
        &store,
        signed_in_with(&server, AuthMethod::Password, "correct horse").await,
    )
    .await;
    assert!(wait_for_albums(&store, &server, std::slice::from_ref(&file)).await);
    let album_id = album_of(&store, &photos).album_id.expect("the album id");
    engine
        .set_album_title(&path, "Holiday 2024")
        .expect("set the title");
    assert!(
        wait_until(
            || server
                .album_for_asset(&asset_id_of(&store, &file))
                .as_deref()
                == Some("Holiday 2024"),
            Duration::from_secs(15)
        )
        .await
    );

    // Toggling the mapping writes the folder row with the title it holds.
    tracking.set_album_mapping(&path, false).expect("unmap");
    tracking.set_album_mapping(&path, true).expect("map again");
    assert_eq!(
        store.folders().expect("read the folders")[0].album_title,
        Some("Holiday 2024".to_string()),
        "#47: a mapping toggle keeps the title"
    );

    // A restart reads the store back.
    engine.stop();
    drop(engine);
    let (engine, tracking) = start(
        &store,
        signed_in_with(&server, AuthMethod::Password, "correct horse").await,
    )
    .await;
    assert_eq!(
        engine.status().folders[0].album_title.as_deref(),
        Some("Holiday 2024"),
        "#47: the title survives a restart"
    );

    // Removing and adding the folder again keeps the album row, and the title
    // with it: the setup dialog's blank field is not an edit.
    assert!(tracking.remove_folder(&path).expect("remove"));
    assert_eq!(
        store.album(&path_key(&photos)).expect("read the album"),
        Some(Album {
            title: Some("Holiday 2024".to_string()),
            album_id: Some(album_id.clone()),
            rename_pending: false,
            rename_problem: None,
        }),
        "#47: the album row, and the title on it, outlives the folder row"
    );
    tracking
        .insert_folder(&SourceFolder {
            path: path.clone(),
            recursive: true,
            album_mapping: true,
            album_scope: AlbumScope::WholeFolder,
            album_title: None,
        })
        .expect("add the folder again through the setup dialog");
    assert_eq!(
        store.folders().expect("read the folders")[0].album_title,
        Some("Holiday 2024".to_string()),
        "#47: re-adding the folder keeps the title"
    );
    assert_eq!(
        album_of(&store, &photos).album_id.as_deref(),
        Some(album_id.as_str()),
        "#47: and the album it already made"
    );
    engine.stop();
}

/// #47: Immich does not require unique titles, so two folders may share one.
/// The albums stay apart because the folder key is the identity (R3.4).
#[tokio::test]
async fn two_folders_may_share_one_album_title() {
    let server = MockImmich::start().await;
    let temp = tempfile::tempdir().expect("create a temp dir");
    let store = store_in(&temp);
    let first = temp.path().join("2023").join("Pictures");
    let second = temp.path().join("2024").join("Pictures");
    let first_file = write_file(&first, "cover.jpg", b"first cover");
    let second_file = write_file(&second, "cover.jpg", b"second cover");
    for folder in [&first, &second] {
        store
            .add_folder(&mapped_folder(
                folder,
                AlbumScope::WholeFolder,
                Some("Holiday 2024"),
            ))
            .expect("add the mapped folder");
    }

    let (engine, _tracking) = start(
        &store,
        signed_in_with(&server, AuthMethod::Password, "correct horse").await,
    )
    .await;
    assert!(
        wait_for_albums(&store, &server, &[first_file.clone(), second_file.clone()]).await,
        "both uploads must join an album: {:?}",
        engine.status()
    );

    assert_eq!(
        server.album_names(),
        vec!["Holiday 2024".to_string(), "Holiday 2024".to_string()],
        "#47: two folders may carry one title; Immich does not merge them"
    );
    assert_ne!(
        album_of(&store, &first).album_id,
        album_of(&store, &second).album_id,
        "one album per folder, whatever the titles agree on"
    );
    engine.stop();
}
