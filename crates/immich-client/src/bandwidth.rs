//! The live bandwidth limit (SPEC R7.1).
//!
//! A hand-rolled byte token bucket over the upload body stream, chosen over
//! `governor` because the budget belongs to the bytes the multipart reader
//! pulls, not to requests (`docs/research/rust-sync-engine-crates.md` §3).
//! One bucket is shared by every upload, so the limit is the client's total
//! upload rate and not a per-upload rate. The rate is an atomic, so a settings
//! change applies to a request already on the wire: [`BandwidthLimit::set`] is
//! read again on every poll of the stream.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, ReadBuf};
use tokio::time::{Instant, Sleep};

/// The most a stream waits before it re-reads the limit. A sleep is capped at
/// this so raising the limit is felt within 50 ms even by an upload that was
/// waiting on a long, low-rate delay.
const MAX_WAIT: Duration = Duration::from_millis(50);

/// How much of a second's budget a bucket may hold at once. A tenth keeps the
/// burst small enough that a short file still averages near the limit.
const BURST_DIVISOR: u64 = 10;

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

    /// The limit right now.
    #[must_use]
    pub fn bytes_per_second(&self) -> u64 {
        self.bytes_per_second.load(Ordering::Acquire)
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
        let capacity = (rate / BURST_DIVISOR) as f64;
        self.tokens = (self.tokens + elapsed * rate as f64).min(capacity);
        self.tokens
    }
}

/// Meters one body stream against a [`BandwidthLimit`].
#[derive(Debug)]
pub(crate) struct MeteredReader<R> {
    reader: R,
    limit: Option<BandwidthLimit>,
    wait: Option<Pin<Box<Sleep>>>,
}

impl<R> MeteredReader<R> {
    /// Wraps `reader`; `None` reads through with no metering at all.
    pub(crate) fn new(reader: R, limit: Option<BandwidthLimit>) -> Self {
        Self {
            reader,
            limit,
            wait: None,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for MeteredReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        let Some(limit) = this.limit.clone() else {
            return Pin::new(&mut this.reader).poll_read(cx, buf);
        };
        let request = u64::try_from(buf.remaining()).unwrap_or(u64::MAX);
        if limit.bytes_per_second() == 0 || request == 0 {
            this.wait = None;
            return Pin::new(&mut this.reader).poll_read(cx, buf);
        }
        // A wait that is already running wakes up here; the check is repeated
        // because the limit may have changed while it slept.
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
        let before = buf.filled().len();
        match Pin::new(&mut this.reader).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let read = u64::try_from(buf.filled().len() - before).unwrap_or(0);
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
        // The bucket starts empty, so the first byte costs its millisecond at
        // 1 kB/s. The bounds leave room for a descheduled test thread.
        let first = limit.wait_for(100);
        assert!(first >= Duration::from_millis(80), "{first:?}");
        assert!(first <= Duration::from_millis(120), "{first:?}");
        // A hundred charged bytes add their own tenth of a second on top.
        limit.debit(100);
        let second = limit.wait_for(100);
        assert!(second >= Duration::from_millis(180), "{second:?}");
        assert!(second <= Duration::from_millis(220), "{second:?}");
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
