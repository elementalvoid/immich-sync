//! Versioned changes (SPEC R4.1–R4.5, ADR 0004).
//!
//! When a tracked file changes in place, the pipeline has uploaded the new
//! bytes as a new asset. Immich has no byte-replacement endpoint, so the new
//! asset is a separate asset with none of the old one's associations, and a
//! plain re-upload loses share state. This module is what puts the two back
//! together, in the order the spec names:
//!
//! 1. `GET /assets/{id}` on the old asset answers both questions at once: the
//!    tags it carries (R4.2) and whether it is already in a stack (R4.3).
//! 2. `PUT /assets/copy` moves album memberships, shared links, favorite
//!    state, the sidecar, and stack membership from the old asset to the new
//!    one (R4.2). Faces, descriptions, and custom metadata are not copied by
//!    that endpoint and stay on the old version (ADR 0004).
//! 3. If the old asset was in no stack, `POST /stacks` puts the two in one
//!    stack with the new asset first, so the new version is the primary one
//!    (R4.3). If it was already stacked, the copy above moved the membership
//!    into the existing stack and the stack keeps its id (R4.3). A retry that
//!    finds the new asset already in that stack must not move membership
//!    again: the server would merge the stack into itself and delete it.
//! 4. `PUT /tags/assets` applies the old asset's tags to the new one (R4.2).
//! 5. The state database records the new version and the stack id (R4.4).
//!
//! The order matters for data safety: nothing is recorded until every server
//! call has answered, so a failure anywhere above leaves the item's chain
//! pointing at the version it really has. The caller retries with the retry
//! policy's machinery (R6.1, R6.5); the uploaded asset is never lost, because
//! the next attempt resolves the same bytes to the same asset id (R3.2).
//!
//! All four operations are `x-immich-state: Stable` in the v3.3.1 spec, which is
//! what R3.5 allows sync correctness to depend on.

use std::future::Future;
use std::sync::Arc;

use immich_client::{ClientError, ImmichClient};

use crate::connection::{RequestCredential, SharedSession};
use crate::failure::FailureKind;
use crate::hash::Sha1;
use crate::store::{NewVersion, StateStore, StoreError};
use crate::upload::UploadFailure;

/// Why the version flow could not finish (R4).
#[derive(Debug, thiserror::Error)]
pub enum VersionError {
    /// The server refused a request, or could not be reached (R6.3).
    #[error(transparent)]
    Client(#[from] ClientError),
    /// The state database refused the version chain (R4.4).
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl VersionError {
    /// The failure the retry policy reads (R6.4, R6.7).
    ///
    /// A client failure keeps its class and retry kind. A state database that
    /// cannot write is transient: the work is done on the server, so a later
    /// attempt records it rather than the row claiming less than happened.
    #[must_use]
    pub fn failure(&self) -> UploadFailure {
        match self {
            Self::Client(error) => UploadFailure::from_client_error(error),
            Self::Store(error) => UploadFailure {
                class: None,
                kind: FailureKind::Transient,
                message: error.to_string(),
                retry_after: None,
                connection: false,
            },
        }
    }
}

/// Carries the previous version's associations onto a new asset
/// (R4.2–R4.4).
///
/// The versioner holds no sync decision of its own: the caller has already
/// decided the upload is a terminal success and recorded its asset id, so this
/// only decides what the server and the state database are told about it.
#[derive(Debug)]
pub struct Versioner {
    /// The connection session, so a version call carries the current credential
    /// and a 401 is answered by the one re-login rule (R1.4) like every other
    /// request the engine makes.
    session: SharedSession,
    store: Arc<StateStore>,
}

impl Versioner {
    /// Builds a versioner over the connection session and the app-scoped state
    /// store.
    #[must_use]
    pub fn new(session: SharedSession, store: Arc<StateStore>) -> Self {
        Self { session, store }
    }

    /// Makes `asset_id` the current version of the item at `path` and returns
    /// the stack its versions share.
    ///
    /// An item with no recorded version starts its chain here: there is no
    /// previous asset to copy from, so the version is recorded and nothing is
    /// sent (R4.4). An item that already has a current version takes the flow
    /// of R4.2 and R4.3, and a repeat of the asset that is already current
    /// records nothing and sends nothing.
    ///
    /// # Errors
    ///
    /// Returns [`VersionError`] when the server refuses a request, cannot be
    /// reached, or the state database refuses the chain. Nothing is recorded in
    /// that case, so the caller retries against the version that is really
    /// current.
    pub async fn promote(
        &self,
        path: &str,
        asset_id: &str,
        sha1: Sha1,
    ) -> Result<Option<String>, VersionError> {
        let current = self
            .store
            .versions(path)?
            .into_iter()
            .find(|version| version.is_current);
        let Some(previous) = current else {
            // R4.4: the chain starts at the first upload. There is nothing to
            // carry over, and no stack until a later version makes one.
            self.store.record_version(&NewVersion {
                path: path.to_string(),
                asset_id: asset_id.to_string(),
                sha1,
                created_at_ns: crate::engine::now_ns(),
                stack_id: None,
            })?;
            return Ok(None);
        };
        if previous.asset_id == asset_id {
            // The chain already points at this asset: a repeat of the record
            // itself, which must not copy associations a second time.
            return Ok(self.store.stack_id(path)?);
        }
        // One read answers both questions about the old version (R4.2, R4.3).
        let info = self
            .request(previous.asset_id.clone(), |client, credential, id| async move {
                client.asset_info(credential.auth(), &id).await
            })
            .await?;
        // A retry after a partial promote (the copy or the stack answered, the
        // tags or the record did not) finds the new asset in the stack the
        // first attempt made. Copying stack membership again would make the
        // server merge that stack into itself and delete it (v3.3.1
        // `asset.service.ts::copyStack`), so membership moves only when the
        // target is not already in the old version's stack.
        let copy_stack = match &info.stack_id {
            Some(stack_id) => {
                let target = self
                    .request(asset_id.to_string(), |client, credential, id| async move {
                        client.asset_info(credential.auth(), &id).await
                    })
                    .await?;
                target.stack_id.as_ref() != Some(stack_id)
            }
            None => true,
        };
        // R4.2: albums, shared links, favorite state, sidecar, stack.
        self.request(
            (previous.asset_id.clone(), asset_id.to_string(), copy_stack),
            |client, credential, (source, target, stack)| async move {
                client
                    .copy_asset(credential.auth(), &source, &target, stack)
                    .await
            },
        )
        .await?;
        // R4.3: a stack the old asset is already in keeps its id, because the
        // copy above moved the membership onto the new asset. One that does not
        // exist yet is made here, with the new asset first so it is primary.
        let stack_id = match info.stack_id {
            Some(stack_id) => stack_id,
            None => {
                let members = vec![asset_id.to_string(), previous.asset_id.clone()];
                self.request(members.clone(), |client, credential, members| async move {
                    client.create_stack(credential.auth(), &members).await
                })
                .await?
            }
        };
        // R4.2: the tag endpoints, which the copy endpoint does not cover.
        if !info.tag_ids.is_empty() {
            let tags = info.tag_ids;
            self.request(
                (tags.clone(), vec![asset_id.to_string()]),
                |client, credential, (tag_ids, asset_ids)| async move {
                    client
                        .bulk_tag_assets(credential.auth(), &tag_ids, &asset_ids)
                        .await
                },
            )
            .await?;
        }
        // R4.4: the last write, after every call above has answered.
        self.store.record_version(&NewVersion {
            path: path.to_string(),
            asset_id: asset_id.to_string(),
            sha1,
            created_at_ns: crate::engine::now_ns(),
            stack_id: Some(stack_id.clone()),
        })?;
        tracing::info!(
            previous = %previous.asset_id,
            asset_id,
            stack_id = %stack_id,
            "R4.2: the new version carries the old one's associations"
        );
        Ok(Some(stack_id))
    }

    /// One version request through the shared session, so a 401 is answered by
    /// the one re-login the engine shares (R1.4).
    ///
    /// `arg` is cloned per attempt because the session may send the request
    /// twice: a 401 costs one re-login and one more try (R1.4).
    async fn request<T, A, Fut>(
        &self,
        arg: A,
        mut call: impl FnMut(ImmichClient, RequestCredential, A) -> Fut,
    ) -> Result<T, ClientError>
    where
        A: Clone,
        Fut: Future<Output = Result<T, ClientError>>,
    {
        let client = self.session.client();
        self.session
            .request(move |credential| call(client.clone(), credential, arg.clone()))
            .await
    }
}
