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
use sync_core::store::{FolderCounts, SourceFolder, StateStore};

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
        // The database holds the folder choices, including one added through a
        // running engine; the in-memory list is only the fallback for a store
        // that never opened (R2.1, R10.4).
        let folders = match self.store.as_ref() {
            Some(store) => store.folders().unwrap_or_else(|_| self.folders.clone()),
            None => self.folders.clone(),
        };
        let path = self.paths.state_db.clone();
        let previous = self.store.take();
        self.counts.clear();
        let rebuilt = cx.background_executor().spawn(async move {
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
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "the state database could not be opened, so the rebuild removes it"
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
