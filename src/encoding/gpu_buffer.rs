//! Rolling replay buffer for the GPU (CS2) encode path.
//!
//! Unlike the RuneLite buffer — which stores opaque MPEG-TS byte chunks that
//! FFmpeg can cut anywhere — the GPU encoder produces a raw H.264 elementary
//! stream. That can only be decoded from an IDR (keyframe), so this buffer is
//! frame-aware: it holds individual access units and only ever evicts whole
//! GOPs, guaranteeing the oldest retained frame is always a keyframe.
//!
//! On save, [`EncodedAuBuffer::snapshot`] concatenates the SPS/PPS header and
//! every retained access unit into a self-contained Annex-B stream ready for
//! FFmpeg (decode → motion-blur blend → MP4).

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;

use super::gpu_encoder::{EncodedAu, VideoCodec};

/// Thread-shared handle to the rolling access-unit buffer.
pub type SharedAuBuffer = Arc<Mutex<EncodedAuBuffer>>;

/// A snapshot of the buffer, ready to hand to FFmpeg.
pub struct AuSnapshot {
    /// Sequence header followed by every retained access unit (Annex-B).
    pub annexb: Vec<u8>,
    /// Wall-clock duration the snapshot spans, in seconds.
    pub duration_secs: f64,
    /// Number of frames in the snapshot (for CFR framerate hints).
    pub frame_count: u64,
    /// Codec of the elementary stream (so the save path picks the right
    /// FFmpeg demuxer).
    pub codec: VideoCodec,
}

/// Time-bounded ring of H.264 access units, trimmed on GOP boundaries.
pub struct EncodedAuBuffer {
    aus: VecDeque<EncodedAu>,
    /// Retention window in 100-ns units.
    window_100ns: i64,
    /// Sequence header (SPS/PPS, or VPS/SPS/PPS for HEVC), prepended on snapshot.
    sequence_header: Vec<u8>,
    /// Codec of the buffered elementary stream.
    codec: VideoCodec,
}

impl EncodedAuBuffer {
    /// Create a buffer retaining roughly `buffer_seconds` of footage.
    pub fn new(buffer_seconds: u32) -> Self {
        Self {
            aus: VecDeque::new(),
            window_100ns: buffer_seconds.max(1) as i64 * 10_000_000,
            sequence_header: Vec::new(),
            codec: VideoCodec::H264,
        }
    }

    /// Record the encoder's sequence header (called once at start).
    pub fn set_sequence_header(&mut self, header: Vec<u8>) {
        if !header.is_empty() {
            self.sequence_header = header;
        }
    }

    /// Record which codec the encoder settled on (called once at start).
    pub fn set_codec(&mut self, codec: VideoCodec) {
        self.codec = codec;
    }

    /// Whether any footage is buffered.
    pub fn is_empty(&self) -> bool {
        self.aus.is_empty()
    }

    /// Total bytes of buffered access units (for the UI memory readout).
    pub fn memory_usage_bytes(&self) -> usize {
        self.aus.iter().map(|a| a.data.len()).sum()
    }

    /// Append a freshly encoded access unit and trim old GOPs.
    pub fn push(&mut self, au: EncodedAu) {
        self.aus.push_back(au);
        self.trim();
    }

    /// Drop whole leading GOPs while doing so still leaves at least the
    /// retention window covered. This keeps the front access unit a keyframe.
    fn trim(&mut self) {
        let newest = match self.aus.back() {
            Some(a) => a.pts_100ns,
            None => return,
        };

        loop {
            // Find the second keyframe (start of the second retained GOP).
            let second_kf = self
                .aus
                .iter()
                .enumerate()
                .filter(|(_, a)| a.keyframe)
                .nth(1)
                .map(|(i, a)| (i, a.pts_100ns));

            match second_kf {
                // Only trim if dropping the first GOP still leaves >= window.
                Some((idx, kf_pts)) if newest - kf_pts >= self.window_100ns => {
                    self.aus.drain(0..idx);
                }
                _ => break,
            }
        }
    }

    /// Approximate buffered duration in seconds.
    pub fn duration_secs(&self) -> f64 {
        match (self.aus.front(), self.aus.back()) {
            (Some(f), Some(b)) if self.aus.len() > 1 => {
                (b.pts_100ns - f.pts_100ns) as f64 / 10_000_000.0
            }
            _ => 0.0,
        }
    }

    /// Build a decodable Annex-B snapshot of the whole buffer. The front is
    /// always a keyframe, so the SPS/PPS header + retained units decode
    /// standalone.
    pub fn snapshot(&self) -> Option<AuSnapshot> {
        if self.aus.is_empty() {
            return None;
        }

        let mut annexb = Vec::with_capacity(
            self.sequence_header.len() + self.aus.iter().map(|a| a.data.len()).sum::<usize>(),
        );
        // Prepend SPS/PPS in case the first IDR doesn't carry it inline.
        annexb.extend_from_slice(&self.sequence_header);
        for au in &self.aus {
            annexb.extend_from_slice(&au.data);
        }

        Some(AuSnapshot {
            annexb,
            duration_secs: self.duration_secs(),
            frame_count: self.aus.len() as u64,
            codec: self.codec,
        })
    }
}
