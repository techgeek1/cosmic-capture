//! CLI surface.
//!
//! The portal owns interactive source/region picking and cursor handling, so
//! we deliberately keep the CLI minimal. Once we ship a libcosmic-based
//! selector overlay we'll add `--region` + `--output` back as bypass paths.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "cosmic-capture", version, about, long_about = None)]
pub struct Cli {
    /// No subcommand → launch the GUI panel.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Capture a still image via xdg-desktop-portal Screenshot.
    Screenshot(ScreenshotArgs),

    /// Record a video clip via xdg-desktop-portal ScreenCast + GStreamer.
    Record(RecordArgs),

    /// Capture a GIF via xdg-desktop-portal ScreenCast + gifski.
    Gif(GifArgs),
}

#[derive(Debug, Args, Clone)]
pub struct CommonArgs {
    /// Destination file. If omitted, writes a timestamped name into
    /// `$XDG_PICTURES_DIR` (or `$XDG_VIDEOS_DIR` for `record`).
    #[arg(long, short = 'f')]
    pub file: Option<PathBuf>,

    /// Send a desktop notification when capture completes.
    #[arg(long, default_value_t = true,
          num_args(0..=1), require_equals = true, default_missing_value = "true")]
    pub notify: bool,

    /// Copy the resulting file path to the clipboard when done.
    #[arg(long, default_value_t = false)]
    pub clipboard: bool,
}

#[derive(Debug, Args)]
pub struct ScreenshotArgs {
    #[command(flatten)]
    pub common: CommonArgs,

    /// Interactive portal mode: user picks the region/output via the
    /// compositor's overlay. Disable for unattended scripts (whole screen).
    #[arg(long, default_value_t = true,
          num_args(0..=1), require_equals = true, default_missing_value = "true")]
    pub interactive: bool,

    /// Modal portal dialog (request keyboard/cursor focus).
    #[arg(long, default_value_t = true,
          num_args(0..=1), require_equals = true, default_missing_value = "true")]
    pub modal: bool,

    /// Delay before capture, in milliseconds.
    #[arg(long, default_value_t = 0)]
    pub delay_ms: u64,
}

#[derive(Debug, Args)]
pub struct RecordArgs {
    #[command(flatten)]
    pub common: CommonArgs,

    /// Target framerate.
    #[arg(long, default_value_t = 60)]
    pub fps: u32,

    /// Encoder. `auto` probes VAAPI → NVENC → x264.
    #[arg(long, value_enum, default_value_t = VideoEncoder::Auto)]
    pub encoder: VideoEncoder,

    /// Container format.
    #[arg(long, value_enum, default_value_t = VideoContainer::Mp4)]
    pub container: VideoContainer,

    /// Capture system audio (default sink monitor).
    #[arg(long, default_value_t = false)]
    pub audio: bool,

    /// Include the cursor.
    #[arg(long, default_value_t = true,
          num_args(0..=1), require_equals = true, default_missing_value = "true")]
    pub cursor: bool,

    /// Skip the region selector and record the full source the portal returns.
    #[arg(long, default_value_t = false)]
    pub full: bool,

    /// Stop after N seconds. Omit to record until SIGINT.
    #[arg(long)]
    pub duration_secs: Option<u64>,
}

#[derive(Debug, Args)]
pub struct GifArgs {
    #[command(flatten)]
    pub common: CommonArgs,

    /// Capture + playback framerate.
    #[arg(long, default_value_t = 20)]
    pub fps: u32,

    /// Gifski quality (1-100).
    #[arg(long, default_value_t = 90)]
    pub quality: u8,

    /// Max output width; the stream is scaled down to fit. 0 = no scale.
    #[arg(long, default_value_t = 800)]
    pub max_width: u32,

    /// Include the cursor.
    #[arg(long, default_value_t = true,
          num_args(0..=1), require_equals = true, default_missing_value = "true")]
    pub cursor: bool,

    /// Skip the region selector and capture the full source.
    #[arg(long, default_value_t = false)]
    pub full: bool,

    /// Stop after N seconds.
    #[arg(long, default_value_t = 5)]
    pub duration_secs: u64,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum VideoEncoder {
    /// Probe for VAAPI, then NVENC, then x264.
    Auto,
    X264,
    Vaapi,
    Nvenc,
}

impl VideoEncoder {
    /// Best-effort encoder pick. Walks a candidate list of modern → legacy
    /// hardware encoders, falling back to CPU x264 only if nothing else is
    /// present. Returns the gst element string (factory + properties).
    pub fn resolve(self) -> String {
        use gstreamer::ElementFactory;
        let has = |name: &str| ElementFactory::find(name).is_some();

        const X264_CPU: &str =
            "x264enc tune=zerolatency speed-preset=ultrafast bitrate=8000";

        match self {
            VideoEncoder::Auto => {
                // Modern GStreamer 1.20+ VA-API (gst-plugin-bad's `va` plugin);
                // replaces the deprecated gst-plugin-vaapi.
                if has("vah264enc") {
                    return "vah264enc".into();
                }
                if has("vah264lpenc") {
                    return "vah264lpenc".into();
                }
                // NVIDIA CUDA-based encoder (newer) → classic NVENC.
                if has("nvcudah264enc") {
                    return "nvcudah264enc".into();
                }
                if has("nvh264enc") {
                    return "nvh264enc".into();
                }
                // Legacy VA-API (gst-plugin-vaapi).
                if has("vaapih264enc") {
                    return "vaapih264enc".into();
                }
                tracing::warn!(
                    "no hardware H.264 encoder found; falling back to x264 CPU encode. \
                     Install gst-plugin-bad's `va` plugin (Intel/AMD) or `nvcodec` (NVIDIA) \
                     for real-time recording."
                );
                X264_CPU.into()
            }
            VideoEncoder::X264 => X264_CPU.into(),
            VideoEncoder::Vaapi => {
                if has("vah264enc") { "vah264enc".into() } else { "vaapih264enc".into() }
            }
            VideoEncoder::Nvenc => {
                if has("nvcudah264enc") { "nvcudah264enc".into() } else { "nvh264enc".into() }
            }
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum VideoContainer {
    Mp4,
    Mkv,
    WebM,
}

impl VideoContainer {
    pub fn extension(self) -> &'static str {
        match self {
            VideoContainer::Mp4 => "mp4",
            VideoContainer::Mkv => "mkv",
            VideoContainer::WebM => "webm",
        }
    }

    /// gst muxer factory + the parser between encoder and muxer.
    pub fn mux_chain(self) -> (&'static str, &'static str) {
        match self {
            VideoContainer::Mp4 => ("h264parse", "mp4mux faststart=true"),
            VideoContainer::Mkv => ("h264parse", "matroskamux"),
            VideoContainer::WebM => ("vp8enc-equivalent", "webmmux"),
        }
    }
}
