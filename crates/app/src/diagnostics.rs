//! The diagnostics model (SPEC R10.2, R10.4): the server version, the granted
//! permissions, the sqlite state, the last error per class, and the upload
//! throughput, plus the state rebuild.
//!
//! The wire reads go through the one shared session, so a 401 is answered by
//! the same re-login rule every other request uses (R1.4). The sqlite reads and
//! the rebuild run off the UI thread (N2).

use std::path::PathBuf;
use std::time::Instant;

use gpui_kit::*;
use sync_core::ClientError;
use sync_core::connection::{RequestCredential, SharedSession};
use sync_core::engine::EngineStatus;
use sync_core::store::{ClassError, SqliteInfo};

use crate::connection::{ConnectionModel, worker};
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

/// The diagnostics view's model.
pub struct DiagnosticsModel {
    store: Entity<StoreModel>,
    engine: Entity<EngineModel>,
    connection: Entity<ConnectionModel>,
    info: Option<SqliteInfo>,
    errors: Vec<ClassError>,
    server: ServerInfo,
    status: EngineStatus,
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
            info: None,
            errors: Vec::new(),
            server: ServerInfo::default(),
            status: EngineStatus {
                activity: sync_core::engine::Activity::Idle,
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
    pub fn status(&self) -> &EngineStatus {
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
        let Ok(runtime) = worker() else {
            self.server.problem = Some("The network runtime could not start.".to_string());
            cx.notify();
            return;
        };
        let (results, incoming) = async_channel::bounded(1);
        runtime.spawn(async move {
            let _ = results.send(probe_server(session).await).await;
        });
        cx.spawn(async move |this, cx| {
            let Ok(info) = incoming.recv().await else {
                return;
            };
            let _ = this.update(cx, |model, cx| {
                model.server = info;
                cx.notify();
            });
        })
        .detach();
    }

    /// Rebuilds the state database and rescans the folders (R10.4). The engine
    /// restarts over the fresh store, runs its startup scan, and this model
    /// re-reads the state and re-checks the server when that settles.
    pub fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.info = None;
        self.errors.clear();
        self.problem = None;
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
