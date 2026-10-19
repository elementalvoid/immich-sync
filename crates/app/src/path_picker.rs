//! The two Browse… dialogs and what their result means for the field (#30).
//!
//! Both surfaces ask for one path and fill their existing field, so the dialog
//! result maps to the surface's next step here, once. The mapping takes the
//! `Result` instead of reading it at the click site because the test harness
//! can answer a prompt with a choice or a cancel but never with an error, and
//! the failure arm still needs a test.

use std::path::PathBuf;

/// What one Browse… dialog asks the surface to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Picked {
    /// The user chose this path: put it in the field.
    Path(PathBuf),
    /// The user cancelled: change nothing.
    Cancelled,
    /// The dialog could not open: show this notice and leave the typed path
    /// alone.
    Failed(String),
}

/// Maps one dialog result to the surface's next step.
///
/// Both prompts allow a single path, so a selection is that one path. The
/// notice keeps the dialog's own reason and says the field still works, so a
/// user whose session has no portal can type the path.
#[must_use]
pub fn picked(result: anyhow::Result<Option<Vec<PathBuf>>>) -> Picked {
    match result {
        Ok(Some(paths)) => paths.into_iter().next().map_or(Picked::Cancelled, Picked::Path),
        Ok(None) => Picked::Cancelled,
        Err(error) => Picked::Failed(format!(
            "Couldn't open the file picker ({error}). Type the path instead."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{Picked, picked};
    use std::path::PathBuf;

    /// The error a Linux session with no xdg-desktop-portal returns
    /// (`gpui_pre_linux`), so the notice is tested against the real text.
    const PORTAL_MISSING: &str =
        "Couldn't open file picker due to missing xdg-desktop-portal implementation.";

    #[test]
    fn a_chosen_path_is_the_path_to_fill_in() {
        let chosen = PathBuf::from("/home/you/Pictures");

        assert_eq!(
            picked(Ok(Some(vec![chosen.clone()]))),
            Picked::Path(chosen)
        );
    }

    #[test]
    fn a_cancel_and_an_empty_selection_change_nothing() {
        assert_eq!(picked(Ok(None)), Picked::Cancelled);
        assert_eq!(picked(Ok(Some(Vec::new()))), Picked::Cancelled);
    }

    #[test]
    fn a_dialog_that_cannot_open_is_a_readable_notice() {
        let Picked::Failed(notice) = picked(Err(anyhow::anyhow!(PORTAL_MISSING))) else {
            panic!("a dialog that could not open is not a path");
        };

        assert!(
            notice.contains("xdg-desktop-portal"),
            "the notice keeps the reason the dialog gave: {notice}"
        );
        assert!(
            notice.contains("Type the path instead"),
            "the notice says the field still works: {notice}"
        );
    }
}
