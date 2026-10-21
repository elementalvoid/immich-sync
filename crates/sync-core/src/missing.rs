//! What a tracked file's disappearance is (SPEC R5.1, R5.2).
//!
//! One pass answers three questions for a row whose file is gone: a file of
//! the same content now sits at an untracked path (a rename), another tracked
//! item still holds the content and its file is on disk (a second copy), or
//! neither (a missing file that waits for the user's decision).
//!
//! The rules live here, apart from the scan loop, because they decide what the
//! client believes about the user's files, and the only things they need are a
//! size index of the present files and a [`Host`] for the rest. A test drives
//! them with a fake host and no engine, watcher, or server.
//!
//! The rename and the twin check are two functions rather than one decision
//! function: a target another row claimed is not retried with a second
//! candidate, it falls through to the twin check.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::hash::Sha1;
use crate::store::ItemRecord;

/// The present files a rename could have moved to, indexed by size (R5.2).
///
/// A rename keeps the content, so the size is the one cheap test that rules a
/// file out before it is read. The scan's order is kept, so the first match is
/// the one the earlier pass would have offered.
#[derive(Debug, Default)]
pub struct SizeIndex(HashMap<u64, Vec<PathBuf>>);

impl SizeIndex {
    /// Indexes the regular files a scan found, in the order it found them.
    ///
    /// A path whose metadata cannot be read, and one that is not a regular
    /// file, is dropped: neither can be a rename target (R5.1).
    #[must_use]
    pub fn new(present: &[PathBuf]) -> Self {
        let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
        for path in present {
            let Ok(metadata) = std::fs::symlink_metadata(path) else {
                continue;
            };
            if metadata.is_file() {
                by_size
                    .entry(metadata.len())
                    .or_default()
                    .push(path.clone());
            }
        }
        Self(by_size)
    }

    /// The candidates of one size, or nothing when no present file has it.
    fn candidates(&self, size: u64) -> &[PathBuf] {
        self.0.get(&size).map_or(&[], Vec::as_slice)
    }
}

/// What the decision needs from the world outside this module (R5.1, R5.2).
///
/// The engine answers from its store and the filesystem. A test answers from
/// maps, which is the point of the seam: the rules need no state database and
/// no engine to be decided.
pub trait Host {
    /// Whether an item already tracks this path. It is asked live, because a
    /// row moved earlier in the same pass has claimed its target (R5.2).
    fn is_tracked(&self, path: &Path) -> bool;

    /// Whether another tracked item holds the same content and still has its
    /// file on disk (R5.2, the second-copy acceptance line).
    fn has_present_twin(&self, row: &ItemRecord) -> bool;

    /// The SHA-1 of a file's content, or `None` when it cannot be read
    /// (R5.2).
    fn sha1(&self, path: &Path) -> Option<Sha1>;
}

/// What a row no rename target took is (R5.1, R5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// Another item still holds the content, so the asset needs no decision.
    Retired,
    /// The file is missing and waits for the user's decision.
    Missing,
}

/// The first present, untracked file whose size and content match `row`
/// (R5.2).
///
/// The target is not claimed here: the caller claims it by moving the row, and
/// a move the store refuses leaves the row undecided rather than trying the
/// next candidate.
#[must_use]
pub fn rename_target(row: &ItemRecord, index: &SizeIndex, host: &dyn Host) -> Option<PathBuf> {
    for path in index.candidates(row.size) {
        // A tracked path is another item's home, never a rename target: the
        // item it holds is its own.
        if host.is_tracked(path) {
            continue;
        }
        if host.sha1(path) == Some(row.sha1) {
            return Some(path.clone());
        }
    }
    None
}

/// What happens to a row no rename target took (R5.1, R5.2).
#[must_use]
pub fn fate(row: &ItemRecord, host: &dyn Host) -> Fate {
    if host.has_present_twin(row) {
        Fate::Retired
    } else {
        Fate::Missing
    }
}

/// Whether the tracked path is no longer a file (R5.1).
///
/// A path that cannot be read, and one that is now a directory or a symlink, is
/// no longer the file the item tracked. Both wait for a decision rather than
/// silently changing the server (ADR 0002).
#[must_use]
pub fn is_gone(path: &str) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => !metadata.is_file(),
        Err(_) => true,
    }
}
