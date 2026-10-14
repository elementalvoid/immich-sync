//! Album mapping (SPEC R3.4, R3.5, GLOSSARY).
//!
//! Album mapping is an opt-in per-folder rule that copies a folder name to the
//! name of an Immich album. The album scope is per folder: the whole source
//! folder, or every folder that directly contains files.
//!
//! Two decisions this module makes, both from the glossary:
//!
//! - The album name is the folder's own name, so the user recognises the album.
//!   It is a copy, not the identity: Immich does not enforce album-name
//!   uniqueness, so nothing about the server merges two folders.
//! - The album belongs to the **folder**, not the name, and is keyed by that
//!   folder's path key. Two folders that share a name are two albums (R3.4),
//!   and one folder spelled with another case is one album (R2.4).
//!   The client's only album write is "add to album", so an album that held
//!   two folders could never be split again.
//!
//! `POST /albums` and `PUT /albums/{id}/assets` are `x-immich-state: Stable`
//! in the v3.3.1 spec, which is what R3.5 allows sync correctness to depend on.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use immich_client::ClientError;
use tokio::sync::Mutex;

use crate::connection::SharedSession;
use crate::paths::path_key;
use crate::store::{AlbumScope, SourceFolder, StateStore};

/// The album one file joins: the name it carries and the key of the folder it
/// belongs to, or `None` when its folder is not mapped (R3.4).
///
/// Under the whole-folder scope every file below the source folder joins one
/// album held by that source folder. Under the leaf scope a file joins the
/// album held by the folder that directly contains it, which is the source
/// folder itself when the file sits at its root (GLOSSARY).
fn album_of(folder: &SourceFolder, file: &Path) -> Option<(String, String)> {
    if !folder.album_mapping {
        return None;
    }
    let root = Path::new(&folder.path);
    let holder: PathBuf = match folder.album_scope {
        AlbumScope::WholeFolder => root.to_path_buf(),
        AlbumScope::Leaf => file
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(root)
            .to_path_buf(),
    };
    Some((folder_name(&holder), path_key(&holder)))
}

/// The name one folder contributes to an album.
///
/// A folder with no last component (a drive root, or `/`) would give an empty
/// album name, which no album can be found under again, so the whole path is
/// used instead.
#[must_use]
pub fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Puts finished assets into the albums their folders map to (R3.4).
///
/// The mapper holds no sync decision of its own: the caller has already
/// recorded the upload, so a failure here can only be reported, never retried
/// through the upload path.
#[derive(Debug)]
pub struct AlbumMapper {
    /// The connection session, so an album call carries the current credential
    /// and a 401 is answered by the one re-login rule (R1.4) like every other
    /// request the engine makes.
    session: SharedSession,
    store: Arc<StateStore>,
    /// The album ids this client knows, by the mapped folder's path key.
    ///
    /// The lock is held across the one request that creates an album, so two
    /// workers uploading the first files of one folder cannot both create it.
    known: Mutex<HashMap<String, String>>,
}

impl AlbumMapper {
    /// Builds a mapper over the connection session and the app-scoped state
    /// store.
    #[must_use]
    pub fn new(session: SharedSession, store: Arc<StateStore>) -> Self {
        Self {
            session,
            store,
            known: Mutex::new(HashMap::new()),
        }
    }

    /// Adds `asset_id` to the album `file` maps to under `folder`, creating
    /// that album when the client has none for the folder yet.
    ///
    /// Returns the album the asset joined, or `None` when the folder is not
    /// mapped (R3.4). An asset the album already holds is left alone, so a
    /// duplicate upload (R3.2) and a fresh one are both harmless.
    ///
    /// # Errors
    ///
    /// Returns the [`ClientError`] of the album call so the caller can report
    /// it against the folder. The finished upload is untouched either way.
    pub async fn attach(
        &self,
        folder: &SourceFolder,
        file: &Path,
        asset_id: &str,
    ) -> Result<Option<String>, ClientError> {
        let Some((name, folder_key)) = album_of(folder, file) else {
            return Ok(None);
        };
        let album = self.album_id(&folder_key, &name).await?;
        let client = self.session.client();
        let requested_album = album.clone();
        let asset = asset_id.to_string();
        self.session
            .request(move |credential| {
                let client = client.clone();
                let requested_album = requested_album.clone();
                let asset = asset.clone();
                async move {
                    client
                        .add_assets_to_album(credential.auth(), &requested_album, &[asset])
                        .await
                }
            })
            .await?;
        tracing::debug!(album = %name, folder = %folder_key, asset_id, "the asset joined its album");
        Ok(Some(name))
    }

    /// The album id for the mapped folder `folder_key`: the one this client
    /// made before, or a new one.
    ///
    /// # Errors
    ///
    /// Returns the [`ClientError`] of the create call. A state database that
    /// cannot record the new id is logged and the album is still used, because
    /// the next file can find it in this run's cache.
    async fn album_id(&self, folder_key: &str, name: &str) -> Result<String, ClientError> {
        let mut known = self.known.lock().await;
        if let Some(id) = known.get(folder_key) {
            return Ok(id.clone());
        }
        let stored = match self.store.album(folder_key) {
            Ok(id) => id,
            Err(error) => {
                tracing::error!(album = %name, folder = %folder_key, error = %error, "the album table could not be read");
                None
            }
        };
        let id = match stored {
            Some(id) => id,
            None => {
                let client = self.session.client();
                let album_name = name.to_string();
                let album = self
                    .session
                    .request(move |credential| {
                        let client = client.clone();
                        let album_name = album_name.clone();
                        async move {
                            client
                                .create_album(credential.auth(), &album_name, &[])
                                .await
                        }
                    })
                    .await?;
                tracing::info!(album = %name, album_id = %album.id, "an album was created");
                if let Err(error) = self.store.remember_album(folder_key, name, &album.id) {
                    tracing::error!(album = %name, folder = %folder_key, error = %error, "the album id could not be recorded");
                }
                album.id
            }
        };
        known.insert(folder_key.to_string(), id.clone());
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AlbumScope;

    /// A mapped or unmapped folder at `/photos`.
    fn folder(mapping: bool, scope: AlbumScope) -> SourceFolder {
        SourceFolder {
            path: "/photos".to_string(),
            recursive: true,
            album_mapping: mapping,
            album_scope: scope,
        }
    }

    #[test]
    fn an_unmapped_folder_names_no_album_whatever_the_scope() {
        for scope in [AlbumScope::Leaf, AlbumScope::WholeFolder] {
            assert_eq!(
                album_of(
                    &folder(false, scope),
                    Path::new("/photos/Holiday/beach.jpg")
                ),
                None,
                "R3.4: album mapping is opt-in per folder"
            );
        }
    }

    #[test]
    fn the_leaf_scope_belongs_to_the_folder_that_holds_the_file() {
        let leaf = folder(true, AlbumScope::Leaf);
        assert_eq!(
            album_of(&leaf, Path::new("/photos/Holiday/beach.jpg")).map(|(name, _)| name),
            Some("Holiday".to_string())
        );
        assert_eq!(
            album_of(&leaf, Path::new("/photos/Holiday/beach.jpg")).map(|(_, key)| key),
            Some(path_key(Path::new("/photos/Holiday"))),
            "R3.4: the album belongs to the folder that holds the file"
        );
        // A file directly in the source folder maps to the source folder.
        assert_eq!(
            album_of(&leaf, Path::new("/photos/cover.jpg")).map(|(name, _)| name),
            Some("photos".to_string())
        );
    }

    #[test]
    fn the_whole_folder_scope_belongs_to_the_source_folder_for_every_file() {
        let whole = folder(true, AlbumScope::WholeFolder);
        for file in [
            "/photos/cover.jpg",
            "/photos/Holiday/beach.jpg",
            "/photos/Holiday/2024/deep.jpg",
        ] {
            assert_eq!(
                album_of(&whole, Path::new(file)),
                Some(("photos".to_string(), path_key(Path::new("/photos")))),
                "{file} maps to the source folder's album"
            );
        }
    }

    #[test]
    fn a_folder_without_a_last_component_still_names_its_album() {
        // A drive root has no name to copy, and an empty album name cannot be
        // found again.
        assert_eq!(folder_name(Path::new("/")), "/");
    }
}
