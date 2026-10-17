//! Optional performance instrumentation for the capture/encode pipeline.
//!
//! Everything here is gated behind the `metrics` Cargo feature. With the
//! feature off every entry point below compiles down to nothing: [`Timer`] is a
//! zero-sized struct, the record/mark functions have empty bodies, and no
//! histogram or sampler thread exists. That keeps the shipped release build
//! byte-for-byte free of measurement overhead while letting the hot path carry
//! permanent, un-`cfg`-ed instrumentation call sites.
//!
//! Build and run an instrumented binary with:
//!
//! ```text
//! cargo run --release --features metrics
//! ```
//!
//! Three things get measured:
//!
//! - **Latency** — per-[`Stage`] HdrHistograms, reported as p50/p99/p99.9/max
//!   rather than means, because the tail is what a dropped frame looks like.
//! - **Allocations** — a counting [`alloc::Counting`] global allocator, sampled
//!   per reporting window so steady-state allocations *per frame* fall out.
//! - **Process CPU and memory** — [`proc`] samples the OS counters directly
//!   instead of trusting Task Manager.
//!
//! Results stream to a CSV per run plus a final percentile dump; see
//! [`report::run_dir`] for where they land.

// With the feature off the call sites compile to no-ops, leaving the
// collectors themselves unused by design.
#![cfg_attr(not(feature = "metrics"), allow(dead_code))]

pub mod alloc;
pub mod proc;
pub mod report;

#[cfg(feature = "metrics")]
use std::sync::atomic::Ordering;

/// A measurement point in the frame pipeline.
///
/// Stages are deliberately split so a regression shows up in one bucket
/// instead of being averaged away in an end-to-end number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Capture callback entry until the frame is handed to `ProcessInput`.
    /// Covers the GPU copy, the channel hop, and encoder-thread wakeup.
    CaptureToSubmit,
    /// `ProcessInput` until the matching access unit comes back out of the
    /// hardware MFT. This is encoder pipeline depth, not just encode work.
    Encode,
    /// [`crate::encoding::gpu_buffer::EncodedAuBuffer::push`] — ring insert
    /// plus GOP trim, under the buffer lock.
    BufferPush,
    /// Capture callback entry until the access unit is resident in the ring
    /// buffer. The number that answers "how stale is my replay buffer?".
    EndToEnd,
    /// Wall-clock gap between consecutive captured frames. Its spread is
    /// capture jitter; compare against 1/fps.
    FrameInterval,
    /// Hotkey press until the clip file is closed on disk.
    Export,
}

impl Stage {
    /// Number of distinct stages (histogram array length).
    pub const COUNT: usize = 6;

    /// Short identifier used as a CSV column prefix and log label.
    pub fn name(self) -> &'static str {
        match self {
            Stage::CaptureToSubmit => "capture_to_submit",
            Stage::Encode => "encode",
            Stage::BufferPush => "buffer_push",
            Stage::EndToEnd => "end_to_end",
            Stage::FrameInterval => "frame_interval",
            Stage::Export => "export",
        }
    }

    /// Every stage, in histogram-index order.
    pub const ALL: [Stage; Stage::COUNT] = [
        Stage::CaptureToSubmit,
        Stage::Encode,
        Stage::BufferPush,
        Stage::EndToEnd,
        Stage::FrameInterval,
        Stage::Export,
    ];
}

/// A capture-time marker carried alongside a frame through the pipeline so
/// downstream stages can measure back to it.
///
/// Zero-sized with the feature off, so threading one through a channel adds
/// nothing to the frame payload in release builds.
#[derive(Debug, Clone, Copy)]
pub struct Timestamp {
    #[cfg(feature = "metrics")]
    at: std::time::Instant,
}

impl Timestamp {
    /// Stamp the current instant.
    #[inline]
    pub fn now() -> Self {
        #[cfg(feature = "metrics")]
        {
            Self {
                at: std::time::Instant::now(),
            }
        }
        #[cfg(not(feature = "metrics"))]
        {
            Self {}
        }
    }

    /// Time since this stamp was taken. Always zero with the feature off.
    #[inline]
    pub fn elapsed(&self) -> std::time::Duration {
        #[cfg(feature = "metrics")]
        {
            self.at.elapsed()
        }
        #[cfg(not(feature = "metrics"))]
        {
            std::time::Duration::ZERO
        }
    }

    /// Time from `earlier` to this stamp. Saturates at zero.
    #[inline]
    pub fn duration_since(&self, earlier: Timestamp) -> std::time::Duration {
        #[cfg(feature = "metrics")]
        {
            self.at.saturating_duration_since(earlier.at)
        }
        #[cfg(not(feature = "metrics"))]
        {
            let _ = earlier;
            std::time::Duration::ZERO
        }
    }
}

impl Default for Timestamp {
    fn default() -> Self {
        Self::now()
    }
}

/// Record the time elapsed from `since` until now into `stage`.
#[inline]
pub fn record_since(stage: Stage, since: Timestamp) {
    record(stage, since.elapsed());
}

/// Note that a frame was captured at `ts`, recording the gap since the
/// previous captured frame into [`Stage::FrameInterval`].
///
/// Owns the "previous frame" state so the capture callback stays a one-liner.
#[inline]
pub fn note_capture(ts: Timestamp) {
    #[cfg(feature = "metrics")]
    {
        use parking_lot::Mutex;
        use std::sync::OnceLock;
        static LAST: OnceLock<Mutex<Option<Timestamp>>> = OnceLock::new();
        let cell = LAST.get_or_init(|| Mutex::new(None));
        let prev = cell.lock().replace(ts);
        if let Some(prev) = prev {
            record(Stage::FrameInterval, ts.duration_since(prev));
        }
    }
    #[cfg(not(feature = "metrics"))]
    {
        let _ = ts;
    }
}

/// Scoped stopwatch: records the elapsed time into `stage` when dropped.
///
/// Zero-sized and free when the `metrics` feature is off.
pub struct Timer {
    #[cfg(feature = "metrics")]
    start: std::time::Instant,
    #[cfg(feature = "metrics")]
    stage: Stage,
}

impl Timer {
    /// Start timing `stage`.
    #[inline]
    pub fn start(stage: Stage) -> Self {
        #[cfg(feature = "metrics")]
        {
            Self {
                start: std::time::Instant::now(),
                stage,
            }
        }
        #[cfg(not(feature = "metrics"))]
        {
            let _ = stage;
            Self {}
        }
    }
}

impl Drop for Timer {
    #[inline]
    fn drop(&mut self) {
        #[cfg(feature = "metrics")]
        record(self.stage, self.start.elapsed());
    }
}

/// Record one observation for `stage`.
#[inline]
pub fn record(stage: Stage, d: std::time::Duration) {
    #[cfg(feature = "metrics")]
    {
        // Histograms hold microseconds: a 60 fps frame budget is 16 666 us, so
        // us resolution keeps three significant figures meaningful without the
        // histogram spanning nine orders of magnitude.
        let us = d.as_micros().min(u64::MAX as u128) as u64;
        imp::registry().record(stage, us);
    }
    #[cfg(not(feature = "metrics"))]
    {
        let _ = (stage, d);
    }
}

/// Count one frame that made it through the pipeline — buffered as an access
/// unit on the GPU path, accepted for encoding on the readback path. Drives
/// the frames/sec and allocations-per-frame columns.
#[inline]
pub fn mark_frame() {
    #[cfg(feature = "metrics")]
    imp::registry().frames.fetch_add(1, Ordering::Relaxed);
}

/// Count one frame dropped at a full channel.
#[inline]
pub fn mark_dropped() {
    #[cfg(feature = "metrics")]
    imp::registry().dropped.fetch_add(1, Ordering::Relaxed);
}

/// Frames and drops counted so far, as `(frames, dropped)`.
pub fn frame_counts() -> (u64, u64) {
    #[cfg(feature = "metrics")]
    {
        let r = imp::registry();
        (
            r.frames.load(Ordering::Relaxed),
            r.dropped.load(Ordering::Relaxed),
        )
    }
    #[cfg(not(feature = "metrics"))]
    {
        (0, 0)
    }
}

/// Percentile summary of one stage, in microseconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Summary {
    /// Number of recorded observations.
    pub count: u64,
    /// Median.
    pub p50_us: u64,
    /// 99th percentile.
    pub p99_us: u64,
    /// 99.9th percentile — where a stall shows up first.
    pub p999_us: u64,
    /// Worst observation seen.
    pub max_us: u64,
    /// Arithmetic mean, kept only so it can be shown next to p50 as evidence
    /// of how misleading it is.
    pub mean_us: f64,
}

/// Summarise one stage over everything recorded so far.
pub fn summary(stage: Stage) -> Summary {
    #[cfg(feature = "metrics")]
    {
        imp::registry().summary(stage)
    }
    #[cfg(not(feature = "metrics"))]
    {
        let _ = stage;
        Summary::default()
    }
}

/// Full percentile distribution of one stage as `(quantile, value_us)` pairs,
/// for pasting into a README table or plotting a latency curve.
pub fn percentiles(stage: Stage) -> Vec<(f64, u64)> {
    #[cfg(feature = "metrics")]
    {
        imp::registry().percentiles(stage)
    }
    #[cfg(not(feature = "metrics"))]
    {
        let _ = stage;
        Vec::new()
    }
}

/// Drop every recorded observation and reset the counters.
///
/// Call after warm-up so start-up allocations and first-frame encoder
/// negotiation do not pollute the steady-state numbers.
#[allow(dead_code)] // Not wired to a trigger yet; public for benchmark runs.
pub fn reset() {
    #[cfg(feature = "metrics")]
    imp::registry().reset();
}

#[cfg(feature = "metrics")]
mod imp {
    use std::sync::atomic::AtomicU64;
    use std::sync::OnceLock;

    use hdrhistogram::Histogram;
    use parking_lot::Mutex;

    use super::{Stage, Summary};

    /// Longest latency a histogram tracks, in microseconds. Anything slower
    /// than a minute is a hang, not a latency measurement, and clamps here.
    const MAX_TRACKED_US: u64 = 60_000_000;

    pub(super) struct Registry {
        /// One histogram per stage, indexed by `Stage as usize`.
        stages: [Mutex<Histogram<u64>>; Stage::COUNT],
        pub(super) frames: AtomicU64,
        pub(super) dropped: AtomicU64,
    }

    impl Registry {
        fn new() -> Self {
            Self {
                stages: std::array::from_fn(|_| {
                    // Explicit 1 us..60 s range at 3 significant figures.
                    // The bounds must be stated up front: `saturating_record`
                    // clamps to the histogram's *current* range, so a
                    // range-less histogram would pin the first samples of a
                    // run to its tiny initial ceiling.
                    Mutex::new(
                        Histogram::<u64>::new_with_bounds(1, MAX_TRACKED_US, 3)
                            .expect("valid histogram bounds"),
                    )
                }),
                frames: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
            }
        }

        pub(super) fn record(&self, stage: Stage, us: u64) {
            // saturating_record instead of record: a pathological outlier
            // should clamp, never silently drop the sample.
            self.stages[stage as usize].lock().saturating_record(us);
        }

        pub(super) fn summary(&self, stage: Stage) -> Summary {
            let h = self.stages[stage as usize].lock();
            Summary {
                count: h.len(),
                p50_us: h.value_at_quantile(0.50),
                p99_us: h.value_at_quantile(0.99),
                p999_us: h.value_at_quantile(0.999),
                max_us: h.max(),
                mean_us: h.mean(),
            }
        }

        pub(super) fn percentiles(&self, stage: Stage) -> Vec<(f64, u64)> {
            const QUANTILES: [f64; 9] = [0.0, 0.25, 0.50, 0.75, 0.90, 0.99, 0.999, 0.9999, 1.0];
            let h = self.stages[stage as usize].lock();
            QUANTILES
                .iter()
                .map(|&q| (q, h.value_at_quantile(q)))
                .collect()
        }

        #[allow(dead_code)] // Only reached through `metrics::reset`.
        pub(super) fn reset(&self) {
            for s in &self.stages {
                s.lock().reset();
            }
            self.frames.store(0, std::sync::atomic::Ordering::Relaxed);
            self.dropped.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn registry() -> &'static Registry {
        static REGISTRY: OnceLock<Registry> = OnceLock::new();
        REGISTRY.get_or_init(Registry::new)
    }
}

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The registry is process-global, so tests that call [`reset`] must not
    /// overlap. Cargo runs tests in parallel by default.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn records_and_summarises() {
        let _guard = exclusive();
        reset();
        for us in 1..=1000u64 {
            record(Stage::Encode, Duration::from_micros(us));
        }
        let s = summary(Stage::Encode);
        assert_eq!(s.count, 1000);
        // 3 significant figures, so allow a small bucketing error.
        assert!((s.p50_us as i64 - 500).abs() <= 2, "p50 was {}", s.p50_us);
        assert!((s.p99_us as i64 - 990).abs() <= 3, "p99 was {}", s.p99_us);
        assert!(s.max_us >= 1000);
    }

    #[test]
    fn reset_clears_counts() {
        let _guard = exclusive();
        reset();
        record(Stage::BufferPush, Duration::from_micros(10));
        mark_frame();
        reset();
        assert_eq!(summary(Stage::BufferPush).count, 0);
        assert_eq!(frame_counts().0, 0);
    }

    #[test]
    fn timer_records_on_drop() {
        let _guard = exclusive();
        reset();
        {
            let _t = Timer::start(Stage::Export);
            std::thread::sleep(Duration::from_millis(2));
        }
        let s = summary(Stage::Export);
        assert_eq!(s.count, 1);
        assert!(s.max_us >= 1500, "expected >=1.5ms, got {}", s.max_us);
    }

    #[test]
    fn stage_names_are_unique() {
        let mut names: Vec<_> = Stage::ALL.iter().map(|s| s.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Stage::COUNT);
    }
}
