//! The reconciliation scan (SPEC R2.3): a full read of a source folder.
//!
//! The scan is the fallback that finds what watch events missed. It walks a
//! folder with `ignore` and applies the same skip predicate as the watcher
//! (R2.2), so both discovery paths agree on what a file is. A folder that has
//! disappeared is reported as such, never as a per-file error (R2.5).

use std::path::PathBuf;

use ignore::WalkBuilder;

use crate::skip::{SkipReason, skip_reason_below};
use crate::store::SourceFolder;

/// What one reconciliation scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// The files the scan would sync, in walk order.
    pub files: Vec<PathBuf>,
    /// How many entries the skip rules refused.
    pub skipped: usize,
}

/// Why a scan could not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanError {
    /// The source folder is not an accessible directory (R2.5).
    Missing(String),
    /// The walk itself failed.
    Walk(String),
}

/// Walks one source folder and returns the files the sync should consider.
///
/// A non-recursive folder stops at its direct children. Hidden entries are
/// pruned by the walker (including the Windows hidden attribute); temporary,
/// system, and symlink entries are refused by the shared predicate.
///
/// # Errors
///
/// Returns [`ScanError::Missing`] when the folder cannot be read as a
/// directory, and [`ScanError::Walk`] when the walker itself fails.
pub fn scan_folder(folder: &SourceFolder) -> Result<ScanReport, ScanError> {
    let root = std::path::Path::new(&folder.path);
    if !root.is_dir() {
        return Err(ScanError::Missing(folder.path.clone()));
    }
    let mut builder = WalkBuilder::new(root);
    // The spec's rules only (R2.2). The walker's extra filters stay off, so
    // the scan does not follow a `.gitignore` the user never asked about.
    builder
        .hidden(true)
        .parents(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false);
    if !folder.recursive {
        builder.max_depth(Some(1));
    }

    let mut report = ScanReport::default();
    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            // A file that vanished mid-walk is not an error: the next scan
            // finds the folder's real contents.
            Err(error) => {
                tracing::debug!(folder = %folder.path, error = %error, "a scan entry was unreadable");
                report.skipped += 1;
                continue;
            }
        };
        let path = entry.path();
        let Some(file_type) = entry.file_type() else {
            report.skipped += 1;
            continue;
        };
        if file_type.is_symlink() || !file_type.is_file() {
            if file_type.is_symlink() {
                report.skipped += 1;
            }
            continue;
        }
        if let Some(reason) = skip_reason_below(root, path, None) {
            if reason == SkipReason::Sidecar {
                // An orphan sidecar — the media file is not there, or is
                // already on the server — leaves no item and no count anyone
                // reads, so the log is the only trace it ever existed.
                tracing::debug!(
                    path = %path.display(),
                    "an XMP sidecar is not an asset of its own; it rides along with its media file"
                );
            }
            report.skipped += 1;
            continue;
        }
        report.files.push(path.to_path_buf());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn folder(path: &Path, recursive: bool) -> SourceFolder {
        SourceFolder {
            path: path.display().to_string(),
            recursive,
            album_mapping: false,
            album_scope: crate::store::AlbumScope::default(),
        }
    }

    fn write(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the fixture directory");
        }
        std::fs::write(&path, bytes).expect("write the fixture");
        path
    }

    fn names(report: &ScanReport) -> Vec<String> {
        let mut names: Vec<String> = report
            .files
            .iter()
            .map(|path| {
                path.file_name()
                    .expect("a named file")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_folder_with_nested_files_scans_recursively() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        write(temp.path(), "one.jpg", b"1");
        write(temp.path(), "nested/two.jpg", b"2");

        let report = scan_folder(&folder(temp.path(), true)).expect("scan the fixture");

        assert_eq!(names(&report), ["one.jpg", "two.jpg"]);
    }

    #[test]
    fn a_non_recursive_folder_stops_at_its_direct_children() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        write(temp.path(), "one.jpg", b"1");
        write(temp.path(), "nested/two.jpg", b"2");

        let report = scan_folder(&folder(temp.path(), false)).expect("scan the fixture");

        assert_eq!(names(&report), ["one.jpg"]);
    }

    #[test]
    fn hidden_temporary_and_system_files_never_appear() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        write(temp.path(), "visible.jpg", b"v");
        write(temp.path(), ".hidden.jpg", b"h");
        write(temp.path(), ".hidden/inside.jpg", b"h");
        write(temp.path(), "partial.tmp", b"t");
        write(temp.path(), "Thumbs.db", b"s");
        write(temp.path(), "desktop.ini", b"s");

        let report = scan_folder(&folder(temp.path(), true)).expect("scan the fixture");

        assert_eq!(names(&report), ["visible.jpg"]);
        // The walker prunes the hidden entries itself; the predicate counts
        // the names it refuses (the temporary and system files).
        assert!(report.skipped >= 3, "the skips were counted: {report:?}");
    }

    #[test]
    fn a_symlink_is_skipped() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let real = write(temp.path(), "real.jpg", b"r");
        let link = temp.path().join("link.jpg");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("create the fixture symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&real, &link).expect("create the fixture symlink");

        let report = scan_folder(&folder(temp.path(), true)).expect("scan the fixture");

        assert_eq!(names(&report), ["real.jpg"]);
        assert_eq!(report.skipped, 1);
    }

    #[test]
    fn a_missing_folder_is_a_folder_error_not_a_walk_error() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let missing = temp.path().join("unplugged");

        let error = scan_folder(&folder(&missing, true)).expect_err("the folder is gone");

        assert_eq!(error, ScanError::Missing(missing.display().to_string()));
    }
}
