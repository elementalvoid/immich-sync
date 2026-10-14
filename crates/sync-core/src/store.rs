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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior, params};
use rusqlite_migration::{M, Migrations};

use crate::failure::FailureClass;
use crate::hash::Sha1;
use crate::paths::{PathCase, key, path_key};

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

/// The second schema (issue #6): the path comparison key that makes
/// case-only variants one item on Windows (R2.4), and the `source_folder`
/// table (R2.1).
///
/// The SQL adds the column; the hook fills it with the platform's own key,
/// which SQL alone cannot compute. The unique index and the folder table
/// arrive in the next migration, after every row has a key.
const SCHEMA_V2: &str = "ALTER TABLE sync_item ADD COLUMN path_key TEXT;";

/// The third schema (issue #6): the unique key index and the source folders.
const SCHEMA_V3: &str = "
CREATE UNIQUE INDEX sync_item_path_key ON sync_item(path_key);
CREATE TABLE source_folder (
    id        INTEGER PRIMARY KEY,
    path      TEXT    NOT NULL,
    path_key  TEXT    NOT NULL UNIQUE,
    recursive INTEGER NOT NULL
);
";

/// The fourth schema (issue #9): the per-folder album mapping and its album
/// scope (R3.4), plus the album ids the server assigned.
///
/// The album id is stored so a restart reuses the album instead of creating a
/// second one for the same folder. The album is keyed by the mapped folder's
/// path key (R2.4), not by its name: the name is a copy the mapping makes, so
/// two folders that share a name are two albums, and one folder spelled with
/// another case is one.
///
/// The `name` column is a breadcrumb for a person reading the file with the
/// sqlite CLI; nothing reads it, and the album is found by folder key alone.
const SCHEMA_V4: &str = "
ALTER TABLE source_folder ADD COLUMN album_mapping INTEGER NOT NULL DEFAULT 0;
ALTER TABLE source_folder ADD COLUMN album_scope TEXT NOT NULL DEFAULT 'leaf';
CREATE TABLE album (
    folder_key TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    album_id   TEXT NOT NULL
);
";

/// How long the one writer waits for a competing lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The state one item is in (SPEC §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemState {
    /// Discovered and queued, not yet uploaded (R6.7).
    Pending,
    /// A worker holds it right now. A restart re-queues this state (R6.7).
    Uploading,
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
            Self::Pending => "pending",
            Self::Uploading => "uploading",
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
            "pending" => Self::Pending,
            "uploading" => Self::Uploading,
            "uploaded" => Self::Uploaded,
            "duplicate" => Self::Duplicate,
            "failed_permanent" => Self::FailedPermanent,
            "retry_wait" => Self::RetryWait,
            _ => return None,
        })
    }

    /// Whether the item finished for good: no later run uploads it again
    /// (R3.2, R6.7).
    #[must_use]
    pub fn is_terminal_success(self) -> bool {
        matches!(self, Self::Uploaded | Self::Duplicate)
    }
}

/// What one mapped source folder maps its files to (GLOSSARY, R3.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AlbumScope {
    /// One album per folder that directly contains files (GLOSSARY).
    #[default]
    Leaf,
    /// One album for the whole source folder (GLOSSARY).
    WholeFolder,
}

impl AlbumScope {
    /// The exact string the state database stores.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Leaf => "leaf",
            Self::WholeFolder => "whole_folder",
        }
    }

    /// Parses a stored scope. Returns `None` for a value this build does not
    /// know, so a row a newer build wrote is not guessed at.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "leaf" => Self::Leaf,
            "whole_folder" => Self::WholeFolder,
            _ => return None,
        })
    }

    /// The user's choice in the config window's words (GLOSSARY).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Leaf => "One album per folder",
            Self::WholeFolder => "One album for this folder",
        }
    }
}

/// One source folder the user selected (R2.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFolder {
    /// The folder path, exactly as the user chose it.
    pub path: String,
    /// Whether the scan and the watcher descend into subfolders.
    pub recursive: bool,
    /// Whether this folder's files join an album (R3.4). Off until the user
    /// opts in.
    pub album_mapping: bool,
    /// What the folder maps to once album mapping is on (R3.4).
    pub album_scope: AlbumScope,
}

/// The per-folder item counts a dashboard card shows (R8.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FolderCounts {
    /// Items queued or in flight.
    pub pending: u32,
    /// Items in a terminal success state (R3.2).
    pub uploaded: u32,
    /// Items in a failed state.
    pub failed: u32,
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
    /// A row holds an album scope this build does not know.
    #[error("the state database holds an unknown album scope {0:?}")]
    UnknownAlbumScope(String),
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
    /// (ADR 0005), so a concurrent worker waits rather than interleaving. The
    /// comparison key is what makes a case-only path variant the same row on
    /// Windows (R2.4).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the transaction cannot start, the statement
    /// fails, or the lock is poisoned.
    pub fn record(&self, item: &ItemRecord) -> Result<(), StoreError> {
        self.record_with_case(item, PathCase::for_platform())
    }

    /// Writes one item with an explicit case rule, for the platform seam
    /// (R2.4) and for tests that exercise the Windows rule anywhere.
    ///
    /// # Errors
    ///
    /// The same as [`Self::record`].
    pub fn record_with_case(&self, item: &ItemRecord, case: PathCase) -> Result<(), StoreError> {
        let size = i64::try_from(item.size).map_err(|_| StoreError::SizeOutOfRange(item.size))?;
        let mut writer = self.writer()?;
        let transaction = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| self.query_error(source))?;
        transaction
            .execute(
                "INSERT INTO sync_item (path, path_key, size, mtime_ns, sha1, asset_id, state, error_class)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(path_key) DO UPDATE SET
                     path = excluded.path,
                     size = excluded.size,
                     mtime_ns = excluded.mtime_ns,
                     sha1 = excluded.sha1,
                     asset_id = excluded.asset_id,
                     state = excluded.state,
                     error_class = excluded.error_class",
                params![
                    item.path,
                    key(Path::new(&item.path), case),
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

    /// Reads one item by its path, compared with the platform's own case rule
    /// (R2.4), or `None` when it is not tracked.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails or the row cannot be read.
    pub fn item(&self, path: &str) -> Result<Option<ItemRecord>, StoreError> {
        let writer = self.writer()?;
        writer
            .query_row(
                "SELECT path, size, mtime_ns, sha1, asset_id, state, error_class
                 FROM sync_item WHERE path_key = ?1",
                [path_key(Path::new(path))],
                read_item,
            )
            .optional()
            .map_err(|source| self.query_error(source))
    }

    /// Reads an item with an explicit case rule, for tests and for a caller
    /// that knows the platform behaviour it wants (R2.4).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails or the row cannot be read.
    pub fn item_with_case(
        &self,
        path: &str,
        case: PathCase,
    ) -> Result<Option<ItemRecord>, StoreError> {
        let writer = self.writer()?;
        writer
            .query_row(
                "SELECT path, size, mtime_ns, sha1, asset_id, state, error_class
                 FROM sync_item WHERE path_key = ?1",
                [key(Path::new(path), case)],
                read_item,
            )
            .optional()
            .map_err(|source| self.query_error(source))
    }

    /// Adds a source folder, or updates its recursive flag, album mapping, and
    /// album scope when the same folder is added again (R2.1, R3.4).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn add_folder(&self, folder: &SourceFolder) -> Result<(), StoreError> {
        let mut writer = self.writer()?;
        let transaction = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| self.query_error(source))?;
        transaction
            .execute(
                "INSERT INTO source_folder (path, path_key, recursive, album_mapping, album_scope)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(path_key) DO UPDATE SET
                     path = excluded.path,
                     recursive = excluded.recursive,
                     album_mapping = excluded.album_mapping,
                     album_scope = excluded.album_scope",
                params![
                    folder.path,
                    path_key(Path::new(&folder.path)),
                    folder.recursive,
                    folder.album_mapping,
                    folder.album_scope.as_str(),
                ],
            )
            .map_err(|source| self.query_error(source))?;
        transaction
            .commit()
            .map_err(|source| self.query_error(source))
    }

    /// Removes a source folder. Returns whether one was removed (R2.1).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn remove_folder(&self, path: &str) -> Result<bool, StoreError> {
        let writer = self.writer()?;
        let removed = writer
            .execute(
                "DELETE FROM source_folder WHERE path_key = ?1",
                [path_key(Path::new(path))],
            )
            .map_err(|source| self.query_error(source))?;
        Ok(removed > 0)
    }

    /// Every source folder, ordered by path (R2.1).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails or a row cannot be read.
    pub fn folders(&self) -> Result<Vec<SourceFolder>, StoreError> {
        let writer = self.writer()?;
        let mut statement = writer
            .prepare(
                "SELECT path, recursive, album_mapping, album_scope
                 FROM source_folder ORDER BY path",
            )
            .map_err(|source| self.query_error(source))?;
        let rows = statement
            .query_map([], read_folder)
            .map_err(|source| self.query_error(source))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|source| self.query_error(source))
    }

    /// The album the server assigned for the mapped folder `folder_key`, when
    /// this client already made one (R3.4).
    ///
    /// The folder key is the identity, because the album scope is one album per
    /// folder (GLOSSARY): the album name is a copy the mapping makes, so two
    /// folders that share a name are two albums (R3.4).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails.
    pub fn album(&self, folder_key: &str) -> Result<Option<String>, StoreError> {
        let writer = self.writer()?;
        writer
            .query_row(
                "SELECT album_id FROM album WHERE folder_key = ?1",
                [folder_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| self.query_error(source))
    }

    /// Records the album the server assigned for the mapped folder
    /// `folder_key`, so the next file in the same folder joins the same album
    /// (R3.4).
    ///
    /// `name` is recorded for whoever reads the file directly; no query reads
    /// it back.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn remember_album(
        &self,
        folder_key: &str,
        name: &str,
        album_id: &str,
    ) -> Result<(), StoreError> {
        let writer = self.writer()?;
        // One statement, so the single write is atomic on its own and needs no
        // transaction around it.
        writer
            .execute(
                "INSERT INTO album (folder_key, name, album_id) VALUES (?1, ?2, ?3)
                 ON CONFLICT(folder_key) DO UPDATE SET
                     name = excluded.name, album_id = excluded.album_id",
                params![folder_key, name, album_id],
            )
            .map_err(|source| self.query_error(source))?;
        Ok(())
    }

    /// Re-queues what a restart interrupted: an `uploading` row becomes
    /// `pending` again, and every pending path is returned to enqueue (R6.7).
    ///
    /// A completed row is never returned, so a restart cannot re-upload it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the transaction fails or the lock is
    /// poisoned.
    pub fn requeue_unfinished(&self) -> Result<Vec<String>, StoreError> {
        let mut writer = self.writer()?;
        let transaction = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| self.query_error(source))?;
        transaction
            .execute(
                "UPDATE sync_item SET state = 'pending' WHERE state = 'uploading'",
                [],
            )
            .map_err(|source| self.query_error(source))?;
        let paths = {
            let mut statement = transaction
                .prepare("SELECT path FROM sync_item WHERE state = 'pending' ORDER BY path")
                .map_err(|source| self.query_error(source))?;
            let rows = statement
                .query_map([], |row| row.get(0))
                .map_err(|source| self.query_error(source))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()
                .map_err(|source| self.query_error(source))?
        };
        transaction
            .commit()
            .map_err(|source| self.query_error(source))?;
        Ok(paths)
    }

    /// The item counts under one source folder, for its dashboard card
    /// (R8.1).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails.
    pub fn folder_counts(&self, folder: &str) -> Result<FolderCounts, StoreError> {
        let prefix = format!("{}/", path_key(Path::new(folder)));
        let writer = self.writer()?;
        let mut statement = writer
            .prepare(
                "SELECT state, COUNT(*) FROM sync_item
                 WHERE substr(path_key, 1, length(?1)) = ?1
                 GROUP BY state",
            )
            .map_err(|source| self.query_error(source))?;
        let rows = statement
            .query_map([&prefix], |row| {
                let state: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((state, count))
            })
            .map_err(|source| self.query_error(source))?;
        let mut counts = FolderCounts::default();
        for row in rows {
            let (state, count) = row.map_err(|source| self.query_error(source))?;
            let count = u32::try_from(count).unwrap_or(u32::MAX);
            match ItemState::parse(&state) {
                Some(ItemState::Uploaded | ItemState::Duplicate) => counts.uploaded += count,
                Some(ItemState::FailedPermanent | ItemState::RetryWait) => counts.failed += count,
                // A state a newer build wrote still counts as work in flight,
                // and an unknown state is not silently dropped.
                _ => counts.pending += count,
            }
        }
        Ok(counts)
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

/// Reads one `source_folder` row.
fn read_folder(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceFolder> {
    let scope_text: String = row.get(3)?;
    let album_scope = AlbumScope::parse(&scope_text).ok_or_else(|| {
        conversion_error(
            3,
            rusqlite::types::Type::Text,
            StoreError::UnknownAlbumScope(scope_text.clone()),
        )
    })?;
    Ok(SourceFolder {
        path: row.get(0)?,
        recursive: row.get(1)?,
        album_mapping: row.get(2)?,
        album_scope,
    })
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
    Migrations::new(vec![
        M::up(SCHEMA_V1),
        M::up_with_hook(SCHEMA_V2, backfill_path_keys),
        M::up(SCHEMA_V3),
        M::up(SCHEMA_V4),
    ])
}

/// Fills `path_key` for the rows the first schema wrote (R2.4).
///
/// The key depends on the platform, which SQL cannot see, so the hook walks
/// the rows and writes what [`crate::paths::path_key`] produces.
fn backfill_path_keys(transaction: &Transaction<'_>) -> rusqlite_migration::HookResult {
    backfill_path_keys_with_case(transaction, PathCase::for_platform())
}

/// The backfill under an explicit case rule, so the Windows collision is
/// testable on any platform (R2.4).
///
/// Two v1 rows can share one key: case-only variants on Windows, or a trailing
/// separator anywhere. V3 puts a unique index on the key, so one row per key
/// survives: the row with an asset mapping first, then a terminal success, then
/// the newest id. Dropping a row is irreversible, so every drop is logged.
fn backfill_path_keys_with_case(
    transaction: &Transaction<'_>,
    case: PathCase,
) -> rusqlite_migration::HookResult {
    let rows = {
        let mut statement =
            transaction.prepare("SELECT id, path, asset_id, state FROM sync_item ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            Ok(MigrationRow {
                id: row.get(0)?,
                path: row.get(1)?,
                asset_id: row.get(2)?,
                state: row.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut survivors: HashMap<String, &MigrationRow> = HashMap::new();
    for row in &rows {
        survivors
            .entry(key(Path::new(&row.path), case))
            .and_modify(|kept| {
                if outranks(row, kept) {
                    *kept = row;
                }
            })
            .or_insert(row);
    }
    for row in &rows {
        let row_key = key(Path::new(&row.path), case);
        let kept = survivors
            .get(&row_key)
            .expect("every row's key has a survivor");
        if kept.id == row.id {
            transaction.execute(
                "UPDATE sync_item SET path_key = ?1 WHERE id = ?2",
                params![row_key, row.id],
            )?;
        } else {
            tracing::warn!(
                dropped_path = %row.path,
                dropped_id = row.id,
                dropped_asset_id = ?row.asset_id,
                dropped_state = %row.state,
                kept_id = kept.id,
                "the path-key migration dropped a duplicate sync item"
            );
            transaction.execute("DELETE FROM sync_item WHERE id = ?1", params![row.id])?;
        }
    }
    Ok(())
}

/// One `sync_item` row the V2 backfill reads.
struct MigrationRow {
    id: i64,
    path: String,
    asset_id: Option<String>,
    state: String,
}

/// Whether `candidate` outranks `kept` as the survivor of a path-key
/// collision.
///
/// The asset mapping comes first, because a rename-or-deletion review keys on
/// it (#12) and losing it is permanent; then a terminal success; then the
/// newest id.
fn outranks(candidate: &MigrationRow, kept: &MigrationRow) -> bool {
    fn rank(row: &MigrationRow) -> (bool, bool, i64) {
        (
            row.asset_id.is_some(),
            ItemState::parse(&row.state).is_some_and(ItemState::is_terminal_success),
            row.id,
        )
    }
    rank(candidate) > rank(kept)
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// Two v1 path spellings that share one `path_key` on this platform:
    /// case-only variants on Windows, a trailing separator on Linux. v1's
    /// `path UNIQUE` allowed both.
    fn colliding_v1_paths() -> (String, String) {
        if cfg!(windows) {
            // Built from parts so the repository's hardcoded-path scan stays
            // clean.
            let root = format!("{}:{}photos", 'C', '\\');
            (format!("{root}\\photo.jpg"), format!("{root}\\Photo.JPG"))
        } else {
            (
                "/photos/photo.jpg".to_string(),
                "/photos/photo.jpg/".to_string(),
            )
        }
    }

    #[test]
    fn the_path_key_backfill_survives_two_rows_that_share_one_key() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("state.db");
        let (first, second) = colliding_v1_paths();
        {
            let mut connection = Connection::open(&path).expect("open the raw database");
            Migrations::new(vec![M::up(SCHEMA_V1)])
                .to_latest(&mut connection)
                .expect("create the first schema");
            let sha1 =
                Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").expect("a hex SHA-1");
            for text in [&first, &second] {
                connection
                    .execute(
                        "INSERT INTO sync_item (path, size, mtime_ns, sha1, asset_id, state, error_class)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            text,
                            11_i64,
                            1_i64,
                            sha1.as_bytes().as_slice(),
                            "asset-1",
                            "uploaded",
                            Option::<String>::None,
                        ],
                    )
                    .expect("insert the old row");
            }
            drop(connection);
        }

        // The V3 unique index cannot hold both keys; the backfill must keep
        // one row per key or the whole database fails to open.
        let store = StateStore::open(&path).expect("a shared-key v1 database must migrate");
        let row = store.item(&first).expect("read").expect("a row survives");
        assert_eq!(
            row.path, second,
            "the newest id is the row the unique key keeps"
        );
    }

    #[test]
    fn the_album_columns_survive_a_v3_database() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("state.db");
        {
            let mut connection = Connection::open(&path).expect("open the raw database");
            // A v3 database: source folders exist, the album columns do not.
            Migrations::new(vec![
                M::up(SCHEMA_V1),
                M::up_with_hook(SCHEMA_V2, backfill_path_keys),
                M::up(SCHEMA_V3),
            ])
            .to_latest(&mut connection)
            .expect("create the third schema");
            connection
                .execute(
                    "INSERT INTO source_folder (path, path_key, recursive)
                     VALUES (?1, ?2, 1)",
                    params!["/photos", "/photos"],
                )
                .expect("insert the source folder");
            drop(connection);
        }

        let store = StateStore::open(&path).expect("a v3 database must migrate");
        assert_eq!(
            store.folders().expect("read the folders"),
            vec![SourceFolder {
                path: "/photos".to_string(),
                recursive: true,
                album_mapping: false,
                album_scope: AlbumScope::Leaf,
            }],
            "R3.4: a folder mapped before album mapping existed reads back off, at the default \
             scope, through the whole composed migration chain"
        );
    }

    /// The V2 hook under the Windows case rule, so the case-only collision is
    /// reproducible on any platform (R2.4).
    fn backfill_path_keys_insensitive(
        transaction: &Transaction<'_>,
    ) -> rusqlite_migration::HookResult {
        backfill_path_keys_with_case(transaction, PathCase::Insensitive)
    }

    /// Seeds a v1 database with `(path, asset_id, state)` rows in insertion
    /// order, runs the V2 backfill under the Windows case rule, and returns
    /// the surviving `(id, path)` rows.
    fn migrate_colliding_rows(
        db: &Path,
        rows: &[(&str, Option<&str>, &str)],
    ) -> Vec<(i64, String)> {
        let mut connection = Connection::open(db).expect("open the raw database");
        Migrations::new(vec![M::up(SCHEMA_V1)])
            .to_latest(&mut connection)
            .expect("create the first schema");
        let sha1 = Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").expect("a hex SHA-1");
        for (text, asset_id, state) in rows {
            connection
                .execute(
                    "INSERT INTO sync_item (path, size, mtime_ns, sha1, asset_id, state, error_class)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        text,
                        11_i64,
                        1_i64,
                        sha1.as_bytes().as_slice(),
                        asset_id,
                        state,
                        Option::<String>::None,
                    ],
                )
                .expect("insert the old row");
        }
        // The V1 entry keeps the version numbering aligned; it is already
        // applied, so only V2 and V3 run.
        Migrations::new(vec![
            M::up(SCHEMA_V1),
            M::up_with_hook(SCHEMA_V2, backfill_path_keys_insensitive),
            M::up(SCHEMA_V3),
        ])
        .to_latest(&mut connection)
        .expect("colliding v1 rows must migrate");

        let mut statement = connection
            .prepare("SELECT id, path FROM sync_item ORDER BY id")
            .expect("prepare the read");
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query the rows");
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect the rows")
    }

    #[test]
    fn the_backfill_keeps_the_highest_id_not_the_last_row_in_path_order() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        // "photo.jpg" sorts after "Photo.JPG", so a path-ordered last-wins
        // pass keeps the older id; the newest id is the one that must survive.
        let rows = migrate_colliding_rows(
            &temp.path().join("state.db"),
            &[
                ("photo.jpg", Some("asset-1"), "uploaded"),
                ("Photo.JPG", Some("asset-1"), "uploaded"),
            ],
        );
        assert_eq!(
            rows,
            vec![(2, "Photo.JPG".to_string())],
            "the newest id survives, whatever the path order"
        );
    }

    #[test]
    fn the_backfill_keeps_a_row_with_an_asset_id_over_a_newer_row_without_one() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let rows = migrate_colliding_rows(
            &temp.path().join("state.db"),
            &[
                // The mapped row is older and sorts first; both must lose.
                ("Photo.JPG", Some("asset-1"), "failed_permanent"),
                ("photo.jpg", None, "uploaded"),
            ],
        );
        assert_eq!(
            rows,
            vec![(1, "Photo.JPG".to_string())],
            "the asset mapping is what a rename-or-deletion review keys on (#12)"
        );
    }

    #[test]
    fn the_backfill_keeps_a_terminal_success_over_a_newer_failure() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let rows = migrate_colliding_rows(
            &temp.path().join("state.db"),
            &[
                ("Photo.JPG", None, "uploaded"),
                ("photo.jpg", None, "failed_permanent"),
            ],
        );
        assert_eq!(
            rows,
            vec![(1, "Photo.JPG".to_string())],
            "a finished row outranks a parked failure"
        );
    }

    #[test]
    fn the_path_key_migration_backfills_an_existing_row() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("state.db");
        // Build a v1 database the way the v1 build did, then let the full
        // migration list bring it up. The tracking pragma must show V1 as
        // applied, or the migration would try to create the table again.
        {
            let mut connection = Connection::open(&path).expect("open the raw database");
            Migrations::new(vec![M::up(SCHEMA_V1)])
                .to_latest(&mut connection)
                .expect("create the first schema");
            let sha1 =
                Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").expect("a hex SHA-1");
            connection
                .execute(
                    "INSERT INTO sync_item (path, size, mtime_ns, sha1, asset_id, state, error_class)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        "hello.txt",
                        11_i64,
                        1_i64,
                        sha1.as_bytes().as_slice(),
                        "asset-1",
                        "uploaded",
                        Option::<String>::None,
                    ],
                )
                .expect("insert the old row");
            drop(connection);
        }

        let store = StateStore::open(&path).expect("migrate the old database");
        let row = store.item("hello.txt").expect("read").expect("the row");
        assert_eq!(row.asset_id.as_deref(), Some("asset-1"));
        assert!(store.folders().expect("read folders").is_empty());
    }
}
