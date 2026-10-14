//! The default skip rules (SPEC R2.2).
//!
//! Hidden files and folders, symlinks and junctions, temporary and partial
//! files, system files, and XMP sidecars never reach the state store or the
//! wire. The reconciliation scan and the watcher both call
//! [`skip_reason_below`], so the two discovery paths agree on what a file is.
//! A sidecar is skipped because it is metadata for its asset rather than an
//! asset of its own (R3.3); it rides along in the asset's own upload.
//!
//! The `ignore` walker also prunes hidden entries while it walks, and this
//! predicate repeats the check for watcher events, which arrive as bare paths.

use std::fs::Metadata;
use std::path::{Component, Path};

use crate::pairing::is_sidecar_name;
use crate::paths::{PathCase, key};

/// Why a path is skipped (R2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// The file or a parent folder name starts with `.`, or carries the
    /// Windows hidden attribute.
    Hidden,
    /// The path is a symlink, a junction, or a Windows reparse point.
    Symlink,
    /// A temporary or partial file, such as an editor swap file or a
    /// `.crdownload` in progress.
    Temporary,
    /// An operating-system file, such as `Thumbs.db` or a page file.
    System,
    /// An XMP sidecar, which travels with its asset (R3.3).
    Sidecar,
}

impl SkipReason {
    /// The stable name for a log line or a test message.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hidden => "hidden",
            Self::Symlink => "symlink",
            Self::Temporary => "temporary",
            Self::System => "system",
            Self::Sidecar => "sidecar",
        }
    }
}

/// The lower-case names of operating-system files that never upload.
const SYSTEM_NAMES: [&str; 10] = [
    "thumbs.db",
    "desktop.ini",
    ".ds_store",
    "$recycle.bin",
    "system volume information",
    "pagefile.sys",
    "hiberfil.sys",
    "swapfile.sys",
    "@eadir",
    "icon\r",
];

/// The lower-case extensions of temporary and partial files.
const TEMPORARY_EXTENSIONS: [&str; 8] = [
    "tmp",
    "temp",
    "part",
    "partial",
    "crdownload",
    "download",
    "swp",
    "swo",
];

/// Whether a bare (single-component) name is skipped, and why.
#[must_use]
pub fn skip_name(name: &str) -> Option<SkipReason> {
    if name.starts_with('.') {
        return Some(SkipReason::Hidden);
    }
    let lower = name.to_lowercase();
    if SYSTEM_NAMES.contains(&lower.as_str()) {
        return Some(SkipReason::System);
    }
    // Office lock files and editor backup files.
    if lower.starts_with("~$") || lower.ends_with('~') {
        return Some(SkipReason::Temporary);
    }
    if let Some((_, extension)) = lower.rsplit_once('.')
        && TEMPORARY_EXTENSIONS.contains(&extension)
    {
        return Some(SkipReason::Temporary);
    }
    None
}

/// Whether `path`, below `root`, is skipped, and why.
///
/// Every component between `root` and the file is checked, so a hidden or
/// system folder prunes its whole subtree the same way the scan walker's
/// hidden filter does. The root itself is never checked: the user selected it.
///
/// `metadata` is the platform metadata when the caller already read it; the
/// Windows hidden attribute is only visible there.
#[must_use]
pub fn skip_reason_below(
    root: &Path,
    path: &Path,
    metadata: Option<&Metadata>,
) -> Option<SkipReason> {
    if metadata.is_some_and(is_symlink_metadata) {
        return Some(SkipReason::Symlink);
    }
    if metadata.is_some_and(is_hidden_attribute) {
        return Some(SkipReason::Hidden);
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    for component in relative.components() {
        if let Component::Normal(name) = component
            && let Some(reason) = skip_name(&name.to_string_lossy())
        {
            return Some(reason);
        }
    }
    // R3.3: only the file itself can be a sidecar; a folder named `album.xmp`
    // is an ordinary folder and its contents are ordinary files.
    path.file_name()
        .is_some_and(|name| is_sidecar_name(&name.to_string_lossy()))
        .then_some(SkipReason::Sidecar)
}

/// The path below `root` as a forward-slash string, or `None` when `path` is
/// not below `root` under `case`.
///
/// `Path::strip_prefix` compares case-sensitively, so both sides go through
/// [`key`] the way [`crate::paths::is_within`] does: a stored folder spelling
/// that differs only in case still covers the same files on Windows (R2.4).
fn relative_below(root: &Path, path: &Path, case: PathCase) -> Option<String> {
    let root_key = key(root, case);
    let path_key = key(path, case);
    if path_key == root_key {
        return Some(String::new());
    }
    path_key
        .strip_prefix(&root_key)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(str::to_string)
}

/// Whether any directory between `root` (exclusive) and `path` itself is a
/// symlink or reparse point (R2.2).
///
/// The watcher sees a file reported under a symlinked directory as an ordinary
/// file, because `symlink_metadata` follows the intermediate components. The
/// reconciliation scan prunes symlinked directories instead, so this keeps the
/// two discovery paths agreeing on what a file is. `root` is the user's own
/// choice and is never checked.
#[must_use]
pub fn has_symlink_ancestor(root: &Path, path: &Path) -> bool {
    let Some(relative) = relative_below(root, path, PathCase::for_platform()) else {
        return false;
    };
    let components: Vec<&str> = relative
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let mut current = root.to_path_buf();
    // The last component is the file itself; only its ancestors matter.
    for component in components.iter().take(components.len().saturating_sub(1)) {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if is_symlink_metadata(&metadata) => return true,
            Ok(_) => {}
            // A missing ancestor cannot be a symlink; the file's own read is
            // what reports the missing path.
            Err(_) => return false,
        }
    }
    false
}

/// Whether plain metadata describes a symlink or a Windows reparse point.
///
/// The caller passes the metadata of `std::fs::symlink_metadata`, which does
/// not follow the link for a symlink itself.
#[must_use]
pub fn is_symlink_metadata(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    false
}

/// Whether metadata carries the Windows hidden attribute.
#[must_use]
pub fn is_hidden_attribute(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn metadata(path: &Path) -> Metadata {
        std::fs::symlink_metadata(path).expect("read the fixture metadata")
    }

    #[test]
    fn hidden_names_are_skipped() {
        assert_eq!(skip_name(".hidden.jpg"), Some(SkipReason::Hidden));
        assert_eq!(skip_name("visible.jpg"), None);
    }

    #[test]
    fn temporary_and_partial_files_are_skipped() {
        for name in [
            "photo.tmp",
            "photo.TMP",
            "photo.part",
            "photo.crdownload",
            "edit.swp",
            "report~",
            "~$report.docx",
        ] {
            assert_eq!(skip_name(name), Some(SkipReason::Temporary), "{name}");
        }
        assert_eq!(skip_name("photo.jpg"), None);
        assert_eq!(
            skip_name("photo.tmpx"),
            None,
            "only the exact extension is temporary"
        );
    }

    #[test]
    fn system_files_are_skipped() {
        for name in [
            "Thumbs.db",
            "thumbs.DB",
            "desktop.ini",
            ".DS_Store",
            "$RECYCLE.BIN",
            "System Volume Information",
            "pagefile.sys",
        ] {
            assert!(
                matches!(
                    skip_name(name),
                    Some(SkipReason::System) | Some(SkipReason::Hidden)
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn a_sidecar_is_skipped_because_it_is_not_an_asset_of_its_own() {
        // R3.3: the sidecar rides along with IMG_0001.jpg, so discovery must
        // not enqueue it as a second item.
        let root = Path::new("/photos");
        assert_eq!(
            skip_reason_below(root, &PathBuf::from("/photos/IMG_0001.jpg.xmp"), None),
            Some(SkipReason::Sidecar)
        );
        assert_eq!(
            skip_reason_below(root, &PathBuf::from("/photos/IMG_0001.XMP"), None),
            Some(SkipReason::Sidecar)
        );
        assert_eq!(
            skip_reason_below(root, &PathBuf::from("/photos/IMG_0001.jpg"), None),
            None
        );
        assert_eq!(
            skip_reason_below(root, &PathBuf::from("/photos/album.xmp/inside.jpg"), None),
            None,
            "a folder that happens to end in .xmp prunes nothing"
        );
    }

    #[test]
    fn a_hidden_folder_prunes_its_subtree() {
        let root = Path::new("/photos");
        assert_eq!(
            skip_reason_below(root, &PathBuf::from("/photos/.cache/photo.jpg"), None),
            Some(SkipReason::Hidden)
        );
        assert_eq!(
            skip_reason_below(root, &PathBuf::from("/photos/a.jpg"), None),
            None
        );
    }

    #[test]
    fn the_selected_root_is_never_itself_skipped() {
        // A user may select a folder whose name looks hidden; the rule applies
        // below the root, not to the choice.
        assert_eq!(
            skip_reason_below(
                Path::new("/photos/.hidden"),
                Path::new("/photos/.hidden/visible.jpg"),
                None
            ),
            None
        );
    }

    #[test]
    fn a_root_spelled_with_a_different_case_still_covers_its_children() {
        // R2.2: the stored folder spelling may differ in case from the path
        // the walker reports; the symlink check must not fail open.
        assert_eq!(
            relative_below(
                Path::new("/photos"),
                Path::new("/Photos/album/photo.jpg"),
                PathCase::Insensitive,
            )
            .as_deref(),
            Some("album/photo.jpg"),
            "the folded root must still find the file's ancestors"
        );
    }

    #[test]
    fn a_file_under_a_symlinked_directory_has_a_symlink_ancestor() {
        let directory = tempfile::tempdir().expect("create a temp dir");
        let real = directory.path().join("real");
        std::fs::create_dir_all(&real).expect("create the fixture directory");
        let file = real.join("photo.jpg");
        std::fs::write(&file, b"bytes").expect("write the fixture");
        let link = directory.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("create the fixture symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&real, &link).expect("create the fixture symlink");

        assert!(
            has_symlink_ancestor(directory.path(), &link.join("photo.jpg")),
            "R2.2: a file below a symlinked directory is refused"
        );
        assert!(
            !has_symlink_ancestor(directory.path(), &file),
            "a real directory chain is not refused"
        );
    }

    #[test]
    fn a_symlink_is_skipped_even_with_a_visible_name() {
        let directory = tempfile::tempdir().expect("create a temp dir");
        let real = directory.path().join("real.jpg");
        std::fs::write(&real, b"bytes").expect("write the fixture");
        let link = directory.path().join("link.jpg");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("create the fixture symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&real, &link).expect("create the fixture symlink");

        let found = metadata(&link);
        assert_eq!(
            skip_reason_below(directory.path(), &link, Some(&found)),
            Some(SkipReason::Symlink)
        );
        assert_eq!(
            skip_reason_below(directory.path(), &real, Some(&metadata(&real))),
            None
        );
    }
}
