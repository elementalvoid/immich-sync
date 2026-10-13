//! The SQLite state store (ADR 0005, SPEC N4).
//!
//! One writer connection, WAL, `synchronous=FULL`, a busy timeout, and
//! `rusqlite_migration` for the schema. Every write runs inside a
//! `BEGIN IMMEDIATE` transaction, so two workers cannot interleave a
//! read-modify-write. The database lives in the local data directory; the
//! caller passes the path from the platform-directory abstraction (R9.2).
//!
//! Reader connections for the UI arrive with the views that need them; until
//! then the writer serves the engine's own reads.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use rusqlite_migration::{M, Migrations};

use crate::failure::FailureClass;
use crate::hash::Sha1;

/// The first schema (SPEC §5 and issue #5): one row per sync item.
///
/// `sync_item` keeps the file identity the pipeline decides on: where the file
/// is, the size and mtime it had when it was hashed, its SHA-1, the asset the
/// server returned, and where the item stands. The version and attempt tables
/// arrive with their tickets; `id` is the stable item identity they will
/// reference.
const SCHEMA_V1: &str = "
CREATE TABLE sync_item (
    id          INTEGER PRIMARY KEY,
    path        TEXT    NOT NULL UNIQUE,
    size        INTEGER NOT NULL,
    mtime_ns    INTEGER NOT NULL,
    sha1        BLOB    NOT NULL,
    asset_id    TEXT,
    state       TEXT    NOT NULL,
    error_class TEXT
);
";

/// How long the one writer waits for a competing lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The state one item is in (SPEC §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemState {
    /// The server created the asset (R3.2).
    Uploaded,
    /// The server already had the content (R3.2, R4.5).
    Duplicate,
    /// A permanent failure; no automatic retry (R6.3).
    FailedPermanent,
    /// A transient failure that a later attempt may resolve (R6.1).
    RetryWait,
}

impl ItemState {
    /// The exact string the state database stores.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uploaded => "uploaded",
            Self::Duplicate => "duplicate",
            Self::FailedPermanent => "failed_permanent",
            Self::RetryWait => "retry_wait",
        }
    }

    /// Parses a stored state. Returns `None` for a value this build does not
    /// know, so the caller can report the row instead of guessing.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "uploaded" => Self::Uploaded,
            "duplicate" => Self::Duplicate,
            "failed_permanent" => Self::FailedPermanent,
            "retry_wait" => Self::RetryWait,
            _ => return None,
        })
    }
}

/// One `sync_item` row, in Rust form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemRecord {
    /// The local path, exactly as stored.
    pub path: String,
    /// The size in bytes at the time of the last outcome.
    pub size: u64,
    /// The modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// The SHA-1 of the content.
    pub sha1: Sha1,
    /// The Immich asset id, once the server returned one (R3.6).
    pub asset_id: Option<String>,
    /// The item's state.
    pub state: ItemState,
    /// The failure class of the last outcome, when it failed (R6.4).
    pub error_class: Option<FailureClass>,
}

/// The pragmas the writer connection runs with, for tests and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorePragmas {
    /// `journal_mode`; ADR 0005 requires `wal`.
    pub journal_mode: String,
    /// `synchronous`; `2` is `FULL`.
    pub synchronous: i64,
    /// `busy_timeout` in milliseconds.
    pub busy_timeout_ms: i64,
}

/// Why the state store could not be used.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database's directory could not be created.
    #[error("the state database directory for {path} could not be created: {source}")]
    Directory {
        /// The database path.
        path: PathBuf,
        /// The operating system's description of the failure.
        #[source]
        source: std::io::Error,
    },
    /// The database file could not be opened.
    #[error("the state database {path} could not be opened: {source}")]
    Open {
        /// The database path.
        path: PathBuf,
        /// SQLite's description of the failure.
        #[source]
        source: rusqlite::Error,
    },
    /// The startup integrity check refused the database (N4).
    #[error(
        "the state database {path} failed the startup integrity check (PRAGMA quick_check): \
         {reason}"
    )]
    QuickCheck {
        /// The database path.
        path: PathBuf,
        /// What `quick_check` reported.
        reason: String,
    },
    /// A migration failed.
    #[error("the state database {path} could not be migrated: {source}")]
    Migration {
        /// The database path.
        path: PathBuf,
        /// The migration failure.
        #[source]
        source: rusqlite_migration::Error,
    },
    /// A query or write failed.
    #[error("the state database {path} could not be read or written: {source}")]
    Query {
        /// The database path.
        path: PathBuf,
        /// SQLite's description of the failure.
        #[source]
        source: rusqlite::Error,
    },
    /// A row holds a state this build does not know.
    #[error("the state database holds an unknown item state {0:?}")]
    UnknownState(String),
    /// A row holds a SHA-1 that is not 20 bytes.
    #[error("the state database holds a SHA-1 that is not 20 bytes")]
    BadChecksum,
    /// A file size does not fit SQLite's signed 64-bit integer.
    #[error("the file size {0} does not fit the state database")]
    SizeOutOfRange(u64),
    /// A row holds a negative size.
    #[error("the state database holds a negative file size")]
    NegativeSize,
    /// A thread panicked while holding the writer lock.
    #[error("the state database writer is unusable after a panic")]
    Poisoned,
}

/// The state store: one writer connection behind a lock.
#[derive(Debug)]
pub struct StateStore {
    path: PathBuf,
    writer: Mutex<Connection>,
}

impl StateStore {
    /// Opens the database, runs the startup check, applies the pragmas, and
    /// migrates the schema to the latest version (N4).
    ///
    /// The integrity check runs before anything else writes, so a damaged file
    /// is reported as damaged rather than as a failed migration.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the directory cannot be created, the file
    /// cannot be opened, the startup check fails, or the migration fails.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        if let Some(directory) = path.parent()
            && !directory.as_os_str().is_empty()
        {
            std::fs::create_dir_all(directory).map_err(|source| StoreError::Directory {
                path: path.clone(),
                source,
            })?;
        }
        let mut writer = Connection::open(&path).map_err(|source| StoreError::Open {
            path: path.clone(),
            source,
        })?;
        // Before the pragmas: a file that is not a database fails every pragma
        // too, and `quick_check` is the honest reason to report.
        let check: String = writer
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|source| StoreError::QuickCheck {
                path: path.clone(),
                reason: source.to_string(),
            })?;
        if check != "ok" {
            return Err(StoreError::QuickCheck {
                path,
                reason: check,
            });
        }
        set_writer_pragmas(&writer).map_err(|source| StoreError::Open {
            path: path.clone(),
            source,
        })?;
        migrations()
            .to_latest(&mut writer)
            .map_err(|source| StoreError::Migration {
                path: path.clone(),
                source,
            })?;
        Ok(Self {
            path,
            writer: Mutex::new(writer),
        })
    }

    /// The database path.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Runs SQLite's `quick_check` again (N4), for a caller that wants to
    /// confirm the file is still sound.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::QuickCheck`] when SQLite reports anything but
    /// `ok`, and [`StoreError::Query`] when the check itself cannot run.
    pub fn quick_check(&self) -> Result<(), StoreError> {
        let writer = self.writer()?;
        let result: String = writer
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|source| self.query_error(source))?;
        if result == "ok" {
            Ok(())
        } else {
            Err(StoreError::QuickCheck {
                path: self.path.clone(),
                reason: result,
            })
        }
    }

    /// The pragmas the writer runs with.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Query`] when a pragma cannot be read.
    pub fn pragmas(&self) -> Result<StorePragmas, StoreError> {
        let writer = self.writer()?;
        let read = |name: &str| -> Result<i64, StoreError> {
            writer
                .query_row(name, [], |row| row.get(0))
                .map_err(|source| self.query_error(source))
        };
        Ok(StorePragmas {
            journal_mode: writer
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                .map_err(|source| self.query_error(source))?,
            // SQLite reports `synchronous` as an integer; 2 is FULL.
            synchronous: read("PRAGMA synchronous")?,
            busy_timeout_ms: read("PRAGMA busy_timeout")?,
        })
    }

    /// Writes one item's outcome, replacing the row for the same path.
    ///
    /// The write runs inside `BEGIN IMMEDIATE` on the one writer connection
    /// (ADR 0005), so a concurrent worker waits rather than interleaving.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the transaction cannot start, the statement
    /// fails, or the lock is poisoned.
    pub fn record(&self, item: &ItemRecord) -> Result<(), StoreError> {
        let size = i64::try_from(item.size).map_err(|_| StoreError::SizeOutOfRange(item.size))?;
        let mut writer = self.writer()?;
        let transaction = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| self.query_error(source))?;
        transaction
            .execute(
                "INSERT INTO sync_item (path, size, mtime_ns, sha1, asset_id, state, error_class)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(path) DO UPDATE SET
                     size = excluded.size,
                     mtime_ns = excluded.mtime_ns,
                     sha1 = excluded.sha1,
                     asset_id = excluded.asset_id,
                     state = excluded.state,
                     error_class = excluded.error_class",
                params![
                    item.path,
                    size,
                    item.mtime_ns,
                    item.sha1.as_bytes().as_slice(),
                    item.asset_id,
                    item.state.as_str(),
                    item.error_class.map(FailureClass::as_str),
                ],
            )
            .map_err(|source| self.query_error(source))?;
        transaction
            .commit()
            .map_err(|source| self.query_error(source))
    }

    /// Reads one item by its exact path, or `None` when it is not tracked.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails or the row cannot be read.
    pub fn item(&self, path: &str) -> Result<Option<ItemRecord>, StoreError> {
        let writer = self.writer()?;
        writer
            .query_row(
                "SELECT path, size, mtime_ns, sha1, asset_id, state, error_class
                 FROM sync_item WHERE path = ?1",
                [path],
                read_item,
            )
            .optional()
            .map_err(|source| self.query_error(source))
    }

    /// The writer connection, or the poison error after a panic.
    fn writer(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.writer.lock().map_err(|_| StoreError::Poisoned)
    }

    /// One query failure, tagged with the database path.
    fn query_error(&self, source: rusqlite::Error) -> StoreError {
        StoreError::Query {
            path: self.path.clone(),
            source,
        }
    }
}

/// Reads one `sync_item` row.
fn read_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRecord> {
    let size: i64 = row.get(1)?;
    let size = u64::try_from(size).map_err(|_| {
        conversion_error(1, rusqlite::types::Type::Integer, StoreError::NegativeSize)
    })?;
    let bytes: Vec<u8> = row.get(3)?;
    let sha1 = Sha1::from_slice(&bytes)
        .ok_or_else(|| conversion_error(3, rusqlite::types::Type::Blob, StoreError::BadChecksum))?;
    let state_text: String = row.get(5)?;
    let state = ItemState::parse(&state_text).ok_or_else(|| {
        conversion_error(
            5,
            rusqlite::types::Type::Text,
            StoreError::UnknownState(state_text.clone()),
        )
    })?;
    let class_text: Option<String> = row.get(6)?;
    // A class a newer build wrote reads as `None`, which is the same as "no
    // known class"; the row is not rejected for it.
    let error_class = class_text.as_deref().and_then(FailureClass::parse);
    Ok(ItemRecord {
        path: row.get(0)?,
        size,
        mtime_ns: row.get(2)?,
        sha1,
        asset_id: row.get(4)?,
        state,
        error_class,
    })
}

/// One read that does not fit the Rust type, as a SQLite conversion failure.
fn conversion_error(
    index: usize,
    kind: rusqlite::types::Type,
    error: StoreError,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, kind, Box::new(error))
}

/// Applies the store pragmas of ADR 0005 to the writer connection.
fn set_writer_pragmas(writer: &Connection) -> rusqlite::Result<()> {
    // `journal_mode` answers with a row, so it is queried, not executed.
    let mode: String = writer.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    debug_assert_eq!(mode, "wal", "WAL is what ADR 0005 requires");
    writer.pragma_update(None, "synchronous", "FULL")?;
    writer.busy_timeout(BUSY_TIMEOUT)?;
    Ok(())
}

/// The schema migrations, in order (ADR 0005: `rusqlite_migration`).
fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(SCHEMA_V1)])
}
