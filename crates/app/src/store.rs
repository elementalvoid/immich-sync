//! The app-scoped state store handle (SPEC N2, N4).
//!
//! Opening the database runs the startup `quick_check` and the migrations, so
//! it must not run on the UI thread. This model opens it on the GPUI background
//! executor and hands the ready store to the engine and to the manual upload
//! view. Both then share the one writer connection (ADR 0005).

use std::sync::Arc;

use gpui_kit::*;
use sync_core::store::{FolderCounts, SourceFolder, StateStore};

use crate::paths::AppPaths;

/// The state database, once it opened, and the folders it holds.
pub struct StoreModel {
    store: Option<Arc<StateStore>>,
    folders: Vec<SourceFolder>,
    /// The last known counts per folder, in the same order as `folders`.
    counts: Vec<FolderCounts>,
    problem: Option<String>,
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
                    }
                    Err(problem) => model.problem = Some(problem),
                }
                cx.notify();
            });
        })
        .detach();
        Self {
            store: None,
            folders: Vec::new(),
            counts: Vec::new(),
            problem: None,
        }
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
}
