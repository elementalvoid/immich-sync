//! A local test double of the Immich v3 HTTP API used by `sync-core` tests.
//!
//! The double implements only the `Stable` operations the client calls, stores
//! uploaded assets by SHA-1, and injects faults on demand. It binds an
//! ephemeral loopback port, so tests run offline with no Docker.

mod server;

pub use server::{
    AssetCopyCall, DeleteCall, Fault, MockImmich, Operation, StackCall, TagAssetCall,
    TagUpsertCall, UploadRecord,
};
