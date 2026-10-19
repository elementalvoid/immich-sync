//! The rotating file log (SPEC R10.3, N3).
//!
//! The log file lives in the state database's directory (SPEC R9.2), rotates
//! under an injected [`LogPolicy`], and keeps at most `max_files` files. The
//! level is a persisted setting that [`LogControl::set_level`] applies to the
//! running subscriber, so a change needs no restart.
//!
//! Redaction is a property of the recorded types: `Secret` and `SessionToken`
//! redact their `Debug` output, and no event formats `Secret::expose`.
//! `crates/app/tests/log_redaction.rs` greps the real file after real sign-ins
//! to prove it.

use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use tracing::Subscriber;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::Registry;
use tracing_subscriber::filter::{LevelFilter, Targets};

use crate::version;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::reload;

use crate::paths::AppPaths;

/// The prefix every log file shares.
pub const LOG_PREFIX: &str = "immich-sync.log";
/// How many log files the appender keeps by default, including the current one.
pub const DEFAULT_MAX_FILES: usize = 5;
/// How many lines the Logs view reads.
pub const TAIL_LINES: usize = 200;
/// How many bytes the tail reads at most, so a large file is never loaded whole.
pub const TAIL_BYTES: u64 = 128 * 1024;

/// The persisted log level (SPEC R10.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Every event, for diagnosing one hard problem.
    Trace,
    /// Diagnostic detail.
    Debug,
    /// Lifecycle events. The default.
    #[default]
    Info,
    /// Recoverable problems.
    Warn,
    /// Failures that stop the client from working.
    Error,
}

impl LogLevel {
    /// Every level, in the order the Logs view shows them.
    pub const ALL: [Self; 5] = [
        Self::Trace,
        Self::Debug,
        Self::Info,
        Self::Warn,
        Self::Error,
    ];

    /// The subscriber filter this level selects.
    #[must_use]
    pub fn filter(self) -> LevelFilter {
        match self {
            Self::Trace => LevelFilter::TRACE,
            Self::Debug => LevelFilter::DEBUG,
            Self::Info => LevelFilter::INFO,
            Self::Warn => LevelFilter::WARN,
            Self::Error => LevelFilter::ERROR,
        }
    }

    /// The name the Logs view shows.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Trace => "Trace",
            Self::Debug => "Debug",
            Self::Info => "Info",
            Self::Warn => "Warn",
            Self::Error => "Error",
        }
    }

    /// The name the settings file stores.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    /// The element id of the Logs view's control for this level.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Trace => "logs-level-trace",
            Self::Debug => "logs-level-debug",
            Self::Info => "logs-level-info",
            Self::Warn => "logs-level-warn",
            Self::Error => "logs-level-error",
        }
    }
}

/// How the log rotates and how many files it keeps.
///
/// The policy is a value, not a constant, so a test can inject a small cap and
/// exercise the pruning the appender runs when it opens the directory.
/// `tracing-appender` reads its own clock, so the prune that runs at a timed
/// rollover uses the same code path but cannot be triggered deterministically;
/// see `crates/app/tests/logging.rs`.
#[derive(Clone, Debug)]
pub struct LogPolicy {
    /// When a new log file starts.
    pub rotation: Rotation,
    /// How many files the appender keeps, including the current one.
    pub max_files: usize,
}

impl Default for LogPolicy {
    fn default() -> Self {
        Self {
            rotation: Rotation::DAILY,
            max_files: DEFAULT_MAX_FILES,
        }
    }
}

/// The installed subscriber's control, or `None` before [`LogSink::install`].
static CONTROL: OnceLock<LogControl> = OnceLock::new();
/// The non-blocking writer's flush guard, until [`shutdown`] drops it.
static WRITER_GUARD: Mutex<Option<WorkerGuard>> = Mutex::new(None);

/// The running subscriber's control, when logging is installed.
#[must_use]
pub fn control() -> Option<&'static LogControl> {
    CONTROL.get()
}

/// The target the level-change record uses.
///
/// [`targets`] pins it open, so the record lands whatever the previous and the
/// new level are: a change from `Error` down to `Debug` would otherwise be
/// emitted under a filter that drops it.
const LEVEL_CHANGE_TARGET: &str = "immich_sync::logging::level_change";

/// Flushes the queued log lines and stops the writer thread.
///
/// The client calls this once, on the way out; the guard cannot live in a
/// static alone, because statics are never dropped and the tail of the log
/// would be lost at exit.
pub fn shutdown() {
    let guard = WRITER_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    drop(guard);
}

/// Applies a level to the running subscriber.
///
/// A process with no installed subscriber (a test) has nothing to change, so
/// this does nothing.
pub fn set_level(level: LogLevel) {
    if let Some(control) = CONTROL.get()
        && let Err(error) = control.set_level(level)
    {
        tracing::warn!(
            error = %format!("{error:#}"),
            "could not apply the log level"
        );
    }
}

/// The subscriber filter: the app's own crates log at `level`, dependencies at
/// `warn`, and only the one target measured to repeat harmless failures is
/// pinned down.
///
/// Dependencies are not held at `error`: rustls and zbus report real TLS and
/// D-Bus problems at `warn`, and this file is the one a user is told to read
/// (R10.3).
fn targets(level: LogLevel) -> Targets {
    Targets::new()
        .with_default(LevelFilter::WARN)
        .with_target("immich_sync", level.filter())
        .with_target("immich_client", level.filter())
        .with_target("sync_core", level.filter())
        .with_target(LEVEL_CHANGE_TARGET, LevelFilter::TRACE)
        // Measured: zbus logs the same harmless D-Bus failures at warn on
        // every proxy — a missing org.freedesktop.portal.Desktop (twice) and
        // a missing org.a11y.Bus — and they would bury app diagnostics.
        .with_target("zbus::proxy", LevelFilter::ERROR)
}

/// A handle to the running subscriber: where it writes, and how to change its
/// level.
#[derive(Clone)]
pub struct LogControl {
    reload: reload::Handle<Targets, Registry>,
    directory: PathBuf,
}

impl LogControl {
    /// The directory the log files live in.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Applies `level` to the running subscriber, without a restart.
    ///
    /// # Errors
    ///
    /// Returns an error when the subscriber was dropped.
    pub fn set_level(&self, level: LogLevel) -> anyhow::Result<()> {
        tracing::info!(
            target: LEVEL_CHANGE_TARGET,
            level = level.as_str(),
            "log level changed"
        );
        self.reload
            .reload(targets(level))
            .context("apply the log level")?;
        Ok(())
    }
}

/// The running log pipeline: the subscriber, the level handle, and the flush
/// guard.
pub struct LogSink {
    subscriber: Option<Box<dyn Subscriber + Send + Sync>>,
    control: LogControl,
    guard: WorkerGuard,
}

impl LogSink {
    /// Opens the log file in `directory` and builds the subscriber.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory or log file cannot be created.
    pub fn start(directory: &Path, policy: LogPolicy, level: LogLevel) -> anyhow::Result<Self> {
        // The appender creates the directory too, but only after its first
        // prune pass has reported the missing directory on stderr.
        fs::create_dir_all(directory)
            .with_context(|| format!("create the log directory {}", directory.display()))?;
        let appender = RollingFileAppender::builder()
            .rotation(policy.rotation)
            .filename_prefix(LOG_PREFIX)
            .max_log_files(policy.max_files)
            .build(directory)
            .with_context(|| format!("create the log file in {}", directory.display()))?;
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let (filter, reload) = reload::Layer::new(targets(level));
        let subscriber = tracing_subscriber::registry().with(filter).with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false),
        );
        Ok(Self {
            subscriber: Some(Box::new(subscriber)),
            control: LogControl {
                reload,
                directory: directory.to_path_buf(),
            },
            guard,
        })
    }

    /// A handle to the subscriber this sink holds.
    #[must_use]
    pub fn control(&self) -> &LogControl {
        &self.control
    }

    /// Makes this sink the subscriber for the current thread until the returned
    /// guard drops.
    ///
    /// Tests use this instead of [`Self::install`], which a process can do only
    /// once. Hold the sink as well: it owns the flush guard.
    ///
    /// # Panics
    ///
    /// Panics when the subscriber was already taken, which only happens when a
    /// sink is installed twice.
    pub fn thread_default(&mut self) -> tracing::subscriber::DefaultGuard {
        let subscriber = self
            .subscriber
            .take()
            .expect("the log subscriber is installed once");
        tracing::subscriber::set_default(subscriber)
    }

    /// Installs the sink as the process subscriber.
    ///
    /// The writer guard moves to a static and is dropped by [`shutdown`], so
    /// the writer thread lives as long as the client and the tail of the log
    /// is flushed before the process ends.
    ///
    /// # Errors
    ///
    /// Returns an error when a subscriber is already installed.
    pub fn install(mut self) -> anyhow::Result<()> {
        let subscriber = self
            .subscriber
            .take()
            .expect("the log subscriber is installed once");
        tracing::subscriber::set_global_default(subscriber)
            .context("install the log subscriber")?;
        let _ = CONTROL.set(self.control.clone());
        *WRITER_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(self.guard);
        Ok(())
    }
}

/// The newest log file in `directory`, when one exists.
///
/// The appender names the current file with its rotation date, so the newest
/// file by modification time is the one being written.
///
/// # Errors
///
/// Returns an error when the directory cannot be read.
pub fn newest_file(directory: &Path) -> anyhow::Result<Option<PathBuf>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read the log directory {}", directory.display()));
        }
    };
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("read the log directory {}", directory.display()))?;
        if !entry.file_name().to_string_lossy().starts_with(LOG_PREFIX) {
            continue;
        }
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if newest.as_ref().is_none_or(|(at, _)| modified > *at) {
            newest = Some((modified, entry.path()));
        }
    }
    Ok(newest.map(|(_, path)| path))
}

/// Records this process's start line (SPEC R10.3): the client version, the OS,
/// and the resolved config, state, and log paths.
///
/// A path-resolution failure is recorded next to it, so a client started on the
/// fallback paths says so in the file a support session reads.
pub fn record_start(paths: &AppPaths, resolution_problem: Option<&str>) {
    tracing::info!(
        version = version::client_version(),
        os = std::env::consts::OS,
        config = %paths.config_file.display(),
        state = %paths.state_db.display(),
        logs = %paths.log_directory().display(),
        "immich-sync started"
    );
    if let Some(problem) = resolution_problem {
        tracing::error!(
            error = %problem,
            fallback = %paths.config_file.display(),
            "could not resolve the platform directories, so the fallback paths are in use"
        );
    }
}

/// A server URL for a log line: any `user:password@` userinfo is dropped
/// (SPEC N3).
///
/// The client refuses such a URL, but the sign-in attempt is logged before the
/// client is built, so the line still must not carry the userinfo.
#[must_use]
pub fn without_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some(at) = rest[..authority_end].rfind('@') else {
        return url.to_string();
    };
    format!("{scheme}://<redacted>@{}", &rest[at + 1..])
}

/// The last `max_lines` lines of the newest log file in `directory`.
///
/// Reads at most [`TAIL_BYTES`] from the end of the file, so the Logs view
/// never loads a whole log into memory.
///
/// # Errors
///
/// Returns an error when the directory or the newest file cannot be read.
pub fn tail(directory: &Path, max_lines: usize) -> anyhow::Result<Vec<String>> {
    let Some(path) = newest_file(directory)? else {
        return Ok(Vec::new());
    };
    tail_file(&path, max_lines, TAIL_BYTES)
}

/// The last `max_lines` lines of one file, reading at most `max_bytes` from its
/// end.
fn tail_file(path: &Path, max_lines: usize, max_bytes: u64) -> anyhow::Result<Vec<String>> {
    let mut file =
        fs::File::open(path).with_context(|| format!("open the log file {}", path.display()))?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(max_bytes)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read the log file {}", path.display()))?;
    // The region can start in the middle of a line, or of a multi-byte
    // character, so decode lossily and drop the partial first line.
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(lines
        .iter()
        .rev()
        .take(max_lines)
        .rev()
        .map(|line| (*line).to_owned())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    /// A file of consecutive numbered lines, one per ten bytes.
    fn numbered_log(path: &Path, lines: usize) {
        let mut text = String::new();
        for number in 0..lines {
            writeln!(text, "line {number:04}").expect("write to a string cannot fail");
        }
        fs::write(path, text).expect("write the log file");
    }

    #[test]
    fn a_logged_url_drops_embedded_credentials() {
        assert_eq!(
            without_userinfo("https://alice:hunter2@host/immich"),
            "https://<redacted>@host/immich"
        );
        assert_eq!(
            without_userinfo("https://alice@host"),
            "https://<redacted>@host"
        );
        assert_eq!(
            without_userinfo("https://host:8080/immich"),
            "https://host:8080/immich",
            "a port is not userinfo"
        );
        assert_eq!(
            without_userinfo("https://host/immich"),
            "https://host/immich"
        );
        assert_eq!(without_userinfo("not a url"), "not a url");
    }

    #[test]
    fn the_tail_reads_only_the_end_of_a_large_file() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("immich-sync.log.2026-01-01");
        numbered_log(&path, 10_000);

        // 64 bytes is about six lines, so a bounded read can never see the
        // first line of the file.
        let lines = tail_file(&path, 3, 64).expect("tail the log");

        assert_eq!(
            lines,
            ["line 9997", "line 9998", "line 9999"],
            "the tail returns the latest lines in order"
        );
        assert!(
            !lines.iter().any(|line| line.starts_with("line 0000")),
            "the tail did not read the whole file: {lines:?}"
        );
    }

    #[test]
    fn the_tail_keeps_every_line_of_a_file_under_the_cap() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("immich-sync.log.2026-01-01");
        numbered_log(&path, 3);

        let lines = tail_file(&path, 10, TAIL_BYTES).expect("tail the log");

        assert_eq!(lines, ["line 0000", "line 0001", "line 0002"]);
    }

    #[test]
    fn the_tail_drops_the_partial_line_a_bounded_read_starts_in() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("immich-sync.log.2026-01-01");
        numbered_log(&path, 100);

        let lines = tail_file(&path, 1, 25).expect("tail the log");

        assert_eq!(
            lines,
            ["line 0099"],
            "the mid-line start does not become a line of its own"
        );
    }

    #[test]
    fn the_newest_file_is_the_one_being_written() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let old = temp.path().join("immich-sync.log.2026-01-01");
        let current = temp.path().join("immich-sync.log.2026-01-02");
        fs::write(&old, "old\n").expect("write the old file");
        fs::write(&current, "current\n").expect("write the current file");
        let older = SystemTime::now() - std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&old)
            .expect("open the old file")
            .set_modified(older)
            .expect("age the old file");

        assert_eq!(
            newest_file(temp.path()).expect("find the newest file"),
            Some(current)
        );
    }

    #[test]
    fn a_directory_with_no_log_file_has_an_empty_tail() {
        let temp = tempfile::tempdir().expect("create a temp dir");

        assert_eq!(
            tail(temp.path(), TAIL_LINES).expect("tail an empty dir"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_never_rotating_policy_names_the_file_without_a_date() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let policy = LogPolicy {
            rotation: Rotation::NEVER,
            max_files: 1,
        };

        let _sink = LogSink::start(temp.path(), policy, LogLevel::Info).expect("start the log");

        assert!(
            temp.path().join(LOG_PREFIX).is_file(),
            "a session with no rotation writes {}",
            LOG_PREFIX
        );
    }
}
