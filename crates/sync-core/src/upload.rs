//! The one-file upload pipeline (SPEC R3, R3.2, R3.3, R3.6, R4.5).
//!
//! SHA-1 the file, look the checksum up, upload only what the server accepts,
//! and record the outcome. The pipeline is one file and one attempt: it has no
//! retry loop, because #7 owns classification-backed retries. It records the
//! class and the state that the retry work will read.
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
use std::time::{SystemTime, UNIX_EPOCH};

use immich_client::{
    AssetUpload, BulkAction, BulkCheckItem, ClientError, ImmichClient, RejectReason,
};

use crate::connection::RequestCredential;
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
}

impl UploadFailure {
    /// Whether this failure is permanent (R6.3).
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        self.kind == FailureKind::Permanent
    }
}

/// Runs the pipeline for one file over one shared state store.
#[derive(Debug)]
pub struct Uploader {
    client: ImmichClient,
    store: Arc<StateStore>,
}

impl Uploader {
    /// Builds a pipeline over the client and the app-scoped state store.
    #[must_use]
    pub fn new(client: ImmichClient, store: Arc<StateStore>) -> Self {
        Self { client, store }
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
    pub async fn upload_one(&self, path: &Path, credential: &RequestCredential) -> UploadOutcome {
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
            // R4.5: no sidecar has been sent yet. Only the request that
            // carried it and was accepted may claim one.
            sidecar: None,
        };

        let checksum = sha1.base64();
        let check = self
            .client
            .bulk_upload_check(
                credential.auth(),
                &[BulkCheckItem {
                    id: path_text.clone(),
                    checksum: checksum.clone(),
                }],
            )
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
                        },
                    );
                }
            },
            Err(error) => {
                return self.failed(&mut item, &error);
            }
        };

        match action {
            BulkAction::Accept => {}
            BulkAction::Reject {
                reason: RejectReason::Duplicate,
                asset_id,
                ..
            } => {
                let Some(asset_id) = asset_id else {
                    return self.failed(
                        &mut item,
                        &ClientError::Protocol {
                            status: 200,
                            message: "the server reported a duplicate without an asset id"
                                .to_string(),
                        },
                    );
                };
                // R3.3: a duplicate the asset already holds still has to carry
                // a sidecar the server does not have, and POST /assets is the
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
            BulkAction::Reject {
                reason: RejectReason::UnsupportedFormat,
                ..
            } => {
                return self.failed(
                    &mut item,
                    &ClientError::UnsupportedFormat("the checksum check refused the format".into()),
                );
            }
            BulkAction::Reject {
                reason: RejectReason::Other(reason),
                ..
            } => {
                return self.failed(
                    &mut item,
                    &ClientError::Protocol {
                        status: 200,
                        message: format!("the checksum check refused it: {reason}"),
                    },
                );
            }
        }

        self.send_upload(
            &mut item,
            &checksum,
            sidecar.as_ref(),
            live_photo_video.as_deref(),
            credential,
        )
        .await
    }

    /// Uploads a file the caller already hashed and the server already
    /// accepted, marking the row `uploading` first so a crash mid-request
    /// leaves it re-queueable (R6.7).
    ///
    /// The gate is checked immediately before the request, so a pause stops
    /// the next upload even inside an already-dispatched worker (R8.7).
    pub async fn upload_prepared(
        &self,
        prepared: &PreparedItem,
        credential: &RequestCredential,
        gate: &UploadGate,
    ) -> UploadOutcome {
        let mut item = ItemRecord {
            path: prepared.path.to_string_lossy().into_owned(),
            size: prepared.size,
            mtime_ns: prepared.mtime_ns,
            sha1: prepared.sha1,
            asset_id: None,
            state: ItemState::Uploading,
            error_class: None,
            // R4.5: the request carrying the sidecar has not gone out yet.
            sidecar: None,
        };
        if gate.is_paused() {
            item.state = ItemState::Pending;
            self.record(&item);
            tracing::debug!(path = %item.path, "the upload was deferred while paused");
            return UploadOutcome::Deferred;
        }
        self.record(&item);
        self.send_upload(
            &mut item,
            &prepared.sha1.base64(),
            prepared.sidecar.as_ref(),
            prepared.live_photo_video.as_deref(),
            credential,
        )
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
    async fn live_photo_video_id(
        &self,
        photo: &Path,
        video: &Path,
        credential: &RequestCredential,
    ) -> Option<String> {
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
        match Box::pin(self.upload_one(video, credential)).await {
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
            UploadOutcome::Deferred => None,
        }
    }

    /// Sends one accepted file and records the outcome (R3.2, R3.6).
    async fn send_upload(
        &self,
        item: &mut ItemRecord,
        checksum: &str,
        sidecar: Option<&SidecarFile>,
        live_photo_video: Option<&Path>,
        credential: &RequestCredential,
    ) -> UploadOutcome {
        // R3.3: the motion video uploads first, so the link never names an
        // asset the server does not have. A video that fails is parked by its
        // own upload, and the photo still goes up without the link rather than
        // being lost with it.
        let live_photo_video_id = match live_photo_video {
            Some(video) => {
                self.live_photo_video_id(Path::new(&item.path), video, credential)
                    .await
            }
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
        match self
            .client
            .upload_asset(
                credential.auth(),
                &AssetUpload {
                    path: Path::new(&item.path),
                    checksum,
                    created_at,
                    modified_at,
                    sidecar: sidecar.map(|sidecar| sidecar.path.as_path()),
                    live_photo_video_id: live_photo_video_id.as_deref(),
                },
            )
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
                        message: format!(
                            "{sidecar_path} could not be attached: the server already has \
                             these bytes and its API has no Stable way to send a sidecar to an \
                             asset it already holds"
                        ),
                    });
                }
                // R4.5: this request was read, so the asset carries the sidecar
                // it carried. A `duplicate` answer never reaches this line
                // with one: the branch above parked the item instead.
                item.sidecar = sidecar.map(|sidecar| sidecar.digest);
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
            Err(error) => self.failed(item, &error),
        }
    }

    /// Records a client failure with its class and state, and returns it.
    fn failed(&self, item: &mut ItemRecord, error: &ClientError) -> UploadOutcome {
        let (class, kind) = classify(error);
        item.asset_id = None;
        // R4.5: the request never landed, so neither did the sidecar.
        item.sidecar = None;
        item.state = match kind {
            FailureKind::Permanent => ItemState::FailedPermanent,
            FailureKind::Transient => ItemState::RetryWait,
        };
        item.error_class = class;
        self.record(item);
        tracing::warn!(
            path = %item.path,
            class = class.map_or("none", FailureClass::as_str),
            permanent = kind == FailureKind::Permanent,
            reason = %error.redacted_reason(),
            "the upload failed"
        );
        let message = error.to_string();
        UploadOutcome::Failed(UploadFailure {
            class,
            kind,
            message,
        })
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
/// not sent again (R4.5); an item parked with an asset gave up on its sidecar,
/// because the only Stable way to carry one is answered `duplicate` before the
/// server reads the body (see [`Uploader::send_upload`]). A sidecar that cannot
/// be read is left for a later pass rather than failing the media file with it.
///
/// shortcut: the size and mtime are trusted instead of the digest, so a
/// sidecar rewritten within the same size and the same mtime tick is not sent
/// again; compare a stored digest once the scan cost matters more than that
/// edge.
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
        let mut sent = sent_sidecar_of(SIDECAR_SHA1_HEX, &sidecar);
        // One nanosecond of mtime is what the media file's own rule turns on.
        sent.mtime_ns -= 1;

        let pending = pending_sidecar(
            &media,
            &index,
            PathCase::for_platform(),
            Some(&row_with(&media, Some(sent))),
        )
        .await;

        assert_eq!(
            pending.map(|sidecar| sidecar.digest),
            Some(SidecarDigest {
                sha1: Sha1::from_hex(SIDECAR_SHA1_HEX).expect("a hex SHA-1"),
                ..sent_sidecar_of(SIDECAR_SHA1_HEX, &sidecar)
            }),
            "an edited sidecar rides along with the asset again"
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
