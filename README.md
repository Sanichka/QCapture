# QCapture

QCapture is a lightweight, cross-platform screen recorder written in Rust.
Goal: OBS-grade power (hardware encoding, mixed audio, annotations) with the
speed and simplicity of tools like VokoScreenNG — on Windows, Linux, and macOS.

## Platform support

| | Windows | Linux | macOS |
|---|---|---|---|
| Monitor / region / window capture | WGC (GPU) | xcap bridge (AVFoundation-grade on mac; screenshot polling on Linux) | xcap bridge |
| Encoders | MF H.264/HEVC, NVENC/AMF/QSV/x264 | NVENC/QSV/x264 (whatever ffmpeg has) | VideoToolbox/x264 |
| System audio | WASAPI loopback | PipeWire/ALSA monitor source (best-effort) | mic-only (no OS loopback API) |
| Mic | yes | yes | yes |
| Annotations, live draw, widget | yes | yes | yes |
| Cursor fx | yes | no (no portable cursor position API yet) | no |

Linux/macOS share one portable capture backend feeding the same compositor
and ffmpeg pipeline, so recordings behave identically across OSes. CI runs
fmt + clippy + tests on all three; hardware encoding still needs a real GPU
and a human smoke test per machine.

## Features

- **Capture targets** — fullscreen monitor, single window (by title substring),
  or drag-selected region (`pick-region` overlay with frozen screenshot backdrop).
- **Two encode paths**
  - Native MediaFoundation H.264/HEVC (zero ffmpeg dependency, GPU surfaces
    stay on GPU).
  - FFmpeg rawvideo pipe with hardware encoders: NVENC, AMF, QSV, x264, plus
    AAC audio through a Windows named pipe.
- **Rate control** — CBR, VBR (with maxrate ceiling), CQP, CRF (x264).
  MF path is CBR-only; other modes need an ffmpeg encoder.
- **Audio** — system loopback + optional mic, mixed into one AAC track
  (48 kHz stereo) with per-source gain/mute and a live VU meter. WASAPI on
  Windows, cpal monitor source on Linux, mic-only on macOS. Fail-soft:
  a broken audio device never loses the video.
- **Annotations (hybrid)** — vector strokes (pen, line, arrow, rect, ellipse,
  text, image) authored in normalized coords, burned into the video *and*
  saved as a `.qcap.json` sidecar for re-editing. Includes a fullscreen
  visual editor (`annotate`) and a demo doc generator (`annotate-demo`).
- **Live drawing** — eframe draw panel with a live video texture showing
  exactly what the encoder sees (no transparent overlay). Works for
  screen, region, and window targets; strokes burn in in real time.
- **Cursor fx** — opt-in highlight ring + click ripple burned into the video
  (Windows-only for now), with tunable colors (incl. alpha), radii, ripple
  lifetime and an additive glow mode (Advanced window, or
  `--cursor-color/--cursor-size/--ripple-color/--ripple-size/--ripple-ms/--cursor-additive`).
- **Fixed canvas** — the encoder initializes once; resizes and window-size
  drift are center-cropped/padded mid-record instead of re-initing or freezing.
- **Pause/resume** — widget ⏸ button or `--pause-at/--resume-at` seconds on
  the CLI. Both A/V clocks freeze together, so paused spans are cut from the
  file and sync is preserved. `--duration` stays wall-clock.
- **Countdown** — widget ⏳ toggle (3s overlay, persisted) or `--countdown`
  seconds on the CLI. Recording starts when it expires; Ctrl-C cancels.
- **Floating widget** (`widget`) — target picker, Start/Stop, live audio
  controls, Advanced window (fps, bitrate, rate control, canvas, encoder,
  cursor capture), draw viewport, output folder shortcut.

## Requirements

- Windows 10+, a modern Linux desktop (X11 or Wayland), or macOS.
- Rust 1.78+ (`cargo build --release -p qcapture-cli`).
- `ffmpeg` on PATH (`qcapture --probe-ffmpeg` shows what your build provides;
  bare `--encoder h264`/`hevc` resolve to the best available family member).
- Release binary stays under ~11 MB.

## Install

Prebuilt binaries ride every GitHub release (`v*` tags):
`qcapture-<version>-windows-x86_64.zip`,
`qcapture-<version>-linux-x86_64.tar.gz`,
`qcapture-<version>-macos-aarch64.tar.gz` — unpack and run.
`ffmpeg` must be on PATH (see below). Platform notes:

- **Windows**: SmartScreen may flag the unsigned exe (More info → Run anyway).
- **Linux**: needs X11/Wayland client libs (present on any desktop install).
  Window capture needs X11 — Wayland blocks screenshots by design.
- **macOS**: unsigned build — right-click → Open on first launch, and grant
  Screen Recording permission when asked.

## Quickstart (from source)

```powershell
cargo build --release -p qcapture-cli
.\target\release\qcapture.exe --list-screens
.\target\release\qcapture.exe --probe-ffmpeg

# Record primary monitor (native H.264 + system audio)
.\target\release\qcapture.exe record --screen 0 --duration 10 out.mp4

# Record with hardware encoding + live drawing window
.\target\release\qcapture.exe record --encoder nvenc --draw

# Region workflow: select, annotate, burn in
.\target\release\qcapture.exe pick-region
.\target\release\qcapture.exe annotate --save demo.qcap.json
.\target\release\qcapture.exe record --encoder nvenc --region 100,100,1280,720 --annotate demo.qcap.json

# Window + cursor effects (auto-switches to ffmpeg)
.\target\release\qcapture.exe record --window-title "Notepad" --cursor-highlight --cursor-ripple

# Floating control widget (all functions live here)
.\target\release\qcapture.exe widget
# ...or the console-less launcher (logs to %APPDATA%\QCapture\logs)
.\target\release\qcapture-widget.exe
```

Timed annotations (`--annotate`) burn into native encoders too (at native
size; combine with `--canvas` only via an ffmpeg encoder). Live drawing
and cursor fx still need CPU pixels, so native encoders auto-switch to
ffmpeg when those are requested; the switch is logged.

## CLI reference

| Command | Purpose |
|---|---|
| `record` | Record screen / region / window to MP4 |
| `pick-region` | Fullscreen drag-select; prints `x,y,w,h` (or JSON) |
| `annotate` | Select a region, draw on it, save `.qcap.json` |
| `annotate-demo` | Generate a demo annotation doc |
| `widget` | Floating control widget |

Key `record` flags: `--screen`, `--region x,y,w,h`, `--region-screen`,
`--window-title`, `--fps`, `--canvas WxH`, `--encoder auto|h264|hevc|nvenc|amf|qsv|x264`,
`--bitrate`, `--rc cbr|vbr|cqp|crf`, `--qp`, `--crf`, `--maxrate`, `--output`,
`--duration`, `--no-cursor`, `--no-audio`, `--mic`, `--system-gain/--mic-gain`,
`--system-mute/--mic-mute`, `--annotate`, `--draw`, `--draw-test`,
`--cursor-highlight`, `--cursor-ripple`, `--pause-at`, `--resume-at`.

Query flags: `--list-screens`, `--list-windows`, `--list-audio`,
`--probe-ffmpeg`, `--canvas-presets` (each with `--json`).

## Architecture

UI thread / capture thread (WGC on Windows, xcap bridge elsewhere) /
audio thread (WASAPI loopback + mic on Windows, cpal monitor/mic elsewhere) /
compositor+encode pump / mux. Frames travel by handle with `flume` channels;
the pump blends annotations + cursor fx onto BGRA bytes, feeds ffmpeg stdin
(video) and an OS pipe — Windows named pipe, Unix socket — with mixed PCM.

| Crate | Role |
|---|---|
| `qcapture-core` | Shared types: targets, canvas, encoders, rate control, cursor fx |
| `qcapture-capture` | Enumeration + WGC capture + portable xcap bridge + shared pump |
| `qcapture-audio` | WASAPI loopback + portable cpal mixer (DSP shared) |
| `qcapture-encode` | ffmpeg CLI orchestration, rate maps, audio OS pipe |
| `qcapture-annotate` | Annotation doc + CPU rasterizer |
| `qcapture-ui` | Region picker, annotate editor, draw panel, widget |
| `qcapture-cli` | `qcapture` binary |

## Development

```powershell
cargo test --workspace
cargo clippy --workspace -- --deny warnings
cargo fmt --all -- --check
```

## License

MIT OR Apache-2.0.
