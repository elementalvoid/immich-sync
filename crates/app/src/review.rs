//! The deletion review's view model (SPEC R5.3, R5.5): plain data with no UI
//! imports.
//!
//! [`rows`] maps the missing files onto the rows the view renders, and
//! [`trash_problem`] turns a refused trash into the actionable message R5.5
//! asks for, so the text the window shows is testable without a UI.

use sync_core::ClientError;
use sync_core::engine::ReviewError;
use sync_core::store::ReviewItem;

use crate::status::folder_name;

/// One row of the deletion review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewRow {
    /// The local path that disappeared.
    pub path: String,
    /// The file name, which is what the row leads with.
    pub name: String,
    /// The accessible name of the keep control.
    pub keep_label: String,
    /// The accessible name of the trash control.
    pub trash_label: String,
    /// The accessible name of the whole row, so a screen reader hears the file
    /// and what happened to it once.
    pub accessible_name: String,
}

/// The text above the list: how much waits for the user.
#[must_use]
pub fn summary(items: &[ReviewItem]) -> String {
    match items.len() {
        0 => "Nothing waits for a decision.".to_string(),
        1 => "1 file disappeared from a source folder.".to_string(),
        count => format!("{count} files disappeared from your source folders."),
    }
}

/// Maps the missing files onto the rows the view renders (R5.3).
#[must_use]
pub fn rows(items: &[ReviewItem]) -> Vec<ReviewRow> {
    items
        .iter()
        .map(|item| {
            let name = folder_name(&item.path);
            let keep_label = format!("Keep {name} on the server");
            let trash_label = format!("Move {name} to the trash");
            let accessible_name = format!("{name}, missing from {}", item.path);
            ReviewRow {
                path: item.path.clone(),
                name,
                keep_label,
                trash_label,
                accessible_name,
            }
        })
        .collect()
}

/// The actionable message for a trash that did not land (R5.5).
///
/// A missing permission names the permission to grant, because that is the one
/// the user can fix; the other failures say what to do next instead of quoting
/// the server.
#[must_use]
pub fn trash_problem(error: &ReviewError) -> String {
    match error {
        ReviewError::Client(ClientError::PermissionDenied(_)) => "The credential cannot move \
             assets to the trash. Grant the asset.delete permission to the API key, or sign in \
             with an account that has it, then try again."
            .to_string(),
        ReviewError::Client(error) => format!(
            "The asset could not be moved to the trash: {}. Try again when the server is reachable.",
            error.redacted_reason()
        ),
        ReviewError::Store(error) => {
            format!("The decision could not be recorded: {error}")
        }
        ReviewError::NotInReview(path) => {
            format!("{path} is no longer waiting for a decision.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sync_core::engine::ReviewError;

    fn item(path: &str) -> ReviewItem {
        ReviewItem {
            path: path.to_string(),
            size: 1024,
            asset_id: Some("asset-1".to_string()),
            state: sync_core::store::ItemState::Missing,
        }
    }

    #[test]
    fn a_row_names_the_file_its_path_and_both_actions() {
        let rows = rows(&[item("/photos/IMG_0001.jpg")]);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "IMG_0001.jpg");
        assert_eq!(rows[0].keep_label, "Keep IMG_0001.jpg on the server");
        assert_eq!(rows[0].trash_label, "Move IMG_0001.jpg to the trash");
        assert_eq!(
            rows[0].accessible_name,
            "IMG_0001.jpg, missing from /photos/IMG_0001.jpg"
        );
    }

    #[test]
    fn the_summary_counts_the_files_that_wait() {
        assert_eq!(summary(&[]), "Nothing waits for a decision.");
        assert_eq!(
            summary(&[item("a.jpg")]),
            "1 file disappeared from a source folder."
        );
        assert_eq!(
            summary(&[item("a.jpg"), item("b.jpg")]),
            "2 files disappeared from your source folders."
        );
    }

    #[test]
    fn a_missing_permission_says_which_permission_to_grant() {
        let error = ReviewError::Client(ClientError::PermissionDenied(
            "Missing required permission: asset.delete".to_string(),
        ));

        let message = trash_problem(&error);

        assert!(
            message.contains("asset.delete"),
            "R5.5: the message names the permission the user can grant: {message}"
        );
        assert!(
            !message.contains("Missing required permission"),
            "the server's own wording is not repeated: {message}"
        );
    }

    #[test]
    fn a_transport_failure_says_to_try_again() {
        let error = ReviewError::Client(ClientError::InvalidToken);

        let message = trash_problem(&error);

        assert!(message.contains("Try again"), "{message}");
    }
}
