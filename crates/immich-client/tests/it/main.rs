//! One test binary for the whole crate.
//!
//! Separate files under `tests/` each became a separately linked binary (~230 MB
//! for this crate: 51 links across the workspace, 19 of them re-linking the whole
//! GPUI stack. One `main.rs` with the files as modules links once.

mod albums;
mod client;
mod limits;
mod tags;
mod trash;
mod upload;
