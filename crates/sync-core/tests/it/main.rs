//! One test binary for the whole crate.
//!
//! Separate files under `tests/` each became a separately linked binary (~230 MB
//! for this crate: 51 links across the workspace, 19 of them re-linking the whole
//! GPUI stack. One `main.rs` with the files as modules links once.

mod album_title;
mod common;
mod connection;
mod dependency_rule;
mod engine;
mod hash;
mod limits;
mod log_redaction;
mod meter;
mod missing;
mod network;
mod pairing;
mod status;
mod store;
mod tag_backfill;
mod tags;
mod tracking;
mod upload;
