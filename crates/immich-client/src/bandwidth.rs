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
/// the default chunk capacity of `ReaderStream` - the body is streamed from
/// `ReaderStream::new` in `upload_asset` and nowhere else - so below about
/// 137 B/s the wait is longer than the guard's budget and a healthy server
/// looks dead before the body has sent anything. A kilobyte makes the first
/// read cost 4 s and leaves the steady state far inside it.
///
/// That 4 KiB is not this crate's to keep: tokio-util documents the default as
/// an implementation detail outside its semver contract. A chunk of about
/// 29 KiB would spend the whole guard budget on one read and stall every
/// healthy upload again, so anyone changing the body's chunking moves this
/// floor with it.
const RATE_FLOOR: u64 = 1_000;

/// A shareable limit on upload bytes per second; 0 means unlimited.
#[derive(Clone, Debug)]
pub struct BandwidthLimit {
    bytes_per_second: Arc<AtomicU64>,
    bucket: Arc<Mutex<Bucket>>,
    /// The bytes every metered body has handed to the server, shared by every
    /// clone, which is what the diagnostics throughput samples (R10.2).
    moved: Arc<AtomicU64>,
    /// A second counter this view also charges, when it has one. See
    /// [`BandwidthLimit::tallying`].
    tally: Option<Arc<AtomicU64>>,
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
            moved: Arc::new(AtomicU64::new(0)),
            tally: None,
        }
    }

    /// A view of this limit whose bodies also count their bytes in `tally`.
    ///
    /// The rate and the bucket stay shared, so the client still has one budget
    /// and one total; the second counter is how one part of the client reads
    /// its own share of the traffic without summing uploads or borrowing the
    /// bytes of another (R8.1).
    #[must_use]
    pub fn tallying(&self, tally: Arc<AtomicU64>) -> Self {
        Self {
            bytes_per_second: Arc::clone(&self.bytes_per_second),
            bucket: Arc::clone(&self.bucket),
            moved: Arc::clone(&self.moved),
            tally: Some(tally),
        }
    }

    /// A view of this limit with no rate, keeping the shared bucket and total.
    ///
    /// A live photo's motion video streams at the network's pace (R3.3), but its
    /// bytes are still the client's upload bytes, so they must land in the same
    /// total (R10.2). Only the pacing is dropped.
    #[must_use]
    pub fn unthrottled(&self) -> Self {
        Self {
            bytes_per_second: Arc::new(AtomicU64::new(0)),
            bucket: Arc::clone(&self.bucket),
            moved: Arc::clone(&self.moved),
            tally: self.tally.clone(),
        }
    }

    /// The bytes metered bodies have moved so far (R10.2). Every clone shares
    /// the count, so this is the client's total, not one upload's.
    #[must_use]
    pub fn total_bytes_moved(&self) -> u64 {
        self.moved.load(Ordering::Relaxed)
    }

    /// Records `bytes` handed to the server. Separate from [`Self::debit`],
    /// which only charges a limited bucket: an unlimited upload still counts.
    /// A view charges its own counter too (R8.1).
    fn account(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.moved.fetch_add(bytes, Ordering::Relaxed);
        if let Some(tally) = &self.tally {
            tally.fetch_add(bytes, Ordering::Relaxed);
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
                limit.account(read);
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

    #[tokio::test]
    async fn a_tallying_view_counts_the_bytes_its_own_bodies_move() {
        use tokio::io::AsyncReadExt as _;

        // Two folders over one client limit. Each folder's view counts its own
        // body, and the shared total still counts both (R8.1).
        let limit = BandwidthLimit::new(0);
        let photos = Arc::new(AtomicU64::new(0));
        let videos = Arc::new(AtomicU64::new(0));
        let mut photos_body: &[u8] = b"0123456789";
        let mut videos_body: &[u8] = b"abcdefghij";
        let photos_view = limit.tallying(Arc::clone(&photos));
        let (mut photos_reader, _progress) =
            MeteredReader::new(&mut photos_body, photos_view.clone());
        let (mut videos_reader, _progress) =
            MeteredReader::new(&mut videos_body, limit.tallying(Arc::clone(&videos)));
        let mut buf = [0_u8; 4];
        assert_eq!(photos_reader.read(&mut buf).await.expect("read"), 4);
        assert_eq!(videos_reader.read(&mut buf).await.expect("read"), 4);
        assert_eq!(
            photos.load(Ordering::Relaxed),
            4,
            "the photos body counted its own four bytes"
        );
        assert_eq!(
            videos.load(Ordering::Relaxed),
            4,
            "the videos body counted its own four bytes"
        );
        assert_eq!(
            limit.total_bytes_moved(),
            8,
            "the client's total still counts every body"
        );

        // A clone is the same view: the folder's uploads keep charging the one
        // counter they were handed.
        let (mut clone_reader, _progress) = MeteredReader::new(&mut photos_body, photos_view);
        assert_eq!(clone_reader.read(&mut buf).await.expect("read"), 4);
        assert_eq!(
            photos.load(Ordering::Relaxed),
            8,
            "a cloned view keeps counting in the same tally"
        );
        assert_eq!(
            videos.load(Ordering::Relaxed),
            4,
            "the other folder is untouched"
        );
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
