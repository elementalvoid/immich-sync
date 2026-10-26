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
//! 7. The sync tag follows every settled upload, and one task backfills it onto
//!    the assets that were uploaded before the setting was turned on (R3.7).
//!
//! The engine has no UI imports (N6, PLAN). The shell reads [`Engine::status`]
//! for the dashboard and the tray.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use immich_client::{BandwidthLimit, BulkCheckItem, ClientError};
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore, mpsc};
use tokio::task::{JoinHandle, JoinSet};

use crate::albums::AlbumMapper;
use crate::backfill::{BACKFILL_BATCH, Backfill, BackfillPolicy};
use crate::backoff::RetryPolicy;
use crate::breaker::CircuitBreaker;
use crate::connection::SharedSession;
use crate::failure::{FailureClass, FailureKind, classify};
use crate::hash::{Sha1, sha1_file};
use crate::meter::UploadMeter;
use crate::missing::{Fate, Host, SizeIndex, fate, is_gone, rename_target};
use crate::pairing::FolderIndex;
use crate::paths::{PathCase, is_within, key_is_within, path_key};
use crate::scan::{ScanError, scan_folder};
use crate::skip::{has_symlink_ancestor, skip_reason_below};
use crate::status::{AlbumRename, ConnectionHealth, FolderStatus, Status, StatusInputs};
use crate::store::{
    Album, AlbumScope, AttemptOutcome, AttemptRecord, FailedItem, FolderCounts, ItemRecord,
    ItemState, ReviewDecision, ReviewItem, SourceFolder, StateStore, StoreError,
};
use crate::tags::{TagStatus, Tagger};
use crate::tracking::{Observer, Tracking, TrackingError, stored_folder};
use crate::upload::{
    CheckOutcome, PreparedItem, UploadFailure, UploadGate, UploadOutcome, Uploader, check_outcome,
    hash_in_background, pending_sidecar,
};
use crate::versions::Versioner;

use crate::watch::FolderWatcher;

/// How the engine decides what to do and when (SPEC R2.3, R3.1, R6, R7.2).
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
    /// How many automatic attempts one item may spend before it is parked in
    /// the dead letter (R6.1, R6.5). Default 8, from the settings.
    pub attempt_cap: u32,
    /// The wait before a later attempt (R6.1, R6.2).
    pub retry: RetryPolicy,
    /// How many connection failures in a row trip the circuit breaker (R6.6).
    /// Default 5. The connection's attention run is its own, shorter number
    /// ([`Self::attention_threshold`]): the breaker gates the queue, not the
    /// tray.
    pub breaker_threshold: u32,
    /// How many failed requests in a row the connection waits out before the
    /// tray needs the user (R1.4). Default 3: one refused batch spends up to
    /// two refusals (the one re-login and the refusal that starts the run), so
    /// a single batch never worries the user, while a credential that keeps
    /// failing reaches the user on the next scan (about 30 minutes by default)
    /// instead of about an hour a run of five allowed. A success
    /// resets the run.
    pub attention_threshold: u32,
    /// How long the queue waits between credential probes while the breaker is
    /// open (R6.6).
    pub breaker_cooldown: Duration,
    /// Whether uploaded assets get the sync tag (R3.7). The library default is
    /// off; the app passes the user's on-by-default setting.
    /// [`Engine::set_tag_uploads`] is the live path.
    pub tag_uploaded_assets: bool,
    /// How many asset ids one backfill request carries (R3.7). Default
    /// [`BACKFILL_BATCH`]; a test lowers it so a small library needs more than
    /// one request.
    pub tag_batch_size: usize,
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
            attempt_cap: 8,
            retry: RetryPolicy::default(),
            breaker_threshold: 5,
            attention_threshold: 3,
            breaker_cooldown: Duration::from_secs(10),
            tag_uploaded_assets: false,
            tag_batch_size: BACKFILL_BATCH,
        }
    }
}

/// The user-set limits the running engine can be moved to (SPEC R7).
///
/// [`Engine::set_limits`] applies both without a restart: the bandwidth limit
/// is read by an upload already on the wire, and a concurrency change moves
/// the next attempt to a freshly sized pool.
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
    /// The client already tracks this source folder (#40, R2.1).
    #[error("{0} is already a source folder of this client")]
    FolderExists(String),
    /// The path is not one of this engine's source folders.
    #[error("{0} is not a source folder of this client")]
    UnknownFolder(String),
}

impl From<TrackingError> for EngineError {
    fn from(error: TrackingError) -> Self {
        match error {
            TrackingError::Store(error) => Self::Store(error),
            TrackingError::EmptyFolder => Self::EmptyFolder,
            TrackingError::UnknownFolder(path) => Self::UnknownFolder(path),
            TrackingError::FolderExists(path) => Self::FolderExists(path),
        }
    }
}

/// Why a deletion-review decision could not be recorded (R5.3, R5.5).
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    /// The state database refused the decision.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The server refused the trash call, or could not be reached (R5.5).
    #[error(transparent)]
    Client(#[from] ClientError),
    /// The path is not waiting for a decision.
    #[error("{0} is not waiting for a review decision")]
    NotInReview(String),
}

/// One source folder plus the live state the dashboard reads.
struct FolderEntry {
    folder: SourceFolder,
    available: bool,
    counts: FolderCounts,
    last_error: Option<String>,
    /// When a scan last read this folder and when it last uploaded, for the
    /// folder detail (#43). The store holds the same two values, so a restart
    /// reads them back.
    last_scan_ns: Option<i64>,
    last_upload_ns: Option<i64>,
    /// This folder's upload meter (R8.1). Every upload the engine runs for a
    /// file under this folder streams under its limit view and counts itself
    /// here, so the bytes of two concurrent uploads can never be swapped.
    meter: Arc<UploadMeter>,
    /// The folder's album row: its id and the title the server does not hold
    /// yet (#47). It is the store's row, refreshed where the row changes; the
    /// status poll never reads the database.
    album: Album,
}

/// The shared engine state. Tasks and the shell hold it through an `Arc`.
struct EngineInner {
    store: Arc<StateStore>,
    /// The connection session, so every request carries the current credential
    /// and a 401 is answered by the one re-login rule (R1.4).
    session: SharedSession,
    uploader: Uploader,
    albums: AlbumMapper,
    /// Carries a changed file's associations onto its new version (R4.2-R4.4).
    versions: Versioner,
    /// Applies the sync tag to a settled asset (R3.7).
    tags: Tagger,
    /// Backfills the tag onto the library that was uploaded before the tag was
    /// turned on (R3.7).
    backfill: Backfill,
    /// Wakes the backfill task: the setting was turned on or the user asked
    /// for the library to be tagged again (R3.7).
    tag_wake: Notify,
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
    /// The upload pool in force right now. A resize replaces the whole
    /// `Arc<Semaphore>` instead of moving permits in and out of one pool.
    upload_slots: Mutex<Arc<Semaphore>>,
    /// The upload bandwidth limit, shared with every upload so a change
    /// reaches one already on the wire (R7.1).
    bandwidth: BandwidthLimit,
    /// The concurrency the pool is sized to right now (R7.2).
    concurrency: AtomicUsize,
    /// Serializes a pool resize, so two quick settings changes cannot race
    /// the swap and this count.
    limits_lock: AsyncMutex<()>,
    watcher: Mutex<Option<FolderWatcher>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Set by `stop` before it aborts, so no worker is spawned afterwards.
    stopped: AtomicBool,
    /// Whether `scan_once` is walking the folders right now (#56). The status
    /// poll reads it, so the dashboard can report the click; `stop` leaves it
    /// to the aborted pass's guard.
    scanning: AtomicBool,
    /// The upload workers the dispatcher spawned, so `stop` can abort the ones
    /// still on the wire instead of leaving them running after the engine.
    uploads: Mutex<JoinSet<()>>,
    /// Paths the watcher reported gone (R5.1). The watcher callback must not
    /// block, so it only records the path and wakes the scan loop, which asks
    /// the store whether anything tracked really disappeared.
    absences: Mutex<Vec<PathBuf>>,
    /// The folder listings a scan pass reuses (R3.3). The scan clears it and
    /// the watcher drops the folder a change touched, so a batch reads only
    /// what is missing: a 10k-file folder is listed once per scan rather than
    /// once per batch.
    index_cache: Mutex<FolderIndex>,
    /// Wakes the scan loop when an absence needs reconciling.
    reconcile: Notify,
    /// Wakes the album retry, which sends the titles the user changed (#47).
    album_wake: Notify,
}

/// The folder sync engine (R2, R3, R6.7, R8.7).
pub struct Engine {
    inner: Arc<EngineInner>,
    queue_rx: Mutex<Option<mpsc::UnboundedReceiver<PathBuf>>>,
    /// The folders and review decisions this engine follows.
    tracking: Arc<Tracking>,
    /// The engine's own observer, so a stop detaches this one and no other.
    observer: Arc<dyn Observer>,
}

impl Engine {
    /// Builds the engine over the shared tracking module and a live session.
    ///
    /// The queue is re-seeded with what a previous run left unfinished
    /// (R6.7); completed rows are never re-queued. The folders and their
    /// counts are read once here, so the dashboard has live data from the
    /// first frame. The engine attaches its observer immediately, so a folder
    /// edit that lands while the engine starts still reaches it.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] when the folders or the queue cannot be read.
    pub fn new(
        tracking: Arc<Tracking>,
        session: SharedSession,
        config: EngineConfig,
    ) -> Result<Self, StoreError> {
        let store = Arc::clone(tracking.store());
        // Re-queue what a previous run left unfinished before the counts are
        // read, so a row this run will work on is not seeded as parked (R6.7):
        // a `retry_wait` row otherwise counts as failed and then becomes
        // pending, and no later bookkeeping moves the failed count back down.
        let (queue, queue_rx) = mpsc::unbounded_channel();
        for path in store.requeue_unfinished()? {
            let _ = queue.send(PathBuf::from(path));
        }
        // One limit for the whole client (R7.1), viewed once per folder so a
        // card can read its own share of the bytes it meters (R8.1).
        let bandwidth = BandwidthLimit::new(config.bandwidth_bytes_per_second);
        // #52: the counts are seeded with the ownership rule, over the one
        // ordered folder list, so a nested folder's card matches its meter.
        let stored_folders = store.folders()?;
        let counts = store
            .folder_counts_by_owner(&stored_folders)
            .unwrap_or_else(|error| {
                tracing::warn!(error = %error, "the folder counts could not be read");
                vec![FolderCounts::default(); stored_folders.len()]
            });
        // A store read that returned fewer counts than folders must not vanish
        // a folder's card, so the count is looked up per folder.
        debug_assert_eq!(counts.len(), stored_folders.len());
        let mut entries = Vec::new();
        for (index, folder) in stored_folders.into_iter().enumerate() {
            let counts = counts.get(index).copied().unwrap_or_default();
            // #43: the folder detail keeps its timestamps across a restart.
            let times = store.folder_times(&folder.path).unwrap_or_else(|error| {
                tracing::warn!(folder = %folder.path, error = %error, "the folder times could not be read");
                crate::store::FolderTimes::default()
            });
            // #47: the album row holds the title the user chose, so the
            // status reads it from memory rather than the database.
            let album = album_of_store(&store, &folder);
            entries.push(FolderEntry {
                available: Path::new(&folder.path).is_dir(),
                meter: Arc::new(UploadMeter::new(&bandwidth)),
                folder,
                counts,
                last_error: None,
                last_scan_ns: times.last_scan_ns,
                last_upload_ns: times.last_upload_ns,
                album,
            });
        }
        let uploader = Uploader::new(session.clone(), store.clone());
        // R1.4: the album calls go through the same session, so a 401 on one of
        // them spends the one re-login the engine already shares.
        let albums = AlbumMapper::new(session.clone(), store.clone());
        let versions = Versioner::new(session.clone(), store.clone());
        // R3.7: the tagger starts with the caller's setting (the library
        // default keeps it off; the app's stored value is on by default) and is
        // moved live by `set_tag_uploads`.
        let tags = Tagger::new(session.clone());
        tags.set_enabled(config.tag_uploaded_assets);
        // R3.7: the backfill reads its recorded progress at construction, so a
        // restart publishes where the last run stopped before this one starts.
        let backfill = Backfill::new(
            store.clone(),
            BackfillPolicy {
                batch: config.tag_batch_size.max(1),
                attempts: config.attempt_cap.max(1),
                retry: config.retry,
            },
        )?;
        let breaker = CircuitBreaker::new(config.breaker_threshold, config.breaker_cooldown);
        // R1.4: the attention run is its own, shorter number. A refused batch
        // spends up to two failures, so the default of three keeps one batch
        // below the alarm while still reaching the user on the next scan.
        session.set_attention_threshold(config.attention_threshold);
        let inner = Arc::new(EngineInner {
            store,
            session,
            uploader,
            albums,
            versions,
            tags,
            backfill,
            tag_wake: Notify::new(),
            breaker,
            upload_slots: Mutex::new(Arc::new(Semaphore::new(config.concurrency.max(1)))),
            bandwidth,
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
            album_wake: Notify::new(),
            watcher: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            scanning: AtomicBool::new(false),
            uploads: Mutex::new(JoinSet::new()),
            absences: Mutex::new(Vec::new()),
            reconcile: Notify::new(),
            index_cache: Mutex::new(FolderIndex::default()),
        });
        let observer: Arc<dyn Observer> = Arc::new(EngineObserver {
            inner: Arc::downgrade(&inner),
        });
        tracking.attach(Arc::clone(&observer));
        Ok(Self {
            inner,
            queue_rx: Mutex::new(Some(queue_rx)),
            tracking,
            observer,
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
        // R3.7: the tag's backfill has one task of its own, woken by the
        // setting and by "Tag existing assets now". The first run starts now,
        // so an API key that is missing a permission reaches Settings before
        // the first upload does.
        let backfill = tokio::spawn(tag_backfill_loop(Arc::clone(&self.inner)));
        self.inner
            .tasks
            .lock()
            .expect("the task list")
            .push(backfill);
        self.inner.tag_wake.notify_one();
        // #47: a title the user changed is sent by its own task, woken by the
        // edit and by the scan, so no status poll sends a request.
        let albums = tokio::spawn({
            let inner = Arc::clone(&self.inner);
            async move { album_retry_loop(inner).await }
        });
        self.inner.tasks.lock().expect("the task list").push(albums);
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

    /// The bytes this engine has put on the wire since it started (R10.2).
    ///
    /// The count is the metered upload bodies' own, not a second measurement,
    /// so the diagnostics throughput and the bandwidth limit cannot disagree.
    #[must_use]
    pub fn uploaded_bytes(&self) -> u64 {
        self.inner.bandwidth.total_bytes_moved()
    }

    /// Moves both limits while the engine runs (R7.1, R7.2).
    ///
    /// The bandwidth limit is read by every upload on its next poll, so a
    /// change reaches an upload already on the wire without a restart and
    /// without re-sending a request. A resize swaps in a freshly sized pool:
    /// the attempts in flight finish on the pool they took their permit from,
    /// and every worker waiting on the old pool fails out at once and re-queues
    /// its item for the new pool, so the new limit is in force for the next
    /// attempt instead of after the whole waiting list has run. The new pool
    /// admits its own full size while those attempts are still running, so the
    /// running count can briefly exceed a shrinking limit; no upload is
    /// cancelled or failed.
    ///
    /// The overshoot is per swap: the running count can reach the attempts in
    /// flight at the swap plus the new pool's size. That is not a one-time
    /// correction, because a later swap does not wait for the attempts the
    /// previous one admitted: a second resize while those are still running
    /// lets the count overshoot again by the attempts in flight at that swap
    /// plus its own new size, so repeated swaps can compound the peak.
    pub async fn set_limits(&self, limits: Limits) {
        self.inner.bandwidth.set(limits.bandwidth_bytes_per_second);
        // One resize at a time, so a second change cannot race the swap.
        let _resize = self.inner.limits_lock.lock().await;
        let target = limits.concurrency.max(1);
        let current = self.inner.concurrency.load(Ordering::Acquire);
        if target != current {
            let fresh = Arc::new(Semaphore::new(target));
            let old = std::mem::replace(
                &mut *self.inner.upload_slots.lock().expect("the upload slots"),
                fresh,
            );
            // Closing the old pool releases its waiters now instead of leaving
            // them queued behind permits nothing new will release into; their
            // deferred items go back to the queue and take a permit from the
            // new pool. The in-flight attempts keep the permits they hold.
            old.close();
            // Published only once the swap is done, so the reported limit and
            // the pool it stands for cannot disagree.
            self.inner.concurrency.store(target, Ordering::Release);
        }
        tracing::info!(
            bandwidth = limits.bandwidth_bytes_per_second,
            concurrency = target,
            "the limits changed"
        );
    }

    /// The sync tag's live state, for the Settings section (R3.7).
    ///
    /// The engine model polls this with the rest of the status, so the view
    /// never makes a second permission probe of its own, and the backfill's
    /// progress rides the same poll.
    #[must_use]
    pub fn tag_status(&self) -> TagStatus {
        TagStatus {
            backfill: self.inner.backfill.status(),
            ..self.inner.tags.status()
        }
    }

    /// Turns the sync tag on or off while the engine runs (R3.7).
    ///
    /// Turning it on wakes the backfill, which resolves the tag again before its
    /// first batch: an API key missing a permission is reported to Settings
    /// before the first upload, and a tag the user deleted and recreated in
    /// Immich is upserted again, so the new id replaces the cached one.
    /// Turning it off stops new tagging and pauses the backfill at its next
    /// batch boundary: assets that already carry the tag keep it, and the tag
    /// itself is not deleted.
    pub async fn set_tag_uploads(&self, enabled: bool) {
        let changed = self.inner.tags.set_enabled(enabled);
        if changed && enabled {
            self.inner.tag_wake.notify_one();
        }
        tracing::info!(enabled, "R3.7: the sync tag setting changed");
    }

    /// Tags every asset the client ever uploaded again (R3.7: "Tag existing
    /// assets now").
    ///
    /// The recorded completion is cleared and the published progress starts
    /// over, then the backfill picks the library up at its first batch. It is
    /// safe on assets that already carry the tag, which is what makes it the
    /// answer to a tag that was deleted in Immich or a permission that was
    /// fixed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backfill state cannot be cleared.
    pub fn retag_all(&self) -> Result<(), StoreError> {
        self.inner.backfill.restart()?;
        self.inner.tag_wake.notify_one();
        tracing::info!("R3.7: the library was asked to be tagged again");
        Ok(())
    }

    /// Adds a source folder and scans it (R2.1).
    ///
    /// This is an upsert: a path the client already tracks has its recursive
    /// flag set and keeps its album settings, which is what a card's Recursive
    /// switch writes. Use [`Self::add_folder_settings`] to record a new folder
    /// and refuse one that already exists.
    ///
    /// The folder row belongs to the tracking module; this engine hears about
    /// the change through its observer, which installs the watch and asks for
    /// a scan.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when the path is blank or the store refuses it.
    pub fn add_folder(&self, path: &str, recursive: bool) -> Result<(), EngineError> {
        self.tracking.add_folder(path, recursive)?;
        Ok(())
    }

    /// Adds a source folder with the settings the setup panel confirmed
    /// (#40, R2.1, R3.4).
    ///
    /// Unlike [`Self::add_folder`], this is an insert: a folder the client
    /// already tracks is [`EngineError::FolderExists`] and nothing is written,
    /// so the panel can say "Already added" and leave the stored settings
    /// alone. The tracking module decides, so the answer is the same whether
    /// or not this engine runs (R2.4).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::EmptyFolder`] for a blank path,
    /// [`EngineError::FolderExists`] when the client already tracks the path,
    /// and [`EngineError::Store`] when the write fails.
    pub fn add_folder_settings(&self, folder: &SourceFolder) -> Result<(), EngineError> {
        self.tracking.insert_folder(folder)?;
        Ok(())
    }

    /// Turns album mapping on or off for one source folder (R3.4).
    ///
    /// The mapping is opt-in per folder, so an unmapped folder sends no album
    /// request at all.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when the path is not one of this client's
    /// source folders, or when the store refuses the change.
    pub fn set_album_mapping(&self, path: &str, enabled: bool) -> Result<(), EngineError> {
        self.tracking.set_album_mapping(path, enabled)?;
        Ok(())
    }

    /// Sets the album scope for one source folder (R3.4).
    ///
    /// # Errors
    ///
    /// The same as [`Self::set_album_mapping`].
    pub fn set_album_scope(&self, path: &str, scope: AlbumScope) -> Result<(), EngineError> {
        self.tracking.set_album_scope(path, scope)?;
        Ok(())
    }

    /// Sets the title of one source folder's own album (#47).
    ///
    /// The title is stored with the folder's album row even while album
    /// mapping is off, and a blank one means the folder's name. An album that
    /// already exists is renamed by the retry the write wakes, so a failed or
    /// offline rename keeps the title and is tried again on the next scan,
    /// restart, or reconnect; a session that is signed out stores it and sends
    /// it once an engine runs.
    ///
    /// # Errors
    ///
    /// The same as [`Self::set_album_mapping`].
    pub fn set_album_title(&self, path: &str, title: &str) -> Result<(), EngineError> {
        self.tracking.set_album_title(path, title)?;
        Ok(())
    }

    /// Removes a source folder; its items stay in the store (R2.1).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when the store refuses the change.
    pub fn remove_folder(&self, path: &str) -> Result<(), EngineError> {
        self.tracking.remove_folder(path)?;
        Ok(())
    }

    /// The live status the dashboard and the tray read (R8.1, R8.2).
    ///
    /// This is also the meter's clock: the rate is measured from the status
    /// polls, so the card needs no timer of its own (R8.1).
    #[must_use]
    pub fn status(&self) -> Status {
        let now = Instant::now();
        let folders: Vec<FolderStatus> = self
            .inner
            .folders
            .lock()
            .expect("the folder lock")
            .iter()
            .map(|entry| {
                let traffic = entry.meter.reading(now);
                FolderStatus {
                    path: entry.folder.path.clone(),
                    recursive: entry.folder.recursive,
                    album_mapping: entry.folder.album_mapping,
                    album_scope: entry.folder.album_scope,
                    album_title: entry.folder.album_title.clone(),
                    album_rename: entry.album.rename_pending.then(|| {
                        entry
                            .album
                            .rename_problem
                            .as_ref()
                            .map_or(AlbumRename::Pending, |reason| {
                                AlbumRename::Failed(reason.clone())
                            })
                    }),
                    available: entry.available,
                    counts: entry.counts,
                    last_error: entry.last_error.clone(),
                    last_scan_ns: entry.last_scan_ns,
                    last_upload_ns: entry.last_upload_ns,
                    uploads: traffic,
                }
            })
            .collect();
        // R5.4: the review is counted from the store, not summed over the
        // current folders. A row under a removed source folder is still a
        // decision the user owes, so a folder sum would let the tray total
        // disagree with the list. On a refused read the sum is the fallback.
        let review_total = match self.inner.store.review_count() {
            Ok(total) => Some(total),
            Err(error) => {
                tracing::warn!(error = %error, "the review count could not be read");
                None
            }
        };
        Status::derive(StatusInputs {
            folders,
            paused: self.inner.gate.is_paused(),
            connection: ConnectionHealth::from_state(&self.inner.session.state()),
            review_total,
            last_error: self
                .inner
                .last_error
                .lock()
                .expect("the error lock")
                .clone(),
            // #56: the scan publishes itself here; the 250 ms poll is the
            // transport, so no notify and no timer is needed.
            scanning: self.inner.scanning.load(Ordering::SeqCst),
        })
    }

    /// The shared state store, so the shell can show store-backed data.
    #[must_use]
    pub fn store(&self) -> &Arc<StateStore> {
        self.tracking.store()
    }

    /// The session this engine sends its requests through (R1.4).
    #[must_use]
    pub fn session(&self) -> &SharedSession {
        &self.inner.session
    }

    /// The items the retry policy gave up on, in path order (R6.5).
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] when the query fails.
    pub fn failed_items(&self) -> Result<Vec<FailedItem>, StoreError> {
        self.tracking.failed_items()
    }

    /// The missing files that wait for a decision, in path order (R5.3).
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] when the query fails.
    pub fn review_items(&self) -> Result<Vec<ReviewItem>, StoreError> {
        self.tracking.review_items()
    }

    /// Keeps the server asset for one missing file and clears the item from
    /// the review (R5.3). Nothing is sent: the server already holds the asset.
    /// Returns whether an item was waiting for the decision.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] when the write fails.
    pub fn keep_review_item(&self, path: &str) -> Result<bool, StoreError> {
        self.tracking.keep_review(path)
    }

    /// Moves one missing file's asset to the trash and clears the item from
    /// the review (R5.3).
    ///
    /// The request carries no force, so the asset is trashed and never
    /// permanently deleted (ADR 0002). A refusal changes nothing: the asset
    /// stays where it is and the item stays in the review for the user to act
    /// on (R5.5).
    ///
    /// # Errors
    ///
    /// Returns [`ReviewError::NotInReview`] when the path is not waiting for a
    /// decision or has no server copy to trash, [`ReviewError::Client`] when
    /// the server refuses or cannot be reached, and [`ReviewError::Store`]
    /// when the decision cannot be recorded.
    pub async fn trash_review_item(&self, path: &str) -> Result<(), ReviewError> {
        let row = self
            .inner
            .store
            .item(path)?
            .filter(|row| row.state == ItemState::Missing)
            .ok_or_else(|| ReviewError::NotInReview(path.to_string()))?;
        let Some(asset_id) = row.asset_id else {
            // No server copy, so there is nothing to trash. A Trash click
            // must say so instead of recording a Keep the user did not ask
            // for (R5.5).
            return Err(ReviewError::NotInReview(path.to_string()));
        };
        let client = self.inner.session.client();
        let asset_ids = vec![asset_id.clone()];
        self.inner
            .session
            .request(move |credential| {
                let client = client.clone();
                let asset_ids = asset_ids.clone();
                async move { client.trash_assets(credential.auth(), &asset_ids).await }
            })
            .await?;
        if self
            .inner
            .store
            .decide_review(path, ReviewDecision::Trash)?
        {
            self.inner.note_review_decided(path);
            tracing::info!(path, asset_id, "R5.3: the asset moved to the trash");
        } else if self.inner.store.record_review_trash(path)? {
            // A Keep landed while the DELETE was in flight. The asset is gone
            // all the same, so the row records the trash instead of claiming a
            // keep; the count was already taken off by that Keep (R5.3).
            tracing::info!(
                path,
                asset_id,
                "R5.3: the asset moved to the trash while a keep was recorded"
            );
        }
        Ok(())
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
        self.tracking.retry_failed(path)
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
        // Detach first: a folder edit made while this engine stops must not
        // wait on the state it is tearing down.
        self.tracking.detach(&self.observer);
        self.stop();
    }
}

/// The engine's side of a [`Tracking`] change: the live folder entries, the
/// watcher, and the upload queue follow every write the shell makes.
struct EngineObserver {
    inner: Weak<EngineInner>,
}

impl Observer for EngineObserver {
    fn folder_added(&self, folder: &SourceFolder) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let key = path_key(Path::new(&folder.path));
        let mut folders = inner.folders.lock().expect("the folder lock");
        // The store is read under the lock that applies it. A change that
        // races this one commits before its own callback, and that callback
        // waits here, so the last callback to run applies a state at least as
        // new as every write it waited behind. An "added" whose folder a later
        // change removed finds no row, and drops itself.
        let stored = match stored_folder(&inner.store, &folder.path) {
            Ok(stored) => stored,
            Err(error) => {
                tracing::warn!(folder = %folder.path, error = %error, "the folders could not be read");
                return;
            }
        };
        let Some(stored) = stored else {
            return;
        };
        let available = Path::new(&stored.path).is_dir();
        let existing = folders
            .iter()
            .position(|entry| path_key(Path::new(&entry.folder.path)) == key);
        // #52: the store upserts the raw `path` and orders by that column, so a
        // re-add that spells the path differently can move the folder in the
        // list. For the order and the counts that is an insert.
        let respelled = existing.is_some_and(|index| folders[index].folder.path != stored.path);
        if let Some(index) = existing {
            // The whole row is taken, so the spelling the store wrote is
            // the one the live entry holds, and no field is left stale.
            let entry = &mut folders[index];
            entry.folder = stored.clone();
            entry.available = available;
            entry.album = album_of_store(&inner.store, &stored);
        } else {
            // R8.1: a folder new to this engine starts from what the store
            // already holds for it; a zeroed card would never fill, because
            // the rows are settled and `prepare` does not count them again.
            // #43: a folder new to this engine starts from the times
            // the store already holds for it.
            let times = inner.store.folder_times(&stored.path).unwrap_or_default();
            folders.push(FolderEntry {
                available,
                folder: stored.clone(),
                counts: FolderCounts::default(),
                last_error: None,
                last_scan_ns: times.last_scan_ns,
                last_upload_ns: times.last_upload_ns,
                // R8.1: every upload under this folder meters into this
                // one, under the folder's view of the client limit (R7.1).
                meter: Arc::new(UploadMeter::new(&inner.bandwidth)),
                // #47: a folder new to this engine starts from the title
                // the store already holds for it.
                album: album_of_store(&inner.store, &stored),
            });
        }
        if existing.is_none() || respelled {
            // N1: `store.folders()` orders by path, and the first-owner rule
            // follows that order, so the live list keeps it: a nested folder
            // added after the folder that contains it owns the same rows a
            // restart gives it.
            folders.sort_by(|left, right| left.folder.path.cmp(&right.folder.path));
        }
        // #52: a row moves cards only between a folder and one that contains
        // it or lies inside it. A re-spell with no such neighbour keeps every
        // live counter it has: the read would replace them with a store
        // snapshot that lags an upload already in flight.
        let moves_rows = respelled
            && folders.iter().any(|entry| {
                let other = path_key(Path::new(&entry.folder.path));
                other != key && (key_is_within(&other, &key) || key_is_within(&key, &other))
            });
        if existing.is_none() || moves_rows {
            // #52: the seed is the ownership read over this list. It scans the
            // table, so it runs with the folder lock released: a status poll
            // never queues behind it (N2).
            let ordered: Vec<SourceFolder> =
                folders.iter().map(|entry| entry.folder.clone()).collect();
            drop(folders);
            let counts = owned_counts(&inner, &ordered, &stored.path);
            folders = inner.folders.lock().expect("the folder lock");
            // The list the read counted: a later add or remove replaced it and
            // has a read of its own, whose answer is the newer one.
            if same_paths(&folders, &ordered)
                && let Some(counts) = counts
            {
                apply_owned_counts(&mut folders, &key, counts);
            }
        }
        drop(folders);
        // #47: a title stored before an album existed, or before this engine
        // ran, is sent now instead of at the next scan interval.
        inner.album_wake.notify_one();
        let mut watcher = inner.watcher.lock().expect("the watcher lock");
        let mut watch_error = None;
        if let Some(watcher) = watcher.as_mut()
            && available
        {
            // Re-adding an existing folder must move the watch to the new
            // mode. A recursive watch also watches every subfolder, and
            // re-watching the root alone leaves those behind, so the old
            // watch is removed first (R2.1). A folder that was never watched
            // (the folder did not exist, or the watcher is off) makes the
            // unwatch a no-op, and a failed unwatch may leave the old watch
            // running.
            let unwatched = match watcher.unwatch(Path::new(&stored.path)) {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(
                        folder = %stored.path,
                        error = %error,
                        "the old folder watch could not be removed"
                    );
                    false
                }
            };
            if let Err(error) = watcher.watch(Path::new(&stored.path), stored.recursive) {
                tracing::warn!(
                    folder = %stored.path,
                    error = %error,
                    "the folder could not be watched"
                );
                // Only a removed old watch leaves the folder unwatched, so
                // only then does the failure belong on the card (R2.1).
                if unwatched {
                    watch_error = Some(format!("the folder could not be watched: {error}"));
                }
            }
        }
        drop(watcher);
        if let Some(error) = watch_error {
            set_folder_error(&inner, &stored.path, error);
        }
        // Existing files in the new folder need discovery, not only future
        // watch events.
        inner.sync_now.notify_one();
    }

    fn folder_changed(&self, folder: &SourceFolder) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let key = path_key(Path::new(&folder.path));
        let mut folders = inner.folders.lock().expect("the folder lock");
        let Ok(Some(stored)) = stored_folder(&inner.store, &folder.path) else {
            return;
        };
        if let Some(entry) = folders
            .iter_mut()
            .find(|entry| path_key(Path::new(&entry.folder.path)) == key)
        {
            entry.album = album_of_store(&inner.store, &stored);
            entry.folder = stored;
        }
        drop(folders);
        // #47: the change may be an album title, which the server has to be
        // told about. A mapping or scope change queues nothing, so the retry
        // finds no pending row and sends no request.
        inner.album_wake.notify_one();
    }

    fn folder_removed(&self, path: &str) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let key = path_key(Path::new(path));
        let mut folders = inner.folders.lock().expect("the folder lock");
        let Ok(stored) = stored_folder(&inner.store, path) else {
            return;
        };
        if stored.is_some() {
            // Re-added since: the add callback owns the entry.
            return;
        }
        folders.retain(|entry| path_key(Path::new(&entry.folder.path)) != key);
        // #52: the removed folder's rows move to another card only when a
        // remaining folder contains it or lies inside it. Otherwise every live
        // counter stays as it is: a read taken while an upload is in flight
        // would replace it with an older row.
        let related = folders.iter().any(|entry| {
            let other = path_key(Path::new(&entry.folder.path));
            key_is_within(&other, &key) || key_is_within(&key, &other)
        });
        if related {
            let ordered: Vec<SourceFolder> =
                folders.iter().map(|entry| entry.folder.clone()).collect();
            // The read scans the table, so it runs with the folder lock
            // released: a status poll never queues behind it (N2).
            drop(folders);
            let counts = owned_counts(&inner, &ordered, path);
            folders = inner.folders.lock().expect("the folder lock");
            // The list the read counted: a later add or remove replaced it and
            // has a read of its own, whose answer is the newer one.
            if same_paths(&folders, &ordered)
                && let Some(counts) = counts
            {
                apply_owned_counts(&mut folders, &key, counts);
            }
        }
        drop(folders);
        let mut watcher = inner.watcher.lock().expect("the watcher lock");
        if let Some(watcher) = watcher.as_mut() {
            let _ = watcher.unwatch(Path::new(path));
        }
    }

    fn review_decided(&self, path: &str) {
        if let Some(inner) = self.inner.upgrade() {
            inner.note_review_decided(path);
        }
    }

    fn item_requeued(&self, path: &str) {
        if let Some(inner) = self.inner.upgrade() {
            let _ = inner.queue.send(PathBuf::from(path));
        }
    }
}

impl EngineInner {
    /// The upload pool in force right now (R7.2).
    ///
    /// Records a request the server answered: the run of failures the breaker
    /// counts ends, and a connection that only needed attention because the
    /// server was unreachable leaves that state (R6.6, R8.2).
    async fn note_reachable(&self) {
        self.breaker.record_success();
        self.session.note_reachable().await;
    }

    /// A resize swaps the pool, so an attempt takes its permit from the pool
    /// that was current when the attempt started, and a later attempt takes
    /// one from whatever pool is current then.
    fn upload_pool(&self) -> Arc<Semaphore> {
        Arc::clone(&self.upload_slots.lock().expect("the upload slots"))
    }

    /// Queues the settled paths the watcher reported, after the shared skip
    /// rules (R2.2). A path that disappeared is recorded and reconciled by the
    /// scan loop (R5.1).
    fn enqueue_watch_batch(&self, paths: Vec<PathBuf>) {
        // A change in a folder makes its cached listing stale, whether or not
        // the changed path itself is queued (a sidecar is not an item, but it
        // still pairs with the media file in the same batch). R3.3.
        self.index_cache
            .lock()
            .expect("the index cache lock")
            .invalidate(&paths);
        for path in paths {
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                // The path is gone. Whether that is a missing file, a rename,
                // or a second copy with the same content is decided in one
                // place, by the reconciliation scan; the watcher only asks for
                // one. The store is not read here: this callback runs on the
                // debouncer's thread and must not block (R5.1, R5.2).
                self.absences.lock().expect("the absence list").push(path);
                self.reconcile.notify_one();
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

    /// The meter of the folder that owns `path` (R8.1).
    ///
    /// The owning folder is the first entry that contains the path, as
    /// [`folder_index_in`] decides, so the path is metered once.
    ///
    /// #52: the counts seed by the same rule
    /// ([`crate::store::StateStore::folder_counts_by_owner`]), so a nested
    /// source folder's card and its meter name one owner.
    ///
    /// A path no folder owns — its folder was removed while the item was
    /// queued — is metered on no card, but its bytes still count in the
    /// client's total (R10.2) and stream under the client's limit (R7.1).
    fn meter_for(&self, path: &Path) -> Arc<UploadMeter> {
        // One lock for the owner and its meter. Resolving the owner under one
        // lock and reading the meter under another lets a removal shift the
        // entries in between, and the upload is metered on the card of a
        // folder that never owned it (R8.1).
        let folders = self.folders.lock().expect("the folder lock");
        match folder_index_in(&folders, path) {
            Some(index) => Arc::clone(&folders[index].meter),
            None => Arc::new(UploadMeter::new(&self.bandwidth)),
        }
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
    /// A missing row whose file came back leaves the review the same way
    /// (R5.1), so the tray's count follows it down.
    fn note_pending(&self, path: &Path, previous: Option<ItemState>) {
        // #48: one guard for the owner and its counts. Resolving the owner under
        // one lock and indexing the live list under another lets a removal move
        // the entries in between, so the file is counted on another folder's
        // card, or past the end of the shortened list.
        let mut folders = self.folders.lock().expect("the folder lock");
        let Some(entry) = folder_entry_owning(&mut folders, path) else {
            return;
        };
        let counts = &mut entry.counts;
        match previous {
            Some(ItemState::RetryWait) => counts.failed = counts.failed.saturating_sub(1),
            Some(ItemState::Missing) => counts.review = counts.review.saturating_sub(1),
            _ => {}
        }
        counts.pending += 1;
    }

    /// Records that one item left the review by a decision (R5.3, R5.4).
    fn note_review_decided(&self, path: &str) {
        let mut folders = self.folders.lock().expect("the folder lock");
        if let Some(entry) = folder_entry_owning(&mut folders, Path::new(path)) {
            entry.counts.review = entry.counts.review.saturating_sub(1);
        }
    }

    /// Records that one item entered the review, raising the tray count
    /// (R5.4).
    fn note_review_missing(&self, path: &str) {
        let mut folders = self.folders.lock().expect("the folder lock");
        if let Some(entry) = folder_entry_owning(&mut folders, Path::new(path)) {
            entry.counts.review += 1;
        }
    }

    /// Drains the paths the watcher reported gone, so the scan loop can ask
    /// the store whether any of them is tracked (R5.1).
    fn drain_absences(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.absences.lock().expect("the absence list"))
    }

    /// Records one finished item against its folder and releases its claim.
    fn finish_item(&self, item: &PreparedItem, outcome: &UploadOutcome) {
        // #43: the owning folder is resolved once, by path, and the store write
        // and the live update both name it that way. `remove_folder` retains
        // the list, so an index cannot survive between the two.
        let folder_path = {
            let folders = self.folders.lock().expect("the folder lock");
            folder_index_in(&folders, &item.path).map(|index| folders[index].folder.path.clone())
        };
        // #43: Created and Duplicate are both terminal successes (R3.2), and
        // the folder detail shows when this folder last uploaded one. The store
        // write runs before the folder lock is taken, so it can never hold the
        // lock the status read needs (N2).
        let uploaded_ns = match outcome {
            UploadOutcome::Created { .. } | UploadOutcome::Duplicate { .. } => folder_path
                .as_deref()
                .map(|folder| record_folder_time(self, folder, FolderMoment::Upload)),
            _ => None,
        };
        if let Some(folder) = folder_path.as_deref() {
            let mut folders = self.folders.lock().expect("the folder lock");
            if let Some(entry) = folder_entry_mut(&mut folders, folder) {
                match outcome {
                    UploadOutcome::Created { .. } | UploadOutcome::Duplicate { .. } => {
                        entry.counts.pending = entry.counts.pending.saturating_sub(1);
                        entry.counts.uploaded += 1;
                        // R8.2: a later success clears the parked error, so a
                        // transient failure does not read as attention for a full
                        // scan interval.
                        entry.last_error = None;
                        if let Some(at_ns) = uploaded_ns {
                            entry.last_upload_ns = Some(at_ns);
                        }
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
            // R8.2: the tray card reads the global copy, so a success that
            // recovers every folder must clear it too.
            clear_global_error_if_recovered(self, &folders);
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
        let mut folders = self.folders.lock().expect("the folder lock");
        if let Some(entry) = folder_entry_owning(&mut folders, &item.path) {
            entry.last_error = Some(failure_text(failure));
        }
    }

    /// Parks one item in the dead letter after the attempt cap (R6.5).
    ///
    /// The last reason the store holds is kept when this call has no failure of
    /// its own, which is how a restart that finds the cap already spent still
    /// shows why the item is parked. The asset id the row already knows is
    /// kept too, the same rule `park_row` follows: a promote that failed after
    /// the upload landed put it there, and the dead letter must not claim the
    /// server holds nothing (R4.4). An upload that never reached the server
    /// wrote `None`, so the row still claims no asset that does not exist.
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
            asset_id: existing.as_ref().and_then(|row| row.asset_id.clone()),
            state: ItemState::DeadLetter,
            error_class,
            error_message,
            attempts: 0,
            // A parked item owes a sidecar the row does not record, and the
            // request that would have carried it never went out. A digest the
            // row already has is kept: a promote failure parked the item
            // after the upload read the sidecar, and a manual retry (R6.5)
            // must not offer it again.
            sidecar: existing.as_ref().and_then(|row| row.sidecar),
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

    /// Records one batch-level failure against every item of the batch.
    ///
    /// A refused credential failed as a whole, not once per item (R1.4):
    /// nothing is written, so the rows stay `pending` and the next scan or
    /// sign-in queues them again. The session may still read `Connected` when
    /// the refusal is below the attention threshold, so the refusal itself is
    /// the signal. Parking a batch of them would leave every file looking
    /// permanently broken to a sign-in that fixes it.
    fn fail_batch(&self, prepared: &[PreparedItem], error: &ClientError) {
        // A refusal the session could not answer is the credential's problem,
        // not the files': the state may still read `Connected` while the
        // refusal is below the attention threshold (R1.4).
        let refused = matches!(error, ClientError::InvalidToken);
        if !self.session.state().is_connected() || refused {
            tracing::warn!(
                batch = prepared.len(),
                reason = %error.redacted_reason(),
                "the connection needs the user, so the batch stays pending for the next scan"
            );
            for item in prepared {
                self.release_item(item);
            }
            return;
        }
        let failure = UploadFailure::from_client_error(error);
        for item in prepared {
            self.fail_item(item, &failure, None);
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
    fn fail_item(&self, item: &PreparedItem, failure: &UploadFailure, asset_id: Option<&str>) {
        let outcome = self.park_row(item, failure, asset_id);
        self.finish_item(item, &outcome);
        tracing::warn!(
            path = %item.path.display(),
            class = failure.class.map_or("none", FailureClass::as_str),
            reason = %failure.message,
            "the file was parked after a failure"
        );
    }

    /// Records one failure without finishing the item, so a caller that still
    /// holds the item (the retry loop) keeps its own bookkeeping.
    ///
    /// `asset_id` is the asset the server already holds for this item, when the
    /// failure happened after the upload landed. It is kept so the asset is
    /// never lost: a later attempt resolves the same bytes to the same asset
    /// (R3.2) and finishes the work that failed (R4.4). An upload that never
    /// reached the server passes `None`, because the row must not claim an
    /// asset that does not exist.
    fn park_row(
        &self,
        item: &PreparedItem,
        failure: &UploadFailure,
        asset_id: Option<&str>,
    ) -> UploadOutcome {
        let state = match failure.kind {
            crate::failure::FailureKind::Permanent => ItemState::FailedPermanent,
            crate::failure::FailureKind::Transient => ItemState::RetryWait,
        };
        let path = item.path.to_string_lossy().into_owned();
        // R3.3, R4.5: a failure sends nothing, so a sidecar that was owed is
        // still owed; but a promote failure happens after the upload landed
        // and recorded the sidecar it carried, and erasing that digest makes
        // the retry re-send the sidecar and park with a false "could not be
        // attached". The row's last word on what the asset carries is kept.
        let sidecar = self
            .store
            .item(&path)
            .ok()
            .flatten()
            .and_then(|row| row.sidecar);
        let record = ItemRecord {
            path,
            size: item.size,
            mtime_ns: item.mtime_ns,
            sha1: item.sha1,
            asset_id: asset_id.map(str::to_string),
            state,
            error_class: failure.class,
            error_message: Some(failure.message.clone()),
            attempts: 0,
            sidecar,
        };
        if let Err(store_error) = self.store.record(&record) {
            tracing::error!(error = %store_error, "the state database could not record the failure");
        }
        UploadOutcome::Failed(failure.clone())
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
                inner.note_reachable().await;
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

/// Adds a finished asset to the albums its folder maps to (R3.4).
///
/// The upload is already recorded when this runs, so a failure here never costs
/// the upload: the row keeps its terminal state and its asset mapping. What is
/// lost is the membership, and the only trace of the loss is the folder's last
/// error: it lives in memory, `scan_once` clears it on the next successful
/// scan, and `Engine::start` starts every folder with none, so within one scan
/// interval or any restart the asset is on the server and in no album with
/// nothing durable saying so. The retry policy (#7) owns making that durable
/// and re-attempting the album call.
async fn attach_albums(inner: &Arc<EngineInner>, file: &Path, asset_id: &str) {
    let Some(folder) = folder_entry_for(inner, file) else {
        return;
    };
    match inner.albums.attach(&folder, file, asset_id).await {
        // R3.4: an unmapped folder maps no album, and sends no request.
        Ok(None) => {}
        Ok(Some(album)) => {
            tracing::debug!(path = %file.display(), album = %album, "the asset joined its album");
        }
        Err(error) => {
            tracing::warn!(
                path = %file.display(),
                folder = %folder.path,
                reason = %error.redacted_reason(),
                "the asset is on the server, but album mapping failed"
            );
            set_folder_error(
                inner,
                &folder.path,
                format!("album mapping failed: {error}"),
            );
        }
    }
}

/// The position in `folders` of the entry that contains `path` (R8.1).
fn folder_index_in(folders: &[FolderEntry], path: &Path) -> Option<usize> {
    let case = PathCase::for_platform();
    folders
        .iter()
        .position(|entry| is_within(Path::new(&entry.folder.path), path, case))
}

/// The source folder that contains `path`, as stored.
fn folder_entry_for(inner: &EngineInner, path: &Path) -> Option<SourceFolder> {
    // #48: the owner is resolved under the guard that clones it, so a removal
    // cannot hand back another folder's entry.
    let folders = inner.folders.lock().expect("the folder lock");
    let index = folder_index_in(&folders, path)?;
    folders.get(index).map(|entry| entry.folder.clone())
}

/// Prepares one batch and checks it with one `bulk-upload-check` (R3.1).
async fn process_batch(inner: &Arc<EngineInner>, batch: Vec<PathBuf>) {
    // R3.3: the pairing rules need the folders' other files. A scan pass
    // shares one listing per folder across its batches, so a 10k-file folder
    // is walked once per scan instead of once per batch.
    let index = batch_index(inner, &batch).await;
    let mut prepared = Vec::with_capacity(batch.len());
    for path in batch {
        if let Some(item) = prepare(inner, &path, &index).await {
            prepared.push(item);
        }
    }
    // R3.3: the photo uploads its motion video first and links it, so a video a
    // photo in this batch claims is not also uploaded as an item of its own.
    // The membership test folds case like the rest of the engine (R2.4): the
    // walker and the watcher spell the same file differently on Windows.
    let claimed: HashSet<String> = prepared
        .iter()
        .filter_map(|item| item.live_photo_video.as_deref())
        .map(path_key)
        .collect();
    prepared.retain(|item| {
        if claimed.contains(&path_key(&item.path)) {
            // A claim is a borrow, not a deletion: the item keeps its pending
            // count and its `pending` row, and the next scan uploads it as an
            // ordinary asset — or finds it already sent by the photo that
            // claimed it. Releasing it is what stops the in-flight claim from
            // making it un-preparable for the rest of the process.
            inner.release_item(item);
            return false;
        }
        true
    });
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
                inner.note_reachable().await;
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
            Some(action) => match check_outcome(&action) {
                // R3.3: a duplicate whose asset is live still has to carry a
                // sidecar the server does not have, so the upload runs; the
                // server answers duplicate again and no second asset is
                // created. Without one, the row settles through the version
                // flow.
                CheckOutcome::Duplicate { asset_id } if item.sidecar.is_none() => {
                    settle_duplicate(inner, &item, &asset_id).await;
                }
                CheckOutcome::Duplicate { .. } | CheckOutcome::Upload => {
                    spawn_upload(inner, item).await;
                }
                CheckOutcome::Restore { asset_id } => {
                    // R5.3: the bytes are in the trash. Nothing can upload
                    // them back - the checksum header answers duplicate even
                    // then - so the asset is restored. A sidecar still cannot
                    // attach to an asset that already exists, so it follows
                    // the ordinary duplicate upload path and parks with its
                    // reason.
                    match restore_asset(inner, &asset_id).await {
                        Ok(()) if item.sidecar.is_none() => {
                            settle_duplicate(inner, &item, &asset_id).await;
                        }
                        Ok(()) => spawn_upload(inner, item).await,
                        Err(error) => {
                            inner.fail_item(
                                &item,
                                &UploadFailure::from_client_error(&error),
                                Some(&asset_id),
                            );
                        }
                    }
                }
                CheckOutcome::Refused(error) => {
                    inner.fail_item(&item, &UploadFailure::from_client_error(&error), None);
                }
            },
            None => {
                inner.fail_item(
                    &item,
                    &UploadFailure::from_client_error(&missing_result_error()),
                    None,
                );
            }
        }
    }
}

/// Settles an item whose bytes the server already holds under `asset_id`
/// (R3.2, R4.2).
///
/// The version flow runs first: bytes the server holds are still a new
/// version of a tracked file. A promote failure parks the row with its asset
/// id, and the next scan or "Sync now" finishes the chain (R6.7). Then the
/// row is terminal and the asset joins its folder's album.
async fn settle_duplicate(inner: &Arc<EngineInner>, item: &PreparedItem, asset_id: &str) {
    let id = item.path.to_string_lossy().into_owned();
    if let Err(error) = inner.versions.promote(&id, asset_id, item.sha1).await {
        inner.fail_item(item, &error.failure(), Some(asset_id));
        return;
    }
    record_terminal(inner, item, asset_id, ItemState::Duplicate);
    inner.finish_item(
        item,
        &UploadOutcome::Duplicate {
            asset_id: asset_id.to_string(),
        },
    );
    // R3.4: a duplicate is a success (R3.2), and it still maps to its
    // folder's album.
    attach_albums(inner, &item.path, asset_id).await;
    // R3.7: the sync tag follows the settled asset. It runs last and its
    // failure is the tagger's own to record: a tag failure must not fail or
    // park an upload the server accepted.
    let _ = inner.tags.tag(asset_id).await;
}

/// Restores one trashed asset through the shared session (R5.3).
async fn restore_asset(inner: &Arc<EngineInner>, asset_id: &str) -> Result<(), ClientError> {
    let client = inner.session.client();
    let ids = vec![asset_id.to_string()];
    inner
        .session
        .request(move |credential| {
            let client = client.clone();
            let ids = ids.clone();
            async move { client.restore_assets(credential.auth(), &ids).await }
        })
        .await
}

/// The folder listings one batch needs, read into the scan's shared cache on
/// a blocking thread (R3.3).
async fn batch_index(inner: &Arc<EngineInner>, batch: &[PathBuf]) -> FolderIndex {
    let missing = {
        let cache = inner.index_cache.lock().expect("the index cache lock");
        cache.missing_paths(batch)
    };
    if !missing.is_empty() {
        let read = tokio::task::spawn_blocking(move || FolderIndex::read(&missing))
            .await
            .unwrap_or_default();
        inner
            .index_cache
            .lock()
            .expect("the index cache lock")
            .merge(read);
    }
    let cache = inner.index_cache.lock().expect("the index cache lock");
    cache.for_paths(batch)
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
    let path = item.path.to_string_lossy().into_owned();
    // R4.5: this branch only runs when no sidecar was owed, but the row may
    // already record one - a promote failure or a restart can leave a digest
    // the settled row must not erase, or the next scan offers the sidecar
    // again and answers `duplicate` for an asset that already carries it.
    let sidecar = inner
        .store
        .item(&path)
        .ok()
        .flatten()
        .and_then(|row| row.sidecar);
    let record = ItemRecord {
        path,
        size: item.size,
        mtime_ns: item.mtime_ns,
        sha1: item.sha1,
        asset_id: Some(asset_id.to_string()),
        state,
        error_class: None,
        error_message: None,
        attempts: 0,
        sidecar,
    };
    if let Err(error) = inner.store.record(&record) {
        tracing::error!(error = %error, "the state database could not record the outcome");
    }
}

/// Hashes one discovered path, skips what is already known, and records the
/// pending row the durable queue needs (R6.7).
///
/// The folder index resolves what belongs to the asset rather than to a file of
/// its own (R3.3): the XMP sidecar to send, and the live photo video to upload
/// first. A sidecar the asset does not carry yet is work even when the media
/// file itself is settled, so a sidecar written while the client was stopped
/// rides along with the next upload — unless the asset is already on the
/// server, which no Stable operation can change (see `upload::pending_sidecar`).
async fn prepare(
    inner: &Arc<EngineInner>,
    path: &Path,
    index: &FolderIndex,
) -> Option<PreparedItem> {
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
    let case = PathCase::for_platform();
    let sidecar = pending_sidecar(path, index, case, existing.as_ref()).await;
    if let Some(row) = &existing
        && row.size == size
        && row.mtime_ns == mtime_ns
        && is_settled(row.state)
        && sidecar.is_none()
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
        && sidecar.is_none()
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
        live_photo_video: index
            .live_photo_video_for(path, case)
            .map(Path::to_path_buf),
        sidecar,
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
        // R4.5: the digest the row already recorded is what the asset
        // carries, even though this new attempt has not sent anything yet. A
        // crash here re-queues the item and sends any newer sidecar again
        // (R6.7).
        sidecar: existing.as_ref().and_then(|row| row.sidecar),
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

/// Whether a recorded row is finished on its own: a terminal success, a
/// failure no automatic retry can fix, or a decision the user already made.
/// A pending, uploading, `retry_wait`, `missing`, or `review_trash` row is
/// still work the pipeline owns, so a scan may pick it up again.
///
/// A `review_trash` row is deliberately not settled: the local file can come
/// back, and the asset it maps to can be restored, so the path must be
/// re-discovered rather than skipped (R5.3).
fn is_settled(state: ItemState) -> bool {
    matches!(
        state,
        ItemState::Uploaded
            | ItemState::Duplicate
            | ItemState::FailedPermanent
            | ItemState::DeadLetter
            | ItemState::ReviewKeep
    )
}

/// Hands one accepted file to the upload pool (R7.2).
///
/// The pool's permit is taken inside [`upload_with_retries`], around one
/// attempt, so a worker sleeping its backoff holds no slot: lowering the
/// concurrency limit drains the pool instead of waiting out the sleep.
async fn spawn_upload(inner: &Arc<EngineInner>, item: PreparedItem) {
    if inner.gate.is_paused() {
        inner.finish_item(&item, &UploadOutcome::Deferred);
        return;
    }
    let inner = Arc::clone(inner);
    let mut uploads = inner.uploads.lock().expect("the upload list");
    if inner.stopped.load(Ordering::SeqCst) {
        // `stop` ran first: the item stays pending for the next engine (R6.7).
        inner.release_item(&item);
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
                // R3.4: the asset joins the album its folder maps to. This runs
                // after `finish_item`, so a later success cannot clear the error
                // an album failure records against the folder.
                attach_albums(&worker, &item.path, asset_id).await;
                // R3.7: the sync tag follows the settled asset. A failure is
                // recorded for Settings and never reaches the item's state.
                let _ = worker.tags.tag(asset_id).await;
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
            return inner.park_row(item, &UploadFailure::from_client_error(&error), None);
        }
        if attempts >= cap {
            // The cap was already spent before this run, so the item is parked
            // without a request (R6.5, R6.7).
            return inner.park_dead_letter(item, None);
        }
        // R7.2: one permit per attempt, released before the backoff sleep so
        // a lowered limit can drain the pool without waiting it out. A closed
        // pool has no worker to take the item, so it stays queued for the
        // next engine (R6.7).
        let permit = match inner.upload_pool().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                inner.release_item(item);
                return UploadOutcome::Deferred;
            }
        };
        let started_ns = now_ns();
        attempts += 1;
        let meter = inner.meter_for(&item.path);
        let mut outcome = inner
            .uploader
            .upload_prepared(item, &inner.gate, &meter)
            .await;
        // R4.2: an upload that reached the server is a new version of the file
        // when the item already has one, so the associations carry over before
        // the item counts as finished. A failure here keeps the uploaded asset
        // on the row and spends this attempt, which is what the retry loop and
        // the dead letter are for (R6.1, R6.5).
        if let UploadOutcome::Created { asset_id } | UploadOutcome::Duplicate { asset_id } =
            &outcome
            && let Err(error) = inner
                .versions
                .promote(&path_text, asset_id, item.sha1)
                .await
        {
            let failure = error.failure();
            inner.park_row(item, &failure, Some(asset_id));
            outcome = UploadOutcome::Failed(failure);
        }
        // The attempt is over; a later one takes its own permit.
        drop(permit);
        let (attempt_outcome, failure) = match &outcome {
            UploadOutcome::Created { .. } | UploadOutcome::Duplicate { .. } => {
                // R6.6: a request the server answered ends the run of
                // connection failures, so one unlucky request is not a run.
                inner.note_reachable().await;
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
/// and the version chain record it.
pub(crate) fn now_ns() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

/// Runs the sync tag's backfill whenever the setting or "Tag existing assets
/// now" asks for it (R3.7).
///
/// One task owns the whole backfill, so a library of any size costs one loop and
/// one request per batch instead of a task per asset, and uploads keep the pool
/// they would have without it. Each wake resolves the tag again, which is what a
/// deleted and recreated tag needs, and then walks the library from the cursor
/// the state database recorded.
async fn tag_backfill_loop(inner: Arc<EngineInner>) {
    loop {
        inner.tag_wake.notified().await;
        inner.tags.prepare().await;
        inner.backfill.run(&inner.tags).await;
    }
}

/// Sends the album titles the user changed, and keeps the folders' published
/// state in step with the album rows (#47).
///
/// It is woken by a folder change and by the scan, never by a status poll, so a
/// server that keeps refusing a rename is asked once per event instead of once
/// per frame.
async fn album_retry_loop(inner: Arc<EngineInner>) {
    loop {
        inner.album_wake.notified().await;
        settle_album_titles(&inner).await;
    }
}

/// Sends every album title that is still pending, then republishes what the
/// album rows now hold (#47).
async fn settle_album_titles(inner: &Arc<EngineInner>) {
    inner.albums.settle_titles().await;
    refresh_album_rows(inner);
}

/// Re-reads every folder's album row into its live entry (#47).
///
/// The retry writes the rows it settles, so the status a signed-in or
/// signed-out client shows follows them without a database read per poll.
fn refresh_album_rows(inner: &EngineInner) {
    let folders = match inner.store.folders() {
        Ok(folders) => folders,
        Err(error) => {
            tracing::warn!(error = %error, "the folders could not be read");
            return;
        }
    };
    let mut entries = inner.folders.lock().expect("the folder lock");
    for folder in folders {
        let key = path_key(Path::new(&folder.path));
        let album = album_of_store(&inner.store, &folder);
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| path_key(Path::new(&entry.folder.path)) == key)
        {
            entry.album = album;
            entry.folder.album_title = folder.album_title;
        }
    }
}

/// The album row of one folder, or the default when it holds none or cannot be
/// read (#47).
fn album_of_store(store: &StateStore, folder: &SourceFolder) -> Album {
    let key = path_key(Path::new(&folder.path));
    match store.album(&key) {
        Ok(album) => album.unwrap_or_default(),
        Err(error) => {
            tracing::warn!(folder = %folder.path, error = %error, "the album row could not be read");
            Album::default()
        }
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
            () = inner.reconcile.notified() => {}
        }
        // A scan is expensive, so the watcher's absences buy one only when
        // they name something the client tracks; a temporary file that came
        // and went does not walk every source folder.
        let absences = inner.drain_absences();
        let tracked = absences.iter().any(|path| {
            inner
                .store
                .item(&path.to_string_lossy())
                .ok()
                .flatten()
                .is_some()
        });
        if !absences.is_empty() && !tracked {
            continue;
        }
        scan_once(&inner).await;
    }
}

/// Walks every source folder and queues what it finds (R2.3), then reconciles
/// what disappeared (R5.1-R5.3).
///
/// The walk of every folder happens before the reconciliation, so a rename
/// across folders is seen: the reconciliation has the whole picture, not one
/// folder at a time.
async fn scan_once(inner: &Arc<EngineInner>) {
    // #56: the dashboard reads this while the pass walks and after it queues,
    // so a click that finds nothing still shows the scan it ran. The guard
    // clears it on every way out, including a task abort or an unwind; a
    // stopped engine's scan dies with the task, so the flag is never read
    // afterwards.
    inner.scanning.store(true, Ordering::SeqCst);
    let _scanning = ScanInProgress(&inner.scanning);
    // R3.3: a new pass pairs against fresh listings, not the previous pass's.
    *inner.index_cache.lock().expect("the index cache lock") = FolderIndex::default();
    let folders: Vec<SourceFolder> = inner
        .folders
        .lock()
        .expect("the folder lock")
        .iter()
        .map(|entry| entry.folder.clone())
        .collect();
    let mut reports: Vec<(SourceFolder, crate::scan::ScanReport)> = Vec::new();
    for folder in folders {
        let report = tokio::task::spawn_blocking({
            let folder = folder.clone();
            move || scan_folder(&folder)
        })
        .await;
        match report {
            Ok(Ok(report)) => {
                set_folder_available(inner, &folder.path, true);
                // #43: the folder detail shows when the folder last scanned;
                // this is the one pass that read it.
                note_folder_scanned(inner, &folder.path);
                reports.push((folder, report));
            }
            Ok(Err(ScanError::Missing(path))) => {
                set_folder_available(inner, &path, false);
                tracing::info!(folder = %path, "the source folder is not available");
            }
            Ok(Err(ScanError::Denied(path))) => {
                // #65 (R2.5): a folder the operating system refuses says so and
                // carries the setting that lifts it. It is not an empty folder,
                // and nothing retries it: the denial lasts until the user
                // changes System Settings.
                set_folder_available(inner, &path, false);
                set_folder_error(inner, &path, crate::scan::PERMISSION_DENIED);
                tracing::info!(
                    folder = %path,
                    "the source folder needs the Files & Folders permission"
                );
            }
            Ok(Err(ScanError::Walk(error))) => {
                set_folder_error(inner, &folder.path, error);
            }
            Err(join) => {
                set_folder_error(inner, &folder.path, join.to_string());
            }
        }
    }
    // R5.1: a scan that could not read a folder says nothing about what is
    // missing under it, so only the folders that answered are reconciled
    // (R2.5).
    let present: Vec<PathBuf> = reports
        .iter()
        .flat_map(|(_, report)| report.files.iter().cloned())
        .collect();
    let scanned: Vec<SourceFolder> = reports.iter().map(|(folder, _)| folder.clone()).collect();
    if !scanned.is_empty() {
        reconcile_files(inner, &present, &scanned).await;
    }
    let mut queued = 0_usize;
    for (_, report) in reports {
        queued += report.files.len();
        for file in report.files {
            let _ = inner.queue.send(file);
        }
    }
    // #47: a scan is the one event that runs on a start, a reconnect, and the
    // interval, so a title the server did not take is sent here rather than
    // from the upload path or a timer of its own.
    settle_album_titles(inner).await;
    tracing::info!(queued, "the reconciliation scan finished");
}

/// Clears the scan flag when it leaves scope, so every exit from [`scan_once`]
/// publishes the end of the scan (#56).
struct ScanInProgress<'a>(&'a AtomicBool);

impl Drop for ScanInProgress<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Decides what each tracked disappearance under the scanned folders is
/// (R5.1-R5.3): a rename, a second copy, or a missing file that waits for a
/// decision.
async fn reconcile_files(inner: &Arc<EngineInner>, present: &[PathBuf], folders: &[SourceFolder]) {
    let mut gone: Vec<ItemRecord> = Vec::new();
    for folder in folders {
        let candidates = match inner.store.review_candidates(&folder.path) {
            Ok(candidates) => candidates,
            Err(error) => {
                tracing::warn!(folder = %folder.path, error = %error, "the review candidates could not be read");
                continue;
            }
        };
        for row in candidates {
            // The filesystem, not the scan report, is the test: a file the
            // skip rules refused this pass, or one a non-recursive folder no
            // longer walks, is still there and is not missing.
            if is_gone(&row.path) {
                gone.push(row);
            }
        }
    }
    if gone.is_empty() {
        return;
    }
    // A candidate hashed here is read whole, so the pass runs on the blocking
    // pool where the scan's walk runs; its store writes are short.
    let present = present.to_vec();
    let inner = Arc::clone(inner);
    let joined = tokio::task::spawn_blocking(move || reconcile_gone(&inner, gone, &present)).await;
    if let Err(join) = joined {
        tracing::warn!(error = %join, "the missing-file reconciliation did not finish");
    }
}

/// Runs the rename pass, then the twin-or-missing pass over what it left
/// undecided (R5.1, R5.2).
fn reconcile_gone(inner: &EngineInner, gone: Vec<ItemRecord>, present: &[PathBuf]) {
    let index = SizeIndex::new(present);
    let host = EngineHost { inner };
    let mut undecided = Vec::new();
    for row in gone {
        let Some(target) = rename_target(&row, &index, &host) else {
            undecided.push(row);
            continue;
        };
        let Ok(metadata) = std::fs::symlink_metadata(&target) else {
            undecided.push(row);
            continue;
        };
        let moved = inner.store.move_item(
            &row.path,
            &target.to_string_lossy(),
            row.size,
            mtime_ns(&metadata),
        );
        match moved {
            Ok(true) => {
                if row.state == ItemState::Missing {
                    // The item found its content again, so it leaves the review
                    // (R5.4).
                    inner.note_review_decided(&row.path);
                }
                tracing::info!(
                    from = %row.path,
                    to = %target.display(),
                    "R5.2: the item was renamed and keeps its asset mapping"
                )
            }
            // A target another item claimed in this same pass: the row is
            // still gone, and the twin check below decides it.
            Ok(false) => undecided.push(row),
            Err(error) => {
                tracing::warn!(path = %row.path, error = %error, "the renamed item could not be moved");
                undecided.push(row);
            }
        }
    }
    for row in undecided {
        // A second copy of the same content, still on disk: the asset is held
        // by another item, so no decision is needed (the SPEC acceptance line
        // for a second instance).
        match fate(&row, &host) {
            Fate::Retired => match inner.store.retire_item(&row.path) {
                Ok(true) => {
                    if row.state == ItemState::Missing {
                        // The retired item no longer waits for a decision, so
                        // the tray count follows it down (R5.4).
                        inner.note_review_decided(&row.path);
                    }
                    tracing::info!(
                        path = %row.path,
                        "R5.2: another item still holds this content, so no review is needed"
                    );
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(path = %row.path, error = %error, "the twin copy could not be retired")
                }
            },
            Fate::Missing => match inner.store.mark_missing(&row.path) {
                Ok(true) => {
                    inner.note_review_missing(&row.path);
                    tracing::info!(path = %row.path, "R5.1: the file is missing and waits for a review");
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(path = %row.path, error = %error, "the missing file could not be recorded")
                }
            },
        }
    }
}

/// The engine's answer to what [`Host`] asks (R5.1, R5.2).
struct EngineHost<'a> {
    inner: &'a EngineInner,
}

impl Host for EngineHost<'_> {
    fn is_tracked(&self, path: &Path) -> bool {
        self.inner
            .store
            .item(&path.to_string_lossy())
            .ok()
            .flatten()
            .is_some()
    }

    fn has_present_twin(&self, row: &ItemRecord) -> bool {
        has_present_content_twin(self.inner, row)
    }

    fn sha1(&self, path: &Path) -> Option<Sha1> {
        match sha1_file(path) {
            Ok(sha1) => Some(sha1),
            Err(error) => {
                tracing::debug!(path = %path.display(), error = %error, "a rename candidate could not be read");
                None
            }
        }
    }
}

/// Whether another tracked item holds the same content and still has its file
/// on disk (R5.2, the second-copy acceptance line).
fn has_present_content_twin(inner: &EngineInner, row: &ItemRecord) -> bool {
    let Ok(twins) = inner.store.content_twins(row.sha1, row.size, &row.path) else {
        return false;
    };
    twins.iter().any(|twin| {
        std::fs::symlink_metadata(&twin.path)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
    })
}

/// Records that a reconciliation scan just read `path`, and publishes it
/// (#43).
///
/// The folder is named by its path key throughout: `remove_folder` retains the
/// list, so an index resolved under one lock can name another folder by the
/// time it is used again.
fn note_folder_scanned(inner: &EngineInner, path: &str) {
    let at_ns = record_folder_time(inner, path, FolderMoment::Scan);
    let mut folders = inner.folders.lock().expect("the folder lock");
    if let Some(entry) = folder_entry_mut(&mut folders, path) {
        entry.last_scan_ns = Some(at_ns);
    }
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
    if available {
        clear_global_error_if_recovered(inner, &folders);
    }
}

/// Which of a folder's two timestamps a write records (#43).
#[derive(Clone, Copy)]
enum FolderMoment {
    /// A reconciliation scan read the folder.
    Scan,
    /// The folder uploaded an asset.
    Upload,
}

/// The live entry that owns `path`, resolved and borrowed under one guard
/// (#48).
///
/// The caller supplies the guard, so `remove_folder`'s `retain` cannot run
/// between the lookup and the write and move the write onto another entry.
fn folder_entry_owning<'a>(
    folders: &'a mut [FolderEntry],
    path: &Path,
) -> Option<&'a mut FolderEntry> {
    let index = folder_index_in(folders, path)?;
    folders.get_mut(index)
}

/// The live entry whose folder's exact path key is `path`, for a caller already
/// holding the folder lock (#43).
///
/// Unlike [`folder_entry_owning`], this matches the folder's own path key, not
/// containment of `path`.
fn folder_entry_mut<'a>(folders: &'a mut [FolderEntry], path: &str) -> Option<&'a mut FolderEntry> {
    let key = path_key(Path::new(path));
    folders
        .iter_mut()
        .find(|entry| path_key(Path::new(&entry.folder.path)) == key)
}

/// The ownership read over `ordered`, with a failed read logged against the
/// folder whose edit asked for it (#52).
///
/// `None` is a read that could not be answered: the caller leaves the live
/// counts as they are. Both call sites read it before taking the folder lock,
/// so the table scan never holds it (N2).
fn owned_counts(
    inner: &EngineInner,
    ordered: &[SourceFolder],
    folder: &str,
) -> Option<Vec<FolderCounts>> {
    match inner.store.folder_counts_by_owner(ordered) {
        Ok(counts) => Some(counts),
        Err(error) => {
            tracing::warn!(
                folder = %folder,
                error = %error,
                "the folder counts could not be read"
            );
            None
        }
    }
}

/// Whether the live list still holds the paths the ownership read counted.
///
/// Only the ordered raw paths decide a row's owner, so an edit to a folder's
/// other fields inside the read window keeps the answer (#52).
fn same_paths(folders: &[FolderEntry], ordered: &[SourceFolder]) -> bool {
    folders
        .iter()
        .map(|entry| entry.folder.path.as_str())
        .eq(ordered.iter().map(|folder| folder.path.as_str()))
}

/// Applies an ownership read to the live entries a folder edit can move rows
/// between (#52): `key`'s entry and every entry that contains it or lies inside
/// it.
///
/// A row changes cards only when two folders contain one another, so every
/// other card keeps its live count: replacing it with the store's snapshot
/// could lose an increment that landed after the read.
///
/// shortcut: a row written just before the read and counted on its card just
/// after it lands twice until the next start; upgrade if a live card must never
/// be off by the items in flight.
fn apply_owned_counts(folders: &mut [FolderEntry], key: &str, counts: Vec<FolderCounts>) {
    for (entry, counts) in folders.iter_mut().zip(counts) {
        let other = path_key(Path::new(&entry.folder.path));
        if other == key || key_is_within(&other, key) || key_is_within(key, &other) {
            entry.counts = counts;
        }
    }
}

/// Writes one folder timestamp to the store and returns when it happened
/// (#43).
///
/// The folder is named by its path key, so a removal that shifts the live list
/// can never record the time on another folder's row (R2.1). The write runs
/// without the folder lock, so a slow store never holds the lock the status
/// read needs (N2). The caller publishes the returned time on the live entry,
/// because the observation stands even when the write failed.
fn record_folder_time(inner: &EngineInner, path: &str, moment: FolderMoment) -> i64 {
    let at_ns = now_ns();
    let stored = match moment {
        FolderMoment::Scan => inner.store.record_folder_scan(path, at_ns),
        FolderMoment::Upload => inner.store.record_folder_upload(path, at_ns),
    };
    if let Err(error) = stored {
        tracing::warn!(
            folder = %path,
            error = %error,
            "the folder time could not be recorded"
        );
    }
    at_ns
}

/// Clears the global last-error once no folder holds one, so the tray card
/// stops reporting a failure every folder has recovered from (R8.2).
fn clear_global_error_if_recovered(inner: &EngineInner, folders: &[FolderEntry]) {
    if folders.iter().all(|entry| entry.last_error.is_none()) {
        *inner.last_error.lock().expect("the error lock") = None;
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
    use crate::meter::UploadTraffic;
    use crate::status::Activity;

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
        assert!(
            !config.tag_uploaded_assets,
            "R3.7: the library default keeps the sync tag off; the app's setting is on"
        );
    }

    /// A file whose folder was removed while it was queued still streams under
    /// the client's limit (R7.1).
    ///
    /// The fallback must not be a fresh unlimited meter: that would lift the
    /// limit for exactly the uploads no card accounts for.
    #[test]
    fn a_removed_folders_file_still_streams_under_the_client_limit() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let store =
            Arc::new(StateStore::open(temp.path().join("state.db")).expect("open the state store"));
        let folder = temp.path().join("photos");
        std::fs::create_dir_all(&folder).expect("create the folder");
        store
            .add_folder(&SourceFolder {
                path: folder.display().to_string(),
                recursive: true,
                album_mapping: false,
                album_scope: AlbumScope::default(),
                album_title: None,
            })
            .expect("add the folder");
        // The session is never used: this test sends no request.
        let session = crate::connection::Connection::new(
            "http://127.0.0.1:1",
            Arc::new(crate::credential::MemoryCredentialStore::new()),
        )
        .expect("a valid URL")
        .shared();
        let config = EngineConfig {
            bandwidth_bytes_per_second: 4_096,
            ..EngineConfig::default()
        };
        let engine =
            Engine::new(Arc::new(Tracking::new(store)), session, config).expect("build the engine");
        engine
            .remove_folder(&folder.display().to_string())
            .expect("remove the folder");

        let queued = folder.join("queued.jpg");
        let meter = engine.inner.meter_for(&queued);
        assert_eq!(
            meter.limit().bytes_per_second(),
            4_096,
            "R7.1: a removed folder does not drop its queued uploads to unlimited"
        );
    }

    /// A removal cannot shift the folder an upload is metered on (R8.1).
    ///
    /// `remove_folder` retains the list, so every entry above the removal moves
    /// down one. A lookup that resolved its owner under one lock and read the
    /// meter under another would meet that shift in between: with folders
    /// `[a, b, c]`, resolving `b`'s file and then removing `a` leaves index 1
    /// naming `c`, so the upload is metered on `c`'s card instead of `b`'s.
    /// The list is shifted and restored under the lookup until it meets that
    /// interleaving; one lock cannot be split, so the answer is `b`'s meter
    /// throughout.
    #[test]
    fn a_removal_cannot_shift_the_folder_an_upload_is_metered_on() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let store =
            Arc::new(StateStore::open(temp.path().join("state.db")).expect("open the state store"));
        let mut dirs = Vec::new();
        for name in ["a", "b", "c"] {
            let dir = temp.path().join(name);
            std::fs::create_dir_all(&dir).expect("create the folder");
            store
                .add_folder(&SourceFolder {
                    path: dir.display().to_string(),
                    recursive: true,
                    album_mapping: false,
                    album_scope: AlbumScope::default(),
                    album_title: None,
                })
                .expect("add the folder");
            dirs.push(dir);
        }
        // The session is never used: this test sends no request.
        let session = crate::connection::Connection::new(
            "http://127.0.0.1:1",
            Arc::new(crate::credential::MemoryCredentialStore::new()),
        )
        .expect("a valid URL")
        .shared();
        let engine = Engine::new(
            Arc::new(Tracking::new(store)),
            session,
            EngineConfig::default(),
        )
        .expect("build the engine");
        let inner = Arc::clone(&engine.inner);
        let metered = dirs[1].join("metered.jpg");
        let b_meter = inner.meter_for(&metered);
        assert_eq!(
            folder_index_in(&inner.folders.lock().expect("the folder lock"), &metered),
            Some(1),
            "the list is [a, b, c] and the middle entry owns the file"
        );

        let shifted = Arc::new(AtomicBool::new(false));
        let churn = {
            let inner = Arc::clone(&inner);
            let shifted = Arc::clone(&shifted);
            std::thread::spawn(move || {
                for _ in 0..500 {
                    // What `remove_folder`'s retain does to the list, and the
                    // re-add that restores `a` in front of `b` so the shift
                    // recurs instead of happening once.
                    let removed = inner.folders.lock().expect("the folder lock").remove(0);
                    // Out of the list long enough for a lookup to meet it.
                    std::thread::sleep(Duration::from_micros(100));
                    inner
                        .folders
                        .lock()
                        .expect("the folder lock")
                        .insert(0, removed);
                }
                shifted.store(true, Ordering::SeqCst);
            })
        };
        while !shifted.load(Ordering::SeqCst) {
            let meter = inner.meter_for(&metered);
            assert!(
                Arc::ptr_eq(&meter, &b_meter),
                "R8.1: the upload was metered on another folder's card"
            );
        }
        churn.join().expect("the churn thread finished");
    }

    /// #43: a scan time lands on the folder the scan read, never on whichever
    /// entry occupies its index after a removal.
    ///
    /// `remove_folder` retains the list, so every entry above the removal moves
    /// down one. `note_folder_scanned` resolved the folder's index, released
    /// the lock, and let `record_folder_time` read that index again: with
    /// folders `[a, b, c]`, a scan of `b` that meets the removal of `a` in
    /// between writes the time on `c`'s row. The list is shifted under the
    /// lookup until it meets that interleaving; the write is keyed by path, so
    /// no other folder can carry the time.
    #[test]
    fn a_removal_cannot_shift_the_folder_a_scan_time_is_recorded_on() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let store =
            Arc::new(StateStore::open(temp.path().join("state.db")).expect("open the state store"));
        let mut dirs = Vec::new();
        for name in ["a", "b", "c"] {
            let dir = temp.path().join(name);
            std::fs::create_dir_all(&dir).expect("create the folder");
            store
                .add_folder(&SourceFolder {
                    path: dir.display().to_string(),
                    recursive: true,
                    album_mapping: false,
                    album_scope: AlbumScope::default(),
                    album_title: None,
                })
                .expect("add the folder");
            dirs.push(dir);
        }
        // The session is never used: this test sends no request.
        let session = crate::connection::Connection::new(
            "http://127.0.0.1:1",
            Arc::new(crate::credential::MemoryCredentialStore::new()),
        )
        .expect("a valid URL")
        .shared();
        let engine = Engine::new(
            Arc::new(Tracking::new(Arc::clone(&store))),
            session,
            EngineConfig::default(),
        )
        .expect("build the engine");
        let inner = Arc::clone(&engine.inner);
        let scanned = dirs[1].display().to_string();
        let untouched: Vec<String> = [&dirs[0], &dirs[2]]
            .iter()
            .map(|dir| dir.display().to_string())
            .collect();
        assert_eq!(
            folder_index_in(
                &inner.folders.lock().expect("the folder lock"),
                &dirs[1].join("photo.jpg")
            ),
            Some(1),
            "the list is [a, b, c] and the middle entry was scanned"
        );

        let shifted = Arc::new(AtomicBool::new(false));
        let churn = {
            let inner = Arc::clone(&inner);
            let shifted = Arc::clone(&shifted);
            std::thread::spawn(move || {
                for _ in 0..500 {
                    // What `remove_folder`'s retain does to the list, and the
                    // re-add that restores `a` in front of `b` so the shift
                    // recurs instead of happening once.
                    let removed = inner.folders.lock().expect("the folder lock").remove(0);
                    // Out of the list long enough for a lookup to meet it.
                    std::thread::sleep(Duration::from_micros(100));
                    inner
                        .folders
                        .lock()
                        .expect("the folder lock")
                        .insert(0, removed);
                }
                shifted.store(true, Ordering::SeqCst);
            })
        };
        while !shifted.load(Ordering::SeqCst) {
            note_folder_scanned(&inner, &scanned);
            // The live entries are the second write: the old path resolved the
            // index again after the store write, so a shift during that write
            // publishes the time on another folder's entry.
            let folders = inner.folders.lock().expect("the folder lock");
            for entry in folders.iter().filter(|entry| entry.folder.path != scanned) {
                assert_eq!(
                    entry.last_scan_ns, None,
                    "#43: a removal shifted the scan time onto {}",
                    entry.folder.path
                );
            }
        }
        churn.join().expect("the churn thread finished");
        for path in &untouched {
            assert_eq!(
                store
                    .folder_times(path)
                    .expect("read the folder times")
                    .last_scan_ns,
                None,
                "#43: a removal shifted the stored scan time onto another folder"
            );
        }
        assert!(
            store
                .folder_times(&scanned)
                .expect("read the folder times")
                .last_scan_ns
                .is_some(),
            "#43: the folder the scan read keeps its own time"
        );
    }

    /// A removal cannot move a folder's counts onto another folder (#48).
    ///
    /// The count paths resolved the owning index under one folder guard and
    /// indexed the live list under a second. `remove_folder` retains the list,
    /// so with folders `[a, b, c]` a removal of `a` in between moves `c` down to
    /// index 1 and the file's counts land on `b`'s card; the same shift with two
    /// folders indexes past the end and panics on the worker thread. The list is
    /// shifted and restored under the bookkeeping until it meets that
    /// interleaving; one guard cannot be split, so only the folder that owns the
    /// file is ever counted.
    #[test]
    fn a_removal_cannot_shift_the_counts_onto_another_folder() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let store =
            Arc::new(StateStore::open(temp.path().join("state.db")).expect("open the state store"));
        let mut dirs = Vec::new();
        for name in ["a", "b", "c"] {
            let dir = temp.path().join(name);
            std::fs::create_dir_all(&dir).expect("create the folder");
            store
                .add_folder(&SourceFolder {
                    path: dir.display().to_string(),
                    recursive: true,
                    album_mapping: false,
                    album_scope: AlbumScope::default(),
                    album_title: None,
                })
                .expect("add the folder");
            dirs.push(dir);
        }
        // The session is never used: this test sends no request.
        let session = crate::connection::Connection::new(
            "http://127.0.0.1:1",
            Arc::new(crate::credential::MemoryCredentialStore::new()),
        )
        .expect("a valid URL")
        .shared();
        let engine = Engine::new(
            Arc::new(Tracking::new(store)),
            session,
            EngineConfig::default(),
        )
        .expect("build the engine");
        let inner = Arc::clone(&engine.inner);
        let counted = dirs[2].join("photo.jpg");
        let counted_key = path_key(&counted);
        let owner_path = dirs[2].display().to_string();
        assert_eq!(
            folder_index_in(&inner.folders.lock().expect("the folder lock"), &counted),
            Some(2),
            "the list is [a, b, c] and the last entry owns the file"
        );
        let item = PreparedItem {
            path: counted.clone(),
            size: 1,
            mtime_ns: 0,
            sha1: Sha1::from_slice(&[0; 20]).expect("a full digest"),
            sidecar: None,
            live_photo_video: None,
        };
        let failure = UploadFailure {
            class: None,
            kind: FailureKind::Transient,
            message: "the connection dropped".to_string(),
            retry_after: None,
            connection: true,
        };

        let shifted = Arc::new(AtomicBool::new(false));
        let churn = {
            let inner = Arc::clone(&inner);
            let shifted = Arc::clone(&shifted);
            std::thread::spawn(move || {
                for _ in 0..500 {
                    // What `remove_folder`'s retain does to the list, and the
                    // re-add that restores `a` in front of `b` so the shift
                    // recurs instead of happening once.
                    let removed = inner.folders.lock().expect("the folder lock").remove(0);
                    // Out of the list long enough for a lookup to meet it.
                    std::thread::sleep(Duration::from_micros(100));
                    inner
                        .folders
                        .lock()
                        .expect("the folder lock")
                        .insert(0, removed);
                }
                shifted.store(true, Ordering::SeqCst);
            })
        };
        let mut queued = 0;
        // Keep churning until at least one file reached the owner, so a
        // descheduled main thread cannot leave the counts unexercised.
        while !shifted.load(Ordering::SeqCst) || queued == 0 {
            inner.note_pending(&counted, None);
            inner.note_review_missing(&counted_key);
            inner.note_retrying(&item, &failure);
            queued += 1;
            let folders = inner.folders.lock().expect("the folder lock");
            for entry in folders
                .iter()
                .filter(|entry| entry.folder.path != owner_path)
            {
                assert_eq!(
                    entry.counts,
                    FolderCounts::default(),
                    "#48: a removal shifted the counts onto {}",
                    entry.folder.path
                );
                assert_eq!(
                    entry.last_error, None,
                    "#48: a removal shifted the retry reason onto {}",
                    entry.folder.path
                );
            }
        }
        churn.join().expect("the churn thread finished");
        let folders = inner.folders.lock().expect("the folder lock");
        let owner = folders
            .iter()
            .find(|entry| entry.folder.path == owner_path)
            .expect("the owner is still listed");
        assert_eq!(
            (owner.counts.pending, owner.counts.review),
            (queued, queued),
            "R8.1: the owner counted every file handed to it"
        );
    }

    /// #52: removing an outer source folder hands its rows to the next folder
    /// that contains them, so the card the user keeps counts them at once
    /// instead of showing nothing until the next start.
    #[test]
    fn removing_an_outer_folder_hands_its_rows_to_the_next_owner() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let store =
            Arc::new(StateStore::open(temp.path().join("state.db")).expect("open the state store"));
        let outer = temp.path().join("photos");
        let inner = outer.join("2024");
        for root in [&outer, &inner] {
            std::fs::create_dir_all(root).expect("create the folder");
            store
                .add_folder(&SourceFolder {
                    path: root.display().to_string(),
                    recursive: true,
                    album_mapping: false,
                    album_scope: AlbumScope::default(),
                    album_title: None,
                })
                .expect("add the folder");
        }
        store
            .record(&ItemRecord {
                path: inner.join("photo.jpg").display().to_string(),
                size: 1,
                mtime_ns: 0,
                sha1: Sha1::from_slice(&[0; 20]).expect("a full digest"),
                asset_id: Some("asset-1".to_string()),
                state: ItemState::Uploaded,
                error_class: None,
                error_message: None,
                attempts: 0,
                sidecar: None,
            })
            .expect("write the uploaded row");
        // The session is never used: this test sends no request.
        let session = crate::connection::Connection::new(
            "http://127.0.0.1:1",
            Arc::new(crate::credential::MemoryCredentialStore::new()),
        )
        .expect("a valid URL")
        .shared();
        let engine = Engine::new(
            Arc::new(Tracking::new(store)),
            session,
            EngineConfig::default(),
        )
        .expect("build the engine");
        assert_eq!(
            engine.status().folders[0].counts.uploaded,
            1,
            "the outer folder is first in path order and owns the row"
        );
        assert_eq!(
            engine.status().folders[1].counts,
            FolderCounts::default(),
            "the nested folder owns nothing while the outer folder is there"
        );
        // #52: the file's bytes and its row land on one card, so the meter
        // `meter_for` hands out is the card's own.
        let owner_meter = {
            let folders = engine.inner.folders.lock().expect("the folder lock");
            Arc::clone(&folders[0].meter)
        };
        assert!(
            Arc::ptr_eq(
                &engine.inner.meter_for(&inner.join("photo.jpg")),
                &owner_meter
            ),
            "#52: the meter and the counts must name the same owner"
        );

        engine
            .remove_folder(&outer.display().to_string())
            .expect("remove the outer folder");

        let folders = engine.status().folders;
        assert_eq!(folders.len(), 1, "only the nested folder is left");
        assert_eq!(
            folders[0].counts.uploaded, 1,
            "#52: the nested folder owns the row the removed folder held"
        );
        assert_eq!(engine.status().counts.uploaded, 1, "the total is unchanged");
        engine.stop();
    }

    /// #52: a removal re-reads only the cards a row can move to, so a counter
    /// the store has not confirmed yet is not replaced by an older snapshot.
    ///
    /// The store row of an upload is written before the live counter moves.
    /// Re-seeding every remaining card from a snapshot taken in that window
    /// leaves the item counted twice (snapshot plus the later increment) on a
    /// card the removal never touches. `a` and `b` are siblings, so removing
    /// `a` moves no row and `b` keeps its live count.
    #[test]
    fn removing_an_unrelated_folder_keeps_the_live_counts() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let store =
            Arc::new(StateStore::open(temp.path().join("state.db")).expect("open the state store"));
        let first = temp.path().join("a");
        let second = temp.path().join("b");
        for root in [&first, &second] {
            std::fs::create_dir_all(root).expect("create the folder");
            store
                .add_folder(&SourceFolder {
                    path: root.display().to_string(),
                    recursive: true,
                    album_mapping: false,
                    album_scope: AlbumScope::default(),
                    album_title: None,
                })
                .expect("add the folder");
        }
        store
            .record(&ItemRecord {
                path: second.join("queued.jpg").display().to_string(),
                size: 1,
                mtime_ns: 0,
                sha1: Sha1::from_slice(&[0; 20]).expect("a full digest"),
                asset_id: None,
                state: ItemState::Pending,
                error_class: None,
                error_message: None,
                attempts: 0,
                sidecar: None,
            })
            .expect("write the pending row");
        // The session is never used: this test sends no request.
        let session = crate::connection::Connection::new(
            "http://127.0.0.1:1",
            Arc::new(crate::credential::MemoryCredentialStore::new()),
        )
        .expect("a valid URL")
        .shared();
        let engine = Engine::new(
            Arc::new(Tracking::new(store)),
            session,
            EngineConfig::default(),
        )
        .expect("build the engine");
        assert_eq!(
            engine.status().folders[1].counts.pending,
            1,
            "the stored row seeds the card"
        );
        // In flight: the store already holds the row and the live counter has
        // moved past it.
        engine.inner.note_pending(&second.join("queued.jpg"), None);
        assert_eq!(engine.status().folders[1].counts.pending, 2);

        engine
            .remove_folder(&first.display().to_string())
            .expect("remove the unrelated folder");

        let folders = engine.status().folders;
        assert_eq!(folders.len(), 1, "only the second folder is left");
        assert_eq!(
            folders[0].counts.pending, 2,
            "#52: the removal replaced a live count with an older store snapshot"
        );
        engine.stop();
    }

    /// #52: the live list keeps the store's path order, so a nested folder
    /// added after the folder inside it owns the same rows the next start does.
    ///
    /// `store.folders()` orders by path while the live list used to append, so
    /// with the inner folder listed first every row stayed on the inner card
    /// until a restart moved it to the outer one.
    #[test]
    fn adding_an_outer_folder_keeps_the_live_counts_in_store_order() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let store =
            Arc::new(StateStore::open(temp.path().join("state.db")).expect("open the state store"));
        let outer = temp.path().join("photos");
        let inner = outer.join("2024");
        std::fs::create_dir_all(&inner).expect("create the folder");
        store
            .add_folder(&SourceFolder {
                path: inner.display().to_string(),
                recursive: true,
                album_mapping: false,
                album_scope: AlbumScope::default(),
                album_title: None,
            })
            .expect("add the inner folder first");
        store
            .record(&ItemRecord {
                path: inner.join("photo.jpg").display().to_string(),
                size: 1,
                mtime_ns: 0,
                sha1: Sha1::from_slice(&[0; 20]).expect("a full digest"),
                asset_id: Some("asset-1".to_string()),
                state: ItemState::Uploaded,
                error_class: None,
                error_message: None,
                attempts: 0,
                sidecar: None,
            })
            .expect("write the uploaded row");
        // The session is never used: this test sends no request.
        let session = crate::connection::Connection::new(
            "http://127.0.0.1:1",
            Arc::new(crate::credential::MemoryCredentialStore::new()),
        )
        .expect("a valid URL")
        .shared();
        let engine = Engine::new(
            Arc::new(Tracking::new(Arc::clone(&store))),
            session,
            EngineConfig::default(),
        )
        .expect("build the engine");
        assert_eq!(
            engine.status().folders[0].counts.uploaded,
            1,
            "the only folder owns the row"
        );

        engine
            .add_folder(&outer.display().to_string(), true)
            .expect("add the outer folder mid-session");

        let status = engine.status();
        let stored = store.folders().expect("read the stored folders");
        let counts = store
            .folder_counts_by_owner(&stored)
            .expect("read the stored counts");
        assert_eq!(
            status
                .folders
                .iter()
                .map(|folder| (folder.path.clone(), folder.counts))
                .collect::<Vec<_>>(),
            stored
                .iter()
                .zip(counts)
                .map(|(folder, counts)| (folder.path.clone(), counts))
                .collect::<Vec<_>>(),
            "#52: the live cards must own what the stored ones own, in the same order"
        );
        engine.stop();
    }

    /// #52: a re-add that spells the path differently rewrites the stored
    /// `path`, and the store orders by that raw column. The live list must
    /// re-sort and re-read with it, or its cards and meters name another owner
    /// than the next start does.
    ///
    /// The re-spelled path sorts after the nested one, so the nested folder
    /// becomes the owner. Windows folds case (R2.4); on Linux a trailing
    /// separator is dropped from the key and sorts after the nested path's
    /// separator.
    #[test]
    fn a_respelled_re_add_keeps_the_live_counts_in_store_order() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let store =
            Arc::new(StateStore::open(temp.path().join("state.db")).expect("open the state store"));
        let outer = temp.path().join("photos");
        let inner = outer.join("2024");
        std::fs::create_dir_all(&inner).expect("create the folder");
        // The raw spellings the store keys as one folder: `before` is stored,
        // `after` is the re-add, and both sort after the nested path.
        let (before, inner_path, after) = if cfg!(windows) {
            let before = outer.with_file_name("PHOTOS").display().to_string();
            let inner_path = format!("{before}\\2024");
            (before, inner_path, outer.display().to_string())
        } else {
            let before = outer.display().to_string();
            let inner_path = format!("{before}/2024");
            let after = format!("{before}\\");
            (before, inner_path, after)
        };
        for path in [&before, &inner_path] {
            store
                .add_folder(&SourceFolder {
                    path: path.clone(),
                    recursive: true,
                    album_mapping: false,
                    album_scope: AlbumScope::default(),
                    album_title: None,
                })
                .expect("add the folder");
        }
        store
            .record(&ItemRecord {
                path: format!("{inner_path}/photo.jpg"),
                size: 1,
                mtime_ns: 0,
                sha1: Sha1::from_slice(&[0; 20]).expect("a full digest"),
                asset_id: Some("asset-1".to_string()),
                state: ItemState::Uploaded,
                error_class: None,
                error_message: None,
                attempts: 0,
                sidecar: None,
            })
            .expect("write the uploaded row");
        // The session is never used: this test sends no request.
        let session = crate::connection::Connection::new(
            "http://127.0.0.1:1",
            Arc::new(crate::credential::MemoryCredentialStore::new()),
        )
        .expect("a valid URL")
        .shared();
        let engine = Engine::new(
            Arc::new(Tracking::new(Arc::clone(&store))),
            session,
            EngineConfig::default(),
        )
        .expect("build the engine");
        assert_eq!(
            engine.status().folders[0].counts.uploaded,
            1,
            "the outer folder is first in the stored order and owns the row"
        );

        engine
            .add_folder(&after, true)
            .expect("re-add the outer folder with another spelling");

        let status = engine.status();
        let stored = store.folders().expect("read the stored folders");
        let counts = store
            .folder_counts_by_owner(&stored)
            .expect("read the stored counts");
        assert_eq!(
            status.folders[0].path, inner_path,
            "the re-spelled outer path sorts after the nested one"
        );
        assert_eq!(
            status
                .folders
                .iter()
                .map(|folder| (folder.path.clone(), folder.counts))
                .collect::<Vec<_>>(),
            stored
                .iter()
                .zip(counts)
                .map(|(folder, counts)| (folder.path.clone(), counts))
                .collect::<Vec<_>>(),
            "#52: the live cards must own what the stored ones own, in the same order"
        );
        engine.stop();
    }

    /// #52: a re-spelled re-add of a folder that contains no other and lies
    /// inside none moves no row, so the live counters stay. The ownership read
    /// would replace them with a store snapshot that lags an upload already in
    /// flight, and the folder has no neighbour to take a row from anyway.
    #[test]
    fn a_respelled_re_add_that_cannot_move_rows_keeps_the_live_counts() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let store =
            Arc::new(StateStore::open(temp.path().join("state.db")).expect("open the state store"));
        let photos = temp.path().join("photos");
        std::fs::create_dir_all(&photos).expect("create the folder");
        store
            .add_folder(&SourceFolder {
                path: photos.display().to_string(),
                recursive: true,
                album_mapping: false,
                album_scope: AlbumScope::default(),
                album_title: None,
            })
            .expect("add the folder");
        store
            .record(&ItemRecord {
                path: photos.join("queued.jpg").display().to_string(),
                size: 1,
                mtime_ns: 0,
                sha1: Sha1::from_slice(&[0; 20]).expect("a full digest"),
                asset_id: None,
                state: ItemState::Pending,
                error_class: None,
                error_message: None,
                attempts: 0,
                sidecar: None,
            })
            .expect("write the pending row");
        // The session is never used: this test sends no request.
        let session = crate::connection::Connection::new(
            "http://127.0.0.1:1",
            Arc::new(crate::credential::MemoryCredentialStore::new()),
        )
        .expect("a valid URL")
        .shared();
        let engine = Engine::new(
            Arc::new(Tracking::new(store)),
            session,
            EngineConfig::default(),
        )
        .expect("build the engine");
        assert_eq!(engine.status().folders[0].counts.pending, 1);
        // In flight: the store already holds the row and the live counter has
        // moved past it.
        engine.inner.note_pending(&photos.join("queued.jpg"), None);
        assert_eq!(engine.status().folders[0].counts.pending, 2);

        // The trailing separator is the same path under R2.4 and sorts after
        // it; the folder is the only entry, so no row can change cards.
        engine
            .add_folder(&format!("{}/", photos.display()), true)
            .expect("re-add the folder with another spelling");

        let folders = engine.status().folders;
        assert_eq!(folders.len(), 1, "the re-add is the same folder");
        assert_eq!(
            folders[0].path,
            format!("{}/", photos.display()),
            "the live entry took the spelling the store wrote"
        );
        assert_eq!(
            folders[0].counts.pending, 2,
            "#52: a re-spell that moves no row replaced a live count with an older snapshot"
        );
        engine.stop();
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
        let mut status = Status {
            activity: Activity::Idle,
            folders: Vec::new(),
            counts: FolderCounts::default(),
            last_error: None,
            uploads: UploadTraffic::default(),
            scanning: false,
        };
        assert_eq!(status.progress(), 0.0);
        status.counts.uploaded = 3;
        assert_eq!(status.progress(), 1.0);
        status.counts.pending = 1;
        assert!((status.progress() - 0.75).abs() < f32::EPSILON);
    }

    /// An item the upload pool could not hand to a worker stays claimable
    /// (R6.7).
    ///
    /// `prepare` skips a path that is already claimed, so an item left claimed
    /// by either early return in `spawn_upload` was never queued again for the
    /// life of the engine, whatever the run that claimed it was doing.
    #[tokio::test]
    async fn an_item_the_pool_could_not_spawn_is_claimable_again() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let store =
            Arc::new(StateStore::open(temp.path().join("state.db")).expect("open the state store"));
        let file = temp.path().join("queued.jpg");
        std::fs::write(&file, b"queued bytes").expect("write the file");
        // The session is never used: neither path below sends a request.
        let session = crate::connection::Connection::new(
            "http://127.0.0.1:1",
            Arc::new(crate::credential::MemoryCredentialStore::new()),
        )
        .expect("a valid URL")
        .shared();
        let engine = Arc::new(
            Engine::new(
                Arc::new(Tracking::new(store)),
                session,
                EngineConfig::default(),
            )
            .expect("build the engine"),
        );
        let inner = Arc::clone(&engine.inner);

        // `stop` ran between the batch and the worker: the item stays pending
        // for the next engine (R6.7), so its claim must go with it.
        let index = FolderIndex::read(std::slice::from_ref(&file));
        let Some(item) = prepare(&inner, &file, &index).await else {
            panic!("the first prepare must claim the file");
        };
        inner.stopped.store(true, Ordering::SeqCst);
        spawn_upload(&inner, item).await;
        // The claim is what `prepare` skips on, so claiming the file again is
        // the proof that the pool released it.
        let Some(reclaimed) = prepare(&inner, &file, &index).await else {
            panic!("an item a stopped engine could not spawn is stuck claimed forever");
        };

        // The pool is closed, so no permit is left to take. The worker takes
        // its permit asynchronously now, so give it the chance to fail and
        // release the claim before asking whether the file is claimable.
        inner.upload_pool().close();
        spawn_upload(&inner, reclaimed).await;
        let mut reclaimed = false;
        for _ in 0..100 {
            if prepare(&inner, &file, &index).await.is_some() {
                reclaimed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            reclaimed,
            "an item a closed pool could not spawn is stuck claimed forever"
        );
    }

    /// A pool shrink must not queue behind every worker already waiting
    /// (R7.2).
    ///
    /// Taking the permits back one by one queues the shrink behind the whole
    /// waiting list, and each waiter runs its attempt before releasing
    /// anything, so the resize cost grows with the number of queued files (a
    /// probe measured 310 ms for 4 permits behind 120 waiters of 10 ms). The
    /// resize swaps the pool instead: attempts in flight finish on the pool
    /// they hold a permit from, and the waiters fail out at once and go back
    /// to the queue for the new pool.
    #[tokio::test]
    async fn shrinking_the_pool_does_not_wait_behind_the_queued_workers() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let store =
            Arc::new(StateStore::open(temp.path().join("state.db")).expect("open the state store"));
        // The session is never used: no request is sent.
        let session = crate::connection::Connection::new(
            "http://127.0.0.1:1",
            Arc::new(crate::credential::MemoryCredentialStore::new()),
        )
        .expect("a valid URL")
        .shared();
        let engine = Arc::new(
            Engine::new(
                Arc::new(Tracking::new(store)),
                session,
                EngineConfig {
                    concurrency: 4,
                    ..EngineConfig::default()
                },
            )
            .expect("build the engine"),
        );
        let inner = Arc::clone(&engine.inner);
        let mut workers = Vec::new();
        // Four attempts hold the whole pool; 120 more wait behind them.
        for _ in 0..4 {
            let pool = inner.upload_pool();
            workers.push(tokio::spawn(async move {
                let _permit = pool.acquire_owned().await.expect("the pool is open");
                tokio::time::sleep(Duration::from_secs(30)).await;
            }));
        }
        for _ in 0..120 {
            let pool = inner.upload_pool();
            workers.push(tokio::spawn(async move {
                let _permit = pool.acquire_owned().await;
            }));
        }
        // Let the holders take their permits and the rest settle in the queue.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let changed = tokio::time::timeout(
            Duration::from_secs(2),
            engine.set_limits(Limits {
                bandwidth_bytes_per_second: 0,
                concurrency: 1,
            }),
        )
        .await;
        assert!(
            changed.is_ok(),
            "R7.2: the shrink waited behind the queued workers"
        );
        assert_eq!(
            engine.limits().concurrency,
            1,
            "the published limit must be the pool the waiters now queue on"
        );
        for worker in workers {
            worker.abort();
        }
    }

    /// A scan with more files than one batch must list a folder once, not
    /// once per batch (R3.3).
    ///
    /// `process_batch` used to call `FolderIndex::read` for its own batch, so
    /// a 10k-file folder re-stated itself once per 100-file batch. The scan's
    /// cache now serves every batch of the pass.
    #[tokio::test]
    async fn a_scan_lists_each_folder_once_for_all_its_batches() {
        use crate::connection::{AuthMethod, Secret, SignIn};

        let server = mock_immich::MockImmich::start().await;
        let temp = tempfile::tempdir().expect("create a temp dir");
        let store =
            Arc::new(StateStore::open(temp.path().join("state.db")).expect("open the state store"));
        let folder = temp.path().join("photos");
        std::fs::create_dir_all(&folder).expect("create the folder");
        for index in 0..130 {
            std::fs::write(
                folder.join(format!("file-{index}.jpg")),
                format!("bytes {index}"),
            )
            .expect("write the fixture");
        }
        store
            .add_folder(&SourceFolder {
                path: folder.display().to_string(),
                recursive: true,
                album_mapping: false,
                album_scope: AlbumScope::default(),
                album_title: None,
            })
            .expect("add the folder");
        let mut connection = crate::connection::Connection::new(
            server.base_url(),
            Arc::new(crate::credential::MemoryCredentialStore::new()),
        )
        .expect("a valid URL");
        connection
            .sign_in(SignIn {
                email: "sync@example.com".to_string(),
                secret: Secret::new("correct horse"),
                method: AuthMethod::Password,
            })
            .await;
        let engine = Arc::new(
            Engine::new(
                Arc::new(Tracking::new(store.clone())),
                connection.shared(),
                EngineConfig {
                    scan_interval: Duration::from_secs(3600),
                    watch: false,
                    ..EngineConfig::default()
                },
            )
            .expect("build the engine"),
        );
        engine.start();

        let paths: Vec<PathBuf> = (0..130)
            .map(|index| folder.join(format!("file-{index}.jpg")))
            .collect();
        let mut finished = false;
        for _ in 0..1500 {
            finished = paths.iter().all(|path| {
                matches!(
                    store.item(&path.display().to_string()),
                    Ok(Some(row)) if row.state.is_terminal_success()
                )
            });
            if finished {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(finished, "the 130 files never finished");

        let listings = crate::pairing::LISTED_FOLDERS
            .lock()
            .expect("the listing log")
            .iter()
            .filter(|path| path.starts_with(&folder))
            .count();
        assert_eq!(
            listings, 1,
            "R3.3: one scan pass listed the folder {listings} times"
        );
        engine.stop();
    }
}
