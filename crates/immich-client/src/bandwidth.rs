//! The live bandwidth limit (SPEC R7.1).
//!
//! A hand-rolled byte token bucket over the upload body stream, chosen over
//! `governor` because the budget belongs to the bytes the multipart reader
//! pulls, not to requests (`docs/research/rust-sync-engine-crates.md` §3).
//! One bucket is shared by every upload, so the limit is the client's total
//! upload rate and not a per-upload rate. The rate is an atomic, so a settings
//! change applies to a request already on the wire: [`BandwidthLimit::set`] is
//! read again on every poll of the stream. Each metered body also publishes
//! when it last moved a byte, which is what the client's stall guard watches
//! (`crate::client`).

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, ReadBuf};
use tokio::time::{Instant, Sleep};

/// The most a stream waits before it re-reads the limit. A sleep is capped at
/// this so raising the limit is felt within 50 ms even by an upload that was
/// waiting on a long, low-rate delay.
const MAX_WAIT: Duration = Duration::from_millis(50);

/// How much of a second's budget a bucket may hold at once. A tenth is short
/// enough that a file smaller than the bucket still finishes near its limit,
/// but only while the rate is above `BUCKET_FLOOR * BURST_DIVISOR` (about
/// 640 kB/s). Below that the floor is the capacity, not a tenth of a second.
const BURST_DIVISOR: u64 = 10;

/// The least a bucket holds, whatever the rate is.
///
/// A read asks for as much as the stream's buffer offers, a few tens of
/// kilobytes at most. A bucket that holds less than one read can never pay for
/// it, so any rate below `read / BURST_DIVISOR` would leave the body waiting
/// for ever - and the stall guard would rightly call that a dead server. This
/// floor keeps a very slow limit paying out one read at a time, and it is what
/// the bucket holds below `BUCKET_FLOOR * BURST_DIVISOR`, where a tenth of a
/// second is too little to pay for even one read.
const BUCKET_FLOOR: u64 = 64 * 1024;

/// The slowest rate the limiter will run at; 0 still means unlimited.
///
/// The client fails an upload that has moved no byte for 30 s, and the first
/// read out of an empty bucket costs `read / rate`. One read is at most 4 KiB,
/// so below about 137 B/s the wait is longer than the guard's budget and a
/// healthy server looks dead before the body has sent anything. A kilobyte
/// makes the first read cost 4 s and leaves the steady state far inside it.
const RATE_FLOOR: u64 = 1_000;

/// A shareable limit on upload bytes per second; 0 means unlimited.
#[derive(Clone, Debug)]
pub struct BandwidthLimit {
    bytes_per_second: Arc<AtomicU64>,
    bucket: Arc<Mutex<Bucket>>,
}

impl Default for BandwidthLimit {
    fn default() -> Self {
        Self::new(0)
    }
}

impl BandwidthLimit {
    /// A limit of `bytes_per_second`; 0 means unlimited.
    #[must_use]
    pub fn new(bytes_per_second: u64) -> Self {
        Self {
            bytes_per_second: Arc::new(AtomicU64::new(bytes_per_second)),
            bucket: Arc::new(Mutex::new(Bucket::new())),
        }
    }

    /// Changes the limit. An upload already on the wire reads the new value on
    /// its next poll, so no request is cancelled and no restart is needed.
    pub fn set(&self, bytes_per_second: u64) {
        self.bytes_per_second
            .store(bytes_per_second, Ordering::Release);
    }

    /// The rate the limiter runs at right now: what was set, except that a
    /// rate under [`RATE_FLOOR`] is used at the floor. Zero is left alone; it
    /// is unlimited, not slow.
    #[must_use]
    pub fn bytes_per_second(&self) -> u64 {
        let bytes = self.bytes_per_second.load(Ordering::Acquire);
        if bytes == 0 { 0 } else { bytes.max(RATE_FLOOR) }
    }

    /// How long `bytes` must wait for budget. A zero duration means now.
    fn wait_for(&self, bytes: u64) -> Duration {
        let rate = self.bytes_per_second();
        if rate == 0 {
            return Duration::ZERO;
        }
        let mut bucket = self.bucket.lock().unwrap_or_else(PoisonError::into_inner);
        let tokens = bucket.refill(rate);
        if tokens >= bytes as f64 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((bytes as f64 - tokens) / rate as f64)
        }
    }

    /// Charges `bytes` to the budget. The debt may go negative; the next
    /// refill repays it, which is what keeps the long-run rate honest.
    fn debit(&self, bytes: u64) {
        let rate = self.bytes_per_second();
        if rate == 0 || bytes == 0 {
            return;
        }
        let mut bucket = self.bucket.lock().unwrap_or_else(PoisonError::into_inner);
        bucket.refill(rate);
        bucket.tokens -= bytes as f64;
    }

    /// Sets how long ago the bucket was last refilled, so a test measures from
    /// a known instant instead of from the wall clock.
    #[cfg(test)]
    fn set_idle(&self, elapsed: Duration) {
        self.bucket
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .refilled = Instant::now() - elapsed;
    }
}

/// The token count and the last time it was refilled.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    refilled: Instant,
}

impl Bucket {
    fn new() -> Self {
        Self {
            tokens: 0.0,
            refilled: Instant::now(),
        }
    }

    /// Tops the bucket up for the time since the last refill and returns the
    /// tokens now available.
    fn refill(&mut self, rate: u64) -> f64 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.refilled).as_secs_f64();
        self.refilled = now;
        let capacity = (rate / BURST_DIVISOR).max(BUCKET_FLOOR) as f64;
        self.tokens = (self.tokens + elapsed * rate as f64).min(capacity);
        self.tokens
    }
}

/// When one metered body last handed the server a byte.
///
/// The stall guard reads this instead of a rate: an upload the limit is pacing
/// slowly still moves bytes, so only a server that stopped taking them trips
/// the guard (R7.1).
#[derive(Debug, Clone)]
pub(crate) struct Progress(Arc<AtomicU64>);

/// The instant the stall clock counts from. [`Instant`] has no integer form
/// and the clock has to be one atomic wide.
static CLOCK_BASE: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since [`CLOCK_BASE`]. The clock is [`tokio::time::Instant`],
/// so the meter that publishes progress and the guard that reads it measure
/// the same clock.
fn millis_now() -> u64 {
    let base = *CLOCK_BASE.get_or_init(Instant::now);
    u64::try_from(base.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl Default for Progress {
    /// A body that has not moved a byte yet.
    fn default() -> Self {
        Self(Arc::new(AtomicU64::new(millis_now())))
    }
}

impl Progress {
    /// Records that a byte moved.
    pub(crate) fn moved(&self) {
        self.0.store(millis_now(), Ordering::Relaxed);
    }

    /// When a byte last moved, on the clock's millisecond scale.
    #[must_use]
    pub(crate) fn stamp(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// How long the body has made no progress.
    #[must_use]
    pub(crate) fn idle(&self) -> Duration {
        Duration::from_millis(millis_now().saturating_sub(self.stamp()))
    }
}

/// Meters one body stream against a [`BandwidthLimit`].
#[derive(Debug)]
pub(crate) struct MeteredReader<R> {
    reader: R,
    limit: BandwidthLimit,
    wait: Option<Pin<Box<Sleep>>>,
    progress: Progress,
}

impl<R> MeteredReader<R> {
    /// Wraps `reader`, and hands back the clock the stall guard watches.
    pub(crate) fn new(reader: R, limit: BandwidthLimit) -> (Self, Progress) {
        let progress = Progress::default();
        let metered = Self {
            reader,
            limit,
            wait: None,
            progress: progress.clone(),
        };
        (metered, progress)
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for MeteredReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        let limit = this.limit.clone();
        let request = u64::try_from(buf.remaining()).unwrap_or(u64::MAX);
        if limit.bytes_per_second() == 0 || request == 0 {
            this.wait = None;
        } else {
            // A wait that is already running wakes up here; the check is
            // repeated because the limit may have changed while it slept.
            if let Some(wait) = &mut this.wait {
                if std::future::Future::poll(wait.as_mut(), cx).is_pending() {
                    return Poll::Pending;
                }
                this.wait = None;
            }
            let wait = limit.wait_for(request).min(MAX_WAIT);
            if !wait.is_zero() {
                let mut sleep = Box::pin(tokio::time::sleep(wait));
                if std::future::Future::poll(sleep.as_mut(), cx).is_pending() {
                    this.wait = Some(sleep);
                    return Poll::Pending;
                }
            }
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.reader).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let read = u64::try_from(buf.filled().len() - before).unwrap_or(0);
                // An end of file moves nothing: a body that is done must not
                // keep a stalled server alive.
                if read > 0 {
                    this.progress.moved();
                }
                limit.debit(read);
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlimited_bucket_never_waits() {
        let limit = BandwidthLimit::new(0);
        assert_eq!(limit.wait_for(1_000_000), Duration::ZERO);
        limit.debit(1_000_000);
        assert_eq!(limit.wait_for(1_000_000), Duration::ZERO);
        assert_eq!(limit.bytes_per_second(), 0);
    }

    #[test]
    fn a_spent_bucket_reports_the_time_the_budget_needs() {
        let limit = BandwidthLimit::new(1_000);
        // The bucket is measured from a known instant. It used to be measured
        // from `BandwidthLimit::new()`, so a test thread descheduled in
        // between refilled the bucket and failed the absolute bounds below.
        limit.set_idle(Duration::ZERO);
        let first = limit.wait_for(100);
        assert!(
            first.abs_diff(Duration::from_millis(100)) <= Duration::from_millis(10),
            "an empty bucket at 1 kB/s needs 100 ms: {first:?}"
        );
        // A hundred charged bytes add their own tenth of a second on top.
        limit.debit(100);
        limit.set_idle(Duration::ZERO);
        let second = limit.wait_for(100);
        assert!(
            second.abs_diff(Duration::from_millis(200)) <= Duration::from_millis(10),
            "a hundred charged bytes add their own hundred: {second:?}"
        );
    }

    #[test]
    fn a_slow_limit_can_still_pay_out_one_read() {
        // Four kilobytes is one read of the stream's buffer. At 1 kB/s that is
        // five seconds of budget, which a bucket holding a tenth of a second
        // could never pay out: the body would wait for ever and the stall
        // guard would call the server dead.
        let limit = BandwidthLimit::new(1_000);
        limit.set_idle(Duration::from_secs(5));
        assert_eq!(
            limit.wait_for(4_096),
            Duration::ZERO,
            "the bucket cannot hold one read at this rate"
        );
    }

    #[test]
    fn a_rate_under_the_floor_runs_at_the_floor_and_zero_runs_unlimited() {
        let limit = BandwidthLimit::new(1);
        assert_eq!(limit.bytes_per_second(), 1_000);
        limit.set(0);
        assert_eq!(limit.bytes_per_second(), 0);
        assert_eq!(limit.wait_for(1_000_000), Duration::ZERO);
    }

    #[test]
    fn setting_the_limit_is_visible_immediately() {
        let limit = BandwidthLimit::new(0);
        limit.set(2_000_000);
        assert_eq!(limit.bytes_per_second(), 2_000_000);
        limit.set(0);
        assert_eq!(limit.bytes_per_second(), 0);
    }
}
