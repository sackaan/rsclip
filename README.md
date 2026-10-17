# RSClip

Instant-replay clipper for [RuneLite](https://runelite.net/) (Old School RuneScape) on Windows.

RSClip continuously buffers the last N seconds of your RuneLite game canvas in
the background. Press a hotkey to dump the buffer to MP4 — no scrubbing, no
manual record/stop, no fullscreen lock.

## Features

- **Windows Graphics Capture** — captures the RuneLite window directly. Works
  with windowed, borderless, and overlapping windows. No fullscreen lock.
- **Hardware H.264 encoding** — autodetects NVENC (NVIDIA), QSV (Intel), or
  AMF (AMD). Falls back to libx264 software encoding, including when a
  hardware encoder is detected but fails to start.
- **Native-resolution, colour-accurate clips** — encodes at the game canvas
  size by default (optional aspect-preserving scaling) in tagged BT.709.
- **Exact clips** — the buffer is cut on keyframes and audio is aligned to
  video by capture timestamps, so clips start cleanly and stay in sync.
- **Per-process audio** — captures only RuneLite's audio via WASAPI process
  loopback (Windows 10 2004+). Optional microphone mix.
- **Configurable** — buffer length, FPS, output resolution, rate control
  (CRF or CBR), encoder preset, scaling filter, audio bitrate.
- **Global hotkeys** — save clip / toggle recording without focusing the app.
- **Automatic canvas detection** — crops to the game viewport, ignoring
  RuneLite's side panels and chrome, and follows it while recording when the
  window is resized or the sidebar is toggled.
- **Nothing lost on stop** — after stopping, or if RuneLite closes, the
  buffer can still be saved until you start a new recording.

## Requirements

- Windows 11. (Video capture works on Windows 10 1903+, but per-process
  audio needs build 20348+, which no Windows 10 client release has.)
- [FFmpeg](https://ffmpeg.org/download.html) on `PATH`, or `ffmpeg.exe` placed
  next to `rsclip.exe`.
- A GPU that supports Windows Graphics Capture (virtually any modern card).

## Build

```powershell
cargo build --release
```

The binary lands in `target/release/rsclip.exe`. Drop `ffmpeg.exe` beside it
or add FFmpeg to your `PATH`.

## Use

1. Launch RuneLite.
2. Launch `rsclip.exe`.
3. Press **▶** to start buffering.
4. Press the save hotkey (default **F8**) to dump the last N seconds to your
   clips folder (default: `Videos\RSClips`).

The save hotkey works while RuneLite is focused — you don't need to switch
windows.

## Hardware encoder support

| Vendor | Codec       | Notes                                              |
|--------|-------------|----------------------------------------------------|
| NVIDIA | `h264_nvenc`| GTX 600-series or newer, recent driver             |
| Intel  | `h264_qsv`  | iGPU with QuickSync (Haswell+); uses NV12          |
| AMD    | `h264_amf`  | GCN-era or newer with AMF driver                   |
| —      | `libx264`   | Software fallback; chosen if no hardware probe succeeds |

The encoder is probed once at startup by encoding a single dummy frame. If
no probe succeeds RSClip uses libx264; if the chosen hardware encoder fails
when recording starts, RSClip switches to libx264 for the rest of the session
and says so in the status line.

**NVENC and driver versions:** FFmpeg 8.x needs NVIDIA driver 570 or newer
for NVENC. With an older driver, either update it or place the `ffmpeg.exe`
from an FFmpeg 7.1 build next to `rsclip.exe` (RSClip prefers that copy over
the one on `PATH`).

## Architecture

```
src/
├── capture/       Windows Graphics Capture + WASAPI audio
│   ├── screen.rs    BGRA frames → channel
│   ├── audio.rs     ring buffers per source + mixer
│   └── wasapi.rs    per-process loopback + mic capture
├── encoding/      Live FFmpeg pipeline
│   ├── encoder.rs   persistent child process, writer/reader threads
│   ├── buffer.rs    keyframe-aligned ring of MPEG-TS GOPs
│   ├── remux.rs     on save: concat TS + WAV → MP4 (stream copy)
│   ├── hw.rs        encoder detection + ffmpeg arg building
│   ├── config.rs    EncoderConfig, RateControl, presets
│   └── types.rs     RawFrame, PreviewFrame
├── platform/      Win32 glue
│   ├── runelite.rs  EnumWindows + canvas crop math
│   └── hotkeys.rs   global-hotkey wrapper + egui key mapping
├── ui/            egui (eframe) GUI
├── logging.rs     in-memory ring buffer for the GUI console
├── utils.rs       paths, defaults, status enums
├── main.rs
└── build.rs       embeds the .ico into the .exe
```

Data flow:

```
WGC FrameArrived ──► (throttle, GPU crop, readback, fit to encoder size)
                         │
                         ▼
                    RawFrame channel ──► encoder-writer thread (constant fps)
                                            │
                                            ▼
                                       ffmpeg.exe (stdin)
                                            │
                                            ▼ stdout (MPEG-TS)
                                       encoder-reader thread
                                            │
                                            ▼
                                       EncodedReplayBuffer (GOPs, evicted by PTS)
                                            │  on save: snapshot + its
                                            │  wall-clock span → matching audio
                                            ▼
                                       save-worker thread ──► remux → MP4
```

## Troubleshooting

- **"FFmpeg not found"** — install FFmpeg and add it to `PATH`, or copy
  `ffmpeg.exe` next to `rsclip.exe`.
- **No audio in saved clips** — per-process loopback needs Windows 10 build
  20348 or later. On older builds RSClip will warn and produce video-only
  clips. Microphone capture works on any Windows 10/11 build.
- **"RuneLite not found"** — RSClip looks for top-level windows titled
  `RuneLite` or `RuneLite - <name>` with a `SunAwt*` window class. If
  RuneLite isn't detected, make sure the launcher's `--mode` is not hiding
  the title.
- **Canvas not detected (no `(canvas)` tag)** — RSClip falls back to
  capturing the full RuneLite window. The game canvas detection relies on
  finding a child `SunAwtCanvas` window; some plugin layouts can hide it.
- **Dropped frames counter rising** — frames are only counted as dropped
  when the encoder's input queue is full (capture faster than the target FPS
  is throttled, not dropped), so a rising count means the encoder can't keep
  up. Lower FPS, switch to a faster preset, or use a hardware encoder.

## Contributing

```powershell
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
# Tests that drive a real FFmpeg (encoder, keyframe ring, remux, A/V sync):
cargo test -- --ignored
```

PRs welcome. For non-trivial changes please open an issue first to discuss
the approach.

## License

[GPL-3.0-only](./LICENSE).
