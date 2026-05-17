//! GStreamer video pipeline driven by a ScreenCast PipeWire stream.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use gstreamer::prelude::*;
use gstreamer::{ElementFactory, MessageView, PadProbeReturn, PadProbeType, Pipeline, State};

use crate::capture::screencast::PipeWireStream;
use crate::cli::{VideoContainer, VideoEncoder};

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
    frames_at_crop: Arc<AtomicU64>,
    bytes_at_sink: Arc<AtomicU64>,
    _stream: PipeWireStream,
}

impl VideoSession {
    pub fn build(
        stream: PipeWireStream,
        output: &Path,
        fps: u32,
        container: VideoContainer,
        encoder: VideoEncoder,
        audio: bool,
        crop: Option<CropRect>,
    ) -> Result<Self> {
        gstreamer::init().context("gstreamer init")?;

        let enc = encoder.resolve();
        let (parser, muxer) = match container {
            VideoContainer::Mp4 => ("h264parse", "mp4mux faststart=true"),
            VideoContainer::Mkv => ("h264parse", "matroskamux"),
            VideoContainer::WebM => {
                anyhow::bail!("webm container needs vp8/vp9 encode; not wired yet")
            }
        };

        let crop_str = match (crop, stream.size) {
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
                    let nudged = out_w as i32 != c.w as i32 || out_h as i32 != c.h as i32;
                    if nudged {
                        tracing::info!(
                            requested_w = c.w, requested_h = c.h, out_w, out_h,
                            "rounded crop dimensions to even for H.264"
                        );
                    }
                    tracing::info!(
                        stream_w = w, stream_h = h,
                        crop_top = t, crop_left = l, crop_right = r, crop_bottom = b,
                        out_w, out_h,
                        "videocrop applied"
                    );
                    format!("! videocrop name=crop top={t} left={l} right={r} bottom={b} ")
                }
            }
            (Some(c), None) => {
                tracing::warn!(
                    ?c,
                    "no source size from portal; cropping with right=bottom=0 + caps clamp"
                );
                // Force even dims at the caps filter.
                let w = c.w & !1;
                let h = c.h & !1;
                format!(
                    "! videocrop name=crop top={t} left={l} right=0 bottom=0 \
                     ! video/x-raw,width={w},height={h} ",
                    t = c.y.max(0),
                    l = c.x.max(0),
                )
            }
            (None, _) => String::new(),
        };

        let location = escape_for_gst(output.to_string_lossy().as_ref());
        let audio_branch = if audio {
            "pulsesrc ! audioconvert ! audioresample ! avenc_aac ! mux.audio_0"
        } else {
            ""
        };
        // Pipeline shape, intentional details:
        //  * `queue leaky=downstream` before the encoder — if the encoder
        //    can't keep up we drop frames in real-time rather than building
        //    a 30s post-stop backlog.
        //  * `videorate drop-only=true max-rate={fps}` — without these,
        //    videorate doesn't actually cap above-target rates (it only
        //    duplicates upward), so a 240Hz pipewiresrc deluges the
        //    encoder.
        //  * `do-timestamp=true` on pipewiresrc uses our local clock so
        //    videorate has monotonic timestamps to work with.
        //  * Capsfilter `video/x-raw` right after pipewiresrc constrains
        //    negotiation to raw video. Without it, cosmic-comp's screencast
        //    advertises a parameter set that includes alternatives and
        //    older gst-plugin-pipewire hits
        //    `handle_format_change: assertion 'gst_caps_is_fixed (pwsrc->caps)'`
        //    because the handler expects fixed caps. The downstream
        //    capsfilter forces upstream negotiation to converge on a single
        //    raw-video format before the assertion runs.
        let pipeline_str = format!(
            "pipewiresrc fd={fd} path={node} do-timestamp=true keepalive-time=1000 \
             ! video/x-raw \
             ! queue max-size-buffers=4 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! videorate drop-only=true max-rate={fps} \
             ! video/x-raw,framerate={fps}/1 \
             {crop_str}\
             ! videoconvert \
             ! queue max-size-buffers=4 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! {enc} \
             ! {parser} \
             ! {muxer} name=mux \
             ! filesink name=sink location={location} {audio}",
            fd = stream.raw_fd(),
            node = stream.node_id,
            audio = audio_branch,
        );
        tracing::info!(%pipeline_str, "constructing gst pipeline");

        let element = gstreamer::parse::launch(&pipeline_str).context("gst parse_launch")?;
        let pipeline = element
            .downcast::<Pipeline>()
            .map_err(|_| anyhow::anyhow!("parse_launch did not return a Pipeline"))?;

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

        Ok(Self { pipeline, frames_at_crop, bytes_at_sink, _stream: stream })
    }

    pub async fn run(self, stop: tokio::sync::oneshot::Receiver<()>) -> Result<()> {
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
