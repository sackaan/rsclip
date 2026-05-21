# RSClip

Instant-replay clipper for [RuneLite](https://runelite.net/) (Old School RuneScape) on Windows.

RSClip continuously buffers the last N seconds of your RuneLite game canvas in
the background. Press a hotkey to dump the buffer to MP4 — no scrubbing, no
manual record/stop, no fullscreen lock.

## Features

- **Windows Graphics Capture** — captures the RuneLite window directly. Works
  with windowed, borderless, and overlapping windows. No fullscreen lock.
- **Hardware H.264 encoding** — autodetects NVENC (NVIDIA), QSV (Intel), or
  AMF (AMD). Falls back to libx264 software encoding.
- **Per-process audio** — captures only RuneLite's audio via WASAPI process
  loopback (Windows 10 2004+). Optional microphone mix.
- **Configurable** — buffer length, FPS, output resolution, rate control
  (CRF or CBR), encoder preset, scaling filter, audio bitrate.
- **Global hotkeys** — save clip / toggle recording without focusing the app.
- **Automatic canvas detection** — crops to the game viewport, ignoring
  RuneLite's side panels and chrome.

## Requirements

- Windows 10 build 20348 (21H2) or later, or Windows 11.
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

The encoder is probed at startup by encoding a single dummy frame. If the
first probe fails (e.g. driver missing), RSClip silently falls back.

## Architecture

```
src/
├── capture/       Windows Graphics Capture + WASAPI audio
│   ├── screen.rs    BGRA frames → channel
│   ├── audio.rs     ring buffers per source + mixer
│   └── wasapi.rs    per-process loopback + mic capture
├── encoding/      Live FFmpeg pipeline
│   ├── encoder.rs   persistent child process, writer/reader threads
│   ├── buffer.rs    time-bounded ring of MPEG-TS chunks
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
WGC FrameArrived ──► RawFrame channel ──► encoder-writer thread
                                            │
                                            ▼
                                       ffmpeg.exe (stdin)
                                            │
                                            ▼ stdout (MPEG-TS)
                                       encoder-reader thread
                                            │
                                            ▼
                                       EncodedReplayBuffer (time-bounded ring)
                                            │  on save:
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
- **Dropped frames counter rising** — encoder can't keep up. Lower FPS,
  switch to a faster preset, or use a hardware encoder.

## Contributing

```powershell
cargo fmt
cargo clippy --all-targets
cargo test
```

PRs welcome. For non-trivial changes please open an issue first to discuss
the approach.

## License

[GPL-3.0-only](./LICENSE).
