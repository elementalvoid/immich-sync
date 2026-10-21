//! The folder filter the failed-items and deletion-review views share (#41).
//!
//! One selected source folder narrows both lists. The rule is a path-prefix
//! match under the platform's case behaviour (R2.4): a recursive folder
//! includes what lies below it, a non-recursive one only its direct children,
//! and a sibling whose name merely starts with the same letters never matches.
//! [`sync_core::paths::is_within`] already answers the prefix question, so this
//! module only adds the recursion depth.

use std::path::Path;

use sync_core::paths::{PathCase, equal, is_within, key};
use sync_core::store::SourceFolder;

use crate::status::folder_name;

/// The default: no folder selected, so every item is listed.
pub const ALL_FOLDERS: &str = "All folders";

/// Whether `item` belongs to the source folder `folder` under `case`.
///
/// `recursive` decides the depth: a recursive folder keeps anything below it, a
/// non-recursive one keeps only the files whose parent folder is `folder`
/// itself. The boundary is a separator, so `/a/photos` never matches
/// `/a/photos2/x.jpg`. The parent test reads [`key`], which unifies separators
/// as well as folding case, so both behaviours agree.
#[must_use]
pub fn includes(folder: &str, recursive: bool, item: &str, case: PathCase) -> bool {
    let root = Path::new(folder);
    let item = Path::new(item);
    is_within(root, item, case) && (recursive || parent_key(item, case) == key(root, case))
}

/// The comparison key of the folder that directly holds `item`.
fn parent_key(item: &Path, case: PathCase) -> String {
    let item = key(item, case);
    item.rsplit_once('/')
        .map_or_else(String::new, |(parent, _)| parent.to_string())
}

/// The folder one list is narrowed to, and whether its options are showing.
///
/// It is the control's whole state, so both views render it the same way
/// (see [`crate::folder_filter_view`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderFilter {
    /// The selected folder path; `None` is [`ALL_FOLDERS`].
    selected: Option<String>,
    /// Whether the option list is showing.
    open: bool,
}

impl FolderFilter {
    /// A filter that starts on [`ALL_FOLDERS`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The selected folder path, when one is selected.
    #[must_use]
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Whether one folder is selected.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.selected.is_some()
    }

    /// Selects one folder and closes the options.
    pub fn select(&mut self, path: impl Into<String>) {
        self.selected = Some(path.into());
        self.open = false;
    }

    /// Goes back to [`ALL_FOLDERS`].
    pub fn clear(&mut self) {
        self.selected = None;
        self.open = false;
    }

    /// The text the trigger and the chip show: the folder's name, or
    /// [`ALL_FOLDERS`].
    #[must_use]
    pub fn label(&self) -> String {
        self.selected
            .as_deref()
            .map_or_else(|| ALL_FOLDERS.to_string(), folder_name)
    }

    /// Whether the option list is showing.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Shows the options, or hides them.
    pub fn toggle(&mut self) {
        self.open = !self.open;
    }

    /// Whether the filter keeps `item`.
    ///
    /// The selected folder's recursion comes from the current source folders,
    /// so a recursion toggle on the dashboard takes effect here too. A folder
    /// that is no longer a source folder still filters by its path prefix
    /// rather than quietly listing everything.
    #[must_use]
    pub fn keeps(&self, item: &str, folders: &[SourceFolder], case: PathCase) -> bool {
        let Some(selected) = self.selected.as_deref() else {
            return true;
        };
        let recursive = folders
            .iter()
            .find(|folder| equal(Path::new(&folder.path), Path::new(selected), case))
            .is_none_or(|folder| folder.recursive);
        includes(selected, recursive, item, case)
    }

    /// The heading of an empty list.
    ///
    /// A filtered list names the folder it is empty for (`"No failed items in
    /// Camera Roll"`); an unfiltered one keeps the view's own heading.
    #[must_use]
    pub fn empty_heading(&self, filtered: &str, unfiltered: &str) -> String {
        match &self.selected {
            Some(selected) => format!("{filtered} in {}", folder_name(selected)),
            None => unfiltered.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOWS: PathCase = PathCase::Insensitive;
    const LINUX: PathCase = PathCase::Sensitive;

    fn folder(path: &str, recursive: bool) -> SourceFolder {
        SourceFolder {
            path: path.to_string(),
            recursive,
            album_mapping: false,
            album_scope: sync_core::store::AlbumScope::Leaf,
            album_title: None,
        }
    }

    /// A Windows path, built from parts so the repository's hardcoded-path scan
    /// stays clean (its own test scans every source file).
    fn win(rest: &str) -> String {
        format!("{}:\\{}", 'C', rest.replace('/', "\\"))
    }

    /// The items `filter` keeps, in the order they arrived.
    fn kept_by<'a>(
        filter: &FolderFilter,
        items: &[&'a str],
        folders: &[SourceFolder],
        case: PathCase,
    ) -> Vec<&'a str> {
        let mut kept = items.to_vec();
        kept.retain(|item| filter.keeps(item, folders, case));
        kept
    }

    #[test]
    fn a_recursive_folder_includes_its_subfolders() {
        assert!(includes("/photos", true, "/photos/a.jpg", LINUX));
        assert!(includes("/photos", true, "/photos/2024/a.jpg", LINUX));
    }

    #[test]
    fn a_non_recursive_folder_includes_only_its_direct_children() {
        assert!(includes("/photos", false, "/photos/a.jpg", LINUX));
        assert!(!includes("/photos", false, "/photos/2024/a.jpg", LINUX));
    }

    #[test]
    fn a_sibling_with_a_shared_name_prefix_does_not_match() {
        assert!(!includes("/photos", true, "/photos2/a.jpg", LINUX));
        assert!(!includes("/photos", true, "/photos2/2024/a.jpg", LINUX));
        assert!(!includes("/photos", false, "/photos2/a.jpg", LINUX));
    }

    #[test]
    fn a_case_only_variant_matches_on_the_windows_rule() {
        assert!(includes(
            &win("Pictures"),
            true,
            &win("pictures/2024/a.jpg").replace('C', "c"),
            WINDOWS
        ));
        assert!(!includes("/Pictures", true, "/pictures/2024/a.jpg", LINUX));
    }

    #[test]
    fn the_filter_keeps_every_item_until_a_folder_is_selected() {
        let folders = [folder("/photos", true), folder("/videos", false)];
        let items = [
            "/photos/a.jpg",
            "/videos/b.mp4",
            "/videos/sub/c.mp4",
            "/elsewhere/d.jpg",
        ];
        let mut filter = FolderFilter::new();

        let kept = kept_by(&filter, &items, &folders, LINUX);
        assert_eq!(kept, items, "All folders keeps every item");

        filter.select("/photos");
        let kept = kept_by(&filter, &items, &folders, LINUX);
        assert_eq!(kept, ["/photos/a.jpg"]);
        assert!(filter.is_active());

        // `/videos` is a non-recursive source folder, so selecting it keeps its
        // own files and drops the ones in its subfolder: the recursion comes
        // from the folder list, not from the selection.
        filter.select("/videos");
        let kept = kept_by(&filter, &items, &folders, LINUX);
        assert_eq!(
            kept,
            ["/videos/b.mp4"],
            "a non-recursive folder keeps only its direct children"
        );

        filter.clear();
        let kept = kept_by(&filter, &items, &folders, LINUX);
        assert_eq!(kept, items, "clearing restores every item");
        assert!(!filter.is_active());
    }

    #[test]
    fn the_selected_folder_is_labelled_by_its_name() {
        let mut filter = FolderFilter::new();
        assert_eq!(filter.label(), ALL_FOLDERS);

        filter.select("/home/you/Pictures/Camera Roll");
        assert_eq!(filter.label(), "Camera Roll");
    }

    #[test]
    fn an_empty_filtered_list_names_the_folder() {
        let mut filter = FolderFilter::new();
        assert_eq!(
            filter.empty_heading("No failed items", "Nothing has failed"),
            "Nothing has failed",
            "the unfiltered empty state is the view's own"
        );

        filter.select("/home/you/Pictures/Camera Roll");
        assert_eq!(
            filter.empty_heading("No failed items", "Nothing has failed"),
            "No failed items in Camera Roll"
        );
        assert_eq!(
            filter.empty_heading("No missing files", "Every tracked file is present"),
            "No missing files in Camera Roll"
        );
    }

    #[test]
    fn a_selected_folder_that_is_no_longer_a_source_folder_still_filters() {
        let filter = {
            let mut filter = FolderFilter::new();
            filter.select("/gone");
            filter
        };

        let kept = kept_by(&filter, &["/gone/a.jpg", "/other/b.jpg"], &[], LINUX);

        assert_eq!(
            kept,
            ["/gone/a.jpg"],
            "a path-prefix filter, not every item"
        );
    }
}
