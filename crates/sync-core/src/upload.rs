//! The one-file upload pipeline (SPEC R3, R3.2, R3.3, R3.6, R4.5).
//!
//! SHA-1 the file, look the checksum up, upload only what the server accepts,
//! and record the outcome. The pipeline is one file and one attempt: the
//! engine's retry loop owns the backoff and the attempt cap, and the session
//! owns the one re-login of a 401 (R1.4), so a run here either finishes or
//! reports the class the retry policy reads.
//!
//! The pipeline also carries what belongs to the asset rather than to a file
//! of its own (R3.3): the XMP sidecar rides along in the same request, and a
//! live photo's motion video uploads first so the photo can link it.
//!
//! **A sidecar only reaches an asset that is not on the server yet.** `POST
//! /assets` is the only Stable operation that carries a `sidecarData` part, and
//! Immich answers a checksum it already holds as `duplicate` from the
//! `x-immich-checksum` header, in an interceptor that runs before multer parses
//! the form. The server also deletes the uploaded file when it answers
//! `duplicate` from the body checksum. Nothing attaches a sidecar afterwards:
//! `PUT /assets/:id/original` (`replaceAsset`) is gone in v3.3.1 and
//! `PUT /assets/copy` only moves the sidecar between two assets the client
//! already holds. So a sidecar that appears *after* the asset uploaded cannot
//! be delivered; the client sends it once, parks the item with its asset id,
//! and reports it on the folder rather than claiming a sidecar that never
//! arrived.
//!
//! The shape is what #6 batches: one [`Uploader`] over the shared state store,
//! called once per item from a worker.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use immich_client::{
    AssetUpload, BandwidthLimit, BulkAction, BulkCheckItem, ClientError, RejectReason,
};

use crate::connection::SharedSession;
use crate::failure::{FailureClass, FailureKind, classify};
use crate::hash::{HashError, Sha1};
use crate::pairing::FolderIndex;
use crate::paths::PathCase;
use crate::store::{ItemRecord, ItemState, SidecarDigest, StateStore};

/// What one pipeline run produced (R3.2, R3.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadOutcome {
    /// The server created the asset.
    Created {
        /// The asset id the server returned (R3.6).
        asset_id: String,
    },
    /// The content is already on the server, or the stored SHA-1 says it is
    /// unchanged (R3.2, R4.5). Both count as success.
    Duplicate {
        /// The existing asset id.
        asset_id: String,
    },
    /// The run failed.
    Failed(UploadFailure),
    /// The retry policy spent the attempt cap, so the item is parked until the
    /// user retries it by hand (R6.5).
    DeadLetter(UploadFailure),
    /// New uploads are paused (R8.7). The item stays queued and uploads after
    /// [`UploadGate::resume`].
    Deferred,
}

/// Whether new uploads may start (R8.7).
///
/// Pause stops new upload requests inside the acceptance window; an upload
/// already on the wire finishes. The flag is shared with the engine's worker
/// pool, so a resume takes effect without a restart.
#[derive(Clone, Debug, Default)]
pub struct UploadGate {
    paused: Arc<AtomicBool>,
}

impl UploadGate {
    /// A gate that lets uploads through.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stops new uploads (R8.7).
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    /// Lets new uploads start again (R8.7).
    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    /// Whether new uploads are held right now.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
}

/// The XMP sidecar that rides along with one upload (R3.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarFile {
    /// The sidecar path, sent as the `sidecarData` part.
    pub path: PathBuf,
    /// What the sidecar is: its content digest, its size and its modification
    /// time, recorded once the asset carries it (R4.5).
    pub digest: SidecarDigest,
}

/// One file that was hashed and is ready to upload (R3.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedItem {
    /// The local path.
    pub path: PathBuf,
    /// The size at the time of hashing.
    pub size: u64,
    /// The modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// The SHA-1 of the content.
    pub sha1: Sha1,
    /// The XMP sidecar the asset does not carry yet (R3.3). `None` when there
    /// is no sidecar, or when the asset already carries this exact one.
    pub sidecar: Option<SidecarFile>,
    /// The live photo video paired with this photo (R3.3). It uploads first,
    /// and its asset id is the `livePhotoVideoId` link.
    pub live_photo_video: Option<PathBuf>,
}

/// Why one pipeline run failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadFailure {
    /// The persisted failure class, when one fits (R6.4).
    pub class: Option<FailureClass>,
    /// Whether a later attempt may succeed (R6.3).
    pub kind: FailureKind,
    /// The user-facing reason.
    pub message: String,
    /// The wait the server asked for with a `Retry-After` header (R6.2).
    pub retry_after: Option<Duration>,
    /// Whether the server could not be reached at all (R6.6), as opposed to
    /// answering with a refusal.
    pub connection: bool,
}

impl UploadFailure {
    /// Whether this failure is permanent (R6.3).
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        self.kind == FailureKind::Permanent
    }

    /// The failure one client error is, with its class and retry kind (R6.4).
    ///
    /// One place builds it, so every path that turns a refusal into a parked
    /// row classifies the same way.
    #[must_use]
    pub fn from_client_error(error: &ClientError) -> Self {
        let (class, kind) = classify(error);
        Self {
            class,
            kind,
            message: error.to_string(),
            retry_after: error.retry_after(),
            connection: matches!(error, ClientError::Unreachable(_)),
        }
    }
}

/// What one bulk-check answer means for the item (R3.1, R3.2, R6.3).
#[derive(Debug)]
pub(crate) enum CheckOutcome {
    /// The server does not hold these bytes; upload them.
    Upload,
    /// The server holds them under `asset_id`, and the asset is live.
    Duplicate { asset_id: String },
    /// The server holds them, but the asset is in the trash. The bytes are
    /// not usable until the asset is restored (R5.3).
    Restore { asset_id: String },
    /// The server refused; the failure is already classified.
    Refused(ClientError),
}

/// Maps one bulk-check answer onto the decision both callers act on.
///
/// One place owns the protocol strings and the `is_trashed` rule, so the
/// engine's batch path and the single-file pipeline cannot disagree about
/// what the server meant.
pub(crate) fn check_outcome(action: &BulkAction) -> CheckOutcome {
    match action {
        BulkAction::Accept => CheckOutcome::Upload,
        // A trashed duplicate is not a held asset: re-POSTing the bytes
        // answers `duplicate` from the checksum header before the body is
        // read, so the only way back to a live asset is a restore (R5.3).
        BulkAction::Reject {
            reason: RejectReason::Duplicate,
            asset_id: Some(asset_id),
            is_trashed: true,
        } => CheckOutcome::Restore {
            asset_id: asset_id.clone(),
        },
        BulkAction::Reject {
            reason: RejectReason::Duplicate,
            asset_id: Some(asset_id),
            is_trashed: false,
        } => CheckOutcome::Duplicate {
            asset_id: asset_id.clone(),
        },
        BulkAction::Reject {
            reason: RejectReason::Duplicate,
            asset_id: None,
            ..
        } => CheckOutcome::Refused(ClientError::Protocol {
            status: 200,
            message: "the server reported a duplicate without an asset id".to_string(),
            retry_after: None,
        }),
        BulkAction::Reject {
            reason: RejectReason::UnsupportedFormat,
            ..
        } => CheckOutcome::Refused(ClientError::UnsupportedFormat(
            "the checksum check refused the format".to_string(),
        )),
        BulkAction::Reject {
            reason: RejectReason::Other(reason),
            ..
        } => CheckOutcome::Refused(ClientError::Protocol {
            status: 200,
            message: format!("the checksum check refused it: {reason}"),
            retry_after: None,
        }),
    }
}

/// Runs the pipeline for one file over one shared state store.
#[derive(Debug)]
pub struct Uploader {
    session: SharedSession,
    store: Arc<StateStore>,
}

impl Uploader {
    /// Builds a pipeline over the session and the app-scoped state store.
    #[must_use]
    pub fn new(session: SharedSession, store: Arc<StateStore>) -> Self {
        Self { session, store }
    }

    /// Hashes `path`, checks the checksum, uploads it when accepted, and records
    /// the outcome.
    ///
    /// The XMP sidecar and the live photo video are resolved here from the
    /// folder the file sits in (R3.3), so a one-file upload — the manual
    /// upload view, or the live photo's own video — pairs the same way the
    /// engine does.
    ///
    /// # Errors
    ///
    /// Every failure is returned as [`UploadOutcome::Failed`]; a local read
    /// failure leaves no row, because there is no hash to record.
    pub async fn upload_one(&self, path: &Path) -> UploadOutcome {
        let path_text = path.to_string_lossy().into_owned();
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(source) => return self.local_failure(&path_text, source),
        };
        let size = metadata.len();
        let mtime_ns = system_time_ns(metadata.modified().ok());
        let sha1 = match hash_in_background(path.to_path_buf()).await {
            Ok(sha1) => sha1,
            Err(error) => return self.local_hash_failure(&path_text, error),
        };

        let existing = self.store.item(&path_text).ok().flatten();
        let (sidecar, live_photo_video) = self.paired_with(path, existing.as_ref()).await;

        // R4.5: the same SHA-1 with a known asset does not upload again. A row
        // with no asset id is a previous failure, so it is attempted again.
        // A sidecar the asset does not carry yet is work even so (R3.3).
        if let Some(row) = &existing
            && row.sha1 == sha1
            && sidecar.is_none()
            && let Some(asset_id) = row.asset_id.clone()
        {
            tracing::info!(
                path = %path_text,
                outcome = "duplicate",
                "the SHA-1 is unchanged, so nothing is uploaded"
            );
            return UploadOutcome::Duplicate { asset_id };
        }

        // The row this run will write, with the identity already known.
        let mut item = ItemRecord {
            path: path_text.clone(),
            size,
            mtime_ns,
            sha1,
            asset_id: None,
            state: ItemState::RetryWait,
            error_class: None,
            error_message: None,
            attempts: 0,
            // R4.5: no sidecar has been sent yet. Only the request that
            // carried it and was accepted may claim one.
            sidecar: None,
        };

        let checksum = sha1.base64();
        let client = self.session.client();
        let check = self
            .session
            .request(|credential| {
                let client = client.clone();
                let items = vec![BulkCheckItem {
                    id: path_text.clone(),
                    checksum: checksum.clone(),
                }];
                async move { client.bulk_upload_check(credential.auth(), &items).await }
            })
            .await;
        let action = match check {
            Ok(results) => match results.into_iter().find(|result| result.id == path_text) {
                Some(result) => result.action,
                None => {
                    return self.failed(
                        &mut item,
                        &ClientError::Protocol {
                            status: 200,
                            message: "the checksum check answered for a different id".to_string(),
                            retry_after: None,
                        },
                        false,
                    );
                }
            },
            Err(error) => {
                return self.failed(&mut item, &error, false);
            }
        };

        match check_outcome(&action) {
            CheckOutcome::Upload => {}
            CheckOutcome::Duplicate { asset_id } => {
                // R3.3: a duplicate whose asset is live still has to carry a
                // sidecar the server does not have, and POST /assets is the
                // only Stable way to send one. The server answers duplicate
                // again, so no second asset is created.
                if sidecar.is_none() {
                    item.asset_id = Some(asset_id.clone());
                    item.state = ItemState::Duplicate;
                    self.record(&item);
                    tracing::info!(
                        path = %path_text,
                        asset_id = %asset_id,
                        outcome = "duplicate",
                        "the server already has these bytes"
                    );
                    return UploadOutcome::Duplicate { asset_id };
                }
                item.asset_id = Some(asset_id);
            }
            CheckOutcome::Restore { asset_id } => {
                // The bytes are in the trash. A re-POST cannot bring them
                // back, so restore first; a sidecar then follows the ordinary
                // duplicate path, which parks it with its reason (R3.3).
                if let Err(error) = self.restore_asset(&asset_id).await {
                    return self.failed(&mut item, &error, false);
                }
                if sidecar.is_none() {
                    item.asset_id = Some(asset_id.clone());
                    item.state = ItemState::Duplicate;
                    self.record(&item);
                    tracing::info!(
                        path = %path_text,
                        asset_id = %asset_id,
                        outcome = "restored",
                        "the trashed asset is live again"
                    );
                    return UploadOutcome::Duplicate { asset_id };
                }
                item.asset_id = Some(asset_id);
            }
            CheckOutcome::Refused(error) => return self.failed(&mut item, &error, false),
        }

        // This path is the caller's own, not the engine's: no limit is in
        // force, so the body streams at the network's pace.
        self.send_upload(
            &mut item,
            &checksum,
            sidecar.as_ref(),
            live_photo_video.as_deref(),
            &BandwidthLimit::default(),
        )
        .await
    }

    /// Uploads a file the caller already hashed and the server already
    /// accepted, marking the row `uploading` first so a crash mid-request
    /// leaves it re-queueable (R6.7).
    ///
    /// The gate is checked immediately before the request, so a pause stops
    /// the next upload even inside an already-dispatched worker (R8.7). The
    /// bandwidth limit is live, so the request streams under whatever limit is
    /// set when its bytes are read (R7.1).
    pub async fn upload_prepared(
        &self,
        prepared: &PreparedItem,
        gate: &UploadGate,
        bandwidth: &BandwidthLimit,
    ) -> UploadOutcome {
        let path = prepared.path.to_string_lossy().into_owned();
        // R4.5: the digest a previous attempt recorded. The row keeps it while
        // this attempt runs, and a sidecar with the same digest is not sent
        // again: a promote failure leaves the item retrying after the server
        // already read the sidecar, and re-sending it is answered `duplicate`
        // from the checksum header, which parks the item with a false "could
        // not be attached".
        let recorded = self
            .store
            .item(&path)
            .ok()
            .flatten()
            .and_then(|row| row.sidecar);
        let mut item = ItemRecord {
            path,
            size: prepared.size,
            mtime_ns: prepared.mtime_ns,
            sha1: prepared.sha1,
            asset_id: None,
            state: ItemState::Uploading,
            error_class: None,
            error_message: None,
            attempts: 0,
            sidecar: recorded,
        };
        if gate.is_paused() {
            item.state = ItemState::Pending;
            self.record(&item);
            tracing::debug!(path = %item.path, "the upload was deferred while paused");
            return UploadOutcome::Deferred;
        }
        self.record(&item);
        let sidecar = prepared
            .sidecar
            .as_ref()
            .filter(|sidecar| recorded != Some(sidecar.digest));
        self.send_upload(
            &mut item,
            &prepared.sha1.base64(),
            sidecar,
            prepared.live_photo_video.as_deref(),
            bandwidth,
        )
        .await
    }

    /// Brings one trashed asset back from the trash (R5.3).
    ///
    /// `POST /trash/restore/assets` is the Stable operation that clears
    /// `deletedAt`. A checksum the server already knows cannot be uploaded
    /// into a live asset: the upload interceptor answers `duplicate` even when
    /// the asset is trashed, and the unique index ignores `deletedAt`.
    async fn restore_asset(&self, asset_id: &str) -> Result<(), ClientError> {
        let client = self.session.client();
        let ids = vec![asset_id.to_string()];
        self.session
            .request(move |credential| {
                let client = client.clone();
                let ids = ids.clone();
                async move { client.restore_assets(credential.auth(), &ids).await }
            })
            .await
    }

    /// The XMP sidecar and the live photo video that belong to `path` (R3.3).
    ///
    /// `row` is what the store recorded for this item: the sidecar digest the
    /// asset already carries, and the state that says whether another attempt
    /// is worth sending one.
    async fn paired_with(
        &self,
        path: &Path,
        row: Option<&ItemRecord>,
    ) -> (Option<SidecarFile>, Option<PathBuf>) {
        let case = PathCase::for_platform();
        let index = FolderIndex::read(std::slice::from_ref(&path.to_path_buf()));
        let sidecar = pending_sidecar(path, &index, case, row).await;
        let live_photo_video = index
            .live_photo_video_for(path, case)
            .map(Path::to_path_buf);
        (sidecar, live_photo_video)
    }

    /// The asset id of the live photo video for `photo`, uploading it when the
    /// server does not hold it yet (R3.3).
    ///
    /// The store is the lookup, so a video uploaded by an earlier pass or an
    /// earlier run is found without sending it again.
    async fn live_photo_video_id(&self, photo: &Path, video: &Path) -> Option<String> {
        if let Ok(Some(row)) = self.store.item(&video.to_string_lossy())
            && row.state.is_terminal_success()
            && let Some(asset_id) = row.asset_id.clone()
        {
            tracing::debug!(path = %video.display(), asset_id = %asset_id, "the live photo video is already on the server");
            return Some(asset_id);
        }
        // The video goes through the same pipeline, which resolves its own
        // pairing; a video has no video sibling, so the recursion stops here.
        // The indirection is boxed only to break the async type cycle.
        match Box::pin(self.upload_one(video)).await {
            UploadOutcome::Created { asset_id } | UploadOutcome::Duplicate { asset_id } => {
                Some(asset_id)
            }
            UploadOutcome::Failed(failure) => {
                tracing::warn!(
                    photo = %photo.display(),
                    video = %video.display(),
                    permanent = failure.is_permanent(),
                    class = failure.class.map_or("none", FailureClass::as_str),
                    "the live photo video did not upload; the photo goes up unlinked"
                );
                None
            }
            // The retry policy gave up on the video, so there is no asset to
            // link: the photo goes up unlinked rather than being lost with it.
            UploadOutcome::DeadLetter(_) | UploadOutcome::Deferred => None,
        }
    }

    /// Sends one accepted file and records the outcome (R3.2, R3.3, R3.6).
    async fn send_upload(
        &self,
        item: &mut ItemRecord,
        checksum: &str,
        sidecar: Option<&SidecarFile>,
        live_photo_video: Option<&Path>,
        bandwidth: &BandwidthLimit,
    ) -> UploadOutcome {
        // R3.3: the motion video uploads first, so the link never names an
        // asset the server does not have. A video that fails is parked by its
        // own upload, and the photo still goes up without the link rather than
        // being lost with it.
        let live_photo_video_id = match live_photo_video {
            Some(video) => self.live_photo_video_id(Path::new(&item.path), video).await,
            None => None,
        };
        // Immich requires the file's own dates on the asset (R3.1). The
        // creation time is missing on some platforms and filesystems; the
        // modification time is the honest fallback.
        let metadata = std::fs::metadata(Path::new(&item.path)).ok();
        let modified_at = metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let created_at = metadata
            .as_ref()
            .and_then(|metadata| metadata.created().ok())
            .unwrap_or(modified_at);
        let checksum = checksum.to_string();
        let client = self.session.client();
        let path = Path::new(&item.path);
        match self
            .session
            .request(|credential| {
                let client = client.clone();
                let upload = AssetUpload {
                    path,
                    checksum: &checksum,
                    created_at,
                    modified_at,
                    sidecar: sidecar.map(|sidecar| sidecar.path.as_path()),
                    live_photo_video_id: live_photo_video_id.as_deref(),
                    bandwidth: bandwidth.clone(),
                };
                async move { client.upload_asset(credential.auth(), &upload).await }
            })
            .await
        {
            Ok(response) => {
                let asset_id = response.asset_id;
                item.asset_id = Some(asset_id.clone());
                // R4.5 + the API limitation: only a request the server read
                // may claim the sidecar it carried. A `duplicate` answer was
                // made from the `x-immich-checksum` header in an interceptor
                // that runs before multer parses the form, so the sidecar in
                // this body was discarded, and `POST /assets` is the only
                // Stable operation that carries one — there is no attach, and
                // `PUT /assets/:id/original` (which dropped `sidecarFile`) is
                // gone in v3.3.1. Another automatic attempt would be answered
                // the same way, so the item parks with its asset id, the
                // sidecar stays owed, and the folder says why.
                if response.duplicate
                    && let Some(sidecar) = sidecar
                {
                    let sidecar_path = sidecar.path.display().to_string();
                    item.sidecar = None;
                    item.state = ItemState::FailedPermanent;
                    item.error_class = None;
                    let message = format!(
                        "{sidecar_path} could not be attached: the server already has \
                         these bytes and its API has no Stable way to send a sidecar to an \
                         asset it already holds"
                    );
                    // R6.5: the reason goes on the row, so the failed-items view
                    // shows it beside the class after a restart.
                    item.error_message = Some(message.clone());
                    self.record(item);
                    tracing::warn!(
                        path = %item.path,
                        asset_id = %asset_id,
                        sidecar = %sidecar_path,
                        "the server answered duplicate without reading the body, so the sidecar was not sent; Immich cannot attach one to an asset that already exists"
                    );
                    return UploadOutcome::Failed(UploadFailure {
                        class: None,
                        kind: FailureKind::Permanent,
                        message,
                        retry_after: None,
                        connection: false,
                    });
                }
                // R4.5: a request the server read carries the sidecar it
                // carried. Without one the row keeps the digest it already
                // recorded: erasing it would make a later pass offer a sidecar
                // the asset already has. A `duplicate` answer never reaches
                // this line with a sidecar: the branch above parked the item.
                if let Some(sidecar) = sidecar {
                    item.sidecar = Some(sidecar.digest);
                }
                item.state = if response.duplicate {
                    ItemState::Duplicate
                } else {
                    ItemState::Uploaded
                };
                self.record(item);
                let outcome = if response.duplicate {
                    "duplicate"
                } else {
                    "created"
                };
                tracing::info!(
                    path = %item.path,
                    asset_id = %asset_id,
                    outcome,
                    "the upload finished"
                );
                if response.duplicate {
                    UploadOutcome::Duplicate { asset_id }
                } else {
                    UploadOutcome::Created { asset_id }
                }
            }
            Err(error) => self.failed(item, &error, sidecar.is_some()),
        }
    }

    /// Records a client failure with its class and state, and returns it.
    ///
    /// `carried` says whether the failed attempt sent a sidecar. Only then may
    /// the recorded digest be dropped: an attempt that carried none (the row
    /// already recorded this sidecar's digest, so it was filtered out) leaves
    /// the digest in place, and a later pass does not offer the sidecar the
    /// asset already carries (R4.5).
    fn failed(&self, item: &mut ItemRecord, error: &ClientError, carried: bool) -> UploadOutcome {
        let failure = UploadFailure::from_client_error(error);
        item.asset_id = None;
        item.sidecar = if carried { None } else { item.sidecar };
        item.state = match failure.kind {
            FailureKind::Permanent => ItemState::FailedPermanent,
            FailureKind::Transient => ItemState::RetryWait,
        };
        item.error_class = failure.class;
        item.error_message = Some(failure.message.clone());
        self.record(item);
        tracing::warn!(
            path = %item.path,
            class = failure.class.map_or("none", FailureClass::as_str),
            permanent = failure.is_permanent(),
            reason = %error.redacted_reason(),
            "the upload failed"
        );
        UploadOutcome::Failed(failure)
    }

    /// A failure before the file could be hashed. It leaves no row: the schema
    /// needs a SHA-1, and an unread file has none.
    fn local_failure(&self, path: &str, source: std::io::Error) -> UploadOutcome {
        let message = format!("the file {path} could not be read: {source}");
        tracing::warn!(
            path,
            class = FailureClass::LocalReadError.as_str(),
            permanent = true,
            error = %source,
            "the upload failed before hashing"
        );
        UploadOutcome::Failed(UploadFailure {
            class: Some(FailureClass::LocalReadError),
            kind: FailureKind::Permanent,
            message,
            retry_after: None,
            connection: false,
        })
    }

    /// A hashing failure, which is the same local read class as a metadata
    /// failure.
    fn local_hash_failure(&self, path: &str, error: HashError) -> UploadOutcome {
        self.local_failure(path, std::io::Error::other(error.to_string()))
    }

    /// Writes one row. A store failure does not change the outcome: the upload
    /// already happened, and a later run re-checks the server.
    fn record(&self, item: &ItemRecord) {
        if let Err(error) = self.store.record(item) {
            tracing::error!(
                path = %item.path,
                error = %error,
                "the state database could not record the outcome"
            );
        }
    }
}

/// Hashes the file on a blocking thread, so the async worker is not stalled by
/// the disk (rust-skills async-spawn-blocking).
pub(crate) async fn hash_in_background(path: PathBuf) -> Result<Sha1, HashError> {
    let display = path.display().to_string();
    tokio::task::spawn_blocking(move || crate::hash::sha1_file(&path))
        .await
        .unwrap_or_else(|join| {
            Err(HashError::Read {
                path: display,
                source: std::io::Error::other(join.to_string()),
            })
        })
}

/// The XMP sidecar for `media` that the asset does not carry yet (R3.3).
///
/// `row` is what the store recorded for this item. A sidecar that still has the
/// size and modification time the store recorded has reached the server and is
/// not sent again (R4.5), and neither is one whose bytes still hash to the
/// recorded digest: a `touch`, a backup restore or a tool that rewrites XMP
/// metadata moves the modification time and changes nothing else. An item
/// parked with an asset gave up on its sidecar, because the only Stable way to
/// carry one is answered `duplicate` before the server reads the body (see
/// [`Uploader::send_upload`]). A sidecar that cannot be read is left for a
/// later pass rather than failing the media file with it.
///
/// shortcut: the size and mtime are trusted when both match, so a sidecar
/// rewritten within the same size and the same mtime tick is not sent again;
/// hash it there too when that edge matters more than the scan cost.
pub(crate) async fn pending_sidecar(
    media: &Path,
    index: &FolderIndex,
    case: PathCase,
    row: Option<&ItemRecord>,
) -> Option<SidecarFile> {
    if row.is_some_and(parked_with_an_asset) {
        return None;
    }
    let found = index.sidecar_for(media, case)?.to_path_buf();
    let metadata = std::fs::metadata(&found).ok()?;
    let size = metadata.len();
    let mtime_ns = system_time_ns(metadata.modified().ok());
    // R4.5: an unchanged sidecar is not read again. Comparing the size and
    // mtime the store recorded is what keeps a settled library from re-hashing
    // every sidecar on every reconciliation scan — the same trade the media
    // file's own columns make.
    if row
        .and_then(|row| row.sidecar)
        .is_some_and(|sent| sent.size == size && sent.mtime_ns == mtime_ns)
    {
        return None;
    }
    let sha1 = match hash_in_background(found.clone()).await {
        Ok(sha1) => sha1,
        Err(error) => {
            tracing::warn!(
                path = %found.display(),
                error = %error,
                "the XMP sidecar could not be read; the asset goes up without it"
            );
            return None;
        }
    };
    // R4.5: the digest is what says the asset carries the sidecar, whatever the
    // metadata says. Bytes that hash to the recorded digest are already sent:
    // the server answers `duplicate` from the checksum header before it reads
    // the body, which parks the item claiming a sidecar it does carry could
    // not be attached.
    if row
        .and_then(|row| row.sidecar)
        .is_some_and(|sent| sent.sha1 == sha1)
    {
        return None;
    }
    Some(SidecarFile {
        path: found,
        digest: SidecarDigest {
            sha1,
            size,
            mtime_ns,
        },
    })
}

/// Whether the item gave up on its sidecar: it is parked permanently while its
/// asset is already on the server (R3.3).
///
/// shortcut: once parked, the item stops offering its sidecar for good, even
/// after the sidecar is edited; re-offering it needs the row cleared, which
/// #7's manual retry will own.
fn parked_with_an_asset(row: &ItemRecord) -> bool {
    row.state == ItemState::FailedPermanent && row.asset_id.is_some()
}

/// A modification time as nanoseconds since the Unix epoch.
///
/// A file older than 1970 is negative. A platform that reports no modification
/// time becomes 0.
fn system_time_ns(time: Option<SystemTime>) -> i64 {
    let Some(time) = time else {
        return 0;
    };
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a row carries the identity of the sidecar the asset carries.
    fn sent_sidecar_of(sha1: &str, path: &Path) -> SidecarDigest {
        let metadata = std::fs::metadata(path).expect("read the sidecar fixture");
        SidecarDigest {
            sha1: Sha1::from_hex(sha1).expect("a hex SHA-1"),
            size: metadata.len(),
            mtime_ns: system_time_ns(metadata.modified().ok()),
        }
    }

    /// `sha1("hello world")`, hex.
    const HELLO_WORLD_SHA1_HEX: &str = "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed";

    /// A sidecar fixture, and the digest of its exact bytes as a literal.
    const SIDECAR: &[u8] = b"<x:xmpmeta>sidecar</x:xmpmeta>";
    const SIDECAR_SHA1_HEX: &str = "f8d2e9cfb0466aeebda758acb4dbbdfa7ce77140";

    /// Different bytes, same length as [`SIDECAR`], and a different digest.
    const REWRITTEN_SIDECAR: &[u8] = b"<x:xmpmeta>tagged!</x:xmpmeta>";

    /// The digest of [`REWRITTEN_SIDECAR`]'s exact bytes, as a literal.
    const REWRITTEN_SIDECAR_SHA1_HEX: &str = "76af39006095c46ffb85cefc8132787804403e45";

    /// The media, its sidecar, and the index that pairs them.
    fn paired_folder(temp: &Path) -> (PathBuf, PathBuf, FolderIndex) {
        let folder = temp.to_path_buf();
        let media = folder.join("IMG_0001.jpg");
        std::fs::write(&media, b"photo bytes").expect("write the media fixture");
        let sidecar = folder.join("IMG_0001.jpg.xmp");
        std::fs::write(&sidecar, SIDECAR).expect("write the sidecar fixture");
        let index = FolderIndex::read(std::slice::from_ref(&media));
        (media, sidecar, index)
    }

    /// Moves `path`'s modification time two seconds on, leaving its bytes alone,
    /// the way a `touch`, a backup restore or a metadata rewrite does.
    fn bump_mtime(path: &Path) {
        let modified = std::fs::metadata(path)
            .expect("read the sidecar fixture")
            .modified()
            .expect("a modification time");
        std::fs::File::options()
            .write(true)
            .open(path)
            .expect("reopen the sidecar")
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(modified + std::time::Duration::from_secs(2)),
            )
            .expect("move the modification time");
    }

    /// A row that sent `sent`, and nothing else the rule looks at.
    fn row_with(media: &Path, sent: Option<SidecarDigest>) -> ItemRecord {
        ItemRecord {
            path: media.display().to_string(),
            size: 1,
            mtime_ns: 0,
            sha1: Sha1::from_hex(HELLO_WORLD_SHA1_HEX).expect("a hex SHA-1"),
            asset_id: Some("asset-1".to_string()),
            state: ItemState::Uploaded,
            error_class: None,
            error_message: None,
            attempts: 0,
            sidecar: sent,
        }
    }

    #[tokio::test]
    async fn a_sidecar_with_the_recorded_size_and_mtime_is_not_read_again() {
        // R4.5: a settled library is not re-read on every reconciliation scan, and
        // that has to hold for the sidecars too — a large library would re-hash
        // every sidecar every scan interval otherwise.
        //
        // The fixture replaces the sidecar's bytes with different ones of the same
        // length and puts the modification time back, so a client that hashes to
        // decide would still see a changed digest. The rule trusts size and mtime,
        // exactly as the media file's own columns do.
        let temp = tempfile::tempdir().expect("create a temp dir");
        let (media, sidecar, index) = paired_folder(temp.path());
        let sent = sent_sidecar_of(SIDECAR_SHA1_HEX, &sidecar);
        let modified = std::fs::metadata(&sidecar)
            .expect("read the sidecar fixture")
            .modified()
            .expect("a modification time");
        std::fs::write(&sidecar, REWRITTEN_SIDECAR).expect("rewrite the sidecar");
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .expect("reopen the sidecar")
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .expect("restore the modification time");
        assert_eq!(std::fs::metadata(&sidecar).expect("read").len(), sent.size);
        assert_ne!(
            crate::hash::sha1_file(&sidecar)
                .expect("hash the rewritten sidecar")
                .hex(),
            SIDECAR_SHA1_HEX,
            "the bytes really are different, so a hashing client would re-send"
        );

        let pending = pending_sidecar(
            &media,
            &index,
            PathCase::for_platform(),
            Some(&row_with(&media, Some(sent))),
        )
        .await;

        assert!(
            pending.is_none(),
            "the sidecar still has the size and mtime the store recorded: {pending:?}"
        );
    }

    #[tokio::test]
    async fn a_sidecar_edited_since_it_was_sent_is_pending_again() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let (media, sidecar, index) = paired_folder(temp.path());
        let sent = sent_sidecar_of(SIDECAR_SHA1_HEX, &sidecar);
        // The bytes really change: same length, different content, in a file
        // whose modification time moved.
        std::fs::write(&sidecar, REWRITTEN_SIDECAR).expect("rewrite the sidecar");
        bump_mtime(&sidecar);

        let pending = pending_sidecar(
            &media,
            &index,
            PathCase::for_platform(),
            Some(&row_with(&media, Some(sent))),
        )
        .await;

        assert_eq!(
            pending.map(|sidecar| (sidecar.digest.sha1.hex(), sidecar.digest.size)),
            Some((REWRITTEN_SIDECAR_SHA1_HEX.to_string(), sent.size)),
            "different bytes are a sidecar the asset does not carry"
        );
    }

    #[tokio::test]
    async fn a_sidecar_whose_bytes_match_is_not_pending_after_a_touch() {
        // R4.5: the digest, not the modification time, says the asset already
        // carries the sidecar. A `touch`, a backup restore or a tool that
        // rewrites XMP metadata moves the mtime and leaves the bytes alone, and
        // offering the sidecar again answers `duplicate` from the checksum
        // header, which parks the item claiming it could not be attached.
        let temp = tempfile::tempdir().expect("create a temp dir");
        let (media, sidecar, index) = paired_folder(temp.path());
        let sent = sent_sidecar_of(SIDECAR_SHA1_HEX, &sidecar);
        bump_mtime(&sidecar);

        let pending = pending_sidecar(
            &media,
            &index,
            PathCase::for_platform(),
            Some(&row_with(&media, Some(sent))),
        )
        .await;

        assert!(
            pending.is_none(),
            "the same bytes in a moved file is still the sidecar the asset carries: {pending:?}"
        );
    }

    #[tokio::test]
    async fn a_sidecar_is_pending_when_the_row_sent_none() {
        let temp = tempfile::tempdir().expect("create a temp dir");
        let (media, _, index) = paired_folder(temp.path());

        let pending = pending_sidecar(
            &media,
            &index,
            PathCase::for_platform(),
            Some(&row_with(&media, None)),
        )
        .await;

        assert_eq!(
            pending.map(|sidecar| sidecar.digest.sha1.hex()),
            Some(SIDECAR_SHA1_HEX.to_string()),
            "a sidecar the asset never carried is pending"
        );
    }

    #[tokio::test]
    async fn a_parked_item_with_an_asset_is_not_offered_a_sidecar() {
        // R3.3: `POST /assets` is the only Stable way to carry one and the server
        // answers `duplicate` before it reads the body, so a parked item stops
        // offering the same sidecar for ever.
        let temp = tempfile::tempdir().expect("create a temp dir");
        let (media, sidecar, index) = paired_folder(temp.path());
        let mut row = row_with(&media, None);
        row.state = ItemState::FailedPermanent;

        let pending = pending_sidecar(&media, &index, PathCase::for_platform(), Some(&row)).await;

        assert!(pending.is_none(), "{pending:?}");
        let _ = sidecar;
    }

    #[test]
    fn a_row_before_the_sidecar_columns_is_read_as_sending_no_sidecar() {
        // The migration leaves the columns NULL, which must mean "unknown", not a
        // sidecar with no size: the worst it costs is one more hash.
        let temp = tempfile::tempdir().expect("create a temp dir");
        let (media, _, _) = paired_folder(temp.path());
        let store = StateStore::open(temp.path().join("state.db")).expect("open the store");
        store
            .record(&ItemRecord {
                path: media.display().to_string(),
                size: 1,
                mtime_ns: 0,
                sha1: Sha1::from_hex(HELLO_WORLD_SHA1_HEX).expect("a hex SHA-1"),
                asset_id: None,
                state: ItemState::Uploaded,
                error_class: None,
                error_message: None,
                attempts: 0,
                sidecar: None,
            })
            .expect("record the row");

        assert_eq!(
            store
                .item(&media.display().to_string())
                .expect("read the row")
                .expect("the row exists")
                .sidecar,
            None
        );
    }
}
