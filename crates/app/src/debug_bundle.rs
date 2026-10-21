//! The debug bundle (issue #25): one archive to attach to a support message.
//!
//! The archive is a `.zip` with its entries stored uncompressed. That is the
//! friendliest format on Windows (a user can open it without another tool), and
//! keeping `zip`'s compression features off keeps the dependency footprint to
//! the crate itself: the log cap below already bounds the archive's size.
//!
//! Its entries are:
//!
//! - `README.txt`: what the archive holds, how the logs were cut, and the
//!   privacy note the view repeats.
//! - `versions.txt`: the client, the OS and arch, the Immich server, and the
//!   schema version.
//! - `logs/<file>`: the newest [`LOG_FILES`] log files, each cut to its last
//!   [`LOG_BYTES`] bytes. The #17 pipeline already redacted them.
//! - `database.txt`: `sqlite_info()` and the last error per class.
//! - `configuration.txt`: the settings values and the source folders the state
//!   database holds.
//! - `settings.toml`: the settings the client knows, in the file's own format.
//!   The values are re-serialized rather than copied, so a line pasted into the
//!   file by hand cannot smuggle anything into the archive.
//! - `sizing.txt`: the tracked bytes, the item counts by state, and the named
//!   counts.
//! - `runtime.txt`: the connection state, the engine's activity, and the upload
//!   rate, sampled when the action ran.
//!
//! **Nothing secret goes in.** The settings file never holds the password or
//! the API key (R1.2), and the log pipeline redacts them (#17). A recorded
//! failure message is left out too: its text can be the server's own answer,
//! which a broken or hostile server can fill with a submitted credential
//! (SPEC N3).
//!
//! Collection runs on the GPUI background executor (the caller does that). A
//! missing log directory, a state database that cannot be read, and a server
//! that was not reachable all produce a partial archive with a note, never an
//! error: only failing to write the archive itself is an error.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use sync_core::connection::AuthMethod;
use sync_core::store::{
    ClassError, ItemState, Sizing, SourceFolder, SqliteInfo, StateStore, StoreError,
};

use crate::diagnostics::{format_bytes, format_rate};
use crate::logging;
use crate::paths::AppPaths;
use crate::settings::Settings;
use crate::version;

/// How many log files the archive keeps, newest first.
pub const LOG_FILES: usize = 3;
/// How many bytes of each log file the archive keeps, from the end.
pub const LOG_BYTES: u64 = 256 * 1024;
/// The file-name prefix a bundle gets.
pub const BUNDLE_PREFIX: &str = "immich-sync-debug-";

/// The values only the UI thread can read, sampled when the action ran.
///
/// Everything else the archive holds is read from the file system and the state
/// database by the collector, off the UI thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The Immich server version the diagnostics probe last read, if it did.
    pub server_version: Option<String>,
    /// The connection state's summary, without any server-supplied reason.
    pub connection: String,
    /// What the engine is doing right now.
    pub activity: String,
    /// The upload rate, as the Diagnostics view shows it.
    pub throughput: String,
}

/// What one collection produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleReport {
    /// The archive, ready to attach.
    pub path: PathBuf,
    /// What the archive could not hold. The same lines are in `README.txt`, so
    /// the archive explains itself once it is detached from the client.
    pub notes: Vec<String>,
}

/// The folder a bundle is saved in: the user's Downloads folder, then the home
/// directory, then the current directory.
///
/// Downloads is visible in every file manager, which is what "easy to find"
/// needs; the path is shown in the view either way.
#[must_use]
pub fn bundle_directory() -> PathBuf {
    let Some(dirs) = directories::UserDirs::new() else {
        return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    };
    dirs.download_dir()
        .map_or_else(|| dirs.home_dir().to_path_buf(), Path::to_path_buf)
}

/// Collects one archive into `directory` and returns it with its notes.
///
/// # Errors
///
/// Returns an error only when the archive cannot be written. Everything that
/// could not be collected is a note beside a partial archive.
pub fn collect(
    paths: &AppPaths,
    store: Option<&StateStore>,
    snapshot: &Snapshot,
    directory: &Path,
) -> anyhow::Result<BundleReport> {
    let now = time::OffsetDateTime::now_utc();
    let mut notes = Vec::new();
    let entries = entries(paths, store, snapshot, now, &mut notes);
    let path = destination(directory, now)?;
    write_archive(&path, &entries)?;
    Ok(BundleReport { path, notes })
}

/// The archive's entries, in the order the archive holds them.
fn entries(
    paths: &AppPaths,
    store: Option<&StateStore>,
    snapshot: &Snapshot,
    now: time::OffsetDateTime,
    notes: &mut Vec<String>,
) -> Vec<(String, Vec<u8>)> {
    let info = match store {
        Some(store) => match store.sqlite_info() {
            Ok(info) => Some(info),
            Err(error) => {
                notes.push(format!("The state database could not be read: {error}"));
                None
            }
        },
        None => {
            notes.push(
                "The state database is not open, so the database, folder, and sizing sections \
                 are incomplete."
                    .to_string(),
            );
            None
        }
    };
    let errors = match store {
        Some(store) => store.last_error_per_class().unwrap_or_else(|error| {
            notes.push(format!(
                "The last error per class could not be read: {error}"
            ));
            Vec::new()
        }),
        None => Vec::new(),
    };
    let folders = match store {
        Some(store) => store.folders().unwrap_or_else(|error| {
            notes.push(format!("The source folders could not be read: {error}"));
            Vec::new()
        }),
        None => Vec::new(),
    };
    let sizing = store.map(|store| store.sizing());

    if snapshot.server_version.is_none() {
        notes.push(
            "The Immich server version is missing: the client was not connected to the server \
             when the bundle was collected. The Diagnostics view shows why."
                .to_string(),
        );
    }

    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    entries.push((
        "versions.txt".to_string(),
        versions_text(&info, snapshot).into_bytes(),
    ));
    entries.push((
        "database.txt".to_string(),
        database_text(paths, &info, &errors).into_bytes(),
    ));
    let settings_copied = configuration_text(paths, &folders, &mut entries, notes);
    entries.push(("sizing.txt".to_string(), sizing_text(&sizing).into_bytes()));
    entries.push((
        "runtime.txt".to_string(),
        runtime_text(now, snapshot, &errors).into_bytes(),
    ));
    log_entries(paths, &mut entries, notes);
    let readme = readme_text(now, settings_copied, notes);
    entries.push(("README.txt".to_string(), readme.into_bytes()));
    entries
}

/// The two versions sections: the client, the OS, the server, and the schema.
fn versions_text(info: &Option<SqliteInfo>, snapshot: &Snapshot) -> String {
    let mut text = String::new();
    text.push_str(&format!("Client version: {}\n", version::client_version()));
    match os_release() {
        // The kernel release is the only OS version a plain file read can
        // report without a platform-specific dependency.
        Some(release) => text.push_str(&format!(
            "Operating system: {} (kernel {release})\n",
            std::env::consts::OS
        )),
        None => text.push_str(&format!("Operating system: {}\n", std::env::consts::OS)),
    }
    text.push_str(&format!("Architecture: {}\n", std::env::consts::ARCH));
    match &snapshot.server_version {
        Some(server) => text.push_str(&format!("Immich server: {}\n", server_version_text(server))),
        None => text.push_str(
            "Immich server: not read (the client was not connected to the server when the \
             bundle was collected)\n",
        ),
    }
    match info {
        Some(info) => text.push_str(&format!(
            "Schema version (PRAGMA user_version): {}\n",
            info.migration_version
        )),
        None => text.push_str(
            "Schema version (PRAGMA user_version): not available (the state database could \
             not be read)\n",
        ),
    }
    text
}

/// The server's version, or a byte count when it is not a release number.
///
/// The value is the server's own text, the one server-controlled string the
/// archive copies: only a `v?NN.NN.NN` release number is copied, so a broken or
/// hostile server cannot ride anything else in through it (SPEC N3).
fn server_version_text(version: &str) -> String {
    let number = version.strip_prefix('v').unwrap_or(version);
    let is_number = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let mut groups = number.split('.');
    if (0..3).all(|_| groups.next().is_some_and(is_number)) && groups.next().is_none() {
        return version.to_string();
    }
    format!("unrecognized ({} bytes)", version.len())
}

/// The kernel release, where the platform exposes one as a file.
///
/// Linux publishes it at `/proc/sys/kernel/osrelease`; elsewhere the read fails
/// and the versions section reports the OS name and architecture alone.
fn os_release() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// What the state database says about itself, and the last error per class.
fn database_text(paths: &AppPaths, info: &Option<SqliteInfo>, errors: &[ClassError]) -> String {
    let mut text = String::new();
    text.push_str(&format!("Database file: {}\n", paths.state_db.display()));
    match info {
        Some(info) => {
            text.push_str(&format!(
                "Migration version (PRAGMA user_version): {}\n",
                info.migration_version
            ));
            text.push_str(&format!(
                "Size: {} ({} bytes)\n",
                format_bytes(info.size_bytes),
                info.size_bytes
            ));
            text.push_str(&format!(
                "Integrity (PRAGMA quick_check): {}\n",
                info.integrity
            ));
            text.push_str("\nTable rows:\n");
            for count in &info.tables {
                text.push_str(&format!("  {} {}\n", count.table, count.rows));
            }
        }
        None => text.push_str(
            "The state database could not be read, so its schema, size, tables, and integrity \
             check are not in this archive.\n",
        ),
    }
    push_errors(&mut text, errors, "Last error per class (newest first)");
    text
}

/// One class per line. The recorded message stays out of the archive: it can be
/// the server's own text, and a server can echo a submitted credential in it.
fn push_errors(text: &mut String, errors: &[ClassError], title: &str) {
    text.push_str(&format!("\n{title}:\n"));
    if errors.is_empty() {
        text.push_str("  (none recorded)\n");
        return;
    }
    for error in errors {
        text.push_str(&format!("  {}\n", error.class.as_str()));
    }
    text.push_str(
        "  (the recorded message is omitted: it can quote the server, and a server can echo a \
         submitted credential in it)\n",
    );
}

/// The settings values, the source folders, and the settings file itself.
///
/// Returns whether the settings file went into the archive, so the README lists
/// it exactly when it is there.
fn configuration_text(
    paths: &AppPaths,
    folders: &[SourceFolder],
    entries: &mut Vec<(String, Vec<u8>)>,
    notes: &mut Vec<String>,
) -> bool {
    let mut settings = None;
    let mut first_run = false;
    match fs::read_to_string(&paths.config_file) {
        Ok(text) => match toml::from_str::<Settings>(&text) {
            // Only the settings this build knows are copied. A line pasted
            // into the file by hand stays out of the archive, the same rule
            // that keeps a server's text out of it (SPEC N3).
            Ok(mut parsed) => {
                // A hand-edited file can carry a password in the URL. The log
                // pipeline drops that userinfo, and so does the archive (N3).
                parsed.server_url = logging::without_userinfo(&parsed.server_url);
                match toml::to_string_pretty(&parsed) {
                    Ok(written) => {
                        entries.push(("settings.toml".to_string(), written.into_bytes()));
                        settings = Some(parsed);
                    }
                    Err(error) => notes.push(format!(
                        "The settings could not be written into the archive: {error}"
                    )),
                }
            }
            Err(error) => notes.push(format!("The settings file could not be parsed: {error}")),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            notes.push(
                "There is no settings file yet, so the client uses its defaults.".to_string(),
            );
            first_run = true;
            settings = Some(Settings::default());
        }
        Err(error) => notes.push(format!("The settings file could not be read: {error}")),
    }

    let mut text = String::new();
    text.push_str(&format!("Settings file: {}\n", paths.config_file.display()));
    if entries.iter().any(|(name, _)| name == "settings.toml") {
        text.push_str("These values are also settings.toml in this archive.\n");
    }
    text.push('\n');
    if first_run {
        text.push_str("There is no settings file yet, so these are the defaults.\n\n");
    }
    match &settings {
        Some(settings) => push_settings(&mut text, settings),
        None => text.push_str(
            "The settings values are not in this archive: the settings file could not be read.\n",
        ),
    }
    text.push_str(&format!(
        "\nSource folders in the state database ({}):\n",
        folders.len()
    ));
    if folders.is_empty() {
        text.push_str("  (none, or the state database could not be read)\n");
    }
    for folder in folders {
        text.push_str(&format!("  {}\n", folder.path));
        text.push_str(&format!(
            "    recursive: {}\n",
            if folder.recursive { "yes" } else { "no" }
        ));
        text.push_str(&format!(
            "    album mapping: {}\n",
            if folder.album_mapping { "on" } else { "off" }
        ));
        text.push_str(&format!(
            "    album scope: {}\n",
            folder.album_scope.label()
        ));
    }
    entries.push(("configuration.txt".to_string(), text.into_bytes()));
    entries.iter().any(|(name, _)| name == "settings.toml")
}

/// The settings values, one per line.
fn push_settings(text: &mut String, settings: &Settings) {
    text.push_str(&format!("Server URL: {}\n", settings.server_url));
    text.push_str(&format!("Email: {}\n", settings.email));
    text.push_str(&format!(
        "Auth method: {}\n",
        match settings.auth_method {
            AuthMethod::Password => "password",
            AuthMethod::ApiKey => "API key",
        }
    ));
    text.push_str(&format!("Log level: {}\n", settings.log_level.as_str()));
    text.push_str(&format!(
        "Autostart at login: {}\n",
        if settings.autostart { "on" } else { "off" }
    ));
    if settings.bandwidth_bytes_per_second == 0 {
        text.push_str("Bandwidth limit: unlimited\n");
    } else {
        text.push_str(&format!(
            "Bandwidth limit: {} ({} bytes/s)\n",
            format_rate(settings.bandwidth_bytes_per_second as f64),
            settings.bandwidth_bytes_per_second
        ));
    }
    text.push_str(&format!("Concurrent uploads: {}\n", settings.concurrency));
    text.push_str(&format!(
        "Reconciliation scan interval: {} minutes\n",
        settings.scan_interval_minutes
    ));
    text.push_str(&format!("Retry attempt cap: {}\n", settings.attempt_cap));
}

/// The library's size and the shape of its tracked state.
fn sizing_text(sizing: &Option<Result<Sizing, StoreError>>) -> String {
    let mut text = String::new();
    let Some(sizing) = sizing else {
        text.push_str("The state database is not open, so the sizing is not in this archive.\n");
        return text;
    };
    let sizing = match sizing {
        Ok(sizing) => sizing,
        Err(error) => {
            text.push_str(&format!(
                "The state database could not be read, so the sizing is not in this archive: \
                 {error}\n"
            ));
            return text;
        }
    };
    text.push_str(&format!(
        "Tracked bytes: {} ({} bytes)\n",
        format_bytes(sizing.tracked_bytes),
        sizing.tracked_bytes
    ));
    text.push_str("Items by state:\n");
    if sizing.by_state.is_empty() {
        text.push_str("  (no tracked item)\n");
    }
    for (state, rows) in &sizing.by_state {
        text.push_str(&format!("  {} {rows}\n", state.as_str()));
    }
    text.push_str(&format!("Source folders: {}\n", sizing.folders));
    text.push_str(&format!(
        "Review items awaiting a decision: {}\n",
        count_of(&sizing.by_state, ItemState::Missing)
    ));
    text.push_str(&format!(
        "Dead-letter items: {}\n",
        count_of(&sizing.by_state, ItemState::DeadLetter)
    ));
    text.push_str(&format!("Recorded versions: {}\n", sizing.versions));
    text
}

/// One state's count in a sizing read, or zero when no row holds it.
fn count_of(by_state: &[(ItemState, u64)], state: ItemState) -> u64 {
    by_state
        .iter()
        .find(|(held, _)| *held == state)
        .map_or(0, |(_, rows)| *rows)
}

/// The connection state, the engine's activity, and the upload rate.
fn runtime_text(now: time::OffsetDateTime, snapshot: &Snapshot, errors: &[ClassError]) -> String {
    let mut text = String::new();
    text.push_str(&format!("Collected: {}\n", timestamp(now)));
    text.push_str(&format!("Connection state: {}\n", snapshot.connection));
    text.push_str(&format!("Engine activity: {}\n", snapshot.activity));
    text.push_str(&format!(
        "Upload rate: {} (sampled when the bundle was collected)\n",
        snapshot.throughput
    ));
    push_errors(&mut text, errors, "Last error per class (newest first)");
    text.push_str(
        "\nThe engine's current error text is not included: it can quote the server, and a \
         server can echo a submitted credential in it. The class list above and the log files \
         are where to look.\n",
    );
    text
}

/// The newest log files, each cut to its last [`LOG_BYTES`] bytes.
fn log_entries(paths: &AppPaths, entries: &mut Vec<(String, Vec<u8>)>, notes: &mut Vec<String>) {
    let directory = paths.log_directory();
    let files = match logging::recent_files(&directory, usize::MAX) {
        Ok(files) => files,
        Err(error) => {
            notes.push(format!("The log files could not be read: {error}"));
            return;
        }
    };
    if files.is_empty() {
        notes.push(format!(
            "No log file has been written yet in {}.",
            directory.display()
        ));
        return;
    }
    if files.len() > LOG_FILES {
        notes.push(format!(
            "Kept the newest {LOG_FILES} of {} log files.",
            files.len()
        ));
    }
    for path in files.iter().take(LOG_FILES) {
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        let size = fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let lines = match logging::tail_lines(path, usize::MAX, LOG_BYTES) {
            Ok(lines) => lines,
            Err(error) => {
                notes.push(format!("The log file {name} could not be read: {error}"));
                continue;
            }
        };
        let mut body = String::new();
        if size > LOG_BYTES {
            notes.push(format!(
                "{name} kept its last {} of {}.",
                format_bytes(LOG_BYTES),
                format_bytes(size)
            ));
            body.push_str(&format!(
                "[immich-sync: this is the last {} of a {} log file]\n",
                format_bytes(LOG_BYTES),
                format_bytes(size)
            ));
        }
        body.push_str(&lines.join("\n"));
        if !body.ends_with('\n') {
            body.push('\n');
        }
        entries.push((format!("logs/{name}"), body.into_bytes()));
    }
}

/// The archive's own explanation: what it holds, the privacy note, and what was
/// skipped.
fn readme_text(now: time::OffsetDateTime, settings_copied: bool, notes: &[String]) -> String {
    let mut text = String::new();
    text.push_str("Immich Sync debug bundle\n");
    text.push_str("========================\n\n");
    text.push_str(&format!("Collected: {}\n", timestamp(now)));
    text.push_str(&format!("Client version: {}\n", version::client_version()));
    text.push('\n');
    text.push_str("What this archive holds\n");
    text.push_str("-----------------------\n");
    text.push_str("  README.txt          this file\n");
    text.push_str("  versions.txt        client, operating system, Immich server, schema\n");
    text.push_str(&format!(
        "  logs/               the newest {LOG_FILES} log files, {} each at most\n",
        format_bytes(LOG_BYTES)
    ));
    text.push_str(
        "  database.txt        the state database's schema, size, tables, and last errors\n",
    );
    text.push_str("  configuration.txt   the settings and the source folders\n");
    if settings_copied {
        text.push_str("  settings.toml       the settings values, in the file format\n");
    }
    text.push_str("  sizing.txt          the tracked bytes and the item counts\n");
    text.push_str("  runtime.txt         the connection state and the upload rate\n");
    text.push('\n');
    text.push_str("Privacy\n");
    text.push_str("-------\n");
    text.push_str(
        "This archive holds no password, API key, or session token. The settings file never \
         stores them, and the log pipeline redacts them. A recorded failure message is left out \
         too, because its text can come from the server, which can echo a submitted credential \
         in it.\n\n",
    );
    text.push_str(
        "It does hold your folder paths, your account email, the server URL, and the log files. \
         Review it before you attach it to a message.\n",
    );
    text.push('\n');
    text.push_str("Notes\n");
    text.push_str("-----\n");
    if notes.is_empty() {
        text.push_str("  (none: every section was collected)\n");
    }
    for note in notes {
        text.push_str(&format!("  - {note}\n"));
    }
    text
}

/// The archive's path: a timestamped file in `directory`, never overwriting an
/// earlier one.
fn destination(directory: &Path, now: time::OffsetDateTime) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(directory)
        .with_context(|| format!("create the bundle directory {}", directory.display()))?;
    let stamp = format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    let mut path = directory.join(format!("{BUNDLE_PREFIX}{stamp}.zip"));
    let mut counter = 1;
    while path.exists() {
        counter += 1;
        path = directory.join(format!("{BUNDLE_PREFIX}{stamp}-{counter}.zip"));
    }
    Ok(path)
}

/// Writes the entries beside `path`, then renames the finished archive into
/// place, so a failed collection never leaves a truncated `.zip` behind.
fn write_archive(path: &Path, entries: &[(String, Vec<u8>)]) -> anyhow::Result<()> {
    let temporary = path.with_extension("zip.tmp");
    let result = (|| -> anyhow::Result<()> {
        let file = fs::File::create(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        let mut archive = zip::ZipWriter::new(std::io::BufWriter::new(file));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            archive
                .start_file(name.as_str(), options)
                .with_context(|| format!("add {name} to the debug bundle"))?;
            archive
                .write_all(bytes)
                .with_context(|| format!("write {name} into the debug bundle"))?;
        }
        let mut buffer = archive.finish().context("finish the debug bundle")?;
        buffer.flush().context("flush the debug bundle")?;
        fs::rename(&temporary, path)
            .with_context(|| format!("move the debug bundle into {}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// A UTC timestamp, in the form the bundle's text and file name use.
fn timestamp(now: time::OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

#[cfg(test)]
mod tests {
    use super::{
        BUNDLE_PREFIX, BundleReport, LOG_BYTES, LOG_FILES, Snapshot, collect, count_of,
        database_text, sizing_text, versions_text,
    };
    use crate::logging::LOG_PREFIX;
    use crate::paths::AppPaths;
    use std::fs;
    use std::io::{Cursor, Read as _};
    use std::path::Path;
    use std::time::{Duration, SystemTime};
    use sync_core::failure::FailureClass;
    use sync_core::hash::sha1;
    use sync_core::store::{
        AlbumScope, AttemptOutcome, AttemptRecord, ItemRecord, ItemState, NewVersion, SourceFolder,
        StateStore, StoreError,
    };

    /// A credential-like string nothing in the archive may hold.
    const SENTINEL: &str = "s3cr3t-api-key-9f3c1b2a";

    const SEEDED_SETTINGS: &str = "\
autostart = true
bandwidth_bytes_per_second = 1048576
concurrency = 2
scan_interval_minutes = 15
attempt_cap = 3
server_url = \"https://immich.example.com\"
email = \"sync@example.com\"
auth_method = \"api_key\"
log_level = \"debug\"
";

    fn temporary_paths(dir: &Path) -> AppPaths {
        AppPaths {
            config_file: dir.join("config").join("settings.toml"),
            state_db: dir.join("data").join("state.db"),
            autostart_dir: dir.join("config").join("autostart"),
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            server_version: Some("v3.3.1".to_string()),
            connection: "Connected".to_string(),
            activity: "Idle".to_string(),
            throughput: "0 B/s".to_string(),
        }
    }

    fn write_log(directory: &Path, name: &str, body: &str, age: u64) {
        fs::create_dir_all(directory).expect("create the log directory");
        let path = directory.join(name);
        fs::write(&path, body).expect("write the log file");
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("open the log file")
            .set_modified(SystemTime::now() - Duration::from_secs(age))
            .expect("age the log file");
    }

    /// Every entry's name and text, read back through the archive reader, plus
    /// the raw archive bytes.
    fn read_bundle(report: &BundleReport) -> (Vec<(String, String)>, Vec<u8>) {
        let bytes = fs::read(&report.path).expect("read the archive");
        let mut archive =
            zip::ZipArchive::new(Cursor::new(bytes.clone())).expect("open the archive");
        let names: Vec<String> = archive
            .file_names()
            .map(|name| name.expect("a readable entry name").into_owned())
            .collect();
        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            let mut body = String::new();
            archive
                .by_name(&name)
                .expect("open the entry")
                .read_to_string(&mut body)
                .expect("read the entry");
            entries.push((name, body));
        }
        (entries, bytes)
    }

    fn section<'a>(entries: &'a [(String, String)], name: &str) -> &'a str {
        entries.iter().find(|(entry, _)| entry == name).map_or_else(
            || {
                panic!(
                    "the archive holds {name}: {:?}",
                    entries.iter().map(|(n, _)| n).collect::<Vec<_>>()
                )
            },
            |(_, body)| body.as_str(),
        )
    }

    fn item(path: &str, size: u64, state: ItemState) -> ItemRecord {
        ItemRecord {
            path: path.to_string(),
            size,
            mtime_ns: 1_700_000_000_000_000_000,
            sha1: sha1(b"fixture"),
            asset_id: Some("asset-1".to_string()),
            state,
            error_class: None,
            error_message: None,
            attempts: 0,
            sidecar: None,
        }
    }

    /// Issue #25, acceptance criteria 2 and 3: one archive holds every required
    /// section, and a credential-like sentinel the client holds nowhere in it.
    #[test]
    fn the_bundle_holds_every_section_and_no_secret() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = temporary_paths(temp.path());
        fs::create_dir_all(paths.config_file.parent().expect("the config directory"))
            .expect("create the config directory");
        // The last line is a key this build does not know, holding the
        // sentinel: the client ignores it, and the archive must not carry it.
        fs::write(
            &paths.config_file,
            format!("{SEEDED_SETTINGS}password = \"{SENTINEL}\"\n"),
        )
        .expect("write the settings");
        write_log(
            &paths.log_directory(),
            &format!("{LOG_PREFIX}.2026-10-16"),
            "2026-10-16T10:00:00Z  INFO immich_sync: immich-sync started\n",
            0,
        );

        let store = StateStore::open(&paths.state_db).expect("open the state database");
        store
            .add_folder(&SourceFolder {
                path: "/photos".to_string(),
                recursive: true,
                album_mapping: true,
                album_scope: AlbumScope::WholeFolder,
            })
            .expect("add the source folder");
        let uploaded = item("/photos/IMG_0001.jpg", 2048, ItemState::Uploaded);
        store.record(&uploaded).expect("write the uploaded item");
        // The recorded failure message is the sentinel: the archive must not
        // carry the stored server text even when it looks like a credential.
        let mut failed = item("/photos/RAW_0002.cr3", 4096, ItemState::FailedPermanent);
        failed.error_class = Some(FailureClass::UnsupportedFormat);
        failed.error_message = Some(format!("the server answered: {SENTINEL}"));
        store.record(&failed).expect("write the failed item");
        store
            .record_attempt(&AttemptRecord {
                path: failed.path.clone(),
                started_ns: 10,
                outcome: AttemptOutcome::Failed,
                error_class: Some(FailureClass::UnsupportedFormat),
            })
            .expect("record the attempt");
        store
            .record_version(&NewVersion {
                path: uploaded.path.clone(),
                asset_id: "asset-1".to_string(),
                sha1: sha1(b"fixture"),
                created_at_ns: 1,
                stack_id: None,
            })
            .expect("record the version");

        let directory = temp.path().join("downloads");
        let report = collect(&paths, Some(&store), &snapshot(), &directory)
            .expect("collect the debug bundle");

        assert_eq!(
            report.path.parent(),
            Some(directory.as_path()),
            "the archive lands in the folder the view names"
        );
        assert!(
            report
                .path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(BUNDLE_PREFIX)
                    && name.to_string_lossy().ends_with(".zip")),
            "the archive is a timestamped .zip: {}",
            report.path.display()
        );
        assert!(
            report.notes.is_empty(),
            "a complete collection has nothing to note: {:?}",
            report.notes
        );

        let (entries, bytes) = read_bundle(&report);
        for required in [
            "README.txt",
            "versions.txt",
            "database.txt",
            "configuration.txt",
            "settings.toml",
            "sizing.txt",
            "runtime.txt",
            &format!("logs/{LOG_PREFIX}.2026-10-16"),
        ] {
            assert!(
                entries.iter().any(|(name, _)| name == required),
                "the archive holds {required}: {:?}",
                entries.iter().map(|(name, _)| name).collect::<Vec<_>>()
            );
        }

        // No entry, and the archive's own bytes, hold the sentinel.
        for (name, body) in &entries {
            assert!(
                !body.contains(SENTINEL),
                "{name} carries the sentinel: {body}"
            );
        }
        assert!(
            !bytes
                .windows(SENTINEL.len())
                .any(|window| window == SENTINEL.as_bytes()),
            "the raw archive bytes carry the sentinel"
        );

        let versions = section(&entries, "versions.txt");
        for expected in [
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            "v3.3.1",
            "v3.3.1",
            // The latest migration the state database stands at (V11: the
            // folder times #43 added on top of the tag backfill #46).
            "Schema version (PRAGMA user_version): 11",
        ] {
            assert!(
                versions.contains(expected),
                "{expected} is in versions.txt: {versions}"
            );
        }

        let database = section(&entries, "database.txt");
        for expected in [
            "Migration version",
            "quick_check",
            "ok",
            "sync_item",
            "unsupported_format",
        ] {
            assert!(
                database.contains(expected),
                "{expected} is in database.txt: {database}"
            );
        }

        let configuration = section(&entries, "configuration.txt");
        for expected in [
            "https://immich.example.com",
            "sync@example.com",
            "API key",
            "debug",
            "1.0 MiB/s",
            "Concurrent uploads: 2",
            "/photos",
            "album mapping: on",
            "One album for this folder",
        ] {
            assert!(
                configuration.contains(expected),
                "{expected} is in configuration.txt: {configuration}"
            );
        }
        let copied = section(&entries, "settings.toml");
        assert!(
            copied.contains("email = \"sync@example.com\"")
                && copied.contains("auth_method = \"api_key\"")
                && copied.contains("log_level = \"debug\""),
            "the settings values are in the archive: {copied}"
        );
        assert!(
            !copied.contains("password"),
            "a line pasted into the settings file stays out: {copied}"
        );

        let sizing = section(&entries, "sizing.txt");
        for expected in [
            "Tracked bytes:",
            "6144",
            "uploaded 1",
            "failed_permanent 1",
            "Source folders: 1",
            "Review items awaiting a decision: 0",
            "Dead-letter items: 0",
            "Recorded versions: 1",
        ] {
            assert!(
                sizing.contains(expected),
                "{expected} is in sizing.txt: {sizing}"
            );
        }

        let runtime = section(&entries, "runtime.txt");
        for expected in [
            "Connection state: Connected",
            "Engine activity: Idle",
            "Upload rate: 0 B/s",
            "unsupported_format",
        ] {
            assert!(
                runtime.contains(expected),
                "{expected} is in runtime.txt: {runtime}"
            );
        }

        let readme = section(&entries, "README.txt");
        assert!(readme.contains("Privacy"), "the archive explains itself");
        assert!(
            readme.contains("folder paths") && readme.contains("password"),
            "the README states what the archive does and does not hold: {readme}"
        );
        assert!(
            readme.contains("(none: every section was collected)"),
            "with nothing skipped the README says so: {readme}"
        );
    }

    /// Issue #25, acceptance criterion 3: a settings file with a password in
    /// the URL is redacted the same way the log pipeline redacts one.
    #[test]
    fn a_settings_url_with_userinfo_is_redacted() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = temporary_paths(temp.path());
        fs::create_dir_all(paths.config_file.parent().expect("the config directory"))
            .expect("create the config directory");
        fs::write(
            &paths.config_file,
            format!(
                "server_url = \"https://sync:{SENTINEL}@immich.example.com\"\n\
                 email = \"sync@example.com\"\n"
            ),
        )
        .expect("write the settings");

        let report = collect(&paths, None, &snapshot(), &temp.path().join("downloads"))
            .expect("collect the debug bundle");

        let (entries, _) = read_bundle(&report);
        for name in ["configuration.txt", "settings.toml"] {
            let text = section(&entries, name);
            assert!(
                !text.contains(SENTINEL),
                "{name} carries the URL's password: {text}"
            );
            assert!(
                text.contains("https://<redacted>@immich.example.com"),
                "{name} keeps the rest of the URL: {text}"
            );
        }
    }

    /// Issue #25, acceptance criterion 4: missing logs, an unreadable database,
    /// and an unreachable server make a partial archive with a note, not an
    /// error.
    #[test]
    fn a_partial_bundle_notes_what_it_could_not_collect() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = temporary_paths(temp.path());
        let snapshot = Snapshot {
            server_version: None,
            connection: "Needs attention".to_string(),
            activity: "Idle".to_string(),
            throughput: "0 B/s".to_string(),
        };

        let report = collect(&paths, None, &snapshot, &temp.path().join("downloads"))
            .expect("a partial bundle is still written");

        let notes = report.notes.join("\n");
        for expected in [
            "No log file",
            "state database",
            "server version",
            "settings file",
        ] {
            assert!(
                notes.contains(expected),
                "the note names {expected}: {notes}"
            );
        }

        let (entries, _) = read_bundle(&report);
        assert!(
            !entries.iter().any(|(name, _)| name.starts_with("logs/")),
            "a client that never logged has nothing to add"
        );
        assert!(
            !entries.iter().any(|(name, _)| name == "settings.toml"),
            "a missing settings file is not invented"
        );
        assert!(
            section(&entries, "versions.txt").contains("not read"),
            "the server version says why it is missing"
        );
        assert!(
            section(&entries, "database.txt").contains("could not be read"),
            "the database section says it is incomplete"
        );
        assert!(
            section(&entries, "sizing.txt").contains("not in this archive"),
            "the sizing section says it is incomplete"
        );
        assert!(
            section(&entries, "configuration.txt").contains("these are the defaults"),
            "a first run reports the defaults instead of an error"
        );
        assert!(
            section(&entries, "README.txt").contains("No log file"),
            "the archive carries the same notes"
        );
    }

    /// Issue #25: the log cap is the newest files, each cut to its end, and the
    /// cut is said both inside the entry and in the notes.
    #[test]
    fn the_bundle_keeps_the_newest_logs_and_notes_the_cut() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = temporary_paths(temp.path());
        let directory = paths.log_directory();
        for (index, age) in [(1, 40), (2, 30), (3, 20), (4, 10), (5, 0)] {
            write_log(
                &directory,
                &format!("{LOG_PREFIX}.0{index}"),
                &format!("line from file {index}\n"),
                age,
            );
        }
        // The newest file is larger than the cap, so only its end is kept.
        let oversized = format!("{LOG_PREFIX}.05");
        let mut body = "first line that must be cut\n".to_string();
        while (body.len() as u64) < LOG_BYTES + 4096 {
            body.push_str("filler line\n");
        }
        body.push_str("last line that must survive\n");
        fs::write(directory.join(&oversized), &body).expect("write the oversized log");

        let report = collect(&paths, None, &snapshot(), &temp.path().join("downloads"))
            .expect("collect the debug bundle");

        let (entries, _) = read_bundle(&report);
        let logs: Vec<String> = entries
            .iter()
            .map(|(name, _)| name.clone())
            .filter(|name| name.starts_with("logs/"))
            .collect();
        assert_eq!(
            logs.len(),
            LOG_FILES,
            "only the newest log files are kept: {logs:?}"
        );
        assert_eq!(
            logs,
            vec![
                format!("logs/{oversized}"),
                format!("logs/{LOG_PREFIX}.04"),
                format!("logs/{LOG_PREFIX}.03")
            ],
            "the newest files come first"
        );
        let notes = report.notes.join("\n");
        assert!(
            notes.contains("newest 3 of 5 log files"),
            "the rotation is noted: {notes}"
        );
        assert!(
            notes.contains("kept its last"),
            "the byte cap on the newest file is noted: {notes}"
        );
        let newest = section(&entries, &format!("logs/{oversized}"));
        assert!(
            newest.starts_with("[immich-sync: this is the last"),
            "the entry says it was cut: {}",
            &newest[..newest.len().min(120)]
        );
        assert!(
            newest.contains("last line that must survive"),
            "the entry holds the end of the file"
        );
        assert!(
            !newest.contains("first line that must be cut"),
            "the entry drops what the cap cut away"
        );
    }

    /// Issue #25, acceptance criterion 3: the server version is the one
    /// server-controlled string the archive copies, so only a release number
    /// is copied and anything else is counted, never echoed.
    #[test]
    fn a_server_version_that_is_not_a_release_number_is_counted_not_copied() {
        let mut hostile = snapshot();
        hostile.server_version = Some(format!("1.2.3 {SENTINEL}"));
        let versions = versions_text(&None, &hostile);
        assert!(
            versions.contains("Immich server: unrecognized (") && !versions.contains(SENTINEL),
            "a server's text is counted, not copied: {versions}"
        );

        let mut release = snapshot();
        release.server_version = Some("v1.119.0".to_string());
        assert!(
            versions_text(&None, &release).contains("Immich server: v1.119.0"),
            "a release number is kept"
        );
    }

    /// Issue #25, acceptance criterion 4: a store that is open but cannot be
    /// read makes each section say so, rather than failing the collection.
    #[test]
    fn an_unreadable_database_is_a_note_in_each_section() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = temporary_paths(temp.path());
        let unreadable = StoreError::QuickCheck {
            path: paths.state_db.clone(),
            reason: "the database disk image is malformed".to_string(),
        };

        let database = database_text(&paths, &None, &[]);
        assert!(
            database.contains("could not be read"),
            "the database section says why it is empty: {database}"
        );
        let sizing = sizing_text(&Some(Err(unreadable)));
        assert!(
            sizing.contains("could not be read") && sizing.contains("malformed"),
            "the sizing section says why it is empty: {sizing}"
        );
    }

    /// A destination that cannot be created is the one real failure: the view
    /// reports it instead of pretending a bundle exists.
    #[test]
    fn an_archive_that_cannot_be_written_is_an_error() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let paths = temporary_paths(temp.path());
        let blocker = temp.path().join("downloads");
        fs::write(&blocker, b"not a directory").expect("write the blocker");

        let error = collect(&paths, None, &snapshot(), &blocker)
            .expect_err("a file where the folder goes is not a destination");

        assert!(
            format!("{error:#}").contains("bundle directory"),
            "the failure names what could not be created: {error:#}"
        );
    }

    #[test]
    fn a_state_the_sizing_does_not_hold_counts_as_zero() {
        assert_eq!(count_of(&[], ItemState::Missing), 0);
        assert_eq!(count_of(&[(ItemState::Missing, 3)], ItemState::Missing), 3);
    }
}
