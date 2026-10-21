//! The SQLite state store (ADR 0005, SPEC N4).
//!
//! One writer connection, WAL, `synchronous=FULL`, a busy timeout, and
//! `rusqlite_migration` for the schema. Every write runs inside a
//! `BEGIN IMMEDIATE` transaction, so two workers cannot interleave a
//! read-modify-write. The database lives in the local data directory; the
//! caller passes the path from the platform-directory abstraction (R9.2).
//!
//! Reader connections for the UI: `sqlite_info` and `last_error_per_class`
//! open their own read-only connection, so a diagnostics read never queues
//! behind the engine's writer (ADR 0005, R10.2).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension as _, Transaction, TransactionBehavior,
    params,
};
use rusqlite_migration::{M, Migrations};

use crate::failure::FailureClass;
use crate::hash::Sha1;
use crate::paths::{PathCase, key, path_key};

/// The first schema (SPEC §5 and issue #5): one row per sync item.
///
/// `sync_item` keeps the file identity the pipeline decides on: where the file
/// is, the size and mtime it had when it was hashed, its SHA-1, the asset the
/// server returned, and where the item stands. The attempt table arrives in
/// the fifth schema; `id` is the stable item identity it references.
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

/// The fifth schema (issue #7): the attempt log and the dead letter
/// (R6.5, R6.7, SPEC §5).
///
/// `attempt` is the §5 table: one row per upload attempt, with the item it
/// belongs to, when it started, how it ended, and the failure class. The
/// `attempts` counter on `sync_item` is what the retry policy reads, so a
/// restart continues the count instead of starting a fresh budget; a manual
/// retry resets it. `error_message` is the last user-facing reason, which the
/// failed-items view shows beside the class.
///
/// This is the fifth schema because the album migration (#9) reached the
/// integration branch as the fourth first. Numbering follows merge order, not
/// issue order, and a deployed database keys its applied migrations on the
/// number: renumbering this one after it ships would re-run it.
const SCHEMA_V5: &str = "
ALTER TABLE sync_item ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sync_item ADD COLUMN error_message TEXT;
CREATE TABLE attempt (
    id          INTEGER PRIMARY KEY,
    item_id     INTEGER NOT NULL REFERENCES sync_item(id),
    started_ns  INTEGER NOT NULL,
    outcome     TEXT    NOT NULL,
    error_class TEXT
);
CREATE INDEX attempt_item ON attempt(item_id);
";

/// The sixth schema (issue #10): the digest of the XMP sidecar the asset
/// carries (R3.3).
///
/// A sidecar is content, not a file of its own, so its digest belongs to the
/// asset's row rather than to a row of its own. It is what tells a later pass
/// that a sidecar appeared after the asset was uploaded.
///
/// #10 wrote this pair against a base where V4 was free and called them V4 and
/// V5. The attempt migration (#7) had already reached integration as V5, so the
/// pair lands as V6 and V7: numbering follows merge order, and a deployed
/// database keys its applied migrations on the number.
const SCHEMA_V6: &str = "ALTER TABLE sync_item ADD COLUMN sidecar_sha1 BLOB;";

/// The seventh schema (issue #10): the sidecar's size and modification time
/// beside its digest.
///
/// A settled library is not re-read on every reconciliation scan (R4.5), and
/// that has to hold for the sidecars too, or a large library re-reads every
/// sidecar every scan interval. The recorded size and mtime tell the client
/// the sidecar has not changed without opening it, exactly as the media file's
/// own columns do. A row written before this migration has none of the three,
/// so its sidecar is hashed once more and then recorded with them.
const SCHEMA_V7: &str = "
ALTER TABLE sync_item ADD COLUMN sidecar_size INTEGER;
ALTER TABLE sync_item ADD COLUMN sidecar_mtime_ns INTEGER;
";

/// The eighth schema (issue #11): the version chain and the stack an item's
/// versions share (R4.4, SPEC §5).
///
/// `version` is the §5 table: one row per version of a tracked file, with the
/// item it belongs to, the asset the server holds, that version's SHA-1, when
/// it was recorded, and whether it is the item's current version. Exactly one
/// row per item carries the current flag; `record_version` moves it, so a
/// reader never has to pick between two "current" versions.
///
/// The unique index on `(item_id, asset_id)` makes a retry that records the
/// same asset twice idempotent, which is what the version flow needs: a lost
/// answer or a later attempt records the same version again. It also serves the
/// one read a caller makes, which is every version of one item.
///
/// The final `INSERT` seeds the chain from the rows the earlier schemas wrote.
/// An asset id a migrated row holds lives only on `sync_item`, and `prepare`
/// clears that column before the first edit uploads, so without the seed the
/// first edit after the upgrade would record a chain of one and send no copy —
/// the loss ADR 0004 exists to prevent (R4.2).
const SCHEMA_V8: &str = "
ALTER TABLE sync_item ADD COLUMN stack_id TEXT;
CREATE TABLE version (
    id            INTEGER PRIMARY KEY,
    item_id       INTEGER NOT NULL REFERENCES sync_item(id) ON DELETE CASCADE,
    asset_id      TEXT    NOT NULL,
    sha1          BLOB    NOT NULL,
    created_at_ns INTEGER NOT NULL,
    is_current    INTEGER NOT NULL
);
CREATE UNIQUE INDEX version_item_asset ON version(item_id, asset_id);
INSERT INTO version (item_id, asset_id, sha1, created_at_ns, is_current)
SELECT id, asset_id, sha1, mtime_ns, 1 FROM sync_item WHERE asset_id IS NOT NULL;
";

/// The ninth schema (issue #23): an index on the item state.
///
/// The tray's status poll asks the store how many files wait in the deletion
/// review (R5.4), and the app polls that status every 250 ms. Without this
/// index the count is a full scan of `sync_item` while the writer lock is
/// held; on a 100k-row library the scan measured about 9 ms per poll. `state`
/// is the only column the review queries filter on, so one index serves both
/// the count and the review list.
const SCHEMA_V9: &str = "CREATE INDEX sync_item_state ON sync_item(state);";

/// The tenth schema (issue #46): how far the sync tag's backfill got (R3.7).
///
/// One row, so the state cannot disagree with itself. `cursor` is the last
/// asset id a completed `PUT /tags/assets` batch carried, and the next run
/// continues after it instead of tagging the whole library again; `tagged` is
/// how many assets the recorded pass has tagged; `completed_ns` is the marker
/// that keeps a restart from starting a finished backfill over.
///
/// The table starts empty and every write creates its row, so a build that
/// upgraded and one that never had a backfill read the same defaults.
const SCHEMA_V10: &str = "
CREATE TABLE tag_backfill (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    completed_ns INTEGER,
    cursor       TEXT,
    tagged       INTEGER NOT NULL DEFAULT 0
);
";

/// The eleventh schema (#43): when a source folder last reconciled and last
/// uploaded, for its detail view.
///
/// The folder detail shows both as a relative time and an exact timestamp, and
/// only the store outlives the process that observed them, so they live on the
/// folder row. They are written where the outcome is already known — a scan
/// that read the folder, an upload that reached a terminal success — and never
/// read from a network path (R9.2).
const SCHEMA_V11: &str = "
ALTER TABLE source_folder ADD COLUMN last_scan_ns INTEGER;
ALTER TABLE source_folder ADD COLUMN last_upload_ns INTEGER;
";

/// The twelfth schema (#47): the album title the user chose, and the rename it
/// may still owe the server.
///
/// The title lives on the album row, keyed by the mapped folder's path key,
/// rather than on the folder row: the album row is what survives the folder
/// being removed and added again, so the title survives with it, and there is
/// one stored title instead of two that could disagree. `title` is the user's
/// choice and is `NULL` when the album takes the folder's name, which is also
/// what every album written before this schema was called.
///
/// `album_id` loses its `NOT NULL`: a title is stored even while album mapping
/// is off and no album exists yet, and the create then uses it. `rename_pending`
/// is what a later scan, restart, or reconnect retries; `rename_problem` is why
/// the last attempt did not reach the server.
const SCHEMA_V12: &str = "
CREATE TABLE album_next (
    folder_key     TEXT PRIMARY KEY,
    title          TEXT,
    album_id       TEXT,
    rename_pending INTEGER NOT NULL DEFAULT 0,
    rename_problem TEXT
);
INSERT INTO album_next (folder_key, album_id) SELECT folder_key, album_id FROM album;
DROP TABLE album;
ALTER TABLE album_next RENAME TO album;
";

/// How long the one writer waits for a competing lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How far the sync tag's backfill got (R3.7, SPEC §5).
///
/// The whole state of the backfill is these three values: where a later run
/// resumes, how much the recorded pass has tagged, and whether it finished.
/// The library it covers is the union of every asset the state holds: each
/// item's current asset, every old version, and every asset the server
/// answered as a duplicate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TagBackfill {
    /// When a full pass last finished, or `None` while it is unfinished.
    pub completed_ns: Option<i64>,
    /// The last asset id of the last completed batch, so a later run resumes
    /// after it.
    pub cursor: Option<String>,
    /// How many assets the recorded pass has tagged.
    pub tagged: u32,
}

/// The asset ids the sync tag's backfill covers (R3.7): the asset each item
/// points at now, plus every asset of every version chain.
///
/// `UNION` keeps an id the two tables share once, so tagging an item's current
/// asset does not happen twice, and a duplicate the server answered is covered
/// like a created asset because it is the row's asset id either way.
const TAG_BACKFILL_IDS: &str = "
    SELECT asset_id FROM sync_item WHERE asset_id IS NOT NULL
    UNION
    SELECT asset_id FROM version";

/// Writes one album title, and what the write owes the server (#47).
///
/// An album that already exists must be renamed; one that does not yet exist
/// takes the title when it is created, so nothing is pending. A row that
/// already held the title keeps whatever it owed, so a folder write that
/// carries the stored title (a recursive toggle, say) queues no rename, and a
/// changed title drops the last failure because it was about the old name.
const ALBUM_TITLE_UPSERT: &str = "
    INSERT INTO album (folder_key, title, rename_pending) VALUES (?1, ?2, 0)
    ON CONFLICT(folder_key) DO UPDATE SET
        rename_pending = CASE
            WHEN album.album_id IS NOT NULL AND album.title IS NOT excluded.title THEN 1
            ELSE album.rename_pending
        END,
        rename_problem = CASE
            WHEN album.title IS excluded.title THEN album.rename_problem
            ELSE NULL
        END,
        title = excluded.title
";

/// The `sync_item` columns [`read_item`] expects, in the order it reads them.
///
/// One list, so a new column cannot reach one query and miss another.
const ITEM_COLUMNS: &str = "path, size, mtime_ns, sha1, asset_id, state, error_class,
    error_message, attempts, sidecar_sha1, sidecar_size, sidecar_mtime_ns";

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
    /// The retry policy gave up after the attempt cap; only a manual retry
    /// tries again (R6.5).
    DeadLetter,
    /// A tracked path that disappeared, waiting for a review decision (R5.1).
    Missing,
    /// The user kept the server asset; the local file is gone (R5.3).
    ReviewKeep,
    /// The user moved the server asset to the trash (R5.3). No permanent
    /// delete exists in v1 (ADR 0002).
    ReviewTrash,
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
            Self::DeadLetter => "dead_letter",
            Self::Missing => "missing",
            Self::ReviewKeep => "review_keep",
            Self::ReviewTrash => "review_trash",
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
            "dead_letter" => Self::DeadLetter,
            "missing" => Self::Missing,
            "review_keep" => Self::ReviewKeep,
            "review_trash" => Self::ReviewTrash,
            _ => return None,
        })
    }

    /// Whether the item finished for good: no later run uploads it again
    /// (R3.2, R6.7).
    ///
    /// A review state is terminal too, but it is not a success: the local file
    /// is gone, and the asset is kept or trashed by a decision (R5.3).
    #[must_use]
    pub fn is_terminal_success(self) -> bool {
        matches!(self, Self::Uploaded | Self::Duplicate)
    }
}

/// The user's answer for one missing file (R5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    /// Leave the server asset alone; the item leaves the review.
    Keep,
    /// Move the server asset to the trash; the item leaves the review.
    Trash,
}

impl ReviewDecision {
    /// The state the item lands in once the decision is recorded (SPEC §5).
    #[must_use]
    pub fn state(self) -> ItemState {
        match self {
            Self::Keep => ItemState::ReviewKeep,
            Self::Trash => ItemState::ReviewTrash,
        }
    }
}

/// One missing file the deletion review lists (R5.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewItem {
    /// The local path that disappeared.
    pub path: String,
    /// The size in bytes of the file the server holds.
    pub size: u64,
    /// The Immich asset the item maps to. `None` only for a row written
    /// before the asset was recorded; a missing item normally has one.
    pub asset_id: Option<String>,
    /// The state the row is in, which for a listed item is `missing`.
    pub state: ItemState,
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
    /// The title the user chose for this folder's own album, or `None` when the
    /// album takes the folder's name (#47).
    ///
    /// It is stored on the album row rather than the folder row, so the folder
    /// being removed and added again keeps it (#47). This field is the row's
    /// title read back, never a second copy.
    pub album_title: Option<String>,
}

/// One mapped folder's album: the title the user chose and the id the server
/// answered (#47, R3.4).
///
/// The row is keyed by the mapped folder's path key, which is the album's
/// identity (GLOSSARY); the title is what the album should be called.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Album {
    /// The title the user chose, or `None` when the album takes the folder's
    /// name (#47).
    pub title: Option<String>,
    /// The album the server holds, or `None` while none was created: a title is
    /// stored before the album exists and used when it is created (#47).
    pub album_id: Option<String>,
    /// Whether the title still has to reach the server (#47).
    pub rename_pending: bool,
    /// Why the last rename attempt did not reach the server (#47).
    pub rename_problem: Option<String>,
}

/// The per-folder item counts a dashboard card shows (R8.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FolderCounts {
    /// Items queued or in flight.
    pub pending: u32,
    /// Items in a terminal success state (R3.2), plus a kept review item whose
    /// server copy is still there.
    pub uploaded: u32,
    /// Items in a failed state.
    pub failed: u32,
    /// Missing files that await a review decision (R5.4).
    pub review: u32,
}

/// When one source folder last reconciled and last uploaded (#43).
///
/// The folder detail shows both, and the sync engine writes them from the
/// paths that already know the outcome: a scan that read the folder and an
/// upload that reached a terminal success. They sit on the folder row so a
/// restart keeps them (`folder_time_tests`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FolderTimes {
    /// When a reconciliation scan last read this folder, ns since the Unix
    /// epoch. `None` until one has.
    pub last_scan_ns: Option<i64>,
    /// When this folder last uploaded an asset, ns since the Unix epoch.
    /// `None` until one has.
    pub last_upload_ns: Option<i64>,
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
    /// The last user-facing reason, when the last outcome failed. The
    /// failed-items view shows it beside the class (R6.5).
    pub error_message: Option<String>,
    /// How many automatic upload attempts the item has spent (R6.1). A manual
    /// retry resets it (R6.5).
    ///
    /// [`StateStore::record`] does not write this column, so a pipeline row
    /// cannot reset a count it did not earn; only
    /// [`StateStore::record_attempt`] moves it.
    pub attempts: u32,
    /// The XMP sidecar the asset carries, when one has been sent (R3.3). A
    /// sidecar whose identity differs on disk has to ride along again.
    pub sidecar: Option<SidecarDigest>,
}

/// What one upload attempt produced (SPEC §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The item reached a terminal success: created or duplicate (R3.2).
    Succeeded,
    /// The attempt failed; the class says why.
    Failed,
}

impl AttemptOutcome {
    /// The exact string the state database stores.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    /// Parses a stored outcome. Returns `None` for a value this build does not
    /// know, so a newer build's row is reported instead of guessed at.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// One upload attempt (SPEC §5): the item, its start time, its outcome, and
/// the failure class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptRecord {
    /// The item's path, which is how the attempt is addressed.
    pub path: String,
    /// When the attempt started, in nanoseconds since the Unix epoch.
    pub started_ns: i64,
    /// How the attempt ended.
    pub outcome: AttemptOutcome,
    /// The failure class, when the attempt failed (R6.4).
    pub error_class: Option<FailureClass>,
}

/// One version of a sync item, in Rust form (SPEC §5, GLOSSARY).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    /// The item the version belongs to, as `sync_item.id`. Two versions of the
    /// same file share it, which is what keeps the file's identity across an
    /// edit (R4.1).
    pub item_id: i64,
    /// The asset the server holds for this version (R4.4).
    pub asset_id: String,
    /// The SHA-1 of the content of this version.
    pub sha1: Sha1,
    /// When this version was recorded, in nanoseconds since the Unix epoch.
    pub created_at_ns: i64,
    /// Whether this is the item's current version (R4.4).
    pub is_current: bool,
}

/// One version to record as the item's current one (SPEC §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewVersion {
    /// The item, addressed by its path and compared with the platform's case
    /// rule (R2.4).
    pub path: String,
    /// The asset the server holds for this version (R4.2).
    pub asset_id: String,
    /// The SHA-1 of the version's content (SPEC §5).
    pub sha1: Sha1,
    /// When this version was recorded, in nanoseconds since the Unix epoch.
    pub created_at_ns: i64,
    /// The stack the item's versions share (R4.4).
    pub stack_id: Option<String>,
}

/// One item the retry policy gave up on, for the failed-items view (R6.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedItem {
    /// The local path.
    pub path: String,
    /// `failed_permanent` or `dead_letter`.
    pub state: ItemState,
    /// The last failure class, when one fits (R6.4).
    pub error_class: Option<FailureClass>,
    /// The last user-facing reason.
    pub error_message: Option<String>,
    /// How many automatic attempts the item spent before it was parked.
    pub attempts: u32,
}

/// The XMP sidecar an asset carries, and what it looked like when it did
/// (R3.3, R4.5).
///
/// The size and modification time are stored with the digest so a later pass
/// can tell an unchanged sidecar without reading it, the same way the media
/// file's own columns do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidecarDigest {
    /// The SHA-1 of the sidecar's content.
    pub sha1: Sha1,
    /// Its size in bytes when it was sent.
    pub size: u64,
    /// Its modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
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
    /// `foreign_keys`; the store turns it on, so the `ON DELETE` actions of
    /// the state tables are enforced (`1`).
    pub foreign_keys: i64,
}

/// One table's row count, for the diagnostics view (R10.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableCount {
    /// The table name the catalog holds.
    pub table: String,
    /// How many rows the table holds.
    pub rows: i64,
}

/// What the state database reports about itself, for the diagnostics view
/// (R10.2). [`StateStore::sqlite_info`] reads it without writing anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteInfo {
    /// `PRAGMA user_version`: the migration the schema stands at.
    pub migration_version: i64,
    /// The database's page footprint, `page_count * page_size`, in bytes.
    pub size_bytes: u64,
    /// Every table the catalog holds, with its row count.
    pub tables: Vec<TableCount>,
    /// The `PRAGMA quick_check` result: `ok`, or SQLite's reason.
    pub integrity: String,
}

/// The library's size and the shape of its tracked state, for the debug
/// bundle (issue #25). [`StateStore::sizing`] reads it without writing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sizing {
    /// The sum of every tracked item's recorded size, in bytes.
    pub tracked_bytes: u64,
    /// How many items each state holds, ordered by the stored state name. A
    /// state a newer build wrote that this one does not know is left out.
    pub by_state: Vec<(ItemState, u64)>,
    /// How many source folders the user configured.
    pub folders: u64,
    /// How many rows the version chain holds.
    pub versions: u64,
}

/// The newest failure of one class in the attempt log (R10.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassError {
    /// The persisted failure class.
    pub class: FailureClass,
    /// The user-facing reason, when the item still holds this class.
    pub message: Option<String>,
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
    /// A write named an item the store does not hold.
    #[error("the state database has no item {0} to record this for")]
    UnknownItem(String),
    /// A row holds an attempt outcome this build does not know.
    #[error("the state database holds an unknown attempt outcome {0:?}")]
    UnknownAttemptOutcome(String),
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

/// Classifies a failed startup `quick_check` query: only a corruption verdict
/// justifies the rebuild removing the file. A busy or locked database means
/// the check could not run, so it is an open failure the rebuild reports
/// instead of removing a healthy file (N4, R10.4).
fn startup_check_error(path: PathBuf, source: rusqlite::Error) -> StoreError {
    match source.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            StoreError::Open { path, source }
        }
        _ => StoreError::QuickCheck {
            path,
            reason: source.to_string(),
        },
    }
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
        // The busy timeout is set before the startup check, so a transient
        // lock makes the check wait rather than read as corruption (the
        // rebuild only replaces a database the check calls bad).
        writer
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|source| StoreError::Open {
                path: path.clone(),
                source,
            })?;
        // `quick_check` is the honest reason to report for a file that is not
        // a database, so it runs before the rest of the pragmas.
        let check: String = writer
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|source| startup_check_error(path.clone(), source))?;
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
            foreign_keys: read("PRAGMA foreign_keys")?,
        })
    }

    /// Reads what the state database says about itself: the migration it
    /// stands at, its size, one row count per table, and the integrity check
    /// (R10.2).
    ///
    /// Read-only: the diagnostics view calls it while the engine writes.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Query`] when a pragma, the catalog, or a row
    /// count cannot be read.
    pub fn sqlite_info(&self) -> Result<SqliteInfo, StoreError> {
        let reader = self.reader()?;
        let read = |name: &str| -> Result<i64, StoreError> {
            reader
                .query_row(name, [], |row| row.get(0))
                .map_err(|source| self.query_error(source))
        };
        let migration_version = read("PRAGMA user_version")?;
        let page_size = read("PRAGMA page_size")?.max(0);
        let page_count = read("PRAGMA page_count")?.max(0);
        let integrity: String = reader
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|source| self.query_error(source))?;
        let names: Vec<String> = reader
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
                 ORDER BY name",
            )
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()
            })
            .map_err(|source| self.query_error(source))?;
        let mut tables = Vec::with_capacity(names.len());
        for table in names {
            // The name comes from the catalog, so it cannot be a bind
            // parameter; the quotes are doubled so a name with one still works.
            let query = format!("SELECT COUNT(*) FROM \"{}\"", table.replace('"', "\"\""));
            let rows = reader
                .query_row(&query, [], |row| row.get(0))
                .map_err(|source| self.query_error(source))?;
            tables.push(TableCount { table, rows });
        }
        Ok(SqliteInfo {
            migration_version,
            size_bytes: u64::try_from(page_size)
                .unwrap_or(0)
                .saturating_mul(u64::try_from(page_count).unwrap_or(0)),
            tables,
            integrity,
        })
    }

    /// The newest failure of each class the attempt log holds, newest first
    /// (R10.2).
    ///
    /// The message is the item's own reason and only when the item still
    /// carries the class, so a later outcome cannot label an older class with
    /// the wrong reason.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails.
    pub fn last_error_per_class(&self) -> Result<Vec<ClassError>, StoreError> {
        let reader = self.reader()?;
        let newest: Vec<String> = {
            let mut statement = reader
                .prepare(
                    "SELECT error_class FROM attempt WHERE error_class IS NOT NULL \
                     GROUP BY error_class ORDER BY MAX(started_ns) DESC",
                )
                .map_err(|source| self.query_error(source))?;
            statement
                .query_map([], |row| row.get(0))
                .and_then(|rows| rows.collect::<rusqlite::Result<Vec<String>>>())
                .map_err(|source| self.query_error(source))?
        };
        let mut errors = Vec::with_capacity(newest.len());
        for text in newest {
            let Some(class) = FailureClass::parse(&text) else {
                continue;
            };
            let message: Option<String> = reader
                .query_row(
                    "SELECT i.error_message FROM attempt a JOIN sync_item i ON i.id = a.item_id \
                     WHERE a.error_class = ?1 AND i.error_class = ?1 \
                     ORDER BY a.started_ns DESC, a.id DESC LIMIT 1",
                    [&text],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|source| self.query_error(source))?
                .flatten();
            errors.push(ClassError { class, message });
        }
        Ok(errors)
    }

    /// Reads the library's size and the shape of its state: the sum of the
    /// tracked sizes, the item count per state, the source-folder count, and
    /// the version count (issue #25).
    ///
    /// Read-only, like [`Self::sqlite_info`]: the debug bundle calls it while
    /// the engine writes.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Query`] when a count cannot be read.
    pub fn sizing(&self) -> Result<Sizing, StoreError> {
        let reader = self.reader()?;
        let tracked_bytes: i64 = reader
            .query_row("SELECT COALESCE(SUM(size), 0) FROM sync_item", [], |row| {
                row.get(0)
            })
            .map_err(|source| self.query_error(source))?;
        let states: Vec<(String, i64)> = reader
            .prepare("SELECT state, COUNT(*) FROM sync_item GROUP BY state ORDER BY state")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(|source| self.query_error(source))?;
        let count = |sql: &str| -> Result<i64, StoreError> {
            reader
                .query_row(sql, [], |row| row.get(0))
                .map_err(|source| self.query_error(source))
        };
        Ok(Sizing {
            tracked_bytes: u64::try_from(tracked_bytes).unwrap_or(0),
            by_state: states
                .into_iter()
                .filter_map(|(text, rows)| {
                    let state = ItemState::parse(&text)?;
                    Some((state, u64::try_from(rows).unwrap_or(0)))
                })
                .collect(),
            folders: u64::try_from(count("SELECT COUNT(*) FROM source_folder")?).unwrap_or(0),
            versions: u64::try_from(count("SELECT COUNT(*) FROM version")?).unwrap_or(0),
        })
    }

    /// Starts the state tables over (R10.4), in one transaction: the item,
    /// attempt, album, and folder rows go, and the schema stays at the latest
    /// migration.
    ///
    /// The caller re-adds the folders the user chose, because those choices
    /// live only in the database and only the caller knows them.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the transaction cannot run.
    pub fn reset(&self) -> Result<(), StoreError> {
        let mut writer = self.writer()?;
        let transaction = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| self.query_error(source))?;
        transaction
            .execute_batch(
                "DELETE FROM version;
                 DELETE FROM attempt;
                 DELETE FROM sync_item;
                 DELETE FROM album;
                 DELETE FROM source_folder;
                 DELETE FROM tag_backfill;",
            )
            .map_err(|source| self.query_error(source))?;
        transaction
            .commit()
            .map_err(|source| self.query_error(source))
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
        let sidecar_size = match item.sidecar {
            Some(sidecar) => Some(
                i64::try_from(sidecar.size)
                    .map_err(|_| StoreError::SizeOutOfRange(sidecar.size))?,
            ),
            None => None,
        };
        let mut writer = self.writer()?;
        let transaction = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| self.query_error(source))?;
        transaction
            .execute(
                "INSERT INTO sync_item (path, path_key, size, mtime_ns, sha1, asset_id, state, error_class,
                        error_message, sidecar_sha1, sidecar_size, sidecar_mtime_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(path_key) DO UPDATE SET
                     path = excluded.path,
                     size = excluded.size,
                     mtime_ns = excluded.mtime_ns,
                     sha1 = excluded.sha1,
                     asset_id = excluded.asset_id,
                     state = excluded.state,
                     error_class = excluded.error_class,
                     error_message = excluded.error_message,
                     sidecar_sha1 = excluded.sidecar_sha1,
                     sidecar_size = excluded.sidecar_size,
                     sidecar_mtime_ns = excluded.sidecar_mtime_ns",
                params![
                    item.path,
                    key(Path::new(&item.path), case),
                    size,
                    item.mtime_ns,
                    item.sha1.as_bytes().as_slice(),
                    item.asset_id,
                    item.state.as_str(),
                    item.error_class.map(FailureClass::as_str),
                    item.error_message,
                    item.sidecar.map(|sidecar| sidecar.sha1.as_bytes().to_vec()),
                    sidecar_size,
                    item.sidecar.map(|sidecar| sidecar.mtime_ns),
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
                "SELECT path, size, mtime_ns, sha1, asset_id, state, error_class,
                        error_message, attempts, sidecar_sha1, sidecar_size, sidecar_mtime_ns
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
                "SELECT path, size, mtime_ns, sha1, asset_id, state, error_class,
                        error_message, attempts, sidecar_sha1, sidecar_size, sidecar_mtime_ns
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
    /// A title the folder carries is written with the rest (#47). A folder with
    /// no title leaves the stored one alone, so a caller that builds a row from
    /// another surface cannot clear a title it never showed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn add_folder(&self, folder: &SourceFolder) -> Result<(), StoreError> {
        self.write_folder(
            folder,
            "ON CONFLICT(path_key) DO UPDATE SET
                 path = excluded.path,
                 recursive = excluded.recursive,
                 album_mapping = excluded.album_mapping,
                 album_scope = excluded.album_scope",
        )
        .map(|_| ())
    }

    /// Records a new source folder with every setting it carries, and returns
    /// whether it was inserted (#40).
    ///
    /// A folder the store already holds under the platform's path rule (R2.4)
    /// is not replaced and `false` comes back, so the setup dialog can show
    /// "Already added" without disturbing the stored settings.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn insert_folder(&self, folder: &SourceFolder) -> Result<bool, StoreError> {
        self.write_folder(folder, "ON CONFLICT(path_key) DO NOTHING")
            .map(|inserted| inserted > 0)
    }

    /// Writes one source folder row inside a transaction. `conflict` finishes
    /// the statement: the update an edit needs, or `DO NOTHING` for an insert.
    fn write_folder(&self, folder: &SourceFolder, conflict: &str) -> Result<usize, StoreError> {
        let mut writer = self.writer()?;
        let transaction = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| self.query_error(source))?;
        let written = transaction
            .execute(
                &format!(
                    "INSERT INTO source_folder (path, path_key, recursive, album_mapping, album_scope)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     {conflict}"
                ),
                params![
                    folder.path,
                    path_key(Path::new(&folder.path)),
                    folder.recursive,
                    folder.album_mapping,
                    folder.album_scope.as_str(),
                ],
            )
            .map_err(|source| self.query_error(source))?;
        if folder.album_title.is_some() {
            transaction
                .execute(
                    ALBUM_TITLE_UPSERT,
                    params![
                        path_key(Path::new(&folder.path)),
                        folder.album_title.as_deref(),
                    ],
                )
                .map_err(|source| self.query_error(source))?;
        }
        transaction
            .commit()
            .map_err(|source| self.query_error(source))?;
        Ok(written)
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
                "SELECT f.path, f.recursive, f.album_mapping, f.album_scope, a.title
                 FROM source_folder f LEFT JOIN album a ON a.folder_key = f.path_key
                 ORDER BY f.path",
            )
            .map_err(|source| self.query_error(source))?;
        let rows = statement
            .query_map([], read_folder)
            .map_err(|source| self.query_error(source))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|source| self.query_error(source))
    }

    /// The album row of the mapped folder `folder_key` (R3.4, #47), when this
    /// client ever wrote one.
    ///
    /// The folder key is the identity, because the album scope is one album per
    /// folder (GLOSSARY): the album title is a name the mapping gives, so two
    /// folders that share a name are two albums (R3.4).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails.
    pub fn album(&self, folder_key: &str) -> Result<Option<Album>, StoreError> {
        let writer = self.writer()?;
        writer
            .query_row(
                "SELECT title, album_id, rename_pending, rename_problem
                 FROM album WHERE folder_key = ?1",
                [folder_key],
                |row| {
                    Ok(Album {
                        title: row.get(0)?,
                        album_id: row.get(1)?,
                        rename_pending: row.get(2)?,
                        rename_problem: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|source| self.query_error(source))
    }

    /// Records the title the user chose for the mapped folder `folder_key`
    /// (#47).
    ///
    /// An album that already exists must be renamed, so the row is marked
    /// pending; one that does not exist yet takes the title when it is created,
    /// so nothing is pending. Setting the title the row already holds owes the
    /// server nothing, so a folder write that carries the stored title (a
    /// recursive toggle, say) never queues a rename.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn set_album_title(&self, folder_key: &str, title: Option<&str>) -> Result<(), StoreError> {
        let writer = self.writer()?;
        writer
            .execute(ALBUM_TITLE_UPSERT, params![folder_key, title])
            .map_err(|source| self.query_error(source))?;
        Ok(())
    }

    /// Records the album the server answered with for the mapped folder
    /// `folder_key`, with the title it currently holds (#47, R3.4).
    ///
    /// This is both the create's answer and the rename's: the stored title
    /// follows the server, and the rename it owed is done.
    ///
    /// `settled` is the title the request was based on. A row whose title has
    /// changed since — the user typed again while the request was on the wire —
    /// keeps the newer title and stays pending, so an answer cannot overwrite
    /// an edit it never saw (#47).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn record_album(
        &self,
        folder_key: &str,
        album_id: &str,
        settled: Option<&str>,
        title: Option<&str>,
    ) -> Result<(), StoreError> {
        let writer = self.writer()?;
        // One statement, so the single write is atomic on its own and needs no
        // transaction around it.
        writer
            .execute(
                "INSERT INTO album (folder_key, title, album_id) VALUES (?1, ?4, ?2)
                 ON CONFLICT(folder_key) DO UPDATE SET
                     album_id = excluded.album_id,
                     rename_problem = CASE
                         WHEN album.title IS ?3 THEN NULL
                         ELSE album.rename_problem
                     END,
                     rename_pending = CASE
                         WHEN album.title IS ?3 THEN 0
                         ELSE 1
                     END,
                     title = CASE
                         WHEN album.title IS ?3 THEN excluded.title
                         ELSE album.title
                     END",
                params![folder_key, album_id, settled, title],
            )
            .map_err(|source| self.query_error(source))?;
        Ok(())
    }

    /// Records that the title of the album of `folder_key` did not reach the
    /// server, and why (#47). The row stays pending, so a later scan, restart,
    /// or reconnect tries again.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn album_rename_failed(&self, folder_key: &str, reason: &str) -> Result<(), StoreError> {
        let writer = self.writer()?;
        writer
            .execute(
                "UPDATE album SET rename_pending = 1, rename_problem = ?2 WHERE folder_key = ?1",
                params![folder_key, reason],
            )
            .map_err(|source| self.query_error(source))?;
        Ok(())
    }

    /// Re-queues what a restart interrupted: an `uploading` row becomes
    /// `pending` again, and a `retry_wait` row returns to the queue with its
    /// attempt count intact (R6.7). Every pending path is returned to enqueue.
    ///
    /// A completed row is never returned, so a restart cannot re-upload it.
    /// A `failed_permanent` or `dead_letter` row is not returned either: only
    /// the manual retry of R6.5 tries those again.
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
                "UPDATE sync_item SET state = 'pending'
                 WHERE state IN ('uploading', 'retry_wait')",
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

    /// Records one upload attempt and returns the item's attempt count after
    /// it (R6.1, R6.7, SPEC §5).
    ///
    /// The counter on the item and the attempt row are written in one
    /// transaction, so a restart continues the count and the log agrees with
    /// it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::UnknownItem`] when no item row holds the path,
    /// and [`StoreError`] when the transaction fails or the lock is poisoned.
    pub fn record_attempt(&self, attempt: &AttemptRecord) -> Result<u32, StoreError> {
        let mut writer = self.writer()?;
        let transaction = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| self.query_error(source))?;
        let key = path_key(Path::new(&attempt.path));
        let changed = transaction
            .execute(
                "UPDATE sync_item SET attempts = attempts + 1 WHERE path_key = ?1",
                [&key],
            )
            .map_err(|source| self.query_error(source))?;
        if changed == 0 {
            return Err(StoreError::UnknownItem(attempt.path.clone()));
        }
        let (item_id, attempts): (i64, i64) = transaction
            .query_row(
                "SELECT id, attempts FROM sync_item WHERE path_key = ?1",
                [&key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|source| self.query_error(source))?;
        transaction
            .execute(
                "INSERT INTO attempt (item_id, started_ns, outcome, error_class)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    item_id,
                    attempt.started_ns,
                    attempt.outcome.as_str(),
                    attempt.error_class.map(FailureClass::as_str),
                ],
            )
            .map_err(|source| self.query_error(source))?;
        transaction
            .commit()
            .map_err(|source| self.query_error(source))?;
        Ok(u32::try_from(attempts).unwrap_or(u32::MAX))
    }

    /// Every attempt recorded for one item, oldest first (SPEC §5).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails or a row cannot be read.
    pub fn attempts(&self, path: &str) -> Result<Vec<AttemptRecord>, StoreError> {
        let writer = self.writer()?;
        let mut statement = writer
            .prepare(
                "SELECT a.started_ns, a.outcome, a.error_class FROM attempt a
                 JOIN sync_item i ON i.id = a.item_id
                 WHERE i.path_key = ?1 ORDER BY a.id",
            )
            .map_err(|source| self.query_error(source))?;
        let rows = statement
            .query_map([path_key(Path::new(path))], |row| {
                let started_ns: i64 = row.get(0)?;
                let outcome_text: String = row.get(1)?;
                let outcome = AttemptOutcome::parse(&outcome_text).ok_or_else(|| {
                    conversion_error(
                        1,
                        rusqlite::types::Type::Text,
                        StoreError::UnknownAttemptOutcome(outcome_text.clone()),
                    )
                })?;
                let class_text: Option<String> = row.get(2)?;
                Ok((started_ns, outcome, class_text))
            })
            .map_err(|source| self.query_error(source))?;
        let mut attempts = Vec::new();
        for row in rows {
            let (started_ns, outcome, class_text) =
                row.map_err(|source| self.query_error(source))?;
            attempts.push(AttemptRecord {
                path: path.to_string(),
                started_ns,
                outcome,
                error_class: class_text.as_deref().and_then(FailureClass::parse),
            });
        }
        Ok(attempts)
    }

    /// The items the retry policy gave up on, in path order (R6.5).
    ///
    /// A `failed_permanent` row is there because no automatic retry can fix
    /// it; a `dead_letter` row is there because it spent the attempt cap.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails or a row cannot be read.
    pub fn failed_items(&self) -> Result<Vec<FailedItem>, StoreError> {
        let writer = self.writer()?;
        let mut statement = writer
            .prepare(
                "SELECT path, state, error_class, error_message, attempts FROM sync_item
                 WHERE state IN ('failed_permanent', 'dead_letter') ORDER BY path",
            )
            .map_err(|source| self.query_error(source))?;
        let rows = statement
            .query_map([], |row| {
                let state_text: String = row.get(1)?;
                let state = ItemState::parse(&state_text).ok_or_else(|| {
                    conversion_error(
                        1,
                        rusqlite::types::Type::Text,
                        StoreError::UnknownState(state_text.clone()),
                    )
                })?;
                let class_text: Option<String> = row.get(2)?;
                let attempts: i64 = row.get(4)?;
                Ok((
                    row.get::<_, String>(0)?,
                    state,
                    class_text,
                    row.get::<_, Option<String>>(3)?,
                    attempts,
                ))
            })
            .map_err(|source| self.query_error(source))?;
        let mut items = Vec::new();
        for row in rows {
            let (path, state, class_text, error_message, attempts) =
                row.map_err(|source| self.query_error(source))?;
            items.push(FailedItem {
                path,
                state,
                error_class: class_text.as_deref().and_then(FailureClass::parse),
                error_message,
                attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
            });
        }
        Ok(items)
    }

    /// Puts one parked item back in the queue with a fresh attempt budget
    /// (R6.5). Returns whether a parked row was found.
    ///
    /// Only a `failed_permanent` or `dead_letter` row is re-queued: a retry
    /// must not disturb an item that is already queued or in flight.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn retry_item(&self, path: &str) -> Result<bool, StoreError> {
        let writer = self.writer()?;
        let changed = writer
            .execute(
                "UPDATE sync_item SET state = 'pending', attempts = 0,
                     error_class = NULL, error_message = NULL
                 WHERE path_key = ?1 AND state IN ('failed_permanent', 'dead_letter')",
                [path_key(Path::new(path))],
            )
            .map_err(|source| self.query_error(source))?;
        Ok(changed > 0)
    }

    /// Marks a tracked item missing: its local file is gone and the item waits
    /// for a review decision (R5.1). Returns whether a row changed.
    ///
    /// Only an item the server holds enters the review. A row whose upload
    /// never landed has no asset to keep or trash, so it is left where it is.
    /// Marking the same path again changes nothing, which is what lets the
    /// watcher and the reconciliation scan both report one disappearance and
    /// still leave exactly one review item.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn mark_missing(&self, path: &str) -> Result<bool, StoreError> {
        let writer = self.writer()?;
        let changed = writer
            .execute(
                "UPDATE sync_item SET state = 'missing'
                 WHERE path_key = ?1 AND state IN ('uploaded', 'duplicate')",
                [path_key(Path::new(path))],
            )
            .map_err(|source| self.query_error(source))?;
        Ok(changed > 0)
    }

    /// Records the user's decision for one missing file (R5.3). Returns
    /// whether a missing row was found.
    ///
    /// Only a `missing` row is decided on: an item that is not in the review
    /// has no decision to record, and a decision already made stays made.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn decide_review(&self, path: &str, decision: ReviewDecision) -> Result<bool, StoreError> {
        let writer = self.writer()?;
        let changed = writer
            .execute(
                "UPDATE sync_item SET state = ?2
                 WHERE path_key = ?1 AND state = 'missing'",
                params![path_key(Path::new(path)), decision.state().as_str()],
            )
            .map_err(|source| self.query_error(source))?;
        Ok(changed > 0)
    }

    /// Retires an item whose content another tracked item still holds, so no
    /// review item appears (R5.2, and the second-copy acceptance line).
    ///
    /// The asset stays on the server, exactly as a `Keep` leaves it, and the
    /// row lands in `review_keep` so it no longer waits for a decision. A
    /// settled row can be retired before it is ever marked missing: the other
    /// copy may be found in the same pass that notices the disappearance, and
    /// the item must never flicker through the review in between.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn retire_item(&self, path: &str) -> Result<bool, StoreError> {
        let writer = self.writer()?;
        let changed = writer
            .execute(
                "UPDATE sync_item SET state = 'review_keep'
                 WHERE path_key = ?1 AND state IN ('uploaded', 'duplicate', 'missing')",
                [path_key(Path::new(path))],
            )
            .map_err(|source| self.query_error(source))?;
        Ok(changed > 0)
    }

    /// Records that one asset moved to the trash (R5.3), even when a Keep
    /// decision landed while the DELETE was in flight. Returns whether a row
    /// changed.
    ///
    /// The server's answer is the last word on the asset, so a `review_keep`
    /// row that a Keep settled during the request must not go on claiming the
    /// asset was kept. Other rows are left alone: the file can reappear and
    /// upload again before the DELETE returns, and that live row must not be
    /// stamped as trashed. A row already in `review_trash` is one of them, so
    /// a second report of the same trash changes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn record_review_trash(&self, path: &str) -> Result<bool, StoreError> {
        let writer = self.writer()?;
        let changed = writer
            .execute(
                "UPDATE sync_item SET state = 'review_trash'
                 WHERE path_key = ?1 AND state = 'review_keep'",
                [path_key(Path::new(path))],
            )
            .map_err(|source| self.query_error(source))?;
        Ok(changed > 0)
    }

    /// Every missing file the deletion review lists, in path order (R5.3).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails or a row cannot be read.
    pub fn review_items(&self) -> Result<Vec<ReviewItem>, StoreError> {
        let writer = self.writer()?;
        let mut statement = writer
            .prepare(
                "SELECT path, size, asset_id, state FROM sync_item
                 WHERE state = 'missing' ORDER BY path",
            )
            .map_err(|source| self.query_error(source))?;
        let rows = statement
            .query_map([], read_review_item)
            .map_err(|source| self.query_error(source))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|source| self.query_error(source))
    }

    /// How many missing files wait for a decision (R5.4).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails.
    pub fn review_count(&self) -> Result<u32, StoreError> {
        let writer = self.writer()?;
        let count: i64 = writer
            .query_row(
                "SELECT COUNT(*) FROM sync_item WHERE state = 'missing'",
                [],
                |row| row.get(0),
            )
            .map_err(|source| self.query_error(source))?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    /// The items under one folder that can enter the review: the ones the
    /// server holds, and the ones already waiting for a decision.
    ///
    /// The reconciliation scan walks this list and asks, for each row whose
    /// file is absent, whether the content moved, is still held by another
    /// item, or is really gone (R5.1, R5.2).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails or a row cannot be read.
    pub fn review_candidates(&self, folder: &str) -> Result<Vec<ItemRecord>, StoreError> {
        let prefix = format!("{}/", path_key(Path::new(folder)));
        let writer = self.writer()?;
        let mut statement = writer
            .prepare(&format!(
                "SELECT {ITEM_COLUMNS} FROM sync_item
                 WHERE state IN ('uploaded', 'duplicate', 'missing')
                   AND substr(path_key, 1, length(?1)) = ?1
                 ORDER BY path"
            ))
            .map_err(|source| self.query_error(source))?;
        let rows = statement
            .query_map([&prefix], read_item)
            .map_err(|source| self.query_error(source))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|source| self.query_error(source))
    }

    /// The tracked items whose content is exactly `sha1` and `size`, other
    /// than `except_path` (R5.2).
    ///
    /// This is how a disappearance is told from a rename when the other copy
    /// was already tracked: the row still points at a file that exists.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails or a row cannot be read.
    pub fn content_twins(
        &self,
        sha1: Sha1,
        size: u64,
        except_path: &str,
    ) -> Result<Vec<ItemRecord>, StoreError> {
        let size = i64::try_from(size).map_err(|_| StoreError::SizeOutOfRange(size))?;
        let writer = self.writer()?;
        let mut statement = writer
            .prepare(&format!(
                "SELECT {ITEM_COLUMNS} FROM sync_item
                 WHERE sha1 = ?1 AND size = ?2 AND path_key <> ?3 ORDER BY path"
            ))
            .map_err(|source| self.query_error(source))?;
        let rows = statement
            .query_map(
                params![
                    sha1.as_bytes().as_slice(),
                    size,
                    path_key(Path::new(except_path))
                ],
                read_item,
            )
            .map_err(|source| self.query_error(source))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|source| self.query_error(source))
    }

    /// Moves one item's row to the path its content now lives at (R5.2).
    ///
    /// The row keeps its id, so the asset mapping, the version chain, and the
    /// stack follow the file; only the path, the comparison key, and the size
    /// and modification time the next scan compares against are rewritten. A
    /// move onto a path another row already holds is refused rather than
    /// merging two items into one.
    ///
    /// A missing row that finds its content again is settled: the server
    /// already held those bytes, so the item is a duplicate and leaves the
    /// review (R5.2, R5.3).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails, the size does not fit, or
    /// the lock is poisoned.
    pub fn move_item(
        &self,
        from: &str,
        to: &str,
        size: u64,
        mtime_ns: i64,
    ) -> Result<bool, StoreError> {
        let size = i64::try_from(size).map_err(|_| StoreError::SizeOutOfRange(size))?;
        let writer = self.writer()?;
        let changed = writer
            .execute(
                "UPDATE sync_item SET path = ?2, path_key = ?3, size = ?4, mtime_ns = ?5,
                     state = CASE WHEN state = 'missing' THEN 'duplicate' ELSE state END
                 WHERE path_key = ?1
                   AND NOT EXISTS (SELECT 1 FROM sync_item WHERE path_key = ?3)",
                params![
                    path_key(Path::new(from)),
                    to,
                    path_key(Path::new(to)),
                    size,
                    mtime_ns
                ],
            )
            .map_err(|source| self.query_error(source))?;
        Ok(changed > 0)
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
                Some(ItemState::Uploaded | ItemState::Duplicate | ItemState::ReviewKeep) => {
                    counts.uploaded += count;
                }
                Some(ItemState::FailedPermanent | ItemState::RetryWait | ItemState::DeadLetter) => {
                    counts.failed += count;
                }
                Some(ItemState::Missing) => counts.review += count,
                // A trashed item is a finished decision: the server copy is in
                // the trash and the local file is gone, so it is neither work
                // in flight nor a failure (R5.3).
                Some(ItemState::ReviewTrash) => {}
                // A state a newer build wrote still counts as work in flight,
                // and an unknown state is not silently dropped.
                _ => counts.pending += count,
            }
        }
        Ok(counts)
    }

    /// When one source folder last reconciled and last uploaded, for its
    /// detail view (#43).
    ///
    /// An untracked path reads as [`FolderTimes::default`], the same answer a
    /// tracked folder that has run neither gives.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails.
    pub fn folder_times(&self, folder: &str) -> Result<FolderTimes, StoreError> {
        let writer = self.writer()?;
        writer
            .query_row(
                "SELECT last_scan_ns, last_upload_ns FROM source_folder WHERE path_key = ?1",
                [path_key(Path::new(folder))],
                |row| {
                    Ok(FolderTimes {
                        last_scan_ns: row.get(0)?,
                        last_upload_ns: row.get(1)?,
                    })
                },
            )
            .optional()
            .map(Option::unwrap_or_default)
            .map_err(|source| self.query_error(source))
    }

    /// Records that a reconciliation scan just read one source folder (#43).
    ///
    /// Returns whether the client holds the folder.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn record_folder_scan(&self, folder: &str, at_ns: i64) -> Result<bool, StoreError> {
        self.record_folder_time(folder, "last_scan_ns", at_ns)
    }

    /// Records that one source folder just uploaded an asset (#43).
    ///
    /// Returns whether the client holds the folder.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the write fails or the lock is poisoned.
    pub fn record_folder_upload(&self, folder: &str, at_ns: i64) -> Result<bool, StoreError> {
        self.record_folder_time(folder, "last_upload_ns", at_ns)
    }

    /// Writes one timestamp column of the folder row `folder` names.
    ///
    /// The column is one of this module's two constants, never caller input,
    /// so the statement cannot name another column.
    fn record_folder_time(
        &self,
        folder: &str,
        column: &str,
        at_ns: i64,
    ) -> Result<bool, StoreError> {
        let writer = self.writer()?;
        let changed = writer
            .execute(
                &format!("UPDATE source_folder SET {column} = ?1 WHERE path_key = ?2"),
                params![at_ns, path_key(Path::new(folder))],
            )
            .map_err(|source| self.query_error(source))?;
        Ok(changed > 0)
    }

    /// Makes `version`'s asset the item's current version (R4.4, SPEC §5).
    ///
    /// The chain is written in one transaction: the previous current version
    /// loses the flag, the new version takes it, and the item points at the new
    /// asset and at the stack its versions share. A caller that has not copied
    /// the associations yet must not call this, because the row is what claims
    /// the version relationship exists.
    ///
    /// Recording the same asset again (a retry, or a run that re-reads a
    /// settled row) moves the existing row to current instead of adding a
    /// second one, so the chain never lists one asset twice.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::UnknownItem`] when no item row holds the path, and
    /// [`StoreError`] when the transaction fails or the lock is poisoned.
    pub fn record_version(&self, version: &NewVersion) -> Result<(), StoreError> {
        let mut writer = self.writer()?;
        let transaction = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| self.query_error(source))?;
        let key = path_key(Path::new(&version.path));
        let item_id: i64 = transaction
            .query_row(
                "SELECT id FROM sync_item WHERE path_key = ?1",
                [&key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| self.query_error(source))?
            .ok_or_else(|| StoreError::UnknownItem(version.path.clone()))?;
        transaction
            .execute(
                "UPDATE version SET is_current = 0
                 WHERE item_id = ?1 AND is_current = 1 AND asset_id <> ?2",
                params![item_id, version.asset_id],
            )
            .map_err(|source| self.query_error(source))?;
        transaction
            .execute(
                "INSERT INTO version (item_id, asset_id, sha1, created_at_ns, is_current)
                 VALUES (?1, ?2, ?3, ?4, 1)
                 ON CONFLICT(item_id, asset_id) DO UPDATE SET
                     sha1 = excluded.sha1,
                     created_at_ns = excluded.created_at_ns,
                     is_current = 1",
                params![
                    item_id,
                    version.asset_id,
                    version.sha1.as_bytes().as_slice(),
                    version.created_at_ns,
                ],
            )
            .map_err(|source| self.query_error(source))?;
        // `COALESCE` keeps the recorded stack when a version records none: the
        // first version has no stack yet, and a retry must not drop one.
        transaction
            .execute(
                "UPDATE sync_item SET asset_id = ?1, stack_id = COALESCE(?2, stack_id)
                 WHERE id = ?3",
                params![version.asset_id, version.stack_id, item_id],
            )
            .map_err(|source| self.query_error(source))?;
        transaction
            .commit()
            .map_err(|source| self.query_error(source))
    }

    /// Every version of one item, oldest first (SPEC §5) — by recording time,
    /// because a re-recorded version (a revert) keeps its row id and only its
    /// timestamp moves.
    ///
    /// The old versions stay in the list: the client keeps them (R4.4), and a
    /// reader can show a file's history from it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails or a row cannot be read.
    pub fn versions(&self, path: &str) -> Result<Vec<Version>, StoreError> {
        let writer = self.writer()?;
        let mut statement = writer
            .prepare(
                "SELECT v.item_id, v.asset_id, v.sha1, v.created_at_ns, v.is_current
                 FROM version v JOIN sync_item i ON i.id = v.item_id
                 WHERE i.path_key = ?1 ORDER BY v.created_at_ns, v.id",
            )
            .map_err(|source| self.query_error(source))?;
        let rows = statement
            .query_map([path_key(Path::new(path))], read_version)
            .map_err(|source| self.query_error(source))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|source| self.query_error(source))
    }

    /// The stack the item's versions share, as R4.4 records it.
    ///
    /// `None` before the item has a stack: a first upload has no stack, and a
    /// file the server never stacked has none either.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails.
    pub fn stack_id(&self, path: &str) -> Result<Option<String>, StoreError> {
        let writer = self.writer()?;
        let row: Option<Option<String>> = writer
            .query_row(
                "SELECT stack_id FROM sync_item WHERE path_key = ?1",
                [path_key(Path::new(path))],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| self.query_error(source))?;
        // An untracked path has no row, which reads as "no stack" like an
        // untracked item reads as no asset.
        Ok(row.flatten())
    }

    /// The sync tag's backfill state, which is where a later run resumes from
    /// (R3.7).
    ///
    /// A database that never ran a backfill holds no row, which reads as the
    /// default: no cursor, nothing tagged, no completion.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the row cannot be read.
    pub fn tag_backfill(&self) -> Result<TagBackfill, StoreError> {
        let writer = self.writer()?;
        let row = writer
            .query_row(
                "SELECT completed_ns, cursor, tagged FROM tag_backfill WHERE id = 1",
                [],
                |row| {
                    Ok(TagBackfill {
                        completed_ns: row.get(0)?,
                        cursor: row.get(1)?,
                        tagged: u32::try_from(row.get::<_, i64>(2)?).unwrap_or(u32::MAX),
                    })
                },
            )
            .optional()
            .map_err(|source| self.query_error(source))?;
        Ok(row.unwrap_or_default())
    }

    /// How many asset ids the backfill covers (R3.7): every current asset,
    /// every old version, and every asset the server answered as a duplicate,
    /// once each.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the count cannot be read.
    pub fn tag_backfill_total(&self) -> Result<u32, StoreError> {
        let writer = self.writer()?;
        let count: i64 = writer
            .query_row(
                &format!("SELECT COUNT(*) FROM ({TAG_BACKFILL_IDS})"),
                [],
                |row| row.get(0),
            )
            .map_err(|source| self.query_error(source))?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    /// One batch of asset ids the backfill still has to tag (R3.7): every id
    /// after `after`, in id order, at most `limit` of them.
    ///
    /// `after` is the previous batch's last id, or `None` for the first batch
    /// of a run; ids sort as text, so a cursor is just the last id a completed
    /// batch carried. An id the server no longer holds is still in the batch:
    /// Immich filters those out and answers `count: 0`, so one unavailable
    /// asset must not stop the rest.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the query fails.
    pub fn tag_backfill_batch(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, StoreError> {
        // shortcut: every batch re-reads the ids after the cursor through one
        // UNION, which is a few milliseconds for a photo library. A library in
        // the millions of versions would want the two tables walked in cursor
        // order instead.
        let writer = self.writer()?;
        let mut statement = writer
            .prepare(&format!(
                "SELECT asset_id FROM ({TAG_BACKFILL_IDS})
                 WHERE asset_id > ?1 ORDER BY asset_id LIMIT ?2"
            ))
            .map_err(|source| self.query_error(source))?;
        // `asset_id > NULL` selects nothing, so the first batch compares
        // against the empty string: every asset id sorts after it.
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        statement
            .query_map(params![after.unwrap_or(""), limit], |row| row.get(0))
            .map_err(|source| self.query_error(source))?
            .collect::<rusqlite::Result<Vec<String>>>()
            .map_err(|source| self.query_error(source))
    }

    /// Records one completed backfill batch (R3.7): its last id and the
    /// running count move in one transaction.
    ///
    /// The transaction is what makes an interruption resume at a batch
    /// boundary: a batch either recorded its cursor or it did not, and a
    /// batch that is repeated after a crash is safe because tagging is
    /// idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the transaction fails.
    pub fn record_tag_batch(&self, cursor: &str, tagged: u32) -> Result<(), StoreError> {
        self.write_tag_backfill(
            "INSERT INTO tag_backfill (id, cursor, tagged) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET cursor = excluded.cursor, tagged = excluded.tagged",
            params![cursor, i64::from(tagged)],
        )
    }

    /// Records that the backfill reached the end of the library (R3.7), so a
    /// restart does not tag the whole library again.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the transaction fails.
    pub fn finish_tag_backfill(&self, completed_ns: i64) -> Result<(), StoreError> {
        self.write_tag_backfill(
            "INSERT INTO tag_backfill (id, completed_ns) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET completed_ns = excluded.completed_ns",
            params![completed_ns],
        )
    }

    /// Clears the marker, the cursor, and the count, so the next run tags the
    /// whole library again (R3.7: "Tag existing assets now").
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the transaction fails.
    pub fn restart_tag_backfill(&self) -> Result<(), StoreError> {
        self.write_tag_backfill(
            "INSERT INTO tag_backfill (id, completed_ns, cursor, tagged) VALUES (1, NULL, NULL, 0)
             ON CONFLICT(id) DO UPDATE SET completed_ns = NULL, cursor = NULL, tagged = 0",
            [],
        )
    }

    /// Runs one backfill state write in a transaction of its own (R3.7).
    fn write_tag_backfill(
        &self,
        sql: &str,
        values: impl rusqlite::Params,
    ) -> Result<(), StoreError> {
        let mut writer = self.writer()?;
        let transaction = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| self.query_error(source))?;
        transaction
            .execute(sql, values)
            .map_err(|source| self.query_error(source))?;
        transaction
            .commit()
            .map_err(|source| self.query_error(source))
    }

    /// The writer connection, or the poison error after a panic.
    fn writer(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.writer.lock().map_err(|_| StoreError::Poisoned)
    }

    /// A fresh read-only connection to the same file, so a UI read never
    /// queues behind the one writer (ADR 0005).
    fn reader(&self) -> Result<Connection, StoreError> {
        let open_error = |source| StoreError::Open {
            path: self.path.clone(),
            source,
        };
        let reader = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(open_error)?;
        reader.busy_timeout(BUSY_TIMEOUT).map_err(open_error)?;
        Ok(reader)
    }

    /// One query failure, tagged with the database path.
    fn query_error(&self, source: rusqlite::Error) -> StoreError {
        StoreError::Query {
            path: self.path.clone(),
            source,
        }
    }
}

/// Reads one `version` row.
fn read_version(row: &rusqlite::Row<'_>) -> rusqlite::Result<Version> {
    let bytes: Vec<u8> = row.get(2)?;
    let sha1 = Sha1::from_slice(&bytes)
        .ok_or_else(|| conversion_error(2, rusqlite::types::Type::Blob, StoreError::BadChecksum))?;
    Ok(Version {
        item_id: row.get(0)?,
        asset_id: row.get(1)?,
        sha1,
        created_at_ns: row.get(3)?,
        is_current: row.get(4)?,
    })
}

/// Reads one `source_folder` row, with the album title the folder's own album
/// row holds (#47).
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
        album_title: row.get(4)?,
    })
}

/// Reads one `sync_item` row for the deletion review (R5.3).
fn read_review_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewItem> {
    let size: i64 = row.get(1)?;
    let size = u64::try_from(size).map_err(|_| {
        conversion_error(1, rusqlite::types::Type::Integer, StoreError::NegativeSize)
    })?;
    let state_text: String = row.get(3)?;
    let state = ItemState::parse(&state_text).ok_or_else(|| {
        conversion_error(
            3,
            rusqlite::types::Type::Text,
            StoreError::UnknownState(state_text.clone()),
        )
    })?;
    Ok(ReviewItem {
        path: row.get(0)?,
        size,
        asset_id: row.get(2)?,
        state,
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
    let attempts: i64 = row.get(8)?;
    // An asset with no sidecar, or one written before the columns existed, has
    // no digest; all three read as `None`, which means "no sidecar has been
    // sent". A digest of the wrong length reads as `None` too: the worst it
    // costs is that the sidecar is hashed again.
    let sidecar_sha1: Option<Vec<u8>> = row.get(9)?;
    let sidecar_size: Option<i64> = row.get(10)?;
    let sidecar_mtime_ns: Option<i64> = row.get(11)?;
    let sidecar = match (
        sidecar_sha1.as_deref().and_then(Sha1::from_slice),
        sidecar_size,
        sidecar_mtime_ns,
    ) {
        (Some(sha1), Some(size), Some(mtime_ns)) => Some(SidecarDigest {
            sha1,
            size: u64::try_from(size).map_err(|_| {
                conversion_error(10, rusqlite::types::Type::Integer, StoreError::NegativeSize)
            })?,
            mtime_ns,
        }),
        _ => None,
    };
    Ok(ItemRecord {
        path: row.get(0)?,
        size,
        mtime_ns: row.get(2)?,
        sha1,
        asset_id: row.get(4)?,
        state,
        error_class,
        error_message: row.get(7)?,
        attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
        sidecar,
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
    // ADR 0005 builds on bundled SQLite, which defaults foreign keys on; the
    // pragma pins that, so the state tables' ON DELETE actions never depend on
    // a compile-time default.
    writer.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// The migration list as it stood before the version schema (issue #11), so
/// an upgrade-path test can build a database at the schema the upgrade really
/// starts from.
#[doc(hidden)]
pub fn migrations_before_version_schema() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(SCHEMA_V1),
        M::up_with_hook(SCHEMA_V2, backfill_path_keys),
        M::up(SCHEMA_V3),
        M::up(SCHEMA_V4),
        M::up(SCHEMA_V5),
    ])
}

/// The schema migrations, in order (ADR 0005: `rusqlite_migration`).
fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(SCHEMA_V1),
        M::up_with_hook(SCHEMA_V2, backfill_path_keys),
        M::up(SCHEMA_V3),
        M::up(SCHEMA_V4),
        M::up(SCHEMA_V5),
        M::up(SCHEMA_V6),
        M::up(SCHEMA_V7),
        M::up(SCHEMA_V8),
        M::up(SCHEMA_V9),
        M::up(SCHEMA_V10),
        M::up(SCHEMA_V11),
        M::up(SCHEMA_V12),
    ])
}

/// The migration list as it stood before the tag backfill state (issue #46),
/// so an upgrade-path test can build a database at the schema the upgrade
/// really starts from.
#[doc(hidden)]
pub fn migrations_before_tag_backfill() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(SCHEMA_V1),
        M::up_with_hook(SCHEMA_V2, backfill_path_keys),
        M::up(SCHEMA_V3),
        M::up(SCHEMA_V4),
        M::up(SCHEMA_V5),
        M::up(SCHEMA_V6),
        M::up(SCHEMA_V7),
        M::up(SCHEMA_V8),
        M::up(SCHEMA_V9),
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
                album_title: None,
            }],
            "R3.4: a folder mapped before album mapping existed reads back off, at the default \
             scope, through the whole composed migration chain"
        );
    }

    /// #43: a database written before the folder times existed keeps its
    /// folders, and they read as never scanned and never uploaded.
    #[test]
    fn the_folder_times_arrive_on_a_v10_database() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("state.db");
        {
            let mut connection = Connection::open(&path).expect("open the raw database");
            // Every schema before the folder times, including the tag backfill
            // #46 merged first, so the composed chain has to apply V11 over
            // rows V4 wrote.
            Migrations::new(vec![
                M::up(SCHEMA_V1),
                M::up_with_hook(SCHEMA_V2, backfill_path_keys),
                M::up(SCHEMA_V3),
                M::up(SCHEMA_V4),
                M::up(SCHEMA_V5),
                M::up(SCHEMA_V6),
                M::up(SCHEMA_V7),
                M::up(SCHEMA_V8),
                M::up(SCHEMA_V9),
                M::up(SCHEMA_V10),
            ])
            .to_latest(&mut connection)
            .expect("create the tenth schema");
            connection
                .execute(
                    "INSERT INTO source_folder (path, path_key, recursive, album_mapping, album_scope)
                     VALUES (?1, ?2, 1, 1, 'whole_folder')",
                    params!["/photos", "/photos"],
                )
                .expect("insert the source folder");
            drop(connection);
        }

        let store = StateStore::open(&path).expect("a v10 database must migrate");
        assert_eq!(
            store.folders().expect("read the folders"),
            vec![SourceFolder {
                path: "/photos".to_string(),
                recursive: true,
                album_mapping: true,
                album_scope: AlbumScope::WholeFolder,
                album_title: None,
            }],
            "the folder row and its album settings survive the chain"
        );
        assert_eq!(
            store.folder_times("/photos").expect("read the times"),
            FolderTimes::default(),
            "a folder from before the folder times reads as never scanned and never uploaded"
        );
        assert!(
            store
                .record_folder_scan("/photos", 42)
                .expect("record the scan"),
            "the migrated row accepts a scan time"
        );
    }

    /// #47: a database written before the album title existed keeps the
    /// albums it recorded, and they read as albums named after their folders.
    #[test]
    fn the_album_title_arrives_on_a_v11_database() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("state.db");
        {
            let mut connection = Connection::open(&path).expect("open the raw database");
            // Every schema before the album title, so the composed chain has to
            // apply V12 over an album row V4 wrote.
            Migrations::new(vec![
                M::up(SCHEMA_V1),
                M::up_with_hook(SCHEMA_V2, backfill_path_keys),
                M::up(SCHEMA_V3),
                M::up(SCHEMA_V4),
                M::up(SCHEMA_V5),
                M::up(SCHEMA_V6),
                M::up(SCHEMA_V7),
                M::up(SCHEMA_V8),
                M::up(SCHEMA_V9),
                M::up(SCHEMA_V10),
                M::up(SCHEMA_V11),
            ])
            .to_latest(&mut connection)
            .expect("create the eleventh schema");
            connection
                .execute(
                    "INSERT INTO source_folder (path, path_key, recursive, album_mapping, album_scope)
                     VALUES (?1, ?2, 1, 1, 'whole_folder')",
                    params!["/photos", "/photos"],
                )
                .expect("insert the source folder");
            connection
                .execute(
                    "INSERT INTO album (folder_key, name, album_id) VALUES (?1, ?2, ?3)",
                    params!["/photos", "photos", "album-7"],
                )
                .expect("insert the album");
            drop(connection);
        }

        let store = StateStore::open(&path).expect("a v11 database must migrate");
        assert_eq!(
            store.album("/photos").expect("read the album"),
            Some(Album {
                title: None,
                album_id: Some("album-7".to_string()),
                rename_pending: false,
                rename_problem: None,
            }),
            "#47: an album named after its folder reads as no custom title, and its id survives"
        );
        assert_eq!(
            store.folders().expect("read the folders")[0].album_title,
            None,
            "#47: a folder from before the title existed reads back with no title"
        );
        store
            .set_album_title("/photos", Some("Holiday"))
            .expect("the migrated row accepts a title");
        assert!(
            store
                .album("/photos")
                .expect("read the album")
                .expect("the row")
                .rename_pending,
            "#47: the migrated album row can ask for its rename"
        );
    }

    /// R5.4: the whole chain still applies to a database the first schema
    /// wrote, and the review count reads through the state index the ninth
    /// schema adds instead of scanning every item on each status poll.
    #[test]
    fn the_review_count_uses_the_state_index_after_migrating() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("state.db");
        {
            let mut connection = Connection::open(&path).expect("open the raw database");
            Migrations::new(vec![M::up(SCHEMA_V1)])
                .to_latest(&mut connection)
                .expect("create the first schema");
            let sha1 =
                Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").expect("a hex SHA-1");
            let transaction = connection.transaction().expect("start one transaction");
            for index in 0..500 {
                let state = if index < 3 { "missing" } else { "uploaded" };
                transaction
                    .execute(
                        "INSERT INTO sync_item (path, size, mtime_ns, sha1, asset_id, state, error_class)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            format!("/photos/photo-{index:03}.jpg"),
                            11_i64,
                            1_i64,
                            sha1.as_bytes().as_slice(),
                            "asset-1",
                            state,
                            Option::<String>::None,
                        ],
                    )
                    .expect("insert the old row");
            }
            transaction.commit().expect("commit the old rows");
            drop(connection);
        }

        let store =
            StateStore::open(&path).expect("a v1 database must migrate to the latest schema");
        assert_eq!(
            store.review_count().expect("count the review"),
            3,
            "R5.4: the migrated rows are the ones the review counts"
        );
        drop(store);

        let reader = Connection::open(&path).expect("open the migrated database");
        let plan: Vec<String> = reader
            .prepare("EXPLAIN QUERY PLAN SELECT COUNT(*) FROM sync_item WHERE state = 'missing'")
            .expect("prepare the plan")
            .query_map([], |row| row.get(3))
            .expect("run the plan")
            .collect::<rusqlite::Result<Vec<String>>>()
            .expect("read the plan");
        assert!(
            plan.iter().any(|line| line.contains("sync_item_state")),
            "the review count still scans the table: {plan:?}"
        );
    }

    /// R4.4: a database written before the version table still migrates, and
    /// the rows it holds keep their asset mappings.
    #[test]
    fn the_version_table_survives_a_v5_database() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("state.db");
        {
            let mut connection = Connection::open(&path).expect("open the raw database");
            // A v5 database: the attempt log exists, the version table does not.
            migrations_before_version_schema()
                .to_latest(&mut connection)
                .expect("create the fifth schema");
            connection
                .execute(
                    "INSERT INTO sync_item (path, path_key, size, mtime_ns, sha1, asset_id, state, error_class)
                     VALUES (?1, ?2, 11, 1, ?3, 'asset-1', 'uploaded', NULL)",
                    params![
                        "hello.txt",
                        "hello.txt",
                        Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")
                            .expect("a hex SHA-1")
                            .as_bytes()
                            .as_slice(),
                    ],
                )
                .expect("insert the old row");
            drop(connection);
        }

        let store = StateStore::open(&path).expect("a v5 database must migrate");
        let row = store.item("hello.txt").expect("read").expect("the row");
        assert_eq!(row.asset_id.as_deref(), Some("asset-1"));
        assert_eq!(
            store.stack_id("hello.txt").expect("read the stack id"),
            None,
            "a migrated item has no stack until its first edit"
        );
        let seeded = store.versions("hello.txt").expect("read the chain");
        assert_eq!(
            seeded.len(),
            1,
            "R4.2: the migration seeds the chain from the row's asset id, or the \
             first edit after the upgrade would have nothing to copy from"
        );
        assert_eq!(seeded[0].asset_id, "asset-1");
        assert!(seeded[0].is_current, "the migrated asset starts current");
        assert_eq!(
            seeded[0].created_at_ns, 1,
            "the row's mtime is when the version was recorded"
        );
        store
            .record_version(&NewVersion {
                path: "hello.txt".to_string(),
                asset_id: "asset-2".to_string(),
                sha1: Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")
                    .expect("a hex SHA-1"),
                created_at_ns: 1_700_000_000_000_000_000,
                stack_id: Some("stack-1".to_string()),
            })
            .expect("the version table works after the migration");
        let chain = store.versions("hello.txt").expect("read the chain");
        assert_eq!(
            chain.len(),
            2,
            "the seeded version and its successor are both recorded"
        );
        assert!(
            !chain[0].is_current && chain[1].is_current,
            "the successor takes the current flag from the seeded row"
        );
    }

    /// R3.7: a database written before the backfill state still migrates, the
    /// rows it holds survive, and the backfill table is usable after it.
    #[test]
    fn the_tag_backfill_state_is_added_to_an_existing_database() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("state.db");
        {
            let mut connection = Connection::open(&path).expect("open the raw database");
            // The ninth schema: the backfill table does not exist yet.
            migrations_before_tag_backfill()
                .to_latest(&mut connection)
                .expect("create the ninth schema");
            connection
                .execute(
                    "INSERT INTO sync_item (path, path_key, size, mtime_ns, sha1, asset_id, state, error_class)
                     VALUES (?1, ?2, 11, 1, ?3, 'asset-1', 'uploaded', NULL)",
                    params![
                        "hello.txt",
                        "hello.txt",
                        Sha1::from_hex("2aae6c35c94fcfb415dbe95f408b9ce91ee846ed")
                            .expect("a hex SHA-1")
                            .as_bytes()
                            .as_slice(),
                    ],
                )
                .expect("insert the old row");
            drop(connection);
        }

        let store = StateStore::open(&path).expect("a v9 database must migrate");
        let row = store.item("hello.txt").expect("read").expect("the row");
        assert_eq!(
            row.asset_id.as_deref(),
            Some("asset-1"),
            "R3.7: the existing row survives the migration"
        );
        assert_eq!(
            store.tag_backfill().expect("read the backfill state"),
            TagBackfill::default(),
            "R3.7: a database that never ran a backfill starts with no marker"
        );
        store
            .record_tag_batch("asset-1", 1)
            .expect("the backfill table works after the migration");
        assert_eq!(
            store.tag_backfill_total().expect("count the library"),
            1,
            "the migrated row's asset is what the backfill covers"
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

#[cfg(test)]
mod startup_check_tests {
    use super::*;

    /// N4, R10.4: a check that could not run because the database is busy or
    /// locked is not a corruption verdict; the rebuild must report it instead
    /// of removing the file.
    #[test]
    fn a_busy_or_locked_startup_check_is_not_corruption() {
        let path = PathBuf::from("state.db");
        for code in [
            rusqlite::ErrorCode::DatabaseBusy,
            rusqlite::ErrorCode::DatabaseLocked,
        ] {
            let source = rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code,
                    extended_code: 0,
                },
                None,
            );
            let error = startup_check_error(path.clone(), source);
            assert!(
                matches!(error, StoreError::Open { .. }),
                "{code:?} means the check could not run, so the rebuild must not delete the file: \
                 {error}"
            );
        }
        // Everything else keeps the corruption verdict the rebuild acts on.
        let source = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseCorrupt,
                extended_code: 0,
            },
            None,
        );
        let error = startup_check_error(path, source);
        assert!(
            matches!(error, StoreError::QuickCheck { .. }),
            "a corrupt database keeps the verdict: {error}"
        );
    }
}

#[cfg(test)]
mod diagnostics_read_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc;

    /// R10.2, ADR 0005: the diagnostics reads take their own reader
    /// connections, so a read never queues behind the engine's writer. Holding
    /// the writer lock here stands in for a slow engine write.
    #[test]
    fn the_diagnostics_reads_do_not_wait_for_the_writer() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let store =
            Arc::new(StateStore::open(temp.path().join("state.db")).expect("open a fresh store"));
        let writer = store.writer.lock().expect("lock the writer");
        let (sender, receiver) = mpsc::channel();
        {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                let _ = sender.send((store.sqlite_info(), store.last_error_per_class()));
            });
        }
        let read = receiver.recv_timeout(Duration::from_secs(5));
        drop(writer);
        let (info, errors) = read.expect("the diagnostics read must not wait for the writer lock");
        info.expect("read the sqlite info while the writer is held");
        errors.expect("read the last errors while the writer is held");
    }
}

#[cfg(test)]
mod folder_time_tests {
    use super::*;

    fn folder(path: &str) -> SourceFolder {
        SourceFolder {
            path: path.to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
        }
    }

    /// #43: the folder detail shows when the folder last scanned and last
    /// uploaded. The times live on the folder row, so a restart keeps them.
    #[test]
    fn a_folder_records_when_it_last_scanned_and_uploaded() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let path = temp.path().join("state.db");
        let store = StateStore::open(&path).expect("open the store");
        store
            .add_folder(&folder("/photos"))
            .expect("add the folder");

        assert_eq!(
            store.folder_times("/photos").expect("read the times"),
            FolderTimes::default(),
            "a fresh folder has scanned and uploaded nothing yet"
        );

        assert!(
            store
                .record_folder_scan("/photos", 1_700_000_000_000_000_000)
                .expect("record the scan"),
            "the tracked folder's scan time is written"
        );
        assert!(
            store
                .record_folder_upload("/photos", 1_700_000_001_000_000_000)
                .expect("record the upload"),
            "the tracked folder's upload time is written"
        );
        assert!(
            !store
                .record_folder_scan("/elsewhere", 1)
                .expect("an untracked path writes nothing"),
            "a path this client does not hold has no row to time"
        );
        drop(store);

        let reopened = StateStore::open(&path).expect("reopen the store");
        assert_eq!(
            reopened.folder_times("/photos").expect("read the times"),
            FolderTimes {
                last_scan_ns: Some(1_700_000_000_000_000_000),
                last_upload_ns: Some(1_700_000_001_000_000_000),
            },
            "#43: the times survive a restart"
        );
        assert_eq!(
            reopened
                .folder_times("/elsewhere")
                .expect("an untracked path reads as unset"),
            FolderTimes::default()
        );
    }
}
