<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/header-dark.png">
  <img src="docs/header-light.png" alt="RSClip" width="344">
</picture>

**Instant replay for RuneLite. Always buffering, one hotkey to save, clips written by stream copy with no re-encode.**

[![CI](https://img.shields.io/github/actions/workflow/status/sackaan/rsclip/ci.yml?branch=main&label=build)](https://github.com/sackaan/rsclip/actions/workflows/ci.yml)
[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue)](./LICENSE)
[![Rust](https://img.shields.io/badge/rust-2021-orange?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/platform-Windows%2011-0078D6?logo=windows11&logoColor=white)](#requirements)

![Demo](docs/demo.gif)

</div>

RSClip keeps the last N seconds of your [RuneLite](https://runelite.net/) game canvas in a rolling buffer. Something good happens, you press **F8**, and the clip is on disk as an MP4. No record button, no scrubbing, no fullscreen lock.

## Why RSClip

| Feature | Impact |
|---|---|
| **Windows Graphics Capture** | Captures the RuneLite window directly. Windowed, borderless and overlapped windows all work, with no fullscreen lock. |
| **Hardware H.264 encoding** | Autodetects NVENC, QSV or AMF and falls back to libx264 if none works, including when a detected encoder fails to start. |
| **Keyframe-aligned buffer** | The ring is cut on GOP boundaries by video PTS, so every clip starts on a decodable keyframe with an exact duration. |
| **Save by stream copy** | Video is remuxed, not re-encoded, so saving costs a file write and a short AAC audio encode. |
| **Per-process audio** | WASAPI process loopback captures RuneLite only, not Discord or your browser. Optional microphone and extra app channels. |
| **Timestamp-aligned A/V** | Audio is anchored to wall time and matched to each clip's wall-clock span, so sound stays in sync. |
| **Canvas auto-detect** | Crops to the game viewport and ignores side panels. It follows resizes and sidebar toggles while recording. |
| **Native-resolution output** | Encodes at canvas size by default in tagged BT.709, with optional aspect-preserving scaling. |
| **Nothing lost on stop** | If you stop recording or RuneLite closes, the buffer stays saveable until you start a new session. |
| **Multi-client aware** | With several RuneLite clients open, capture follows whichever one has focus, mid-recording included. |
| **Global hotkeys** | Save (F8) and an optional start/stop hotkey work while the game has focus. |

## Quick start

Requires Windows 11, [Rust](https://rustup.rs/) and [FFmpeg](https://ffmpeg.org/download.html).

```powershell
winget install Gyan.FFmpeg
git clone https://github.com/sackaan/rsclip.git
cd rsclip
cargo run --release
```

Then:

1. Launch RuneLite.
2. Press **▶** in RSClip.
3. Press **F8**. The last 30 s lands in `Videos\RSClips`.

RSClip is a GUI app and has no command-line flags. Everything is set in the settings screen and persisted. If you don't install FFmpeg system-wide, put `ffmpeg.exe` next to `rsclip.exe`. RSClip prefers that copy over the one on `PATH`.

### Common setups

| Scenario | Settings |
|---|---|
| **Default** | 30 s buffer, 60 FPS, native canvas resolution, CRF 23 |
| **Long buffer** | Buffer up to 300 s |
| **High frame rate** | FPS up to 360 |
| **Fixed 1080p output** | Turn off *native resolution*, set 1920×1080. Aspect ratio is kept and the scale filter is selectable (Lanczos by default). |
| **4K output** | Turn off *native resolution*, set 3840×2160 (up to 7680×4320). Use a hardware encoder; x264 at 4K60 will drop frames on most CPUs. |
| **Specific client** | RSClip only ever captures RuneLite windows. With several clients open, click into the one you want; capture switches to it. |
| **Fixed bitrate for uploads** | Rate control **CBR**, 6000 kbps by default (500 to 50000) |
| **Voice over gameplay** | Enable microphone and pick the device. Add Discord or others as app channels. |
| **Low CPU on no-GPU machines** | x264 preset `ultrafast` or `superfast` |

## Architecture

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/architecture-dark.png">
  <img src="docs/architecture-light.png" alt="RSClip pipeline: Windows Graphics Capture, bounded queue, ffmpeg encoder and GOP ring buffer on the video path; WASAPI audio rings; a save worker that stream-copies video and mixes audio into an MP4">
</picture>

### Lifecycle

**1. Frame capture** (`src/capture/screen.rs`)
RSClip uses Windows Graphics Capture, not DXGI Desktop Duplication. That is why it can capture one window even when it is covered. The `FrameArrived` callback runs on a WGC thread. It throttles to the target FPS, copies the canvas region on the GPU into a staging texture, maps it, and writes a packed BGRA frame of exactly the encoder's size. Output size is fixed for the whole session so FFmpeg never restarts. If the source changes size, the frame is scaled to fit and letterboxed.

**2. Buffer and queue management** (`src/encoding/encoder.rs`)
Frames travel over a bounded `crossbeam` channel of depth 2, with a small pool of recycled frame buffers to avoid per-frame allocation. The capture thread never blocks. If the queue is full, the frame is counted as dropped. A writer thread feeds FFmpeg at a constant rate. Writes up to 20 ms late are stamped with their scheduled time to absorb timer jitter. If it falls more than 100 ms behind, it resyncs to wall time instead of bursting duplicate frames.

**3. Audio synchronization** (`src/capture/audio.rs`, `wasapi.rs`)
Each source (game, microphone, extra apps) has its own ring buffer of f32 PCM, anchored to wall time. Delivery gaps over 30 ms are filled with silence so the sample timeline keeps matching the clock. Game audio uses WASAPI process loopback, which needs Windows build 20348 or later. At save time the mixer blends the sources for the clip's exact wall-clock span.

**4. Hardware encoding** (`src/encoding/hw.rs`)
A single persistent `ffmpeg` process reads raw BGRA from stdin and writes MPEG-TS to stdout. At startup RSClip probes each encoder with a dummy frame and picks the first that works. The keyframe interval equals the FPS, so the buffer can be cut once per second.

**5. Muxing and file I/O** (`src/encoding/buffer.rs`, `remux.rs`)
A reader thread re-frames FFmpeg's output into 188-byte TS packets and groups them into GOPs that start at a random-access keyframe. Old GOPs are evicted by video PTS. On hotkey, a snapshot of the ring is taken under a short lock and handed to a save worker, so capture keeps running. The worker remuxes to MP4 by stream copy and encodes the mixed audio to AAC. If a clip's wall-clock span differs from its encoded duration by more than 0.1%, the video is retimed to the wall-clock span.

The diagram is generated by `scripts/architecture.py` (needs `rsvg-convert`). Edit it there and rerun to update both theme variants.

## Hardware encoding

| Vendor | Encoder | Notes |
|---|---|---|
| NVIDIA | `h264_nvenc` | Maxwell (GTX 750 / 900 series) or newer |
| Intel | `h264_qsv` | iGPU with QuickSync (Haswell+), uses NV12 |
| AMD | `h264_amf` | GCN or newer with AMF driver |
| any | `libx264` | Software fallback if no probe succeeds |

FFmpeg 8.x needs NVIDIA driver 570 or newer for NVENC. With an older driver, update it or place an FFmpeg 7.1 `ffmpeg.exe` next to `rsclip.exe`.

## Performance

RSClip does not publish comparison numbers it hasn't measured. The table lists what each tool does by design. The measured column is yours to fill in with the built-in instrumentation.

| | RSClip | OBS (Replay Buffer) | ShadowPlay Instant Replay |
|---|---|---|---|
| Save path | Stream copy of buffered GOPs | Writes buffered encoded packets | Writes buffered encoded stream |
| Capture API | Windows Graphics Capture | WGC, DXGI duplication or game hook | NVIDIA driver |
| Target | RuneLite game canvas, cropped | Any scene | Games or desktop |
| Per-app audio | Yes (process loopback) | Yes (Application Audio Capture) | No |
| GPU vendor | NVIDIA, Intel, AMD, or CPU | Any | NVIDIA only |
| Dropped-frame accounting | Counted when the encoder queue is full | Reported in stats | Not exposed |
| Instrumentation | HdrHistogram p50/p99/p99.9, CPU, allocs per frame | Stats dock | None |

### Measure it on your machine

The `metrics` feature adds per-stage latency histograms, a counting allocator and a process CPU/memory sampler. It is compiled out of normal builds, so release builds carry no measurement overhead.

```powershell
cargo run --release --features metrics
```

Each run writes a CSV plus a percentile summary. It covers capture-to-submit latency, buffer push, frame interval jitter, and hotkey-to-file export time.

One number is already measured and drives a default: libx264 is capped at 8 threads. x264's own default (1.5× logical cores) spent about 25 to 30% more CPU for the same output, and 8 threads still encode 1440p60 at `veryfast` roughly 2.8× faster than real time.

| Metric (60 FPS, native canvas) | NVENC | QSV | AMF | x264 veryfast |
|---|---|---|---|---|
| Process CPU % | _TBD_ | _TBD_ | _TBD_ | _TBD_ |
| Capture to submit p99 | _TBD_ | _TBD_ | _TBD_ | _TBD_ |
| Frame interval p99 | _TBD_ | _TBD_ | _TBD_ | _TBD_ |
| Dropped frames per hour | _TBD_ | _TBD_ | _TBD_ | _TBD_ |
| Export time (30 s clip) | _TBD_ | _TBD_ | _TBD_ | _TBD_ |

Results on your hardware are welcome as PRs or issues.

## Requirements

- Windows 11. Video capture works on Windows 10 1903+, but per-process audio needs build 20348+, which no Windows 10 client release has.
- FFmpeg on `PATH`, or `ffmpeg.exe` next to `rsclip.exe`.
- A GPU that supports Windows Graphics Capture (nearly any modern card).

## Project layout

```text
src/
├── capture/     screen.rs (WGC)  audio.rs (rings + mixer)  wasapi.rs (loopback, mic)
├── encoding/    encoder.rs  buffer.rs (GOP ring)  remux.rs  hw.rs  config.rs
├── platform/    runelite.rs (window + canvas detection)  hotkeys.rs
├── metrics/     latency histograms, allocator, CPU sampler (feature-gated)
└── ui/          egui / eframe GUI
```

## Troubleshooting

- **"FFmpeg not found"** — install FFmpeg and add it to `PATH`, or copy `ffmpeg.exe` next to `rsclip.exe`.
- **No audio in clips** — per-process loopback needs Windows build 20348+. On older builds clips are video-only. Microphone capture works everywhere.
- **"RuneLite not found"** — RSClip looks for top-level windows titled `RuneLite` or `RuneLite - <name>` with a `SunAwt*` class.
- **No `(canvas)` tag** — RSClip falls back to the whole window. Some plugin layouts hide the `SunAwtCanvas` child.
- **Dropped frames rising** — the encoder can't keep up. Lower FPS, use a faster preset, or use a hardware encoder.

## Contributing

```powershell
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
cargo test -- --ignored   # tests that drive a real FFmpeg
```

Good first areas: HEVC/AV1 output, Linux or macOS capture backends, more games beyond RuneLite, and benchmark results. For non-trivial changes, open an issue first.

If RSClip saved you a clip, a ⭐ helps others find it.

## License

[GPL-3.0-only](./LICENSE)
