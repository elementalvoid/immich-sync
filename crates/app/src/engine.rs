//! The app-scoped folder sync engine (SPEC R2.1–R2.5, R8.1–R8.3, R8.7).
//!
//! The model owns the engine for the process lifetime, so hiding the window
//! does not stop a sync. It starts the engine when a credential is available,
//! persists folder changes while the client is signed out, and republishes the
//! engine's status to the dashboard on a short poll.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::*;
use sync_core::engine::{Engine, EngineConfig, Limits};
use sync_core::meter::UploadTraffic;
use sync_core::status::{Activity, ConnectionHealth, FolderStatus, Status, StatusInputs};
use sync_core::store::{
    AlbumScope, FailedItem, FolderCounts, ReviewItem, SourceFolder, StateStore,
};

use crate::connection::{
    ConnectionModel, STORE_OPENING, WORKER_UNAVAILABLE, run_on_worker, run_on_worker_or_lost,
    worker,
};
use crate::folder_panel::{ALREADY_ADDED, confirmed_folder};
use crate::notify::Notifier;
use crate::paths::AppPaths;
use crate::settings::SettingsStore;
use crate::status::DashboardModel;
use crate::store::StoreModel;
use sync_core::connection::{ConnectionState, SharedSession};
use sync_core::tags::TagStatus;

/// How often the dashboard picks up the engine's live counts.
const STATUS_POLL: Duration = Duration::from_millis(250);

/// How often the failed-items list and the deletion review are re-read from
/// the store (R6.5, R5.3).
///
/// The read runs on the background executor, never on the UI thread (N2), so
/// this only decides how soon a parked item or a missing file shows up.
const FAILED_POLL_TICKS: u32 = 4;

/// The live settings the app pushes to a running engine (R7.1, R7.2, R3.7).
///
/// One channel carries both, so a change to either reaches the engine in the
/// order the view made it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LiveSettings {
    limits: Limits,
    tag_uploaded_assets: bool,
}

/// The app-scoped engine and the status the views render.
pub struct EngineModel {
    connection: Entity<ConnectionModel>,
    store_model: Entity<StoreModel>,
    paths: AppPaths,
    engine: Option<Arc<Engine>>,
    /// The newest live-settings change for the running engine. One applier task
    /// per engine drains it, so two quick edits cannot land out of order.
    live: Option<tokio::sync::watch::Sender<LiveSettings>>,
    status: Status,
    /// The sync tag's live state, which the Settings section renders (R3.7).
    tag: TagStatus,
    /// The parked items the failed-items view lists (R6.5), re-read off the UI
    /// thread.
    failed: Vec<FailedItem>,
    /// The missing files the deletion review lists (R5.3), re-read off the UI
    /// thread.
    review: Vec<ReviewItem>,
    /// The message from a trash the server refused (R5.5). It lives with the
    /// model, not the row: the item is still in the review, and a later success
    /// clears it.
    review_problem: Option<String>,
    /// Decides which system notifications one observation raises (R5.4,
    /// R10.1). It owns the latches, so the model keeps none of them.
    notifier: Notifier,
    /// Whether a successful read of the parked list has landed. The notifier
    /// owns the seeding of its failure latch (R10.1).
    failed_read: bool,
    paused: bool,
    problem: Option<String>,
    /// The newest folder-setup outcome, for the add dialog (#40).
    folder_add: Option<FolderAddResult>,
    /// Bumped by every setup-dialog add, kept apart from `generation` so an
    /// unrelated folder edit cannot drop the result the dialog waits for, and
    /// a slower earlier add cannot answer a newer dialog (#40).
    add_generation: u64,
    /// Bumped by every folder edit, so an older edit result cannot overwrite a
    /// newer one.
    generation: u64,
    /// Bumped by every engine start attempt, kept apart from `generation` so a
    /// folder edit cannot discard an engine that is already being built.
    start_generation: u64,
    /// The store generation the model has seen, so a rebuild's fresh store is
    /// noticed and the engine starts over it (R10.4).
    store_generation: u64,
    /// Whether a rebuild is running: no engine starts until the fresh store is
    /// in place.
    rebuilding: bool,
    _subscriptions: Vec<Subscription>,
    _poll: Task<()>,
}

/// What one album edit changes on a source folder (R3.4).
#[derive(Clone, Copy)]
enum AlbumChange {
    /// Turn album mapping on or off.
    Mapping(bool),
    /// Set the album scope.
    Scope(AlbumScope),
}

/// The outcome of one folder-setup add (#40).
///
/// The request id lets the dialog that sent the add recognise its own result,
/// so a slower earlier add cannot close a newer dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderAddResult {
    /// The request this result answers.
    pub request: u64,
    /// Why nothing was written, or `None` when the folder was added.
    pub problem: Option<String>,
}

impl EngineModel {
    /// Builds the model and starts watching for a usable credential.
    pub fn new(
        connection: Entity<ConnectionModel>,
        store_model: Entity<StoreModel>,
        paths: &AppPaths,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            // The status, the review list and the parked list are refreshed
            // when the store opens, so a review that already waits in the
            // database raises its notification and fills the view at start
            // (R5.3, R5.4), and the parked list seeds its notification latch
            // before the engine's first scan can park a new failure (R10.1).
            cx.observe(&store_model, |this, store, cx| {
                let generation = store.read(cx).generation();
                this.store_changed(generation, cx);
                // A decision with no engine updates the store's cached counts,
                // which do not move the generation, so the status follows every
                // store notification (R5.4).
                this.refresh_status(cx);
                this.refresh_review(cx);
                this.refresh_failed(cx);
            }),
            cx.observe(&connection, |this, _, cx| this.try_start_engine(cx)),
        ];
        let poll = cx.spawn(async move |this, cx| {
            let mut ticks = 0_u32;
            loop {
                cx.background_executor().timer(STATUS_POLL).await;
                let alive = this
                    .update(cx, |model, cx| {
                        // R1.4: a 401 the engine answered on its own thread
                        // reaches the model, and so the tray, on this poll.
                        model
                            .connection
                            .update(cx, |connection, cx| connection.refresh_state(cx));
                        if model.engine.is_some() {
                            model.refresh_status(cx);
                        }
                        ticks = ticks.wrapping_add(1);
                        if ticks.is_multiple_of(FAILED_POLL_TICKS) {
                            model.refresh_failed(cx);
                            model.refresh_review(cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        Self {
            connection,
            store_model,
            paths: paths.clone(),
            engine: None,
            live: None,
            status: empty_status(),
            tag: TagStatus::default(),
            failed: Vec::new(),
            review: Vec::new(),
            review_problem: None,
            notifier: Notifier::default(),
            failed_read: false,
            paused: false,
            problem: None,
            folder_add: None,
            add_generation: 0,
            generation: 0,
            start_generation: 0,
            store_generation: 0,
            rebuilding: false,
            _subscriptions: subscriptions,
            _poll: poll,
        }
    }

    /// The engine's latest status.
    #[must_use]
    pub fn status(&self) -> &Status {
        &self.status
    }

    /// The last model problem, if any.
    #[must_use]
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// The newest folder-setup outcome, if one has arrived (#40).
    #[must_use]
    pub fn folder_add_result(&self) -> Option<&FolderAddResult> {
        self.folder_add.as_ref()
    }

    /// The parked items the failed-items view lists (R6.5).
    #[must_use]
    pub fn failed_items(&self) -> &[FailedItem] {
        &self.failed
    }

    /// The missing files the deletion review lists (R5.3).
    #[must_use]
    pub fn review_items(&self) -> &[ReviewItem] {
        &self.review
    }

    /// The message from a trash the server refused, if any (R5.5).
    #[must_use]
    pub fn review_problem(&self) -> Option<&str> {
        self.review_problem.as_deref()
    }

    /// Keeps the server asset for one missing file and clears its row (R5.3).
    ///
    /// Nothing is sent to the server, so this works while the client is signed
    /// out; the state write still runs on the worker runtime (N2).
    pub fn keep_review(&mut self, path: String, cx: &mut Context<Self>) {
        let tracking = self.store_model.read(cx).tracking();
        let action_path = path.clone();
        let result = run_on_worker(
            cx,
            move || async move {
                // N2: the store write belongs on the worker, not the UI thread.
                let tracking = tracking.ok_or_else(|| STORE_OPENING.to_string())?;
                tracking
                    .keep_review(&action_path)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            },
            move |model, result, cx| model.finish_review_action(result, &path, cx),
        );
        if result.is_err() {
            self.review_problem = Some(WORKER_UNAVAILABLE.to_string());
            cx.notify();
        }
    }

    /// Moves one missing file's asset to the trash and clears its row (R5.3).
    ///
    /// A refusal leaves the item in the review and its reason in
    /// [`Self::review_problem`], which is what R5.5 asks the view to show.
    pub fn trash_review(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(engine) = self.engine.clone() else {
            self.review_problem = Some("Sign in before moving an asset to the trash.".to_string());
            cx.notify();
            return;
        };
        let action_path = path.clone();
        let result = run_on_worker(
            cx,
            move || async move {
                engine
                    .trash_review_item(&action_path)
                    .await
                    .map_err(|error| crate::review::trash_problem(&error))
            },
            move |model, result, cx| model.finish_review_action(result, &path, cx),
        );
        if result.is_err() {
            self.review_problem = Some(WORKER_UNAVAILABLE.to_string());
            cx.notify();
        }
    }

    /// Publishes the outcome of a keep or a trash and refreshes the rows
    /// (R5.3-R5.5).
    fn finish_review_action(
        &mut self,
        result: Result<(), String>,
        path: &str,
        cx: &mut Context<Self>,
    ) {
        let decided = result.is_ok();
        match result {
            Ok(()) => self.review_problem = None,
            Err(problem) => self.review_problem = Some(problem),
        }
        self.refresh_review(cx);
        // R5.4: the tray count follows the decision at once.
        self.refresh_status(cx);
        // Without a running engine the status is the store's cached counts,
        // which do not know about the write that just landed, so re-read the
        // affected folder's counts instead of patching the UI model: a later
        // refresh must not resurrect the item.
        if decided && self.engine.is_none() {
            self.store_model
                .update(cx, |store, cx| store.refresh_counts(path, cx));
        }
    }

    /// Re-reads the missing files from the store on the background executor
    /// (N2) and republishes them when they changed.
    pub fn refresh_review(&mut self, cx: &mut Context<Self>) {
        let Some(tracking) = self.store_model.read(cx).tracking() else {
            return;
        };
        let read = cx
            .background_executor()
            .spawn(async move { tracking.review_items() });
        cx.spawn(async move |this, cx| {
            let read = read.await;
            let _ = this.update(cx, |model, cx| match read {
                Ok(items) => {
                    if items != model.review {
                        model.review = items;
                        // R5.4: the status counts this list, so it follows the
                        // read that just landed instead of waiting for the poll.
                        model.refresh_status(cx);
                        cx.notify();
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "the deletion review could not be read");
                }
            });
        })
        .detach();
    }

    /// Re-attempts one parked item by hand (R6.5).
    pub fn retry_failed(&mut self, path: String, cx: &mut Context<Self>) {
        let tracking = self.store_model.read(cx).tracking();
        let result = run_on_worker(
            cx,
            move || async move {
                // The store write runs on the worker runtime, so the UI thread
                // never waits behind a worker's `BEGIN IMMEDIATE` (N2).
                let tracking = tracking.ok_or_else(|| STORE_OPENING.to_string())?;
                tracking
                    .retry_failed(&path)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            },
            |model, result, cx| {
                match result {
                    Ok(()) => model.problem = None,
                    Err(problem) => model.problem = Some(problem),
                }
                model.refresh_failed(cx);
            },
        );
        if result.is_err() {
            self.problem = Some(WORKER_UNAVAILABLE.to_string());
            cx.notify();
        }
    }

    /// Re-reads the parked items from the store on the background executor
    /// (N2) and republishes them when they changed.
    pub fn refresh_failed(&mut self, cx: &mut Context<Self>) {
        let Some(tracking) = self.store_model.read(cx).tracking() else {
            return;
        };
        let read = cx
            .background_executor()
            .spawn(async move { tracking.failed_items() });
        cx.spawn(async move |this, cx| {
            let read = read.await;
            let _ = this.update(cx, |model, cx| match read {
                Ok(items) => {
                    if items != model.failed {
                        model.failed = items;
                        cx.notify();
                    }
                    // R10.1: the first read is the parked list the process
                    // started with, so the notifier seeds its latch instead of
                    // notifying: a relaunch must not announce the dead letter
                    // again. After that, a newly parked item raises a
                    // notification of its own.
                    model.failed_read = true;
                    model.publish_notifications(cx);
                }
                Err(error) => {
                    tracing::warn!(error = %error, "the failed items could not be read");
                }
            });
        })
        .detach();
    }

    /// The one dashboard model the view renders (R8.1).
    #[must_use]
    pub fn dashboard_model(&self, cx: &App) -> DashboardModel {
        let connection = self.connection.read(cx).summary();
        DashboardModel::from_engine(&self.status, Some(connection))
    }

    /// Starts the engine once both the store and a credential are ready.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.try_start_engine(cx);
    }

    /// Holds new uploads (R8.7).
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        self.paused = true;
        if let Some(engine) = &self.engine {
            engine.pause();
            self.refresh_status(cx);
        } else {
            cx.notify();
        }
    }

    /// Lets uploads start again (R8.7).
    pub fn resume(&mut self, cx: &mut Context<Self>) {
        self.paused = false;
        if let Some(engine) = &self.engine {
            engine.resume();
            self.refresh_status(cx);
        } else {
            cx.notify();
        }
    }

    /// Whether new uploads are held.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Asks for a reconciliation scan now (R8.3).
    pub fn sync_now(&mut self, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.sync_now();
        } else {
            self.try_start_engine(cx);
        }
    }

    /// Whether the engine has been built and is owned by this model.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.engine.is_some()
    }

    /// The bytes the running engine has put on the wire (R10.2).
    #[must_use]
    pub fn uploaded_bytes(&self) -> u64 {
        self.engine
            .as_ref()
            .map_or(0, |engine| engine.uploaded_bytes())
    }

    /// Whether a state rebuild is running (R10.4).
    #[must_use]
    pub fn is_rebuilding(&self) -> bool {
        self.rebuilding
    }

    /// Rebuilds the state database and rescans the folders (R10.4).
    ///
    /// The engine is stopped first, so the rebuild replaces the store while
    /// nothing writes through the old one. It starts again over the fresh
    /// store, and a start runs one reconciliation scan immediately, which is
    /// the rescan this promises. While the first open or another rebuild is
    /// still running this is a no-op.
    pub fn rebuild(&mut self, cx: &mut Context<Self>) {
        // A second click while a rebuild runs cannot start another (R10.4).
        if self.rebuilding {
            return;
        }
        // The first open owns the store until it lands; the store model
        // refuses the rebuild and publishes the reason. Waiting here would
        // leave this model rebuilding against a generation that may never
        // arrive if the open fails (R10.4).
        if !self.store_model.read(cx).settled() {
            self.store_model.update(cx, |store, cx| store.rebuild(cx));
            cx.notify();
            return;
        }
        if let Some(engine) = self.engine.take() {
            engine.stop();
        }
        // A start already in flight must not install its engine over the
        // store this rebuild replaces (R10.4).
        self.start_generation = self.start_generation.wrapping_add(1);
        self.live = None;
        self.rebuilding = true;
        self.status = empty_status();
        self.tag = TagStatus::default();
        self.store_model.update(cx, |store, cx| store.rebuild(cx));
        cx.notify();
    }

    /// Handles a store notification: a new generation is the fresh store a
    /// rebuild was waiting for, and it is what lets the next engine start.
    fn store_changed(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation != self.store_generation {
            self.store_generation = generation;
            self.rebuilding = false;
            self.refresh_status(cx);
        }
        self.try_start_engine(cx);
    }

    /// The session the running engine sends its requests through (R1.4).
    ///
    /// It is the one the connection form signs in on, so the two never observe
    /// two session state machines.
    #[must_use]
    pub fn session(&self) -> Option<SharedSession> {
        self.engine.as_ref().map(|engine| engine.session().clone())
    }

    /// The live limits of the running engine (R7.1, R7.2). `None` before it
    /// starts; the start path reads the stored values.
    #[must_use]
    pub fn limits(&self) -> Option<Limits> {
        self.engine.as_ref().map(|engine| engine.limits())
    }

    /// Applies both limits to the running engine without a restart (R7.1,
    /// R7.2). Before it starts this is a no-op: the settings file it reads at
    /// start already holds the value the view wrote.
    pub fn set_limits(&mut self, bandwidth_bytes_per_second: u64, concurrency: usize) {
        // The applier task wakes on the newest value, so a change that
        // overtakes this one cannot be overwritten by it.
        if let Some(live) = &self.live {
            let mut next = *live.borrow();
            next.limits = Limits {
                bandwidth_bytes_per_second,
                concurrency: concurrency.max(1),
            };
            live.send_replace(next);
        }
    }

    /// The sync tag's live state, for the Settings section (R3.7).
    ///
    /// It comes from the engine, which did the one permission probe the view
    /// must not repeat.
    #[must_use]
    pub fn tag_status(&self) -> &TagStatus {
        &self.tag
    }

    /// Turns the sync tag on or off on the running engine without a restart
    /// (R3.7). Before it starts this is a no-op: the settings file it reads at
    /// start already holds the value the view wrote.
    pub fn set_tag_uploads(&mut self, enabled: bool) {
        if let Some(live) = &self.live {
            let mut next = *live.borrow();
            next.tag_uploaded_assets = enabled;
            live.send_replace(next);
        }
    }

    /// Adds or edits a source folder and starts discovering it (R2.1).
    pub fn add_folder(&mut self, path: String, recursive: bool, cx: &mut Context<Self>) {
        let path = path.trim().to_string();
        if path.is_empty() {
            self.problem = Some("Enter the path of a folder to add.".to_string());
            cx.notify();
            return;
        }
        // A newer edit supersedes an older result still in flight.
        self.generation = self.generation.wrapping_add(1);
        let tracking = self.store_model.read(cx).tracking();
        let generation = self.generation;
        let result = run_on_worker(
            cx,
            move || async move {
                // The store write runs on the worker runtime, so the UI thread
                // never waits behind a worker's `BEGIN IMMEDIATE` (N2).
                let tracking = tracking.ok_or_else(|| STORE_OPENING.to_string())?;
                tracking
                    .add_folder(&path, recursive)
                    .map_err(|error| error.to_string())?;
                tracking.folders().map_err(|error| error.to_string())
            },
            move |model, result, cx| {
                if model.generation != generation {
                    return;
                }
                match result {
                    Ok(folders) => {
                        model.store_model.update(cx, |store, cx| {
                            store.set_folders(folders, cx);
                        });
                        model.problem = None;
                    }
                    Err(problem) => model.problem = Some(problem),
                }
                model.refresh_status(cx);
            },
        );
        if result.is_err() {
            self.problem = Some(WORKER_UNAVAILABLE.to_string());
            cx.notify();
        }
    }

    /// Adds a source folder with the settings the setup dialog confirmed
    /// (#40, R2.1, R3.4).
    ///
    /// The request runs on the worker runtime (N2): it validates the path,
    /// refuses a folder the client already tracks without touching its row,
    /// and then writes the path and all three settings in one store write. The
    /// returned id tags the result, so the dialog that sent the add can wait
    /// for its own outcome and keep itself open on a refusal.
    pub fn add_folder_settings(&mut self, draft: SourceFolder, cx: &mut Context<Self>) -> u64 {
        self.add_generation = self.add_generation.wrapping_add(1);
        let request = self.add_generation;
        let tracking = self.store_model.read(cx).tracking();
        let result = run_on_worker_or_lost(
            cx,
            move || async move {
                // N2: the filesystem probe and the store write belong on the
                // worker, not the UI thread.
                let folder = confirmed_folder(&draft)?;
                // The tracking module decides, and it serves the engine and the
                // signed-out client alike; the insert refuses a path the client
                // already holds under the platform's path rule (R2.4).
                let tracking = tracking.ok_or_else(|| STORE_OPENING.to_string())?;
                tracking
                    .insert_folder(&folder)
                    .map_err(|error| match error {
                        sync_core::tracking::TrackingError::FolderExists(_) => {
                            ALREADY_ADDED.to_string()
                        }
                        other => other.to_string(),
                    })?;
                tracking.folders().map_err(|error| error.to_string())
            },
            move |model, result, cx| {
                // A newer add supersedes an older result still in flight.
                if model.add_generation != request {
                    return;
                }
                let problem = match result {
                    // The engine's card follows the tracking observer; the
                    // cached list the signed-out views read is refreshed here,
                    // because the insert wrote it.
                    Ok(folders) => {
                        model.store_model.update(cx, |store, cx| {
                            store.set_folders(folders, cx);
                        });
                        None
                    }
                    Err(problem) => Some(problem),
                };
                model.folder_add = Some(FolderAddResult { request, problem });
                model.refresh_status(cx);
                cx.notify();
            },
            move |model, cx| model.folder_add_lost(request, cx),
        );
        if result.is_err() {
            self.folder_add_lost(request, cx);
        }
        request
    }

    /// Records a setup-dialog add whose worker answer never arrived (#40).
    ///
    /// The dialog waits for its own request, so a lost answer would leave it
    /// waiting with Cancel refused. The reason is the runtime that is gone, and
    /// it belongs to the dialog: it names what the add could not do, the same
    /// way a refusal does.
    pub(crate) fn folder_add_lost(&mut self, request: u64, cx: &mut Context<Self>) {
        // A newer add supersedes an older result still in flight.
        if self.add_generation != request {
            return;
        }
        self.folder_add = Some(FolderAddResult {
            request,
            problem: Some(WORKER_UNAVAILABLE.to_string()),
        });
        cx.notify();
    }

    /// Removes a source folder; its items stay in the store (R2.1).
    pub fn remove_folder(&mut self, path: String, cx: &mut Context<Self>) {
        // A newer edit supersedes an older result still in flight.
        self.generation = self.generation.wrapping_add(1);
        let tracking = self.store_model.read(cx).tracking();
        let generation = self.generation;
        let result = run_on_worker(
            cx,
            move || async move {
                // N2: the store write belongs on the worker, not the UI thread.
                let tracking = tracking.ok_or_else(|| STORE_OPENING.to_string())?;
                tracking
                    .remove_folder(&path)
                    .map_err(|error| error.to_string())?;
                tracking.folders().map_err(|error| error.to_string())
            },
            move |model, result, cx| {
                if model.generation != generation {
                    return;
                }
                match result {
                    Ok(folders) => {
                        model.store_model.update(cx, |store, cx| {
                            store.set_folders(folders, cx);
                        });
                        model.problem = None;
                    }
                    Err(problem) => model.problem = Some(problem),
                }
                model.refresh_status(cx);
            },
        );
        if result.is_err() {
            self.problem = Some(WORKER_UNAVAILABLE.to_string());
            cx.notify();
        }
    }

    /// Turns album mapping on or off for one source folder (R3.4).
    ///
    /// The store write runs on the worker runtime, so the UI thread never waits
    /// behind it (N2), and the status republishes when it lands.
    pub fn set_album_mapping(&mut self, path: String, enabled: bool, cx: &mut Context<Self>) {
        self.change_album(path, AlbumChange::Mapping(enabled), cx);
    }

    /// Sets the album scope for one source folder (R3.4).
    pub fn set_album_scope(&mut self, path: String, scope: AlbumScope, cx: &mut Context<Self>) {
        self.change_album(path, AlbumChange::Scope(scope), cx);
    }

    /// Runs one album edit on the worker runtime and republishes the status.
    ///
    /// A newer edit supersedes an older result still in flight, the same way an
    /// add or a remove does, so a slow write cannot undo a later one. The
    /// folder list comes back with the write, so the card that shows the mapping
    /// shows what the store now holds (R8.1).
    fn change_album(&mut self, path: String, change: AlbumChange, cx: &mut Context<Self>) {
        // A newer edit supersedes an older result still in flight.
        self.generation = self.generation.wrapping_add(1);
        let tracking = self.store_model.read(cx).tracking();
        let generation = self.generation;
        let result = run_on_worker(
            cx,
            move || async move {
                // N2: the store write belongs on the worker, not the UI thread.
                let tracking = tracking.ok_or_else(|| STORE_OPENING.to_string())?;
                match change {
                    AlbumChange::Mapping(enabled) => tracking.set_album_mapping(&path, enabled),
                    AlbumChange::Scope(scope) => tracking.set_album_scope(&path, scope),
                }
                .map_err(|error| error.to_string())?;
                tracking.folders().map_err(|error| error.to_string())
            },
            move |model, result, cx| {
                if model.generation != generation {
                    return;
                }
                match result {
                    Ok(folders) => {
                        model.store_model.update(cx, |store, cx| {
                            store.set_folders(folders, cx);
                        });
                        model.problem = None;
                    }
                    Err(problem) => model.problem = Some(problem),
                }
                model.refresh_status(cx);
            },
        );
        if result.is_err() {
            self.problem = Some(WORKER_UNAVAILABLE.to_string());
            cx.notify();
        }
    }

    /// Starts the engine when it is not running yet and both inputs exist.
    fn try_start_engine(&mut self, cx: &mut Context<Self>) {
        // A rebuild owns the store until its fresh one lands (R10.4); starting
        // over the old store would leave the engine watching a replaced file.
        if self.rebuilding {
            return;
        }
        let connected = self.connection.read(cx).state().is_connected();
        if self.engine.is_some() {
            if connected {
                // R1.4: a connection that comes back resumes the rows a refused
                // credential left pending. Without this they wait for the next
                // reconciliation scan, 30 minutes away by default.
                if let Some(engine) = &self.engine {
                    engine.sync_now();
                }
            }
            self.refresh_status(cx);
            return;
        }
        if !connected {
            self.refresh_status(cx);
            return;
        }
        let Some(tracking) = self.store_model.read(cx).tracking() else {
            self.refresh_status(cx);
            return;
        };
        let Some(session) = self.connection.read(cx).session() else {
            self.refresh_status(cx);
            return;
        };
        let config_file = self.paths.config_file.clone();
        let paused = self.paused;
        self.start_generation = self.start_generation.wrapping_add(1);
        let start_generation = self.start_generation;
        let result = run_on_worker(
            cx,
            move || async move {
                // The settings file is small; reading it here keeps the UI
                // thread out of the file system (N2).
                let settings = SettingsStore::open(config_file).settings().clone();
                let config = EngineConfig {
                    concurrency: usize::try_from(settings.concurrency.max(1)).unwrap_or(4),
                    bandwidth_bytes_per_second: settings.bandwidth_bytes_per_second,
                    scan_interval: Duration::from_secs(
                        u64::from(settings.scan_interval_minutes.max(1)) * 60,
                    ),
                    // R6.1: the attempt cap is user-settable, 8 by default.
                    attempt_cap: settings.attempt_cap.max(1),
                    // R3.7: the sync tag is opt-in and persisted.
                    tag_uploaded_assets: settings.tag_uploaded_assets,
                    ..EngineConfig::default()
                };
                match Engine::new(tracking, session, config) {
                    Ok(engine) => {
                        let engine = Arc::new(engine);
                        // R8.7: a pause set before the engine existed still
                        // holds.
                        if paused {
                            engine.pause();
                        }
                        engine.start();
                        Ok(engine)
                    }
                    Err(error) => Err(error.to_string()),
                }
            },
            move |model, started, cx| {
                if model.start_generation != start_generation {
                    // A newer start won the race. Stop the engine this attempt
                    // built before discarding it, so its watcher and workers
                    // do not keep running behind the surviving engine.
                    if let Ok(engine) = &started {
                        engine.stop();
                    }
                    return;
                }
                match started {
                    Ok(engine) => {
                        // R8.7: the pause state, whichever way it was clicked
                        // while the engine was being built, holds when it
                        // arrives here. Resume on a running gate is a no-op.
                        if model.paused {
                            engine.pause();
                        } else {
                            engine.resume();
                        }
                        // One live-settings applier per engine (R7.1, R7.2,
                        // R3.7). Its channel lives in the model, so replacing
                        // the engine drops the old sender and ends the old
                        // task.
                        if let Ok(runtime) = worker() {
                            let (changes, mut incoming) =
                                tokio::sync::watch::channel(LiveSettings {
                                    limits: engine.limits(),
                                    tag_uploaded_assets: engine.tag_status().enabled,
                                });
                            let applier = Arc::clone(&engine);
                            runtime.spawn(async move {
                                while incoming.changed().await.is_ok() {
                                    // Copy first: the borrow is not `Send`, so
                                    // it must not live across the await.
                                    let live = *incoming.borrow_and_update();
                                    applier.set_limits(live.limits).await;
                                    applier.set_tag_uploads(live.tag_uploaded_assets).await;
                                }
                            });
                            model.live = Some(changes);
                        }
                        model.engine = Some(engine);
                        model.problem = None;
                        tracing::info!("the sync engine is running");
                    }
                    Err(problem) => {
                        tracing::error!(error = %problem, "the sync engine could not start");
                        model.problem = Some(problem);
                    }
                }
                model.refresh_status(cx);
            },
        );
        if result.is_err() {
            self.problem = Some(WORKER_UNAVAILABLE.to_string());
            cx.notify();
        }
    }

    /// Republishes the engine's status, or a folder-only status before it runs.
    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        self.tag = match &self.engine {
            // R3.7: the tag state rides the same refresh, so the Settings
            // section sees the engine's one permission probe.
            Some(engine) => engine.tag_status(),
            None => TagStatus::default(),
        };
        self.status = match &self.engine {
            Some(engine) => engine.status(),
            None => {
                let store = self.store_model.read(cx);
                let folders = store
                    .folders()
                    .iter()
                    .zip(store.folder_counts())
                    .zip(store.folder_times())
                    .map(|((folder, counts), times)| FolderStatus {
                        path: folder.path.clone(),
                        recursive: folder.recursive,
                        album_mapping: folder.album_mapping,
                        album_scope: folder.album_scope,
                        available: Path::new(&folder.path).is_dir(),
                        // The last known counts from the store (R8.1).
                        counts: *counts,
                        last_error: None,
                        // #43: the last known scan and upload times, from the
                        // store the engine writes them to.
                        last_scan_ns: times.last_scan_ns,
                        last_upload_ns: times.last_upload_ns,
                        // The meter dies with the engine it belongs to; the
                        // store-only status has nothing on the wire (R8.1).
                        uploads: UploadTraffic::default(),
                    })
                    .collect::<Vec<_>>();
                Status::derive(StatusInputs {
                    folders,
                    paused: self.paused,
                    connection: ConnectionHealth::from_state(self.connection.read(cx).state()),
                    // R5.4: the cached list is the store's whole review,
                    // including items under a removed source folder, so it is
                    // the total the engine would have read.
                    review_total: Some(u32::try_from(self.review.len()).unwrap_or(u32::MAX)),
                    last_error: None,
                })
            }
        };
        self.publish_notifications(cx);
        cx.notify();
    }

    /// Posts what the notification policy answers for the current status and
    /// parked list (R5.4, R10.1).
    fn publish_notifications(&mut self, cx: &mut Context<Self>) {
        let reason = match self.connection.read(cx).state() {
            ConnectionState::NeedsAttention(attention) => Some(attention.message()),
            _ => None,
        };
        let failed = self.failed_read.then_some(self.failed.as_slice());
        for notification in self
            .notifier
            .observe(&self.status, failed, reason.as_deref())
        {
            cx.show_system_notification(notification);
        }
    }

    /// The shared store, once it has opened.
    #[must_use]
    pub fn store(&self, cx: &App) -> Option<Arc<StateStore>> {
        self.store_model.read(cx).store()
    }

    /// The current source folders, from the store the client tracks them in.
    #[must_use]
    pub fn folders(&self, cx: &App) -> Vec<SourceFolder> {
        self.store_model.read(cx).folders().to_vec()
    }
}

/// The status before the engine has anything.
fn empty_status() -> Status {
    Status {
        activity: Activity::Idle,
        folders: Vec::new(),
        counts: FolderCounts::default(),
        last_error: None,
        uploads: UploadTraffic::default(),
    }
}
