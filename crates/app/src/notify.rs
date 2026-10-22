//! The notification policy (R5.4, R10.1).

use std::collections::HashSet;

use gpui_kit::SystemNotification;
use sync_core::status::{Activity, Status};
use sync_core::store::FailedItem;

/// The latches behind the system notifications: what was already announced,
/// so a repeat observation cannot repeat a notification. `observe` runs on
/// every status refresh and every parked-list read.
#[derive(Default)]
pub struct Notifier {
    /// The review count the last notification was based on (R5.4).
    review: u32,
    /// Whether the attention notification already fired for the current
    /// attention episode (R10.1).
    attention: bool,
    /// The failed items the attention and failure notifications already cover
    /// (R10.1).
    failed: HashSet<String>,
    /// Whether the first read of the parked list has seeded `failed` (R10.1).
    failed_seeded: bool,
}

impl Notifier {
    /// Answers one observation with the notifications to post, in the order
    /// the user reads them.
    ///
    /// `failed` is `None` while the parked list has not been read: the failure
    /// latch stays unseeded then, so the first read that arrives is still the
    /// list the process started with (R10.1).
    #[must_use]
    pub fn observe(
        &mut self,
        status: &Status,
        failed: Option<&[FailedItem]>,
        connection_reason: Option<&str>,
    ) -> Vec<SystemNotification> {
        let mut notifications = Vec::new();
        if status.counts.review > self.review {
            tracing::info!(
                count = status.counts.review,
                "R5.4: the deletion review raised a notification"
            );
            notifications.push(new_review_notification(status.counts.review));
        }
        self.review = status.counts.review;
        notifications.extend(self.attention(status, connection_reason));
        if let Some(failed) = failed {
            notifications.extend(self.failures(failed));
        }
        notifications
    }

    /// Raises the attention notification once per attention episode (R10.1).
    fn attention(
        &mut self,
        status: &Status,
        connection_reason: Option<&str>,
    ) -> Option<SystemNotification> {
        if status.activity != Activity::NeedsAttention {
            self.attention = false;
            return None;
        }
        if self.attention {
            return None;
        }
        let reason = connection_reason.map(str::to_string).or_else(|| {
            // A parked failure has its own notification, so the generic one
            // stays quiet while that is the only reason.
            if status.counts.failed > 0 {
                return None;
            }
            if let Some(folder) = status.folders.iter().find(|folder| !folder.available) {
                return Some(format!("The source folder {} is unavailable.", folder.path));
            }
            status.last_error.clone()
        })?;
        self.attention = true;
        tracing::info!(
            reason = %reason,
            "R10.1: the client entered attention and raised a notification"
        );
        Some(new_attention_notification(&reason))
    }

    /// Raises one notification for the paths that parked since the last read
    /// (R10.1).
    fn failures(&mut self, failed: &[FailedItem]) -> Option<SystemNotification> {
        // An item that left the list forgets it was announced, so a later
        // failure notifies again.
        self.failed
            .retain(|path| failed.iter().any(|item| item.path == *path));
        if !self.failed_seeded {
            // The first read is the parked list the process started with, so
            // it seeds the latch instead of notifying: a relaunch must not
            // announce the dead letter again.
            self.failed_seeded = true;
            self.failed
                .extend(failed.iter().map(|item| item.path.clone()));
            return None;
        }
        let fresh: Vec<String> = failed
            .iter()
            .filter(|item| !self.failed.contains(&item.path))
            .map(|item| item.path.clone())
            .collect();
        if fresh.is_empty() {
            return None;
        }
        self.failed.extend(fresh.iter().cloned());
        tracing::info!(
            count = fresh.len(),
            "R10.1: a failed item raised a notification"
        );
        Some(new_failure_notification(&fresh))
    }
}

/// The system notification a new deletion review item raises (R5.4, R10.1).
///
/// The stable tag replaces a previous notification where the platform
/// supports it.
#[must_use]
pub fn new_review_notification(count: u32) -> SystemNotification {
    let body = match count {
        1 => "A file disappeared from a source folder. Review it to keep the server copy or move \
              it to the trash."
            .to_string(),
        files => format!(
            "{files} files disappeared from your source folders. Review them to keep the server \
             copies or move them to the trash."
        ),
    };
    SystemNotification {
        tag: "deletion-review".into(),
        title: "Immich Sync".into(),
        body: body.into(),
        actions: Vec::new(),
    }
}

/// The system notification the first transition into attention raises
/// (R10.1).
///
/// The stable tag replaces a previous episode's notification where the
/// platform supports it.
#[must_use]
pub fn new_attention_notification(reason: &str) -> SystemNotification {
    SystemNotification {
        tag: "needs-attention".into(),
        title: "Immich Sync".into(),
        body: format!("Needs attention: {reason}").into(),
        actions: Vec::new(),
    }
}

/// The system notification a newly failed item raises (R10.1).
///
/// The paths are the failures this notification is the first to cover; one
/// path is named in full and several are counted, so a burst does not produce
/// a wall of file names.
#[must_use]
pub fn new_failure_notification(paths: &[String]) -> SystemNotification {
    let body = match paths {
        [path] => format!("{path} could not be uploaded and needs attention."),
        paths => format!(
            "{} files could not be uploaded and need attention.",
            paths.len()
        ),
    };
    SystemNotification {
        tag: "failed-item".into(),
        title: "Immich Sync".into(),
        body: body.into(),
        actions: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::Notifier;
    use gpui_kit::SystemNotification;
    use sync_core::meter::UploadTraffic;
    use sync_core::status::{Activity, FolderStatus, Status};
    use sync_core::store::{AlbumScope, FailedItem, FolderCounts, ItemState};

    fn status(activity: Activity, review: u32, failed: u32) -> Status {
        Status {
            activity,
            folders: Vec::new(),
            counts: FolderCounts {
                pending: 0,
                uploaded: 0,
                failed,
                review,
            },
            last_error: None,
            uploads: UploadTraffic::default(),
            scanning: false,
        }
    }

    fn with_folder(mut status: Status, path: &str, available: bool) -> Status {
        status.folders.push(FolderStatus {
            path: path.to_string(),
            recursive: true,
            album_mapping: false,
            album_scope: AlbumScope::Leaf,
            album_title: None,
            album_rename: None,
            available,
            counts: FolderCounts::default(),
            last_error: None,
            last_scan_ns: None,
            last_upload_ns: None,
            uploads: UploadTraffic::default(),
        });
        status
    }

    fn parked(path: &str) -> FailedItem {
        FailedItem {
            path: path.to_string(),
            state: ItemState::FailedPermanent,
            error_class: None,
            error_message: None,
            attempts: 1,
        }
    }

    fn tags(notifications: &[SystemNotification]) -> Vec<&str> {
        notifications
            .iter()
            .map(|notification| notification.tag.as_ref())
            .collect()
    }

    /// R5.4: a review count that rises posts once, and the observation that
    /// follows with the same count stays quiet.
    #[test]
    fn a_review_rise_posts_once_and_the_next_observation_stays_quiet() {
        let mut notifier = Notifier::default();
        assert!(
            notifier
                .observe(&status(Activity::Idle, 0, 0), None, None)
                .is_empty()
        );
        let posted = notifier.observe(&status(Activity::NeedsAttention, 1, 0), None, None);
        assert_eq!(tags(&posted), ["deletion-review"]);
        assert!(
            notifier
                .observe(&status(Activity::NeedsAttention, 1, 0), None, None)
                .is_empty(),
            "R5.4: the same count does not raise a second notification"
        );
    }

    /// R5.4: a decision lowers the count, and the next item raises a
    /// notification again.
    #[test]
    fn a_review_decrease_and_a_later_rise_posts_again() {
        let mut notifier = Notifier::default();
        let _ = notifier.observe(&status(Activity::NeedsAttention, 2, 0), None, None);
        assert!(
            notifier
                .observe(&status(Activity::Idle, 0, 0), None, None)
                .is_empty()
        );
        assert_eq!(
            tags(&notifier.observe(&status(Activity::NeedsAttention, 1, 0), None, None)),
            ["deletion-review"]
        );
    }

    /// R10.1: one notification per attention episode, and a later episode
    /// notifies again.
    #[test]
    fn attention_posts_once_per_episode_and_again_after_leaving() {
        let mut notifier = Notifier::default();
        let attention = with_folder(status(Activity::NeedsAttention, 0, 0), "/photos", false);
        assert_eq!(
            tags(&notifier.observe(&attention, None, None)),
            ["needs-attention"]
        );
        assert!(
            notifier.observe(&attention, None, None).is_empty(),
            "R10.1: the episode is announced once"
        );
        assert!(
            notifier
                .observe(&status(Activity::Idle, 0, 0), None, None)
                .is_empty(),
            "leaving attention is silent"
        );
        assert_eq!(
            tags(&notifier.observe(&attention, None, None)),
            ["needs-attention"],
            "R10.1: a new episode notifies again"
        );
    }

    /// R10.1: a parked failure has its own notification, so the generic
    /// attention notice stays quiet while that is the reason.
    #[test]
    fn a_parked_failure_suppresses_the_generic_attention_notice() {
        let mut notifier = Notifier::default();
        let attention = with_folder(status(Activity::NeedsAttention, 0, 1), "/photos", false);
        assert!(
            notifier.observe(&attention, None, None).is_empty(),
            "R10.1: the failed-item notification covers a parked failure"
        );
    }

    /// R10.1: an unavailable folder names the path the user must fix.
    #[test]
    fn an_unavailable_folder_names_its_path() {
        let mut notifier = Notifier::default();
        let attention = with_folder(status(Activity::NeedsAttention, 0, 0), "/photos", false);
        let posted = notifier.observe(&attention, None, None);
        assert_eq!(tags(&posted), ["needs-attention"]);
        assert!(
            posted[0]
                .body
                .contains("The source folder /photos is unavailable."),
            "the notification names the folder: {}",
            posted[0].body
        );
    }

    /// R10.1: the first parked list is the dead letter the process started
    /// with, so it stays quiet; a path that parks later notifies.
    #[test]
    fn the_first_failed_list_seeds_silently_and_a_new_path_posts() {
        let mut notifier = Notifier::default();
        let old = [parked("/photos/old.jpg")];
        assert!(
            notifier
                .observe(&status(Activity::NeedsAttention, 0, 1), Some(&old), None)
                .is_empty(),
            "R10.1: a relaunch does not announce the dead letter"
        );
        let both = [parked("/photos/old.jpg"), parked("/photos/new.jpg")];
        let posted = notifier.observe(&status(Activity::NeedsAttention, 0, 2), Some(&both), None);
        assert_eq!(tags(&posted), ["failed-item"]);
        assert!(
            posted[0].body.contains("new.jpg") && !posted[0].body.contains("old.jpg"),
            "the notification names only the new failure: {}",
            posted[0].body
        );
    }

    /// R10.1: a poll that could not read the parked list must not seed the
    /// latch, so the first real read still stays quiet.
    #[test]
    fn an_unread_failed_list_leaves_the_latch_unseeded() {
        let mut notifier = Notifier::default();
        let old = [parked("/photos/old.jpg")];
        assert!(
            notifier
                .observe(&status(Activity::NeedsAttention, 0, 1), None, None)
                .is_empty()
        );
        assert!(
            notifier
                .observe(&status(Activity::NeedsAttention, 0, 1), Some(&old), None)
                .is_empty(),
            "R10.1: the first read still seeds instead of notifying"
        );
    }

    /// R10.1: one observation can raise all three, in the order the user
    /// reads them, and the connection reason wins for the attention notice.
    #[test]
    fn review_attention_and_failure_arrive_in_order() {
        let mut notifier = Notifier::default();
        let old = [parked("/photos/old.jpg")];
        assert!(
            notifier
                .observe(&status(Activity::Idle, 0, 0), Some(&old), None)
                .is_empty()
        );
        let both = [parked("/photos/old.jpg"), parked("/photos/new.jpg")];
        let posted = notifier.observe(
            &with_folder(status(Activity::NeedsAttention, 3, 1), "/photos", false),
            Some(&both),
            Some("the server is unreachable."),
        );
        assert_eq!(
            tags(&posted),
            ["deletion-review", "needs-attention", "failed-item"]
        );
        assert_eq!(
            posted[1].body.as_ref(),
            "Needs attention: the server is unreachable.",
            "the connection reason wins and is prefixed once"
        );
    }
}
