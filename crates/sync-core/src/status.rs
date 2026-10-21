//! The status the dashboard and the tray read (SPEC R8.1, R8.2).
//!
//! The status is derived, never accumulated: one function takes what the
//! client knows and returns the activity and the counts. The engine calls it
//! over its live entries; the shell calls it over the store-backed list while
//! no engine runs, so the two answers cannot drift apart.

use crate::connection::ConnectionState;
use crate::meter::UploadTraffic;
use crate::store::{AlbumScope, FolderCounts};

/// The activity state the tray and the dashboard show (R8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// Nothing is queued.
    Idle,
    /// Files are queued or in flight.
    Syncing,
    /// The user paused new uploads (R8.7).
    Paused,
    /// A failure or a missing folder needs the user.
    NeedsAttention,
}

impl Activity {
    /// The human-readable name.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Syncing => "Syncing",
            Self::Paused => "Paused",
            Self::NeedsAttention => "Needs attention",
        }
    }
}

/// The live state of one source folder (R2.5, R8.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderStatus {
    /// The folder path the user chose.
    pub path: String,
    /// Whether the watcher and the scan descend into subfolders (R2.1).
    pub recursive: bool,
    /// Whether this folder's files join an album (R3.4).
    pub album_mapping: bool,
    /// What the folder maps to once album mapping is on (R3.4).
    pub album_scope: AlbumScope,
    /// Whether the folder is readable right now (R2.5).
    pub available: bool,
    /// The folder's live counts.
    pub counts: FolderCounts,
    /// The last failure under this folder, if any.
    pub last_error: Option<String>,
    /// This folder's share of the upload traffic: the bytes its upload bodies
    /// have moved and the requests on the wire right now (R8.1).
    pub uploads: UploadTraffic,
}

/// Everything the dashboard and the tray read (R8.1, R8.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The global activity state.
    pub activity: Activity,
    /// One entry per source folder, in the stored order.
    pub folders: Vec<FolderStatus>,
    /// The summed counts across folders. The review count is the store's
    /// global one when the caller has it, because a row under a removed source
    /// folder still waits for a decision.
    pub counts: FolderCounts,
    /// The last failure the engine recorded, if any.
    pub last_error: Option<String>,
    /// The upload traffic across every folder (R8.1): the rate the card shows
    /// and how many uploads are on the wire. Like the counts, it is the sum of
    /// the folders'.
    pub uploads: UploadTraffic,
}

impl Status {
    /// Derives the status from what the client knows (R8.1, R8.2).
    #[must_use]
    pub fn derive(inputs: StatusInputs) -> Self {
        let StatusInputs {
            folders,
            paused,
            connection,
            review_total,
            last_error,
        } = inputs;
        let mut counts = FolderCounts::default();
        let mut uploads = UploadTraffic::default();
        for folder in &folders {
            counts.pending += folder.counts.pending;
            counts.uploaded += folder.counts.uploaded;
            counts.failed += folder.counts.failed;
            counts.review += folder.counts.review;
            uploads.add(folder.uploads);
        }
        // R5.4: a row under a removed source folder is still a decision the
        // user owes, so the store's own count wins when the caller has it.
        // Without it, the folder sum is the best answer the caller has.
        if let Some(total) = review_total {
            counts.review = total;
        }
        let needs_the_user = folders
            .iter()
            .any(|folder| !folder.available || folder.last_error.is_some());
        let activity = if paused {
            Activity::Paused
        } else if needs_the_user || connection == ConnectionHealth::NeedsAttention {
            Activity::NeedsAttention
        } else if counts.pending > 0 {
            Activity::Syncing
        } else if counts.review > 0 || counts.failed > 0 {
            // R5.4: a file waiting for a keep-or-trash decision is the user's
            // to answer, like a failure.
            Activity::NeedsAttention
        } else {
            Activity::Idle
        };
        Self {
            activity,
            folders,
            counts,
            last_error,
            uploads,
        }
    }

    /// The upload progress across every folder, 0.0 to 1.0 (R8.1).
    #[must_use]
    pub fn progress(&self) -> f32 {
        let total = self.counts.uploaded + self.counts.pending;
        if total == 0 {
            return 0.0;
        }
        self.counts.uploaded as f32 / total as f32
    }
}

/// What the status needs to know about the connection (R8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionHealth {
    /// A session exists and the server answers.
    Ready,
    /// The client lost a session it had, so the user must act (R1.4).
    NeedsAttention,
    /// No session yet, or a sign-in is running.
    NotConnected,
}

impl ConnectionHealth {
    /// Maps the state the session holds onto the status input.
    ///
    /// A client that has not signed in yet is not a problem to raise; a client
    /// that lost a session is (R1.4).
    #[must_use]
    pub fn from_state(state: &ConnectionState) -> Self {
        match state {
            ConnectionState::Connected => Self::Ready,
            ConnectionState::NeedsAttention(_) => Self::NeedsAttention,
            ConnectionState::NotConfigured | ConnectionState::Connecting => Self::NotConnected,
        }
    }
}

/// Everything [`Status::derive`] needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusInputs {
    /// One entry per source folder, in the order the cards show them.
    pub folders: Vec<FolderStatus>,
    /// Whether the user holds new uploads (R8.7).
    pub paused: bool,
    /// The connection, as far as the status cares.
    pub connection: ConnectionHealth,
    /// The store's global review count, when the caller can supply it.
    pub review_total: Option<u32>,
    /// The last failure, for the global card.
    pub last_error: Option<String>,
}
