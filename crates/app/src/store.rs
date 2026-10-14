//! The app-scoped state store handle (SPEC N2, N4).
//!
//! Opening the database runs the startup `quick_check` and the migrations, so
//! it must not run on the UI thread. This model opens it on the GPUI background
//! executor and hands the ready store to the engine and to the manual upload
//! view. Both then share the one writer connection (ADR 0005).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use gpui_kit::*;
use sync_core::store::{FolderCounts, SourceFolder, StateStore, StoreError};

use crate::paths::AppPaths;

/// The state database, once it opened, and the folders it holds.
pub struct StoreModel {
    /// The resolved paths, so a rebuild knows which file to replace.
    paths: AppPaths,
    store: Option<Arc<StateStore>>,
    folders: Vec<SourceFolder>,
    /// The last known counts per folder, in the same order as `folders`.
    counts: Vec<FolderCounts>,
    problem: Option<String>,
    /// Whether the first open is still running: until it lands, it owns the
    /// store and a rebuild must not touch it.
    opening: bool,
    /// Bumped whenever the store is replaced, so a model that must restart
    /// over the new one (the engine) can see the swap.
    generation: u64,
}

impl StoreModel {
    /// Starts opening the database on the background executor, never on the
    /// UI thread (N2).
    ///
    /// The views show the problem until the store arrives, so a database that
    /// cannot be opened does not stop the window (N4).
    pub fn open(paths: &AppPaths, cx: &mut Context<Self>) -> Self {
        let path = paths.state_db.clone();
        let opened = cx.background_executor().spawn(async move {
            match StateStore::open(&path) {
                Ok(store) => {
                    // The folder list and its counts are read here, off the UI
                    // thread, so the first dashboard frame is populated (N2).
                    match store.folders() {
                        Ok(folders) => {
                            let counts = folders
                                .iter()
                                .map(|folder| store.folder_counts(&folder.path).unwrap_or_default())
                                .collect();
                            Ok((Arc::new(store), folders, counts))
                        }
                        Err(error) => Err(error.to_string()),
                    }
                }
                Err(error) => Err(error.to_string()),
            }
        });
        cx.spawn(async move |this, cx| {
            let opened = opened.await;
            let _ = this.update(cx, |model, cx| {
                model.opening = false;
                match opened {
                    Ok((store, folders, counts)) => {
                        model.store = Some(store);
                        model.folders = folders;
                        model.counts = counts;
                        model.problem = None;
                        model.generation = model.generation.wrapping_add(1);
                    }
                    Err(problem) => model.problem = Some(problem),
                }
                cx.notify();
            });
        })
        .detach();
        Self {
            paths: paths.clone(),
            store: None,
            folders: Vec::new(),
            counts: Vec::new(),
            problem: None,
            opening: true,
            generation: 0,
        }
    }

    /// Rebuilds the state database off the UI thread (R10.4): the tables start
    /// over, the folders the client knows are re-added, and the store is
    /// replaced so a deleted or damaged file is recreated.
    ///
    /// The store is reopened rather than reset in place, because a handle to a
    /// file removed by hand writes to nothing: only a fresh open at the same
    /// path brings the database back.
    pub fn rebuild(&mut self, cx: &mut Context<Self>) {
        // The first open owns the store until it lands: a rebuild now would
        // reset an empty database and wipe the folders it is about to load
        // (R10.4). The problem clears when the open completes.
        if !self.settled() {
            self.problem = Some("The state database is still opening.".to_string());
            cx.notify();
            return;
        }
        let path = self.paths.state_db.clone();
        let previous = self.store.take();
        let fallback = self.folders.clone();
        self.counts.clear();
        let rebuilt = cx.background_executor().spawn(async move {
            // The folder choices live in the database, and the read runs here,
            // off the UI thread: the store's lock can be held by an engine
            // write (N2, R2.1, R10.4).
            let folders = previous
                .as_ref()
                .and_then(|store| store.folders().ok())
                .unwrap_or(fallback);
            // Drop this handle first: on Windows an open database file cannot
            // be removed, and the rebuild must replace it.
            drop(previous);
            rebuild_store(&path, &folders)
        });
        cx.spawn(async move |this, cx| {
            let rebuilt = rebuilt.await;
            let _ = this.update(cx, |model, cx| {
                match rebuilt {
                    Ok((store, folders, counts)) => {
                        model.store = Some(store);
                        model.folders = folders;
                        model.counts = counts;
                        model.problem = None;
                    }
                    Err(error) => model.problem = Some(format!("{error:#}")),
                }
                // Both outcomes replace the store the engine was watching (or
                // leave it without one), so the generation moves either way and
                // nothing keeps waiting for a store that will not arrive.
                model.generation = model.generation.wrapping_add(1);
                cx.notify();
            });
        })
        .detach();
    }

    /// The open store, or `None` while it is still opening.
    #[must_use]
    pub fn store(&self) -> Option<Arc<StateStore>> {
        self.store.clone()
    }

    /// Whether the first open has finished, successfully or not.
    #[must_use]
    pub fn settled(&self) -> bool {
        !self.opening
    }

    /// The source folders the database holds (R2.1).
    #[must_use]
    pub fn folders(&self) -> &[SourceFolder] {
        &self.folders
    }

    /// The last known counts per folder, in the same order as [`Self::folders`]
    /// (R8.1).
    #[must_use]
    pub fn folder_counts(&self) -> &[FolderCounts] {
        &self.counts
    }

    /// Replaces the cached folder list after an add or a remove, keeping the
    /// counts of the folders that are still there.
    pub fn set_folders(&mut self, folders: Vec<SourceFolder>, cx: &mut Context<Self>) {
        let counts = folders
            .iter()
            .map(|folder| {
                self.folders
                    .iter()
                    .position(|old| old.path == folder.path)
                    .and_then(|index| self.counts.get(index).copied())
                    .unwrap_or_default()
            })
            .collect();
        self.folders = folders;
        self.counts = counts;
        cx.notify();
    }

    /// The problem that stopped the store, if any.
    #[must_use]
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// The state database's path (R10.2).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.paths.state_db
    }

    /// The store's generation: it changes when the store is replaced.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Reopens `path`, starts the state over, and re-adds the folders the user
/// chose (R10.4).
///
/// A file the integrity check refuses cannot be migrated, so the rebuild is the
/// one path that removes it and opens a fresh database.
fn rebuild_store(
    path: &Path,
    folders: &[SourceFolder],
) -> anyhow::Result<(Arc<StateStore>, Vec<SourceFolder>, Vec<FolderCounts>)> {
    let store = match StateStore::open(path) {
        Ok(store) => store,
        // Only a real corruption verdict justifies removing the file: a
        // transient lock or a path SQLite cannot open must report its own
        // reason, or the rebuild deletes a healthy database (N4, R10.4).
        Err(error) if !matches!(error, StoreError::QuickCheck { .. }) => {
            return Err(anyhow::Error::new(error).context(format!(
                "open the state database {} to rebuild it",
                path.display()
            )));
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "the state database failed its integrity check, so the rebuild removes it"
            );
            remove_state_files(path)?;
            StateStore::open(path)
                .with_context(|| format!("open the rebuilt state database {}", path.display()))?
        }
    };
    store.reset().context("start the state tables over")?;
    for folder in folders {
        store
            .add_folder(folder)
            .context("restore a source folder")?;
    }
    let folders = store.folders().context("read the restored folders")?;
    let counts = folders
        .iter()
        .map(|folder| store.folder_counts(&folder.path).unwrap_or_default())
        .collect();
    Ok((Arc::new(store), folders, counts))
}

/// Removes the database file and its WAL sidecars.
fn remove_state_files(path: &Path) -> anyhow::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let file = PathBuf::from(format!("{}{suffix}", path.display()));
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "remove {}; close any other copy of the client and try again",
                        file.display()
                    )
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::rebuild_store;
    use sync_core::store::{AlbumScope, SourceFolder, StoreError};

    fn folder(path: &str) -> SourceFolder {
        SourceFolder {
            path: path.to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
        }
    }

    /// R10.4: a database the user deleted while the client was stopped is the
    /// ordinary recovery, and the folders come back with it.
    #[test]
    fn a_rebuild_recreates_a_missing_database_with_the_folders() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("state.db");
        let folders = vec![folder("/photos")];

        let (store, restored, counts) =
            rebuild_store(&path, &folders).expect("the rebuild creates the database");

        assert_eq!(restored, folders, "the folder choices come back");
        assert_eq!(counts.len(), 1, "the restored folder is counted");
        assert_eq!(
            store
                .sqlite_info()
                .expect("read the rebuilt state")
                .integrity,
            "ok"
        );
    }

    /// R10.4, N4: a corruption verdict is the one open failure that justifies
    /// removing the file.
    #[test]
    fn a_rebuild_replaces_a_corrupt_database() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("state.db");
        std::fs::write(&path, b"this is not a SQLite database").expect("write garbage");
        let folders = vec![folder("/photos")];

        let (store, restored, _) =
            rebuild_store(&path, &folders).expect("the rebuild replaces a corrupt database");

        assert_eq!(
            restored, folders,
            "the folders are restored into the fresh file"
        );
        store
            .quick_check()
            .expect("the replacement database is sound");
    }

    /// N4: an open failure that is not corruption reports its own reason and
    /// leaves whatever is at the path alone.
    #[test]
    fn a_rebuild_leaves_an_unopenable_path_alone() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("state.db");
        std::fs::create_dir_all(&path).expect("put a directory where the database goes");

        let error = rebuild_store(&path, &[]).expect_err("a directory is not a database");

        assert!(
            matches!(
                error.downcast_ref::<StoreError>(),
                Some(StoreError::Open { .. })
            ),
            "the open failure is reported as itself, not as a remove failure: {error:#}"
        );
        assert!(
            path.is_dir(),
            "the rebuild must not remove what it cannot open"
        );
    }
}
