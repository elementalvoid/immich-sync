//! The pairing rules for XMP sidecars and live photos (SPEC R3.3).
//!
//! Two rules run before an upload leaves the client:
//!
//! * **Sidecar.** An XMP file belongs to the asset next to it. Immich's own
//!   naming is [`FolderIndex::sidecar_for`]: `IMG_0001.jpg.xmp` is preferred,
//!   `IMG_0001.xmp` is the fallback, and a sidecar named for another extension
//!   (`IMG_0001.jpeg.xmp` for `IMG_0001.jpg`) is not recognized
//!   ([xmp-sidecars](https://github.com/immich-app/immich/blob/v3.3.1/docs/docs/features/xmp-sidecars.md)).
//!   A sidecar rides along as `sidecarData` and is never a sync item of its
//!   own, so [`is_sidecar_name`] is the skip rule the scan and the watcher
//!   share.
//! * **Live photo.** A photo and a motion video with the same stem in the same
//!   folder are one live photo. The video uploads first and the photo links it
//!   with `livePhotoVideoId`; [`FolderIndex::live_photo_video_for`] finds it.
//!
//! Both rules compare names under [`PathCase`], so a case-only spelling
//! difference is one file on Windows (R2.4).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::paths::{PathCase, equal, key};

/// The extensions that count as a live photo's motion video (R3.3).
///
/// `.mov` is what an iPhone writes and `.mp4` what Android and most cameras
/// write. An Android *motion photo* is one file with the video inside it; it
/// needs no pairing and is an ordinary asset here.
pub const LIVE_PHOTO_VIDEO_EXTENSIONS: [&str; 2] = ["mov", "mp4"];

/// The extension of an XMP sidecar.
const SIDECAR_EXTENSION: &str = "xmp";

/// Whether the extension of `name` is `extension`, compared without case.
///
/// The pairing rules run on names the filesystem accepted, so the comparison
/// must not turn on how the name happens to be spelled.
fn has_extension(name: &OsStr, extension: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|found| found.eq_ignore_ascii_case(extension))
}

/// Whether `name` is an XMP sidecar: `IMG_0001.jpg.xmp` and `IMG_0001.xmp`
/// both are, `IMG_0001.jpg` is not.
///
/// A sidecar is metadata for its asset, not an asset (R3.3), so discovery skips
/// it and the upload carries it.
#[must_use]
pub fn is_sidecar_name(name: &str) -> bool {
    has_extension(OsStr::new(name), SIDECAR_EXTENSION)
}

/// Whether `sidecar` is one of the two names Immich recognizes for `media`
/// (R3.3).
///
/// The sidecar's own stem is the media name: `IMG_0001.jpg.xmp` strips to
/// `IMG_0001.jpg`, the preferred form, and `IMG_0001.xmp` strips to `IMG_0001`,
/// the fallback. Compared under `case` (R2.4).
#[must_use]
pub fn is_sidecar_of(media: &Path, sidecar: &Path, case: PathCase) -> bool {
    if !has_extension(sidecar.file_name().unwrap_or_default(), SIDECAR_EXTENSION) {
        return false;
    }
    let (Some(media_name), Some(media_stem), Some(sidecar_stem)) =
        (media.file_name(), media.file_stem(), sidecar.file_stem())
    else {
        return false;
    };
    let sidecar_stem = sidecar.with_file_name(sidecar_stem);
    equal(&media.with_file_name(media_name), &sidecar_stem, case)
        || equal(&media.with_file_name(media_stem), &sidecar_stem, case)
}

/// Whether two paths sit in one folder and have the same stem, compared under
/// `case` (R2.4).
#[must_use]
pub fn same_stem(left: &Path, right: &Path, case: PathCase) -> bool {
    match (left.file_stem(), right.file_stem()) {
        (Some(left_stem), Some(right_stem)) => equal(
            &left.with_file_name(left_stem),
            &right.with_file_name(right_stem),
            case,
        ),
        _ => false,
    }
}

/// The files of the folders one batch discovered, read once per folder.
///
/// Pairing needs the folder's other files, so the engine reads each folder
/// once per batch and asks this index, instead of asking the filesystem once
/// per candidate name. A folder that cannot be read yields no candidates,
/// which leaves the media file an ordinary asset rather than losing it.
#[derive(Debug, Default)]
pub struct FolderIndex {
    folders: HashMap<String, Vec<PathBuf>>,
}

impl FolderIndex {
    /// Reads the files of every folder `paths` sit in.
    #[must_use]
    pub fn read(paths: &[PathBuf]) -> Self {
        let mut index = Self::default();
        for path in paths {
            let Some(folder) = path.parent() else {
                continue;
            };
            let folder_key = key(folder, PathCase::for_platform());
            if index.folders.contains_key(&folder_key) {
                continue;
            }
            index.folders.insert(folder_key, read_files(folder));
        }
        index
    }

    /// Builds an index of folders whose files are already known, so the rules
    /// can be tested without a filesystem.
    #[must_use]
    pub fn known(folders: impl IntoIterator<Item = (PathBuf, Vec<PathBuf>)>) -> Self {
        let mut index = Self::default();
        for (folder, files) in folders {
            index
                .folders
                .insert(key(&folder, PathCase::for_platform()), files);
        }
        index
    }

    /// The XMP sidecar of `media` that exists, Immich's preferred name first
    /// (R3.3).
    ///
    /// The media file's own name is what makes the pairing: the sidecar sits
    /// in the same folder and is named after the media file.
    #[must_use]
    pub fn sidecar_for(&self, media: &Path, case: PathCase) -> Option<&Path> {
        let files = self.files_in(media)?;
        let mut fallback = None;
        for candidate in files {
            if !is_sidecar_of(media, candidate, case) {
                continue;
            }
            // `IMG_0001.jpg.xmp` is preferred over `IMG_0001.xmp`, whatever
            // order the folder listed them in.
            if candidate.file_stem() == media.file_name() {
                return Some(candidate);
            }
            fallback.get_or_insert(candidate.as_path());
        }
        fallback
    }

    /// The live photo video paired with `photo` (R3.3), preferring
    /// [`LIVE_PHOTO_VIDEO_EXTENSIONS`] order over the order the folder listed
    /// its files in, so the pairing does not depend on the filesystem.
    ///
    /// The media file itself is never its own video: a `.mov` asked for its
    /// own pairing finds nothing, so a live photo video is never uploaded as
    /// the photo's own video.
    #[must_use]
    pub fn live_photo_video_for(&self, photo: &Path, case: PathCase) -> Option<&Path> {
        let files = self.files_in(photo)?;
        LIVE_PHOTO_VIDEO_EXTENSIONS.iter().find_map(|extension| {
            files
                .iter()
                .find(|candidate| {
                    !equal(candidate, photo, case)
                        && has_extension(candidate.file_name().unwrap_or_default(), extension)
                        && same_stem(photo, candidate, case)
                })
                .map(PathBuf::as_path)
        })
    }

    /// The indexed files of the folder `path` sits in.
    fn files_in(&self, path: &Path) -> Option<&[PathBuf]> {
        let folder = path.parent()?;
        self.folders
            .get(&key(folder, PathCase::for_platform()))
            .map(Vec::as_slice)
    }
}

/// The files of one folder, or none when it cannot be read.
fn read_files(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The files of `/photos`, as the engine would read them.
    fn index_of(names: &[&str]) -> FolderIndex {
        let files: Vec<PathBuf> = names
            .iter()
            .map(|name| PathBuf::from(format!("/photos/{name}")))
            .collect();
        FolderIndex::known([(PathBuf::from("/photos"), files)])
    }

    fn photo() -> PathBuf {
        PathBuf::from("/photos/IMG_0001.jpg")
    }

    #[test]
    fn the_full_name_sidecar_is_the_one_immich_prefers() {
        let index = index_of(&["IMG_0001.jpg", "IMG_0001.jpg.xmp", "IMG_0001.xmp"]);

        assert_eq!(
            index.sidecar_for(&photo(), PathCase::Sensitive),
            Some(Path::new("/photos/IMG_0001.jpg.xmp")),
            "R3.3: IMG_0001.jpg.xmp wins over the fallback"
        );
    }

    #[test]
    fn the_stem_sidecar_is_the_fallback() {
        let index = index_of(&["IMG_0001.jpg", "IMG_0001.xmp"]);

        assert_eq!(
            index.sidecar_for(&photo(), PathCase::Sensitive),
            Some(Path::new("/photos/IMG_0001.xmp"))
        );
    }

    #[test]
    fn a_sidecar_for_another_extension_does_not_pair() {
        // R3.3: Immich does not recognize `IMG_0001.jpeg.xmp` for
        // `IMG_0001.jpg`, and neither does this client.
        let index = index_of(&["IMG_0001.jpg", "IMG_0001.jpeg.xmp"]);

        assert_eq!(index.sidecar_for(&photo(), PathCase::Sensitive), None);
        let other = PathBuf::from("/photos/IMG_0001.jpeg");
        assert_eq!(
            index.sidecar_for(&other, PathCase::Sensitive),
            Some(Path::new("/photos/IMG_0001.jpeg.xmp")),
            "the same sidecar does pair with the media file it names"
        );
    }

    #[test]
    fn a_name_that_is_not_a_sidecar_never_pairs() {
        let index = index_of(&["IMG_0001.jpg", "IMG_0001.jpg.aae"]);

        assert_eq!(index.sidecar_for(&photo(), PathCase::Sensitive), None);
        assert!(is_sidecar_name("IMG_0001.jpg.xmp"));
        assert!(is_sidecar_name("IMG_0001.XMP"));
        assert!(!is_sidecar_name("IMG_0001.jpg"));
    }

    #[test]
    fn a_case_variant_pairs_under_the_windows_rule_only() {
        // R2.4 + R3.3: on Windows the two spellings are one file, so the
        // sidecar pairs. On Linux they are two files and nothing pairs.
        let index = index_of(&["IMG_0001.JPG", "img_0001.jpg.xmp"]);

        assert_eq!(
            index.sidecar_for(&photo(), PathCase::Insensitive),
            Some(Path::new("/photos/img_0001.jpg.xmp")),
            "R2.4: the Windows rule folds case"
        );
        assert_eq!(index.sidecar_for(&photo(), PathCase::Sensitive), None);
    }

    #[test]
    fn a_media_file_with_no_sidecar_has_none() {
        let index = index_of(&["IMG_0001.jpg", "IMG_0002.jpg", "IMG_0002.jpg.xmp"]);

        assert_eq!(index.sidecar_for(&photo(), PathCase::Sensitive), None);
    }

    #[test]
    fn the_video_sharing_a_stem_is_the_live_photos_video() {
        let index = index_of(&["IMG_0001.jpg", "IMG_0001.mov"]);

        assert_eq!(
            index.live_photo_video_for(&photo(), PathCase::Sensitive),
            Some(Path::new("/photos/IMG_0001.mov"))
        );
    }

    #[test]
    fn both_motion_video_extensions_pair() {
        for (name, video) in [
            ("IMG_0001.jpg", "IMG_0001.mov"),
            ("IMG_0001.jpg", "IMG_0001.mp4"),
            ("IMG_0001.jpg", "IMG_0001.MP4"),
        ] {
            let index = index_of(&[name, video]);
            assert_eq!(
                index.live_photo_video_for(&photo(), PathCase::Sensitive),
                Some(Path::new(&format!("/photos/{video}"))),
                "{video} pairs with {name}"
            );
        }
    }

    #[test]
    fn a_mov_wins_over_an_mp4_of_the_same_stem() {
        // The listing order is the filesystem's, so the rule must not depend
        // on it.
        let index = index_of(&["IMG_0001.jpg", "IMG_0001.mp4", "IMG_0001.mov"]);

        assert_eq!(
            index.live_photo_video_for(&photo(), PathCase::Sensitive),
            Some(Path::new("/photos/IMG_0001.mov"))
        );
    }

    #[test]
    fn another_stem_or_another_extension_is_not_the_live_photos_video() {
        assert_eq!(
            index_of(&["IMG_0001.jpg", "IMG_0002.mov"])
                .live_photo_video_for(&photo(), PathCase::Sensitive),
            None,
            "a different stem does not pair"
        );
        assert_eq!(
            index_of(&["IMG_0001.jpg", "IMG_0001.mkv"])
                .live_photo_video_for(&photo(), PathCase::Sensitive),
            None,
            "a container Immich does not use for live photos is not one"
        );
        assert_eq!(
            index_of(&["IMG_0001.jpg"]).live_photo_video_for(&photo(), PathCase::Sensitive),
            None,
            "a photo on its own has no video"
        );
    }

    #[test]
    fn a_video_is_never_its_own_live_photos_video() {
        // R3.3: the motion video uploads as an ordinary asset of its own when
        // no photo claims it, so asking it for a pairing must find nothing.
        let index = index_of(&["IMG_0001.jpg", "IMG_0001.mov"]);

        assert_eq!(
            index.live_photo_video_for(Path::new("/photos/IMG_0001.mov"), PathCase::Sensitive),
            None
        );
    }

    #[test]
    fn a_case_variant_video_pairs_under_the_windows_rule_only() {
        let index = index_of(&["IMG_0001.jpg", "img_0001.MOV"]);

        assert_eq!(
            index.live_photo_video_for(&photo(), PathCase::Insensitive),
            Some(Path::new("/photos/img_0001.MOV")),
            "R2.4: the Windows rule folds case"
        );
        assert_eq!(
            index.live_photo_video_for(&photo(), PathCase::Sensitive),
            None
        );
    }

    #[test]
    fn a_video_in_another_folder_does_not_pair() {
        let index = FolderIndex::known([
            (PathBuf::from("/photos"), vec![photo()]),
            (
                PathBuf::from("/videos"),
                vec![PathBuf::from("/videos/IMG_0001.mov")],
            ),
        ]);

        assert_eq!(
            index.live_photo_video_for(&photo(), PathCase::Sensitive),
            None
        );
    }

    #[test]
    fn an_unreadable_folder_pairs_nothing_and_loses_no_media() {
        let index = FolderIndex::read(&[photo()]);

        assert_eq!(index.sidecar_for(&photo(), PathCase::Sensitive), None);
        assert_eq!(
            index.live_photo_video_for(&photo(), PathCase::Sensitive),
            None
        );
    }

    #[test]
    fn a_stem_and_a_sidecar_are_never_each_others_pair() {
        // R3.3: the two rules never cross over, whatever the names look like.
        let photo = photo();
        let video = Path::new("/photos/IMG_0001.mov");
        let sidecar = Path::new("/photos/IMG_0001.jpg.xmp");

        assert!(is_sidecar_of(&photo, sidecar, PathCase::Sensitive));
        assert!(!is_sidecar_of(&photo, video, PathCase::Sensitive));
        assert!(!is_sidecar_of(video, sidecar, PathCase::Sensitive));
        assert!(same_stem(&photo, video, PathCase::Sensitive));
        assert!(!same_stem(&photo, sidecar, PathCase::Sensitive));
        assert!(!same_stem(
            &photo,
            Path::new("/photos/IMG_0002.mov"),
            PathCase::Sensitive
        ));
    }
}
