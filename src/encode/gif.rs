//! GIF encoding fed by a GStreamer `appsink` on RGBA samples.
//!
//! Mirrors the video pipeline architecture: instead of `pipewiresrc`, the
//! source is a gst `appsrc` fed by [`crate::capture::pipewire_capture`].
//! That avoids gst-plugin-pipewire's `gst_caps_is_fixed (pwsrc->caps)`
//! assertion under cosmic-comp and gives us a single capture consumer that
//! both video and gif paths share.
//!
//! gifski runs the encode on its own threads; we feed RGBA + PTS through
//! a `Collector`, then a writer thread drains the encoder to disk.

use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use gifski::{Repeat, Settings};
use gstreamer::prelude::*;
use gstreamer::{Buffer, ClockTime, MessageView, Pipeline, State};
use gstreamer_app::{AppSink, AppSrc};
use gstreamer_video::VideoInfo;

use crate::capture::pipewire_capture::{gst_format_name, Frame, StreamFormat};
use crate::encode::video::{build_crop_str, CaptureHandle, CropRect};

pub struct GifSession {
    pipeline: Pipeline,
    appsrc: AppSrc,
    writer_thread: std::thread::JoinHandle<Result<()>>,
    collector: Arc<Mutex<Option<gifski::Collector>>>,
    frame_counter: Arc<AtomicUsize>,
    /// Keeps whichever frame source feeds this session alive for the
    /// recording's duration; drop on session teardown closes it.
    /// `CaptureHandle` is an erased Box so a single GifSession can be fed
    /// from pipewire (display via portal), cosmic-screencopy on an output,
    /// or cosmic-screencopy on a toplevel.
    _capture: CaptureHandle,
}

impl GifSession {
    pub fn build(
        capture: CaptureHandle,
        format: StreamFormat,
        output: &Path,
        fps: u32,
        quality: u8,
        max_width: u32,
        crop: Option<CropRect>,
    ) -> Result<Self> {
        gstreamer::init().context("gstreamer init")?;

        let settings = Settings {
            width: None,
            height: None,
            quality,
            fast: false,
            repeat: Repeat::Infinite,
        };
        let (collector, writer) = gifski::new(settings).context("gifski::new")?;

        let path_for_thread = output.to_owned();
        let writer_thread = std::thread::spawn(move || -> Result<()> {
            let file = std::fs::File::create(&path_for_thread)
                .with_context(|| format!("create {}", path_for_thread.display()))?;
            let mut bw = std::io::BufWriter::new(file);
            let mut progress = gifski::progress::NoProgress {};
            writer
                .write(&mut bw, &mut progress)
                .context("gifski writer")
        });

        let collector = Arc::new(Mutex::new(Some(collector)));

        // appsrc → queue → videorate → optional crop → videoconvert → videoscale
        //        → single terminal capsfilter → appsink.
        //
        // Earlier shape chained two caps filters (`! video/x-raw,width=… !
        // video/x-raw,format=RGBA`) and used the `(int)800` type annotation.
        // That tripped gst-launch's tokenizer into looking up "video" as an
        // element factory ("no element 'video'") because the parenthesised
        // typed value confused the comma-token scanner. Collapsing every
        // negotiation into one terminal caps filter sidesteps it.
        //
        // We compute the output width AND height in Rust so the terminal
        // capsfilter pins both. Specifying width only and letting videoscale
        // pick the height was producing stretched GIFs: with no
        // `pixel-aspect-ratio=1/1` constraint, downstream negotiation could
        // settle on a non-1:1 PAR and keep the source height alongside the
        // shrunken width — gifski then reads the raw `width × height` grid
        // and the PAR information is lost, so the saved GIF displays as the
        // un-corrected pixel grid (stretched horizontally).
        let crop_str = build_crop_str(crop, Some((format.width, format.height)));
        let (src_w, src_h) = match crop {
            Some(c) => {
                let cw = c.w.min(format.width.saturating_sub(c.x.max(0) as u32));
                let ch = c.h.min(format.height.saturating_sub(c.y.max(0) as u32));
                if cw == 0 || ch == 0 {
                    (format.width, format.height)
                } else {
                    (cw, ch)
                }
            }
            None => (format.width, format.height),
        };
        let (out_w, out_h) = if max_width > 0 && src_w > max_width {
            let scale = max_width as f64 / src_w as f64;
            let h = ((src_h as f64) * scale).round() as u32;
            (max_width, h.max(1))
        } else {
            (src_w, src_h)
        };
        let out_caps = format!(
            "video/x-raw,format=RGBA,framerate={fps}/1,\
             width={out_w},height={out_h},pixel-aspect-ratio=1/1",
        );
        let pipeline_str = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true \
             ! queue max-size-buffers=4 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! videorate drop-only=true max-rate={fps} \
             {crop_str}! videoconvert \
             ! videoscale \
             ! {out_caps} \
             ! appsink name=sink emit-signals=true max-buffers=8 drop=false sync=false",
        );
        tracing::info!(%pipeline_str, "constructing gst gif pipeline");

        let element =
            gstreamer::parse::launch(&pipeline_str).context("gst parse_launch (gif)")?;
        let pipeline = element
            .downcast::<Pipeline>()
            .map_err(|_| anyhow::anyhow!("parse_launch did not return a Pipeline"))?;

        let appsrc = pipeline
            .by_name("src")
            .ok_or_else(|| anyhow::anyhow!("appsrc 'src' missing from gif pipeline"))?
            .dynamic_cast::<AppSrc>()
            .map_err(|_| anyhow::anyhow!("'src' element is not an AppSrc"))?;

        // Same defense-in-depth caps setup as video.rs: declare the fixed
        // format we negotiated with the compositor so videorate/videocrop
        // downstream can pick valid filters.
        let gst_fmt = gst_format_name(format.format);
        let caps_str = format!(
            "video/x-raw,format={gst_fmt},width={src_w},height={src_h},framerate={src_fps}/1",
            src_w = format.width,
            src_h = format.height,
            src_fps = format.fps.max(1),
        );
        let caps = gstreamer::Caps::from_str(&caps_str)
            .with_context(|| format!("parse gif appsrc caps {caps_str:?}"))?;
        appsrc.set_caps(Some(&caps));
        tracing::info!(%caps_str, "gif appsrc caps set via API");

        let appsink = pipeline
            .by_name("sink")
            .context("appsink not found in gif pipeline")?
            .downcast::<AppSink>()
            .map_err(|_| anyhow::anyhow!("sink element is not AppSink"))?;

        let start_time = Instant::now();
        let collector_cb = collector.clone();
        let frame_counter = Arc::new(AtomicUsize::new(0));
        let frame_counter_cb = frame_counter.clone();
        appsink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gstreamer::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gstreamer::FlowError::Error)?;
                    let caps = sample.caps().ok_or(gstreamer::FlowError::Error)?;
                    let info = VideoInfo::from_caps(caps).map_err(|_| gstreamer::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gstreamer::FlowError::Error)?;

                    let width = info.width() as usize;
                    let height = info.height() as usize;
                    let stride = info.stride()[0] as usize;
                    let pts_secs = start_time.elapsed().as_secs_f64();

                    let mut pixels: Vec<rgb::RGBA8> = Vec::with_capacity(width * height);
                    let bytes = map.as_slice();
                    for y in 0..height {
                        let row = &bytes[y * stride..y * stride + width * 4];
                        for x in 0..width {
                            let p = &row[x * 4..x * 4 + 4];
                            pixels.push(rgb::RGBA8 { r: p[0], g: p[1], b: p[2], a: p[3] });
                        }
                    }
                    let img = imgref::ImgVec::new(pixels, width, height);

                    let idx = frame_counter_cb.fetch_add(1, Ordering::Relaxed);
                    let guard = collector_cb.lock().unwrap();
                    if let Some(c) = guard.as_ref() {
                        if let Err(e) = c.add_frame_rgba(idx, img, pts_secs) {
                            tracing::error!(error = %e, "gifski add_frame_rgba failed");
                            return Err(gstreamer::FlowError::Error);
                        }
                    }
                    Ok(gstreamer::FlowSuccess::Ok)
                })
                .build(),
        );

        Ok(Self {
            pipeline,
            appsrc,
            writer_thread,
            collector,
            frame_counter,
            _capture: capture,
        })
    }

    pub async fn run(
        self,
        stop: tokio::sync::oneshot::Receiver<()>,
        mut frame_rx: tokio::sync::mpsc::Receiver<Frame>,
    ) -> Result<()> {
        // Frame pump — mirrors VideoSession::run. Yields between buffers so
        // a heavy gifski quantize pass on the appsink thread doesn't starve
        // anything else. Stops when frame_rx closes.
        let appsrc = self.appsrc.clone();
        let _pump_handle = tokio::spawn(async move {
            let start = Instant::now();
            while let Some(frame) = frame_rx.recv().await {
                let mut buf = Buffer::with_size(frame.bytes.len()).expect("alloc gst buffer");
                {
                    let buf_mut = buf.get_mut().expect("fresh buffer is unique");
                    let mut map = buf_mut.map_writable().expect("map writable");
                    map.copy_from_slice(&frame.bytes);
                    drop(map);
                    let elapsed = start.elapsed().as_nanos() as u64;
                    buf_mut.set_pts(ClockTime::from_nseconds(elapsed));
                }
                if let Err(e) = appsrc.push_buffer(buf) {
                    // "Pad is flushing" is the normal race between the pump
                    // task and the EOS we send on Stop — the appsrc has
                    // already been told to flush by the time the next
                    // pipewire frame arrives. Anything else is a real
                    // problem worth surfacing.
                    if matches!(e, gstreamer::FlowError::Flushing) {
                        tracing::debug!("gif pump stopping: appsrc flushed (normal on Stop)");
                    } else {
                        tracing::warn!(error = %e,
                            "gif appsrc push_buffer failed; stopping pump");
                    }
                    break;
                }
            }
            let _ = appsrc.end_of_stream();
            tracing::info!("gif frame pump exited");
        });

        // set_state on a blocking thread w/ timeout, identical to video.rs
        // — pipewiresrc is gone from gif now, but a heavy hardware encoder
        // probe inside gst can still block for a noticeable fraction of a
        // second on first use.
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
            Ok(Err(e)) => anyhow::bail!("gif set_state(Playing) task panicked: {e}"),
            Err(_) => anyhow::bail!("gif pipeline failed to start within 3s"),
        };
        state_result.context("gif pipeline → Playing")?;

        let bus = self.pipeline.bus().context("pipeline bus")?;
        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<Result<()>>();
        // Poll the bus on a short timeout so a `shutdown` flag set by the
        // main task can wake the thread without needing to post a custom
        // message on the bus. iter_timed(NONE) would otherwise block forever
        // if no EOS/Error ever arrives (e.g. after set_state(Null), which
        // doesn't synthesize an EOS), and the bus_thread.join() at the end
        // would hang indefinitely.
        let bus_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let bus_shutdown_thread = bus_shutdown.clone();
        let bus_thread = std::thread::spawn(move || {
            let tick = gstreamer::ClockTime::from_mseconds(100);
            loop {
                if bus_shutdown_thread.load(Ordering::Relaxed) {
                    break;
                }
                let Some(msg) = bus.timed_pop(Some(tick)) else {
                    continue;
                };
                match msg.view() {
                    MessageView::Error(e) => {
                        let _ = msg_tx.send(Err(anyhow::anyhow!(
                            "{}: {}",
                            e.error(),
                            e.debug().unwrap_or_default()
                        )));
                        break;
                    }
                    MessageView::Eos(_) => {
                        let _ = msg_tx.send(Ok(()));
                        break;
                    }
                    _ => {}
                }
            }
        });

        // Stop on either the user-initiated stop signal or a fatal bus error.
        let run_result: Result<()> = tokio::select! {
            msg = msg_rx.recv() => msg.unwrap_or(Ok(())),
            _ = stop => Ok(()),
        };

        // Drive the pipeline to EOS via the appsrc, then wait briefly for
        // the bus to drain so the appsink can flush queued samples into
        // gifski before we yank the pipeline.
        let _ = self.appsrc.end_of_stream();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            msg_rx.recv(),
        )
        .await;
        // Bounded shutdown — Null transition can also block if downstream
        // muxer is mid-write; same blocking-thread trick.
        let pipeline_for_stop = self.pipeline.clone();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::task::spawn_blocking(move || pipeline_for_stop.set_state(State::Null)),
        )
        .await;
        bus_shutdown.store(true, Ordering::Relaxed);
        let _ = bus_thread.join();

        // Closing the collector signals the writer thread to finalize.
        drop(self.collector.lock().unwrap().take());
        let frames = self.frame_counter.load(Ordering::Relaxed);
        tracing::info!(frames, "gifski collector closed");
        let write_result = self
            .writer_thread
            .join()
            .map_err(|_| anyhow::anyhow!("gif writer thread panicked"))?;

        run_result.and(write_result)
    }
}
