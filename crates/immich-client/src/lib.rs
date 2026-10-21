//! A typed, UI-free Immich HTTP client (ADR 0006).
//!
//! The client covers the Stable operations the client application needs:
//! `POST /auth/login`, `POST /auth/validateToken`, `GET /server/about`,
//! `POST /assets/bulk-upload-check`, and `POST /assets`. The client holds no
//! credential and no session state; the session in `sync-core` owns those.
//!
//! Failures are typed, not strings, so the session and the retry policy can act
//! on the difference between a rejected password, a server with password login
//! disabled, an unreachable server, and an unusable response (R1.4, R1.6).
//! TLS verification stays on (N3).

mod bandwidth;
mod client;
mod error;
mod model;

pub use bandwidth::BandwidthLimit;
pub use client::ImmichClient;
pub use error::ClientError;
pub use model::{
    Album, ApiKeyInfo, AssetUpload, Auth, BulkAction, BulkCheckItem, BulkCheckResult,
    LoginResponse, RejectReason, ServerAbout, SessionToken, UploadResponse,
};
