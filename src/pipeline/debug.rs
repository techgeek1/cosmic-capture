//! Debug entry points for exercising capture-layer code paths outside
//! the GUI / full recording pipelines. Used during development to verify
//! a path end-to-end before it gets wired into the production flow.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use anyhow::{Context, Result};
use cosmic_client_toolkit::screencopy::CaptureSource;
use gstreamer::prelude::*;
use gstreamer::{Buffer, ClockTime, MessageView, Pipeline, State};
use gstreamer_app::AppSrc;
use wayland_client::Connection;

use crate::capture::dmabuf_stream::{self};
use crate::capture::wayland::WaylandHelper;
use crate::cli::{DebugDmabufArgs, DebugDmabufRecordArgs, DebugDmabufRecordMultiArgs};
use crate::cli::VideoContainer;
use crate::encode::video::CropRect;
use crate::encode::video_dmabuf::{
    query_gst_consumer_formats, CompositePart, VideoSessionDmabuf, VideoSessionDmabufMulti,
};

/// Capture one frame from a named output via the dmabuf path, CPU-map the
/// resulting GBM buffer object, and write it to PNG. This is the only
/// place we do a CPU readback from a dmabuf — the recording pipeline
/// hands the same fd to gst's `vapostproc` and never touches pixels on
/// the CPU.
pub async fn run_dmabuf(args: DebugDmabufArgs) -> Result<()> {
    let conn = Connection::connect_to_env().context("wayland connect")?;
    let helper = WaylandHelper::new(conn).context("WaylandHelper::new")?;
    // Give the helper one dispatch tick to populate outputs.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let output = helper
        .output_for_name(&args.output)
        .ok_or_else(|| anyhow::anyhow!("no wayland output named {:?}", args.output))?;

    let frame = helper
        .capture_source_dmabuf(CaptureSource::Output(output), args.cursor, None)
        .await
        .context("capture_source_dmabuf")?;
    tracing::info!(?frame, "dmabuf captured");

    let path = args.file.unwrap_or_else(|| {
        PathBuf::from(format!("./dmabuf-debug-{}.png", args.output))
    });

    // CPU readback via gbm's map. `map` requires LINEAR modifier on the
    // buffer to be meaningful — Mesa returns a synthesized linear view
    // for some modifiers, but the safest assumption is that this only
    // round-trips correctly when the compositor handed us LINEAR. If
    // GBM picked a tiled modifier, the pixels will be detiled by the
    // mapping read on most drivers, but it's not guaranteed by the
    // dmabuf spec. For debug verification this is fine; the production
    // path never CPU-maps and so doesn't care.
    let width = frame.width;
    let height = frame.height;
    let png_bytes = frame
        .bo
        .map(0, 0, width, height, |mapping| {
            let stride = mapping.stride() as usize;
            let buf = mapping.buffer();
            let row_bytes = (width as usize) * 4;
            // Repack into tightly-packed RGBA. cosmic-comp writes the
            // fourcc memory layout we asked for: AR24 (DRM_FORMAT_ARGB8888)
            // is byte order B,G,R,A on little-endian platforms, so swap
            // to RGBA for png.
            let mut out = vec![0u8; row_bytes * (height as usize)];
            for y in 0..(height as usize) {
                let src = &buf[y * stride..y * stride + row_bytes];
                let dst = &mut out[y * row_bytes..(y + 1) * row_bytes];
                for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
                    d[0] = s[2]; // R
                    d[1] = s[1]; // G
                    d[2] = s[0]; // B
                    d[3] = s[3]; // A
                }
            }
            out
        })
        .map_err(|e| anyhow::anyhow!("gbm map: {e}"))?;

    let file = BufWriter::new(File::create(&path).with_context(|| format!("create {:?}", path))?);
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("png header")?;
    writer
        .write_image_data(&png_bytes)
        .context("png write_image_data")?;
    println!("{}", path.display());
    Ok(())
}

/// Same one-shot capture as `run_dmabuf`, but the BO is fed through a
/// gst pipeline (`appsrc ! vapostproc ! pngenc ! filesink`) instead of
/// CPU-mapped. Used during dmabuf-pipeline bring-up to confirm gst's
/// `vapostproc` accepts the buffer + caps we construct, before scaling
/// to the real recording / compositing pipeline.
pub async fn run_dmabuf_gst(args: DebugDmabufArgs) -> Result<()> {
    let conn = Connection::connect_to_env().context("wayland connect")?;
    let helper = WaylandHelper::new(conn).context("WaylandHelper::new")?;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let output = helper
        .output_for_name(&args.output)
        .ok_or_else(|| anyhow::anyhow!("no wayland output named {:?}", args.output))?;

    // Query vapostproc's accepted dmabuf formats so cosmic-screencopy
    // hands us a modifier the VA driver can actually import. On AMD
    // GFX10+, vapostproc only accepts one specific tiled modifier per
    // fourcc — leaving cosmic-comp's free choice in place gets us DCC-
    // with-retile buffers that vapostproc rejects at negotiation.
    let consumer = query_gst_consumer_formats("vapostproc")?;

    let frame = helper
        .capture_source_dmabuf(CaptureSource::Output(output), args.cursor, Some(&consumer))
        .await
        .context("capture_source_dmabuf")?;
    tracing::info!(?frame, "dmabuf captured, pushing into gst pipeline");

    let path = args
        .file
        .unwrap_or_else(|| PathBuf::from(format!("./dmabuf-gst-{}.png", args.output)));

    gstreamer::init().context("gstreamer init")?;

    // Build the pipeline. `appsrc` produces our single dmabuf-backed
    // buffer; `vapostproc` imports the dmabuf via the GstVaDisplay it
    // shares with the AMD VA driver, converts to whatever sysmem format
    // `pngenc` can ingest, and `pngenc` writes the file.
    //
    // `num-buffers=1` on appsrc would normally signal EOS after one
    // buffer, but we control EOS explicitly via end_of_stream() — simpler
    // than fighting appsrc's automatic counting in the one-shot case.
    let location = path.to_string_lossy().to_string();
    let pipeline_str = format!(
        "appsrc name=src is-live=false format=time \
           ! vapostproc \
           ! video/x-raw,format=RGBA \
           ! videoconvert \
           ! pngenc \
           ! filesink location=\"{}\"",
        location.replace('"', "\\\""),
    );
    tracing::info!(%pipeline_str, "constructing debug gst pipeline");
    let element = gstreamer::parse::launch(&pipeline_str).context("parse_launch")?;
    let pipeline = element
        .downcast::<Pipeline>()
        .map_err(|_| anyhow::anyhow!("parse_launch did not return a Pipeline"))?;
    let appsrc = pipeline
        .by_name("src")
        .ok_or_else(|| anyhow::anyhow!("appsrc 'src' missing"))?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow::anyhow!("'src' element is not AppSrc"))?;

    // Build caps via VideoInfoDmaDrm — gst-rs takes care of the exact
    // structure name + drm-format spelling, including the modifier hex
    // padding rules. The fourcc passed to ::new is the DRM fourcc number
    // (not the gst VideoFormat enum); gst maps it internally for
    // downstream import.
    let vinfo = gstreamer_video::VideoInfo::builder(
        gstreamer_video::VideoFormat::Bgra,
        frame.width,
        frame.height,
    )
    .fps(gstreamer::Fraction::new(1, 1))
    .build()
    .map_err(|e| anyhow::anyhow!("VideoInfo::builder: {e}"))?;
    let dma_info = gstreamer_video::VideoInfoDmaDrm::new(
        vinfo,
        frame.fourcc as u32,
        frame.modifier,
    );
    let caps = dma_info
        .to_caps()
        .map_err(|e| anyhow::anyhow!("VideoInfoDmaDrm::to_caps: {e}"))?;
    appsrc.set_caps(Some(&caps));
    tracing::info!(caps = %caps, "appsrc caps set");

    // Build the GstBuffer wrapping the BO. dup the fd via fd_for_plane()
    // (gbm returns OwnedFd, which gst's DmaBufAllocator::alloc takes by
    // IntoRawFd — gst owns the fd from that point on). The BO can be
    // dropped before gst finishes; the dmabuf survives as long as gst's
    // dup'd fd is alive.
    let allocator = gstreamer_allocators::DmaBufAllocator::new();
    let plane_count = frame.bo.plane_count() as i32;
    // For tiled / compressed modifiers (AMD DCC) the dmabuf carries
    // auxiliary planes (DCC metadata, retile maps) packed into the same
    // underlying buffer at different offsets. We detect that case by
    // comparing the backing inode of each plane's fd: if they all point
    // to one dmabuf, we wrap it as a single GstMemory (vapostproc reads
    // the modifier from caps and derives aux-plane offsets itself). Only
    // if the planes are in genuinely separate dmabufs (rare for screen
    // capture) do we append one memory per fd.
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
    tracing::info!(
        plane_count,
        same_backing,
        "dmabuf plane layout"
    );

    let mut buf = Buffer::new();
    {
        let buf_mut = buf.get_mut().expect("fresh buffer is unique");
        if same_backing {
            // One physical dmabuf. Append a single memory spanning the
            // whole allocation; the modifier in caps tells vapostproc
            // where the aux planes live within it.
            let fd = plane_fds.into_iter().next().unwrap();
            let size = rustix::fs::seek(&fd, rustix::fs::SeekFrom::End(0))
                .context("lseek dmabuf fd to end")?;
            rustix::fs::seek(&fd, rustix::fs::SeekFrom::Start(0))
                .context("lseek dmabuf fd back to start")?;
            let mem = unsafe { allocator.alloc(fd, size as usize) }
                .map_err(|e| anyhow::anyhow!("DmaBufAllocator::alloc: {e}"))?;
            buf_mut.append_memory(mem);
        } else {
            for fd in plane_fds {
                let size = rustix::fs::seek(&fd, rustix::fs::SeekFrom::End(0))
                    .context("lseek dmabuf fd to end")?;
                rustix::fs::seek(&fd, rustix::fs::SeekFrom::Start(0))
                    .context("lseek dmabuf fd back to start")?;
                let mem = unsafe { allocator.alloc(fd, size as usize) }
                    .map_err(|e| anyhow::anyhow!("DmaBufAllocator::alloc: {e}"))?;
                buf_mut.append_memory(mem);
            }
        }
        // No VideoMeta: DMA_DRM caps carry the modifier, and vapostproc
        // derives everything (plane offsets, stride, aux-plane layout)
        // from that. Adding VideoMeta with mismatched info trips the
        // negotiation. If a downstream complains, the right path is a
        // `VideoMetaDmaDrm` (added in gst 1.24+) — TBD if needed.
        buf_mut.set_pts(ClockTime::ZERO);
        buf_mut.set_duration(ClockTime::from_seconds(1));
    }

    pipeline.set_state(State::Playing).context("→ Playing")?;
    appsrc
        .push_buffer(buf)
        .map_err(|e| anyhow::anyhow!("push_buffer: {e}"))?;
    let _ = appsrc.end_of_stream();

    let bus = pipeline.bus().context("pipeline bus")?;
    let mut result: Result<()> = Ok(());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            result = Err(anyhow::anyhow!("gst pipeline did not finish within 10s"));
            break;
        }
        let timeout = ClockTime::from_mseconds(remaining.as_millis() as u64);
        let Some(msg) = bus.timed_pop(Some(timeout)) else {
            continue;
        };
        match msg.view() {
            MessageView::Eos(_) => {
                tracing::info!("gst pipeline EOS");
                break;
            }
            MessageView::Error(e) => {
                result = Err(anyhow::anyhow!(
                    "gst error from {:?}: {} ({:?})",
                    e.src().map(|s| s.name().to_string()),
                    e.error(),
                    e.debug(),
                ));
                break;
            }
            MessageView::Warning(w) => {
                tracing::warn!(error = %w.error(), debug = ?w.debug(), "gst warning");
            }
            _ => {}
        }
    }
    let _ = pipeline.set_state(State::Null);
    result?;
    println!("{}", path.display());
    Ok(())
}

/// Continuous dmabuf recording of one output. Drives
/// `dmabuf_stream::start_dmabuf_for_output` for `duration` seconds and
/// feeds the resulting frame stream into `VideoSessionDmabuf`. The
/// produced mp4 should be visually correct (matches what's on the
/// captured display) and contain ≈ fps × duration frames.
pub async fn run_dmabuf_record(args: DebugDmabufRecordArgs) -> Result<()> {
    let conn = Connection::connect_to_env().context("wayland connect")?;
    let helper = WaylandHelper::new(conn).context("WaylandHelper::new")?;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify the output exists before we go through gst init etc.
    if helper.output_for_name(&args.output).is_none() {
        anyhow::bail!("no wayland output named {:?}", args.output);
    }

    let path = args
        .file
        .unwrap_or_else(|| PathBuf::from(format!("./dmabuf-record-{}.mp4", args.output)));
    let consumer = query_gst_consumer_formats("vapostproc")?;

    let (capture, fmt_rx, frame_rx) = dmabuf_stream::start_dmabuf_for_output(
        helper,
        args.output.clone(),
        args.cursor,
        args.fps,
        consumer,
    )
    .context("start_dmabuf_for_output")?;

    let format = tokio::time::timeout(std::time::Duration::from_secs(5), fmt_rx)
        .await
        .map_err(|_| anyhow::anyhow!("dmabuf stream did not negotiate format within 5s"))?
        .map_err(|_| anyhow::anyhow!("dmabuf stream dropped before first frame"))?;
    tracing::info!(?format, "dmabuf stream format negotiated");

    let session = VideoSessionDmabuf::build(
        Box::new(capture),
        format,
        &path,
        args.fps,
        VideoContainer::Mp4,
        /* audio */ false,
        None,
    )
    .context("VideoSessionDmabuf::build")?;

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let dur = args.duration;
    let stop_task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(dur)).await;
        let _ = stop_tx.send(());
    });

    session.run(stop_rx, frame_rx).await.context("session.run")?;
    let _ = stop_task.await;

    println!("{}", path.display());
    Ok(())
}

/// Cross-screen region recording. Finds every output that overlaps the
/// logical-coord `region`, starts one dmabuf stream per output, and
/// composites them through `vacompositor` into one mp4. The geometry
/// math here mirrors `gui::app::stitch_region_screenshot`'s overlap
/// loop — when this lands in production the two should share one
/// helper (`compute_region_parts`).
pub async fn run_dmabuf_record_multi(args: DebugDmabufRecordMultiArgs) -> Result<()> {
    let region = parse_region(&args.region)
        .with_context(|| format!("parse --region {:?}", args.region))?;
    let conn = Connection::connect_to_env().context("wayland connect")?;
    let helper = WaylandHelper::new(conn).context("WaylandHelper::new")?;
    // Wayland connection needs a moment to populate outputs.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Find overlapping outputs + their geometry. Capture per-output
    // metadata up front so we can compute per-branch crop/dst without
    // holding wayland handles in the gst phase.
    struct OverlapPart {
        name: String,
        // intersection of `region` and the output, in *output-local
        // physical* px — what vapostproc.crop wants.
        src_crop: CropRect,
        // intersection's position on the composite canvas, in
        // *canvas* px (`region.w * target_scale × region.h * target_scale`).
        dst_pos: (u32, u32),
        dst_size: (u32, u32),
    }

    let outputs = helper.outputs();
    let mut infos: Vec<(String, (i32, i32), (u32, u32), i32)> = Vec::new();
    for o in &outputs {
        let Some(info) = helper.output_info(o) else { continue };
        let Some(name) = info.name.clone() else { continue };
        let pos = info.logical_position.unwrap_or((0, 0));
        let size = info.logical_size.map(|(w, h)| (w as u32, h as u32)).unwrap_or((0, 0));
        if size.0 == 0 || size.1 == 0 {
            continue;
        }
        infos.push((name, pos, size, info.scale_factor.max(1)));
    }

    let target_scale = infos
        .iter()
        .filter(|(_, pos, size, _)| {
            let r_left = pos.0;
            let r_top = pos.1;
            let r_right = r_left + size.0 as i32;
            let r_bottom = r_top + size.1 as i32;
            region.0 < r_right
                && region.0 + region.2 as i32 > r_left
                && region.1 < r_bottom
                && region.1 + region.3 as i32 > r_top
        })
        .map(|(_, _, _, s)| *s as u32)
        .max()
        .unwrap_or(1);

    let mut parts: Vec<OverlapPart> = Vec::new();
    for (name, pos, size, scale) in infos {
        let r_left = pos.0;
        let r_top = pos.1;
        let r_right = r_left + size.0 as i32;
        let r_bottom = r_top + size.1 as i32;
        let ix_l = region.0.max(r_left);
        let iy_t = region.1.max(r_top);
        let ix_r = (region.0 + region.2 as i32).min(r_right);
        let iy_b = (region.1 + region.3 as i32).min(r_bottom);
        if ix_r <= ix_l || iy_b <= iy_t {
            continue;
        }
        // src crop in this output's physical pixels.
        let s = scale.max(1) as u32;
        let src_x = ((ix_l - r_left).max(0) as u32) * s;
        let src_y = ((iy_t - r_top).max(0) as u32) * s;
        let src_w = ((ix_r - ix_l) as u32) * s;
        let src_h = ((iy_b - iy_t) as u32) * s;
        // canvas position uses target_scale to keep cross-DPI rectangles
        // aligned, matching the screenshot stitch.
        let dst_x = ((ix_l - region.0) as u32) * target_scale;
        let dst_y = ((iy_t - region.1) as u32) * target_scale;
        let dst_w = ((ix_r - ix_l) as u32) * target_scale;
        let dst_h = ((iy_b - iy_t) as u32) * target_scale;
        parts.push(OverlapPart {
            name,
            src_crop: CropRect {
                x: src_x as i32,
                y: src_y as i32,
                w: src_w,
                h: src_h,
            },
            dst_pos: (dst_x, dst_y),
            dst_size: (dst_w, dst_h),
        });
    }
    anyhow::ensure!(
        !parts.is_empty(),
        "region {:?} doesn't overlap any known output",
        region
    );
    tracing::info!(
        target_scale,
        parts = ?parts.iter().map(|p| (&p.name, &p.src_crop, p.dst_pos, p.dst_size)).collect::<Vec<_>>(),
        "computed region parts"
    );

    let consumer = query_gst_consumer_formats("vapostproc")?;

    // Start one dmabuf stream per overlapping output and wait for all
    // of them to negotiate their format.
    let mut captures: Vec<Box<dyn std::any::Any + Send>> = Vec::new();
    let mut frame_rxs = Vec::new();
    let mut composite_parts = Vec::new();
    for part in parts {
        let (capture, fmt_rx, frame_rx) = dmabuf_stream::start_dmabuf_for_output(
            helper.clone(),
            part.name.clone(),
            args.cursor,
            args.fps,
            consumer.clone(),
        )
        .with_context(|| format!("start_dmabuf_for_output {}", part.name))?;
        let format = tokio::time::timeout(std::time::Duration::from_secs(5), fmt_rx)
            .await
            .map_err(|_| anyhow::anyhow!("stream {} did not negotiate format within 5s", part.name))?
            .map_err(|_| anyhow::anyhow!("stream {} dropped before first frame", part.name))?;
        tracing::info!(name = %part.name, ?format, "branch format negotiated");
        captures.push(Box::new(capture));
        frame_rxs.push(frame_rx);
        composite_parts.push(CompositePart {
            src_crop: part.src_crop,
            dst_pos: part.dst_pos,
            dst_size: part.dst_size,
            format,
        });
    }

    let path = args
        .file
        .unwrap_or_else(|| PathBuf::from("./dmabuf-record-multi.mp4"));
    let session = VideoSessionDmabufMulti::build(
        captures,
        composite_parts,
        &path,
        args.fps,
        VideoContainer::Mp4,
        /* audio */ false,
    )
    .context("VideoSessionDmabufMulti::build")?;

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let dur = args.duration;
    let stop_task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(dur)).await;
        let _ = stop_tx.send(());
    });
    session
        .run(stop_rx, frame_rxs)
        .await
        .context("session.run multi")?;
    let _ = stop_task.await;

    println!("{}", path.display());
    Ok(())
}

fn parse_region(s: &str) -> Result<(i32, i32, u32, u32)> {
    let nums: Vec<i64> = s
        .split(',')
        .map(|p| p.trim().parse::<i64>().context("region component"))
        .collect::<Result<_>>()?;
    anyhow::ensure!(nums.len() == 4, "--region expects X,Y,W,H (4 values)");
    anyhow::ensure!(nums[2] > 0 && nums[3] > 0, "region width/height must be > 0");
    Ok((nums[0] as i32, nums[1] as i32, nums[2] as u32, nums[3] as u32))
}
