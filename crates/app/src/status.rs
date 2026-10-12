//! The dashboard view model: plain data with no UI imports.
//!
//! The real engine fills these types later. Until then [`sample`] supplies fake
//! data so the dashboard layout is exercised.

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
    /// Deletion review items that await a decision.
    pub review_count: u32,
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
            counts: format!(
                "{} uploaded · {} pending · {} failed",
                self.counts.uploaded, self.counts.pending, self.counts.failed
            ),
            review: (self.review_count > 0)
                .then(|| format!("{} missing files to review", self.review_count)),
            error: self.last_error.clone(),
        }
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
            last_error: None,
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
                last_error: None,
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
                last_error: None,
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
                last_error: Some(
                    "permission_denied: the API key cannot write to this album".to_string(),
                ),
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
                last_error: None,
            },
        ],
    }
}
