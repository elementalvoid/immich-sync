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
use sync_core::store::{FolderCounts, FolderTimes, SourceFolder, StateStore, StoreError};
use sync_core::tracking::Tracking;

use crate::paths::AppPaths;

/// What a fresh store hands the model: the database, the folders and decisions
/// over it, and the folder list with its counts.
struct OpenedStore {
    store: Arc<StateStore>,
    tracking: Arc<Tracking>,
    folders: Vec<SourceFolder>,
    counts: Vec<FolderCounts>,
    times: Vec<FolderTimes>,
}

/// The state database, once it opened, and the folders it holds.
pub struct StoreModel {
    /// The resolved paths, so a rebuild knows which file to replace.
    paths: AppPaths,
    store: Option<Arc<StateStore>>,
    /// The folders and review decisions, over the same store. It exists as
    /// soon as the store does, so a signed-out edit writes through the same
    /// module a running engine uses.
    tracking: Option<Arc<Tracking>>,
    folders: Vec<SourceFolder>,
    /// The last known counts per folder, in the same order as `folders`.
    counts: Vec<FolderCounts>,
    /// The last known scan and upload times per folder, in the same order as
    /// `folders` (#43).
    times: Vec<FolderTimes>,
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
                    let store = Arc::new(store);
                    let tracking = Arc::new(Tracking::new(Arc::clone(&store)));
                    // The folder list and its counts are read here, off the UI
                    // thread, so the first dashboard frame is populated (N2).
                    match store.folders() {
                        Ok(folders) => {
                            // A zeroed card is the fallback here: nothing is
                            // cached yet, so a failed read must still show the
                            // folders.
                            let counts = owned_counts(&store, &folders)
                                .unwrap_or_else(|| vec![FolderCounts::default(); folders.len()]);
                            let times = folders
                                .iter()
                                .map(|folder| store.folder_times(&folder.path).unwrap_or_default())
                                .collect();
                            Ok(OpenedStore {
                                store,
                                tracking,
                                folders,
                                counts,
                                times,
                            })
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
                    Ok(opened) => {
                        model.store = Some(opened.store);
                        model.tracking = Some(opened.tracking);
                        model.folders = opened.folders;
                        model.counts = opened.counts;
                        model.times = opened.times;
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
            tracking: None,
            folders: Vec::new(),
            counts: Vec::new(),
            times: Vec::new(),
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
        let previous_tracking = self.tracking.take();
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
            // Drop both handles first: on Windows an open database file cannot
            // be removed, and the rebuild must replace it.
            drop(previous);
            drop(previous_tracking);
            rebuild_store(&path, &folders)
        });
        cx.spawn(async move |this, cx| {
            let rebuilt = rebuilt.await;
            let _ = this.update(cx, |model, cx| {
                match rebuilt {
                    Ok(opened) => {
                        model.store = Some(opened.store);
                        model.tracking = Some(opened.tracking);
                        model.folders = opened.folders;
                        model.counts = opened.counts;
                        model.times = opened.times;
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

    /// The folders and review decisions, once the store opened.
    #[must_use]
    pub fn tracking(&self) -> Option<Arc<Tracking>> {
        self.tracking.clone()
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

    /// The last known scan and upload times per folder, in the same order as
    /// [`Self::folders`] (#43).
    #[must_use]
    pub fn folder_times(&self) -> &[FolderTimes] {
        &self.times
    }

    /// Replaces the cached folder list after an add or a remove, keeping each
    /// surviving folder's counts and times while the ownership read for the new
    /// list runs off the UI thread (N2, #52).
    pub fn set_folders(&mut self, folders: Vec<SourceFolder>, cx: &mut Context<Self>) {
        // R2.4: the cache must keep a folder's counts and times across a
        // re-add that spells the path differently, the same comparison the
        // tracking write made.
        let cached = |folder: &SourceFolder| {
            self.folders.iter().position(|old| {
                sync_core::paths::equal(
                    Path::new(&old.path),
                    Path::new(&folder.path),
                    sync_core::paths::PathCase::for_platform(),
                )
            })
        };
        let counts = folders
            .iter()
            .map(|folder| {
                cached(folder)
                    .and_then(|index| self.counts.get(index).copied())
                    .unwrap_or_default()
            })
            .collect();
        let times = folders
            .iter()
            .map(|folder| {
                cached(folder)
                    .and_then(|index| self.times.get(index).copied())
                    .unwrap_or_default()
            })
            .collect();
        // #52: only a changed path list can move a row between cards, so an
        // album, scope, or recursive edit keeps the counts it has. The raw
        // spelling is part of it: the store orders by that column, so a
        // re-spelled path is a change even when it names the same folder.
        let unchanged = folders.len() == self.folders.len()
            && folders
                .iter()
                .zip(&self.folders)
                .all(|(new, old)| new.path == old.path);
        self.folders = folders;
        self.counts = counts;
        self.times = times;
        cx.notify();
        if unchanged {
            return;
        }
        // #52: a folder's counts depend on the folders before it, so a nested
        // add or remove makes the kept values wrong: the outer card still holds
        // the new inner folder's rows, and a new outer folder shows none. The
        // ownership read is the one rule, and it runs off the UI thread (N2).
        let Some(store) = self.store.clone() else {
            return;
        };
        let ordered = self.folders.clone();
        let read_folders = ordered.clone();
        let read = cx
            .background_executor()
            .spawn(async move { owned_counts(&store, &read_folders) });
        cx.spawn(async move |this, cx| {
            let Some(counts) = read.await else {
                return;
            };
            let _ = this.update(cx, |model, cx| {
                // A later add or remove replaced the list and spawned its own
                // read, whose answer is the newer one. An edit that kept the
                // paths left every owner as it was, so this answer still
                // holds (#52).
                if !same_paths(&model.folders, &ordered) {
                    return;
                }
                model.counts = counts;
                cx.notify();
            });
        })
        .detach();
    }

    /// Re-reads the cached counts from the store after a review decision made
    /// with no engine running (R5.4), so the cache a later status refresh reads
    /// cannot resurrect the decided item.
    ///
    /// The decision changes one row, which one folder owns, so only that
    /// folder's card is replaced; its counts still come from the ownership read
    /// over the whole list (#52), because that is what says the row is its own.
    pub fn refresh_counts(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let case = sync_core::paths::PathCase::for_platform();
        let Some(folder) = self
            .folders
            .iter()
            .find(|folder| {
                sync_core::paths::is_within(
                    std::path::Path::new(&folder.path),
                    std::path::Path::new(path),
                    case,
                )
            })
            .map(|folder| folder.path.clone())
        else {
            return;
        };
        let ordered = self.folders.clone();
        let read_folders = ordered.clone();
        let read = cx
            .background_executor()
            .spawn(async move { owned_counts(&store, &read_folders) });
        cx.spawn(async move |this, cx| {
            let Some(counts) = read.await else {
                return;
            };
            let _ = this.update(cx, |model, cx| {
                // The folder list can change while the read runs: a list with
                // other paths has its own read, so only the paths this read
                // counted are safe to write. An edit that kept them did not
                // move the row (#52).
                if !same_paths(&model.folders, &ordered) {
                    return;
                }
                // The paths are unchanged, so their order is the read's order:
                // the folder that owns the path owns this entry.
                let Some(index) = model.folders.iter().position(|entry| entry.path == folder)
                else {
                    return;
                };
                if let Some(counts) = counts.get(index) {
                    model.counts[index] = *counts;
                    cx.notify();
                }
            });
        })
        .detach();
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

    /// The resolved paths, so the debug bundle collects the settings file, the
    /// logs, and the database the client actually uses (#25).
    #[must_use]
    pub fn paths(&self) -> &AppPaths {
        &self.paths
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
fn rebuild_store(path: &Path, folders: &[SourceFolder]) -> anyhow::Result<OpenedStore> {
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
    let store = Arc::new(store);
    let tracking = Arc::new(Tracking::new(Arc::clone(&store)));
    let folders = store.folders().context("read the restored folders")?;
    let counts = owned_counts(&store, &folders)
        .unwrap_or_else(|| vec![FolderCounts::default(); folders.len()]);
    let times = folders
        .iter()
        .map(|folder| store.folder_times(&folder.path).unwrap_or_default())
        .collect();
    Ok(OpenedStore {
        store,
        tracking,
        folders,
        counts,
        times,
    })
}

/// Whether the cache still holds the ordered raw paths a count read counted.
///
/// Only the ordered raw paths decide which card owns a row, so an album, scope,
/// or recursive edit inside the read window keeps the answer (#52).
fn same_paths(cache: &[SourceFolder], ordered: &[SourceFolder]) -> bool {
    cache
        .iter()
        .map(|folder| folder.path.as_str())
        .eq(ordered.iter().map(|folder| folder.path.as_str()))
}

/// The per-folder counts with the ownership rule (#52), or `None` when the
/// read fails: the caller keeps the counts it has instead of overwriting good
/// ones with zeroes.
fn owned_counts(store: &StateStore, folders: &[SourceFolder]) -> Option<Vec<FolderCounts>> {
    match store.folder_counts_by_owner(folders) {
        Ok(counts) => Some(counts),
        Err(error) => {
            tracing::warn!(error = %error, "the folder counts could not be read");
            None
        }
    }
}

/// Removes the database file and its WAL sidecars.
fn remove_state_files(path: &Path) -> anyhow::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        // `OsString::push` keeps a non-UTF-8 path intact; `display()` would
        // replace its bytes and target a different file.
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let file = PathBuf::from(name);
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
    use super::{OpenedStore, rebuild_store};
    use sync_core::store::{AlbumScope, SourceFolder, StoreError};

    fn folder(path: &str) -> SourceFolder {
        SourceFolder {
            path: path.to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
        }
    }

    /// R10.4: a database the user deleted while the client was stopped is the
    /// ordinary recovery, and the folders come back with it.
    #[test]
    fn a_rebuild_recreates_a_missing_database_with_the_folders() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("state.db");
        let folders = vec![folder("/photos")];

        let OpenedStore {
            store,
            folders: restored,
            counts,
            ..
        } = rebuild_store(&path, &folders).expect("the rebuild creates the database");

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

        let OpenedStore {
            store,
            folders: restored,
            ..
        } = rebuild_store(&path, &folders).expect("the rebuild replaces a corrupt database");

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

        let error = rebuild_store(&path, &[])
            .err()
            .expect("a directory is not a database");

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

    /// The WAL sidecars follow the path's own bytes, not a lossy display of
    /// them, so a non-UTF-8 database name removes the right files (Unix).
    #[cfg(unix)]
    #[test]
    fn sidecars_of_a_non_utf8_database_are_removed() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;
        use std::path::PathBuf;

        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join(OsStr::from_bytes(b"state-\xff.db"));
        std::fs::write(&path, b"database").expect("write the database");
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push("-wal");
        std::fs::write(PathBuf::from(&sidecar), b"wal").expect("write the sidecar");

        super::remove_state_files(&path).expect("remove the database and its sidecars");

        assert!(!path.exists(), "the database is gone");
        assert!(
            !PathBuf::from(sidecar).exists(),
            "the sidecar is gone, at the path its own bytes name"
        );
    }
}
