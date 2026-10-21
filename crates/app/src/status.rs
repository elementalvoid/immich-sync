//! The dashboard view model: plain data with no UI imports.
//!
//! [`DashboardModel::from_engine`] maps the live engine status onto the cards
//! (R8.1). [`sample`] is the layout fixture the view tests render; the shell
//! never uses it.

use std::path::Path;

use crate::diagnostics::format_rate;
use sync_core::meter::UploadTraffic;
use sync_core::status::{Activity, Status};
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
    /// The upload traffic this card reads (R8.1): the rate over the last two
    /// seconds and the uploads on the wire. The global card's is the sum of
    /// the folders'.
    pub uploads: UploadTraffic,
    /// The connection state, on the global card (R8.1).
    pub connection: Option<String>,
    /// The last error, already classified by the engine.
    pub last_error: Option<String>,
    /// When this folder last uploaded an asset, ns since the Unix epoch (#43).
    /// `None` on the global card, which is not a folder.
    pub last_upload_ns: Option<i64>,
    /// When a reconciliation scan last read this folder, ns since the Unix
    /// epoch (#43). `None` on the global card.
    pub last_scan_ns: Option<i64>,
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
    /// The upload line, on the global card (R8.1): the rate and the uploads on
    /// the wire. `Idle` only when nothing is in flight *and* the rate is zero;
    /// a rate still fading out is shown on its own.
    pub uploads: Option<String>,
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
            // R8.1: the line sums the folders, so it belongs to the card that
            // is the sum.
            uploads: self.path.is_none().then(|| upload_line(self.uploads)),
            connection: self.connection.clone(),
            error: self.last_error.clone(),
        }
    }
}

/// The upload line a card shows (R8.1).
///
/// The engine's rate is a rolling average over two seconds, so the window still
/// holds the last bytes after an upload finishes, and a burst shorter than one
/// status poll has a rate with nothing on the wire. The line is `Idle` only when
/// nothing is on the wire *and* no byte is moving; a rate on its own is shown
/// alone rather than with a zero count.
fn upload_line(uploads: UploadTraffic) -> String {
    if uploads.in_flight == 0 {
        return if uploads.bytes_per_second == 0 {
            "Idle".to_string()
        } else {
            format_rate(uploads.bytes_per_second as f64)
        };
    }
    format!(
        "{} · {} uploading",
        format_rate(uploads.bytes_per_second as f64),
        uploads.in_flight
    )
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
/// The platform's own separators decide, the same way `sync-core` names an
/// album from its folder (album names and card titles must agree).
#[must_use]
pub fn folder_name(path: &str) -> String {
    Path::new(path).file_name().map_or_else(
        || path.to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
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
    pub fn from_engine(status: &Status, connection: Option<String>) -> Self {
        let global = StatusCard {
            title: "All folders".to_string(),
            path: None,
            activity: status.activity.into(),
            progress: status.progress(),
            counts: counts_from(status.counts),
            review_count: status.counts.review,
            available: true,
            uploads: status.uploads,
            connection,
            last_error: status.last_error.clone(),
            last_upload_ns: None,
            last_scan_ns: None,
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
                uploads: folder.uploads,
                connection: None,
                last_error: folder.last_error.clone(),
                // #43: the folder detail shows both, relative and exact.
                last_upload_ns: folder.last_upload_ns,
                last_scan_ns: folder.last_scan_ns,
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
            uploads: UploadTraffic {
                bytes: 41_943_040,
                bytes_per_second: 2_516_582,
                in_flight: 3,
            },
            connection: Some("Connected".to_string()),
            last_error: None,
            last_upload_ns: None,
            last_scan_ns: None,
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
                uploads: UploadTraffic {
                    bytes: 41_943_040,
                    bytes_per_second: 2_516_582,
                    in_flight: 3,
                },
                connection: None,
                last_error: None,
                last_upload_ns: Some(1_700_000_000_000_000_000),
                last_scan_ns: Some(1_700_000_060_000_000_000),
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
                uploads: UploadTraffic::default(),
                connection: None,
                last_error: None,
                last_upload_ns: Some(1_700_000_000_000_000_000),
                last_scan_ns: None,
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
                uploads: UploadTraffic::default(),
                connection: None,
                last_error: Some(
                    "permission_denied: the API key cannot write to this album".to_string(),
                ),
                last_upload_ns: Some(1_700_000_000_000_000_000),
                last_scan_ns: Some(1_700_000_060_000_000_000),
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
                uploads: UploadTraffic::default(),
                connection: None,
                last_error: None,
                last_upload_ns: None,
                last_scan_ns: None,
                recursive: true,
                album_mapping: false,
                album_scope: AlbumScope::Leaf,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::upload_line;
    use sync_core::meter::UploadTraffic;

    /// A burst shorter than the poll interval leaves a rate with no upload on
    /// the wire: the window still holds its bytes, so the line must show the
    /// rate rather than claim the client is idle (R8.1).
    #[test]
    fn a_short_burst_reads_as_a_rate_then_fades_to_idle() {
        assert_eq!(
            upload_line(UploadTraffic {
                bytes: 1_048_576,
                bytes_per_second: 524_288,
                in_flight: 0,
            }),
            "512.0 KiB/s",
            "the burst is still in the window, so the line shows its rate"
        );
        assert_eq!(
            upload_line(UploadTraffic {
                bytes: 1_048_576,
                bytes_per_second: 0,
                in_flight: 0,
            }),
            "Idle",
            "once the window is empty the client reads idle"
        );
        assert_eq!(
            upload_line(UploadTraffic {
                bytes: 0,
                bytes_per_second: 524_288,
                in_flight: 2,
            }),
            "512.0 KiB/s · 2 uploading"
        );
    }
}
