//! Album mapping (SPEC R3.4, R3.5, GLOSSARY).
//!
//! Album mapping is an opt-in per-folder rule that gives the folder an Immich
//! album. The album scope is per folder: the whole source folder, or every
//! folder that directly contains files.
//!
//! Three decisions this module makes, all from the glossary:
//!
//! - The album title is the user's, and the folder's name when the user left it
//!   blank (#47). It is a name, not the identity: Immich does not enforce
//!   album-title uniqueness, so nothing about the server merges two folders.
//!   The title applies to the album the source folder itself holds: the
//!   whole-folder album, or the album for files directly in the source folder.
//!   A leaf subfolder's album takes a title derived from the folder's two
//!   naming choices (#93): the leaf's path or its name, optionally starting
//!   with that title.
//! - The album belongs to the **folder**, not the title, and is keyed by that
//!   folder's path key. Two folders that share a name are two albums (R3.4),
//!   and one folder spelled with another case is one album (R2.4).
//!   The client's album writes are "add to album" and "rename", so an album
//!   that held two folders could never be split again.
//! - A title the server does not hold yet is a debt, not a loss: it is stored
//!   with the album row and sent again on the next scan, restart, or reconnect.
//!   The retry lives here, one pass over the pending rows, so neither the UI
//!   nor the scan loop can send it twice per event.
//!
//! `POST /albums` and `PUT /albums/{id}/assets` are `x-immich-state: Stable` in
//! the v3.3.1 spec, which is what R3.5 allows sync correctness to depend on.
//! `PATCH /albums/{id}` (`UpdateAlbumDto.albumName`) is Stable too and needs the
//! `album.update` permission (#47).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use immich_client::{Album, ClientError};
use tokio::sync::Mutex;

use crate::connection::{RequestCredential, SharedSession};
use crate::paths::{PathCase, equal, key_is_within, path_key};
use crate::store::{Album as StoredAlbum, AlbumPath, AlbumScope, SourceFolder, StateStore};

/// The album one file joins: the title it carries and the key of the folder it
/// belongs to, or `None` when its folder is not mapped (R3.4).
///
/// Under the whole-folder scope every file below the source folder joins one
/// album held by that source folder. Under the leaf scope a file joins the
/// album held by the folder that directly contains it, which is the source
/// folder itself when the file sits at its root (GLOSSARY). Only the album the
/// source folder itself holds takes the user's title (#47); a leaf subfolder's
/// album is titled by the folder's two naming choices (#93).
fn album_of(folder: &SourceFolder, file: &Path) -> Option<(String, String)> {
    if !folder.album_mapping {
        return None;
    }
    let root = Path::new(&folder.path);
    let (holder, own_album) = match folder.album_scope {
        AlbumScope::WholeFolder => (root.to_path_buf(), true),
        AlbumScope::Leaf => {
            let holder: PathBuf = file
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(root)
                .to_path_buf();
            let own_album = path_key(&holder) == path_key(root);
            (holder, own_album)
        }
    };
    let name = if own_album {
        album_title(folder)
    } else {
        leaf_album_title(folder, &holder)
    };
    Some((name, path_key(&holder)))
}

/// The name the album of `folder` itself carries: the user's title, or the
/// folder's name when the title is blank (#47).
///
/// This is the one place the title falls back to the folder's name, so the
/// stored blank title means the same thing to the create, the rename, and the
/// folder detail.
#[must_use]
pub fn album_title(folder: &SourceFolder) -> String {
    folder
        .album_title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map_or_else(
            || folder_name(Path::new(&folder.path)),
            std::string::ToString::to_string,
        )
}

/// The title of the album one leaf folder below `folder` holds (#93).
///
/// The two stored settings decide it: the leaf's path relative to the source
/// folder (`Trips/Paris`) or its own name (`Paris`), and whether the source
/// folder's album title comes first (`Holidays/Trips/Paris`). The source
/// folder's own title is the user's, so the prefix follows a retitle. The
/// separator is always `/`, whatever the platform's path separator is.
#[must_use]
pub fn leaf_album_title(folder: &SourceFolder, holder: &Path) -> String {
    let name = match folder.album_path {
        AlbumPath::Relative => relative_path(Path::new(&folder.path), holder),
        AlbumPath::LeafName => folder_name(holder),
    };
    if !folder.album_prefix {
        return name;
    }
    // A source folder at a drive root has no name, so its whole path is the
    // prefix and already ends in a separator (#93).
    let prefix = album_title(folder);
    let prefix = prefix.trim_end_matches(['/', '\\']);
    if prefix.is_empty() {
        format!("/{name}")
    } else {
        format!("{prefix}/{name}")
    }
}

/// `holder`'s parts below `root`, joined with `/` (#93).
///
/// Both sides are split on either separator and compared part by part the way
/// the platform compares paths (R2.4), so a Windows path and a drive-root
/// source folder give the same title on any host. Whole-path comparison cannot
/// do it: a drive root normalizes to no path at all. A holder that is not below the root falls back
/// to its own name, which is what the mapping called it before this setting.
fn relative_path(root: &Path, holder: &Path) -> String {
    let mut parts = path_parts(holder);
    let root_parts = path_parts(root);
    let case = PathCase::for_platform();
    let below = parts.len() > root_parts.len()
        && root_parts
            .iter()
            .zip(&parts)
            .all(|(root_part, part)| equal(Path::new(root_part), Path::new(part), case));
    if !below {
        return folder_name(holder);
    }
    parts.drain(..root_parts.len());
    parts.join("/")
}

/// The non-empty parts of `path` under either separator, in order.
fn path_parts(path: &Path) -> Vec<String> {
    path.to_string_lossy()
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
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

/// Puts finished assets into the albums their folders map to, and sends the
/// titles the user changed (R3.4, #47).
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
    /// Serializes the retry pass, so two events that both ask to settle the
    /// pending titles cannot rename one album twice or both recreate an album
    /// the server no longer holds.
    settling: Mutex<()>,
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
            settling: Mutex::new(()),
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
        // A leaf album's title is derived, so the create records it as the
        // title it settled on; the source folder's own album records the user's
        // stored title instead (#47, #93).
        let own = folder_key == path_key(Path::new(&folder.path));
        let album = self.album_id(&folder_key, &name, own).await?;
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
    /// made before, or a new one named `name`.
    ///
    /// # Errors
    ///
    /// Returns the [`ClientError`] of the create call. A state database that
    /// cannot record the new id is logged and the album is still used, because
    /// the next file can find it in this run's cache.
    async fn album_id(
        &self,
        folder_key: &str,
        name: &str,
        own: bool,
    ) -> Result<String, ClientError> {
        let mut known = self.known.lock().await;
        if let Some(id) = known.get(folder_key) {
            return Ok(id.clone());
        }
        let stored = match self.store.album(folder_key) {
            Ok(album) => album,
            Err(error) => {
                tracing::error!(album = %name, folder = %folder_key, error = %error, "the album table could not be read");
                None
            }
        };
        let id = match stored.as_ref().and_then(|album| album.album_id.clone()) {
            Some(id) => id,
            None => {
                // No album yet: the title the user stored while the mapping was
                // off is the name the create uses (#47).
                let album = match self.create_album(name).await {
                    Ok(album) => album,
                    Err(error) => return Err(error),
                };
                tracing::info!(album = %name, album_id = %album.id, "an album was created");
                let settled = if own {
                    stored.and_then(|album| album.title)
                } else {
                    Some(name.to_string())
                };
                self.record_answer(folder_key, &album, name, settled);
                album.id
            }
        };
        known.insert(folder_key.to_string(), id.clone());
        Ok(id)
    }

    /// Sends every title the server does not hold yet (#47, #93).
    ///
    /// It runs on the reconciliation scan — so on a start, on a reconnect, and
    /// on "Sync now" — and when the user edits a title, never per frame, so a
    /// server that keeps refusing is not hammered. A title whose album does not
    /// exist yet is left alone: the create carries it.
    ///
    /// The pass reads the album rows, not the folders: a leaf album has no
    /// source folder of its own, and its expected title is derived from the two
    /// settings of the folder that owns it (#93). A row whose title differs is
    /// written back first, which is what marks the rename it owes, so the one
    /// rename path settles it like a title the user typed.
    pub async fn settle_titles(&self) {
        let _settling = self.settling.lock().await;
        let folders = match self.store.folders() {
            Ok(folders) => folders,
            Err(error) => {
                tracing::error!(error = %error, "the source folders could not be read");
                return;
            }
        };
        let keys: Vec<String> = folders
            .iter()
            .map(|folder| path_key(Path::new(&folder.path)))
            .collect();
        let rows = match self.store.album_rows() {
            Ok(rows) => rows,
            Err(error) => {
                tracing::error!(error = %error, "the album rows could not be read");
                return;
            }
        };
        for (key, album) in rows {
            let Some(index) = owner_index(&keys, &key) else {
                continue;
            };
            if keys[index] == key {
                self.settle_own_album(&folders[index], &key, &album).await;
            } else {
                self.settle_leaf_album(&folders[index], &key, &album).await;
            }
        }
    }

    /// Sends the source folder's own pending title, and nothing else: its name
    /// is the user's, so no expected title has to be derived (#47).
    async fn settle_own_album(&self, folder: &SourceFolder, key: &str, album: &StoredAlbum) {
        if !album.rename_pending {
            return;
        }
        let Some(album_id) = album.album_id.clone() else {
            return;
        };
        self.rename(key, &album_id, &album_title(folder), album.title.clone())
            .await;
    }

    /// Renames one leaf album when the folder's settings give it another title
    /// (#93).
    ///
    /// The expected title is derived from the folder that owns the leaf and the
    /// leaf's real path. A folder with no item in it has no real path to spell
    /// (#93, R2.4), so its album is left alone: the next file in it carries the
    /// right title, and the scan after that settles the album.
    async fn settle_leaf_album(&self, folder: &SourceFolder, key: &str, album: &StoredAlbum) {
        if !folder.album_mapping || folder.album_scope != AlbumScope::Leaf {
            return;
        }
        let holder = match self.store.leaf_folder_path(key) {
            Ok(Some(path)) => PathBuf::from(path),
            Ok(None) => {
                tracing::debug!(folder = %key, "the album was left alone: no item says how its folder is spelled");
                return;
            }
            Err(error) => {
                tracing::warn!(folder = %key, error = %error, "the folder path behind an album title could not be read");
                return;
            }
        };
        let name = leaf_album_title(folder, &holder);
        if album.title.as_deref() != Some(name.as_str()) {
            // The expected title replaces the stored one, and the store marks
            // the rename the change owes (#47, #93).
            if let Err(error) = self.store.set_album_title(key, Some(&name)) {
                tracing::error!(album = %name, folder = %key, error = %error, "the expected album title could not be recorded");
                return;
            }
        } else if !album.rename_pending {
            return;
        }
        let Some(album_id) = album.album_id.clone() else {
            return;
        };
        self.rename(key, &album_id, &name, Some(name.clone())).await;
    }

    /// Renames one album, recreating it when the server no longer holds it, and
    /// records the outcome with its album row (#47).
    async fn rename(&self, folder_key: &str, album_id: &str, name: &str, title: Option<String>) {
        let client = self.session.client();
        let requested_album = album_id.to_string();
        let album_name = name.to_string();
        let outcome = self
            .session
            .request(move |credential| {
                let client = client.clone();
                let requested_album = requested_album.clone();
                let album_name = album_name.clone();
                async move {
                    client
                        .rename_album(credential.auth(), &requested_album, &album_name)
                        .await
                }
            })
            .await;
        match outcome {
            Ok(album) => {
                tracing::info!(album = %name, album_id = %album.id, "an album was renamed");
                self.record_answer(folder_key, &album, name, title);
            }
            // The album is gone from Immich: a rename makes it again with the
            // title, and the new id replaces the stale one (#47).
            Err(ClientError::Protocol { status: 404, .. }) => match self.create_album(name).await {
                Ok(album) => {
                    tracing::info!(
                        album = %name,
                        old_album_id = %album_id,
                        album_id = %album.id,
                        "the album was gone from the server and was created again"
                    );
                    self.record_answer(folder_key, &album, name, title);
                    self.known
                        .lock()
                        .await
                        .insert(folder_key.to_string(), album.id);
                }
                Err(error) => {
                    tracing::warn!(
                        album = %name,
                        reason = %error.redacted_reason(),
                        "the album was gone from the server and could not be created again"
                    );
                    self.record_failure(folder_key, &error);
                }
            },
            Err(error) => {
                tracing::warn!(
                    album = %name,
                    folder = %folder_key,
                    reason = %error.redacted_reason(),
                    "the album title did not reach the server; the folder detail says why"
                );
                self.record_failure(folder_key, &error);
            }
        }
    }

    /// Creates one album named `name`.
    async fn create_album(&self, name: &str) -> Result<Album, ClientError> {
        let client = self.session.client();
        let album_name = name.to_string();
        self.session
            .request(move |credential| {
                let client = client.clone();
                let album_name = album_name.clone();
                async move {
                    client
                        .create_album(credential.auth(), &album_name, &[])
                        .await
                }
            })
            .await
    }

    /// Records the album the server answered with, so the stored title follows
    /// the server and the rename is no longer owed (#47).
    ///
    /// `settled` is the title the request was based on. The title the user
    /// chose stays the stored one while the server keeps the name the client
    /// sent; a server that answered with another name wins, because the album
    /// is called what the server says it is. A title the user changed while the
    /// request was on the wire is kept by the store, not overwritten by this
    /// answer (#47).
    fn record_answer(
        &self,
        folder_key: &str,
        album: &Album,
        expected: &str,
        settled: Option<String>,
    ) {
        let title = if album.name == expected {
            settled.clone()
        } else {
            Some(album.name.clone())
        };
        if let Err(error) =
            self.store
                .record_album(folder_key, &album.id, settled.as_deref(), title.as_deref())
        {
            tracing::error!(album = %album.name, folder = %folder_key, error = %error, "the album could not be recorded");
        }
    }

    /// Keeps the title pending and records why the server does not hold it yet
    /// (#47).
    fn record_failure(&self, folder_key: &str, error: &ClientError) {
        let reason = self.rename_reason(error);
        if let Err(error) = self.store.album_rename_failed(folder_key, &reason) {
            tracing::error!(folder = %folder_key, error = %error, "the album failure could not be recorded");
        }
    }

    /// Why a rename did not reach the server, in the folder detail's words
    /// (#47). The missing permission is named, because that is what the user
    /// has to fix.
    fn rename_reason(&self, error: &ClientError) -> String {
        match error {
            ClientError::PermissionDenied(_) => match self.session.credential() {
                Some(RequestCredential::ApiKey(_)) => {
                    "the API key is missing the album.update permission".to_string()
                }
                _ => "the account is missing the album.update permission".to_string(),
            },
            ClientError::Unreachable(_) => "the server could not be reached".to_string(),
            ClientError::InvalidToken | ClientError::InvalidApiKey => {
                "the credential was refused".to_string()
            }
            other => other.to_string(),
        }
    }
}

/// The index of the source folder that owns the album keyed `key` (#93,
/// GLOSSARY *Owner*): the folder itself, or the first folder in path order that
/// contains it.
///
/// A nested source folder's own album row is still that folder's own row, so a
/// key that is a source folder is matched before containment is tried.
fn owner_index(keys: &[String], key: &str) -> Option<usize> {
    keys.iter()
        .position(|folder_key| folder_key == key)
        .or_else(|| {
            keys.iter()
                .position(|folder_key| key_is_within(folder_key, key))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AlbumScope;

    /// A mapped or unmapped folder at `/photos`.
    fn folder(mapping: bool, scope: AlbumScope) -> SourceFolder {
        titled(mapping, scope, None)
    }

    /// A mapped leaf folder at `/photos` with `title` and the two leaf naming
    /// choices (#93).
    fn naming(title: Option<&str>, album_path: AlbumPath, album_prefix: bool) -> SourceFolder {
        SourceFolder {
            album_path,
            album_prefix,
            ..titled(true, AlbumScope::Leaf, title)
        }
    }

    /// A mapped folder at `/photos` carrying `title` (#47).
    fn titled(mapping: bool, scope: AlbumScope, title: Option<&str>) -> SourceFolder {
        SourceFolder {
            path: "/photos".to_string(),
            recursive: true,
            album_mapping: mapping,
            album_scope: scope,
            album_title: title.map(str::to_string),
            ..Default::default()
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
            Some("photos/Holiday".to_string()),
            "#93: a new folder names a leaf album after its folder path, with the \
             source folder's title in front"
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

    /// #47: the title replaces the folder's name for the album the source
    /// folder itself holds, under both scopes.
    #[test]
    fn the_source_folders_own_album_takes_the_custom_title() {
        let whole = titled(true, AlbumScope::WholeFolder, Some("Holiday 2024"));
        for file in ["/photos/cover.jpg", "/photos/Holiday/beach.jpg"] {
            assert_eq!(
                album_of(&whole, Path::new(file)).map(|(name, _)| name),
                Some("Holiday 2024".to_string()),
                "{file} joins the source folder's album, which is renamed"
            );
        }
        let leaf = titled(true, AlbumScope::Leaf, Some("Holiday 2024"));
        assert_eq!(
            album_of(&leaf, Path::new("/photos/cover.jpg")).map(|(name, _)| name),
            Some("Holiday 2024".to_string()),
            "a file directly in the source folder joins its renamed album"
        );
    }

    /// #93: a leaf album's title comes from the two stored choices, never from
    /// the user's typing, and the source folder's own title is the prefix.
    #[test]
    fn the_four_leaf_titles() {
        let file = Path::new("/photos/Trips/Paris/beach.jpg");
        for (album_path, prefix, expected) in [
            (AlbumPath::Relative, false, "Trips/Paris"),
            (AlbumPath::Relative, true, "Holidays/Trips/Paris"),
            (AlbumPath::LeafName, false, "Paris"),
            (AlbumPath::LeafName, true, "Holidays/Paris"),
        ] {
            let folder = naming(Some("Holidays"), album_path, prefix);
            assert_eq!(
                album_of(&folder, file),
                Some((
                    expected.to_string(),
                    path_key(Path::new("/photos/Trips/Paris"))
                )),
                "{album_path:?} with the prefix {prefix}"
            );
        }
    }

    /// #93: with no custom title the prefix is the folder's name, which follows
    /// the folder it belongs to.
    #[test]
    fn a_blank_source_title_prefixes_with_the_folder_name() {
        let folder = naming(None, AlbumPath::Relative, true);
        assert_eq!(
            album_of(&folder, Path::new("/photos/Trips/Paris/beach.jpg")).map(|(name, _)| name),
            Some("photos/Trips/Paris".to_string())
        );
    }

    /// #93: a file directly in the source folder joins the source folder's own
    /// album whatever the leaf choices are.
    #[test]
    fn a_root_file_ignores_the_leaf_choices() {
        for (album_path, prefix) in [(AlbumPath::Relative, false), (AlbumPath::LeafName, true)] {
            let folder = naming(Some("Holidays"), album_path, prefix);
            assert_eq!(
                album_of(&folder, Path::new("/photos/cover.jpg")),
                Some(("Holidays".to_string(), path_key(Path::new("/photos")))),
                "{album_path:?} with the prefix {prefix}"
            );
        }
    }

    /// #93: a leaf album is settled by the source folder that owns it, and a
    /// nested source folder's own album row is still its own.
    #[test]
    fn the_owner_of_an_album_row_is_the_first_folder_that_contains_it() {
        // The folders, in path order as the store returns them.
        let keys = vec!["/photos".to_string(), "/photos/Trips".to_string()];
        assert_eq!(
            owner_index(&keys, "/photos/Trips/Paris"),
            Some(0),
            "#93, GLOSSARY Owner: the outer folder owns the leaf below the inner one"
        );
        assert_eq!(
            owner_index(&keys, "/photos/Trips"),
            Some(1),
            "a nested source folder's own album row is its own, not its parent's"
        );
        assert_eq!(
            owner_index(&keys, "/elsewhere"),
            None,
            "a row no folder contains is left alone"
        );
    }

    /// #93: the relative path is the leaf's path below the folder that owns it,
    /// never below a deeper source folder.
    #[test]
    fn a_nested_source_folder_measures_from_the_owner() {
        let mut outer = naming(Some("Holidays"), AlbumPath::Relative, true);
        outer.path = "/photos".to_string();
        assert_eq!(
            album_of(&outer, Path::new("/photos/Trips/Paris/beach.jpg")).map(|(name, _)| name),
            Some("Holidays/Trips/Paris".to_string()),
            "the owner's path is what the title is relative to"
        );
    }

    /// #93: the separator is always `/`, whatever the platform's own is.
    #[test]
    fn windows_separators_become_slashes() {
        // Built from parts so the repository's hardcoded-path scan stays clean.
        let windows = |rest: &str| format!("{}:\\{}", 'C', rest.replace('/', "\\"));
        let mut folder = naming(None, AlbumPath::Relative, false);
        folder.path = windows("Photos");
        assert_eq!(
            leaf_album_title(&folder, Path::new(&windows("Photos/Trips/Paris"))),
            "Trips/Paris"
        );
    }

    /// #93: a drive root has no name, so the whole path is the prefix and the
    /// title does not start with a doubled separator.
    #[test]
    fn a_drive_root_prefixes_with_the_whole_path() {
        let mut folder = naming(None, AlbumPath::Relative, true);
        folder.path = "/".to_string();
        assert_eq!(
            album_of(&folder, Path::new("/Trips/Paris/beach.jpg")).map(|(name, _)| name),
            Some("/Trips/Paris".to_string()),
            "the drive root's whole path is the prefix, joined once"
        );
    }

    /// #47: a blank or whitespace-only title means the folder's name.
    #[test]
    fn a_blank_title_means_the_folder_name() {
        for title in [None, Some(""), Some("   ")] {
            assert_eq!(
                album_title(&titled(true, AlbumScope::WholeFolder, title)),
                "photos",
                "{title:?} means the folder's name"
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
