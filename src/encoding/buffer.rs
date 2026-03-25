// Time-limited circular buffer of encoded MPEG-TS data.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use std::sync::Arc;

struct EncodedChunk {
    data: Vec<u8>,
    timestamp: Instant,
}

/// Circular buffer of encoded MPEG-TS video data, evicted by elapsed time.
pub struct EncodedReplayBuffer {
    chunks: VecDeque<EncodedChunk>,
    max_duration: Duration,
    total_bytes: usize,
}

impl EncodedReplayBuffer {
    pub fn new(max_duration_secs: u32) -> Self {
        Self {
            chunks: VecDeque::new(),
            max_duration: Duration::from_secs(max_duration_secs as u64),
            total_bytes: 0,
        }
    }

    pub fn push(&mut self, data: Vec<u8>) {
        self.total_bytes += data.len();
        self.chunks.push_back(EncodedChunk {
            data,
            timestamp: Instant::now(),
        });
        self.evict_old();
    }

    fn evict_old(&mut self) {
        if self.chunks.len() < 2 {
            return;
        }
        let newest = self.chunks.back().unwrap().timestamp;
        // Keep at least a small margin so the buffer is slightly over-full
        // rather than cutting content short.
        let cutoff = self.max_duration + Duration::from_secs(2);
        while self.chunks.len() > 1 {
            let oldest = self.chunks.front().unwrap().timestamp;
            if newest.duration_since(oldest) > cutoff {
                if let Some(old) = self.chunks.pop_front() {
                    self.total_bytes -= old.data.len();
                }
            } else {
                break;
            }
        }
    }

    pub fn snapshot(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.total_bytes);
        for chunk in &self.chunks {
            buf.extend_from_slice(&chunk.data);
        }
        buf
    }

    pub fn duration_secs(&self) -> f64 {
        if self.chunks.len() < 2 {
            return 0.0;
        }
        let first = self.chunks.front().unwrap().timestamp;
        let last = self.chunks.back().unwrap().timestamp;
        last.duration_since(first).as_secs_f64()
    }

    pub fn memory_usage_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.chunks.clear();
        self.total_bytes = 0;
    }

    #[cfg(test)]
    fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    pub fn set_max_duration(&mut self, secs: u32) {
        self.max_duration = Duration::from_secs(secs as u64);
        self.evict_old();
    }
}

pub type SharedEncodedBuffer = Arc<RwLock<EncodedReplayBuffer>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_push_and_memory() {
        let mut buf = EncodedReplayBuffer::new(10);
        assert_eq!(buf.memory_usage_bytes(), 0);
        buf.push(vec![0u8; 1000]);
        assert_eq!(buf.memory_usage_bytes(), 1000);
        buf.push(vec![1u8; 500]);
        assert_eq!(buf.memory_usage_bytes(), 1500);
        buf.clear();
        assert_eq!(buf.memory_usage_bytes(), 0);
    }

    #[test]
    fn test_snapshot_concatenates() {
        let mut buf = EncodedReplayBuffer::new(10);
        buf.push(vec![1, 2, 3]);
        buf.push(vec![4, 5]);
        let snap = buf.snapshot();
        assert_eq!(snap, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_eviction_by_duration() {
        let mut buf = EncodedReplayBuffer::new(1); // 1 second + 2s margin = 3s cutoff
        buf.push(vec![0u8; 100]);
        // Sleep past the max duration + margin
        thread::sleep(Duration::from_millis(3500));
        buf.push(vec![1u8; 100]);
        // The old chunk should be evicted
        assert_eq!(buf.chunk_count(), 1);
        assert_eq!(buf.memory_usage_bytes(), 100);
    }

    #[test]
    fn test_is_empty() {
        let mut buf = EncodedReplayBuffer::new(10);
        assert!(buf.is_empty());
        buf.push(vec![0u8; 10]);
        assert!(!buf.is_empty());
    }

    #[test]
    fn test_duration_single_chunk_is_zero() {
        let mut buf = EncodedReplayBuffer::new(10);
        buf.push(vec![0u8; 100]);
        assert_eq!(buf.duration_secs(), 0.0);
    }

    #[test]
    fn test_duration_two_chunks() {
        let mut buf = EncodedReplayBuffer::new(10);
        buf.push(vec![0u8; 100]);
        thread::sleep(Duration::from_millis(100));
        buf.push(vec![0u8; 100]);
        let dur = buf.duration_secs();
        assert!(dur >= 0.05, "duration was {}", dur);
        assert!(dur < 1.0);
    }

    #[test]
    fn test_set_max_duration() {
        let mut buf = EncodedReplayBuffer::new(100);
        buf.push(vec![0u8; 100]);
        thread::sleep(Duration::from_millis(3500));
        buf.push(vec![0u8; 100]);
        assert_eq!(buf.chunk_count(), 2);

        // Shrink the max duration to 1s (+ 2s margin = 3s cutoff).
        // The 3.5s gap should now cause eviction.
        buf.set_max_duration(1);
        assert_eq!(buf.chunk_count(), 1);
    }

    #[test]
    fn test_clear() {
        let mut buf = EncodedReplayBuffer::new(10);
        buf.push(vec![0u8; 100]);
        buf.push(vec![0u8; 200]);
        assert_eq!(buf.memory_usage_bytes(), 300);
        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.memory_usage_bytes(), 0);
        assert_eq!(buf.chunk_count(), 0);
    }

    #[test]
    fn test_shared_buffer_thread_safety() {
        let buf: SharedEncodedBuffer = Arc::new(RwLock::new(EncodedReplayBuffer::new(10)));
        let buf2 = buf.clone();

        let handle = std::thread::spawn(move || {
            buf2.write().push(vec![1, 2, 3]);
        });
        handle.join().unwrap();

        let snap = buf.read().snapshot();
        assert_eq!(snap, vec![1, 2, 3]);
    }
}
