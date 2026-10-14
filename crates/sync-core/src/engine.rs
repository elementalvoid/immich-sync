//! The folder sync engine (SPEC R2, R3, R6.7, R8.7).
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
//! 5. Pause holds new upload requests inside the acceptance window; an upload
//!    already on the wire finishes (R8.7).
//!
//! The engine has no UI imports (N6, PLAN). The shell reads [`Engine::status`]
//! for the dashboard and the tray.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use immich_client::{
    BandwidthLimit, BulkAction, BulkCheckItem, ClientError, ImmichClient, RejectReason,
};
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore, mpsc};
use tokio::task::{JoinHandle, JoinSet};

use crate::connection::RequestCredential;
use crate::failure::{FailureClass, classify};
use crate::hash::Sha1;
use crate::paths::{PathCase, is_within, path_key};
use crate::scan::{ScanError, scan_folder};
use crate::skip::{has_symlink_ancestor, skip_reason_below};
use crate::store::{FolderCounts, ItemRecord, ItemState, SourceFolder, StateStore, StoreError};
use crate::upload::{PreparedItem, UploadGate, UploadOutcome, Uploader, hash_in_background};
use crate::watch::FolderWatcher;

/// How the engine decides what to do and when (SPEC R2.3, R3.1, R7.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    /// How many checksums one `bulk-upload-check` carries (R3.1). Default 100.
    pub batch_size: usize,
    /// How many uploads may run at once (R7.2). Default 4. [`Engine::set_limits`]
    /// is the live path; this is the value the engine starts with.
    pub concurrency: usize,
    /// The upload bandwidth limit in bytes per second; 0 means unlimited
    /// (R7.1). [`Engine::set_limits`] is the live path.
    pub bandwidth_bytes_per_second: u64,
    /// How often the reconciliation scan runs (R2.3). Default 30 minutes.
    pub scan_interval: Duration,
    /// How long the watcher settles before it reports a batch (R2.3).
    pub watch_debounce: Duration,
    /// Whether to watch folders at all; a test can turn the watcher off to
    /// prove the scan finds what the watcher missed (R2.3).
    pub watch: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            batch_size: 100,
            concurrency: 4,
            bandwidth_bytes_per_second: 0,
            scan_interval: Duration::from_secs(30 * 60),
            watch_debounce: Duration::from_secs(2),
            watch: true,
        }
    }
}

/// The user-set limits the running engine can be moved to (SPEC R7).
///
/// [`Engine::set_limits`] applies both without a restart: the bandwidth limit
/// is read by an upload already on the wire, and the concurrency limit admits
/// or retires workers as the pool drains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Upload bytes per second; 0 means unlimited (R7.1).
    pub bandwidth_bytes_per_second: u64,
    /// How many uploads may run at once (R7.2).
    pub concurrency: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bandwidth_bytes_per_second: 0,
            concurrency: 4,
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
    client: ImmichClient,
    credential: RequestCredential,
    uploader: Uploader,
    config: EngineConfig,
    gate: UploadGate,
    queue: mpsc::UnboundedSender<PathBuf>,
    /// Paths a worker is currently preparing or uploading, so a scan and a
    /// watcher event for the same file do not run the pipeline twice.
    in_flight: Mutex<HashSet<String>>,
    folders: Mutex<Vec<FolderEntry>>,
    last_error: Mutex<Option<String>>,
    resume: Notify,
    sync_now: Notify,
    upload_slots: Arc<Semaphore>,
    /// The upload bandwidth limit, shared with every upload so a change
    /// reaches one already on the wire (R7.1).
    bandwidth: BandwidthLimit,
    /// The concurrency the pool is sized to right now (R7.2).
    concurrency: AtomicUsize,
    /// Serializes a pool resize, so two quick settings changes cannot leave
    /// the permits and this count disagreeing.
    limits_lock: AsyncMutex<()>,
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
    /// Builds the engine over the open state store and a signed-in client.
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
        client: ImmichClient,
        credential: RequestCredential,
        config: EngineConfig,
    ) -> Result<Self, StoreError> {
        let mut entries = Vec::new();
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
        let (queue, queue_rx) = mpsc::unbounded_channel();
        for path in store.requeue_unfinished()? {
            let _ = queue.send(PathBuf::from(path));
        }
        let uploader = Uploader::new(client.clone(), store.clone());
        let inner = Arc::new(EngineInner {
            store,
            client,
            credential,
            uploader,
            upload_slots: Arc::new(Semaphore::new(config.concurrency.max(1))),
            bandwidth: BandwidthLimit::new(config.bandwidth_bytes_per_second),
            concurrency: AtomicUsize::new(config.concurrency.max(1)),
            limits_lock: AsyncMutex::new(()),
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

    /// The limits the engine is running with right now (R7.1, R7.2).
    #[must_use]
    pub fn limits(&self) -> Limits {
        Limits {
            bandwidth_bytes_per_second: self.inner.bandwidth.bytes_per_second(),
            concurrency: self.inner.concurrency.load(Ordering::Acquire),
        }
    }

    /// Moves both limits while the engine runs (R7.1, R7.2).
    ///
    /// The bandwidth limit is read by every upload on its next poll, so a
    /// change reaches an upload already on the wire without a restart and
    /// without re-sending a request. A raised concurrency admits new workers
    /// at once. A lowered concurrency lets the in-flight uploads finish: the
    /// pool takes no new work until it has drained to the new number, and no
    /// upload is cancelled or failed.
    pub async fn set_limits(&self, limits: Limits) {
        self.inner.bandwidth.set(limits.bandwidth_bytes_per_second);
        // One resize at a time, so a second change cannot race the permits.
        let _resize = self.inner.limits_lock.lock().await;
        let target = limits.concurrency.max(1);
        let current = self.inner.concurrency.swap(target, Ordering::AcqRel);
        if target > current {
            self.inner.upload_slots.add_permits(target - current);
        } else if target < current {
            // Take the whole pool first, so nothing new starts while it
            // drains; then forget the difference, so the pool stays smaller
            // from then on.
            let slots = Arc::clone(&self.inner.upload_slots);
            let remove = current - target;
            let permits = u32::try_from(current).unwrap_or(u32::MAX);
            if let Ok(mut held) = slots.acquire_many_owned(permits).await {
                match held.split(remove) {
                    Some(removed) => removed.forget(),
                    None => held.forget(),
                }
            }
        }
        tracing::info!(
            bandwidth = limits.bandwidth_bytes_per_second,
            concurrency = target,
            "the limits changed"
        );
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
        } else if folders
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
    fn note_pending(&self, path: &Path) {
        if let Some(index) = self.folder_index_for(path) {
            let mut folders = self.folders.lock().expect("the folder lock");
            folders[index].counts.pending += 1;
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
                UploadOutcome::Failed(failure) => {
                    entry.counts.pending = entry.counts.pending.saturating_sub(1);
                    entry.counts.failed += 1;
                    entry.last_error = Some(match failure.class {
                        Some(class) => {
                            format!("{}: {}", FailureClass::as_str(class), failure.message)
                        }
                        None => failure.message.clone(),
                    });
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

    /// Records a failure that happened before an upload could start.
    fn fail_item(&self, item: &PreparedItem, error: &ClientError) {
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
        };
        if let Err(store_error) = self.store.record(&record) {
            tracing::error!(error = %store_error, "the state database could not record the failure");
        }
        let outcome = UploadOutcome::Failed(crate::upload::UploadFailure {
            class,
            kind,
            message: error.to_string(),
        });
        self.finish_item(item, &outcome);
        tracing::warn!(
            path = %item.path.display(),
            class = class.map_or("none", FailureClass::as_str),
            reason = %error.redacted_reason(),
            "the file was parked after a failure"
        );
    }
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
    let checked = inner
        .client
        .bulk_upload_check(inner.credential.auth(), &checks)
        .await;
    match checked {
        Ok(results) => {
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
                            },
                        );
                    }
                    None => {
                        inner.fail_item(&item, &missing_result_error());
                    }
                }
            }
        }
        Err(error) => {
            for item in prepared {
                inner.fail_item(&item, &error);
            }
        }
    }
}

/// One bulk-check answer that named no asset id.
fn missing_asset_error() -> ClientError {
    ClientError::Protocol {
        status: 200,
        message: "the server reported a duplicate without an asset id".to_string(),
    }
}

/// One bulk-check answer the server never sent.
fn missing_result_error() -> ClientError {
    ClientError::Protocol {
        status: 200,
        message: "the checksum check answered for a different id".to_string(),
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
        // failed row is parked until #7 adds the retry policy.
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
    };
    if let Err(error) = inner.store.record(&record) {
        tracing::error!(error = %error, "the state database could not record the pending item");
    }
    // The dashboard seed already counts a pending or uploading row (R8.1);
    // only a row this run newly queued adds to the pending total.
    if !existing.is_some_and(|row| matches!(row.state, ItemState::Pending | ItemState::Uploading)) {
        inner.note_pending(path);
    }
    Some(item)
}

/// Whether a recorded row is finished on its own: a terminal success or a
/// failure parked for the retry policy (#7). A pending or uploading row is
/// still work the pipeline owns.
fn is_settled(state: ItemState) -> bool {
    matches!(
        state,
        ItemState::Uploaded
            | ItemState::Duplicate
            | ItemState::FailedPermanent
            | ItemState::RetryWait
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
        let outcome = worker
            .uploader
            .upload_prepared(&item, &worker.credential, &worker.gate, &worker.bandwidth)
            .await;
        worker.finish_item(&item, &outcome);
        match &outcome {
            UploadOutcome::Created { asset_id } | UploadOutcome::Duplicate { asset_id } => {
                tracing::info!(path = %item.path.display(), asset_id = %asset_id, "the item finished");
            }
            UploadOutcome::Failed(failure) => {
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
            config.bandwidth_bytes_per_second, 0,
            "R7.1: default bandwidth is unlimited"
        );
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
