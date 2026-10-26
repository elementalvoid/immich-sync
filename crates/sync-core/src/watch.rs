//! The filesystem watcher (SPEC R2.3).
//!
//! `notify` reports changes and `notify-debouncer-full` merges the raw storm
//! into settled batches: one event per file, renames coalesced. The engine
//! applies the same skip predicate to the reported paths that the scan applies
//! (R2.2), so both discovery paths agree.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{
    DebounceEventResult, Debouncer, RecommendedCache, new_debouncer,
    notify::{Error, EventKind, RecommendedWatcher},
};

/// A debounced watcher over the source folders (R2.3).
///
/// shortcut: a network share (SMB/NFS) may emit no events at all, so R2.3's
/// 10-second promise holds on local disks only and the periodic reconciliation
/// scan is the safety net; add a poll watcher per folder if SMB support becomes
/// a target (#66).
pub struct FolderWatcher {
    debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
}

impl FolderWatcher {
    /// Builds a watcher that reports settled batches of changed paths.
    ///
    /// `on_paths` runs on the debouncer's own thread; it must not block. The
    /// callback receives every path the batch touched, whether or not it still
    /// exists: the engine decides what to do with each one.
    ///
    /// # Errors
    ///
    /// Returns the platform watcher error when the backend cannot start.
    pub fn new(
        debounce: Duration,
        mut on_paths: impl FnMut(Vec<PathBuf>) + Send + 'static,
    ) -> Result<Self, Error> {
        let debouncer =
            new_debouncer(
                debounce,
                None,
                move |result: DebounceEventResult| match result {
                    Ok(events) => on_paths(
                        events
                            .into_iter()
                            // A read changes nothing, and the scan's own walk of a
                            // folder opens it, so an `Access` event would make
                            // the engine read a directory change into its own
                            // scan and reconcile forever (#66).
                            .filter(|event| !matches!(event.event.kind, EventKind::Access(_)))
                            .flat_map(|event| event.event.paths)
                            .collect(),
                    ),
                    Err(errors) => {
                        for error in errors {
                            tracing::debug!(error = %error, "the watcher reported an error");
                        }
                    }
                },
            )?;
        Ok(Self { debouncer })
    }

    /// Watches one source folder, recursive or not (R2.1).
    ///
    /// # Errors
    ///
    /// Returns the platform error when the path cannot be watched; a folder
    /// that has disappeared is reported as a folder status by the caller
    /// (R2.5), not as a file error.
    pub fn watch(&mut self, path: &Path, recursive: bool) -> Result<(), Error> {
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        self.debouncer.watch(path, mode)
    }

    /// Stops watching one source folder the user removed (R2.1).
    ///
    /// # Errors
    ///
    /// Returns the platform error when the platform refuses the change.
    pub fn unwatch(&mut self, path: &Path) -> Result<(), Error> {
        self.debouncer.unwatch(path)
    }
}
