//! GStreamer video pipeline driven by an in-process PipeWire consumer.
//!
//! The recording pipeline starts with `appsrc name=src` rather than
//! `pipewiresrc`. Our own [`crate::capture::pipewire_capture`] module
//! consumes the screencast portal's PipeWire node, negotiates a fixed
//! video format on the consumer side, and forwards each buffer's bytes
//! through a tokio mpsc receiver; this module wraps those bytes in a
//! gst `Buffer` and pushes them into the appsrc. The previous flow used
//! gst-plugin-pipewire's `pipewiresrc`, which trips the
//! `gst_caps_is_fixed (pwsrc->caps)` assertion against cosmic-comp's
//! screencast on older plugin builds and wedges the pipeline before any
//! frames flow.

use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use gstreamer::prelude::*;
use gstreamer::{Buffer, ClockTime, ElementFactory, MessageView, PadProbeReturn, PadProbeType, Pipeline, State};
use gstreamer_app::AppSrc;

use crate::capture::pipewire_capture::{Frame, StreamFormat, gst_format_name};
use crate::cli::{VideoContainer, VideoEncoder};

/// Erased drop-handle for whichever capture source is feeding frames into
/// this session. Held only for lifetime; dropping it tears down the
/// source. Two concrete producers today:
///   * `pipewire_capture::Capture` — screencast portal + pipewire stream
///   * `toplevel_capture::Capture` — cosmic-screencopy on a toplevel
pub type CaptureHandle = Box<dyn std::any::Any + Send>;

#[derive(Clone, Copy, Debug)]
pub struct CropRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl CropRect {
    /// Compute videocrop `(top, left, right, bottom)` against a source of
    /// `(src_w, src_h)`. Ensures the resulting output width and height are
    /// both even — H.264 with 4:2:0 chroma needs even dimensions or the
    /// stream is malformed even though the encoder may silently produce
    /// bytes for it.
    fn to_crop_props(self, src_w: u32, src_h: u32) -> (u32, u32, u32, u32) {
        let left = self.x.clamp(0, src_w as i32) as u32;
        let top = self.y.clamp(0, src_h as i32) as u32;
        let mut right = src_w.saturating_sub(left + self.w);
        let mut bottom = src_h.saturating_sub(top + self.h);

        let out_w = src_w.saturating_sub(left + right);
        let out_h = src_h.saturating_sub(top + bottom);
        if out_w % 2 == 1 && right < src_w.saturating_sub(left + 1) {
            right += 1;
        }
        if out_h % 2 == 1 && bottom < src_h.saturating_sub(top + 1) {
            bottom += 1;
        }
        (top, left, right, bottom)
    }
}

pub struct VideoSession {
    pipeline: Pipeline,
    appsrc: AppSrc,
    frames_at_crop: Arc<AtomicU64>,
    bytes_at_sink: Arc<AtomicU64>,
    /// Hold the frame-source alive for the duration of recording —
    /// drop on session teardown stops whichever capture thread (pipewire
    /// or toplevel-screencopy) is producing frames.
    _capture: CaptureHandle,
}

impl VideoSession {
    pub fn build(
        capture: CaptureHandle,
        format: StreamFormat,
        output: &Path,
        fps: u32,
        container: VideoContainer,
        encoder: VideoEncoder,
        audio_mic: bool,
        audio_system: bool,
        crop: Option<CropRect>,
    ) -> Result<Self> {
        gstreamer::init().context("gstreamer init")?;

        // Container drives codec choice: H.264 for mp4/mkv, VP9/VP8 for webm.
        // webmmux accepts vp8/vp9 streams directly, so the parser stage is
        // skipped for WebM (h264parse / aacparse only apply to AVC).
        let (enc, parser_chain, muxer) = match container {
            VideoContainer::Mp4 => (
                encoder.resolve(),
                "h264parse !".to_string(),
                "mp4mux faststart=true",
            ),
            VideoContainer::Mkv => (
                encoder.resolve(),
                "h264parse !".to_string(),
                "matroskamux",
            ),
            VideoContainer::WebM => (resolve_webm_encoder(), String::new(), "webmmux"),
        };

        // Crop math uses the negotiated source dimensions now that we know
        // them up front (vs. the old pipewiresrc path where we sometimes
        // didn't have a size from the portal).
        let crop_str = match crop {
            Some(c) => {
                let (t, l, r, b) = c.to_crop_props(format.width, format.height);
                let out_w = format.width.saturating_sub(l + r);
                let out_h = format.height.saturating_sub(t + b);
                if out_w == 0 || out_h == 0 {
                    tracing::warn!(
                        stream_w = format.width, stream_h = format.height, ?c,
                        "crop rect outside stream bounds; recording full source"
                    );
                    String::new()
                } else {
                    let nudged = out_w as i32 != c.w as i32 || out_h as i32 != c.h as i32;
                    if nudged {
                        tracing::info!(
                            requested_w = c.w, requested_h = c.h, out_w, out_h,
                            "rounded crop dimensions to even for H.264"
                        );
                    }
                    tracing::info!(
                        stream_w = format.width, stream_h = format.height,
                        crop_top = t, crop_left = l, crop_right = r, crop_bottom = b,
                        out_w, out_h,
                        "videocrop applied"
                    );
                    format!("! videocrop name=crop top={t} left={l} right={r} bottom={b} ")
                }
            }
            None => String::new(),
        };

        let location = escape_for_gst(output.to_string_lossy().as_ref());
        let audio_branch = crate::encode::audio::audio_branch_str(audio_mic, audio_system);
        let gst_fmt = gst_format_name(format.format);
        // `appsrc name=src` is fed from `pump_frames` below. is-live=true
        // makes the source clock to wall-clock; format=time so we can stamp
        // PTS on buffers (or omit them and let do-timestamp do its job).
        // Caps are NOT set inline here — gst-launch's quote handling for
        // caps with commas (`format=`, `width=`, `framerate=`) is brittle
        // across versions and gst-launch will silently mis-bind comma-
        // separated pairs as element properties (e.g. `format=RGBA`
        // clobbers appsrc's own `format` property). We let parse_launch
        // construct the element shape with no caps, then `set_caps()` it
        // via the gstreamer-app API below — same effect, no parsing.
        let pipeline_str = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true \
             ! queue max-size-buffers=4 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! videorate drop-only=true max-rate={fps} \
             ! video/x-raw,framerate={fps}/1 \
             {crop_str}\
             ! videoconvert \
             ! queue max-size-buffers=4 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! {enc} \
             ! {parser_chain} \
             {muxer} name=mux \
             ! filesink name=sink location={location} {audio}",
            audio = audio_branch,
        );
        tracing::info!(%pipeline_str, "constructing gst pipeline");

        let element = gstreamer::parse::launch(&pipeline_str).context("gst parse_launch")?;
        let pipeline = element
            .downcast::<Pipeline>()
            .map_err(|_| anyhow::anyhow!("parse_launch did not return a Pipeline"))?;

        let appsrc = pipeline
            .by_name("src")
            .ok_or_else(|| anyhow::anyhow!("appsrc 'src' missing from pipeline"))?
            .dynamic_cast::<AppSrc>()
            .map_err(|_| anyhow::anyhow!("'src' element is not an AppSrc"))?;

        // Set caps via the API too — defense in depth against gst-launch's
        // quoting quirks. Builds the same caps string but as a structured
        // GstCaps so there's no parsing involved.
        let caps_str = format!(
            "video/x-raw,format={gst_fmt},width={src_w},height={src_h},framerate={src_fps}/1",
            src_w = format.width,
            src_h = format.height,
            src_fps = format.fps.max(1),
        );
        let caps = gstreamer::Caps::from_str(&caps_str)
            .with_context(|| format!("parse appsrc caps {caps_str:?}"))?;
        appsrc.set_caps(Some(&caps));
        tracing::info!(%caps_str, "appsrc caps set via API");

        if matches!(encoder, VideoEncoder::Auto)
            && ElementFactory::find("vaapih264enc").is_none()
            && ElementFactory::find("nvh264enc").is_none()
            && ElementFactory::find("x264enc").is_none()
        {
            anyhow::bail!(
                "no H.264 encoder available — install gstreamer plugins (vaapi/nvenc/x264)"
            );
        }

        let frames_at_crop = Arc::new(AtomicU64::new(0));
        let bytes_at_sink = Arc::new(AtomicU64::new(0));

        // Pad probe on the crop element's src pad — counts frames AFTER the
        // crop (or just after videorate if no crop is in use).
        let probe_anchor = pipeline.by_name("crop").or_else(|| pipeline.by_name("sink"));
        if let Some(probe_el) = probe_anchor {
            if let Some(pad) = probe_el.static_pad("src").or_else(|| probe_el.static_pad("sink")) {
                let counter = frames_at_crop.clone();
                pad.add_probe(PadProbeType::BUFFER, move |_pad, _info| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    PadProbeReturn::Ok
                });
            }
        }
        // Bytes at filesink: probe its sink pad.
        if let Some(sink) = pipeline.by_name("sink") {
            if let Some(pad) = sink.static_pad("sink") {
                let counter = bytes_at_sink.clone();
                pad.add_probe(PadProbeType::BUFFER, move |_pad, info| {
                    if let Some(gstreamer::PadProbeData::Buffer(buf)) = info.data.as_ref() {
                        counter.fetch_add(buf.size() as u64, Ordering::Relaxed);
                    }
                    PadProbeReturn::Ok
                });
            }
        }

        Ok(Self {
            pipeline,
            appsrc,
            frames_at_crop,
            bytes_at_sink,
            _capture: capture,
        })
    }

    pub async fn run(
        self,
        stop: tokio::sync::oneshot::Receiver<()>,
        mut frame_rx: tokio::sync::mpsc::Receiver<Frame>,
    ) -> Result<()> {
        // Frame pump: forward pipewire-side buffers into the gst appsrc.
        // Runs on a tokio task that yields between buffers so it doesn't
        // starve the rest of the runtime. Stops automatically when frame_rx
        // closes (pipewire thread dropped or signalled stop).
        let appsrc = self.appsrc.clone();
        let _pump_handle = tokio::spawn(async move {
            let start = Instant::now();
            while let Some(frame) = frame_rx.recv().await {
                // appsrc with do-timestamp=true will fill in PTS for us,
                // so we don't need to override the buffer's timestamp here.
                // We just hand it raw bytes.
                let mut buf = Buffer::with_size(frame.bytes.len()).expect("alloc gst buffer");
                {
                    let buf_mut = buf.get_mut().expect("fresh buffer is unique");
                    let mut map = buf_mut.map_writable().expect("map writable");
                    map.copy_from_slice(&frame.bytes);
                    drop(map);
                    // Set a coarse PTS based on wall-clock from pump start,
                    // so a downstream that ignores do-timestamp still has
                    // monotonic timing to work with.
                    let elapsed = start.elapsed().as_nanos() as u64;
                    buf_mut.set_pts(ClockTime::from_nseconds(elapsed));
                }
                if let Err(e) = appsrc.push_buffer(buf) {
                    tracing::warn!(error = %e, "appsrc push_buffer failed; stopping pump");
                    break;
                }
            }
            // Signal end-of-stream to gst when the pipewire side has shut.
            let _ = appsrc.end_of_stream();
            tracing::info!("frame pump exited");
        });

        tracing::info!("pipeline: → Playing (calling set_state)");
        tracing::info!("pipeline: → Playing (calling set_state)");
        // gst's set_state is documented as non-blocking, but on systems with
        // gst-plugin-pipewire builds that still hit the
        // gst_caps_is_fixed (pwsrc->caps) assertion, the NULL→PLAYING
        // transition can synchronously wedge inside the pwsrc element. That
        // parks the tokio worker thread, so even when the user clicks Stop
        // and stop_tx.send() succeeds, our tokio::select can never poll the
        // receiver — the future is blocked, not pending.
        //
        // Run set_state on a blocking thread with a hard timeout so a stuck
        // pipewiresrc can't take the recording flow with it. The blocking
        // thread leaks until process exit if the call never returns, but
        // that's fine: when this errors we propagate up and shut down.
        let pipeline_for_start = self.pipeline.clone();
        let state_call = tokio::task::spawn_blocking(move || {
            pipeline_for_start.set_state(State::Playing)
        });
        let state_result = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            state_call,
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                anyhow::bail!("set_state(Playing) task panicked: {e}");
            }
            Err(_) => {
                tracing::error!(
                    "set_state(Playing) did not return within 3s — pipewiresrc \
                     is wedged (likely the gst_caps_is_fixed assertion). \
                     Aborting recording so the UI can recover."
                );
                anyhow::bail!(
                    "pipeline failed to start within 3s (pipewiresrc wedged \
                     on caps negotiation)"
                );
            }
        };
        tracing::info!(?state_result, "pipeline: set_state(Playing) returned");
        state_result.context("pipeline → Playing")?;
        let bus = self.pipeline.bus().context("pipeline bus")?;
        tracing::info!("pipeline: bus acquired, entering select loop");

        let (bus_tx, mut bus_rx) = tokio::sync::mpsc::unbounded_channel::<Result<()>>();
        let bus_thread = std::thread::spawn(move || {
            for msg in bus.iter_timed(gstreamer::ClockTime::NONE) {
                match msg.view() {
                    MessageView::Eos(_) => {
                        tracing::info!("bus: EOS");
                        let _ = bus_tx.send(Ok(()));
                        break;
                    }
                    MessageView::Error(e) => {
                        let _ = bus_tx.send(Err(anyhow::anyhow!(
                            "{}: {}",
                            e.error(),
                            e.debug().unwrap_or_default()
                        )));
                        break;
                    }
                    MessageView::Warning(w) => {
                        tracing::warn!(error = %w.error(), debug = ?w.debug(), "bus warning");
                    }
                    MessageView::StateChanged(sc) => {
                        if sc.src().map_or(false, |s| s.name() == "pipeline0"
                            || s.type_().name() == "GstPipeline")
                        {
                            tracing::debug!(
                                from = ?sc.old(),
                                to = ?sc.current(),
                                "pipeline state changed"
                            );
                        }
                    }
                    _ => {}
                }
            }
        });

        let result = tokio::select! {
            r = bus_rx.recv() => {
                tracing::info!(received = r.is_some(),
                    "bus_rx branch fired — pipeline ended on its own");
                r.unwrap_or(Ok(()))
            }
            _ = stop => {
                tracing::info!("stop signal received; sending EOS");
                self.pipeline.send_event(gstreamer::event::Eos::new());
                // Bound the wait: if the pipeline is wedged (e.g. no frames
                // ever flowed because the portal handed us a dead stream),
                // sending EOS won't ever produce a bus EOS/Error message,
                // and a bare recv would hang the user's "Stop" forever.
                // Three seconds is plenty for a well-behaved mux finalize
                // and short enough that a hang is recoverable.
                let drain = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    bus_rx.recv(),
                ).await;
                match drain {
                    Ok(Some(r)) => r,
                    Ok(None) => Ok(()),
                    Err(_) => {
                        let frames = self.frames_at_crop.load(Ordering::Relaxed);
                        let bytes = self.bytes_at_sink.load(Ordering::Relaxed);
                        tracing::warn!(
                            frames, bytes,
                            "pipeline did not respond to EOS within 3s — forcing teardown. \
                             frames=0 means pipewiresrc never produced output (likely the \
                             pwsrc caps assertion); frames>0 means the muxer is stuck \
                             finalizing."
                        );
                        Ok(())
                    }
                }
            }
        };

        let frames = self.frames_at_crop.load(Ordering::Relaxed);
        let bytes = self.bytes_at_sink.load(Ordering::Relaxed);
        tracing::info!(frames, bytes, "pipeline finished");
        if frames == 0 {
            tracing::error!(
                "no frames flowed through the pipeline. \
                 Check: (1) is pipewiresrc receiving from the portal? \
                 (2) does the videocrop region fit the source? \
                 (3) is the encoder rejecting the input caps?"
            );
        } else if bytes == 0 {
            tracing::error!(
                "frames flowed but no bytes reached filesink — encoder/muxer is dropping output"
            );
        }

        // Drop to Null. Same blocking-call concern as the Playing transition:
        // a wedged element can hang the call indefinitely. Bounded at 2s so
        // a broken pipeline can't trap process shutdown either.
        let pipeline_for_stop = self.pipeline.clone();
        let null_call = tokio::task::spawn_blocking(move || {
            pipeline_for_stop.set_state(State::Null)
        });
        match tokio::time::timeout(std::time::Duration::from_secs(2), null_call).await {
            Ok(Ok(r)) => {
                r.context("pipeline → Null")?;
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "set_state(Null) task panicked"),
            Err(_) => tracing::warn!(
                "set_state(Null) did not return within 2s — leaking pipeline"
            ),
        }
        let _ = bus_thread.join();
        result
    }
}

pub fn build_crop_str(crop: Option<CropRect>, source_size: Option<(u32, u32)>) -> String {
    match (crop, source_size) {
        (Some(c), Some((w, h))) => {
            let (t, l, r, b) = c.to_crop_props(w, h);
            let out_w = w.saturating_sub(l + r);
            let out_h = h.saturating_sub(t + b);
            if out_w == 0 || out_h == 0 {
                tracing::warn!(
                    stream_w = w, stream_h = h, ?c,
                    "crop rect outside stream bounds; recording full source"
                );
                String::new()
            } else {
                format!("! videocrop top={t} left={l} right={r} bottom={b} ")
            }
        }
        (Some(c), None) => format!(
            "! videocrop top={t} left={l} right=0 bottom=0 \
             ! video/x-raw,width={w},height={h} ",
            t = c.y.max(0),
            l = c.x.max(0),
            w = c.w,
            h = c.h,
        ),
        (None, _) => String::new(),
    }
}

/// Pick a webm-compatible encoder: hardware VAAPI when present, otherwise
/// libvpx in realtime mode. vp8enc is preferred over vp9enc on CPU because
/// vp9enc is essentially unusable for live screen capture.
fn resolve_webm_encoder() -> String {
    let has = |name: &str| ElementFactory::find(name).is_some();
    if has("vavp9enc") {
        return "vavp9enc".into();
    }
    if has("vavp9lpenc") {
        return "vavp9lpenc".into();
    }
    if has("vavp8enc") {
        return "vavp8enc".into();
    }
    if has("vp8enc") {
        // deadline=1 (1µs budget) puts libvpx in realtime mode; cpu-used
        // trades quality for speed (0=best/slowest, 16=fastest/lowest).
        // 4 keeps frames sub-100ms on typical desktop CPUs.
        tracing::warn!(
            "no hardware VP encoder; falling back to libvpx vp8enc realtime — \
             expect heavy CPU during recording"
        );
        return "vp8enc deadline=1 cpu-used=4 threads=4 target-bitrate=6000000".into();
    }
    if has("vp9enc") {
        tracing::warn!(
            "only libvpx vp9enc available; encoder is too slow for realtime \
             screen capture and the recording will drop frames"
        );
        return "vp9enc deadline=1 cpu-used=8 threads=8 target-bitrate=6000000".into();
    }
    // Last-ditch — produce a string that will fail loudly in parse_launch
    // with a discoverable element name rather than silently passing.
    "vp8enc".into()
}

fn escape_for_gst(s: &str) -> String {
    let escaped: String = s
        .chars()
        .flat_map(|c| match c {
            '\\' => vec!['\\', '\\'].into_iter(),
            '"' => vec!['\\', '"'].into_iter(),
            other => vec![other].into_iter(),
        })
        .collect();
    format!("\"{escaped}\"")
}
