# cosmic-capture

Screenshot, video, and GIF capture for the [COSMIC desktop][cosmic], with a
unified GUI panel and a scriptable CLI. Built around `cosmic-screencopy` and
the `xdg-desktop-portal` ScreenCast portal, with a zero-copy
dmabuf → VA-API → H.264 path for recording on AMD/Intel GPUs.

[cosmic]: https://github.com/pop-os/cosmic-epoch

## Status

Pre-1.0. Built and tested against COSMIC alpha on CachyOS / Arch with an
RDNA3 AMD GPU. Other distros should work as long as the runtime dependencies
below are present; other GPUs will fall back as VA-API plugin support allows.

## What it does

- **Screenshot** of a region, single window, or full display. Saves to file
  (PNG), the clipboard, or both. Region selection spans multiple displays.
- **Record** to MP4, MKV, or WebM via VA-API (H.264). Region recording
  composites cross-screen captures on the GPU through `vacompositor` —
  no CPU touch on pixels between capture and encode.
- **GIF** recording via `gifski`, with the same source picker as video.
- **Audio** for recordings: independent mic and system-monitor toggles.
  Mixed through `audiomixer` when both are on.
- **Toolbar** is a per-output layer-shell pill: mode, source, primary
  action, format/fps options, audio toggles, close. Same surface freezes
  the display for screenshot framing and stays transparent for live
  framing in record mode.
- **Window picker** with live thumbnails for screenshot and recording.

## Requirements

- COSMIC desktop environment (uses `cosmic-screencopy`,
  `cosmic-bg-config`, and libcosmic).
- `xdg-desktop-portal-cosmic` for the screencast portal handoff (CLI
  recording path).
- GStreamer 1.24+ with:
  - `gst-plugins-base`, `gst-plugins-good`, `gst-plugins-bad`,
    `gst-plugins-ugly`, `gst-libav`
  - the `va` plugin from `gst-plugins-bad` (provides `vapostproc`,
    `vah264enc`, `vacompositor`)
- PipeWire with PulseAudio compat (`pipewire-pulse`) for `pulsesrc` /
  `@DEFAULT_MONITOR@`.
- A GPU with VA-API H.264 encode support. Tested on AMD RDNA3; Intel and
  older AMD should work as long as `vah264enc` initializes.
- Rust 1.80+ to build.

### Arch / CachyOS

```sh
sudo pacman -S --needed \
    gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad \
    gst-plugins-ugly gst-libav pipewire-pulse libgbm \
    xdg-desktop-portal-cosmic
```

## Install

```sh
./install.sh
```

Installs the release binary to `~/.local/bin/cosmic-capture` and a
`.desktop` entry to `~/.local/share/applications/`. Pass `--prefix
/usr/local` (with `sudo`) for a system-wide install.

The script does not install the runtime dependencies above — install
those with your distro's package manager first.

## Usage

### GUI

```sh
cosmic-capture
```

Launches the layer-shell toolbar on every output. Pick mode (screenshot
or record), source (region, window, or display), and hit the primary
action. In record mode, the toolbar gains audio toggles (mic /
system-monitor) and an FPS dropdown.

### CLI

```sh
# Screenshot a region you select interactively, copy to clipboard.
cosmic-capture screenshot --clipboard

# Record the full output the screencast portal picks, mic on,
# system audio on, 60 fps, MKV.
cosmic-capture record --mic --system-audio --fps 60 --container mkv

# Record a 10-second GIF.
cosmic-capture gif --duration-secs 10
```

`cosmic-capture --help` lists every subcommand and flag.

## Architecture (brief)

- **Capture** goes through `cosmic-screencopy` with `linux-dmabuf-v1`:
  a GBM buffer object's fd is handed to cosmic-comp, then forwarded
  downstream as a `memory:DMABuf` `GstBuffer`.
- **Encode** uses `vapostproc` for crop/colorspace/scale on the VA
  surface, `vah264enc` for compression, `mp4mux`/`matroskamux`/`webmmux`
  for containerization. Cross-screen region recording adds
  `vacompositor` to merge per-output crops into a single canvas, still
  on the GPU.
- **The CLI's `record` subcommand** uses the `xdg-desktop-portal`
  ScreenCast portal for source selection, then pumps frames through our
  own pipewire-rs consumer (sidestepping a known
  `gst-plugin-pipewire` assertion against cosmic-comp's screencast).
- **The GUI's record path** skips the portal entirely and goes straight
  to `cosmic-screencopy` so the user's source pick lands on the exact
  monitor or window they clicked, not whatever the portal's
  restore-token resolves to.

## License

[MIT](LICENSE).
