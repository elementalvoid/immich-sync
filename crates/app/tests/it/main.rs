//! One test binary for the whole crate.
//!
//! Separate files under `tests/` each became a separately linked binary (~230 MB
//! for this crate: 51 links across the workspace, 19 of them re-linking the whole
//! GPUI stack. One `main.rs` with the files as modules links once.

mod common;
mod dashboard;
mod diagnostics;
mod engine_model;
mod failed_ui;
mod folder_detail;
mod keyring;
mod log_redaction;
mod logging;
mod login;
mod logs_ui;
mod notices;
mod packaging;
mod review_ui;
mod screenshot_seed;
mod settings;
mod settings_ui;
mod shell;
mod upload_ui;
mod window;
