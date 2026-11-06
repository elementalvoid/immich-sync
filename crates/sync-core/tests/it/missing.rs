//! The missing-file rules on their own (SPEC R5.1, R5.2): a fake host and real
//! temp files, with no engine, watcher, or state database.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sync_core::hash::{Sha1, sha1, sha1_file};
use sync_core::missing::{Fate, Host, SizeIndex, fate, rename_target};
use sync_core::store::{ItemRecord, ItemState};

/// A [`Host`] that answers from maps, which is what the seam is for: the rules
/// are decided without a store or a server.
#[derive(Default)]
struct FakeHost {
    /// The item each tracked path homes, by path (R5.2).
    tracked: HashMap<PathBuf, String>,
    /// The present twin a row's content has, by row path (R5.2).
    twins: HashMap<String, String>,
    /// Every path the rules asked to hash, in order.
    hashed: RefCell<Vec<PathBuf>>,
}

impl FakeHost {
    /// Records that another item already homes `path`.
    fn claim(&mut self, path: &Path, item: &str) {
        self.tracked.insert(path.to_path_buf(), item.to_string());
    }

    /// The paths the rules asked to hash, in the order they asked.
    fn hashed(&self) -> Vec<PathBuf> {
        self.hashed.borrow().clone()
    }
}

impl Host for FakeHost {
    fn is_tracked(&self, path: &Path) -> bool {
        self.tracked.contains_key(path)
    }

    fn has_present_twin(&self, row: &ItemRecord) -> bool {
        self.twins.contains_key(&row.path)
    }

    fn sha1(&self, path: &Path) -> Option<Sha1> {
        self.hashed.borrow_mut().push(path.to_path_buf());
        sha1_file(path).ok()
    }
}

/// Writes one fixture file and returns its path.
fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write a fixture");
    path
}

/// One store row for the content `bytes`; the row's own file may be gone.
fn row(path: &Path, bytes: &[u8]) -> ItemRecord {
    ItemRecord {
        path: path.display().to_string(),
        size: u64::try_from(bytes.len()).expect("a fixture fits in u64"),
        mtime_ns: 1_700_000_000_000_000_000,
        sha1: sha1(bytes),
        asset_id: Some("asset-1".to_string()),
        state: ItemState::Uploaded,
        error_class: None,
        error_message: None,
        attempts: 0,
        sidecar: None,
    }
}

#[test]
fn a_rename_target_is_the_first_content_match() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let dir = temp.path();
    // The same size, other bytes: the size bucket offers it, the content
    // rules it out.
    let other = write(dir, "z-not-it.jpg", b"zzzzzzzzz");
    let first = write(dir, "y-match.jpg", b"the bytes");
    let second = write(dir, "a-match.jpg", b"the bytes");
    let gone = row(&dir.join("gone.jpg"), b"the bytes");
    let index = SizeIndex::new(&[other, first.clone(), second]);
    let host = FakeHost::default();

    assert_eq!(
        rename_target(&gone, &index, &host),
        Some(first),
        "R5.2: the first content match in the scan's order wins"
    );
}

#[test]
fn a_tracked_path_is_never_a_rename_target() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let dir = temp.path();
    let tracked = write(dir, "tracked.jpg", b"the bytes");
    let free = write(dir, "free.jpg", b"the bytes");
    let gone = row(&dir.join("gone.jpg"), b"the bytes");
    let index = SizeIndex::new(&[tracked.clone(), free.clone()]);
    let mut host = FakeHost::default();
    host.claim(&tracked, "another item");

    assert_eq!(
        rename_target(&gone, &index, &host),
        Some(free.clone()),
        "R5.2: another item's home is skipped and the next match answers"
    );
    assert_eq!(
        host.hashed(),
        vec![free],
        "a tracked path is never read as a rename candidate"
    );
}

#[test]
fn a_size_bucket_is_the_only_bucket_hashed() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let dir = temp.path();
    let small = write(dir, "small.jpg", b"xy");
    let large = write(dir, "large.jpg", b"0123456789012");
    let gone = row(&dir.join("gone.jpg"), b"bucket");
    let host = FakeHost::default();

    let mut present = vec![small, large];
    let index = SizeIndex::new(&present);
    assert_eq!(
        rename_target(&gone, &index, &host),
        None,
        "no present file has the row's size"
    );
    assert!(
        host.hashed().is_empty(),
        "R5.2: a file outside the row's size bucket is never read"
    );

    let same_size = write(dir, "same-size.jpg", b"other!");
    present.push(same_size.clone());
    let index = SizeIndex::new(&present);
    assert_eq!(
        rename_target(&gone, &index, &host),
        None,
        "the same size with other content is no target"
    );
    assert_eq!(
        host.hashed(),
        vec![same_size],
        "only the row's size bucket is hashed"
    );
}

#[test]
fn an_unreadable_candidate_is_skipped() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let dir = temp.path();
    let broken = write(dir, "broken.jpg", b"the bytes");
    let good = write(dir, "good.jpg", b"the bytes");
    let gone = row(&dir.join("gone.jpg"), b"the bytes");
    let index = SizeIndex::new(&[broken.clone(), good.clone()]);
    std::fs::remove_file(&broken).expect("delete the fixture");
    let host = FakeHost::default();

    assert_eq!(
        rename_target(&gone, &index, &host),
        Some(good.clone()),
        "R5.2: an unreadable candidate is skipped and the next one answers"
    );
    assert_eq!(
        host.hashed(),
        vec![broken, good],
        "the unreadable candidate was tried first and gave no digest"
    );
}

#[test]
fn a_claimed_target_is_not_offered_twice() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let dir = temp.path();
    let target = write(dir, "target.jpg", b"the bytes");
    let gone = row(&dir.join("gone.jpg"), b"the bytes");
    let index = SizeIndex::new(std::slice::from_ref(&target));
    let mut host = FakeHost::default();

    assert_eq!(
        rename_target(&gone, &index, &host),
        Some(target.clone()),
        "R5.2: the first row claims the target"
    );
    // What the caller's move does: the target becomes another item's home.
    host.claim(&target, &gone.path);
    assert_eq!(
        rename_target(&gone, &index, &host),
        None,
        "R5.2: a claimed target is not offered a second time"
    );
}

#[test]
fn a_present_twin_retires_the_row() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let twin = write(temp.path(), "twin.jpg", b"the bytes");
    let gone = row(&temp.path().join("gone.jpg"), b"the bytes");
    let mut host = FakeHost::default();
    host.twins
        .insert(gone.path.clone(), twin.display().to_string());

    assert_eq!(
        fate(&gone, &host),
        Fate::Retired,
        "R5.2: a twin that still has its file holds the content"
    );
}

#[test]
fn no_twin_means_missing() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let gone = row(&temp.path().join("gone.jpg"), b"the bytes");
    let host = FakeHost::default();

    assert_eq!(
        fate(&gone, &host),
        Fate::Missing,
        "R5.1: with no twin the file is missing and waits for a review"
    );
}

#[test]
fn a_directory_is_not_a_candidate() {
    let temp = tempfile::tempdir().expect("create a temp dir");
    let dir = temp.path().join("a-directory");
    std::fs::create_dir(&dir).expect("create a directory");
    let size = std::fs::symlink_metadata(&dir)
        .expect("read the directory")
        .len();
    let gone = ItemRecord {
        size,
        ..row(&temp.path().join("gone.jpg"), b"the bytes")
    };
    let index = SizeIndex::new(std::slice::from_ref(&dir));
    let host = FakeHost::default();

    assert_eq!(
        rename_target(&gone, &index, &host),
        None,
        "R5.1: a directory is not a file and cannot be a rename target"
    );
    assert!(
        host.hashed().is_empty(),
        "the directory is dropped before anything is read"
    );
}
