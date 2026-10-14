//! Sync engine: discovery, hashing, queue, retry, limits, and state.
//!
//! The engine owns every sync decision and never imports the shell, so a
//! headless build of this crate needs no window system. The connection session
//! and the credential store live here so the upload engine reuses them; the
//! remaining engine tickets fill in discovery, the queue, and retry.

pub mod albums;
pub mod backoff;
pub mod breaker;
pub mod connection;
pub mod credential;
pub mod engine;
pub mod failure;
pub mod hash;
pub mod pairing;
pub mod paths;
pub mod scan;
pub mod skip;
pub mod store;
pub mod upload;
pub mod versions;
pub mod watch;

// Re-exported so the shell can hand the engine the client it holds without
// depending on the HTTP client crate directly. The error type comes with it,
// so the review view can tell a missing permission from a transport failure.
pub use immich_client::{ClientError, ImmichClient};
