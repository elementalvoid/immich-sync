//! The reconciliation scan (SPEC R2.3): a full read of a source folder.
//!
//! The scan is the fallback that finds what watch events missed. It walks a
//! folder with `ignore` and applies the same skip predicate as the watcher
//! (R2.2), so both discovery paths agree on what a file is. A folder that has
//! disappeared is reported as such, never as a per-file error (R2.5).
//!
//! A folder the operating system refuses to read is reported as refused too: on
//! macOS the first read of Desktop, Documents, Downloads, and the volume roots
//! waits for the Files & Folders permission, and a denial lasts until the user
//! changes System Settings. Rendering it as an empty folder would hide the one
//! thing the user has to do (#65).

use std::path::PathBuf;

use ignore::WalkBuilder;

use crate::skip::{SkipReason, skip_reason_below};
use crate::store::SourceFolder;

/// What one reconciliation scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// The files the scan would sync, in walk order.
    pub files: Vec<PathBuf>,
    /// How many entries the skip rules refused. Only the unit tests read it,
    /// so a production build carries no dead counter.
    #[cfg(test)]
    pub skipped: usize,
}

/// Why a scan could not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanError {
    /// The source folder is not an accessible directory (R2.5).
    Missing(String),
    /// The operating system refused this process the folder (R2.5): the macOS
    /// Files & Folders permission, or a mode bit. It is not an empty folder.
    Denied(String),
    /// The walk itself failed.
    Walk(String),
}

/// The sentence a source folder the operating system refuses shows (R2.5).
///
/// macOS blocks the first read of a protected folder and keeps blocking it
/// until the user allows the app, so there the message names the switch that
/// lifts the denial. Anywhere else the refusal is a mode bit or a mount
/// setting, so the message asks for permissions without naming a macOS pane:
/// the same sentence on every platform would send a Linux user to System
/// Settings.
pub const fn permission_denied() -> &'static str {
    permission_denied_on(cfg!(target_os = "macos"))
}

/// The sentence for `macos`, split out so both arms are testable on any host.
const fn permission_denied_on(macos: bool) -> &'static str {
    if macos {
        "macOS is blocking access to this folder. Open System \
         Settings › Privacy & Security › Files & Folders and allow Immich Sync."
    } else {
        "This folder cannot be read. Check its permissions."
    }
}

/// The folder error a failed `read_dir` is, when the operating system refused
/// this process the folder (#65).
///
/// Any other failure is left to the missing-folder check, which has always
/// answered it.
fn read_denial(path: &str, error: &std::io::Error) -> Option<ScanError> {
    (error.kind() == std::io::ErrorKind::PermissionDenied)
        .then(|| ScanError::Denied(path.to_string()))
}

/// Walks one source folder and returns the files the sync should consider.
///
/// A non-recursive folder stops at its direct children. Hidden entries are
/// pruned by the walker (including the Windows hidden attribute); temporary,
/// system, and symlink entries are refused by the shared predicate.
///
/// # Errors
///
/// Returns [`ScanError::Denied`] when the operating system refuses this process
/// the folder, [`ScanError::Missing`] when the folder cannot be read as a
/// directory, and [`ScanError::Walk`] when the walker itself fails.
pub fn scan_folder(folder: &SourceFolder) -> Result<ScanReport, ScanError> {
    let root = std::path::Path::new(&folder.path);
    // #65: asked before the walk, because the walker reports a folder it may
    // not list as a folder with no files, and the dashboard would show an
    // empty one. Nothing retries a denial: it lasts until the user allows the
    // app in System Settings.
    if let Err(error) = std::fs::read_dir(root)
        && let Some(denied) = read_denial(&folder.path, &error)
    {
        return Err(denied);
    }
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
                #[cfg(test)]
                {
                    report.skipped += 1;
                }
                continue;
            }
        };
        let path = entry.path();
        let Some(file_type) = entry.file_type() else {
            #[cfg(test)]
            {
                report.skipped += 1;
            }
            continue;
        };
        if file_type.is_symlink() || !file_type.is_file() {
            if file_type.is_symlink() {
                #[cfg(test)]
                {
                    report.skipped += 1;
                }
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
            #[cfg(test)]
            {
                report.skipped += 1;
            }
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
            album_title: None,
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

    /// #65: a folder that is genuinely empty is an empty scan, never a denial.
    #[test]
    fn an_empty_folder_scans_as_empty_and_is_not_a_denial() {
        let temp = tempfile::tempdir().expect("create a temp dir");

        let report = scan_folder(&folder(temp.path(), true)).expect("an empty folder scans");

        assert!(report.files.is_empty(), "an empty folder holds no files");
    }

    /// #65: the refusal is its own folder error, and the message it renders is
    /// the platform's: the Files & Folders instruction on macOS, a plain
    /// permission sentence elsewhere. Both arms are decided here, so a Linux
    /// test covers what a macOS denial maps to.
    #[test]
    fn a_refused_folder_read_maps_to_a_denial_with_the_setting_to_change() {
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);

        assert_eq!(
            read_denial("/photos", &denied),
            Some(ScanError::Denied("/photos".to_string()))
        );
        assert_eq!(
            read_denial("/photos", &missing),
            None,
            "only a refusal is a denial; a missing folder keeps its own error"
        );

        let macos = permission_denied_on(true);
        assert!(
            macos.contains("System Settings › Privacy & Security › Files & Folders"),
            "the macOS message names the setting: {macos}"
        );
        assert!(
            macos.contains("Immich Sync"),
            "the macOS message names the app to allow: {macos}"
        );
        assert_eq!(
            permission_denied_on(false),
            "This folder cannot be read. Check its permissions.",
            "no other platform is told about a macOS setting"
        );
        assert_eq!(
            permission_denied(),
            permission_denied_on(cfg!(target_os = "macos")),
            "the platform picks its own sentence"
        );
    }

    /// The same rule against a directory this process really may not list.
    /// Running as root can list a mode-000 directory, so the test skips itself
    /// then and prints a `SKIP` line. The probe decides that, not the scan: a
    /// scan that succeeds is the regression this test exists for.
    #[cfg(unix)]
    #[test]
    fn a_mode_zero_folder_is_refused_as_a_denial_unless_run_as_root() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("create a temp dir");
        let locked = temp.path().join("locked");
        std::fs::create_dir(&locked).expect("create the folder");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("lock the folder");
        if !std::fs::read_dir(&locked).is_err() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
                .expect("unlock the folder");
            eprintln!(
                "SKIP a_mode_zero_folder_is_refused_as_a_denial_unless_run_as_root: \
                 this process lists a mode-000 directory, so it is root"
            );
            return;
        }
        let scanned = scan_folder(&folder(&locked, true));
        // Restored before the tempdir drops, so its cleanup can empty it.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("unlock the folder");

        assert_eq!(
            scanned,
            Err(ScanError::Denied(locked.display().to_string())),
            "a folder this process may not list is a denial, not an empty folder"
        );
    }
}
