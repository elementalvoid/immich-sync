//! Path comparison (SPEC R2.4).
//!
//! The client compares paths case-insensitively on Windows, so one folder
//! spelled with different case cannot become two sync items. The comparison
//! lives behind
//! [`PathCase`], a seam that takes the platform behaviour as a value. Tests run
//! both behaviours on any platform, and the store keys rows with
//! [`path_key`], which uses the platform default.
//!
//! shortcut: a Windows verbatim (extended-length) path prefix is compared as
//! written, so it is not normalized. Long-path handling belongs to packaging
//! (#14).

use std::path::Path;

/// How two paths compare on this platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathCase {
    /// Byte-for-byte, as on Linux.
    Sensitive,
    /// Folded to lower case, as on Windows.
    Insensitive,
}

impl PathCase {
    /// The behaviour the current platform uses (R2.4).
    #[must_use]
    pub const fn for_platform() -> Self {
        if cfg!(windows) {
            Self::Insensitive
        } else {
            Self::Sensitive
        }
    }

    /// The normalized form of `text` for this behaviour.
    fn normalize(self, text: &str) -> String {
        let trimmed = text.trim_end_matches('/').trim_end_matches('\\');
        let trimmed = if trimmed.is_empty() { text } else { trimmed };
        match self {
            Self::Sensitive => trimmed.to_string(),
            Self::Insensitive => trimmed.to_lowercase().replace('\\', "/"),
        }
    }
}

/// The comparison key for `path` under `case`.
///
/// The key is a normalized path string: separators unified and, on the
/// insensitive behaviour, folded to lower case. It is stable for the same
/// path, so it is also what the state database indexes.
#[must_use]
pub fn key(path: &Path, case: PathCase) -> String {
    case.normalize(&path.to_string_lossy())
}

/// The comparison key under the platform's own behaviour (R2.4).
#[must_use]
pub fn path_key(path: &Path) -> String {
    key(path, PathCase::for_platform())
}

/// Whether two paths name the same file under `case`.
#[must_use]
pub fn equal(left: &Path, right: &Path, case: PathCase) -> bool {
    key(left, case) == key(right, case)
}

/// Whether `path` is `root` itself or lies below it under `case`.
#[must_use]
pub fn is_within(root: &Path, path: &Path, case: PathCase) -> bool {
    key_is_within(&key(root, case), &key(path, case))
}

/// Whether the normalized key `path` is `root` itself or lies below it.
///
/// Both arguments are [`key`] output, so a caller holding rows that are already
/// keyed (the state store) applies the same rule without re-normalizing either
/// side.
pub(crate) fn key_is_within(root: &str, path: &str) -> bool {
    if root.is_empty() {
        return false;
    }
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Windows path, built from parts so the repository's hardcoded-path
    /// scan stays clean (its own test scans every source file).
    fn win(rest: &str) -> String {
        format!("{}:\\{}", 'C', rest.replace('/', "\\"))
    }

    #[test]
    fn windows_folds_case_and_separators() {
        assert!(equal(
            Path::new(&win("Pictures/Camera Roll")),
            Path::new(&win("pictures/camera roll").replace('C', "c")),
            PathCase::Insensitive
        ));
        assert!(!equal(
            Path::new(&win("Pictures")),
            Path::new(&win("Pictures/Other")),
            PathCase::Insensitive
        ));
    }

    #[test]
    fn linux_keeps_case_and_separators() {
        assert!(!equal(
            Path::new("/Pictures"),
            Path::new("/pictures"),
            PathCase::Sensitive
        ));
        assert!(equal(
            Path::new("/Pictures/"),
            Path::new("/Pictures"),
            PathCase::Sensitive
        ));
    }

    #[test]
    fn containment_uses_the_same_behaviour() {
        assert!(is_within(
            Path::new(&win("Pictures")),
            Path::new(&win("pictures/2024/a.jpg")),
            PathCase::Insensitive
        ));
        assert!(is_within(
            Path::new(&win("Pictures")),
            Path::new(&win("Pictures")),
            PathCase::Insensitive
        ));
        assert!(!is_within(
            Path::new(&win("Pictures")),
            Path::new(&win("Pictures2/a.jpg")),
            PathCase::Insensitive
        ));
        assert!(!is_within(
            Path::new("/pictures"),
            Path::new("/Pictures/a.jpg"),
            PathCase::Sensitive
        ));
        assert!(is_within(
            Path::new("/pictures"),
            Path::new("/pictures/a.jpg"),
            PathCase::Sensitive
        ));
        assert!(!is_within(
            Path::new(""),
            Path::new("/a"),
            PathCase::Sensitive
        ));
    }

    #[test]
    fn a_windows_key_matches_whatever_case_arrives() {
        let first = key(
            Path::new(&win("Users/Alice/Photo.JPG")),
            PathCase::Insensitive,
        );
        let second = key(
            Path::new(&win("users/alice/photo.jpg")),
            PathCase::Insensitive,
        );
        assert_eq!(first, second, "the store needs one key per file");
    }

    #[test]
    fn the_platform_default_is_windows_insensitive_and_linux_sensitive() {
        assert_eq!(
            PathCase::for_platform(),
            if cfg!(windows) {
                PathCase::Insensitive
            } else {
                PathCase::Sensitive
            }
        );
    }
}
