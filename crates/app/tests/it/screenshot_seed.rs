//! Seeds the dashboard capture with real folders and rows.
//!
//! The screenshot recipe sets `IMMICH_SYNC_CAPTURE_DB` to the state database
//! under the capture's XDG data directory and runs:
//!
//! ```text
//! IMMICH_SYNC_CAPTURE_DB=... cargo test -p immich-sync --test screenshot_seed \
//!     -- --ignored
//! ```
//!
//! It is test-only: the product never writes these fixtures. The folders it
//! creates under `IMMICH_SYNC_CAPTURE_ROOT` are real directories, so the
//! dashboard's availability check passes and the cards show live counts.

use std::path::Path;

use sync_core::hash::Sha1;
use sync_core::store::{AlbumScope, ItemRecord, ItemState, NewVersion, SourceFolder, StateStore};

fn row(folder: &Path, name: &str, state: ItemState, index: usize) -> ItemRecord {
    let error_class = match state {
        ItemState::RetryWait | ItemState::DeadLetter => {
            Some(sync_core::failure::FailureClass::ServerTransient)
        }
        ItemState::FailedPermanent => Some(sync_core::failure::FailureClass::UnsupportedFormat),
        _ => None,
    };
    let error_message = match state {
        ItemState::DeadLetter => Some(
            "the server answered 503 in a form this client does not understand: Service Unavailable"
                .to_string(),
        ),
        ItemState::FailedPermanent => {
            Some("the server does not accept this file format: image/x-canon-cr3".to_string())
        }
        _ => None,
    };
    let asset_id = match state {
        ItemState::Uploaded | ItemState::Duplicate | ItemState::Missing => {
            Some(format!("asset-{index}"))
        }
        _ => None,
    };
    ItemRecord {
        path: folder.join(name).display().to_string(),
        size: 1024 * 1024,
        mtime_ns: 1_700_000_000_000_000_000,
        sha1: Sha1::from_hex(&format!("{index:040x}")).expect("a 40-character hex digest"),
        asset_id,
        state,
        error_class,
        error_message,
        attempts: 0,
        // R3.3: no sidecar has been sent in the capture, so the asset carries
        // none.
        sidecar: None,
    }
}

/// Records the attempts a parked row spent, which is what the failed-items
/// view shows. The count lives in the attempt log, so the fixture writes the
/// same rows the pipeline would (`StateStore::record_attempt`).
fn record_attempts(store: &StateStore, row: &ItemRecord, count: u32) {
    for index in 0..count {
        store
            .record_attempt(&sync_core::store::AttemptRecord {
                path: row.path.clone(),
                started_ns: row.mtime_ns + i64::from(index),
                outcome: sync_core::store::AttemptOutcome::Failed,
                error_class: row.error_class,
            })
            .expect("record the capture attempt");
    }
}

#[test]
#[ignore = "run by the screenshot recipe, with the capture environment set"]
fn seed_capture_state() {
    let path = std::env::var("IMMICH_SYNC_CAPTURE_DB")
        .expect("the screenshot recipe sets IMMICH_SYNC_CAPTURE_DB");
    let root = std::env::var("IMMICH_SYNC_CAPTURE_ROOT")
        .expect("the screenshot recipe sets IMMICH_SYNC_CAPTURE_ROOT");
    let store = StateStore::open(&path).expect("open the capture state database");

    let pictures = Path::new(&root).join("Pictures");
    let videos = Path::new(&root).join("Videos");
    std::fs::create_dir_all(&pictures).expect("create the Pictures folder");
    std::fs::create_dir_all(&videos).expect("create the Videos folder");
    for (folder, album_mapping) in [(&pictures, true), (&videos, false)] {
        store
            .add_folder(&SourceFolder {
                path: folder.display().to_string(),
                recursive: true,
                // R3.4: the capture shows one mapped and one unmapped folder, so
                // the card states the album scope and the mapping-off form.
                album_mapping,
                album_scope: if album_mapping {
                    AlbumScope::WholeFolder
                } else {
                    AlbumScope::Leaf
                },
                // #47: the capture shows one folder with a custom title and one
                // that takes its folder name.
                album_title: album_mapping.then(|| "Pictures 2024".to_string()),
            })
            .expect("add the capture folder");
    }

    let mut index = 0;
    // #44: the folder detail lists the assets' current versions, newest first,
    // so the uploaded rows get one each, spread over the last two days.
    let upload_base_ns = immich_sync::times::now_ns();
    let mut write = |folder: &Path, name: &str, state: ItemState| {
        index += 1;
        let row = row(folder, name, state, index);
        store.record(&row).expect("record the capture row");
        match state {
            ItemState::DeadLetter => record_attempts(&store, &row, 8),
            ItemState::FailedPermanent => record_attempts(&store, &row, 1),
            _ => {}
        }
        if let Some(asset_id) = row.asset_id.clone() {
            store
                .record_version(&NewVersion {
                    path: row.path.clone(),
                    asset_id,
                    sha1: row.sha1,
                    created_at_ns: upload_base_ns
                        - i64::try_from(index).expect("a small index") * 7 * 60 * 1_000_000_000,
                    stack_id: None,
                })
                .expect("record the capture version");
        }
    };
    for photo in 0..312 {
        write(
            &pictures,
            &format!("IMG_{photo:04}.jpg"),
            ItemState::Uploaded,
        );
    }
    for photo in 0..4 {
        write(
            &pictures,
            &format!("QUEUED_{photo}.jpg"),
            ItemState::Pending,
        );
    }
    for video in 0..98 {
        write(
            &videos,
            &format!("VID_{video:04}.mp4"),
            ItemState::Duplicate,
        );
    }
    for video in 0..7 {
        write(&videos, &format!("QUEUED_{video}.mp4"), ItemState::Pending);
    }
    for failed in 0..2 {
        write(
            &videos,
            &format!("FAILED_{failed}.mp4"),
            ItemState::RetryWait,
        );
    }
    // The failed-items capture: three items the retry cap parked, and two the
    // server refused permanently (R6.3, R6.5).
    for parked in 0..3 {
        write(
            &videos,
            &format!("PARKED_{parked}.mp4"),
            ItemState::DeadLetter,
        );
    }
    for refused in 0..2 {
        write(
            &pictures,
            &format!("RAW_{refused}.cr3"),
            ItemState::FailedPermanent,
        );
    }
    // The deletion-review capture: three files that disappeared and wait for a
    // keep-or-trash decision (R5.3).
    for missing in 0..3 {
        write(
            &pictures,
            &format!("GONE_{missing}.jpg"),
            ItemState::Missing,
        );
    }

    // The folder-detail capture (#43): a scan a few minutes old and an upload
    // just before it, so the dialog shows both timestamp forms instead of
    // "not yet".
    let now_ns = immich_sync::times::now_ns();
    let minute = 60 * 1_000_000_000;
    store
        .record_folder_scan(&pictures.display().to_string(), now_ns - 3 * minute)
        .expect("record the capture scan time");
    store
        .record_folder_upload(&pictures.display().to_string(), now_ns - 8 * minute)
        .expect("record the capture upload time");
    store
        .record_folder_scan(&videos.display().to_string(), now_ns - 3 * minute)
        .expect("record the second capture scan time");
}
