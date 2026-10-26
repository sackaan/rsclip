//! Time-limited, keyframe-aligned buffer of encoded MPEG-TS data.
//!
//! FFmpeg's stdout arrives in arbitrary byte chunks. The buffer re-frames
//! them into 188-byte TS packets and groups them into GOPs, each starting at
//! a video packet flagged with the random-access indicator (a keyframe).
//! Eviction drops whole GOPs and is measured in video PTS, so a snapshot
//! always starts on a decodable keyframe and its duration is exact, not an
//! estimate from when bytes happened to arrive.

use std::collections::VecDeque;

use parking_lot::RwLock;
use std::sync::Arc;

/// Size of one MPEG-TS packet.
const TS_PACKET: usize = 188;
const TS_SYNC: u8 = 0x47;
/// MPEG-TS presentation timestamps tick at 90 kHz.
pub const PTS_HZ: i64 = 90_000;
const PAT_PID: u16 = 0;

/// One group of pictures: TS packets from a keyframe up to the next one.
struct Gop {
    data: Vec<u8>,
    /// PTS of the keyframe that opens this GOP (unwrapped).
    start_pts: i64,
    /// Largest video PTS seen in this GOP (B-frames arrive out of order).
    end_pts: i64,
}

/// Rolling buffer of encoded video, evicted by whole GOPs.
pub struct EncodedReplayBuffer {
    gops: VecDeque<Gop>,
    window_pts: i64,
    total_bytes: usize,
    /// Bytes of an incomplete packet carried over to the next push.
    partial: Vec<u8>,
    /// Latest program tables, prepended to snapshots so a clip that starts
    /// between table repeats still demuxes.
    pat: Option<Vec<u8>>,
    pmt: Option<Vec<u8>>,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    pts: PtsUnwrap,
    /// PTS of the very first keyframe, i.e. encoder frame 0.
    base_pts: Option<i64>,
}

/// A consistent copy of the buffer taken for a save.
pub struct TsSnapshot {
    /// Program tables followed by every retained GOP.
    pub data: Vec<u8>,
    /// PTS of the first frame in `data`.
    pub start_pts: i64,
    /// PTS of the last frame in `data`.
    pub end_pts: i64,
    /// PTS of encoder frame 0, for mapping PTS back to frame numbers.
    pub base_pts: i64,
}

impl EncodedReplayBuffer {
    /// Create an empty buffer that retains at least `max_duration_secs` of
    /// video once that much has been encoded (plus up to one GOP).
    pub fn new(max_duration_secs: u32) -> Self {
        Self {
            gops: VecDeque::new(),
            window_pts: max_duration_secs as i64 * PTS_HZ,
            total_bytes: 0,
            partial: Vec::with_capacity(TS_PACKET),
            pat: None,
            pmt: None,
            pmt_pid: None,
            video_pid: None,
            pts: PtsUnwrap::default(),
            base_pts: None,
        }
    }

    /// Append raw bytes from the encoder's stdout.
    pub fn push(&mut self, mut bytes: &[u8]) {
        // `partial` only ever holds the head of a packet that started with
        // a sync byte, so completing it yields a whole packet.
        if !self.partial.is_empty() {
            let take = (TS_PACKET - self.partial.len()).min(bytes.len());
            self.partial.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.partial.len() < TS_PACKET {
                return;
            }
            let packet = std::mem::take(&mut self.partial);
            self.push_packet(&packet);
            self.partial = packet;
            self.partial.clear();
        }

        while !bytes.is_empty() {
            if bytes[0] != TS_SYNC {
                match bytes.iter().position(|&b| b == TS_SYNC) {
                    Some(i) => bytes = &bytes[i..],
                    None => return,
                }
            }
            if bytes.len() < TS_PACKET {
                self.partial.extend_from_slice(bytes);
                return;
            }
            let (packet, rest) = bytes.split_at(TS_PACKET);
            self.push_packet(packet);
            bytes = rest;
        }
    }

    fn push_packet(&mut self, p: &[u8]) {
        let pid = ((p[1] as u16 & 0x1F) << 8) | p[2] as u16;
        let pusi = p[1] & 0x40 != 0;
        let afc = (p[3] >> 4) & 0x3;
        let mut payload = 4;
        let mut random_access = false;
        if afc & 0x2 != 0 {
            let af_len = p[4] as usize;
            random_access = af_len > 0 && p[5] & 0x40 != 0;
            payload = 5 + af_len;
        }
        let payload = if afc & 0x1 != 0 && payload < TS_PACKET {
            &p[payload..]
        } else {
            &[][..]
        };

        if pid == PAT_PID {
            if pusi {
                self.pmt_pid = parse_pat_pmt_pid(payload).or(self.pmt_pid);
            }
            self.pat = Some(p.to_vec());
        } else if Some(pid) == self.pmt_pid {
            self.pmt = Some(p.to_vec());
        }

        let mut video_pts = None;
        if pusi {
            if let Some((stream_id, pts)) = parse_pes_pts(payload) {
                if (0xE0..=0xEF).contains(&stream_id) {
                    self.video_pid.get_or_insert(pid);
                    video_pts = pts.map(|raw| self.pts.unwrap(raw));
                }
            }
        }

        let is_video = Some(pid) == self.video_pid;
        if is_video && pusi && random_access {
            if let Some(pts) = video_pts {
                self.base_pts.get_or_insert(pts);
                self.gops.push_back(Gop {
                    data: Vec::new(),
                    start_pts: pts,
                    end_pts: pts,
                });
            }
        }

        // Anything before the first keyframe can't be decoded; drop it.
        let Some(gop) = self.gops.back_mut() else {
            return;
        };
        gop.data.extend_from_slice(p);
        self.total_bytes += TS_PACKET;
        if let Some(pts) = video_pts {
            gop.end_pts = gop.end_pts.max(pts);
            self.evict_old();
        }
    }

    fn evict_old(&mut self) {
        let Some(newest) = self.gops.back().map(|g| g.end_pts) else {
            return;
        };
        // Drop the oldest GOP only while the rest still covers the window,
        // so the front is always a keyframe and the window is never short.
        while self.gops.len() >= 2 && newest - self.gops[1].start_pts >= self.window_pts {
            if let Some(old) = self.gops.pop_front() {
                self.total_bytes -= old.data.len();
            }
        }
    }

    /// Program tables plus every retained GOP, starting on a keyframe.
    pub fn snapshot(&self) -> Option<TsSnapshot> {
        let first = self.gops.front()?;
        let last = self.gops.back()?;
        let tables = self.pat.iter().chain(self.pmt.iter());
        let mut data = Vec::with_capacity(self.total_bytes + 2 * TS_PACKET);
        for t in tables {
            data.extend_from_slice(t);
        }
        for gop in &self.gops {
            data.extend_from_slice(&gop.data);
        }
        Some(TsSnapshot {
            data,
            start_pts: first.start_pts,
            end_pts: last.end_pts,
            base_pts: self.base_pts.unwrap_or(first.start_pts),
        })
    }

    /// Video duration currently retained, from first to last frame PTS.
    pub fn duration_secs(&self) -> f64 {
        match (self.gops.front(), self.gops.back()) {
            (Some(f), Some(l)) => (l.end_pts - f.start_pts) as f64 / PTS_HZ as f64,
            _ => 0.0,
        }
    }

    /// Total bytes currently retained.
    pub fn memory_usage_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Whether no decodable video (no keyframe yet) is buffered.
    pub fn is_empty(&self) -> bool {
        self.gops.is_empty()
    }

    /// Update the retention window and immediately evict any GOPs that
    /// now fall outside it.
    pub fn set_max_duration(&mut self, secs: u32) {
        self.window_pts = secs as i64 * PTS_HZ;
        self.evict_old();
    }
}

/// Shared, lock-protected handle to the [`EncodedReplayBuffer`].
pub type SharedEncodedBuffer = Arc<RwLock<EncodedReplayBuffer>>;

/// Extends 33-bit PTS values past their ~26.5 h wraparound.
#[derive(Default)]
struct PtsUnwrap {
    offset: i64,
    last: Option<i64>,
}

impl PtsUnwrap {
    const WRAP: i64 = 1 << 33;

    fn unwrap(&mut self, raw: i64) -> i64 {
        let mut v = raw + self.offset;
        if let Some(last) = self.last {
            // Small backward steps are B-frame reordering; a huge one is
            // the counter wrapping.
            if v < last - Self::WRAP / 2 {
                self.offset += Self::WRAP;
                v += Self::WRAP;
            }
        }
        self.last = Some(self.last.map_or(v, |l| l.max(v)));
        v
    }
}

/// From a PES packet start, return `(stream_id, pts)`.
fn parse_pes_pts(payload: &[u8]) -> Option<(u8, Option<i64>)> {
    if payload.len() < 9 || payload[..3] != [0, 0, 1] {
        return None;
    }
    let stream_id = payload[3];
    let has_pts = payload[7] & 0x80 != 0;
    if !has_pts || payload.len() < 14 {
        return Some((stream_id, None));
    }
    let b = &payload[9..14];
    let pts = ((b[0] as i64 >> 1) & 0x7) << 30
        | (b[1] as i64) << 22
        | (b[2] as i64 >> 1) << 15
        | (b[3] as i64) << 7
        | b[4] as i64 >> 1;
    Some((stream_id, Some(pts)))
}

/// PID of the first program's PMT, from a PAT section.
fn parse_pat_pmt_pid(payload: &[u8]) -> Option<u16> {
    let pointer = *payload.first()? as usize;
    let sec = payload.get(1 + pointer..)?;
    if sec.len() < 8 || sec[0] != 0x00 {
        return None;
    }
    let section_len = (((sec[1] & 0x0F) as usize) << 8) | sec[2] as usize;
    // Program loop runs from byte 8 to the CRC (last 4 bytes of section).
    let end = (3 + section_len).saturating_sub(4).min(sec.len());
    sec.get(8..end)?
        .as_chunks::<4>()
        .0
        .iter()
        .find(|e| u16::from_be_bytes([e[0], e[1]]) != 0)
        .map(|e| u16::from_be_bytes([e[2] & 0x1F, e[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIDEO: u16 = 0x100;
    const PMT: u16 = 0x1000;

    fn packet(pid: u16, pusi: bool, rai: bool, pts: Option<i64>, fill: u8) -> Vec<u8> {
        let mut p = vec![fill; TS_PACKET];
        p[0] = TS_SYNC;
        p[1] = ((pusi as u8) << 6) | (pid >> 8) as u8;
        p[2] = pid as u8;
        let mut i = 4;
        if rai {
            p[3] = 0x30; // adaptation field + payload
            p[4] = 1;
            p[5] = 0x40;
            i = 6;
        } else {
            p[3] = 0x10;
        }
        if let Some(pts) = pts {
            p[i..i + 9].copy_from_slice(&[0, 0, 1, 0xE0, 0, 0, 0x80, 0x80, 5]);
            p[i + 9] = 0x21 | (((pts >> 30) & 7) << 1) as u8;
            p[i + 10] = (pts >> 22) as u8;
            p[i + 11] = (((pts >> 15) & 0x7F) << 1) as u8 | 1;
            p[i + 12] = (pts >> 7) as u8;
            p[i + 13] = ((pts & 0x7F) << 1) as u8 | 1;
        }
        p
    }

    fn pat() -> Vec<u8> {
        let mut p = vec![0xFF; TS_PACKET];
        p[..4].copy_from_slice(&[TS_SYNC, 0x40, 0x00, 0x10]);
        // pointer, table_id, len=13, tsid, ver, sec, last, program 1 -> PMT
        let sec = [
            0x00,
            0x00,
            0xB0,
            13,
            0,
            1,
            0xC1,
            0,
            0,
            0,
            1,
            0xE0 | (PMT >> 8) as u8,
            PMT as u8,
            0,
            0,
            0,
            0,
        ];
        p[4..4 + sec.len()].copy_from_slice(&sec);
        p
    }

    /// `frames` video frames at 60 fps with a keyframe every `gop` frames,
    /// tables every 6 frames, plus two continuation packets per frame.
    fn stream(frames: i64, gop: i64) -> Vec<u8> {
        let mut out = Vec::new();
        for n in 0..frames {
            if n % 6 == 0 {
                out.extend(pat());
                out.extend(packet(PMT, true, false, None, 0xAA));
            }
            let pts = 126_000 + n * 1500;
            out.extend(packet(VIDEO, true, n % gop == 0, Some(pts), n as u8));
            out.extend(packet(VIDEO, false, false, None, n as u8));
            out.extend(packet(VIDEO, false, false, None, n as u8));
        }
        out
    }

    fn first_video_packet(data: &[u8]) -> &[u8] {
        data.as_chunks::<TS_PACKET>()
            .0
            .iter()
            .find(|p| ((p[1] as u16 & 0x1F) << 8 | p[2] as u16) == VIDEO)
            .unwrap()
    }

    #[test]
    fn pes_pts_roundtrip() {
        for pts in [0, 1, 126_000, (1 << 33) - 1] {
            let p = packet(VIDEO, true, false, Some(pts), 0);
            assert_eq!(parse_pes_pts(&p[4..]), Some((0xE0, Some(pts))));
        }
    }

    #[test]
    fn pat_yields_pmt_pid() {
        assert_eq!(parse_pat_pmt_pid(&pat()[4..]), Some(PMT));
    }

    #[test]
    fn arbitrary_chunking_gives_identical_result() {
        let s = stream(600, 60);
        let mut whole = EncodedReplayBuffer::new(4);
        whole.push(&s);
        let mut chunked = EncodedReplayBuffer::new(4);
        let mut i = 0;
        let mut step = 1;
        while i < s.len() {
            let end = (i + step).min(s.len());
            chunked.push(&s[i..end]);
            i = end;
            step = step * 7 % 4093 + 1;
        }
        let (a, b) = (whole.snapshot().unwrap(), chunked.snapshot().unwrap());
        assert_eq!(a.data, b.data);
        assert_eq!((a.start_pts, a.end_pts), (b.start_pts, b.end_pts));
    }

    #[test]
    fn snapshot_starts_on_keyframe_with_tables_first() {
        let mut buf = EncodedReplayBuffer::new(3);
        buf.push(&stream(600, 60)); // 10 s
        let snap = buf.snapshot().unwrap();
        assert_eq!(snap.data.len() % TS_PACKET, 0);
        assert_eq!(&snap.data[..TS_PACKET], &pat()[..]);
        let pmt_pid =
            (snap.data[TS_PACKET + 1] as u16 & 0x1F) << 8 | snap.data[TS_PACKET + 2] as u16;
        assert_eq!(pmt_pid, PMT);
        let v = first_video_packet(&snap.data[2 * TS_PACKET..]);
        assert_eq!(v[5] & 0x40, 0x40, "first video packet is a keyframe");
        assert_eq!((snap.start_pts - 126_000) % 90_000, 0);
        assert_eq!(snap.base_pts, 126_000);
    }

    #[test]
    fn eviction_keeps_window_plus_at_most_one_gop() {
        let mut buf = EncodedReplayBuffer::new(3);
        buf.push(&stream(600, 60));
        let d = buf.duration_secs();
        assert!((3.0..4.0).contains(&d), "duration {d}");
        let snap = buf.snapshot().unwrap();
        assert_eq!(snap.end_pts, 126_000 + 599 * 1500);
    }

    #[test]
    fn packets_before_first_keyframe_are_dropped() {
        let mut buf = EncodedReplayBuffer::new(30);
        buf.push(&packet(VIDEO, true, false, Some(0), 1));
        buf.push(&packet(VIDEO, false, false, None, 1));
        assert!(buf.is_empty());
        assert_eq!(buf.memory_usage_bytes(), 0);
        buf.push(&pat());
        buf.push(&packet(VIDEO, true, true, Some(1500), 2));
        assert!(!buf.is_empty());
        assert_eq!(buf.memory_usage_bytes(), TS_PACKET);
    }

    #[test]
    fn resyncs_after_garbage() {
        let mut clean = EncodedReplayBuffer::new(30);
        clean.push(&stream(120, 60));
        let mut noisy = EncodedReplayBuffer::new(30);
        noisy.push(&[1, 2, 3]);
        noisy.push(&stream(120, 60));
        assert_eq!(
            clean.snapshot().unwrap().data,
            noisy.snapshot().unwrap().data
        );
    }

    #[test]
    fn shrinking_window_evicts_whole_gops() {
        let mut buf = EncodedReplayBuffer::new(30);
        buf.push(&stream(600, 60));
        assert!(buf.duration_secs() > 9.0);
        let before = buf.memory_usage_bytes();
        buf.set_max_duration(2);
        assert!((2.0..3.0).contains(&buf.duration_secs()));
        assert!(buf.memory_usage_bytes() < before);
        let snap = buf.snapshot().unwrap();
        assert_eq!(first_video_packet(&snap.data)[5] & 0x40, 0x40);
    }

    #[test]
    fn pts_unwrap_handles_wrap_and_reordering() {
        let mut u = PtsUnwrap::default();
        let wrap = PtsUnwrap::WRAP;
        assert_eq!(u.unwrap(wrap - 3000), wrap - 3000);
        assert_eq!(u.unwrap(wrap - 6000), wrap - 6000); // B-frame step back
        assert_eq!(u.unwrap(1500), wrap + 1500);
        assert_eq!(u.unwrap(3000), wrap + 3000);
    }

    #[test]
    fn empty_buffer_reports_nothing() {
        let buf = EncodedReplayBuffer::new(10);
        assert!(buf.is_empty());
        assert!(buf.snapshot().is_none());
        assert_eq!(buf.duration_secs(), 0.0);
    }

    /// Feed real FFmpeg output in odd-sized chunks and check the trimmed
    /// snapshot decodes without errors. Opt-in: `cargo test -- --ignored ts_ring`.
    #[test]
    #[ignore]
    fn ts_ring_snapshot_of_real_ffmpeg_output_decodes_cleanly() {
        use crate::encoding::hw::ffmpeg_cmd;
        let dir = std::env::temp_dir().join(format!("rsclip_ts_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.ts");
        let ok = ffmpeg_cmd()
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc2=s=320x240:r=60")
            .args(["-t", "8", "-c:v", "libx264", "-g", "60", "-f", "mpegts"])
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "ffmpeg available");
        let bytes = std::fs::read(&src).unwrap();

        let mut buf = EncodedReplayBuffer::new(3);
        for chunk in bytes.chunks(65_521) {
            buf.push(chunk);
        }
        let snap = buf.snapshot().unwrap();
        let cut = dir.join("cut.ts");
        std::fs::write(&cut, &snap.data).unwrap();

        let out = ffmpeg_cmd()
            .args(["-v", "error", "-i"])
            .arg(&cut)
            .args(["-f", "null", "-"])
            .output()
            .unwrap();
        let errors = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && errors.trim().is_empty(), "{errors}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
