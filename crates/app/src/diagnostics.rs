//! The diagnostics model (SPEC R10.2, R10.4): the server version, the granted
//! permissions, the sqlite state, the last error per class, and the upload
//! throughput, plus the state rebuild.
//!
//! The wire reads go through the one shared session, so a 401 is answered by
//! the same re-login rule every other request uses (R1.4). The sqlite reads and
//! the rebuild run off the UI thread (N2).

use std::path::{Path, PathBuf};
use std::time::Instant;

use gpui_kit::*;
use sync_core::ClientError;
use sync_core::connection::{RequestCredential, SharedSession};
use sync_core::status::Status;
use sync_core::store::{ClassError, SqliteInfo};

use crate::connection::{ConnectionModel, WORKER_UNAVAILABLE, run_on_worker};
use crate::debug_bundle::{self, BundleReport, Snapshot};
use crate::engine::EngineModel;
use crate::store::StoreModel;

/// What the diagnostics view knows about the server (R10.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerInfo {
    /// The version `GET /server/about` reported.
    pub version: Option<String>,
    /// Why the version could not be read.
    pub problem: Option<String>,
    /// The granted permissions, or why they are not shown.
    pub permissions: Permissions,
}

/// What the client can say about the credential's permissions (R10.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Permissions {
    /// No credential is available yet.
    #[default]
    NotSignedIn,
    /// A password session carries the account's role permissions, not a key's
    /// list: `GET /api-keys/me` answers such a session with 403.
    Account,
    /// The API key's own granted permissions.
    Granted {
        /// The key's name in Immich.
        name: String,
        /// The permissions the key was granted.
        permissions: Vec<String>,
    },
    /// The key's permissions could not be read, with the reason.
    Unavailable(String),
}

/// What the Diagnostics view shows about the last bundle collection
/// (issue #25).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BundleState {
    /// No collection has run in this session.
    #[default]
    Idle,
    /// The collector is running on the background executor.
    Collecting,
    /// The archive that was written, with what it could not hold.
    Ready(BundleReport),
    /// The archive could not be written, with the reason.
    Failed(String),
}

/// The diagnostics view's model.
pub struct DiagnosticsModel {
    store: Entity<StoreModel>,
    engine: Entity<EngineModel>,
    connection: Entity<ConnectionModel>,
    /// The folder a collected archive is written to (#25). The shell passes
    /// [`crate::debug_bundle::bundle_directory`]; a test passes a temp dir.
    bundle_dir: PathBuf,
    bundle: BundleState,
    info: Option<SqliteInfo>,
    errors: Vec<ClassError>,
    server: ServerInfo,
    status: Status,
    throughput: Throughput,
    problem: Option<String>,
    rebuilding: bool,
    /// Bumped by every refresh, so a slow read cannot land over a newer one.
    read_generation: u64,
    _subscriptions: Vec<Subscription>,
}

impl DiagnosticsModel {
    /// Builds the model and starts the first reads. The store may still be
    /// opening and the connection may still be signing in, so every read is
    /// retried when those notify.
    pub fn new(
        store: Entity<StoreModel>,
        engine: Entity<EngineModel>,
        connection: Entity<ConnectionModel>,
        bundle_dir: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&store, |this, _, cx| this.refresh(cx)),
            cx.observe(&engine, |this, engine, cx| this.engine_changed(engine, cx)),
            cx.observe(&connection, |this, _, cx| this.probe(cx)),
        ];
        let mut model = Self {
            store,
            engine,
            connection,
            bundle_dir,
            bundle: BundleState::default(),
            info: None,
            errors: Vec::new(),
            server: ServerInfo::default(),
            status: Status {
                activity: sync_core::status::Activity::Idle,
                folders: Vec::new(),
                counts: sync_core::store::FolderCounts::default(),
                last_error: None,
            },
            throughput: Throughput::default(),
            problem: None,
            rebuilding: false,
            read_generation: 0,
            _subscriptions: subscriptions,
        };
        model.refresh(cx);
        model.probe(cx);
        model
    }

    /// The state database the view reports on.
    #[must_use]
    pub fn db_path(&self, cx: &App) -> PathBuf {
        self.store.read(cx).path().to_path_buf()
    }

    /// The last sqlite read.
    #[must_use]
    pub fn info(&self) -> Option<&SqliteInfo> {
        self.info.as_ref()
    }

    /// The last failure of each class the attempt log holds, newest first.
    #[must_use]
    pub fn errors(&self) -> &[ClassError] {
        &self.errors
    }

    /// The server version and permissions from the last probe.
    #[must_use]
    pub fn server(&self) -> &ServerInfo {
        &self.server
    }

    /// The engine's live status, for the throughput card.
    #[must_use]
    pub fn status(&self) -> &Status {
        &self.status
    }

    /// The upload rate from the engine's own byte meter (R10.2).
    #[must_use]
    pub fn throughput_bytes_per_second(&self) -> f64 {
        self.throughput.bytes_per_second()
    }

    /// The last read problem, if any.
    #[must_use]
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// Whether a rebuild is running.
    #[must_use]
    pub fn is_rebuilding(&self) -> bool {
        self.rebuilding
    }

    /// Where a collection saves its archive (#25), for the view's help text.
    #[must_use]
    pub fn bundle_directory(&self) -> &Path {
        &self.bundle_dir
    }

    /// What the last collection produced (#25).
    #[must_use]
    pub fn bundle(&self) -> &BundleState {
        &self.bundle
    }

    /// Collects the debug bundle on the background executor (issue #25).
    ///
    /// The values only this thread can read (the server version, the
    /// connection state, the engine's activity and rate) are sampled here; the
    /// file-system and database reads run in the background, and the result
    /// lands back here through `cx.spawn`. A second click while one runs is a
    /// no-op.
    pub fn collect_bundle(&mut self, cx: &mut Context<Self>) {
        if matches!(self.bundle, BundleState::Collecting) {
            return;
        }
        let (paths, store) = {
            let store = self.store.read(cx);
            (store.paths().clone(), store.store())
        };
        let snapshot = Snapshot {
            server_version: self.server.version.clone(),
            // The summary, never the attention message: a server-supplied
            // reason can quote a submitted credential (SPEC N3).
            connection: self.connection.read(cx).state().summary().to_string(),
            activity: self.status.activity.label().to_string(),
            throughput: format_rate(self.throughput_bytes_per_second()),
        };
        let directory = self.bundle_dir.clone();
        self.bundle = BundleState::Collecting;
        cx.notify();
        let collected = cx.background_executor().spawn(async move {
            debug_bundle::collect(&paths, store.as_deref(), &snapshot, &directory)
        });
        cx.spawn(async move |this, cx| {
            let result = collected.await;
            let _ = this.update(cx, |model, cx| {
                model.bundle = match result {
                    Ok(report) => BundleState::Ready(report),
                    Err(error) => BundleState::Failed(format!(
                        "The debug bundle could not be written: {error:#}"
                    )),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Re-reads the sqlite state and the last error per class, and re-checks
    /// the server (R10.2).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let (store, store_problem) = {
            let store = self.store.read(cx);
            (store.store(), store.problem().map(str::to_string))
        };
        let Some(store) = store else {
            // The state section has nothing to read, so it shows why the store
            // is not there instead of saying it is still opening for ever.
            self.problem = store_problem;
            cx.notify();
            return;
        };
        let read = cx
            .background_executor()
            .spawn(async move { (store.sqlite_info(), store.last_error_per_class()) });
        // A newer refresh supersedes an older one, so a slow read cannot land
        // over it (R10.2).
        self.read_generation = self.read_generation.wrapping_add(1);
        let generation = self.read_generation;
        cx.spawn(async move |this, cx| {
            let (info, errors) = read.await;
            let _ = this.update(cx, |model, cx| {
                if model.read_generation != generation {
                    return;
                }
                match info {
                    Ok(info) => {
                        model.info = Some(info);
                        model.problem = None;
                    }
                    Err(error) => {
                        model.problem =
                            Some(format!("The state database could not be read: {error}"));
                    }
                }
                match errors {
                    Ok(errors) => model.errors = errors,
                    Err(error) => {
                        model.problem =
                            Some(format!("The last failures could not be read: {error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        self.probe(cx);
    }

    /// Re-checks the server: the version, and the key's permissions when the
    /// credential is an API key (R10.2).
    pub fn probe(&mut self, cx: &mut Context<Self>) {
        let (session, connected) = {
            let connection = self.connection.read(cx);
            (connection.session(), connection.state().is_connected())
        };
        let Some(session) = session else {
            self.server = ServerInfo {
                permissions: Permissions::NotSignedIn,
                ..ServerInfo::default()
            };
            cx.notify();
            return;
        };
        if !connected {
            return;
        }
        let result = run_on_worker(
            cx,
            move || async move { probe_server(session).await },
            |model, info, cx| {
                model.server = info;
                cx.notify();
            },
        );
        if result.is_err() {
            self.server.problem = Some(WORKER_UNAVAILABLE.to_string());
            cx.notify();
        }
    }

    /// Rebuilds the state database and rescans the folders (R10.4). The engine
    /// restarts over the fresh store, runs its startup scan, and this model
    /// re-reads the state and re-checks the server when that settles.
    pub fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.info = None;
        self.errors.clear();
        self.problem = None;
        // A read already in flight must not land over the state this clears.
        self.read_generation = self.read_generation.wrapping_add(1);
        self.engine.update(cx, |engine, cx| engine.rebuild(cx));
        cx.notify();
    }

    /// Follows the engine: samples its byte meter and notices a rebuild ending.
    fn engine_changed(&mut self, engine: Entity<EngineModel>, cx: &mut Context<Self>) {
        let (status, uploaded_bytes, rebuilding) = {
            let engine = engine.read(cx);
            (
                engine.status().clone(),
                engine.uploaded_bytes(),
                engine.is_rebuilding(),
            )
        };
        self.status = status;
        self.throughput.record(uploaded_bytes, Instant::now());
        let was_rebuilding = self.rebuilding;
        self.rebuilding = rebuilding;
        if was_rebuilding && !rebuilding {
            // R10.4: the rebuild rescans the folders and re-checks the server.
            self.refresh(cx);
        }
        cx.notify();
    }
}

/// Reads the server version and, for an API key, its permissions (R10.2).
async fn probe_server(session: SharedSession) -> ServerInfo {
    let client = session.client();
    let mut info = ServerInfo::default();
    let about = session
        .request(|credential| {
            let client = client.clone();
            async move { client.about(credential.auth()).await }
        })
        .await;
    match about {
        Ok(about) => info.version = Some(about.version),
        Err(error) => info.problem = Some(about_problem(&error)),
    }
    info.permissions = match session.credential() {
        Some(RequestCredential::ApiKey(_)) => {
            let key = session
                .request(|credential| {
                    let client = client.clone();
                    async move { client.api_key_permissions(credential.auth()).await }
                })
                .await;
            match key {
                Ok(key) => Permissions::Granted {
                    name: key.name,
                    permissions: key.permissions,
                },
                Err(error) => Permissions::Unavailable(key_problem(&error)),
            }
        }
        Some(RequestCredential::Session(_)) => Permissions::Account,
        None => Permissions::NotSignedIn,
    };
    info
}

/// The user-facing reason the version could not be read (R10.2).
fn about_problem(error: &ClientError) -> String {
    match error {
        ClientError::PermissionDenied(message) => format!(
            "The server refused the version check: {message}. Grant the API key the \
             server.about permission in Immich, then refresh."
        ),
        other => format!("The server version could not be read: {other}"),
    }
}

/// The user-facing reason a key's permissions could not be read (R10.2).
fn key_problem(error: &ClientError) -> String {
    match error {
        ClientError::PermissionDenied(message) => {
            format!("The API key's permissions could not be read: {message}")
        }
        other => format!("The API key's permissions could not be read: {other}"),
    }
}

/// The upload rate, sampled from the engine's byte meter (R10.2).
#[derive(Clone, Debug, Default)]
pub struct Throughput {
    previous: Option<(Instant, u64)>,
    bytes_per_second: f64,
}

impl Throughput {
    /// Records one sample and returns the rate since the previous one.
    ///
    /// A meter that went backwards means the engine was rebuilt and started
    /// counting again, so the rate starts over instead of going negative.
    pub fn record(&mut self, total_bytes: u64, now: Instant) -> f64 {
        if let Some((at, bytes)) = self.previous {
            let elapsed = now.saturating_duration_since(at).as_secs_f64();
            if total_bytes < bytes {
                self.bytes_per_second = 0.0;
            } else if elapsed > 0.0 {
                self.bytes_per_second = (total_bytes - bytes) as f64 / elapsed;
            }
        }
        self.previous = Some((now, total_bytes));
        self.bytes_per_second
    }

    /// The rate from the last two samples.
    #[must_use]
    pub fn bytes_per_second(&self) -> f64 {
        self.bytes_per_second
    }
}

/// The rate as the view shows it, in the same binary units as the sizes.
#[must_use]
pub fn format_rate(bytes_per_second: f64) -> String {
    const KIB: f64 = 1024.0;
    if bytes_per_second >= KIB * KIB {
        format!("{:.1} MiB/s", bytes_per_second / (KIB * KIB))
    } else if bytes_per_second >= KIB {
        format!("{:.1} KiB/s", bytes_per_second / KIB)
    } else {
        format!("{bytes_per_second:.0} B/s")
    }
}

/// A byte count as the view shows it.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let bytes = bytes as f64;
    if bytes >= KIB * KIB * KIB {
        format!("{:.1} GiB", bytes / (KIB * KIB * KIB))
    } else if bytes >= KIB * KIB {
        format!("{:.1} MiB", bytes / (KIB * KIB))
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::{Throughput, format_bytes, format_rate};
    use std::time::{Duration, Instant};

    #[test]
    fn the_rate_follows_the_meter_up_and_down() {
        let start = Instant::now();
        let mut throughput = Throughput::default();
        assert_eq!(throughput.record(0, start), 0.0, "no baseline yet");
        assert_eq!(
            throughput.record(2_000_000, start + Duration::from_secs(1)),
            2_000_000.0,
            "one second moved two megabytes"
        );
        assert_eq!(
            throughput.record(2_000_000, start + Duration::from_secs(2)),
            0.0,
            "nothing moved in the second window"
        );
        assert_eq!(
            throughput.record(500, start + Duration::from_secs(3)),
            0.0,
            "a rebuilt engine starts its meter over"
        );
    }

    /// R10.2: a read a refresh left in flight must not land after a rebuild
    /// cleared the state; the rebuild supersedes it.
    #[gpui_kit::test]
    fn a_rebuild_supersedes_a_read_already_in_flight(cx: &mut gpui_kit::TestAppContext) {
        use crate::connection::ConnectionModel;
        use crate::engine::EngineModel;
        use crate::paths::AppPaths;
        use crate::store::StoreModel;
        use gpui_kit::AppContext as _;
        use std::sync::Arc;
        use sync_core::connection::ServerProfile;
        use sync_core::credential::{MemoryCredentialStore, SharedCredentialStore};

        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = AppPaths {
            config_file: temp.path().join("config").join("settings.toml"),
            state_db: temp.path().join("data").join("state.db"),
            autostart_dir: temp.path().join("config").join("autostart"),
        };
        let credentials: SharedCredentialStore = Arc::new(MemoryCredentialStore::new());
        let diagnostics = cx.update(|cx| {
            let connection =
                cx.new(|_| ConnectionModel::new(ServerProfile::default(), credentials));
            let store = cx.new(|cx| StoreModel::open(&paths, cx));
            let engine =
                cx.new(|cx| EngineModel::new(connection.clone(), store.clone(), &paths, cx));
            cx.new(|cx| {
                super::DiagnosticsModel::new(
                    store,
                    engine,
                    connection,
                    temp.path().join("downloads"),
                    cx,
                )
            })
        });
        // The store opens on the background executor; the initial refresh only
        // starts a read once it is there.
        cx.run_until_parked();

        cx.update(|cx| {
            diagnostics.update(cx, |model, cx| {
                model.refresh(cx);
                let pending = model.read_generation;
                model.rebuild(cx);
                assert_ne!(
                    model.read_generation, pending,
                    "the read refresh left in flight must not land over the rebuilt state"
                );
            });
        });
    }

    #[test]
    fn rates_and_sizes_are_formatted_for_reading() {
        assert_eq!(format_rate(0.0), "0 B/s");
        assert_eq!(format_rate(900.0), "900 B/s");
        assert_eq!(format_rate(1_500.0), "1.5 KiB/s");
        assert_eq!(format_rate(2_500_000.0), "2.4 MiB/s");
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(4096), "4.0 KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }
}
