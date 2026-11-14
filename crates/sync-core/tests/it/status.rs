//! The derived status (SPEC R8.1, R8.2): one table for what the tray and the
//! dashboard show, whether or not the engine runs.

use sync_core::meter::UploadTraffic;
use sync_core::network::NetworkStatus;
use sync_core::status::{
    Activity, ConnectionHealth, FolderStatus, PauseReason, Status, StatusInputs,
};
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
        album_title: None,
        album_rename: None,
        available: true,
        counts,
        last_error: None,
        last_scan_ns: None,
        last_upload_ns: None,
        uploads: UploadTraffic::default(),
        ..Default::default()
    }
}

fn inputs(folders: Vec<FolderStatus>) -> StatusInputs {
    StatusInputs {
        folders,
        pause: None,
        network: NetworkStatus::default(),
        connection: ConnectionHealth::Ready,
        review_total: None,
        last_error: None,
        scanning: false,
    }
}

/// #56: the scan lifecycle is derived like every other field, so the engine's
/// live flag and a store-only client share one shape the views can read.
#[test]
fn the_scan_flag_rides_the_derived_status() {
    let mut given = inputs(Vec::new());
    given.scanning = true;
    assert!(
        Status::derive(given).scanning,
        "#56: a running scan must be published"
    );
    assert!(
        !Status::derive(inputs(Vec::new())).scanning,
        "#56: an idle client is not scanning"
    );
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
    given.pause = Some(PauseReason::User);
    assert_eq!(Status::derive(given).activity, Activity::Paused);
}

/// R8.8: the network hold is a reason on the existing paused state, and the
/// reason reaches the views instead of a bare "Paused".
#[test]
fn a_network_hold_pauses_with_its_reason() {
    let mut given = inputs(vec![folder("/photos", counts(3, 0, 0, 0))]);
    given.pause = Some(PauseReason::NotListed);
    let status = Status::derive(given);
    assert_eq!(status.activity, Activity::Paused);
    assert_eq!(status.pause, Some(PauseReason::NotListed));
    assert_eq!(
        status.pause.map(PauseReason::line),
        Some("Paused — not on a known network")
    );
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

/// #94: the global card reads Paused only when every folder is paused. One
/// paused folder leaves the client's state alone and shows the pause on its own
/// card, and the reason stays the folders' rather than the global pause's.
#[test]
fn a_folder_pause_pauses_the_client_only_when_every_folder_is_paused() {
    let mut paused = folder("/photos", counts(3, 0, 0, 0));
    paused.paused = true;
    let mut syncing = folder("/videos", counts(1, 0, 0, 0));

    let one = Status::derive(inputs(vec![paused.clone(), syncing.clone()]));
    assert_eq!(
        one.activity,
        Activity::Syncing,
        "#94: one paused folder leaves the client's own state alone"
    );
    assert_eq!(
        one.pause, None,
        "#94: a folder pause is not a global hold, so it has no global reason"
    );

    syncing.paused = true;
    let every = Status::derive(inputs(vec![paused, syncing]));
    assert_eq!(
        every.activity,
        Activity::Paused,
        "#94: every folder paused reads as Paused"
    );
    assert_eq!(
        every.pause, None,
        "#94: the paused folders show the reason on their own cards"
    );

    assert_eq!(
        Status::derive(inputs(Vec::new())).activity,
        Activity::Idle,
        "no folder is not every folder paused"
    );
}

/// #94: the global card sums only the work it can do, so a paused folder's
/// queue neither holds the client at Syncing nor dilutes its progress. A review
/// the user owes is still counted.
#[test]
fn a_paused_folder_is_left_out_of_the_global_sums() {
    let mut paused = folder("/photos", counts(5, 1, 2, 1));
    paused.paused = true;
    let active = folder("/videos", counts(1, 3, 0, 2));

    let status = Status::derive(inputs(vec![paused, active]));

    assert_eq!(
        status.counts.pending, 1,
        "#94: the paused folder's queue is not the client's work"
    );
    assert_eq!(status.counts.uploaded, 3);
    assert_eq!(status.counts.failed, 0);
    assert_eq!(
        status.counts.review, 3,
        "a decision the user owes is still theirs, paused folder or not"
    );
    assert!(
        (status.progress() - 0.75).abs() < f32::EPSILON,
        "#94: the paused folder must not dilute the global progress"
    );
    assert_eq!(status.activity, Activity::Syncing);

    // With the only unpaused folder idle, the paused queue cannot hold the
    // client at Syncing.
    let mut quiet = folder("/videos", counts(0, 3, 0, 0));
    quiet.paused = false;
    let mut paused = folder("/photos", counts(5, 0, 0, 0));
    paused.paused = true;
    let status = Status::derive(inputs(vec![paused, quiet]));
    assert_eq!(
        status.activity,
        Activity::Idle,
        "#94: a paused folder's queue does not read as the client syncing"
    );
}

/// #94: a paused folder's frozen availability does not hold the client's state;
/// its own card says so.
#[test]
fn a_paused_folder_does_not_hold_the_client_at_needs_attention() {
    let mut paused = folder("/photos", FolderCounts::default());
    paused.paused = true;
    paused.available = false;

    let status = Status::derive(inputs(vec![paused]));

    assert_eq!(status.activity, Activity::Paused);
}
