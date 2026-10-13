//! The one-file upload pipeline (SPEC R3, R3.2, R3.6, R4.5).
//!
//! SHA-1 the file, look the checksum up, upload only what the server accepts,
//! and record the outcome. The pipeline is one file and one attempt: the
//! engine's retry loop owns the backoff and the attempt cap, and the session
//! owns the one re-login of a 401 (R1.4), so a run here either finishes or
//! reports the class the retry policy reads.
//!
//! The shape is what #6 batches: one [`Uploader`] over the shared state store,
//! called once per item from a worker.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use immich_client::{AssetUpload, BulkAction, BulkCheckItem, ClientError, RejectReason};

use crate::connection::SharedSession;
use crate::failure::{FailureClass, FailureKind, classify};
use crate::hash::{HashError, Sha1};
use crate::store::{ItemRecord, ItemState, StateStore};

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

        // R4.5: the same SHA-1 with a known asset does not upload again. A row
        // with no asset id is a previous failure, so it is attempted again.
        if let Ok(Some(item)) = self.store.item(&path_text)
            && item.sha1 == sha1
            && let Some(asset_id) = item.asset_id
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
                            retry_after: None,
                        },
                    );
                };
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
                        retry_after: None,
                    },
                );
            }
        }

        self.send_upload(&mut item, &checksum).await
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
            error_message: None,
            attempts: 0,
        };
        if gate.is_paused() {
            item.state = ItemState::Pending;
            self.record(&item);
            tracing::debug!(path = %item.path, "the upload was deferred while paused");
            return UploadOutcome::Deferred;
        }
        self.record(&item);
        self.send_upload(&mut item, &prepared.sha1.base64()).await
    }

    /// Sends one accepted file and records the outcome (R3.2, R3.6).
    async fn send_upload(&self, item: &mut ItemRecord, checksum: &str) -> UploadOutcome {
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
                };
                async move { client.upload_asset(credential.auth(), &upload).await }
            })
            .await
        {
            Ok(response) => {
                let asset_id = response.asset_id;
                item.asset_id = Some(asset_id.clone());
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
        item.state = match kind {
            FailureKind::Permanent => ItemState::FailedPermanent,
            FailureKind::Transient => ItemState::RetryWait,
        };
        item.error_class = class;
        let message = error.to_string();
        item.error_message = Some(message.clone());
        self.record(item);
        tracing::warn!(
            path = %item.path,
            class = class.map_or("none", FailureClass::as_str),
            permanent = kind == FailureKind::Permanent,
            reason = %error.redacted_reason(),
            "the upload failed"
        );
        UploadOutcome::Failed(UploadFailure {
            class,
            kind,
            message,
            retry_after: error.retry_after(),
            connection: matches!(error, ClientError::Unreachable(_)),
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
