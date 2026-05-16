//! GIF encoding fed by a GStreamer `appsink` on RGBA samples.
//!
//! gifski runs the encode on its own threads; we feed RGBA + PTS through
//! a `Collector`, then a writer thread drains the encoder to disk.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use gifski::{Repeat, Settings};
use gstreamer::prelude::*;
use gstreamer::{MessageView, Pipeline, State};
use gstreamer_app::AppSink;
use gstreamer_video::VideoInfo;

use crate::capture::screencast::PipeWireStream;
use crate::encode::video::{build_crop_str, CropRect};

pub struct GifSession {
    pipeline: Pipeline,
    writer_thread: std::thread::JoinHandle<Result<()>>,
    collector: Arc<Mutex<Option<gifski::Collector>>>,
    frame_counter: Arc<AtomicUsize>,
    _stream: PipeWireStream,
}

impl GifSession {
    pub fn build(
        stream: PipeWireStream,
        output: &Path,
        fps: u32,
        quality: u8,
        max_width: u32,
        crop: Option<CropRect>,
    ) -> Result<Self> {
        gstreamer::init().context("gstreamer init")?;

        // We can't know exact dimensions until the first sample, so the
        // gifski Settings leave width/height unset (None) — gifski uses
        // the first frame's natural size.
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

        // pipewiresrc → videorate(fps) → optional crop → optional videoscale → RGBA → appsink
        let crop_str = build_crop_str(crop, stream.size);
        let scale = if max_width > 0 {
            format!("videoscale ! video/x-raw,width=(int){max_width},pixel-aspect-ratio=1/1 !")
        } else {
            String::new()
        };
        let pipeline_str = format!(
            "pipewiresrc fd={fd} path={node} do-timestamp=true keepalive-time=1000 \
             ! videorate ! video/x-raw,framerate={fps}/1 \
             {crop_str}\
             ! videoconvert \
             ! {scale} \
             video/x-raw,format=RGBA \
             ! appsink name=sink emit-signals=true max-buffers=8 drop=false sync=false",
            fd = stream.raw_fd(),
            node = stream.node_id,
        );
        tracing::info!(%pipeline_str, "constructing gst gif pipeline");

        let element =
            gstreamer::parse::launch(&pipeline_str).context("gst parse_launch (gif)")?;
        let pipeline = element
            .downcast::<Pipeline>()
            .map_err(|_| anyhow::anyhow!("parse_launch did not return a Pipeline"))?;

        let appsink = pipeline
            .by_name("sink")
            .context("appsink not found")?
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
            writer_thread,
            collector,
            frame_counter,
            _stream: stream,
        })
    }

    pub async fn run(self, stop: tokio::sync::oneshot::Receiver<()>) -> Result<()> {
        self.pipeline.set_state(State::Playing).context("pipeline → Playing")?;

        let bus = self.pipeline.bus().context("pipeline bus")?;
        let (err_tx, mut err_rx) = tokio::sync::mpsc::unbounded_channel::<anyhow::Error>();
        let bus_thread = std::thread::spawn(move || {
            for msg in bus.iter_timed(gstreamer::ClockTime::NONE) {
                if let MessageView::Error(e) = msg.view() {
                    let _ = err_tx.send(anyhow::anyhow!(
                        "{}: {}",
                        e.error(),
                        e.debug().unwrap_or_default()
                    ));
                    break;
                }
                if matches!(msg.view(), MessageView::Eos(_)) {
                    break;
                }
            }
        });

        // Wait for either a stop signal (UI's Stop button or CLI's
        // duration/ctrl_c task) or a fatal bus error. Previously this had a
        // bare sleep(duration_secs) timeout, which meant the GUI's gif mode
        // couldn't be stopped by the user — and a default `duration_secs: 0`
        // made the gif finish instantly with no frames.
        let run_result: Result<()> = tokio::select! {
            err = err_rx.recv() => Err(err.unwrap_or_else(|| anyhow::anyhow!("bus channel closed"))),
            _ = stop => Ok(()),
        };

        self.pipeline.send_event(gstreamer::event::Eos::new());
        self.pipeline.set_state(State::Null).context("pipeline → Null")?;
        let _ = bus_thread.join();

        // Drop the collector to signal the writer thread to finalize.
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
