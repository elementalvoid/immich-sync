//! The upload meter the dashboard reads (SPEC R8.1; GLOSSARY "Upload
//! throughput", "Upload in flight").
//!
//! One [`UploadMeter`] per source folder counts the bytes its upload bodies
//! move and how many of its upload requests are on the wire. The engine samples
//! every folder from its `status` call, so the rate on the card costs no timer
//! of its own; the global reading is the sum of the folders.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use immich_client::BandwidthLimit;

/// The upload traffic one card shows (R8.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UploadTraffic {
    /// Upload-body bytes moved since the engine started.
    pub bytes: u64,
    /// Upload-body bytes per second over the last two seconds, from the rate
    /// window the status poll samples.
    pub bytes_per_second: u64,
    /// Upload requests that have started and not finished.
    pub in_flight: usize,
}

impl UploadTraffic {
    /// Adds another reading to this one. The global card is the sum of the
    /// folders, so every field adds.
    pub fn add(&mut self, other: Self) {
        self.bytes += other.bytes;
        self.bytes_per_second += other.bytes_per_second;
        self.in_flight += other.in_flight;
    }
}

/// The rolling window the rate is averaged over.
///
/// About two seconds: long enough that one poll's slice of a body reads as a
/// sustained rate instead of a spike, short enough that a finished upload stops
/// counting soon after it is over.
const WINDOW: Duration = Duration::from_secs(2);

/// One folder's meter (R8.1).
#[derive(Debug)]
pub struct UploadMeter {
    /// The bytes this folder's upload bodies have moved, shared with those
    /// bodies, so a concurrent upload can only ever add its own.
    bytes: Arc<AtomicU64>,
    /// The upload requests on the wire right now, shared with the views of
    /// this meter.
    in_flight: Arc<AtomicUsize>,
    /// The samples behind [`Self::reading`]. The status poll is the only clock,
    /// so the lock is taken once per poll and never by an upload.
    window: Mutex<RateWindow>,
    /// The client's limit, viewed so the bytes it meters also land in `bytes`.
    limit: BandwidthLimit,
}

impl UploadMeter {
    /// A meter whose uploads stream under `limit` (R7.1) and are counted here.
    pub fn new(limit: &BandwidthLimit) -> Self {
        let bytes = Arc::new(AtomicU64::new(0));
        Self {
            limit: limit.tallying(Arc::clone(&bytes)),
            bytes,
            in_flight: Arc::new(AtomicUsize::new(0)),
            window: Mutex::new(RateWindow::default()),
        }
    }

    /// The same folder's counters under no limit (R3.3).
    ///
    /// A live photo's motion video is a request of the photo's own pipeline, and
    /// it has always streamed at the network's pace; what it must not do is
    /// leave the dashboard reading `Idle` while it is on the wire, so it counts
    /// in the folder's bytes and in its uploads in flight (R8.1).
    pub(crate) fn unlimited(&self) -> Self {
        Self {
            // No rate, but the same counters: the limit still has to charge the
            // bytes it meters, or the folder would read as idle while the video
            // is on the wire.
            limit: BandwidthLimit::default().tallying(Arc::clone(&self.bytes)),
            bytes: Arc::clone(&self.bytes),
            in_flight: Arc::clone(&self.in_flight),
            window: Mutex::new(RateWindow::default()),
        }
    }

    /// A meter that counts into nothing: the manual upload in the Upload view
    /// is exempt from the sync limits and from the dashboard (R7.4).
    pub fn detached() -> Self {
        Self::new(&BandwidthLimit::default())
    }

    /// The limit this folder's uploads stream under (R7.1).
    pub fn limit(&self) -> &BandwidthLimit {
        &self.limit
    }

    /// The reading the card shows, sampled at `now` (R8.1).
    pub fn reading(&self, now: Instant) -> UploadTraffic {
        let bytes = self.bytes.load(Ordering::Relaxed);
        let bytes_per_second = self
            .window
            .lock()
            .expect("the rate window")
            .observe(now, bytes);
        UploadTraffic {
            bytes,
            bytes_per_second,
            in_flight: self.in_flight.load(Ordering::Acquire),
        }
    }

    /// Counts one upload request as in flight until the guard is dropped
    /// (R8.1). Hashing, the bulk duplicate check, and the album calls are not
    /// uploads in flight, and they never take this guard.
    #[must_use]
    pub fn uploading(&self) -> InFlight<'_> {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        InFlight(self)
    }
}

/// One upload request, counted until it finishes (R8.1).
///
/// The guard releases the count on every path out of the request, including a
/// worker whose future is dropped when the engine stops.
#[derive(Debug)]
pub struct InFlight<'a>(&'a UploadMeter);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The samples behind one meter's rate, oldest first.
#[derive(Debug, Default)]
struct RateWindow {
    samples: VecDeque<(Instant, u64)>,
}

impl RateWindow {
    /// Records the meter's total and returns the rate over the window just
    /// ended, in bytes per second.
    ///
    /// The bytes counted are those since the newest sample that is at least a
    /// window old, divided by the window, so the reading is the last two
    /// seconds' average and a burst of one poll's bytes is spread across it
    /// instead of reading as that poll's spike.
    fn observe(&mut self, now: Instant, total: u64) -> u64 {
        // A gap longer than the window means the poll that is this meter's
        // clock was not running, so the bytes in it are not a rate: the window
        // starts over rather than reporting the whole gap as one burst.
        if let Some((newest, _)) = self.samples.back()
            && now.saturating_duration_since(*newest) > WINDOW
        {
            self.samples.clear();
        }
        self.samples.push_back((now, total));
        while self.samples.len() > 1 && now.saturating_duration_since(self.samples[1].0) >= WINDOW {
            self.samples.pop_front();
        }
        let baseline = self.samples.front().expect("a sample was just pushed").1;
        // A rebuilt engine counts from zero again, which `saturating_sub` reads
        // as no bytes rather than as an underflow.
        let moved = total.saturating_sub(baseline);
        (moved as f64 / WINDOW.as_secs_f64()).round() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::{RateWindow, UploadMeter, UploadTraffic};
    use immich_client::BandwidthLimit;
    use std::time::{Duration, Instant};

    /// The window's own bytes-per-second for a series of samples.
    fn rate(samples: &[(u64, u64)]) -> u64 {
        let start = Instant::now();
        let mut window = RateWindow::default();
        let mut last = 0;
        for (millis, total) in samples {
            last = window.observe(start + Duration::from_millis(*millis), *total);
        }
        last
    }

    #[test]
    fn a_fresh_window_reports_no_rate() {
        assert_eq!(rate(&[(0, 0)]), 0);
    }

    #[test]
    fn a_burst_reads_as_its_bytes_over_the_window_not_one_poll() {
        // A mebibyte in one 250 ms poll. Averaged over the two-second window
        // that is half a MiB/s, not the four the poll itself saw.
        let half_a_mebibyte_a_second = 1_048_576 / 2;
        assert_eq!(rate(&[(0, 0), (250, 1_048_576)]), half_a_mebibyte_a_second);
        // The rate holds while the burst is inside the window, so the poll
        // that comes after it does not read zero.
        assert_eq!(
            rate(&[(0, 0), (250, 1_048_576), (1_750, 1_048_576)]),
            half_a_mebibyte_a_second
        );
    }

    #[test]
    fn a_burst_fades_out_of_the_window_after_two_seconds() {
        assert_eq!(
            rate(&[(0, 0), (250, 1_048_576), (2_250, 1_048_576)]),
            0,
            "no byte moved in the two seconds the window covers"
        );
    }

    #[test]
    fn a_steady_upload_reads_as_its_rate_once_the_window_is_full() {
        // A mebibyte every second, sampled every 250 ms: once the window holds
        // two seconds of it, the rate is the megabyte a second going in.
        let samples: Vec<(u64, u64)> = (0..=16).map(|step| (step * 250, step * 262_144)).collect();
        assert_eq!(
            rate(&samples),
            1_048_576,
            "the window reports the last two seconds' bytes per second"
        );
    }

    #[test]
    fn a_meter_that_restarts_never_reports_a_negative_rate() {
        // The engine was rebuilt and the new one counts from zero: the rate
        // starts over instead of borrowing the old total.
        assert_eq!(rate(&[(0, 0), (250, 2_000_000), (500, 0)]), 0);
    }

    #[test]
    fn a_poll_gap_starts_the_window_over() {
        // The status poll stopped (the window was hidden), so the bytes that
        // moved in between are not a rate: they would read as one huge burst.
        assert_eq!(rate(&[(0, 0), (250, 0), (60_000, 500_000)]), 0);
    }

    #[test]
    fn an_upload_counts_as_in_flight_until_the_request_finishes() {
        let meter = UploadMeter::new(&BandwidthLimit::new(0));
        let now = Instant::now();
        assert_eq!(meter.reading(now).in_flight, 0);

        let first = meter.uploading();
        assert_eq!(
            meter.reading(now).in_flight,
            1,
            "the request is on the wire"
        );
        let second = meter.uploading();
        assert_eq!(
            meter.reading(now).in_flight,
            2,
            "two requests are on the wire"
        );

        drop(first);
        assert_eq!(meter.reading(now).in_flight, 1);
        drop(second);
        assert_eq!(meter.reading(now).in_flight, 0, "both requests finished");
    }

    #[test]
    fn a_detached_meter_counts_nothing_the_dashboard_reads() {
        // R7.4: the manual upload is exempt, so its meter is never summed.
        let detached = UploadMeter::detached();
        let meter = UploadMeter::new(&BandwidthLimit::new(0));
        let now = Instant::now();
        let _upload = detached.uploading();
        let _upload = detached.uploading();
        assert_eq!(meter.reading(now), UploadTraffic::default());
    }

    #[test]
    fn a_reading_adds_to_the_global_counter() {
        let mut total = UploadTraffic::default();
        total.add(UploadTraffic {
            bytes: 10,
            bytes_per_second: 5,
            in_flight: 1,
        });
        total.add(UploadTraffic {
            bytes: 20,
            bytes_per_second: 7,
            in_flight: 2,
        });
        assert_eq!(
            total,
            UploadTraffic {
                bytes: 30,
                bytes_per_second: 12,
                in_flight: 3,
            }
        );
    }
}
