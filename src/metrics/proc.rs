//! Process-level CPU and memory sampling.
//!
//! Reads the OS counters directly rather than eyeballing Task Manager, so a
//! claim like "2.1% of one core at 1080p60" is reproducible and comparable
//! against another recorder measured the same way.
//!
//! CPU is reported two ways because both get quoted and they differ by the
//! core count: [`ProcReading::cpu_pct_core`] is percent of a *single* core
//! (what a per-thread budget is measured in) and
//! [`ProcReading::cpu_pct_machine`] is percent of total machine capacity.

use std::time::Instant;

/// One derived CPU/memory reading over the interval since the previous tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcReading {
    /// CPU time consumed as a percentage of one logical core. Can exceed 100
    /// when several threads are busy.
    pub cpu_pct_core: f64,
    /// CPU time consumed as a percentage of all logical cores combined.
    pub cpu_pct_machine: f64,
    /// Resident working set in mebibytes.
    pub working_set_mb: f64,
    /// Private (non-shared) commit in mebibytes. The honest "how much RAM does
    /// this cost" figure, since working set includes shared pages.
    pub private_mb: f64,
}

/// Raw counters read straight from the OS.
#[derive(Debug, Clone, Copy, Default)]
struct Raw {
    /// Kernel + user CPU time in 100-ns units.
    cpu_100ns: u64,
    working_set: u64,
    private: u64,
}

/// Differencing sampler. Construct once, then call [`Sampler::tick`] on a
/// fixed interval; each tick reports the average over the interval that just
/// elapsed.
pub struct Sampler {
    last: Raw,
    last_at: Instant,
    cores: f64,
}

impl Sampler {
    /// Start sampling from now. The first [`tick`](Sampler::tick) covers the
    /// span between construction and that call, so discard it if it is short.
    pub fn new() -> Self {
        Self {
            last: read_raw().unwrap_or_default(),
            last_at: Instant::now(),
            cores: std::thread::available_parallelism()
                .map(|n| n.get() as f64)
                .unwrap_or(1.0),
        }
    }

    /// Sample now and report the average over the interval since the last
    /// tick. Returns `None` if the OS counters could not be read.
    pub fn tick(&mut self) -> Option<ProcReading> {
        let now = read_raw()?;
        let at = Instant::now();
        let wall_100ns = self.last_at.elapsed().as_nanos() as f64 / 100.0;

        let reading = if wall_100ns > 0.0 {
            let cpu_delta = now.cpu_100ns.saturating_sub(self.last.cpu_100ns) as f64;
            let per_core = cpu_delta / wall_100ns * 100.0;
            ProcReading {
                cpu_pct_core: per_core,
                cpu_pct_machine: per_core / self.cores,
                working_set_mb: now.working_set as f64 / (1024.0 * 1024.0),
                private_mb: now.private as f64 / (1024.0 * 1024.0),
            }
        } else {
            ProcReading::default()
        };

        self.last = now;
        self.last_at = at;
        Some(reading)
    }

    /// Number of logical cores the machine-percentage figure is divided by.
    /// Worth printing alongside any published number.
    pub fn cores(&self) -> f64 {
        self.cores
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
fn read_raw() -> Option<Raw> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    unsafe {
        let handle = GetCurrentProcess();

        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user).ok()?;

        // PROCESS_MEMORY_COUNTERS_EX is a superset of the base struct; the API
        // takes the base pointer and distinguishes the two by `cb`.
        let mut pmc = PROCESS_MEMORY_COUNTERS_EX::default();
        let cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        GetProcessMemoryInfo(
            handle,
            &mut pmc as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            cb,
        )
        .ok()?;

        Some(Raw {
            cpu_100ns: filetime_100ns(kernel) + filetime_100ns(user),
            working_set: pmc.WorkingSetSize as u64,
            private: pmc.PrivateUsage as u64,
        })
    }
}

/// FILETIME is a split 64-bit count of 100-ns intervals.
#[cfg(windows)]
fn filetime_100ns(ft: windows::Win32::Foundation::FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// Linux fallback so the metrics module builds and is testable off Windows.
#[cfg(not(windows))]
fn read_raw() -> Option<Raw> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the comm field, which may itself contain spaces.
    let after_comm = stat.rsplit_once(known_comm_end(&stat)?)?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // utime and stime are fields 14 and 15 (1-based); the split above dropped
    // the first three, so they land at indices 11 and 12.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let ticks_per_sec = 100u64; // USER_HZ, fixed at 100 on Linux.
    let cpu_100ns = (utime + stime) * (10_000_000 / ticks_per_sec);

    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let mut cols = statm.split_whitespace();
    let _total_pages = cols.next()?;
    let rss_pages: u64 = cols.next()?.parse().ok()?;
    let page = 4096u64;

    Some(Raw {
        cpu_100ns,
        working_set: rss_pages * page,
        private: rss_pages * page,
    })
}

#[cfg(not(windows))]
fn known_comm_end(stat: &str) -> Option<&str> {
    // comm is parenthesised and may contain spaces or ')', so split on the
    // last ')' in the line.
    stat.rfind(')').map(|i| &stat[i..=i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nonzero_memory() {
        let raw = read_raw().expect("process counters readable");
        assert!(raw.working_set > 0, "working set should be non-zero");
    }

    #[test]
    fn tick_reports_plausible_cpu() {
        let mut s = Sampler::new();
        // Burn a little CPU so the delta is not pure noise.
        let mut acc = 0u64;
        let spin_until = Instant::now() + std::time::Duration::from_millis(50);
        while Instant::now() < spin_until {
            acc = acc.wrapping_add(1);
        }
        assert!(acc > 0);

        let r = s.tick().expect("tick");
        assert!(r.cpu_pct_core > 0.0, "expected CPU usage, got {r:?}");
        // One busy thread cannot exceed one core by much.
        assert!(r.cpu_pct_core < 200.0, "implausible reading: {r:?}");
        assert!(r.working_set_mb > 0.0);
        assert!(r.cpu_pct_machine <= r.cpu_pct_core + f64::EPSILON);
    }

    #[test]
    fn cores_at_least_one() {
        assert!(Sampler::new().cores() >= 1.0);
    }
}
