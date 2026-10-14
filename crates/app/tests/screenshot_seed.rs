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
use sync_core::store::{AlbumScope, ItemRecord, ItemState, SourceFolder, StateStore};

fn row(folder: &Path, name: &str, state: ItemState, index: usize) -> ItemRecord {
    let error_class = match state {
        ItemState::RetryWait | ItemState::FailedPermanent => {
            Some(sync_core::failure::FailureClass::ServerTransient)
        }
        _ => None,
    };
    let asset_id = match state {
        ItemState::Uploaded | ItemState::Duplicate => Some(format!("asset-{index}")),
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
        sidecar_sha1: None,
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
                // the album scope controls are visible in both states.
                album_mapping,
                album_scope: if album_mapping {
                    AlbumScope::WholeFolder
                } else {
                    AlbumScope::Leaf
                },
            })
            .expect("add the capture folder");
    }

    let mut index = 0;
    let mut write = |folder: &Path, name: &str, state: ItemState| {
        index += 1;
        store
            .record(&row(folder, name, state, index))
            .expect("record the capture row");
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
}
