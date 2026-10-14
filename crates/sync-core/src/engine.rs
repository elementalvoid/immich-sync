//! The folder sync engine (SPEC R2, R3, R6, R8.7).
//!
//! The engine owns discovery, the durable queue, and the upload worker pool:
//!
//! 1. The watcher reports changes while the client runs; the reconciliation
//!    scan runs at startup, on the configured interval, and on demand from
//!    "Sync now" (R2.3).
//! 2. Discovery applies the shared skip rules (R2.2), hashes with SHA-1, and
//!    records a `pending` row before it asks the server anything (R6.7).
//! 3. `POST /assets/bulk-upload-check` runs in configurable batches (R3.1).
//! 4. Accepted files stream through a `tokio::sync::Semaphore` worker pool.
//!    `created` and `duplicate` are both terminal success (R3.2).
//! 5. A transient failure waits the retry policy's delay and is tried again;
//!    the attempt cap parks the item in the dead letter, and a run of
//!    connection failures holds the queue until the credential probe says the
//!    server is back (R6.1, R6.5, R6.6).
//! 6. Pause holds new upload requests inside the acceptance window; an upload
//!    already on the wire finishes (R8.7).
//!
//! The engine has no UI imports (N6, PLAN). The shell reads [`Engine::status`]
//! for the dashboard and the tray.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use immich_client::{BulkAction, BulkCheckItem, ClientError, RejectReason};
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio::task::{JoinHandle, JoinSet};

use crate::backoff::RetryPolicy;
use crate::breaker::CircuitBreaker;
use crate::connection::SharedSession;
use crate::failure::{FailureClass, FailureKind, classify};
use crate::hash::Sha1;
use crate::paths::{PathCase, is_within, path_key};
use crate::scan::{ScanError, scan_folder};
use crate::skip::{has_symlink_ancestor, skip_reason_below};
use crate::store::{
    AttemptOutcome, AttemptRecord, FailedItem, FolderCounts, ItemRecord, ItemState, SourceFolder,
    StateStore, StoreError,
};
use crate::upload::{
    PreparedItem, UploadFailure, UploadGate, UploadOutcome, Uploader, hash_in_background,
};
use crate::watch::FolderWatcher;

/// How the engine decides what to do and when (SPEC R2.3, R3.1, R6, R7.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    /// How many checksums one `bulk-upload-check` carries (R3.1). Default 100.
    pub batch_size: usize,
    /// How many uploads may run at once (R7.2). Read once at pool start; #8
    /// makes it live.
    pub concurrency: usize,
    /// How often the reconciliation scan runs (R2.3). Default 30 minutes.
    pub scan_interval: Duration,
    /// How long the watcher settles before it reports a batch (R2.3).
    pub watch_debounce: Duration,
    /// Whether to watch folders at all; a test can turn the watcher off to
    /// prove the scan finds what the watcher missed (R2.3).
    pub watch: bool,
    /// How many automatic attempts one item may spend before it is parked in
    /// the dead letter (R6.1, R6.5). Default 8, from the settings.
    pub attempt_cap: u32,
    /// The wait before a later attempt (R6.1, R6.2).
    pub retry: RetryPolicy,
    /// How many connection failures in a row trip the circuit breaker (R6.6).
    pub breaker_threshold: u32,
    /// How long the queue waits between credential probes while the breaker is
    /// open (R6.6).
    pub breaker_cooldown: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            batch_size: 100,
            concurrency: 4,
            scan_interval: Duration::from_secs(30 * 60),
            watch_debounce: Duration::from_secs(2),
            watch: true,
            attempt_cap: 8,
            retry: RetryPolicy::default(),
            breaker_threshold: 5,
            breaker_cooldown: Duration::from_secs(10),
        }
    }
}

/// Why the engine could not change its folders.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The state database refused the change.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The folder path was blank.
    #[error("the source folder path is empty")]
    EmptyFolder,
}

/// The activity state the tray and the dashboard show (R8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// Nothing is queued.
    Idle,
    /// Files are queued or in flight.
    Syncing,
    /// The user paused new uploads (R8.7).
    Paused,
    /// A failure or a missing folder needs the user.
    NeedsAttention,
}

impl Activity {
    /// The human-readable name.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Syncing => "Syncing",
            Self::Paused => "Paused",
            Self::NeedsAttention => "Needs attention",
        }
    }
}

/// The live state of one source folder (R2.5, R8.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderStatus {
    /// The folder path the user chose.
    pub path: String,
    /// Whether the watcher and the scan descend into subfolders (R2.1).
    pub recursive: bool,
    /// Whether the folder is readable right now (R2.5).
    pub available: bool,
    /// The folder's live counts.
    pub counts: FolderCounts,
    /// The last failure under this folder, if any.
    pub last_error: Option<String>,
}

/// Everything the dashboard and the tray read (R8.1, R8.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineStatus {
    /// The global activity state.
    pub activity: Activity,
    /// One entry per source folder, in the stored order.
    pub folders: Vec<FolderStatus>,
    /// The summed counts across folders.
    pub counts: FolderCounts,
    /// The last failure the engine recorded, if any.
    pub last_error: Option<String>,
}

impl EngineStatus {
    /// The upload progress across every folder, 0.0 to 1.0 (R8.1).
    #[must_use]
    pub fn progress(&self) -> f32 {
        let total = self.counts.uploaded + self.counts.pending;
        if total == 0 {
            return 0.0;
        }
        self.counts.uploaded as f32 / total as f32
    }
}

/// One source folder plus the live state the dashboard reads.
struct FolderEntry {
    folder: SourceFolder,
    available: bool,
    counts: FolderCounts,
    last_error: Option<String>,
}

/// The shared engine state. Tasks and the shell hold it through an `Arc`.
struct EngineInner {
    store: Arc<StateStore>,
    /// The connection session, so every request carries the current credential
    /// and a 401 is answered by the one re-login rule (R1.4).
    session: SharedSession,
    uploader: Uploader,
    config: EngineConfig,
    gate: UploadGate,
    /// Trips after a run of connection failures (R6.6).
    breaker: CircuitBreaker,
    queue: mpsc::UnboundedSender<PathBuf>,
    /// Paths a worker is currently preparing or uploading, so a scan and a
    /// watcher event for the same file do not run the pipeline twice.
    in_flight: Mutex<HashSet<String>>,
    folders: Mutex<Vec<FolderEntry>>,
    last_error: Mutex<Option<String>>,
    resume: Notify,
    sync_now: Notify,
    upload_slots: Arc<Semaphore>,
    watcher: Mutex<Option<FolderWatcher>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Set by `stop` before it aborts, so no worker is spawned afterwards.
    stopped: AtomicBool,
    /// The upload workers the dispatcher spawned, so `stop` can abort the ones
    /// still on the wire instead of leaving them running after the engine.
    uploads: Mutex<JoinSet<()>>,
}

/// The folder sync engine (R2, R3, R6.7, R8.7).
pub struct Engine {
    inner: Arc<EngineInner>,
    queue_rx: Mutex<Option<mpsc::UnboundedReceiver<PathBuf>>>,
}

impl Engine {
    /// Builds the engine over the open state store and a live session.
    ///
    /// The queue is re-seeded with what a previous run left unfinished
    /// (R6.7); completed rows are never re-queued. The folders and their
    /// counts are read once here, so the dashboard has live data from the
    /// first frame.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] when the folders or the queue cannot be read.
    pub fn new(
        store: Arc<StateStore>,
        session: SharedSession,
        config: EngineConfig,
    ) -> Result<Self, StoreError> {
        let mut entries = Vec::new();
        // Re-queue what a previous run left unfinished before the counts are
        // read, so a row this run will work on is not seeded as parked (R6.7).
        let (queue, queue_rx) = mpsc::unbounded_channel();
        for path in store.requeue_unfinished()? {
            let _ = queue.send(PathBuf::from(path));
        }
        for folder in store.folders()? {
            let counts = store.folder_counts(&folder.path).unwrap_or_else(|error| {
                tracing::warn!(folder = %folder.path, error = %error, "the folder counts could not be read");
                FolderCounts::default()
            });
            entries.push(FolderEntry {
                available: Path::new(&folder.path).is_dir(),
                folder,
                counts,
                last_error: None,
            });
        }
        let uploader = Uploader::new(session.clone(), store.clone());
        let breaker = CircuitBreaker::new(config.breaker_threshold, config.breaker_cooldown);
        let inner = Arc::new(EngineInner {
            store,
            session,
            uploader,
            breaker,
            upload_slots: Arc::new(Semaphore::new(config.concurrency.max(1))),
            config,
            gate: UploadGate::new(),
            queue,
            in_flight: Mutex::new(HashSet::new()),
            folders: Mutex::new(entries),
            last_error: Mutex::new(None),
            resume: Notify::new(),
            sync_now: Notify::new(),
            watcher: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            uploads: Mutex::new(JoinSet::new()),
        });
        Ok(Self {
            inner,
            queue_rx: Mutex::new(Some(queue_rx)),
        })
    }

    /// Starts the watcher, the dispatcher, and the reconciliation scan.
    ///
    /// Must be called from within a Tokio runtime. The scan runs once
    /// immediately, which is how files added while the client was stopped are
    /// found at startup (R2.3).
    pub fn start(self: &Arc<Self>) {
        let queue_rx = self.queue_rx.lock().expect("the queue lock").take();
        let Some(queue_rx) = queue_rx else {
            return;
        };
        let inner = Arc::clone(&self.inner);
        let dispatcher = tokio::spawn({
            let inner = Arc::clone(&inner);
            async move { dispatch_loop(inner, queue_rx).await }
        });
        self.inner
            .tasks
            .lock()
            .expect("the task list")
            .push(dispatcher);

        // Install the watcher before the first scan (R2.3), so a file created
        // in between is reported by one path or the other, never neither.
        if self.inner.config.watch {
            let weak: Weak<EngineInner> = Arc::downgrade(&self.inner);
            match FolderWatcher::new(self.inner.config.watch_debounce, move |paths| {
                if let Some(inner) = weak.upgrade() {
                    inner.enqueue_watch_batch(paths);
                }
            }) {
                Ok(mut watcher) => {
                    for folder in self.folder_snapshot() {
                        if Path::new(&folder.path).is_dir() {
                            let _ = watcher.watch(Path::new(&folder.path), folder.recursive);
                        }
                    }
                    *self.inner.watcher.lock().expect("the watcher lock") = Some(watcher);
                    tracing::info!("the folder watcher started");
                }
                Err(error) => {
                    tracing::error!(
                        error = %error,
                        "the folder watcher could not start; the reconciliation scan still runs"
                    );
                }
            }
        }
        let scanner = tokio::spawn({
            let inner = Arc::clone(&inner);
            async move { scan_loop(inner).await }
        });
        self.inner
            .tasks
            .lock()
            .expect("the task list")
            .push(scanner);
        tracing::info!(
            folders = self.inner.folders.lock().expect("the folder lock").len(),
            batch_size = self.inner.config.batch_size,
            concurrency = self.inner.config.concurrency,
            "the sync engine started"
        );
    }

    /// Stops the watcher, the scan and dispatch tasks, and the upload workers.
    /// A later run re-queues what was unfinished (R6.7). Safe before the first
    /// start and safe to call twice.
    pub fn stop(&self) {
        // Set before the abort, so a dispatcher already past its last await
        // cannot spawn a worker behind the abort.
        self.inner.stopped.store(true, Ordering::SeqCst);
        if let Some(watcher) = self.inner.watcher.lock().expect("the watcher lock").take() {
            drop(watcher);
        }
        for task in self.inner.tasks.lock().expect("the task list").drain(..) {
            task.abort();
        }
        self.inner
            .uploads
            .lock()
            .expect("the upload list")
            .abort_all();
        tracing::info!("the sync engine stopped");
    }

    /// Holds new uploads; one already on the wire finishes (R8.7).
    pub fn pause(&self) {
        self.inner.gate.pause();
        tracing::info!("the sync engine paused");
    }

    /// Lets uploads start again (R8.7).
    pub fn resume(&self) {
        self.inner.gate.resume();
        self.inner.resume.notify_waiters();
        tracing::info!("the sync engine resumed");
    }

    /// Whether new uploads are held.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.inner.gate.is_paused()
    }

    /// Asks for a reconciliation scan now (R8.3).
    pub fn sync_now(&self) {
        self.inner.sync_now.notify_one();
    }

    /// Adds a source folder and scans it (R2.1).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when the path is blank or the store refuses it.
    pub fn add_folder(&self, path: &str, recursive: bool) -> Result<(), EngineError> {
        let path = path.trim();
        if path.is_empty() {
            return Err(EngineError::EmptyFolder);
        }
        let folder = SourceFolder {
            path: path.to_string(),
            recursive,
        };
        self.inner.store.add_folder(&folder)?;
        // R8.1: a folder new to this engine starts from what the store already
        // holds for it; a zeroed card would never fill, because the rows are
        // settled and `prepare` does not count them again.
        let counts = self.inner.store.folder_counts(path).unwrap_or_else(|error| {
            tracing::warn!(folder = path, error = %error, "the folder counts could not be read");
            FolderCounts::default()
        });
        {
            let mut folders = self.inner.folders.lock().expect("the folder lock");
            let key = path_key(Path::new(path));
            if let Some(entry) = folders
                .iter_mut()
                .find(|entry| path_key(Path::new(&entry.folder.path)) == key)
            {
                entry.folder.recursive = recursive;
                entry.available = Path::new(path).is_dir();
            } else {
                folders.push(FolderEntry {
                    available: Path::new(path).is_dir(),
                    folder,
                    counts,
                    last_error: None,
                });
            }
        }
        let mut watcher = self.inner.watcher.lock().expect("the watcher lock");
        if let Some(watcher) = watcher.as_mut()
            && Path::new(path).is_dir()
        {
            let _ = watcher.watch(Path::new(path), recursive);
        }
        drop(watcher);
        tracing::info!(folder = path, recursive, "a source folder was added");
        // Existing files in the new folder need discovery, not only future
        // watch events.
        self.sync_now();
        Ok(())
    }

    /// Removes a source folder; its items stay in the store (R2.1).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when the store refuses the change.
    pub fn remove_folder(&self, path: &str) -> Result<(), EngineError> {
        self.inner.store.remove_folder(path)?;
        let key = path_key(Path::new(path));
        self.inner
            .folders
            .lock()
            .expect("the folder lock")
            .retain(|entry| path_key(Path::new(&entry.folder.path)) != key);
        let mut watcher = self.inner.watcher.lock().expect("the watcher lock");
        if let Some(watcher) = watcher.as_mut() {
            let _ = watcher.unwatch(Path::new(path));
        }
        tracing::info!(folder = path, "a source folder was removed");
        Ok(())
    }

    /// The live status the dashboard and the tray read (R8.1, R8.2).
    #[must_use]
    pub fn status(&self) -> EngineStatus {
        let folders: Vec<FolderStatus> = self
            .inner
            .folders
            .lock()
            .expect("the folder lock")
            .iter()
            .map(|entry| FolderStatus {
                path: entry.folder.path.clone(),
                recursive: entry.folder.recursive,
                available: entry.available,
                counts: entry.counts,
                last_error: entry.last_error.clone(),
            })
            .collect();
        let mut counts = FolderCounts::default();
        for folder in &folders {
            counts.pending += folder.counts.pending;
            counts.uploaded += folder.counts.uploaded;
            counts.failed += folder.counts.failed;
        }
        let activity = if self.inner.gate.is_paused() {
            Activity::Paused
        } else if !self.inner.session.state().is_connected()
            || folders
                .iter()
                .any(|folder| !folder.available || folder.last_error.is_some())
        {
            Activity::NeedsAttention
        } else if counts.pending > 0 {
            Activity::Syncing
        } else {
            Activity::Idle
        };
        EngineStatus {
            activity,
            folders,
            counts,
            last_error: self
                .inner
                .last_error
                .lock()
                .expect("the error lock")
                .clone(),
        }
    }

    /// The shared state store, so the shell can show store-backed data.
    #[must_use]
    pub fn store(&self) -> &Arc<StateStore> {
        &self.inner.store
    }

    /// The items the retry policy gave up on, in path order (R6.5).
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] when the query fails.
    pub fn failed_items(&self) -> Result<Vec<FailedItem>, StoreError> {
        self.inner.store.failed_items()
    }

    /// Puts one parked item back in the queue for a manual retry (R6.5).
    ///
    /// The item gets a fresh attempt budget, so a retry the user asked for is
    /// not parked again by a cap the previous run already spent. Returns
    /// whether the store held a parked item for the path.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] when the write fails.
    pub fn retry_failed(&self, path: &str) -> Result<bool, StoreError> {
        let queued = self.inner.store.retry_item(path)?;
        if queued {
            tracing::info!(path, "a parked item was queued for a manual retry");
            let _ = self.inner.queue.send(PathBuf::from(path));
        }
        Ok(queued)
    }

    /// A copy of the folder entries' identity, for the watcher setup.
    fn folder_snapshot(&self) -> Vec<SourceFolder> {
        self.inner
            .folders
            .lock()
            .expect("the folder lock")
            .iter()
            .map(|entry| entry.folder.clone())
            .collect()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

impl EngineInner {
    /// Queues the settled paths the watcher reported, after the shared skip
    /// rules (R2.2). A path that disappeared is left to the deletion review
    /// (#12).
    fn enqueue_watch_batch(&self, paths: Vec<PathBuf>) {
        for path in paths {
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Some(root) = self.folder_root_for(&path) else {
                continue;
            };
            // R2.2: the scan prunes symlinked directories, but the watcher
            // reports a file under one as an ordinary file, so refuse it too.
            if has_symlink_ancestor(&root, &path)
                || skip_reason_below(&root, &path, Some(&metadata)).is_some()
            {
                continue;
            }
            let _ = self.queue.send(path);
        }
    }

    /// The root of the folder that contains `path`, if any.
    fn folder_root_for(&self, path: &Path) -> Option<PathBuf> {
        let case = PathCase::for_platform();
        self.folders
            .lock()
            .expect("the folder lock")
            .iter()
            .find(|entry| is_within(Path::new(&entry.folder.path), path, case))
            .map(|entry| PathBuf::from(&entry.folder.path))
    }

    /// The folder entry that contains `path`.
    fn folder_index_for(&self, path: &Path) -> Option<usize> {
        let case = PathCase::for_platform();
        self.folders
            .lock()
            .expect("the folder lock")
            .iter()
            .position(|entry| is_within(Path::new(&entry.folder.path), path, case))
    }

    /// Waits while the user holds uploads (R8.7).
    async fn wait_while_paused(&self) {
        while self.gate.is_paused() {
            let notified = self.resume.notified();
            if !self.gate.is_paused() {
                break;
            }
            notified.await;
        }
    }

    /// Records that a newly queued file entered one folder, adding to the
    /// dashboard's pending count.
    ///
    /// `previous` is the row's state before this run queued it: a row the
    /// pipeline re-prepares out of `retry_wait` was counted as failed when it
    /// was parked, so it moves back to pending rather than being counted twice.
    fn note_pending(&self, path: &Path, previous: Option<ItemState>) {
        if let Some(index) = self.folder_index_for(path) {
            let mut folders = self.folders.lock().expect("the folder lock");
            let counts = &mut folders[index].counts;
            if previous == Some(ItemState::RetryWait) {
                counts.failed = counts.failed.saturating_sub(1);
            }
            counts.pending += 1;
        }
    }

    /// Records one finished item against its folder and releases its claim.
    fn finish_item(&self, item: &PreparedItem, outcome: &UploadOutcome) {
        let folder_index = self.folder_index_for(&item.path);
        if let Some(index) = folder_index {
            let mut folders = self.folders.lock().expect("the folder lock");
            let entry = &mut folders[index];
            match outcome {
                UploadOutcome::Created { .. } | UploadOutcome::Duplicate { .. } => {
                    entry.counts.pending = entry.counts.pending.saturating_sub(1);
                    entry.counts.uploaded += 1;
                    // R8.2: a later success clears the parked error, so a
                    // transient failure does not read as attention for a full
                    // scan interval.
                    entry.last_error = None;
                }
                UploadOutcome::Failed(failure) | UploadOutcome::DeadLetter(failure) => {
                    entry.counts.pending = entry.counts.pending.saturating_sub(1);
                    entry.counts.failed += 1;
                    entry.last_error = Some(failure_text(failure));
                }
                // Deferred keeps the item queued and its pending count.
                UploadOutcome::Deferred => {}
            }
        }
        self.in_flight
            .lock()
            .expect("the in-flight lock")
            .remove(&path_key(&item.path));
        if matches!(outcome, UploadOutcome::Deferred) {
            let _ = self.queue.send(item.path.clone());
        }
    }

    /// Records a transient failure that is about to be retried, so the
    /// dashboard shows why the item is waiting (R8.2) without finishing it.
    fn note_retrying(&self, item: &PreparedItem, failure: &UploadFailure) {
        if let Some(index) = self.folder_index_for(&item.path) {
            let mut folders = self.folders.lock().expect("the folder lock");
            folders[index].last_error = Some(failure_text(failure));
        }
    }

    /// Parks one item in the dead letter after the attempt cap (R6.5).
    ///
    /// The last reason the store holds is kept when this call has no failure of
    /// its own, which is how a restart that finds the cap already spent still
    /// shows why the item is parked.
    fn park_dead_letter(
        &self,
        item: &PreparedItem,
        failure: Option<&UploadFailure>,
    ) -> UploadOutcome {
        let path = item.path.to_string_lossy().into_owned();
        let existing = self.store.item(&path).ok().flatten();
        let (error_class, error_message) = match failure {
            Some(failure) => (failure.class, Some(failure.message.clone())),
            None => (
                existing.as_ref().and_then(|row| row.error_class),
                existing.as_ref().and_then(|row| row.error_message.clone()),
            ),
        };
        let record = ItemRecord {
            path: path.clone(),
            size: item.size,
            mtime_ns: item.mtime_ns,
            sha1: item.sha1,
            asset_id: None,
            state: ItemState::DeadLetter,
            error_class,
            error_message,
            attempts: 0,
        };
        if let Err(error) = self.store.record(&record) {
            tracing::error!(error = %error, "the state database could not record the dead letter");
        }
        let failure = failure.cloned().unwrap_or(UploadFailure {
            class: error_class,
            kind: FailureKind::Transient,
            message: "the attempt cap was reached".to_string(),
            retry_after: None,
            connection: false,
        });
        tracing::warn!(
            path,
            attempts = self
                .store
                .item(&path)
                .ok()
                .flatten()
                .map_or(0, |row| row.attempts),
            "R6.5: the item is in the dead letter and needs a manual retry"
        );
        UploadOutcome::DeadLetter(failure)
    }

    /// Records a failure for every item of a batch whose checksum check could
    /// not be made, and finishes their bookkeeping with it.
    ///
    /// A connection that is no longer usable failed as a whole, not once per
    /// item (R1.4): nothing is written, so the rows stay `pending` and the next
    /// reconciliation scan queues them again. Parking a batch of them would
    /// leave every file looking permanently broken to a sign-in that fixes it.
    fn fail_batch(&self, prepared: &[PreparedItem], error: &ClientError) {
        if !self.session.state().is_connected() {
            for item in prepared {
                self.release_item(item);
            }
            return;
        }
        for item in prepared {
            self.fail_item(item, error);
        }
    }

    /// Releases an item's in-flight claim without finishing it, so a later scan
    /// or "Sync now" can queue it again.
    fn release_item(&self, item: &PreparedItem) {
        self.in_flight
            .lock()
            .expect("the in-flight lock")
            .remove(&path_key(&item.path));
    }

    /// Records a failure that happened before an upload could start, and
    /// finishes the item's bookkeeping with it.
    fn fail_item(&self, item: &PreparedItem, error: &ClientError) {
        let outcome = self.park_row(item, error);
        self.finish_item(item, &outcome);
        tracing::warn!(
            path = %item.path.display(),
            class = classify(error).0.map_or("none", FailureClass::as_str),
            reason = %error.redacted_reason(),
            "the file was parked after a failure"
        );
    }

    /// Records one failure without finishing the item, so a caller that still
    /// holds the item (the retry loop) keeps its own bookkeeping.
    fn park_row(&self, item: &PreparedItem, error: &ClientError) -> UploadOutcome {
        let (class, kind) = classify(error);
        let state = match kind {
            crate::failure::FailureKind::Permanent => ItemState::FailedPermanent,
            crate::failure::FailureKind::Transient => ItemState::RetryWait,
        };
        let record = ItemRecord {
            path: item.path.to_string_lossy().into_owned(),
            size: item.size,
            mtime_ns: item.mtime_ns,
            sha1: item.sha1,
            asset_id: None,
            state,
            error_class: class,
            error_message: Some(error.to_string()),
            attempts: 0,
        };
        if let Err(store_error) = self.store.record(&record) {
            tracing::error!(error = %store_error, "the state database could not record the failure");
        }
        UploadOutcome::Failed(crate::upload::UploadFailure {
            class,
            kind,
            message: error.to_string(),
            retry_after: error.retry_after(),
            connection: matches!(error, ClientError::Unreachable(_)),
        })
    }
}

/// The user-facing line for one failure: the class, when there is one, and the
/// reason.
fn failure_text(failure: &UploadFailure) -> String {
    match failure.class {
        Some(class) => format!("{}: {}", FailureClass::as_str(class), failure.message),
        None => failure.message.clone(),
    }
}

/// Waits while the circuit breaker is open, probing with the
/// credential-validation endpoint until the server answers (R6.6).
///
/// A server that is merely gone is waited out, so the queue resumes as soon as
/// it is back without spending an item's attempt. The probe's failure is
/// returned only when the session needs the user: a credential the server
/// refuses cannot be waited out.
async fn wait_for_the_server(inner: &Arc<EngineInner>) -> Result<(), ClientError> {
    while let Some(wait) = inner.breaker.open_for() {
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        match inner.session.validate().await {
            Ok(()) => {
                inner.breaker.record_success();
                tracing::info!("the server answered the credential probe, so the queue resumes");
                return Ok(());
            }
            Err(error) if classify(&error).0 == Some(FailureClass::AuthFailed) => {
                // Every auth refusal is the user's to fix, not the server's to
                // wait out: a rejected session token and a revoked API key
                // both park the item and ask for attention (R1.4).
                tracing::warn!(
                    reason = %error.redacted_reason(),
                    "the credential probe was refused; the connection needs the user"
                );
                return Err(error);
            }
            Err(error) => {
                // A failed probe restarts the cooldown, so this loop is paced.
                inner.breaker.record_failure();
                tracing::warn!(
                    reason = %error.redacted_reason(),
                    "the server did not answer the credential probe"
                );
            }
        }
    }
    Ok(())
}

/// The dispatcher: batches queued paths, hashes them, checks them in one
/// request, and hands accepted files to the upload pool (R3.1, R3.2).
async fn dispatch_loop(inner: Arc<EngineInner>, mut queue_rx: mpsc::UnboundedReceiver<PathBuf>) {
    loop {
        inner.wait_while_paused().await;
        let Some(first) = queue_rx.recv().await else {
            break;
        };
        let mut batch = vec![first];
        while batch.len() < inner.config.batch_size {
            match queue_rx.try_recv() {
                Ok(path) => batch.push(path),
                Err(_) => break,
            }
        }
        // A pause that arrived while the batch was collected holds it too.
        inner.wait_while_paused().await;
        process_batch(&inner, batch).await;
    }
}

/// Prepares one batch and checks it with one `bulk-upload-check` (R3.1).
async fn process_batch(inner: &Arc<EngineInner>, batch: Vec<PathBuf>) {
    let mut prepared = Vec::with_capacity(batch.len());
    for path in batch {
        if let Some(item) = prepare(inner, &path).await {
            prepared.push(item);
        }
    }
    if prepared.is_empty() {
        return;
    }
    let checks: Vec<BulkCheckItem> = prepared
        .iter()
        .map(|item| BulkCheckItem {
            id: item.path.to_string_lossy().into_owned(),
            checksum: item.sha1.base64(),
        })
        .collect();
    let cap = inner.config.attempt_cap.max(1);
    // The count is per batch, not per file and not per run: a 429 storm that
    // spans two batches starts the second one from `ceiling(1)` again. That is
    // deliberate — the batch spent no upload attempt, so charging it the cap
    // would park files no request ever failed.
    let mut attempt = 0_u32;
    let results = loop {
        // R6.6: while the breaker is open, the batch waits for the credential
        // probe instead of asking a dead server again.
        if let Err(error) = wait_for_the_server(inner).await {
            inner.fail_batch(&prepared, &error);
            return;
        }
        attempt += 1;
        let client = inner.session.client();
        let checked = inner
            .session
            .request(|credential| {
                let client = client.clone();
                let checks = checks.clone();
                async move { client.bulk_upload_check(credential.auth(), &checks).await }
            })
            .await;
        match checked {
            Ok(results) => {
                // R6.6: the server answered, so the run of connection failures
                // the breaker counts ends here.
                inner.breaker.record_success();
                break results;
            }
            Err(error) => {
                let (_, kind) = classify(&error);
                if kind == FailureKind::Permanent || attempt >= cap {
                    // The checksum check could not be made, so no item spent an
                    // upload attempt: they stay for the next scan (R6.1).
                    inner.fail_batch(&prepared, &error);
                    return;
                }
                if matches!(error, ClientError::Unreachable(_)) {
                    inner.breaker.record_failure();
                }
                let delay = inner.config.retry.delay(attempt, error.retry_after());
                tracing::warn!(
                    attempt,
                    of = cap,
                    wait_ms = delay.as_millis() as u64,
                    reason = %error.redacted_reason(),
                    "the checksum check failed; the batch waits before it is tried again"
                );
                tokio::time::sleep(delay).await;
            }
        }
    };
    for item in prepared {
        let id = item.path.to_string_lossy().into_owned();
        let action = results
            .iter()
            .find(|result| result.id == id)
            .map(|result| result.action.clone());
        match action {
            Some(BulkAction::Accept) => spawn_upload(inner, item).await,
            Some(BulkAction::Reject {
                reason: RejectReason::Duplicate,
                asset_id: Some(asset_id),
                ..
            }) => {
                record_terminal(inner, &item, &asset_id, ItemState::Duplicate);
                inner.finish_item(&item, &UploadOutcome::Duplicate { asset_id });
            }
            Some(BulkAction::Reject {
                reason: RejectReason::UnsupportedFormat,
                ..
            }) => {
                inner.fail_item(
                    &item,
                    &ClientError::UnsupportedFormat(
                        "the checksum check refused the format".to_string(),
                    ),
                );
            }
            Some(BulkAction::Reject {
                reason: RejectReason::Duplicate,
                asset_id: None,
                ..
            }) => {
                inner.fail_item(&item, &missing_asset_error());
            }
            Some(BulkAction::Reject {
                reason: RejectReason::Other(reason),
                ..
            }) => {
                inner.fail_item(
                    &item,
                    &ClientError::Protocol {
                        status: 200,
                        message: format!("the checksum check refused it: {reason}"),
                        retry_after: None,
                    },
                );
            }
            None => {
                inner.fail_item(&item, &missing_result_error());
            }
        }
    }
}

/// One bulk-check answer that named no asset id.
fn missing_asset_error() -> ClientError {
    ClientError::Protocol {
        status: 200,
        message: "the server reported a duplicate without an asset id".to_string(),
        retry_after: None,
    }
}

/// One bulk-check answer the server never sent.
fn missing_result_error() -> ClientError {
    ClientError::Protocol {
        status: 200,
        message: "the checksum check answered for a different id".to_string(),
        retry_after: None,
    }
}

/// Writes a terminal row that did not need an upload (R3.2).
fn record_terminal(inner: &EngineInner, item: &PreparedItem, asset_id: &str, state: ItemState) {
    let record = ItemRecord {
        path: item.path.to_string_lossy().into_owned(),
        size: item.size,
        mtime_ns: item.mtime_ns,
        sha1: item.sha1,
        asset_id: Some(asset_id.to_string()),
        state,
        error_class: None,
        error_message: None,
        attempts: 0,
    };
    if let Err(error) = inner.store.record(&record) {
        tracing::error!(error = %error, "the state database could not record the outcome");
    }
}

/// Hashes one discovered path, skips what is already known, and records the
/// pending row the durable queue needs (R6.7).
async fn prepare(inner: &Arc<EngineInner>, path: &Path) -> Option<PreparedItem> {
    let key = path_key(path);
    {
        let mut in_flight = inner.in_flight.lock().expect("the in-flight lock");
        if !in_flight.insert(key.clone()) {
            return None;
        }
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => {
            // The file disappeared between discovery and hashing; the deletion
            // review (#12) owns missing files.
            inner
                .in_flight
                .lock()
                .expect("the in-flight lock")
                .remove(&key);
            return None;
        }
    };
    if !metadata.is_file() {
        inner
            .in_flight
            .lock()
            .expect("the in-flight lock")
            .remove(&key);
        return None;
    }
    let path_text = path.to_string_lossy().into_owned();
    let size = metadata.len();
    let mtime_ns = mtime_ns(&metadata);
    // Read the recorded outcome before hashing (R4.5): a finished file whose
    // size and mtime are unchanged needs no bytes read, so a reconciliation
    // scan does not re-hash an entire settled library.
    let existing = inner.store.item(&path_text).ok().flatten();
    if let Some(row) = &existing
        && row.size == size
        && row.mtime_ns == mtime_ns
        && is_settled(row.state)
    {
        inner
            .in_flight
            .lock()
            .expect("the in-flight lock")
            .remove(&key);
        return None;
    }
    let sha1: Sha1 = match hash_in_background(path.to_path_buf()).await {
        Ok(sha1) => sha1,
        Err(error) => {
            tracing::warn!(path = %path.display(), error = %error, "the file could not be hashed");
            inner
                .in_flight
                .lock()
                .expect("the in-flight lock")
                .remove(&key);
            return None;
        }
    };
    if let Some(row) = &existing
        && row.sha1 == sha1
        && is_settled(row.state)
    {
        // R4.5: the same SHA-1 with a recorded outcome never runs again. A
        // parked failure is retried by the retry policy, not by this skip.
        inner
            .in_flight
            .lock()
            .expect("the in-flight lock")
            .remove(&key);
        return None;
    }
    let item = PreparedItem {
        path: path.to_path_buf(),
        size,
        mtime_ns,
        sha1,
    };
    let record = ItemRecord {
        path: path_text,
        size: item.size,
        mtime_ns: item.mtime_ns,
        sha1: item.sha1,
        asset_id: None,
        state: ItemState::Pending,
        error_class: None,
        error_message: None,
        attempts: 0,
    };
    if let Err(error) = inner.store.record(&record) {
        tracing::error!(error = %error, "the state database could not record the pending item");
    }
    // The dashboard seed already counts a pending or uploading row (R8.1);
    // only a row this run newly queued adds to the pending total.
    if !existing
        .as_ref()
        .is_some_and(|row| matches!(row.state, ItemState::Pending | ItemState::Uploading))
    {
        inner.note_pending(path, existing.as_ref().map(|row| row.state));
    }
    Some(item)
}

/// Whether a recorded row is finished on its own: a terminal success, or a
/// failure no automatic retry can fix. A pending, uploading, or `retry_wait`
/// row is still work the pipeline owns, so a scan may pick it up again.
fn is_settled(state: ItemState) -> bool {
    matches!(
        state,
        ItemState::Uploaded
            | ItemState::Duplicate
            | ItemState::FailedPermanent
            | ItemState::DeadLetter
    )
}

/// Hands one accepted file to the upload pool, holding a semaphore permit for
/// the whole request (R7.2).
async fn spawn_upload(inner: &Arc<EngineInner>, item: PreparedItem) {
    let permit = match Arc::clone(&inner.upload_slots).acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => return,
    };
    if inner.gate.is_paused() {
        drop(permit);
        inner.finish_item(&item, &UploadOutcome::Deferred);
        return;
    }
    let inner = Arc::clone(inner);
    let mut uploads = inner.uploads.lock().expect("the upload list");
    if inner.stopped.load(Ordering::SeqCst) {
        // `stop` ran first: the item stays pending for the next engine (R6.7).
        return;
    }
    // Reap finished workers first, so a long run does not accumulate handles.
    while uploads.try_join_next().is_some() {}
    let worker = Arc::clone(&inner);
    uploads.spawn(async move {
        let outcome = upload_with_retries(&worker, &item).await;
        worker.finish_item(&item, &outcome);
        match &outcome {
            UploadOutcome::Created { asset_id } | UploadOutcome::Duplicate { asset_id } => {
                tracing::info!(path = %item.path.display(), asset_id = %asset_id, "the item finished");
            }
            UploadOutcome::Failed(failure) | UploadOutcome::DeadLetter(failure) => {
                tracing::warn!(
                    path = %item.path.display(),
                    permanent = failure.is_permanent(),
                    class = failure.class.map_or("none", FailureClass::as_str),
                    "the item was parked"
                );
            }
            UploadOutcome::Deferred => {}
        }
        drop(permit);
    });
}

/// Uploads one prepared item, retrying a transient failure with backoff until
/// the attempt cap parks it in the dead letter (R6.1, R6.3, R6.5, R6.6).
///
/// Every attempt that reached the wire is written to the attempt log (SPEC §5),
/// so a restart continues the count instead of starting a fresh budget.
async fn upload_with_retries(inner: &Arc<EngineInner>, item: &PreparedItem) -> UploadOutcome {
    let path_text = item.path.to_string_lossy().into_owned();
    let mut attempts = inner
        .store
        .item(&path_text)
        .ok()
        .flatten()
        .map_or(0, |row| row.attempts);
    let cap = inner.config.attempt_cap.max(1);
    loop {
        // R6.6: a run of connection failures holds the queue until the
        // credential probe says the server is back.
        if let Err(error) = wait_for_the_server(inner).await {
            // The credential is refused, not merely gone: the item needs the
            // user, so it is parked rather than retried (R1.4).
            return inner.park_row(item, &error);
        }
        if attempts >= cap {
            // The cap was already spent before this run, so the item is parked
            // without a request (R6.5, R6.7).
            return inner.park_dead_letter(item, None);
        }
        let started_ns = now_ns();
        attempts += 1;
        let outcome = inner.uploader.upload_prepared(item, &inner.gate).await;
        let (attempt_outcome, failure) = match &outcome {
            UploadOutcome::Created { .. } | UploadOutcome::Duplicate { .. } => {
                // R6.6: a request the server answered ends the run of
                // connection failures, so one unlucky request is not a run.
                inner.breaker.record_success();
                (AttemptOutcome::Succeeded, None)
            }
            UploadOutcome::Failed(failure) | UploadOutcome::DeadLetter(failure) => {
                (AttemptOutcome::Failed, Some(failure))
            }
            // A paused upload never reached the wire, so it is not an attempt.
            UploadOutcome::Deferred => return outcome,
        };
        if let Err(error) = inner.store.record_attempt(&AttemptRecord {
            path: path_text.clone(),
            started_ns,
            outcome: attempt_outcome,
            error_class: failure.and_then(|failure| failure.class),
        }) {
            tracing::error!(error = %error, "the state database could not record the attempt");
        }
        let Some(failure) = failure else {
            return outcome;
        };
        if failure.connection {
            // R6.6: only a connection failure feeds the breaker; a 429 or a
            // 503 is the server answering, not the server being gone.
            inner.breaker.record_failure();
        }
        if failure.is_permanent() || attempts >= cap {
            if !failure.is_permanent() {
                return inner.park_dead_letter(item, Some(failure));
            }
            return outcome;
        }
        inner.note_retrying(item, failure);
        let delay = inner.config.retry.delay(attempts, failure.retry_after);
        tracing::warn!(
            path = %item.path.display(),
            attempt = attempts,
            of = cap,
            wait_ms = delay.as_millis() as u64,
            class = failure.class.map_or("none", FailureClass::as_str),
            "the upload failed and will be tried again"
        );
        tokio::time::sleep(delay).await;
    }
}

/// The current time in nanoseconds since the Unix epoch, as the attempt log
/// records it.
fn now_ns() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

/// The reconciliation scan loop: startup, the configured interval, and
/// "Sync now" (R2.3).
async fn scan_loop(inner: Arc<EngineInner>) {
    let interval = if inner.config.scan_interval.is_zero() {
        Duration::from_secs(1)
    } else {
        inner.config.scan_interval
    };
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = inner.sync_now.notified() => {}
        }
        scan_once(&inner).await;
    }
}

/// Walks every source folder and queues what it finds (R2.3).
async fn scan_once(inner: &Arc<EngineInner>) {
    let folders: Vec<SourceFolder> = inner
        .folders
        .lock()
        .expect("the folder lock")
        .iter()
        .map(|entry| entry.folder.clone())
        .collect();
    let mut queued = 0_usize;
    for folder in folders {
        let report = tokio::task::spawn_blocking({
            let folder = folder.clone();
            move || scan_folder(&folder)
        })
        .await;
        match report {
            Ok(Ok(report)) => {
                set_folder_available(inner, &folder.path, true);
                queued += report.files.len();
                for file in report.files {
                    let _ = inner.queue.send(file);
                }
            }
            Ok(Err(ScanError::Missing(path))) => {
                set_folder_available(inner, &path, false);
                tracing::info!(folder = %path, "the source folder is not available");
            }
            Ok(Err(ScanError::Walk(error))) => {
                set_folder_error(inner, &folder.path, error);
            }
            Err(join) => {
                set_folder_error(inner, &folder.path, join.to_string());
            }
        }
    }
    tracing::info!(queued, "the reconciliation scan finished");
}

/// Marks one folder available or missing (R2.5).
fn set_folder_available(inner: &EngineInner, path: &str, available: bool) {
    let key = path_key(Path::new(path));
    let mut folders = inner.folders.lock().expect("the folder lock");
    if let Some(entry) = folders
        .iter_mut()
        .find(|entry| path_key(Path::new(&entry.folder.path)) == key)
    {
        entry.available = available;
        if available {
            entry.last_error = None;
        }
    }
}

/// Records a folder-level problem; it is not an error per file (R2.5).
fn set_folder_error(inner: &EngineInner, path: &str, error: impl std::fmt::Display) {
    let key = path_key(Path::new(path));
    let message = error.to_string();
    let mut folders = inner.folders.lock().expect("the folder lock");
    if let Some(entry) = folders
        .iter_mut()
        .find(|entry| path_key(Path::new(&entry.folder.path)) == key)
    {
        entry.last_error = Some(message.clone());
    }
    *inner.last_error.lock().expect("the error lock") = Some(message);
}

/// A modification time as nanoseconds since the Unix epoch, as the store
/// records it.
fn mtime_ns(metadata: &std::fs::Metadata) -> i64 {
    let Ok(modified) = metadata.modified() else {
        return 0;
    };
    match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_config_matches_the_spec() {
        let config = EngineConfig::default();
        assert_eq!(config.batch_size, 100, "R3.1: batched checks");
        assert_eq!(config.concurrency, 4, "R7.2: default concurrency");
        assert_eq!(
            config.scan_interval,
            Duration::from_secs(30 * 60),
            "R2.3: default scan interval"
        );
        assert_eq!(config.watch_debounce, Duration::from_secs(2));
    }

    #[test]
    fn activity_labels_use_the_spec_vocabulary() {
        assert_eq!(Activity::Idle.label(), "Idle");
        assert_eq!(Activity::Syncing.label(), "Syncing");
        assert_eq!(Activity::Paused.label(), "Paused");
        assert_eq!(Activity::NeedsAttention.label(), "Needs attention");
    }

    #[test]
    fn progress_is_zero_with_no_items_and_one_when_uploaded() {
        let mut status = EngineStatus {
            activity: Activity::Idle,
            folders: Vec::new(),
            counts: FolderCounts::default(),
            last_error: None,
        };
        assert_eq!(status.progress(), 0.0);
        status.counts.uploaded = 3;
        assert_eq!(status.progress(), 1.0);
        status.counts.pending = 1;
        assert!((status.progress() - 0.75).abs() < f32::EPSILON);
    }
}
