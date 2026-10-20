//! The sync tag (SPEC R3.7, GLOSSARY "Sync tag").
//!
//! The setting is opt-in. When it is on, the engine upserts the one tag
//! `immich-sync` with `PUT /tags`, keeps the id the server answered for the
//! whole run, and adds that tag to every asset an upload settles with
//! `PUT /tags/assets`. A duplicate the server already held is tagged too, and a
//! new version of an edited file is tagged after the version flow finished, so
//! the chain keeps the tag.
//!
//! Tagging is auxiliary. It runs after the item is recorded as uploaded, and a
//! failure here is recorded for the Settings section instead of reaching the
//! item's state: a tag failure never parks a successful upload (R3.7). One tag
//! id is cached for the run, so the upsert runs once and a tag the user deleted
//! in Immich is created again on the next run - or on the next attempt after a
//! failure, because a tag failure clears the cache.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use immich_client::ClientError;
use tokio::sync::Mutex as AsyncMutex;

use crate::connection::{RequestCredential, SharedSession};

/// The one tag the client adds to the assets it syncs (GLOSSARY "Sync tag").
pub const SYNC_TAG: &str = "immich-sync";

/// The permissions an API key needs for the sync tag (R3.7).
///
/// `tag.create` makes the tag, `tag.asset` adds it to an asset, and
/// `asset.update` is what Immich checks on the asset side of the same request
/// (v3.3.1 `TagService.bulkTagAssets`).
const REQUIRED_PERMISSIONS: [&str; 3] = ["tag.create", "tag.asset", "asset.update"];

/// What the Settings section shows about the sync tag (R3.7).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TagStatus {
    /// Whether the running engine is applying the tag.
    pub enabled: bool,
    /// Why the tag is not being applied, or why the last attempt failed.
    pub problem: Option<String>,
}

/// Why the sync tag could not be applied (R3.7).
#[derive(Debug, thiserror::Error)]
pub enum TagError {
    /// A request the tag needs was refused or could not be sent (R6.3).
    #[error(transparent)]
    Client(#[from] ClientError),
    /// The API key is missing a permission the tag needs (R3.7).
    #[error("the API key is missing the {permission} permission")]
    MissingPermission {
        /// The permission the key lacks, as Immich names it.
        permission: &'static str,
    },
}

/// Applies the sync tag to the assets the engine settles (R3.7).
///
/// The tagger holds one cached tag id and the live setting. It makes no sync
/// decision of its own: the caller has already recorded the item as uploaded,
/// so this only decides what the server is told about that asset.
#[derive(Debug)]
pub struct Tagger {
    /// The connection session, so a tag call carries the current credential and
    /// a 401 is answered by the one re-login rule (R1.4) like every other
    /// request the engine makes.
    session: SharedSession,
    /// The tag id this run resolved. `None` until an asset needs it, and
    /// cleared after a failure so the next attempt resolves it again.
    tag_id: AsyncMutex<Option<String>>,
    /// The live setting (R3.7). The upload path reads it without a lock.
    enabled: AtomicBool,
    /// Why the tag is not working, for the Settings section (R3.7).
    problem: Mutex<Option<String>>,
}

impl Tagger {
    /// Builds a tagger over the connection session, off until the setting says
    /// otherwise.
    #[must_use]
    pub fn new(session: SharedSession) -> Self {
        Self {
            session,
            tag_id: AsyncMutex::new(None),
            enabled: AtomicBool::new(false),
            problem: Mutex::new(None),
        }
    }

    /// Turns the feature on or off and reports whether that changed it (R3.7).
    ///
    /// Turning it off stops new tagging. Assets that already carry the tag keep
    /// it, and the tag itself is not deleted.
    pub fn set_enabled(&self, enabled: bool) -> bool {
        self.enabled.swap(enabled, Ordering::SeqCst) != enabled
    }

    /// Whether the feature is on right now.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// What the Settings section shows (R3.7).
    #[must_use]
    pub fn status(&self) -> TagStatus {
        TagStatus {
            enabled: self.enabled(),
            problem: self.problem.lock().expect("the tag problem lock").clone(),
        }
    }

    /// Resolves the tag before anything is uploaded, so a missing API-key
    /// permission reaches Settings before the first asset is tagged (R3.7).
    ///
    /// The outcome is recorded either way; the caller has nothing to do with a
    /// failure here.
    pub async fn prepare(&self) {
        if !self.enabled() {
            return;
        }
        self.record(&self.resolve().await.map(|_| ()));
    }

    /// Adds the sync tag to one settled asset (R3.7).
    ///
    /// The caller runs this after the item is recorded as uploaded, so a
    /// failure is recorded for Settings and never reaches the item's state.
    ///
    /// # Errors
    ///
    /// Returns [`TagError`] for a refused or unreachable request, and for an
    /// API key that is missing one of the permissions the tag needs. The reason
    /// stays available through [`Self::status`].
    pub async fn tag(&self, asset_id: &str) -> Result<(), TagError> {
        if !self.enabled() {
            return Ok(());
        }
        let outcome = self.apply(asset_id).await;
        self.record(&outcome);
        outcome
    }

    /// Applies the tag and drops the cached id when the call fails: a refused
    /// id may be a tag the user deleted in Immich, and the next attempt upserts
    /// it again (R3.7).
    async fn apply(&self, asset_id: &str) -> Result<(), TagError> {
        let tag_id = self.resolve().await?;
        let client = self.session.client();
        let asset_ids = vec![asset_id.to_string()];
        let tagged = self
            .session
            .request(move |credential| {
                let client = client.clone();
                let tag_ids = [tag_id.clone()];
                let asset_ids = asset_ids.clone();
                async move {
                    client
                        .bulk_tag_assets(credential.auth(), &tag_ids, &asset_ids)
                        .await
                }
            })
            .await;
        if tagged.is_err() {
            *self.tag_id.lock().await = None;
        }
        tagged.map_err(TagError::Client)
    }

    /// The tag id this run resolved, creating or reusing the tag on first use
    /// (R3.7).
    ///
    /// The lock is held across the requests on purpose: two uploads that finish
    /// at once must not upsert the tag twice, and the upsert is once per run
    /// either way.
    async fn resolve(&self) -> Result<String, TagError> {
        let mut cached = self.tag_id.lock().await;
        if let Some(id) = cached.as_ref() {
            return Ok(id.clone());
        }
        self.check_permissions().await?;
        let client = self.session.client();
        let tag_id = self
            .session
            .request(move |credential| {
                let client = client.clone();
                async move { client.upsert_tag(credential.auth(), SYNC_TAG).await }
            })
            .await?;
        *cached = Some(tag_id.clone());
        Ok(tag_id)
    }

    /// Refuses the tag when an API key is missing one of the permissions the
    /// three requests need (R3.7). A password session holds the account's
    /// permissions, so it needs no list.
    async fn check_permissions(&self) -> Result<(), TagError> {
        if !matches!(
            self.session.credential(),
            Some(RequestCredential::ApiKey(_))
        ) {
            return Ok(());
        }
        let client = self.session.client();
        let key = self
            .session
            .request(move |credential| {
                let client = client.clone();
                async move { client.api_key_permissions(credential.auth()).await }
            })
            .await?;
        match missing_permission(&key.permissions) {
            Some(permission) => Err(TagError::MissingPermission { permission }),
            None => Ok(()),
        }
    }

    /// Records the newest outcome for Settings, and logs a failure with its
    /// reason (R3.7, R10.3).
    fn record(&self, outcome: &Result<(), TagError>) {
        let problem = outcome.as_ref().err().map(ToString::to_string);
        if let Some(problem) = &problem {
            tracing::warn!(
                reason = %problem,
                "R3.7: the sync tag was not applied; the upload stands"
            );
        }
        *self.problem.lock().expect("the tag problem lock") = problem;
    }
}

/// The first permission an API key needs for the sync tag and does not hold
/// (R3.7).
///
/// Immich's `all` grants every permission, so a key holding it is missing
/// nothing. The order is the order of the requests: the upsert needs
/// `tag.create`, the tagging call needs `tag.asset` and `asset.update`.
#[must_use]
fn missing_permission(granted: &[String]) -> Option<&'static str> {
    if granted.iter().any(|permission| permission == "all") {
        return None;
    }
    REQUIRED_PERMISSIONS
        .into_iter()
        .find(|needed| !granted.iter().any(|permission| permission == *needed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn granted(permissions: &[&str]) -> Vec<String> {
        permissions
            .iter()
            .map(|permission| permission.to_string())
            .collect()
    }

    #[test]
    fn a_key_holding_all_is_missing_nothing() {
        assert_eq!(missing_permission(&granted(&["all"])), None);
    }

    #[test]
    fn a_key_with_the_three_permissions_is_missing_nothing() {
        assert_eq!(
            missing_permission(&granted(&["tag.create", "tag.asset", "asset.update"])),
            None
        );
    }

    #[test]
    fn the_first_missing_permission_is_the_one_named() {
        // Immich's `all` is the only value that covers the others; without it
        // each permission is named alone.
        assert_eq!(
            missing_permission(&granted(&["asset.update", "tag.asset"])),
            Some("tag.create")
        );
        assert_eq!(
            missing_permission(&granted(&["tag.create", "tag.asset"])),
            Some("asset.update")
        );
    }
}
