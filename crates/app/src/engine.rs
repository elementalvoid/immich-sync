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
use sync_core::engine::{Activity, Engine, EngineConfig, EngineStatus, FolderStatus, Limits};
use sync_core::store::{
    AlbumScope, FailedItem, FolderCounts, ReviewDecision, ReviewItem, SourceFolder, StateStore,
};

use crate::connection::{ConnectionModel, worker};
use crate::paths::AppPaths;
use crate::settings::SettingsStore;
use crate::status::DashboardModel;
use crate::store::StoreModel;
use sync_core::connection::SharedSession;

/// How often the dashboard picks up the engine's live counts.
const STATUS_POLL: Duration = Duration::from_millis(250);

/// How often the failed-items list and the deletion review are re-read from
/// the store (R6.5, R5.3).
///
/// The read runs on the background executor, never on the UI thread (N2), so
/// this only decides how soon a parked item or a missing file shows up.
const FAILED_POLL_TICKS: u32 = 4;

/// The app-scoped engine and the status the views render.
pub struct EngineModel {
    connection: Entity<ConnectionModel>,
    store_model: Entity<StoreModel>,
    paths: AppPaths,
    engine: Option<Arc<Engine>>,
    /// The newest limit change for the running engine. One applier task per
    /// engine drains it, so two quick edits cannot land out of order.
    limits: Option<tokio::sync::watch::Sender<Limits>>,
    status: EngineStatus,
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
    /// The review count the last notification was based on, so the 250 ms
    /// status poll cannot post the same notification twice (R5.4).
    notified_review: u32,
    paused: bool,
    problem: Option<String>,
    /// Bumped by every folder edit, so an older edit result cannot overwrite a
    /// newer one.
    generation: u64,
    /// Bumped by every engine start attempt, kept apart from `generation` so a
    /// folder edit cannot discard an engine that is already being built.
    start_generation: u64,
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

impl AlbumChange {
    /// Applies the change to one folder row.
    fn apply(self, folder: &mut SourceFolder) {
        match self {
            Self::Mapping(enabled) => folder.album_mapping = enabled,
            Self::Scope(scope) => folder.album_scope = scope,
        }
    }
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
            // The status and the review list are refreshed when the store opens,
            // so a review that already waits in the database raises its
            // notification and fills the view at start (R5.3, R5.4).
            cx.observe(&store_model, |this, _, cx| {
                this.try_start_engine(cx);
                this.refresh_review(cx);
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
            limits: None,
            status: empty_status(),
            failed: Vec::new(),
            review: Vec::new(),
            review_problem: None,
            notified_review: 0,
            paused: false,
            problem: None,
            generation: 0,
            start_generation: 0,
            _subscriptions: subscriptions,
            _poll: poll,
        }
    }

    /// The engine's latest status.
    #[must_use]
    pub fn status(&self) -> &EngineStatus {
        &self.status
    }

    /// The last model problem, if any.
    #[must_use]
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
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
        let engine = self.engine.clone();
        let store = self.store_model.read(cx).store();
        let Ok(runtime) = worker() else {
            self.review_problem = Some("The network runtime could not start.".to_string());
            cx.notify();
            return;
        };
        let (results, incoming) = async_channel::bounded(1);
        runtime.spawn(async move {
            // N2: the store write belongs on the worker, not the UI thread.
            let result = match engine {
                Some(engine) => engine
                    .keep_review_item(&path)
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
                None => match store {
                    Some(store) => store
                        .decide_review(&path, ReviewDecision::Keep)
                        .map(|_| ())
                        .map_err(|error| error.to_string()),
                    None => Err("The state database is still opening.".to_string()),
                },
            };
            let _ = results.send(result).await;
        });
        self.finish_review_action(incoming, cx);
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
        let Ok(runtime) = worker() else {
            self.review_problem = Some("The network runtime could not start.".to_string());
            cx.notify();
            return;
        };
        let (results, incoming) = async_channel::bounded(1);
        runtime.spawn(async move {
            let result = engine
                .trash_review_item(&path)
                .await
                .map_err(|error| crate::review::trash_problem(&error));
            let _ = results.send(result).await;
        });
        self.finish_review_action(incoming, cx);
    }

    /// Publishes the outcome of a keep or a trash and refreshes the rows
    /// (R5.3-R5.5).
    fn finish_review_action(
        &mut self,
        incoming: async_channel::Receiver<Result<(), String>>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let Ok(result) = incoming.recv().await else {
                return;
            };
            let _ = this.update(cx, |model, cx| {
                match result {
                    Ok(()) => model.review_problem = None,
                    Err(problem) => model.review_problem = Some(problem),
                }
                model.refresh_review(cx);
                // R5.4: the tray count follows the decision at once.
                model.refresh_status(cx);
            });
        })
        .detach();
    }

    /// Re-reads the missing files from the store on the background executor
    /// (N2) and republishes them when they changed.
    pub fn refresh_review(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.store_model.read(cx).store() else {
            return;
        };
        let read = cx
            .background_executor()
            .spawn(async move { store.review_items() });
        cx.spawn(async move |this, cx| {
            let read = read.await;
            let _ = this.update(cx, |model, cx| match read {
                Ok(items) => {
                    if items != model.review {
                        model.review = items;
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
        let engine = self.engine.clone();
        let store = self.store_model.read(cx).store();
        let Ok(runtime) = worker() else {
            self.problem = Some("The network runtime could not start.".to_string());
            cx.notify();
            return;
        };
        let (results, incoming) = async_channel::bounded(1);
        runtime.spawn(async move {
            // The store write runs on the worker runtime, so the UI thread
            // never waits behind a worker's `BEGIN IMMEDIATE` (N2).
            let result = match engine {
                Some(engine) => engine
                    .retry_failed(&path)
                    .map(|_| ())
                    .map_err(|error| error.to_string()),
                None => match store {
                    Some(store) => store
                        .retry_item(&path)
                        .map(|_| ())
                        .map_err(|error| error.to_string()),
                    None => Err("The state database is still opening.".to_string()),
                },
            };
            let _ = results.send(result).await;
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = incoming.recv().await else {
                return;
            };
            let _ = this.update(cx, |model, cx| {
                match result {
                    Ok(()) => model.problem = None,
                    Err(problem) => model.problem = Some(problem),
                }
                model.refresh_failed(cx);
            });
        })
        .detach();
    }

    /// Re-reads the parked items from the store on the background executor
    /// (N2) and republishes them when they changed.
    pub fn refresh_failed(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.store_model.read(cx).store() else {
            return;
        };
        let read = cx
            .background_executor()
            .spawn(async move { store.failed_items() });
        cx.spawn(async move |this, cx| {
            let read = read.await;
            let _ = this.update(cx, |model, cx| match read {
                Ok(items) => {
                    if items != model.failed {
                        model.failed = items;
                        cx.notify();
                    }
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
        if let Some(limits) = &self.limits {
            let _ = limits.send(Limits {
                bandwidth_bytes_per_second,
                concurrency: concurrency.max(1),
            });
        }
    }

    /// Adds a source folder and starts discovering it (R2.1).
    pub fn add_folder(&mut self, path: String, recursive: bool, cx: &mut Context<Self>) {
        let path = path.trim().to_string();
        if path.is_empty() {
            self.problem = Some("Enter the path of a folder to add.".to_string());
            cx.notify();
            return;
        }
        // A newer edit supersedes an older result still in flight.
        self.generation = self.generation.wrapping_add(1);
        let engine = self.engine.clone();
        let store = self.store_model.read(cx).store();
        let Ok(runtime) = worker() else {
            self.problem = Some("The network runtime could not start.".to_string());
            cx.notify();
            return;
        };
        let generation = self.generation;
        let (results, incoming) = async_channel::bounded(1);
        runtime.spawn(async move {
            // The store write runs on the worker runtime, so the UI thread
            // never waits behind a worker's `BEGIN IMMEDIATE` (N2).
            let result = match engine {
                Some(engine) => engine
                    .add_folder(&path, recursive)
                    .map(|()| None)
                    .map_err(|error| error.to_string()),
                None => match store {
                    Some(store) => match store.add_folder(&SourceFolder {
                        path: path.clone(),
                        recursive,
                        // R3.4: adding a folder never opts it into album
                        // mapping; the card's switch does that on purpose.
                        album_mapping: false,
                        album_scope: AlbumScope::default(),
                    }) {
                        Ok(()) => store.folders().map(Some).map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    },
                    None => Err("The state database is still opening.".to_string()),
                },
            };
            let _ = results.send(result).await;
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = incoming.recv().await else {
                return;
            };
            let _ = this.update(cx, |model, cx| {
                if model.generation != generation {
                    return;
                }
                match result {
                    Ok(folders) => {
                        if let Some(folders) = folders {
                            model.store_model.update(cx, |store, cx| {
                                store.set_folders(folders, cx);
                            });
                        }
                        model.problem = None;
                    }
                    Err(problem) => model.problem = Some(problem),
                }
                model.refresh_status(cx);
            });
        })
        .detach();
    }

    /// Removes a source folder; its items stay in the store (R2.1).
    pub fn remove_folder(&mut self, path: String, cx: &mut Context<Self>) {
        // A newer edit supersedes an older result still in flight.
        self.generation = self.generation.wrapping_add(1);
        let engine = self.engine.clone();
        let store = self.store_model.read(cx).store();
        let Ok(runtime) = worker() else {
            self.problem = Some("The network runtime could not start.".to_string());
            cx.notify();
            return;
        };
        let generation = self.generation;
        let (results, incoming) = async_channel::bounded(1);
        runtime.spawn(async move {
            // N2: the store write belongs on the worker, not the UI thread.
            let result = match engine {
                Some(engine) => engine
                    .remove_folder(&path)
                    .map(|()| None)
                    .map_err(|error| error.to_string()),
                None => match store {
                    Some(store) => match store.remove_folder(&path) {
                        Ok(_) => store.folders().map(Some).map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    },
                    None => Ok(None),
                },
            };
            let _ = results.send(result).await;
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = incoming.recv().await else {
                return;
            };
            let _ = this.update(cx, |model, cx| {
                if model.generation != generation {
                    return;
                }
                match result {
                    Ok(folders) => {
                        if let Some(folders) = folders {
                            model.store_model.update(cx, |store, cx| {
                                store.set_folders(folders, cx);
                            });
                        }
                        model.problem = None;
                    }
                    Err(problem) => model.problem = Some(problem),
                }
                model.refresh_status(cx);
            });
        })
        .detach();
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
        let engine = self.engine.clone();
        let store = self.store_model.read(cx).store();
        let Ok(runtime) = worker() else {
            self.problem = Some("The network runtime could not start.".to_string());
            cx.notify();
            return;
        };
        let generation = self.generation;
        let (results, incoming) = async_channel::bounded(1);
        runtime.spawn(async move {
            // N2: the store write belongs on the worker, not the UI thread.
            let result = match engine {
                Some(engine) => match change {
                    AlbumChange::Mapping(enabled) => engine.set_album_mapping(&path, enabled),
                    AlbumChange::Scope(scope) => engine.set_album_scope(&path, scope),
                }
                .map(|()| None)
                .map_err(|error| error.to_string()),
                None => match store {
                    Some(store) => match store.folders() {
                        Ok(folders) => match folders.into_iter().find(|folder| folder.path == path)
                        {
                            Some(mut folder) => {
                                change.apply(&mut folder);
                                match store.add_folder(&folder) {
                                    Ok(()) => store.folders().map(Some),
                                    Err(error) => Err(error),
                                }
                                .map_err(|error| error.to_string())
                            }
                            None => Err("That folder is not in the client.".to_string()),
                        },
                        Err(error) => Err(error.to_string()),
                    },
                    None => Err("The state database is still opening.".to_string()),
                },
            };
            let _ = results.send(result).await;
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = incoming.recv().await else {
                return;
            };
            let _ = this.update(cx, |model, cx| {
                if model.generation != generation {
                    return;
                }
                match result {
                    Ok(Some(folders)) => {
                        model
                            .store_model
                            .update(cx, |store, cx| store.set_folders(folders, cx));
                        model.problem = None;
                    }
                    Ok(None) => model.problem = None,
                    Err(problem) => model.problem = Some(problem),
                }
                model.refresh_status(cx);
            });
        })
        .detach();
    }

    /// Starts the engine when it is not running yet and both inputs exist.
    fn try_start_engine(&mut self, cx: &mut Context<Self>) {
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
        let Some(store) = self.store_model.read(cx).store() else {
            self.refresh_status(cx);
            return;
        };
        let Some(session) = self.connection.read(cx).session() else {
            self.refresh_status(cx);
            return;
        };
        let Ok(runtime) = worker() else {
            self.problem = Some("The network runtime could not start.".to_string());
            cx.notify();
            return;
        };
        let config_file = self.paths.config_file.clone();
        let paused = self.paused;
        self.start_generation = self.start_generation.wrapping_add(1);
        let start_generation = self.start_generation;
        let (results, incoming) = async_channel::bounded(1);
        runtime.spawn(async move {
            // The settings file is small; reading it here keeps the UI thread
            // out of the file system (N2).
            let settings = SettingsStore::open(config_file).settings().clone();
            let config = EngineConfig {
                concurrency: usize::try_from(settings.concurrency.max(1)).unwrap_or(4),
                bandwidth_bytes_per_second: settings.bandwidth_bytes_per_second,
                scan_interval: Duration::from_secs(
                    u64::from(settings.scan_interval_minutes.max(1)) * 60,
                ),
                // R6.1: the attempt cap is user-settable, 8 by default.
                attempt_cap: settings.attempt_cap.max(1),
                ..EngineConfig::default()
            };
            let started = match Engine::new(store, session, config) {
                Ok(engine) => {
                    let engine = Arc::new(engine);
                    // R8.7: a pause set before the engine existed still holds.
                    if paused {
                        engine.pause();
                    }
                    engine.start();
                    Ok(engine)
                }
                Err(error) => Err(error.to_string()),
            };
            let _ = results.send(started).await;
        });
        cx.spawn(async move |this, cx| {
            let Ok(started) = incoming.recv().await else {
                return;
            };
            let _ = this.update(cx, |model, cx| {
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
                        // One limits applier per engine (R7.1, R7.2). Its
                        // channel lives in the model, so replacing the engine
                        // drops the old sender and ends the old task.
                        if let Ok(runtime) = worker() {
                            let (changes, mut incoming) =
                                tokio::sync::watch::channel(engine.limits());
                            let applier = Arc::clone(&engine);
                            runtime.spawn(async move {
                                while incoming.changed().await.is_ok() {
                                    // Copy first: the borrow is not `Send`, so
                                    // it must not live across the await.
                                    let limits = *incoming.borrow_and_update();
                                    applier.set_limits(limits).await;
                                }
                            });
                            model.limits = Some(changes);
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
            });
        })
        .detach();
    }

    /// Republishes the engine's status, or a folder-only status before it runs.
    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        self.status = match &self.engine {
            Some(engine) => engine.status(),
            None => {
                let store = self.store_model.read(cx);
                let folders = store
                    .folders()
                    .iter()
                    .zip(store.folder_counts())
                    .map(|(folder, counts)| FolderStatus {
                        path: folder.path.clone(),
                        recursive: folder.recursive,
                        album_mapping: folder.album_mapping,
                        album_scope: folder.album_scope,
                        available: Path::new(&folder.path).is_dir(),
                        // The last known counts from the store (R8.1).
                        counts: *counts,
                        last_error: None,
                    })
                    .collect::<Vec<_>>();
                let paused = self.paused;
                let unavailable = folders.iter().any(|folder| !folder.available);
                let mut counts = FolderCounts::default();
                for folder in &folders {
                    counts.pending += folder.counts.pending;
                    counts.uploaded += folder.counts.uploaded;
                    counts.failed += folder.counts.failed;
                    counts.review += folder.counts.review;
                }
                EngineStatus {
                    activity: if paused {
                        Activity::Paused
                    } else if unavailable {
                        Activity::NeedsAttention
                    } else if counts.pending > 0 {
                        Activity::Syncing
                    } else if counts.failed > 0 || counts.review > 0 {
                        Activity::NeedsAttention
                    } else {
                        Activity::Idle
                    },
                    folders,
                    counts,
                    last_error: None,
                }
            }
        };
        // R5.4, R10.1: a new review item is the one thing the tray cannot show
        // on its own, so it also raises a system notification. The count the
        // last one was based on is kept, so the poll cannot repeat it.
        let review = self.status.counts.review;
        if let Some(notification) =
            crate::shell::new_review_notification(review, self.notified_review)
        {
            tracing::info!(
                count = review,
                "R5.4: the deletion review raised a notification"
            );
            cx.show_system_notification(notification);
        }
        self.notified_review = review;
        cx.notify();
    }

    /// The shared store, once it has opened.
    #[must_use]
    pub fn store(&self, cx: &App) -> Option<Arc<StateStore>> {
        self.store_model.read(cx).store()
    }

    /// The current source folders, from the engine or the stored list.
    #[must_use]
    pub fn folders(&self, cx: &App) -> Vec<SourceFolder> {
        match &self.engine {
            Some(engine) => engine
                .status()
                .folders
                .iter()
                .map(|folder| SourceFolder {
                    path: folder.path.clone(),
                    recursive: folder.recursive,
                    album_mapping: folder.album_mapping,
                    album_scope: folder.album_scope,
                })
                .collect(),
            None => self.store_model.read(cx).folders().to_vec(),
        }
    }
}

/// The status before the engine has anything.
fn empty_status() -> EngineStatus {
    EngineStatus {
        activity: Activity::Idle,
        folders: Vec::new(),
        counts: FolderCounts::default(),
        last_error: None,
    }
}
