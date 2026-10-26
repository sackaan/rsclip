"""Render docs/architecture-{dark,light}.png. Needs rsvg-convert.

    python3 scripts/architecture.py
"""
import pathlib
import subprocess

DOCS = pathlib.Path(__file__).parent.parent / "docs"
W, H = 1280, 640

THEMES = {
    "dark": dict(box="#161b22", stroke="#30363d", title="#e6edf3", text="#9198a1",
                 path="#6e7681", video="#50c850", audio="#c89632"),
    "light": dict(box="#f6f8fa", stroke="#d1d9e0", title="#1f2328", text="#59636e",
                  path="#818b98", video="#2f9e44", audio="#b07d1a"),
}

# (x, y, w, h, stage, accent, title, lines, source)
BOXES = [
    (44, 40, 256, 200, "CAPTURE", "video", "Windows Graphics Capture",
     ["RuneLite window, any z-order", "GPU crop to game canvas", "BGRA readback, fixed size"],
     "capture/screen.rs"),
    (356, 40, 256, 200, "QUEUE", "video", "Bounded channel",
     ["Depth 2, recycled buffers", "Full queue = dropped frame", "Writer paces constant FPS"],
     "encoding/encoder.rs"),
    (668, 40, 256, 200, "ENCODE", "video", "ffmpeg, one process",
     ["NVENC · QSV · AMF · x264", "Raw BGRA in on stdin", "MPEG-TS out, 1 s GOPs"],
     "encoding/hw.rs"),
    (980, 40, 256, 200, "BUFFER", "video", "GOP ring",
     ["Re-framed 188-byte TS", "Every GOP starts on a keyframe", "Evicted by video PTS"],
     "encoding/buffer.rs"),
    (44, 330, 568, 200, "AUDIO", "audio", "WASAPI capture",
     ["Process loopback: RuneLite audio only", "Optional microphone and extra app sessions",
      "Per-source rings anchored to wall time, gaps filled"],
     "capture/audio.rs · capture/wasapi.rs"),
    (668, 330, 568, 200, "SAVE", "video", "Save worker",
     ["Hotkey snapshots the ring under a short lock", "Video: stream copy, no re-encode",
      "Audio: mixed for the clip's wall-clock span, AAC"],
     "encoding/remux.rs"),
]

# (x1, y1, x2, y2, accent, label, label_x, label_y)
ARROWS = [
    (300, 140, 356, 140, "video", None, 0, 0),
    (612, 140, 668, 140, "video", None, 0, 0),
    (924, 140, 980, 140, "video", None, 0, 0),
    (1108, 240, 1108, 330, "video", "snapshot on F8", 1120, 290),
    (612, 430, 668, 430, "audio", "PCM", 640, 420),
    (952, 530, 952, 570, "video", None, 0, 0),
]


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def svg(t):
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
           f'viewBox="0 0 {W} {H}" font-family="Noto Sans, DejaVu Sans, sans-serif">']
    out.append("<defs>")
    for k in ("video", "audio"):
        out.append(f'<marker id="{k}" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" '
                   f'markerHeight="7" orient="auto"><path d="M0,0 L10,5 L0,10 z" fill="{t[k]}"/></marker>')
    out.append("</defs>")
    for x, y, w, h, stage, accent, title, lines, src in BOXES:
        out.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="10" '
                   f'fill="{t["box"]}" stroke="{t["stroke"]}" stroke-width="1.5"/>')
        out.append(f'<rect x="{x}" y="{y + 18}" width="3" height="40" rx="1.5" fill="{t[accent]}"/>')
        out.append(f'<text x="{x + 22}" y="{y + 32}" font-size="11" font-weight="700" '
                   f'letter-spacing="1.5" fill="{t[accent]}">{esc(stage)}</text>')
        out.append(f'<text x="{x + 22}" y="{y + 56}" font-size="16" font-weight="600" '
                   f'fill="{t["title"]}">{esc(title)}</text>')
        for i, line in enumerate(lines):
            out.append(f'<text x="{x + 22}" y="{y + 92 + i * 24}" font-size="13.5" '
                       f'fill="{t["text"]}">{esc(line)}</text>')
        out.append(f'<text x="{x + 22}" y="{y + h - 18}" font-size="11.5" '
                   f'font-family="DejaVu Sans Mono, monospace" fill="{t["path"]}">{esc(src)}</text>')
    for x1, y1, x2, y2, accent, label, lx, ly in ARROWS:
        out.append(f'<line x1="{x1}" y1="{y1}" x2="{x2 - (6 if x2 > x1 else 0)}" '
                   f'y2="{y2 - (6 if y2 > y1 else 0)}" stroke="{t[accent]}" stroke-width="2" '
                   f'marker-end="url(#{accent})"/>')
        if label:
            anchor = "middle" if y1 == y2 else "start"
            out.append(f'<text x="{lx}" y="{ly}" font-size="12" text-anchor="{anchor}" '
                       f'fill="{t["text"]}">{esc(label)}</text>')
    # Output pill under the save worker.
    out.append(f'<rect x="852" y="574" width="200" height="36" rx="18" fill="none" '
               f'stroke="{t["video"]}" stroke-width="1.5"/>')
    out.append(f'<text x="952" y="597" font-size="13" text-anchor="middle" '
               f'font-family="DejaVu Sans Mono, monospace" fill="{t["title"]}">clip.mp4</text>')
    out.append("</svg>")
    return "\n".join(out)


for name, theme in THEMES.items():
    src = DOCS / f"architecture-{name}.svg"
    src.write_text(svg(theme))
    subprocess.run(["rsvg-convert", "-z", "2", "-o", str(DOCS / f"architecture-{name}.png"), str(src)],
                   check=True)
    src.unlink()
