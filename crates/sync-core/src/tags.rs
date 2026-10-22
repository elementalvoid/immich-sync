//! The sync tag (SPEC R3.7, GLOSSARY "Sync tag").
//!
//! The app's setting is on by default and its stored value wins; this crate's
//! `EngineConfig::default()` keeps the tag off, and only the caller's setting
//! turns it on. When it is on, the engine upserts the one tag `immich-sync`
//! with `PUT /tags`, keeps the id the server answered for the whole run, and
//! adds that tag to every asset an upload settles with `PUT /tags/assets`. A
//! duplicate the server already held is tagged too, and a new version of an
//! edited file is tagged after the version flow finished, so the chain keeps
//! the tag.
//!
//! Tagging is auxiliary. It runs after the item is recorded as uploaded, and a
//! failure here is recorded for the Settings section instead of reaching the
//! item's state: a tag failure never parks a successful upload (R3.7). One tag
//! id is cached for the run, so the upsert runs once and a tag the user deleted
//! in Immich is created again on the next run - or on the next attempt after a
//! failure, because a tag failure clears the cache. An API key that lacks a
//! permission is refused once per credential: the rest of the uploads replay
//! that refusal instead of probing the key again, until a sign-in or re-login
//! moves the session to a credential that may hold the permission.
//!
//! The same tag also backfills the assets the client uploaded before the
//! setting was turned on ([`crate::backfill`]): one `PUT /tags/assets` carries a
//! batch of ids, so the backfill of a whole library costs a request per few
//! hundred assets instead of one per asset.

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
    /// Whether the switch is on (R3.7). It is the setting, not whether the tag
    /// is landing: it stays true while the API key is missing a permission and
    /// nothing is applied.
    pub enabled: bool,
    /// Why the tag is not being applied, or why the last attempt failed.
    pub problem: Option<String>,
    /// How far the backfill over the assets that were uploaded before the tag
    /// was turned on got (R3.7).
    pub backfill: BackfillStatus,
}

/// The sync tag's backfill progress, as the Settings section shows it (R3.7).
///
/// The counters describe the pass the state database records, so a restart
/// publishes what the last run reached before this one starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackfillStatus {
    /// How many assets the recorded pass has tagged.
    pub tagged: u32,
    /// How many assets the pass covers: every current asset, every old
    /// version, and every asset the server answered as a duplicate.
    pub total: u32,
    /// Whether a pass is running right now.
    pub running: bool,
    /// Whether a pass reached the end of the library and recorded that.
    pub completed: bool,
}

/// Why the sync tag could not be applied (R3.7).
#[derive(Debug, thiserror::Error)]
pub enum TagError {
    /// A request the tag needs was refused or could not be sent (R6.3).
    #[error(transparent)]
    Client(#[from] ClientError),
    /// The API key is missing permissions the tag needs (R3.7).
    #[error("the API key is missing these permissions: {}", permissions.join(", "))]
    MissingPermission {
        /// The permissions the key lacks, as Immich names them.
        permissions: Vec<&'static str>,
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
    /// The permissions an API key was already refused and the session's auth
    /// epoch when that happened, so a new credential probes the key again
    /// instead of replaying a refusal that may no longer hold (R3.7).
    refused: Mutex<Option<(u32, Vec<&'static str>)>>,
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
            refused: Mutex::new(None),
        }
    }

    /// Turns the feature on or off and reports whether that changed it (R3.7).
    ///
    /// Turning it off stops new tagging. Assets that already carry the tag keep
    /// it, and the tag itself is not deleted. Either way the user made a new
    /// decision, so the next resolve probes the key again.
    pub fn set_enabled(&self, enabled: bool) -> bool {
        if self.enabled.swap(enabled, Ordering::SeqCst) == enabled {
            return false;
        }
        *self.refused.lock().expect("the tag refusal lock") = None;
        true
    }

    /// Whether the feature is on right now.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// What the Settings section shows (R3.7).
    ///
    /// The backfill's own counters are the engine's, which owns the run; this
    /// reports only what the tagger knows on its own.
    #[must_use]
    pub fn status(&self) -> TagStatus {
        TagStatus {
            enabled: self.enabled(),
            problem: self.problem.lock().expect("the tag problem lock").clone(),
            backfill: BackfillStatus::default(),
        }
    }

    /// Resolves the tag before anything is uploaded, so a missing API-key
    /// permission reaches Settings before the first asset is tagged (R3.7).
    ///
    /// The caller runs this once at the start of a run, and every toggle that
    /// turns the setting on runs it again, so a tag the user deleted and
    /// recreated in Immich is upserted again and the new id replaces the cached
    /// one instead of a stale id being trusted (R3.7).
    ///
    /// The outcome is recorded either way; the caller has nothing to do with a
    /// failure here.
    pub async fn prepare(&self) {
        if !self.enabled() {
            return;
        }
        // A new run or a new toggle probes the key, whatever the last one
        // settled on.
        *self.refused.lock().expect("the tag refusal lock") = None;
        let epoch = self.session.auth_epoch();
        // R3.7: the id a previous run resolved is not trusted here. Upserting
        // costs one request per run and answers the id the tag has now, which
        // is what makes a tag deleted and recreated in Immich work.
        *self.tag_id.lock().await = None;
        self.record(epoch, &self.resolve().await.map(|_| ()), None);
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
        // An API key's permissions do not change within a credential, so one
        // settled refusal is the answer for every later asset: it is replayed
        // instead of probing the key and warning again (R3.7). A sign-in or
        // re-login makes a new credential, which may hold the permission.
        // The epoch is read first: if the credential moves while the tag
        // request runs, the refusal is filed under the old one and the next
        // call probes again rather than replaying it.
        let epoch = self.session.auth_epoch();
        if let Some(refusal) = self.refusal() {
            return Err(refusal);
        }
        let outcome = self.apply(&[asset_id.to_string()]).await;
        self.record(epoch, &outcome, Some(asset_id));
        outcome
    }

    /// Adds the sync tag to a batch of settled assets with one
    /// `PUT /tags/assets` (R3.7).
    ///
    /// This is the backfill's entry point: the ids are added in one request,
    /// which the server applies to every asset it still holds. Tagging is
    /// idempotent, so a batch that is sent twice is safe, and an id the server
    /// no longer has is filtered out rather than failing the batch.
    ///
    /// # Errors
    ///
    /// Returns [`TagError`] for a refused or unreachable request, and for an
    /// API key that is missing one of the permissions the tag needs. The reason
    /// stays available through [`Self::status`].
    pub async fn tag_batch(&self, asset_ids: &[String]) -> Result<(), TagError> {
        if !self.enabled() || asset_ids.is_empty() {
            return Ok(());
        }
        // The same settled refusal an upload replays covers a batch (R3.7).
        let epoch = self.session.auth_epoch();
        if let Some(refusal) = self.refusal() {
            return Err(refusal);
        }
        let outcome = self.apply(asset_ids).await;
        self.record(epoch, &outcome, None);
        outcome
    }

    /// The refusal the key already settled on, if any (R3.7).
    fn refusal(&self) -> Option<TagError> {
        let epoch = self.session.auth_epoch();
        let mut refused = self.refused.lock().expect("the tag refusal lock");
        if refused
            .as_ref()
            .is_some_and(|(settled, _)| *settled != epoch)
        {
            *refused = None;
        }
        refused
            .as_ref()
            .map(|(_, permissions)| TagError::MissingPermission {
                permissions: permissions.clone(),
            })
    }

    /// Applies the tag to one or more assets and drops the cached id when the
    /// call fails: a refused id may be a tag the user deleted in Immich, and
    /// the next attempt upserts it again (R3.7).
    async fn apply(&self, asset_ids: &[String]) -> Result<(), TagError> {
        let tag_id = self.resolve().await?;
        let client = self.session.client();
        let asset_ids = asset_ids.to_vec();
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
        match missing_permissions(&key.permissions) {
            permissions if permissions.is_empty() => Ok(()),
            permissions => Err(TagError::MissingPermission { permissions }),
        }
    }

    /// Records the newest outcome for Settings, and logs a failure with its
    /// reason (R3.7, R10.3).
    ///
    /// `epoch` is the session's auth epoch the outcome was measured against, so
    /// a credential change during the request makes the refusal stale.
    fn record(&self, epoch: u32, outcome: &Result<(), TagError>, asset_id: Option<&str>) {
        let problem = outcome.as_ref().err().map(ToString::to_string);
        if let Err(TagError::MissingPermission { permissions }) = outcome {
            // The rest of this credential's uploads replay this instead of
            // asking again (R3.7); a new credential asks again.
            *self.refused.lock().expect("the tag refusal lock") =
                Some((epoch, permissions.clone()));
        }
        if let Some(problem) = &problem {
            tracing::warn!(
                asset_id,
                reason = %problem,
                "R3.7: the sync tag was not applied; the upload stands"
            );
        }
        *self.problem.lock().expect("the tag problem lock") = problem;
    }
}

/// The permissions an API key needs for the sync tag and does not hold
/// (R3.7), in the order of the requests.
///
/// Immich's `all` grants every permission, so a key holding it is missing
/// nothing. Immich's key editor sets its permissions together, so one message
/// names them all rather than making the user grant them one run at a time.
#[must_use]
fn missing_permissions(granted: &[String]) -> Vec<&'static str> {
    if granted.iter().any(|permission| permission == "all") {
        return Vec::new();
    }
    REQUIRED_PERMISSIONS
        .into_iter()
        .filter(|needed| !granted.iter().any(|permission| permission == needed))
        .collect()
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
        assert!(missing_permissions(&granted(&["all"])).is_empty());
    }

    #[test]
    fn a_key_with_the_three_permissions_is_missing_nothing() {
        assert!(
            missing_permissions(&granted(&["tag.create", "tag.asset", "asset.update"])).is_empty()
        );
    }

    #[test]
    fn every_missing_permission_is_named_in_request_order() {
        // Immich's `all` is the only value that covers the others; without it
        // each missing permission is named, in the order the requests need
        // them.
        assert_eq!(
            missing_permissions(&granted(&["asset.update", "tag.asset"])),
            vec!["tag.create"]
        );
        assert_eq!(
            missing_permissions(&granted(&["tag.create", "tag.asset"])),
            vec!["asset.update"]
        );
        assert_eq!(
            missing_permissions(&granted(&["asset.upload"])),
            vec!["tag.create", "tag.asset", "asset.update"]
        );
    }

    #[test]
    fn the_message_names_every_missing_permission() {
        let error = TagError::MissingPermission {
            permissions: vec!["tag.create", "asset.update"],
        };
        assert_eq!(
            error.to_string(),
            "the API key is missing these permissions: tag.create, asset.update"
        );
    }
}
