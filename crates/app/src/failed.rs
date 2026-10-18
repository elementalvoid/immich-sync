//! The failed-items view model (SPEC R6.5): plain data with no UI imports.
//!
//! [`rows`] maps the parked items onto the rows the view renders, so the text
//! the window shows is testable without a UI.

use sync_core::store::{FailedItem, ItemState};

use crate::status::folder_name;

/// One row of the failed-items view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedRow {
    /// The local path.
    pub path: String,
    /// The file name, which is what the row leads with.
    pub name: String,
    /// The persisted failure class, or `unclassified` when none fits (R6.4).
    pub class: String,
    /// The last error the client recorded.
    pub error: String,
    /// How many automatic attempts the item spent (R6.1).
    pub attempts: u32,
    /// Whether the item is in the dead letter (the attempt cap) or failed
    /// permanently (R6.5).
    pub dead_letter: bool,
    /// The accessible name of the retry control.
    pub retry_label: String,
    /// The accessible name of the whole row, so a screen reader hears the file,
    /// its class, and its reason once.
    pub accessible_name: String,
}

impl FailedRow {
    /// The attempts line: how the item got here and what it spent.
    ///
    /// A refusal the checksum check answered before any upload (R3.1) has no
    /// attempt of its own, so the line says what happened without a count.
    #[must_use]
    pub fn attempts_line(&self) -> String {
        let state = if self.dead_letter {
            "Dead letter"
        } else {
            "Permanent failure"
        };
        let noun = if self.attempts == 1 {
            "attempt"
        } else {
            "attempts"
        };
        match self.attempts {
            0 => state.to_string(),
            attempts => format!("{state} after {attempts} {noun}"),
        }
    }
}

/// The text above the list: how much needs the user.
#[must_use]
pub fn summary(items: &[FailedItem]) -> String {
    match items.len() {
        0 => "Nothing has failed.".to_string(),
        1 => "1 file could not be uploaded.".to_string(),
        count => format!("{count} files could not be uploaded."),
    }
}

/// Maps the parked items onto the rows the view renders (R6.5).
#[must_use]
pub fn rows(items: &[FailedItem]) -> Vec<FailedRow> {
    items
        .iter()
        .map(|item| {
            let name = folder_name(&item.path);
            let class = item
                .error_class
                .map_or("unclassified", |class| class.as_str())
                .to_string();
            let error = item
                .error_message
                .clone()
                .unwrap_or_else(|| "The client did not record a reason.".to_string());
            FailedRow {
                path: item.path.clone(),
                name: name.clone(),
                retry_label: format!("Retry {name}"),
                accessible_name: format!("{name}, {class}: {error}"),
                class,
                error,
                attempts: item.attempts,
                dead_letter: item.state == ItemState::DeadLetter,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sync_core::failure::FailureClass;

    fn item(path: &str, state: ItemState, class: Option<FailureClass>) -> FailedItem {
        FailedItem {
            path: path.to_string(),
            state,
            error_class: class,
            error_message: Some("the server answered 503".to_string()),
            attempts: 8,
        }
    }

    #[test]
    fn a_dead_letter_row_names_the_class_the_reason_and_the_attempts() {
        let rows = rows(&[item(
            "/photos/IMG_1.jpg",
            ItemState::DeadLetter,
            Some(FailureClass::ServerTransient),
        )]);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "IMG_1.jpg");
        assert_eq!(rows[0].class, "server_transient");
        assert_eq!(rows[0].error, "the server answered 503");
        assert_eq!(rows[0].attempts_line(), "Dead letter after 8 attempts");
        assert_eq!(rows[0].retry_label, "Retry IMG_1.jpg");
        assert!(
            rows[0].accessible_name.contains("IMG_1.jpg")
                && rows[0].accessible_name.contains("server_transient"),
            "the row is announced with its class: {}",
            rows[0].accessible_name
        );
    }

    #[test]
    fn a_permanent_row_says_so_and_a_missing_reason_is_stated_not_invented() {
        let mut permanent = item(
            "photo.bin",
            ItemState::FailedPermanent,
            Some(FailureClass::UnsupportedFormat),
        );
        permanent.error_message = None;
        let rows = rows(&[permanent]);

        assert_eq!(
            rows[0].attempts_line(),
            "Permanent failure after 8 attempts"
        );
        assert_eq!(rows[0].error, "The client did not record a reason.");
        assert_eq!(
            rows[0].class, "unsupported_format",
            "the class is still shown"
        );
    }

    #[test]
    fn an_item_that_never_reached_the_wire_has_no_attempt_count() {
        let mut refused = item(
            "photo.bin",
            ItemState::FailedPermanent,
            Some(FailureClass::UnsupportedFormat),
        );
        refused.attempts = 0;

        let rows = rows(&[refused]);

        assert_eq!(rows[0].attempts_line(), "Permanent failure");
    }

    #[test]
    fn the_summary_counts_the_items() {
        assert_eq!(summary(&[]), "Nothing has failed.");
        assert_eq!(
            summary(&[item("a", ItemState::DeadLetter, None)]),
            "1 file could not be uploaded."
        );
        assert_eq!(
            summary(&[
                item("a", ItemState::DeadLetter, None),
                item("b", ItemState::FailedPermanent, None),
            ]),
            "2 files could not be uploaded."
        );
    }
}
