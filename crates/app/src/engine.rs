//! The app-scoped folder sync engine (SPEC R2.1–R2.5, R8.1–R8.3, R8.7).
//!
//! The model owns the engine for the process lifetime, so hiding the window
//! does not stop a sync. It starts the engine when a credential is available,
//! persists folder changes while the client is signed out, and republishes the
//! engine's status to the dashboard on a short poll.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::*;
use sync_core::engine::{Activity, Engine, EngineConfig, EngineStatus, FolderStatus, Limits};
use sync_core::store::{
    AlbumScope, FailedItem, FolderCounts, ReviewDecision, ReviewItem, SourceFolder, StateStore,
};

use crate::connection::{
    ConnectionModel, STORE_OPENING, WORKER_UNAVAILABLE, run_on_worker, worker,
};
use crate::paths::AppPaths;
use crate::settings::SettingsStore;
use crate::status::DashboardModel;
use crate::store::StoreModel;
use sync_core::connection::{ConnectionState, SharedSession};

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
    /// Whether the attention notification already fired for the current
    /// attention episode (R10.1). It clears when the activity leaves attention,
    /// so a later episode notifies again.
    notified_attention: bool,
    /// The failed items the attention and failure notifications already cover
    /// (R10.1), so a repeated read of the same parked item stays quiet while a
    /// newly parked one raises its own notification.
    notified_failed: HashSet<String>,
    paused: bool,
    problem: Option<String>,
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
            cx.observe(&store_model, |this, store, cx| {
                let generation = store.read(cx).generation();
                this.store_changed(generation, cx);
                // A decision with no engine updates the store's cached counts,
                // which do not move the generation, so the status follows every
                // store notification (R5.4).
                this.refresh_status(cx);
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
            notified_attention: false,
            notified_failed: HashSet::new(),
            paused: false,
            problem: None,
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
        let action_path = path.clone();
        let result = run_on_worker(
            cx,
            move || async move {
                // N2: the store write belongs on the worker, not the UI thread.
                match engine {
                    Some(engine) => engine
                        .keep_review_item(&action_path)
                        .map(|_| ())
                        .map_err(|error| error.to_string()),
                    None => match store {
                        Some(store) => store
                            .decide_review(&action_path, ReviewDecision::Keep)
                            .map(|_| ())
                            .map_err(|error| error.to_string()),
                        None => Err(STORE_OPENING.to_string()),
                    },
                }
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
        let result = run_on_worker(
            cx,
            move || async move {
                // The store write runs on the worker runtime, so the UI thread
                // never waits behind a worker's `BEGIN IMMEDIATE` (N2).
                match engine {
                    Some(engine) => engine
                        .retry_failed(&path)
                        .map(|_| ())
                        .map_err(|error| error.to_string()),
                    None => match store {
                        Some(store) => store
                            .retry_item(&path)
                            .map(|_| ())
                            .map_err(|error| error.to_string()),
                        None => Err(STORE_OPENING.to_string()),
                    },
                }
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
                    // R10.1: a newly parked item raises a notification of its
                    // own. An item that left the list forgets it was announced,
                    // so a later failure notifies again.
                    model
                        .notified_failed
                        .retain(|path| model.failed.iter().any(|item| item.path == *path));
                    let fresh: Vec<String> = model
                        .failed
                        .iter()
                        .filter(|item| !model.notified_failed.contains(&item.path))
                        .map(|item| item.path.clone())
                        .collect();
                    if !fresh.is_empty() {
                        tracing::info!(
                            count = fresh.len(),
                            "R10.1: a failed item raised a notification"
                        );
                        model.notified_failed.extend(fresh.iter().cloned());
                        cx.show_system_notification(crate::shell::new_failure_notification(&fresh));
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
        self.limits = None;
        self.rebuilding = true;
        self.status = empty_status();
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
        let generation = self.generation;
        let result = run_on_worker(
            cx,
            move || async move {
                // The store write runs on the worker runtime, so the UI thread
                // never waits behind a worker's `BEGIN IMMEDIATE` (N2).
                match engine {
                    Some(engine) => engine
                        .add_folder(&path, recursive)
                        .map(|()| None)
                        .map_err(|error| error.to_string()),
                    None => match store {
                        Some(store) => match store.folders() {
                            // R3.4: a new folder never opts into album mapping,
                            // but editing an existing one (the recursion
                            // toggle) must keep the album settings it already
                            // has. The lookup compares path keys (R2.4),
                            // so a re-add spelled differently still edits the
                            // row it already has.
                            Ok(folders) => {
                                let case = sync_core::paths::PathCase::for_platform();
                                let existing = folders.into_iter().find(|folder| {
                                    sync_core::paths::equal(
                                        Path::new(&folder.path),
                                        Path::new(&path),
                                        case,
                                    )
                                });
                                let album_mapping =
                                    existing.as_ref().is_some_and(|folder| folder.album_mapping);
                                let album_scope = existing
                                    .map_or_else(AlbumScope::default, |folder| folder.album_scope);
                                match store.add_folder(&SourceFolder {
                                    path: path.clone(),
                                    recursive,
                                    album_mapping,
                                    album_scope,
                                }) {
                                    Ok(()) => {
                                        store.folders().map(Some).map_err(|error| error.to_string())
                                    }
                                    Err(error) => Err(error.to_string()),
                                }
                            }
                            Err(error) => Err(error.to_string()),
                        },
                        None => Err(STORE_OPENING.to_string()),
                    },
                }
            },
            move |model, result, cx| {
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
            },
        );
        if result.is_err() {
            self.problem = Some(WORKER_UNAVAILABLE.to_string());
            cx.notify();
        }
    }

    /// Removes a source folder; its items stay in the store (R2.1).
    pub fn remove_folder(&mut self, path: String, cx: &mut Context<Self>) {
        // A newer edit supersedes an older result still in flight.
        self.generation = self.generation.wrapping_add(1);
        let engine = self.engine.clone();
        let store = self.store_model.read(cx).store();
        let generation = self.generation;
        let result = run_on_worker(
            cx,
            move || async move {
                // N2: the store write belongs on the worker, not the UI thread.
                match engine {
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
                }
            },
            move |model, result, cx| {
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
        let engine = self.engine.clone();
        let store = self.store_model.read(cx).store();
        let generation = self.generation;
        let result = run_on_worker(
            cx,
            move || async move {
                // N2: the store write belongs on the worker, not the UI thread.
                match engine {
                    Some(engine) => match change {
                        AlbumChange::Mapping(enabled) => engine.set_album_mapping(&path, enabled),
                        AlbumChange::Scope(scope) => engine.set_album_scope(&path, scope),
                    }
                    .map(|()| None)
                    .map_err(|error| error.to_string()),
                    None => match store {
                        Some(store) => match store.folders() {
                            Ok(folders) => {
                                match folders.into_iter().find(|folder| folder.path == path) {
                                    Some(mut folder) => {
                                        change.apply(&mut folder);
                                        match store.add_folder(&folder) {
                                            Ok(()) => store.folders().map(Some),
                                            Err(error) => Err(error),
                                        }
                                        .map_err(|error| error.to_string())
                                    }
                                    None => Err("That folder is not in the client.".to_string()),
                                }
                            }
                            Err(error) => Err(error.to_string()),
                        },
                        None => Err(STORE_OPENING.to_string()),
                    },
                }
            },
            move |model, result, cx| {
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
        let Some(store) = self.store_model.read(cx).store() else {
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
                    ..EngineConfig::default()
                };
                match Engine::new(store, session, config) {
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
            },
        );
        if result.is_err() {
            self.problem = Some(WORKER_UNAVAILABLE.to_string());
            cx.notify();
        }
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

        // R10.1: a lost connection, a refused credential, or a folder that
        // cannot be read is the other thing the tray cannot show on its own.
        self.refresh_attention_notification(cx);
        cx.notify();
    }

    /// Raises the attention notification once per attention episode (R10.1).
    ///
    /// A failed item has its own notification, so the generic one stays quiet
    /// while the only reason is parked failures; the connection state is the
    /// one reason that must speak even when the engine is not running yet.
    fn refresh_attention_notification(&mut self, cx: &mut Context<Self>) {
        let connection_attention = matches!(
            self.connection.read(cx).state(),
            ConnectionState::NeedsAttention(_)
        );
        let attention = self.status.activity == Activity::NeedsAttention || connection_attention;
        if !attention {
            self.notified_attention = false;
            return;
        }
        let Some(reason) = self.attention_reason(cx) else {
            return;
        };
        if let Some(notification) =
            crate::shell::new_attention_notification(attention, self.notified_attention, &reason)
        {
            tracing::info!(
                reason = %reason,
                "R10.1: the client entered attention and raised a notification"
            );
            cx.show_system_notification(notification);
        }
        self.notified_attention = true;
    }

    /// The user-facing reason the client needs attention, when it is not a
    /// deletion review or a parked failure. `None` lets the generic
    /// notification stay quiet; the specific notification covers those.
    ///
    /// A permanent folder error keeps the engine's global error too, so it
    /// speaks even with no parked item. A transient retry sets only the
    /// folder's own error, so it stays quiet.
    fn attention_reason(&self, cx: &Context<Self>) -> Option<String> {
        if let ConnectionState::NeedsAttention(attention) = self.connection.read(cx).state() {
            return Some(attention.message());
        }
        if self.status.counts.failed > 0 {
            return None;
        }
        if let Some(folder) = self.status.folders.iter().find(|folder| !folder.available) {
            return Some(format!("The source folder {} is unavailable.", folder.path));
        }
        self.status.last_error.clone()
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
