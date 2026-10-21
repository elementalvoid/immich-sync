//! The derived status (SPEC R8.1, R8.2): one table for what the tray and the
//! dashboard show, whether or not the engine runs.

use sync_core::meter::UploadTraffic;
use sync_core::status::{Activity, ConnectionHealth, FolderStatus, Status, StatusInputs};
use sync_core::store::{AlbumScope, FolderCounts};

fn counts(pending: u32, uploaded: u32, failed: u32, review: u32) -> FolderCounts {
    FolderCounts {
        pending,
        uploaded,
        failed,
        review,
    }
}

fn folder(path: &str, counts: FolderCounts) -> FolderStatus {
    FolderStatus {
        path: path.to_string(),
        recursive: true,
        album_mapping: false,
        album_scope: AlbumScope::Leaf,
        available: true,
        counts,
        last_error: None,
        last_scan_ns: None,
        last_upload_ns: None,
        uploads: UploadTraffic::default(),
    }
}

fn inputs(folders: Vec<FolderStatus>) -> StatusInputs {
    StatusInputs {
        folders,
        paused: false,
        connection: ConnectionHealth::Ready,
        review_total: None,
        last_error: None,
    }
}

#[test]
fn a_client_with_no_folders_is_idle() {
    let status = Status::derive(inputs(Vec::new()));
    assert_eq!(status.activity, Activity::Idle);
    assert_eq!(status.counts, FolderCounts::default());
    assert_eq!(status.progress(), 0.0);
}

#[test]
fn a_pending_item_means_syncing_and_drives_the_progress() {
    let status = Status::derive(inputs(vec![folder("/photos", counts(3, 1, 0, 0))]));
    assert_eq!(status.activity, Activity::Syncing);
    assert_eq!(status.counts.pending, 3);
    assert_eq!(status.counts.uploaded, 1);
    assert!((status.progress() - 0.25).abs() < f32::EPSILON);
}

#[test]
fn a_pause_wins_over_queued_work() {
    let mut given = inputs(vec![folder("/photos", counts(3, 0, 0, 0))]);
    given.paused = true;
    assert_eq!(Status::derive(given).activity, Activity::Paused);
}

#[test]
fn a_lost_connection_needs_attention() {
    let mut given = inputs(vec![folder("/photos", FolderCounts::default())]);
    given.connection = ConnectionHealth::NeedsAttention;
    assert_eq!(Status::derive(given).activity, Activity::NeedsAttention);
}

#[test]
fn a_client_that_never_signed_in_is_not_a_problem_to_raise() {
    let mut given = inputs(vec![folder("/photos", FolderCounts::default())]);
    given.connection = ConnectionHealth::NotConnected;
    assert_eq!(Status::derive(given).activity, Activity::Idle);
}

#[test]
fn an_unavailable_folder_needs_attention() {
    let mut missing = folder("/photos", FolderCounts::default());
    missing.available = false;
    assert_eq!(
        Status::derive(inputs(vec![missing])).activity,
        Activity::NeedsAttention
    );
}

#[test]
fn a_folder_error_needs_attention() {
    let mut failed = folder("/photos", FolderCounts::default());
    failed.last_error = Some("the server refused the upload".to_string());
    assert_eq!(
        Status::derive(inputs(vec![failed])).activity,
        Activity::NeedsAttention
    );
}

#[test]
fn a_waiting_review_needs_attention_even_with_nothing_queued() {
    assert_eq!(
        Status::derive(inputs(vec![folder("/photos", counts(0, 4, 0, 1))])).activity,
        Activity::NeedsAttention
    );
}

/// A parked item under a folder the user removed has no live entry to carry
/// its error, so the count itself has to raise the card.
#[test]
fn a_parked_failure_needs_attention_without_a_folder_error() {
    assert_eq!(
        Status::derive(inputs(vec![folder("/photos", counts(0, 4, 2, 0))])).activity,
        Activity::NeedsAttention
    );
}

#[test]
fn the_store_review_total_wins_over_the_folder_sum() {
    let mut given = inputs(vec![
        folder("/photos", counts(0, 1, 0, 2)),
        folder("/videos", counts(0, 1, 0, 0)),
    ]);
    given.review_total = Some(5);
    let status = Status::derive(given);
    assert_eq!(
        status.counts.review, 5,
        "a review under a removed folder must still count"
    );
}

#[test]
fn the_counts_are_summed_across_folders() {
    let status = Status::derive(inputs(vec![
        folder("/photos", counts(1, 2, 3, 4)),
        folder("/videos", counts(10, 20, 30, 40)),
    ]));
    assert_eq!(status.counts, counts(11, 22, 33, 44));
}

/// R8.1: the global card's throughput is the sum of the folders', so the two
/// cannot disagree about what is on the wire.
#[test]
fn the_upload_traffic_is_summed_across_folders() {
    let mut photos = folder("/photos", FolderCounts::default());
    photos.uploads = UploadTraffic {
        bytes: 1_000,
        bytes_per_second: 400,
        in_flight: 1,
    };
    let mut videos = folder("/videos", FolderCounts::default());
    videos.uploads = UploadTraffic {
        bytes: 2_000,
        bytes_per_second: 600,
        in_flight: 2,
    };
    assert_eq!(
        Status::derive(inputs(vec![photos, videos])).uploads,
        UploadTraffic {
            bytes: 3_000,
            bytes_per_second: 1_000,
            in_flight: 3,
        }
    );
}
