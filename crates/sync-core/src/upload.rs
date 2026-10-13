//! The one-file upload pipeline (SPEC R3, R3.2, R3.6, R4.5).
//!
//! SHA-1 the file, look the checksum up, upload only what the server accepts,
//! and record the outcome. The pipeline is one file and one attempt: it has no
//! retry loop, because #7 owns classification-backed retries. It records the
//! class and the state that the retry work will read.
//!
//! The shape is what #6 batches: one [`Uploader`] over the shared state store,
//! called once per item from a worker.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use immich_client::{
    AssetUpload, BulkAction, BulkCheckItem, ClientError, ImmichClient, RejectReason,
};

use crate::connection::RequestCredential;
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
                    },
                );
            }
        }

        match self
            .client
            .upload_asset(
                credential.auth(),
                &AssetUpload {
                    path,
                    checksum: &checksum,
                },
            )
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
                self.record(&item);
                let outcome = if response.duplicate {
                    "duplicate"
                } else {
                    "created"
                };
                tracing::info!(
                    path = %path_text,
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
            Err(error) => self.failed(&mut item, &error),
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
        self.record(item);
        tracing::warn!(
            path = %item.path,
            class = class.map_or("none", FailureClass::as_str),
            permanent = kind == FailureKind::Permanent,
            error = %error,
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
async fn hash_in_background(path: PathBuf) -> Result<Sha1, HashError> {
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
