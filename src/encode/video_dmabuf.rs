//! VAAPI-backed gst encode pipeline that consumes dmabuf frames from
//! `capture::dmabuf_stream`. Zero CPU touch between capture and encode:
//! `appsrc` emits `memory:DMABuf` buffers wrapping the GBM BO's fd,
//! `vapostproc` imports as a VA surface, `vah264enc` encodes from the
//! same VA surface, `mp4mux` finalizes the container.
//!
//! Crop is applied via `vapostproc`'s built-in `crop-*` properties — no
//! separate `videocrop` element needed, no extra GPU pass.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use gstreamer::prelude::*;
use gstreamer::{Buffer, ClockTime, ElementFactory, MessageView, PadProbeReturn, PadProbeType, Pipeline, State};
use gstreamer_app::AppSrc;

use std::collections::HashMap;

use crate::capture::dmabuf_stream::{DmabufStreamFormat, DmabufStreamFrame};
use crate::cli::VideoContainer;
use crate::encode::video::{CaptureHandle, CropRect};

/// Query a gst element factory's sink-pad template for the set of
/// dmabuf (fourcc, modifier) pairs it accepts. Returned in the same
/// `Vec<(u32, Vec<u64>)>` shape cosmic-screencopy uses for its
/// advertised formats so the two sets can be intersected directly via
/// `capture::dmabuf::pick_format`.
///
/// Each `video/x-raw(memory:DMABuf)` structure with `format=DMA_DRM`
/// on the element's sink template carries a `drm-format` field — a
/// single string or list of strings shaped `"FOURCC:0xMODIFIER"` — and
/// we parse all of them.
pub fn query_gst_consumer_formats(factory_name: &str) -> Result<Vec<(u32, Vec<u64>)>> {
    gstreamer::init().context("gstreamer init")?;
    let factory = gstreamer::ElementFactory::find(factory_name)
        .ok_or_else(|| anyhow::anyhow!("gst element '{factory_name}' not available"))?;
    let templates = factory.static_pad_templates();
    let mut out: HashMap<u32, Vec<u64>> = HashMap::new();
    for tmpl in templates {
        if tmpl.direction() != gstreamer::PadDirection::Sink {
            continue;
        }
        let caps = tmpl.caps();
        for i in 0..caps.size() {
            let s = caps.structure(i).unwrap();
            if s.name() != "video/x-raw" {
                continue;
            }
            if s.get::<String>("format").ok().as_deref() != Some("DMA_DRM") {
                continue;
            }
            let Some(drm_field) = s.value("drm-format").ok() else {
                continue;
            };
            let entries: Vec<String> = if let Ok(single) = drm_field.get::<String>() {
                vec![single]
            } else if let Ok(list) = drm_field.get::<gstreamer::List>() {
                list.iter().filter_map(|v| v.get::<String>().ok()).collect()
            } else {
                Vec::new()
            };
            for entry in entries {
                match gstreamer_video::dma_drm_fourcc_from_str(&entry) {
                    Ok((fourcc, modifier)) => out.entry(fourcc).or_default().push(modifier),
                    Err(e) => tracing::warn!(?entry, error = %e, "drm-format parse"),
                }
            }
        }
    }
    Ok(out.into_iter().collect())
}

pub struct VideoSessionDmabuf {
    pipeline: Pipeline,
    appsrc: AppSrc,
    allocator: gstreamer_allocators::DmaBufAllocator,
    /// Normalized crop rectangle in source physical pixels — even-
    /// dimension-rounded for H.264 4:2:0, clipped to the source bounds.
    /// Applied per-buffer via `GstVideoCropMeta` since vapostproc has
    /// no crop properties on the element itself.
    crop: Option<CropRect>,
    frames_pushed: Arc<AtomicU64>,
    bytes_at_sink: Arc<AtomicU64>,
    _capture: CaptureHandle,
}

impl VideoSessionDmabuf {
    pub fn build(
        capture: CaptureHandle,
        format: DmabufStreamFormat,
        output: &Path,
        fps: u32,
        container: VideoContainer,
        audio: bool,
        crop: Option<CropRect>,
    ) -> Result<Self> {
        gstreamer::init().context("gstreamer init")?;

        // Container drives mux + codec choice. AMD's VAAPI VP9 is solid
        // on RDNA3 (the test machine), but for now mp4/h264 is the
        // simplest cross-container path and matches the rest of the
        // codebase. WebM via VAAPI VP9 is a follow-up.
        let muxer = match container {
            VideoContainer::Mp4 => "mp4mux name=mux faststart=true",
            VideoContainer::Mkv => "matroskamux name=mux",
            VideoContainer::WebM => {
                anyhow::bail!(
                    "WebM is not implemented yet on the dmabuf record path; \
                     use mp4 or mkv until VAAPI VP9 wiring lands"
                );
            }
        };
        // Audio branch in parallel: pulsesrc → AAC → mux.audio_0.
        // Matches VideoSession::build (the SHM/portal path) so the
        // user's `--audio` choice has the same shape in both paths.
        let audio_branch = if audio {
            "pulsesrc ! audioconvert ! audioresample ! avenc_aac ! mux.audio_0"
        } else {
            ""
        };

        // Normalize the requested crop rectangle: clip to source bounds
        // and round dimensions to even for H.264 4:2:0. `effective_crop`
        // is the rect we'll attach as `GstVideoCropMeta` on each buffer;
        // vapostproc honors the meta and downsamples / color-converts
        // the cropped sub-rect into whatever the downstream caps demand.
        let user_crop = crop.and_then(|c| {
            let left = c.x.clamp(0, format.width as i32) as u32;
            let top = c.y.clamp(0, format.height as i32) as u32;
            let mut width = c.w.min(format.width.saturating_sub(left));
            let mut height = c.h.min(format.height.saturating_sub(top));
            if width % 2 == 1 {
                width = width.saturating_sub(1);
            }
            if height % 2 == 1 {
                height = height.saturating_sub(1);
            }
            if width == 0 || height == 0 {
                tracing::warn!(
                    ?c,
                    stream_w = format.width,
                    stream_h = format.height,
                    "crop outside stream bounds; recording full source"
                );
                return None;
            }
            Some(CropRect {
                x: left as i32,
                y: top as i32,
                w: width,
                h: height,
            })
        });
        // vah264enc / H.264 4:2:0 requires even dimensions. Outputs are
        // always even, but toplevels (windows) can be any size — e.g.
        // 1031×835 hits this. When no user crop is set but the source
        // dimensions are odd, synthesize a top-left-anchored even crop
        // so the same `GstVideoCropMeta` mechanism handles both cases.
        let effective_crop = user_crop.or_else(|| {
            let odd_w = format.width % 2 == 1;
            let odd_h = format.height % 2 == 1;
            if !odd_w && !odd_h {
                return None;
            }
            let w = format.width & !1;
            let h = format.height & !1;
            if w == 0 || h == 0 {
                return None;
            }
            Some(CropRect {
                x: 0,
                y: 0,
                w,
                h,
            })
        });

        // When cropping, pin vapostproc's output to the cropped size
        // (VAMemory NV12). vapostproc reads the per-buffer
        // `GstVideoCropMeta` to know which sub-rect of the input to
        // process, then writes out at this size. Without a crop, let
        // negotiation pick the output size — the full source frame.
        let crop_filter = match effective_crop {
            Some(c) => format!(
                "! video/x-raw(memory:VAMemory),format=NV12,width={},height={} !",
                c.w, c.h
            ),
            None => "!".to_string(),
        };

        let location = escape_for_gst(output.to_string_lossy().as_ref());
        // Pipeline:
        //   appsrc → vapostproc → [crop_filter] → vah264enc → h264parse → muxer → filesink
        // `do-timestamp=true` lets appsrc stamp each pushed buffer with
        // the current pipeline clock time; we don't `set_pts` manually.
        let pipeline_str = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true \
               ! queue max-size-buffers=4 max-size-bytes=0 max-size-time=0 leaky=downstream \
               ! vapostproc name=vapp \
               {crop_filter} vah264enc \
               ! h264parse name=h264p \
               ! {muxer} \
               ! filesink name=sink location={location} {audio}",
            audio = audio_branch,
        );
        tracing::info!(%pipeline_str, "constructing dmabuf gst pipeline");

        let element = gstreamer::parse::launch(&pipeline_str).context("parse_launch")?;
        let pipeline = element
            .downcast::<Pipeline>()
            .map_err(|_| anyhow::anyhow!("parse_launch did not return a Pipeline"))?;

        let appsrc = pipeline
            .by_name("src")
            .ok_or_else(|| anyhow::anyhow!("appsrc 'src' missing"))?
            .dynamic_cast::<AppSrc>()
            .map_err(|_| anyhow::anyhow!("'src' element is not AppSrc"))?;

        // Caps built from VideoInfoDmaDrm so the drm-format string is
        // exactly what vapostproc parses on the other side.
        let vinfo = gstreamer_video::VideoInfo::builder(
            gstreamer_video::VideoFormat::Bgra,
            format.width,
            format.height,
        )
        .fps(gstreamer::Fraction::new(fps as i32, 1))
        .build()
        .map_err(|e| anyhow::anyhow!("VideoInfo::builder: {e}"))?;
        let dma_info =
            gstreamer_video::VideoInfoDmaDrm::new(vinfo, format.fourcc, format.modifier);
        let caps = dma_info
            .to_caps()
            .map_err(|e| anyhow::anyhow!("VideoInfoDmaDrm::to_caps: {e}"))?;
        appsrc.set_caps(Some(&caps));
        tracing::info!(caps = %caps, "appsrc dmabuf caps set");

        if ElementFactory::find("vapostproc").is_none()
            || ElementFactory::find("vah264enc").is_none()
        {
            anyhow::bail!(
                "vapostproc + vah264enc required for dmabuf recording. \
                 Install gstreamer-vaapi (gst-plugins-bad's `va` plugin)."
            );
        }

        // Same PTS-rescue probe as the multi pipeline: vah264enc can
        // emit delta frames with PTS=NONE while DTS is valid, and
        // mp4mux rejects those. Mirror DTS → PTS for buffers missing
        // PTS so mp4mux always has a value to write.
        if let Some(h264p) = pipeline.by_name("h264p") {
            if let Some(parser_src) = h264p.static_pad("src") {
                parser_src.add_probe(PadProbeType::BUFFER, |_pad, info| {
                    if let Some(gstreamer::PadProbeData::Buffer(buf)) = info.data.as_mut() {
                        if let Some(buf_mut) = buf.get_mut() {
                            if buf_mut.pts().is_none() {
                                if let Some(dts) = buf_mut.dts() {
                                    buf_mut.set_pts(Some(dts));
                                }
                            }
                        }
                    }
                    PadProbeReturn::Ok
                });
            }
        }

        let frames_pushed = Arc::new(AtomicU64::new(0));
        let bytes_at_sink = Arc::new(AtomicU64::new(0));
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
            allocator: gstreamer_allocators::DmaBufAllocator::new(),
            crop: effective_crop,
            frames_pushed,
            bytes_at_sink,
            _capture: capture,
        })
    }

    pub async fn run(
        self,
        stop: tokio::sync::oneshot::Receiver<()>,
        mut frame_rx: tokio::sync::mpsc::Receiver<DmabufStreamFrame>,
    ) -> Result<()> {
        let appsrc = self.appsrc.clone();
        let allocator = self.allocator.clone();
        let crop = self.crop;
        let frames_pushed = self.frames_pushed.clone();
        let _pump = tokio::spawn(async move {
            while let Some(stream_frame) = frame_rx.recv().await {
                let buf = match crop {
                    Some(c) => build_gst_buffer_cropped(&allocator, &stream_frame, c),
                    None => build_gst_buffer(&allocator, &stream_frame),
                };
                let buf = match buf {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = %e, "build_gst_buffer failed, dropping frame");
                        continue;
                    }
                };
                if let Err(e) = appsrc.push_buffer(buf) {
                    tracing::warn!(error = %e, "appsrc push_buffer failed; stopping pump");
                    break;
                }
                frames_pushed.fetch_add(1, Ordering::Relaxed);
            }
            let _ = appsrc.end_of_stream();
            tracing::info!("dmabuf frame pump exited");
        });

        let pipeline_for_start = self.pipeline.clone();
        let state_call =
            tokio::task::spawn_blocking(move || pipeline_for_start.set_state(State::Playing));
        match tokio::time::timeout(std::time::Duration::from_secs(3), state_call).await {
            Ok(Ok(r)) => r.context("→ Playing")?,
            Ok(Err(e)) => anyhow::bail!("set_state(Playing) panicked: {e}"),
            Err(_) => anyhow::bail!("set_state(Playing) did not return within 3s"),
        };
        let bus = self.pipeline.bus().context("pipeline bus")?;

        let (bus_tx, mut bus_rx) = tokio::sync::mpsc::unbounded_channel::<Result<()>>();
        let bus_thread = std::thread::spawn(move || {
            for msg in bus.iter_timed(ClockTime::NONE) {
                match msg.view() {
                    MessageView::Eos(_) => {
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
                    _ => {}
                }
            }
        });

        let result = tokio::select! {
            r = bus_rx.recv() => r.unwrap_or(Ok(())),
            _ = stop => {
                tracing::info!("dmabuf record: stop signal, sending EOS");
                self.pipeline.send_event(gstreamer::event::Eos::new());
                let drain = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    bus_rx.recv(),
                ).await;
                match drain {
                    Ok(Some(r)) => r,
                    Ok(None) => Ok(()),
                    Err(_) => {
                        tracing::warn!(
                            frames = self.frames_pushed.load(Ordering::Relaxed),
                            bytes = self.bytes_at_sink.load(Ordering::Relaxed),
                            "dmabuf pipeline did not respond to EOS within 3s"
                        );
                        Ok(())
                    }
                }
            }
        };

        tracing::info!(
            frames = self.frames_pushed.load(Ordering::Relaxed),
            bytes = self.bytes_at_sink.load(Ordering::Relaxed),
            "dmabuf pipeline finished"
        );

        let pipeline_for_stop = self.pipeline.clone();
        let null_call =
            tokio::task::spawn_blocking(move || pipeline_for_stop.set_state(State::Null));
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), null_call).await;
        let _ = bus_thread.join();
        result
    }
}

/// Build a `GstBuffer` wrapping one dmabuf frame. Same logic the debug
/// subcommand uses (same-backing detection via fstat inode), but
/// destructures the `DmabufStreamFrame` to also stamp PTS from
/// `pts_ns`.
fn build_gst_buffer(
    allocator: &gstreamer_allocators::DmaBufAllocator,
    stream_frame: &DmabufStreamFrame,
) -> Result<Buffer> {
    let frame = &stream_frame.frame;
    let plane_count = frame.bo.plane_count() as i32;
    let plane_fds: Vec<rustix::fd::OwnedFd> = (0..plane_count)
        .map(|p| {
            frame
                .bo
                .fd_for_plane(p)
                .map_err(|e| anyhow::anyhow!("fd_for_plane({p}): {e:?}"))
        })
        .collect::<Result<_, _>>()?;
    let stats: Vec<_> = plane_fds
        .iter()
        .map(rustix::fs::fstat)
        .collect::<Result<_, _>>()
        .context("fstat dmabuf plane fd")?;
    let same_backing = stats
        .iter()
        .all(|s| s.st_ino == stats[0].st_ino && s.st_dev == stats[0].st_dev);

    let mut buf = Buffer::new();
    {
        let buf_mut = buf.get_mut().expect("fresh buffer is unique");
        let fds: Vec<_> = if same_backing {
            vec![plane_fds.into_iter().next().unwrap()]
        } else {
            plane_fds
        };
        for fd in fds {
            let size = rustix::fs::seek(&fd, rustix::fs::SeekFrom::End(0))
                .context("lseek dmabuf fd")?;
            rustix::fs::seek(&fd, rustix::fs::SeekFrom::Start(0))
                .context("lseek dmabuf fd back")?;
            let mem = unsafe { allocator.alloc(fd, size as usize) }
                .map_err(|e| anyhow::anyhow!("DmaBufAllocator::alloc: {e}"))?;
            buf_mut.append_memory(mem);
        }
        // Attach VideoMeta with the BO's actual per-plane strides and
        // offsets. Without it vapostproc would compute defaults from
        // caps width — fine when the natural stride happens to match
        // (e.g. 2560-wide outputs), but broken for odd or unaligned
        // sizes where GBM pads the stride (e.g. a 1271-wide window
        // gets a 5120-byte stride, not 5084). Symptom is "Internal
        // data stream error" from appsrc as downstream rejects the
        // mis-strided import.
        let plane_count = frame.bo.plane_count() as usize;
        let offsets: Vec<usize> = (0..plane_count as i32)
            .map(|p| frame.bo.offset(p) as usize)
            .collect();
        let strides: Vec<i32> = (0..plane_count as i32)
            .map(|p| frame.bo.stride_for_plane(p) as i32)
            .collect();
        gstreamer_video::VideoMeta::add_full(
            buf_mut,
            gstreamer_video::VideoFrameFlags::empty(),
            gstreamer_video::VideoFormat::Bgra,
            frame.width,
            frame.height,
            &offsets,
            &strides,
        )
        .map_err(|e| anyhow::anyhow!("VideoMeta::add_full: {e}"))?;
        // PTS is stamped by appsrc via `do-timestamp=true` (pipeline
        // clock at push time). `stream_frame.pts_ns` is retained on the
        // stream type for diagnostics / future pacing decisions.
        let _ = stream_frame.pts_ns;
    }
    Ok(buf)
}

/// Same as `build_gst_buffer` plus a `GstVideoCropMeta` carrying the
/// per-branch source crop rect. vapostproc consumes the meta to limit
/// processing to the cropped sub-rect and produces output at whatever
/// size its downstream capsfilter pins.
fn build_gst_buffer_cropped(
    allocator: &gstreamer_allocators::DmaBufAllocator,
    stream_frame: &DmabufStreamFrame,
    crop: CropRect,
) -> Result<Buffer> {
    let mut buf = build_gst_buffer(allocator, stream_frame)?;
    {
        let buf_mut = buf.get_mut().expect("fresh buffer is unique");
        let x = crop.x.max(0) as u32;
        let y = crop.y.max(0) as u32;
        gstreamer_video::VideoCropMeta::add(buf_mut, (x, y, crop.w, crop.h));
    }
    Ok(buf)
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

// ---------------------------------------------------------------------
// Multi-source compositing pipeline (cross-screen region recording).
// ---------------------------------------------------------------------

/// One input branch of the composite pipeline: which source the dmabuf
/// stream is reading, where its captured area sits in the source's own
/// physical pixels, and where the cropped result should land on the
/// composite canvas.
pub struct CompositePart {
    /// Crop applied by this branch's `vapostproc`, in source physical
    /// pixels. The compositor sees only the cropped rectangle.
    pub src_crop: CropRect,
    /// Top-left position of this branch's output within the composite
    /// canvas, in canvas pixels.
    pub dst_pos: (u32, u32),
    /// Dimensions of this branch's pad on the compositor, in canvas
    /// pixels. Same as `(src_crop.w, src_crop.h)` for a 1:1 scale; we
    /// keep them separate so different output scales can be normalized
    /// to a single target scale by the caller before constructing the
    /// pipeline.
    pub dst_size: (u32, u32),
    /// The source's negotiated dmabuf format (fourcc + modifier + size).
    /// Each appsrc has its own caps; all of them feed into one
    /// vacompositor that produces a single VA-memory composite.
    pub format: DmabufStreamFormat,
}

pub struct VideoSessionDmabufMulti {
    pipeline: Pipeline,
    /// Per-branch `(appsrc, src_crop)`. The crop is in source physical
    /// pixels and is applied per-buffer via `GstVideoCropMeta` rather
    /// than a vapostproc element property — vapostproc honors the meta
    /// but doesn't expose crop as a property.
    branches: Vec<(AppSrc, CropRect)>,
    allocator: gstreamer_allocators::DmaBufAllocator,
    frames_pushed: Arc<AtomicU64>,
    bytes_at_sink: Arc<AtomicU64>,
    _captures: Vec<CaptureHandle>,
}

impl VideoSessionDmabufMulti {
    pub fn build(
        captures: Vec<CaptureHandle>,
        parts: Vec<CompositePart>,
        output: &Path,
        fps: u32,
        container: VideoContainer,
        audio: bool,
    ) -> Result<Self> {
        gstreamer::init().context("gstreamer init")?;
        anyhow::ensure!(
            !parts.is_empty() && parts.len() == captures.len(),
            "VideoSessionDmabufMulti::build: parts/captures length mismatch \
             ({} parts, {} captures)",
            parts.len(),
            captures.len()
        );

        if ElementFactory::find("vacompositor").is_none() {
            anyhow::bail!(
                "vacompositor required for cross-screen recording. \
                 Install gstreamer-vaapi (gst-plugins-bad's `va` plugin)."
            );
        }

        // Canvas size: max-extent over all parts. We trust the caller to
        // have computed dst_pos/dst_size such that they don't overlap.
        let canvas_w = parts
            .iter()
            .map(|p| p.dst_pos.0 + p.dst_size.0)
            .max()
            .unwrap_or(0);
        let canvas_h = parts
            .iter()
            .map(|p| p.dst_pos.1 + p.dst_size.1)
            .max()
            .unwrap_or(0);
        anyhow::ensure!(canvas_w > 0 && canvas_h > 0, "composite canvas is empty");
        // H.264 4:2:0 demands even output dimensions.
        let canvas_w = canvas_w - (canvas_w % 2);
        let canvas_h = canvas_h - (canvas_h % 2);

        let pipeline = Pipeline::new();

        // Tail: vacompositor → vah264enc → h264parse → muxer → filesink.
        // Constructed first so we can grab the request pads to link the
        // per-source branches into.
        //
        // `force-live=true` makes vacompositor aggregate on its own
        // output-clock timeout regardless of upstream live signalling.
        // Without it the aggregator waits indefinitely for "all pads
        // ready" semantics that the screencopy-pacing loop doesn't
        // produce — and mp4mux ends up with no PTS on the first
        // buffer because vah264enc never received an input frame. The
        // property is construction-time-only on videoaggregator, so we
        // set it through the builder rather than `set_property`.
        let vacomp = ElementFactory::make("vacompositor")
            .property("force-live", true)
            .property("latency", 0u64)
            // `start-time-selection=zero` makes the aggregator stamp the
            // first output buffer at PTS=0 rather than copying a possibly
            // invalid timestamp from upstream sync events. Without this,
            // vacompositor on this gst build hands an early header/
            // preroll buffer downstream with PTS=NONE, which mp4mux
            // rejects.
            .property_from_str("start-time-selection", "zero")
            .build()?;
        let enc = ElementFactory::make("vah264enc").build()?;
        let parser = ElementFactory::make("h264parse").build()?;
        let muxer = match container {
            VideoContainer::Mp4 => {
                let m = ElementFactory::make("mp4mux").build()?;
                m.set_property("faststart", true);
                m
            }
            VideoContainer::Mkv => ElementFactory::make("matroskamux").build()?,
            VideoContainer::WebM => anyhow::bail!(
                "WebM is not implemented yet on the dmabuf compositor path"
            ),
        };
        let filesink = ElementFactory::make("filesink").build()?;
        filesink.set_property("location", output.to_string_lossy().to_string());

        // Audio branch: pulsesrc → audioconvert → audioresample →
        // avenc_aac → mux.audio_0. Built only when `audio=true`.
        let audio_elements: Option<(
            gstreamer::Element, // pulsesrc
            gstreamer::Element, // audioconvert
            gstreamer::Element, // audioresample
            gstreamer::Element, // avenc_aac
        )> = if audio {
            let pulsesrc = ElementFactory::make("pulsesrc").build()?;
            let aconv = ElementFactory::make("audioconvert").build()?;
            let aresample = ElementFactory::make("audioresample").build()?;
            let aenc = ElementFactory::make("avenc_aac").build()?;
            Some((pulsesrc, aconv, aresample, aenc))
        } else {
            None
        };

        // Composite output caps: fix to VAMemory NV12 at canvas size +
        // target framerate. vacompositor → vah264enc happily auto-
        // negotiates without this, but pinning the size + framerate
        // here protects against the compositor inferring weird sizes
        // from individual pads.
        let comp_caps = gstreamer::Caps::builder("video/x-raw")
            .features(["memory:VAMemory"])
            .field("format", "NV12")
            .field("width", canvas_w as i32)
            .field("height", canvas_h as i32)
            .field("framerate", gstreamer::Fraction::new(fps as i32, 1))
            .build();
        let comp_capsfilter = ElementFactory::make("capsfilter")
            .property("caps", &comp_caps)
            .build()?;

        pipeline.add_many([
            &vacomp,
            &comp_capsfilter,
            &enc,
            &parser,
            &muxer,
            &filesink,
        ])?;
        gstreamer::Element::link_many([
            &vacomp,
            &comp_capsfilter,
            &enc,
            &parser,
            &muxer,
            &filesink,
        ])
        .context("link composite tail")?;

        // Link the audio branch into `mux.audio_0` if it exists. Audio
        // uses the mux's request `audio_%u` pad — different from video's
        // static `sink` pad — so it links via `link_pads`.
        if let Some((pulsesrc, aconv, aresample, aenc)) = audio_elements.as_ref() {
            pipeline.add_many([pulsesrc, aconv, aresample, aenc])?;
            gstreamer::Element::link_many([pulsesrc, aconv, aresample, aenc])
                .context("link audio chain")?;
            let aenc_src = aenc
                .static_pad("src")
                .ok_or_else(|| anyhow::anyhow!("avenc_aac has no src pad"))?;
            let mux_audio = muxer
                .request_pad_simple("audio_%u")
                .ok_or_else(|| anyhow::anyhow!("muxer rejected audio_%u request pad"))?;
            aenc_src.link(&mux_audio).context("link aenc → mux.audio_0")?;
        }

        // vah264enc on this Mesa build emits delta frames with
        // PTS=GST_CLOCK_TIME_NONE while keeping a valid DTS. mp4mux
        // strictly requires PTS on every buffer and aborts otherwise.
        // Since the encoder produces non-B-frame H.264 (has_b_frames=0,
        // verified via ffprobe on the single-output recording), PTS
        // must equal DTS by construction — we restore that invariant
        // with a pad probe on h264parse's src pad.
        if let Some(parser_src) = parser.static_pad("src") {
            parser_src.add_probe(PadProbeType::BUFFER, |_pad, info| {
                if let Some(gstreamer::PadProbeData::Buffer(buf)) = info.data.as_mut() {
                    if let Some(buf_mut) = buf.get_mut() {
                        if buf_mut.pts().is_none() {
                            if let Some(dts) = buf_mut.dts() {
                                buf_mut.set_pts(Some(dts));
                            }
                        }
                    }
                }
                PadProbeReturn::Ok
            });
        }

        // Per-source branches. Each is:
        //   appsrc (DMA_DRM caps) → queue → vapostproc → capsfilter
        //   (VAMemory NV12 at the cropped size) → vacompositor.sink_N
        // The crop itself rides on each pushed buffer as a
        // `GstVideoCropMeta`; vapostproc honors it and downsamples /
        // color-converts the cropped sub-rect into the capsfilter's
        // target size.
        let mut branches = Vec::with_capacity(parts.len());
        for (idx, part) in parts.iter().enumerate() {
            let src_name = format!("src{idx}");
            let appsrc = gstreamer_app::AppSrc::builder()
                .name(&src_name)
                .is_live(true)
                .do_timestamp(true)
                .format(gstreamer::Format::Time)
                .build();
            let vinfo = gstreamer_video::VideoInfo::builder(
                gstreamer_video::VideoFormat::Bgra,
                part.format.width,
                part.format.height,
            )
            .fps(gstreamer::Fraction::new(fps as i32, 1))
            .build()
            .map_err(|e| anyhow::anyhow!("VideoInfo::builder branch {idx}: {e}"))?;
            let dma_info = gstreamer_video::VideoInfoDmaDrm::new(
                vinfo,
                part.format.fourcc,
                part.format.modifier,
            );
            let caps = dma_info
                .to_caps()
                .map_err(|e| anyhow::anyhow!("VideoInfoDmaDrm::to_caps branch {idx}: {e}"))?;
            appsrc.set_caps(Some(&caps));

            let queue = ElementFactory::make("queue")
                .property("max-size-buffers", 4u32)
                .property("max-size-bytes", 0u32)
                .property("max-size-time", 0u64)
                .property_from_str("leaky", "downstream")
                .build()?;
            let vapp = ElementFactory::make("vapostproc").build()?;
            // Pin vapostproc's output to VAMemory NV12 at the cropped
            // dimensions. vacompositor accepts VAMemory NV12 on its
            // sink template; staying on VAMemory keeps the surface on
            // the GPU end-to-end.
            let branch_caps = gstreamer::Caps::builder("video/x-raw")
                .features(["memory:VAMemory"])
                .field("format", "NV12")
                .field("width", part.src_crop.w as i32)
                .field("height", part.src_crop.h as i32)
                .build();
            let branch_capsfilter = ElementFactory::make("capsfilter")
                .property("caps", &branch_caps)
                .build()?;

            pipeline.add_many([appsrc.upcast_ref(), &queue, &vapp, &branch_capsfilter])?;
            gstreamer::Element::link_many([
                appsrc.upcast_ref::<gstreamer::Element>(),
                &queue,
                &vapp,
                &branch_capsfilter,
            ])
            .with_context(|| format!("link branch {idx}"))?;

            // Request a fresh sink pad from vacompositor and configure
            // its geometry. The pad name comes back as "sink_<N>".
            let pad = vacomp
                .request_pad_simple("sink_%u")
                .ok_or_else(|| anyhow::anyhow!("request sink pad on vacompositor branch {idx}"))?;
            pad.set_property("xpos", part.dst_pos.0 as i32);
            pad.set_property("ypos", part.dst_pos.1 as i32);
            pad.set_property("width", part.dst_size.0 as i32);
            pad.set_property("height", part.dst_size.1 as i32);
            let branch_src = branch_capsfilter
                .static_pad("src")
                .ok_or_else(|| anyhow::anyhow!("capsfilter has no src pad branch {idx}"))?;
            branch_src.link(&pad).with_context(|| {
                format!("link branch.src → vacomp.sink for branch {idx}")
            })?;

            branches.push((appsrc, part.src_crop));
        }

        let frames_pushed = Arc::new(AtomicU64::new(0));
        let bytes_at_sink = Arc::new(AtomicU64::new(0));
        if let Some(pad) = filesink.static_pad("sink") {
            let counter = bytes_at_sink.clone();
            pad.add_probe(PadProbeType::BUFFER, move |_pad, info| {
                if let Some(gstreamer::PadProbeData::Buffer(buf)) = info.data.as_ref() {
                    counter.fetch_add(buf.size() as u64, Ordering::Relaxed);
                }
                PadProbeReturn::Ok
            });
        }

        Ok(Self {
            pipeline,
            branches,
            allocator: gstreamer_allocators::DmaBufAllocator::new(),
            frames_pushed,
            bytes_at_sink,
            _captures: captures,
        })
    }

    pub async fn run(
        self,
        stop: tokio::sync::oneshot::Receiver<()>,
        frame_rxs: Vec<tokio::sync::mpsc::Receiver<DmabufStreamFrame>>,
    ) -> Result<()> {
        anyhow::ensure!(
            frame_rxs.len() == self.branches.len(),
            "frame_rxs/branches length mismatch"
        );

        // One pump per branch — each pulls dmabuf frames from its mpsc,
        // attaches the per-branch `GstVideoCropMeta`, and pushes into
        // the matching appsrc. Independent loops keep one stuck output
        // from stalling the others; vacompositor handles per-pad timing.
        let allocator = self.allocator.clone();
        let frames_pushed = self.frames_pushed.clone();
        let mut pumps = Vec::with_capacity(self.branches.len());
        for (idx, ((appsrc, crop), mut frame_rx)) in
            self.branches.iter().cloned().zip(frame_rxs).enumerate()
        {
            let allocator = allocator.clone();
            let frames_pushed = frames_pushed.clone();
            pumps.push(tokio::spawn(async move {
                while let Some(stream_frame) = frame_rx.recv().await {
                    let buf = match build_gst_buffer_cropped(&allocator, &stream_frame, crop) {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::warn!(idx, error = %e, "branch buffer build failed");
                            continue;
                        }
                    };
                    if let Err(e) = appsrc.push_buffer(buf) {
                        tracing::warn!(idx, error = %e, "branch push_buffer failed");
                        break;
                    }
                    frames_pushed.fetch_add(1, Ordering::Relaxed);
                }
                let _ = appsrc.end_of_stream();
                tracing::info!(idx, "branch pump exited");
            }));
        }

        let pipeline_for_start = self.pipeline.clone();
        let state_call =
            tokio::task::spawn_blocking(move || pipeline_for_start.set_state(State::Playing));
        match tokio::time::timeout(std::time::Duration::from_secs(3), state_call).await {
            Ok(Ok(r)) => r.context("composite → Playing")?,
            Ok(Err(e)) => anyhow::bail!("set_state(Playing) panicked: {e}"),
            Err(_) => anyhow::bail!("composite set_state(Playing) did not return within 3s"),
        };
        let bus = self.pipeline.bus().context("pipeline bus")?;

        let (bus_tx, mut bus_rx) = tokio::sync::mpsc::unbounded_channel::<Result<()>>();
        let bus_thread = std::thread::spawn(move || {
            for msg in bus.iter_timed(ClockTime::NONE) {
                match msg.view() {
                    MessageView::Eos(_) => {
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
                    _ => {}
                }
            }
        });

        let result = tokio::select! {
            r = bus_rx.recv() => r.unwrap_or(Ok(())),
            _ = stop => {
                tracing::info!("composite: stop signal, sending EOS");
                self.pipeline.send_event(gstreamer::event::Eos::new());
                let drain = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    bus_rx.recv(),
                ).await;
                match drain {
                    Ok(Some(r)) => r,
                    Ok(None) => Ok(()),
                    Err(_) => {
                        tracing::warn!(
                            frames = self.frames_pushed.load(Ordering::Relaxed),
                            bytes = self.bytes_at_sink.load(Ordering::Relaxed),
                            "composite did not respond to EOS within 3s"
                        );
                        Ok(())
                    }
                }
            }
        };

        tracing::info!(
            frames_pushed = self.frames_pushed.load(Ordering::Relaxed),
            bytes_at_sink = self.bytes_at_sink.load(Ordering::Relaxed),
            "composite pipeline finished"
        );

        for p in pumps {
            p.abort();
        }
        let pipeline_for_stop = self.pipeline.clone();
        let null_call =
            tokio::task::spawn_blocking(move || pipeline_for_stop.set_state(State::Null));
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), null_call).await;
        let _ = bus_thread.join();
        result
    }
}
