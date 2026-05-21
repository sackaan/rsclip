//! Frame and request types shared between capture, encoding, and the GUI.

use std::sync::Arc;

use parking_lot::Mutex;

/// A raw BGRA frame produced by [`crate::capture::screen`].
pub struct RawFrame {
    /// Pixel data, `width * height * 4` bytes, BGRA, top-down, packed.
    pub data: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

/// A small RGBA thumbnail used to render the live preview in the GUI.
pub struct PreviewFrame {
    /// Pixel data, `width * height * 4` bytes, RGBA, top-down.
    pub rgba: Vec<u8>,
    /// Thumbnail width in pixels (≤ 320).
    pub width: u32,
    /// Thumbnail height in pixels.
    pub height: u32,
}

/// Shared slot used by the encoder/preview thread to publish the latest
/// preview frame; read once per GUI repaint.
pub type SharedPreviewFrame = Arc<Mutex<Option<PreviewFrame>>>;

pub(crate) struct SaveRequest {
    pub encoded_data: Vec<u8>,
    pub output_dir: String,
    pub audio_wav: Option<Vec<u8>>,
    pub video_duration_secs: f64,
    pub audio_bitrate_kbps: u32,
}

const PREVIEW_MAX_WIDTH: usize = 320;

/// Build a small RGBA preview from a full BGRA raw frame using
/// nearest-neighbour downscaling. Output is capped at [`PREVIEW_MAX_WIDTH`]
/// pixels wide while preserving the input aspect ratio.
pub fn build_preview_frame(raw: &RawFrame) -> PreviewFrame {
    let src_w = raw.width as usize;
    let src_h = raw.height as usize;

    let (dst_w, dst_h) = if src_w > PREVIEW_MAX_WIDTH {
        let dw = PREVIEW_MAX_WIDTH;
        let dh = (src_h * dw / src_w).max(1);
        (dw, dh)
    } else {
        (src_w.max(1), src_h.max(1))
    };

    let mut rgba = vec![0u8; dst_w * dst_h * 4];
    let src = &raw.data;

    for dy in 0..dst_h {
        let sy = (dy * src_h / dst_h).min(src_h - 1);
        let src_row = sy * src_w * 4;
        let dst_row = dy * dst_w * 4;
        for dx in 0..dst_w {
            let sx = (dx * src_w / dst_w).min(src_w - 1);
            let si = src_row + sx * 4;
            let di = dst_row + dx * 4;
            if si + 3 < src.len() {
                // BGRA -> RGBA
                rgba[di] = src[si + 2]; // R
                rgba[di + 1] = src[si + 1]; // G
                rgba[di + 2] = src[si]; // B
                rgba[di + 3] = 255; // A
            }
        }
    }
    PreviewFrame {
        rgba,
        width: dst_w as u32,
        height: dst_h as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(width: u32, height: u32, pixel: [u8; 4]) -> RawFrame {
        let data = pixel.repeat((width * height) as usize);
        RawFrame {
            data,
            width,
            height,
        }
    }

    #[test]
    fn preview_passthrough_small_frame() {
        let frame = make_frame(100, 80, [0, 128, 255, 255]); // BGRA
        let pf = build_preview_frame(&frame);
        assert_eq!(pf.width, 100);
        assert_eq!(pf.height, 80);
        assert_eq!(pf.rgba.len(), 100 * 80 * 4);
        // BGRA [0, 128, 255, 255] -> RGBA [255, 128, 0, 255]
        assert_eq!(pf.rgba[0], 255); // R
        assert_eq!(pf.rgba[1], 128); // G
        assert_eq!(pf.rgba[2], 0); // B
        assert_eq!(pf.rgba[3], 255); // A
    }

    #[test]
    fn preview_downscales_large_frame() {
        let frame = make_frame(1920, 1080, [0, 0, 255, 255]);
        let pf = build_preview_frame(&frame);
        assert_eq!(pf.width, PREVIEW_MAX_WIDTH as u32);
        assert!(pf.height > 0);
        assert!(pf.height < 1080);
        // Aspect ratio preserved: 320/1920 * 1080 = 180
        assert_eq!(pf.height, 180);
        assert_eq!(pf.rgba.len(), (pf.width * pf.height * 4) as usize);
    }

    #[test]
    fn preview_at_boundary_width() {
        let frame = make_frame(PREVIEW_MAX_WIDTH as u32, 200, [100, 150, 200, 255]);
        let pf = build_preview_frame(&frame);
        assert_eq!(pf.width, PREVIEW_MAX_WIDTH as u32);
        assert_eq!(pf.height, 200);
    }

    #[test]
    fn preview_bgra_to_rgba_conversion() {
        // Create 2x1 frame with two different pixels
        let mut data = Vec::new();
        data.extend_from_slice(&[10, 20, 30, 40]); // BGRA pixel 0
        data.extend_from_slice(&[50, 60, 70, 80]); // BGRA pixel 1
        let frame = RawFrame {
            data,
            width: 2,
            height: 1,
        };
        let pf = build_preview_frame(&frame);
        assert_eq!(pf.width, 2);
        assert_eq!(pf.height, 1);
        // Pixel 0: BGRA [10, 20, 30, 40] -> RGBA [30, 20, 10, 255]
        assert_eq!(pf.rgba[0], 30);
        assert_eq!(pf.rgba[1], 20);
        assert_eq!(pf.rgba[2], 10);
        assert_eq!(pf.rgba[3], 255);
        // Pixel 1: BGRA [50, 60, 70, 80] -> RGBA [70, 60, 50, 255]
        assert_eq!(pf.rgba[4], 70);
        assert_eq!(pf.rgba[5], 60);
        assert_eq!(pf.rgba[6], 50);
        assert_eq!(pf.rgba[7], 255);
    }

    #[test]
    fn preview_1x1_frame() {
        let frame = make_frame(1, 1, [0, 0, 0, 0]);
        let pf = build_preview_frame(&frame);
        assert_eq!(pf.width, 1);
        assert_eq!(pf.height, 1);
        assert_eq!(pf.rgba.len(), 4);
    }
}
