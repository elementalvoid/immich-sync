//! The source folders the client tracks and the decisions the user makes
//! about their items (SPEC R2.1, R2.4, R3.4, R5.3, R6.5).
//!
//! Every write to a source folder row goes through here, so the rules that
//! keep a row whole exist once: the case-folded path key, the album fields a
//! re-add must carry, and the answer for a path the client does not hold. The
//! sync engine attaches an [`Observer`] and reacts to the same changes, which
//! is what lets the shell call one interface whether or not the engine runs.
//!
//! A change is written first and announced after its lock is released, so an
//! observer never waits on a lock its own thread holds. An observer that writes
//! on every notification makes a loop of its own; one that reacts to a
//! particular change is fine.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::paths::path_key;
use crate::store::{
    AlbumScope, FailedItem, ReviewDecision, ReviewItem, SourceFolder, StateStore, StoreError,
};

/// What a [`Tracking`] change does to a running engine.
///
/// An observer runs after the write has landed and after the write lock is
/// released, so it may read the store or call back in. Two changes that race
/// are settled by the later callback reading the later state, not by the order
/// the notifications arrive in.
///
/// The engine is one adapter. The other is no observer at all, and it is not
/// hypothetical: the shell edits folders while the client is signed out.
pub trait Observer: Send + Sync {
    /// A folder was added or re-added, so it may need a watch and a scan.
    fn folder_added(&self, _folder: &SourceFolder) {}
    /// A folder's own fields changed: the album mapping or its scope.
    fn folder_changed(&self, _folder: &SourceFolder) {}
    /// A folder was removed from the client.
    fn folder_removed(&self, _path: &str) {}
    /// One item left the deletion review by a decision (R5.3).
    fn review_decided(&self, _path: &str) {}
    /// One parked item was queued again for a manual retry (R6.5).
    fn item_requeued(&self, _path: &str) {}
}

/// Why a folder change was refused.
#[derive(Debug, thiserror::Error)]
pub enum TrackingError {
    /// The state database refused the change.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The folder path was blank.
    #[error("the source folder path is empty")]
    EmptyFolder,
    /// The path is not one of this client's source folders.
    #[error("{0} is not a source folder of this client")]
    UnknownFolder(String),
}

/// The source folders and the user's decisions over their items.
pub struct Tracking {
    store: Arc<StateStore>,
    /// A list, not one slot: a start that loses its race is stopped but may
    /// still be alive, and detaching it must not unhook the winner.
    observers: Mutex<Vec<Arc<dyn Observer>>>,
    /// Serializes a read-modify-write of one folder row, so an album edit
    /// cannot slip between an add's lookup and its upsert. It is released
    /// before the observers run.
    writes: Mutex<()>,
}

/// The stored source folder a path names, compared by path key (R2.4).
///
/// One lookup for every caller that needs the row a notification is about,
/// so the case rule lives here and nowhere else.
pub(crate) fn stored_folder(
    store: &StateStore,
    path: &str,
) -> Result<Option<SourceFolder>, StoreError> {
    let key = path_key(Path::new(path));
    Ok(store
        .folders()?
        .into_iter()
        .find(|folder| path_key(Path::new(&folder.path)) == key))
}

impl Tracking {
    /// Wraps the open state store.
    #[must_use]
    pub fn new(store: Arc<StateStore>) -> Self {
        Self {
            store,
            observers: Mutex::new(Vec::new()),
            writes: Mutex::new(()),
        }
    }

    /// The store every change is written to.
    #[must_use]
    pub fn store(&self) -> &Arc<StateStore> {
        &self.store
    }

    /// Starts reporting changes to `observer`, until [`Self::detach`].
    pub fn attach(&self, observer: Arc<dyn Observer>) {
        self.observers
            .lock()
            .expect("the observer list")
            .push(observer);
    }

    /// Stops reporting changes to `observer`, and only to it.
    pub fn detach(&self, observer: &Arc<dyn Observer>) {
        self.observers
            .lock()
            .expect("the observer list")
            .retain(|held| !Arc::ptr_eq(held, observer));
    }

    /// The source folders the client tracks (R2.1).
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] when the query fails.
    pub fn folders(&self) -> Result<Vec<SourceFolder>, StoreError> {
        self.store.folders()
    }

    /// Adds a source folder, or edits the recursive flag of one it already
    /// holds (R2.1).
    ///
    /// A re-add keeps the album settings the folder already has: the store's
    /// upsert writes every column, so a new row that started unmapped would
    /// silently lose the mapping. The lookup compares path keys (R2.4), so a
    /// path spelled differently still edits the row it already has.
    ///
    /// # Errors
    ///
    /// Returns [`TrackingError::EmptyFolder`] for a blank path, and a
    /// [`StoreError`] when the write fails.
    pub fn add_folder(&self, path: &str, recursive: bool) -> Result<SourceFolder, TrackingError> {
        let path = path.trim();
        if path.is_empty() {
            return Err(TrackingError::EmptyFolder);
        }
        let folder = {
            let _write = self.writes.lock().expect("the folder write lock");
            let existing = stored_folder(&self.store, path)?;
            let folder = SourceFolder {
                path: path.to_string(),
                recursive,
                album_mapping: existing.as_ref().is_some_and(|folder| folder.album_mapping),
                album_scope: existing.map_or_else(AlbumScope::default, |folder| folder.album_scope),
            };
            self.store.add_folder(&folder)?;
            folder
        };
        tracing::info!(folder = %folder.path, recursive, "a source folder was added");
        self.notify(|observer| observer.folder_added(&folder));
        Ok(folder)
    }

    /// Removes a source folder; its items stay in the store (R2.1).
    ///
    /// Returns whether the client held the folder.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] when the write fails.
    pub fn remove_folder(&self, path: &str) -> Result<bool, TrackingError> {
        let removed = {
            let _write = self.writes.lock().expect("the folder write lock");
            self.store.remove_folder(path)?
        };
        if removed {
            tracing::info!(folder = path, "a source folder was removed");
            self.notify(|observer| observer.folder_removed(path));
        }
        Ok(removed)
    }

    /// Turns album mapping on or off for one source folder (R3.4).
    ///
    /// # Errors
    ///
    /// Returns [`TrackingError::UnknownFolder`] for a path this client does
    /// not hold, and a [`StoreError`] when the write fails.
    pub fn set_album_mapping(
        &self,
        path: &str,
        enabled: bool,
    ) -> Result<SourceFolder, TrackingError> {
        self.update_folder(path, |folder| folder.album_mapping = enabled)
    }

    /// Sets the album scope for one source folder (R3.4).
    ///
    /// # Errors
    ///
    /// The same as [`Self::set_album_mapping`].
    pub fn set_album_scope(
        &self,
        path: &str,
        scope: AlbumScope,
    ) -> Result<SourceFolder, TrackingError> {
        self.update_folder(path, |folder| folder.album_scope = scope)
    }

    /// Keeps the server asset for one missing file and clears it from the
    /// review (R5.3). Nothing is sent: the server already holds the asset.
    ///
    /// Returns whether an item was waiting for the decision.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] when the write fails.
    pub fn keep_review(&self, path: &str) -> Result<bool, StoreError> {
        let kept = self.store.decide_review(path, ReviewDecision::Keep)?;
        if kept {
            tracing::info!(path, "R5.3: the server asset was kept");
            self.notify(|observer| observer.review_decided(path));
        }
        Ok(kept)
    }

    /// Puts one parked item back in the queue for a manual retry (R6.5).
    ///
    /// Returns whether the store held a parked item for the path.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] when the write fails.
    pub fn retry_failed(&self, path: &str) -> Result<bool, StoreError> {
        let queued = self.store.retry_item(path)?;
        if queued {
            tracing::info!(path, "a parked item was queued for a manual retry");
            self.notify(|observer| observer.item_requeued(path));
        }
        Ok(queued)
    }

    /// The parked items the failed-items view lists (R6.5).
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] when the query fails.
    pub fn failed_items(&self) -> Result<Vec<FailedItem>, StoreError> {
        self.store.failed_items()
    }

    /// The missing files that wait for a decision (R5.3).
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] when the query fails.
    pub fn review_items(&self) -> Result<Vec<ReviewItem>, StoreError> {
        self.store.review_items()
    }

    /// Writes one changed field of a source folder back to the store.
    fn update_folder(
        &self,
        path: &str,
        change: impl FnOnce(&mut SourceFolder),
    ) -> Result<SourceFolder, TrackingError> {
        let folder = {
            let _write = self.writes.lock().expect("the folder write lock");
            let mut folder = stored_folder(&self.store, path)?
                .ok_or_else(|| TrackingError::UnknownFolder(path.to_string()))?;
            change(&mut folder);
            // The store write lands before the observers hear about it: a store
            // that refuses the change must not leave a running engine
            // disagreeing with it.
            self.store.add_folder(&folder)?;
            folder
        };
        tracing::info!(
            folder = %folder.path,
            album_mapping = folder.album_mapping,
            album_scope = folder.album_scope.as_str(),
            "the album mapping changed"
        );
        self.notify(|observer| observer.folder_changed(&folder));
        Ok(folder)
    }

    /// Runs one change notification for every attached observer.
    ///
    /// The list is copied out first and no write lock is held, so an observer
    /// may attach, detach, or call back into this module.
    fn notify(&self, change: impl Fn(&dyn Observer)) {
        let observers: Vec<_> = self.observers.lock().expect("the observer list").clone();
        for observer in &observers {
            change(observer.as_ref());
        }
    }
}
