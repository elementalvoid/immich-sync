//! The sync tag's backfill over the library a client uploaded before the tag
//! was turned on (SPEC R3.7, issue #46).
//!
//! One owner, one persisted state row, one task. The engine wakes this when the
//! setting is turned on and when the user asks for "Tag existing assets now";
//! the run then walks the library the state database holds (every current asset,
//! every old version, and every asset the server answered as a duplicate) and
//! sends a batch of ids per `PUT /tags/assets`. Tagging is idempotent, so a batch
//! that is repeated after a crash is harmless, and an asset the server no longer
//! holds is filtered out by Immich rather than failing its batch.
//!
//! Every completed batch moves the cursor and the count in one store write, so
//! an interruption resumes at the last completed batch instead of starting the
//! library over. Reaching the end records the completion, which keeps a restart
//! from tagging the library again; the record survives both a restart and a
//! toggle, and only "Tag existing assets now" clears it.
//!
//! A transient batch failure is retried with the engine's retry policy (R6.1).
//! A permanent one - a missing permission, a refusal the server will answer the
//! same way again - stops the run, keeps the batches that already landed, and
//! leaves the reason for Settings, so the next attempt continues from the
//! cursor.

use std::sync::Arc;
use std::sync::Mutex;

use crate::backoff::RetryPolicy;
use crate::failure::{FailureKind, classify};
use crate::store::{StateStore, StoreError};
use crate::tags::{BackfillStatus, TagError, Tagger};

/// How many asset ids one `PUT /tags/assets` carries (R3.7).
///
/// A few hundred keeps the request body small (a few tens of kilobytes) while
/// a 50k-asset library costs about 250 requests instead of 50,000. The server
/// applies the whole batch in one statement, and an id it no longer holds is
/// filtered out, so the size only trades request count against body size.
pub const BACKFILL_BATCH: usize = 200;

/// The policy one backfill pass follows (R3.7, R6.1).
#[derive(Clone, Copy, Debug)]
pub struct BackfillPolicy {
    /// How many ids one request carries.
    pub batch: usize,
    /// How many transient failures of one batch are retried before the run
    /// stops with the reason.
    pub attempts: u32,
    /// The wait between those attempts (R6.1).
    pub retry: RetryPolicy,
}

impl Default for BackfillPolicy {
    fn default() -> Self {
        Self {
            batch: BACKFILL_BATCH,
            // The retry policy's ceiling reaches a minute by then, so a server
            // that is down stops being hammered before the run gives up.
            attempts: 8,
            retry: RetryPolicy::default(),
        }
    }
}

/// Runs the sync tag's backfill (R3.7).
///
/// The state database is the truth: the run reads the cursor, the count, and
/// the completion from it before every batch, so a restart, a paused toggle, a
/// "tag now", and a run already in flight cannot disagree about where the
/// backfill stands. The status is the copy Settings renders, refreshed once per
/// batch.
pub struct Backfill {
    store: Arc<StateStore>,
    policy: BackfillPolicy,
    /// What Settings shows. It is a copy of the store's counters, updated by
    /// the run, because the view reads it on the UI thread and must not touch
    /// the database there (N2).
    status: Mutex<BackfillStatus>,
}

impl Backfill {
    /// Reads the recorded state, so a restart publishes what the last run
    /// reached before this one starts (R3.7).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the backfill state cannot be read.
    pub fn new(store: Arc<StateStore>, policy: BackfillPolicy) -> Result<Self, StoreError> {
        let recorded = store.tag_backfill()?;
        let status = BackfillStatus {
            tagged: recorded.tagged,
            completed: recorded.completed_ns.is_some(),
            ..BackfillStatus::default()
        };
        Ok(Self {
            store,
            policy,
            status: Mutex::new(status),
        })
    }

    /// What Settings shows (R3.7).
    #[must_use]
    pub fn status(&self) -> BackfillStatus {
        *self.status.lock().expect("the backfill status lock")
    }

    /// Clears the recorded completion and the progress, so the next run tags
    /// the whole library again (R3.7: "Tag existing assets now").
    ///
    /// This is safe while a run is in flight: the run re-reads the state from
    /// the store before every batch, so it picks up the cleared cursor and
    /// starts over instead of continuing past it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the state cannot be cleared.
    pub fn restart(&self) -> Result<(), StoreError> {
        self.store.restart_tag_backfill()?;
        *self.status.lock().expect("the backfill status lock") = BackfillStatus::default();
        Ok(())
    }

    /// One pass over the library the state database holds (R3.7).
    ///
    /// The pass ends when the library is finished, when the setting is turned
    /// off, when a batch is refused for good, or when the store cannot be read
    /// or written; the first two leave the state resumable, the other two keep
    /// the progress that already landed.
    pub async fn run(&self, tagger: &Tagger) {
        if !tagger.enabled() {
            return;
        }
        let total = match self.store.tag_backfill_total() {
            Ok(total) => total,
            Err(error) => {
                tracing::error!(error = %error, "R3.7: the library could not be counted");
                return;
            }
        };
        self.publish(|status| {
            status.running = true;
            status.total = total;
            status.completed = false;
        });
        let mut failures = 0_u32;
        loop {
            // R3.7: the toggle off pauses the run at a batch boundary; the
            // cursor the last batch recorded is where the next run starts.
            if !tagger.enabled() {
                tracing::info!("R3.7: the backfill paused");
                break;
            }
            let recorded = match self.store.tag_backfill() {
                Ok(recorded) => recorded,
                Err(error) => {
                    tracing::error!(error = %error, "R3.7: the backfill state could not be read");
                    break;
                }
            };
            if recorded.completed_ns.is_some() {
                self.publish(|status| {
                    status.tagged = recorded.tagged;
                    status.completed = true;
                });
                break;
            }
            let batch = match self
                .store
                .tag_backfill_batch(recorded.cursor.as_deref(), self.policy.batch.max(1))
            {
                Ok(batch) => batch,
                Err(error) => {
                    tracing::error!(error = %error, "R3.7: the next batch could not be read");
                    break;
                }
            };
            let Some(cursor) = batch.last().cloned() else {
                // The end of the library: record it, so a restart does not tag
                // the whole library again (R3.7).
                match self.store.finish_tag_backfill(now_ns()) {
                    Ok(()) => self.publish(|status| {
                        status.tagged = recorded.tagged;
                        status.completed = true;
                    }),
                    Err(error) => {
                        tracing::error!(error = %error, "R3.7: the completion could not be recorded");
                    }
                }
                break;
            };
            match tagger.tag_batch(&batch).await {
                Ok(()) => {
                    failures = 0;
                    let tagged = recorded
                        .tagged
                        .saturating_add(u32::try_from(batch.len()).unwrap_or(u32::MAX));
                    if let Err(error) = self.store.record_tag_batch(&cursor, tagged) {
                        tracing::error!(error = %error, "R3.7: the completed batch could not be recorded");
                        break;
                    }
                    self.publish(|status| {
                        status.tagged = tagged;
                        status.running = true;
                    });
                    tracing::debug!(tagged, total, "R3.7: a backfill batch landed");
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    // The tagger recorded the reason for Settings itself; a
                    // permanent one stops the run with the progress it made.
                    if is_permanent(&error) || failures >= self.policy.attempts.max(1) {
                        tracing::warn!(
                            cursor = %cursor,
                            tagged = recorded.tagged,
                            reason = %error,
                            "R3.7: the backfill stopped"
                        );
                        break;
                    }
                    let delay = self.policy.retry.delay(failures, None);
                    tracing::warn!(
                        cursor = %cursor,
                        attempt = failures,
                        of = self.policy.attempts,
                        wait_ms = delay.as_millis() as u64,
                        reason = %error,
                        "R3.7: the batch failed and will be tried again"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
        self.publish(|status| status.running = false);
    }

    /// Updates the copy Settings renders (R3.7).
    fn publish(&self, update: impl FnOnce(&mut BackfillStatus)) {
        let mut status = self.status.lock().expect("the backfill status lock");
        update(&mut status);
    }
}

/// Whether another attempt cannot succeed (R6.3): a permission the key does
/// not hold, or a refusal that is not one of the transient classes.
fn is_permanent(error: &TagError) -> bool {
    match error {
        TagError::MissingPermission { .. } => true,
        TagError::Client(error) => classify(error).1 == FailureKind::Permanent,
    }
}

/// The current time in nanoseconds since the Unix epoch, as the state database
/// records it.
fn now_ns() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_is_a_few_hundred_ids() {
        // R3.7: the request body stays small while a large library costs a
        // handful of requests per thousand assets.
        assert!(
            (100..=1000).contains(&BACKFILL_BATCH),
            "one batch is a few hundred asset ids, not {BACKFILL_BATCH}"
        );
        assert_eq!(BackfillPolicy::default().batch, BACKFILL_BATCH);
    }

    #[test]
    fn a_permission_refusal_is_permanent_and_a_server_error_is_not() {
        assert!(is_permanent(&TagError::MissingPermission {
            permissions: vec!["tag.asset"],
        }));
        assert!(is_permanent(&TagError::Client(
            immich_client::ClientError::PermissionDenied("tag.asset".to_string())
        )));
        assert!(!is_permanent(&TagError::Client(
            immich_client::ClientError::Stalled
        )));
        assert!(!is_permanent(&TagError::Client(
            immich_client::ClientError::Protocol {
                status: 503,
                message: String::new(),
                retry_after: None,
            }
        )));
    }
}
