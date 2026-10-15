//! The dashboard view model: plain data with no UI imports.
//!
//! [`DashboardModel::from_engine`] maps the live engine status onto the cards
//! (R8.1). [`sample`] is the layout fixture the view tests render; the shell
//! never uses it.

use sync_core::engine::{Activity, EngineStatus};
use sync_core::store::{AlbumScope, FolderCounts};

/// The activity state of the client or one source folder (SPEC R8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActivityState {
    Idle,
    Syncing,
    Paused,
    NeedsAttention,
}

impl ActivityState {
    /// The human-readable name, using the vocabulary of SPEC R8.2.
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

/// The sync item counts one card shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub pending: u32,
    pub uploaded: u32,
    pub failed: u32,
}

/// One dashboard status card: the global card, or one source folder's folder
/// status.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusCard {
    /// The folder name, or "All folders" on the global card.
    pub title: String,
    /// The source folder path; `None` on the global card.
    pub path: Option<String>,
    pub activity: ActivityState,
    /// Upload progress, 0.0 to 1.0.
    pub progress: f32,
    pub counts: Counts,
    /// The deletion review items that await a decision.
    pub review_count: u32,
    /// Whether the watcher and the scan descend into subfolders (R2.1);
    /// only meaningful on a folder card.
    pub recursive: bool,
    /// Whether this source folder's files join an album (R3.4); always `false`
    /// on the global card, which is not a mapped folder.
    pub album_mapping: bool,
    /// The album scope for this folder (R3.4).
    pub album_scope: AlbumScope,
    /// Whether the source folder is readable (R2.5); always `true` for the
    /// global card.
    pub available: bool,
    /// The connection state, on the global card (R8.1).
    pub connection: Option<String>,
    /// The last error, already classified by the engine.
    pub last_error: Option<String>,
}

/// The text one status card renders.
///
/// Formatting lives here, away from the view, so the card content is testable
/// without a UI.
#[derive(Clone, Debug, PartialEq)]
pub struct CardContent {
    pub title: String,
    pub path: Option<String>,
    pub state: String,
    pub progress: f32,
    pub progress_percent: String,
    pub counts: String,
    pub review: Option<String>,
    /// The album line: `None` only on the card that has no folder of its own
    /// (R3.4).
    pub album: Option<String>,
    /// Whether the folder is readable; the counts line says so when it is not.
    pub available: bool,
    /// The connection line, on the global card.
    pub connection: Option<String>,
    pub error: Option<String>,
}

impl StatusCard {
    /// The text this card renders.
    #[must_use]
    pub fn content(&self) -> CardContent {
        CardContent {
            title: self.title.clone(),
            path: self.path.clone(),
            state: self.activity.label().to_string(),
            progress: self.progress.clamp(0.0, 1.0),
            progress_percent: format!(
                "{}%",
                (self.progress.clamp(0.0, 1.0) * 100.0).round() as u32
            ),
            counts: if self.available {
                format!(
                    "{} uploaded · {} pending · {} failed",
                    self.counts.uploaded, self.counts.pending, self.counts.failed
                )
            } else {
                "Folder unavailable".to_string()
            },
            review: (self.review_count > 0)
                .then(|| format!("{} missing files to review", self.review_count)),
            // The line is always there for a folder: the scope radios change
            // what the folder will map to, so the card must show the stored
            // scope while the mapping is still off (R3.4).
            album: self.path.as_ref().map(|_| {
                if self.album_mapping {
                    format!("Albums: {}", self.album_scope.label())
                } else {
                    "Albums: off".to_string()
                }
            }),
            available: self.available,
            connection: self.connection.clone(),
            error: self.last_error.clone(),
        }
    }
}

impl From<Activity> for ActivityState {
    fn from(activity: Activity) -> Self {
        match activity {
            Activity::Idle => Self::Idle,
            Activity::Syncing => Self::Syncing,
            Activity::Paused => Self::Paused,
            Activity::NeedsAttention => Self::NeedsAttention,
        }
    }
}

/// The card counts from the engine's counts.
fn counts_from(counts: FolderCounts) -> Counts {
    Counts {
        pending: counts.pending,
        uploaded: counts.uploaded,
        failed: counts.failed,
    }
}

/// The card title for a folder path: the last component, or the path itself.
///
/// The split accepts both separators, so a Windows path mapped on Linux still
/// names its folder in a test.
#[must_use]
pub fn folder_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

/// The upload progress for one folder, 0.0 to 1.0.
fn folder_progress(counts: FolderCounts) -> f32 {
    let total = counts.uploaded + counts.pending;
    if total == 0 {
        return 0.0;
    }
    counts.uploaded as f32 / total as f32
}

impl DashboardModel {
    /// Maps the live engine status onto the global card and one card per
    /// source folder (R8.1, R8.2).
    #[must_use]
    pub fn from_engine(status: &EngineStatus, connection: Option<String>) -> Self {
        let global = StatusCard {
            title: "All folders".to_string(),
            path: None,
            activity: status.activity.into(),
            progress: status.progress(),
            counts: counts_from(status.counts),
            review_count: status.counts.review,
            available: true,
            connection,
            last_error: status.last_error.clone(),
            recursive: false,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
        };
        let folders = status
            .folders
            .iter()
            .map(|folder| StatusCard {
                title: folder_name(&folder.path),
                path: Some(folder.path.clone()),
                activity: if !folder.available {
                    ActivityState::NeedsAttention
                } else if status.activity == Activity::Paused {
                    ActivityState::Paused
                } else if folder.counts.pending > 0 {
                    ActivityState::Syncing
                } else if folder.last_error.is_some() {
                    ActivityState::NeedsAttention
                } else {
                    ActivityState::Idle
                },
                progress: folder_progress(folder.counts),
                counts: counts_from(folder.counts),
                review_count: folder.counts.review,
                available: folder.available,
                connection: None,
                last_error: folder.last_error.clone(),
                recursive: folder.recursive,
                album_mapping: folder.album_mapping,
                album_scope: folder.album_scope,
            })
            .collect();
        Self { global, folders }
    }
}

/// Everything the dashboard shows.
#[derive(Clone, Debug, PartialEq)]
pub struct DashboardModel {
    pub global: StatusCard,
    pub folders: Vec<StatusCard>,
}

/// Fake data that exercises the dashboard layout until the engine exists.
#[must_use]
pub fn sample() -> DashboardModel {
    DashboardModel {
        global: StatusCard {
            title: "All folders".to_string(),
            path: None,
            activity: ActivityState::Syncing,
            progress: 0.62,
            counts: Counts {
                pending: 18,
                uploaded: 421,
                failed: 2,
            },
            review_count: 3,
            available: true,
            connection: Some("Connected".to_string()),
            last_error: None,
            recursive: false,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
        },
        folders: vec![
            StatusCard {
                title: "Camera Roll".to_string(),
                path: Some("Pictures\\Camera Roll".to_string()),
                activity: ActivityState::Syncing,
                progress: 0.74,
                counts: Counts {
                    pending: 4,
                    uploaded: 312,
                    failed: 0,
                },
                review_count: 0,
                available: true,
                connection: None,
                last_error: None,
                recursive: true,
                album_mapping: true,
                album_scope: AlbumScope::WholeFolder,
            },
            StatusCard {
                title: "DCIM".to_string(),
                path: Some("Pictures\\DCIM".to_string()),
                activity: ActivityState::Idle,
                progress: 1.0,
                counts: Counts {
                    pending: 0,
                    uploaded: 98,
                    failed: 0,
                },
                review_count: 3,
                available: true,
                connection: None,
                last_error: None,
                recursive: true,
                album_mapping: true,
                album_scope: AlbumScope::Leaf,
            },
            StatusCard {
                title: "Videos".to_string(),
                path: Some("Videos\\Projects".to_string()),
                activity: ActivityState::NeedsAttention,
                progress: 0.31,
                counts: Counts {
                    pending: 7,
                    uploaded: 11,
                    failed: 2,
                },
                review_count: 0,
                available: true,
                connection: None,
                last_error: Some(
                    "permission_denied: the API key cannot write to this album".to_string(),
                ),
                recursive: false,
                album_mapping: true,
                album_scope: AlbumScope::WholeFolder,
            },
            StatusCard {
                title: "Archive".to_string(),
                path: Some("Archive\\2024".to_string()),
                activity: ActivityState::Paused,
                progress: 0.0,
                counts: Counts {
                    pending: 240,
                    uploaded: 0,
                    failed: 0,
                },
                review_count: 0,
                available: true,
                connection: None,
                last_error: None,
                recursive: true,
                album_mapping: false,
                album_scope: AlbumScope::Leaf,
            },
        ],
    }
}
