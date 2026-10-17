//! Background reporter: samples the counters on an interval, streams a CSV,
//! and writes a percentile dump when the run ends.
//!
//! Output lands in `<local data dir>/RSClip/metrics/`:
//!
//! - `run-<timestamp>.csv` — one row per interval, for plotting CPU/memory
//!   over the session.
//! - `run-<timestamp>.txt` — end-of-run summary: full percentile tables per
//!   stage plus the aggregate allocation and CPU figures. This is the file to
//!   read numbers off for a README table or a CV bullet.
//!
//! Percentile columns in the CSV are *cumulative* (whole run so far), not
//! per-interval, so a late row is directly comparable to the final dump.

use std::path::PathBuf;

/// Directory this run's metric files are written to.
pub fn run_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("RSClip")
        .join("metrics")
}

/// Handle to the running reporter. Dropping or calling
/// [`stop`](Reporter::stop) ends sampling and writes the final dump.
pub struct Reporter {
    #[cfg(feature = "metrics")]
    inner: Option<imp::Inner>,
}

impl Reporter {
    /// Stop sampling and write the end-of-run summary. Returns the summary
    /// file path when one was written.
    pub fn stop(mut self) -> Option<PathBuf> {
        #[cfg(feature = "metrics")]
        {
            self.inner.take().map(|i| i.stop())
        }
        #[cfg(not(feature = "metrics"))]
        {
            let _ = &mut self;
            None
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        #[cfg(feature = "metrics")]
        if let Some(inner) = self.inner.take() {
            inner.stop();
        }
    }
}

/// Start the reporter, sampling every `interval_secs`.
///
/// With the `metrics` feature off this returns an inert handle and spawns
/// nothing.
pub fn start(interval_secs: u64) -> Reporter {
    #[cfg(feature = "metrics")]
    {
        Reporter {
            inner: imp::Inner::start(interval_secs.max(1)),
        }
    }
    #[cfg(not(feature = "metrics"))]
    {
        let _ = interval_secs;
        Reporter {}
    }
}

#[cfg(feature = "metrics")]
mod imp {
    use std::fs::File;
    use std::io::{BufWriter, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use log::{info, warn};

    use super::run_dir;
    use crate::metrics::{alloc, proc::Sampler, Stage};

    pub(super) struct Inner {
        running: Arc<AtomicBool>,
        handle: std::thread::JoinHandle<()>,
        summary_path: PathBuf,
    }

    impl Inner {
        pub(super) fn start(interval_secs: u64) -> Option<Self> {
            let dir = run_dir();
            if let Err(e) = std::fs::create_dir_all(&dir) {
                warn!("metrics: cannot create {}: {e}", dir.display());
                return None;
            }
            let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
            let csv_path = dir.join(format!("run-{stamp}.csv"));
            let summary_path = dir.join(format!("run-{stamp}.txt"));

            let csv = match File::create(&csv_path) {
                Ok(f) => BufWriter::new(f),
                Err(e) => {
                    warn!("metrics: cannot create {}: {e}", csv_path.display());
                    return None;
                }
            };

            info!("metrics: recording to {}", csv_path.display());

            let running = Arc::new(AtomicBool::new(true));
            let stop_flag = running.clone();
            let summary_for_thread = summary_path.clone();

            let handle = std::thread::Builder::new()
                .name("metrics-reporter".into())
                .spawn(move || {
                    sample_loop(csv, summary_for_thread, interval_secs, stop_flag);
                })
                .ok()?;

            Some(Self {
                running,
                handle,
                summary_path,
            })
        }

        pub(super) fn stop(self) -> PathBuf {
            self.running.store(false, Ordering::Relaxed);
            let _ = self.handle.join();
            self.summary_path
        }
    }

    /// Column order must match the row written in [`sample_loop`].
    fn write_header(csv: &mut BufWriter<File>) -> std::io::Result<()> {
        let mut cols = vec![
            "t_s".to_string(),
            "cpu_pct_core".into(),
            "cpu_pct_machine".into(),
            "working_set_mb".into(),
            "private_mb".into(),
            "frames".into(),
            "fps".into(),
            "dropped".into(),
            "allocs_window".into(),
            "allocs_per_frame".into(),
            "bytes_alloc_window".into(),
            "bytes_live_mb".into(),
        ];
        for stage in Stage::ALL {
            cols.push(format!("{}_p50_us", stage.name()));
            cols.push(format!("{}_p99_us", stage.name()));
            cols.push(format!("{}_p999_us", stage.name()));
        }
        writeln!(csv, "{}", cols.join(","))
    }

    fn sample_loop(
        mut csv: BufWriter<File>,
        summary_path: PathBuf,
        interval_secs: u64,
        running: Arc<AtomicBool>,
    ) {
        if let Err(e) = write_header(&mut csv) {
            warn!("metrics: header write failed: {e}");
            return;
        }

        let mut sampler = Sampler::new();
        let started = Instant::now();
        let mut prev_alloc = alloc::snapshot();
        let (mut prev_frames, _) = crate::metrics::frame_counts();

        // Poll the stop flag at 100 ms so shutdown is prompt even with a long
        // reporting interval.
        let tick = Duration::from_millis(100);
        let mut next_sample = Instant::now() + Duration::from_secs(interval_secs);

        while running.load(Ordering::Relaxed) {
            std::thread::sleep(tick);
            if Instant::now() < next_sample {
                continue;
            }
            next_sample = Instant::now() + Duration::from_secs(interval_secs);

            let reading = sampler.tick().unwrap_or_default();
            let now_alloc = alloc::snapshot();
            let window_alloc = now_alloc.since(prev_alloc);
            prev_alloc = now_alloc;

            let (frames, dropped) = crate::metrics::frame_counts();
            let window_frames = frames.saturating_sub(prev_frames);
            prev_frames = frames;

            let fps = window_frames as f64 / interval_secs as f64;
            let allocs_per_frame = if window_frames > 0 {
                window_alloc.allocs as f64 / window_frames as f64
            } else {
                0.0
            };

            let mut row = format!(
                "{:.1},{:.3},{:.3},{:.2},{:.2},{},{:.2},{},{},{:.2},{},{:.2}",
                started.elapsed().as_secs_f64(),
                reading.cpu_pct_core,
                reading.cpu_pct_machine,
                reading.working_set_mb,
                reading.private_mb,
                frames,
                fps,
                dropped,
                window_alloc.allocs,
                allocs_per_frame,
                window_alloc.bytes_allocated,
                now_alloc.bytes_live as f64 / (1024.0 * 1024.0),
            );
            for stage in Stage::ALL {
                let s = crate::metrics::summary(stage);
                row.push_str(&format!(",{},{},{}", s.p50_us, s.p99_us, s.p999_us));
            }

            if writeln!(csv, "{row}").is_err() || csv.flush().is_err() {
                warn!("metrics: CSV write failed, stopping reporter");
                return;
            }

            info!(
                "metrics: cpu {:.2}% of a core | ws {:.1} MB | private {:.1} MB | {:.1} fps | {} dropped | {:.2} allocs/frame",
                reading.cpu_pct_core,
                reading.working_set_mb,
                reading.private_mb,
                fps,
                dropped,
                allocs_per_frame,
            );
        }

        if let Err(e) = write_summary(&summary_path, started.elapsed(), &sampler) {
            warn!("metrics: summary write failed: {e}");
        } else {
            info!("metrics: summary written to {}", summary_path.display());
        }
    }

    fn write_summary(
        path: &PathBuf,
        elapsed: Duration,
        sampler: &Sampler,
    ) -> std::io::Result<()> {
        let mut out = BufWriter::new(File::create(path)?);
        let (frames, dropped) = crate::metrics::frame_counts();
        let alloc = alloc::snapshot();

        writeln!(out, "RSClip metrics summary")?;
        writeln!(out, "run duration    : {:.1} s", elapsed.as_secs_f64())?;
        writeln!(out, "logical cores   : {}", sampler.cores())?;
        writeln!(out, "frames buffered : {frames}")?;
        writeln!(out, "frames dropped  : {dropped}")?;
        if frames + dropped > 0 {
            writeln!(
                out,
                "drop rate       : {:.4} %",
                dropped as f64 / (frames + dropped) as f64 * 100.0
            )?;
        }
        writeln!(out, "mean fps        : {:.2}", frames as f64 / elapsed.as_secs_f64())?;
        writeln!(out)?;
        writeln!(out, "allocations (whole process, whole run)")?;
        writeln!(out, "  total allocs      : {}", alloc.allocs)?;
        writeln!(out, "  total bytes       : {}", alloc.bytes_allocated)?;
        writeln!(
            out,
            "  live at exit (MB) : {:.2}",
            alloc.bytes_live as f64 / (1024.0 * 1024.0)
        )?;
        writeln!(
            out,
            "  NOTE: run-long allocs include start-up; read allocs_per_frame from a\n        steady-state CSV row instead."
        )?;
        writeln!(out)?;

        for stage in Stage::ALL {
            let s = crate::metrics::summary(stage);
            if s.count == 0 {
                continue;
            }
            writeln!(out, "stage {} (n={})", stage.name(), s.count)?;
            writeln!(
                out,
                "  mean {:.1} us | p50 {} us | p99 {} us | p99.9 {} us | max {} us",
                s.mean_us, s.p50_us, s.p99_us, s.p999_us, s.max_us
            )?;
            writeln!(out, "  quantile,value_us")?;
            for (q, v) in crate::metrics::percentiles(stage) {
                writeln!(out, "  {q:.4},{v}")?;
            }
            writeln!(out)?;
        }

        out.flush()
    }
}
