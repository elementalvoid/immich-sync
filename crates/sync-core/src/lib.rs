//! Sync engine: discovery, hashing, queue, retry, limits, and state.
//!
//! The engine owns every sync decision and never imports the shell, so a
//! headless build of this crate needs no window system. The engine is filled
//! in by the engine tickets; this crate exists so the workspace and the
//! dependency rule are in place.
