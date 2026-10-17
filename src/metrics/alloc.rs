//! Counting global allocator.
//!
//! Wraps the system allocator and tallies every allocation. Sampling the
//! counters across a fixed window of frames turns into the number that matters
//! for a real-time pipeline: **heap allocations per frame in steady state**.
//! A capture loop that allocates per frame is one page fault or one allocator
//! lock away from a dropped frame, so the target is zero.
//!
//! Only installed as `#[global_allocator]` when the `metrics` feature is on —
//! see `main.rs`. Without the feature the counters stay at zero and
//! [`snapshot`] reports nothing.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

/// Total allocations since process start.
pub static ALLOCS: AtomicU64 = AtomicU64::new(0);
/// Total deallocations since process start.
pub static DEALLOCS: AtomicU64 = AtomicU64::new(0);
/// Total bytes requested by allocations since process start.
pub static BYTES_ALLOCATED: AtomicU64 = AtomicU64::new(0);
/// Bytes currently outstanding (allocated minus freed) — a lower bound on
/// resident heap, ignoring allocator slack and fragmentation.
pub static BYTES_LIVE: AtomicU64 = AtomicU64::new(0);

/// System allocator with counters bolted on.
///
/// The counters are `Relaxed` atomics: a few nanoseconds per call, and exact
/// ordering between threads does not matter because only window *deltas* are
/// ever read.
pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            bump_alloc(layout.size() as u64);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            bump_alloc(layout.size() as u64);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES_LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            // A realloc is a real allocator round trip, so it counts as one
            // allocation event even when it grows in place.
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES_ALLOCATED.fetch_add(new_size as u64, Ordering::Relaxed);
            let old = layout.size() as u64;
            let new = new_size as u64;
            if new >= old {
                BYTES_LIVE.fetch_add(new - old, Ordering::Relaxed);
            } else {
                BYTES_LIVE.fetch_sub(old - new, Ordering::Relaxed);
            }
        }
        new_ptr
    }
}

#[inline]
fn bump_alloc(size: u64) {
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    BYTES_ALLOCATED.fetch_add(size, Ordering::Relaxed);
    BYTES_LIVE.fetch_add(size, Ordering::Relaxed);
}

/// Point-in-time reading of the allocation counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AllocSnapshot {
    /// Cumulative allocation count.
    pub allocs: u64,
    /// Cumulative deallocation count.
    pub deallocs: u64,
    /// Cumulative bytes requested.
    pub bytes_allocated: u64,
    /// Bytes currently outstanding.
    pub bytes_live: u64,
}

impl AllocSnapshot {
    /// Counters accumulated between `earlier` and `self`.
    ///
    /// Cumulative fields subtract; `bytes_live` is a gauge, so the later
    /// reading is carried through as-is.
    pub fn since(self, earlier: AllocSnapshot) -> AllocSnapshot {
        AllocSnapshot {
            allocs: self.allocs.saturating_sub(earlier.allocs),
            deallocs: self.deallocs.saturating_sub(earlier.deallocs),
            bytes_allocated: self.bytes_allocated.saturating_sub(earlier.bytes_allocated),
            bytes_live: self.bytes_live,
        }
    }
}

/// Read the counters. All-zero unless the `metrics` feature installed the
/// counting allocator.
pub fn snapshot() -> AllocSnapshot {
    AllocSnapshot {
        allocs: ALLOCS.load(Ordering::Relaxed),
        deallocs: DEALLOCS.load(Ordering::Relaxed),
        bytes_allocated: BYTES_ALLOCATED.load(Ordering::Relaxed),
        bytes_live: BYTES_LIVE.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_subtracts_cumulative_and_keeps_gauge() {
        let a = AllocSnapshot {
            allocs: 10,
            deallocs: 4,
            bytes_allocated: 1000,
            bytes_live: 600,
        };
        let b = AllocSnapshot {
            allocs: 25,
            deallocs: 20,
            bytes_allocated: 4000,
            bytes_live: 900,
        };
        let d = b.since(a);
        assert_eq!(d.allocs, 15);
        assert_eq!(d.deallocs, 16);
        assert_eq!(d.bytes_allocated, 3000);
        assert_eq!(d.bytes_live, 900);
    }

    #[test]
    fn since_saturates_on_reset() {
        let later = AllocSnapshot::default();
        let earlier = AllocSnapshot {
            allocs: 5,
            ..Default::default()
        };
        assert_eq!(later.since(earlier).allocs, 0);
    }

    /// Exercises `Counting` directly rather than through
    /// `#[global_allocator]`, so the test holds whether or not the feature
    /// installed it. Deltas use `>=` because other test threads may be
    /// allocating through the same counters concurrently.
    #[test]
    fn counting_allocator_tracks_alloc_and_dealloc() {
        const SIZE: usize = 4096;
        let layout = Layout::from_size_align(SIZE, 8).expect("valid layout");

        unsafe {
            let before = snapshot();
            let ptr = Counting.alloc(layout);
            assert!(!ptr.is_null(), "allocation failed");
            let mid = snapshot();

            assert!(mid.allocs > before.allocs);
            assert!(mid.bytes_allocated >= before.bytes_allocated + SIZE as u64);

            Counting.dealloc(ptr, layout);
            let after = snapshot();
            assert!(after.deallocs > mid.deallocs);
        }
    }

    #[test]
    fn realloc_counts_once_and_adjusts_live() {
        let layout = Layout::from_size_align(64, 8).expect("valid layout");
        unsafe {
            let ptr = Counting.alloc(layout);
            assert!(!ptr.is_null());
            let before = snapshot();
            let grown = Counting.realloc(ptr, layout, 256);
            assert!(!grown.is_null());
            let after = snapshot();
            assert!(after.allocs > before.allocs);
            assert!(after.bytes_allocated >= before.bytes_allocated + 256);

            Counting.dealloc(grown, Layout::from_size_align(256, 8).unwrap());
        }
    }
}
