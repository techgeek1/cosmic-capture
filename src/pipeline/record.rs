use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::sync::oneshot;

use crate::capture::dmabuf_stream;
use crate::capture::pipewire_capture;
use crate::capture::screencast;
use crate::capture::wayland::WaylandHelper;
use crate::cli::RecordArgs;
use crate::encode::video::{CropRect, VideoSession};
use crate::encode::video_dmabuf::{
    query_gst_consumer_formats, CompositePart, VideoSessionDmabuf, VideoSessionDmabufMulti,
};
use crate::selector::{self, SelectionCancelled};
use crate::{notify, paths};

/// Record-only path: assumes the caller has already picked the region (or
/// chose to capture full-output) and will independently fire `stop_rx` when
/// it's time to stop. Used by both the CLI's `run` and the GUI panel.
pub async fn record_with_crop(
    args: RecordArgs,
    crop: Option<CropRect>,
    stop_rx: oneshot::Receiver<()>,
) -> Result<PathBuf> {
    tracing::info!(
        container = ?args.container,
        fps = args.fps,
        encoder = ?args.encoder,
        ?crop,
        "record_with_crop: entering screencast start"
    );
    let stream = screencast::start(args.cursor).await?;
    tracing::info!(
        node = stream.node_id,
        size = ?stream.size,
        "ScreenCast started"
    );

    // Hand the PipeWire fd off to our own consumer thread instead of
    // gst-plugin-pipewire. The thread negotiates a fixed video format
    // with the compositor, then streams raw frames to us via `frame_rx`.
    let (capture, fmt_rx, frame_rx) =
        pipewire_capture::start(stream.fd, stream.node_id)?;
    let format = tokio::time::timeout(std::time::Duration::from_secs(5), fmt_rx)
        .await
        .map_err(|_| anyhow::anyhow!("pipewire stream did not negotiate format within 5s"))?
        .map_err(|_| anyhow::anyhow!("pipewire capture thread dropped before format arrived"))?;
    tracing::info!(?format, "pipewire format ready, building gst pipeline");

    let path = paths::resolve(
        args.common.file.clone(),
        paths::Kind::Recording,
        args.container.extension(),
    )?;

    let session = VideoSession::build(
        Box::new(capture),
        format,
        &path,
        args.fps,
        args.container,
        args.encoder,
        args.mic,
        args.system_audio,
        crop,
    )?;

    session.run(stop_rx, frame_rx).await?;

    if args.common.notify {
        if let Err(e) = notify::saved(&path, "Recording").await {
            tracing::warn!(error = %e, "failed to send notification");
        }
    }
    Ok(path)
}

/// Describes which cosmic-screencopy source feeds a GUI-initiated
/// recording. The GUI always knows the exact monitor (or toplevel) it
/// wants — going via the screencast portal would defer the choice to
/// the portal's restore-token, which is what produced the wrong-display
/// bug for region/screen recordings.
#[derive(Debug, Clone)]
pub enum ScreencopyTarget {
    /// Capture a named output. `output_name` matches the wayland output
    /// name (e.g. "DP-1"). When paired with a `CropRect`, the crop is
    /// in output-local physical pixels.
    Output { output_name: String },
    /// Capture a single toplevel by its stable
    /// `ext_foreign_toplevel_list_v1` identifier. Crop is unsupported —
    /// the toplevel's own bounds *are* the crop.
    Toplevel { identifier: String },
}

/// Record a single source via cosmic-screencopy (dmabuf path), bypassing
/// the screencast portal. Used by every GUI-initiated recording
/// (region, display, and window) so the user's pick lands on the exact
/// monitor or window they chose. Zero CPU touch on pixels:
/// cosmic-screencopy writes into a GBM-allocated dmabuf, `vapostproc`
/// imports it as a VA surface, `vah264enc` encodes the VA surface, the
/// muxer writes the file.
///
/// The CLI path (`run`) stays on the portal so the user can pick a
/// source with the portal's own UI.
pub async fn record_via_screencopy(
    helper: WaylandHelper,
    target: ScreencopyTarget,
    args: RecordArgs,
    crop: Option<CropRect>,
    stop_rx: oneshot::Receiver<()>,
) -> Result<PathBuf> {
    tracing::info!(
        ?target,
        container = ?args.container,
        fps = args.fps,
        ?crop,
        "record_via_screencopy: starting (dmabuf)"
    );

    // Negotiate (fourcc, modifier) with what vapostproc can import.
    // Without this, cosmic-comp gives us the DCC-retile variant and
    // vapostproc rejects negotiation.
    let consumer = query_gst_consumer_formats("vapostproc")
        .context("query vapostproc dmabuf formats")?;

    let (capture, fmt_rx, frame_rx, allow_crop) = match &target {
        ScreencopyTarget::Output { output_name } => {
            let (c, f, frx) = dmabuf_stream::start_dmabuf_for_output(
                helper,
                output_name.clone(),
                args.cursor,
                args.fps,
                consumer,
            )
            .context("start dmabuf screencopy for output")?;
            (c, f, frx, true)
        }
        ScreencopyTarget::Toplevel { identifier } => {
            let (c, f, frx) = dmabuf_stream::start_dmabuf_for_toplevel(
                helper,
                identifier.clone(),
                args.cursor,
                args.fps,
                consumer,
            )
            .context("start dmabuf screencopy for toplevel")?;
            (c, f, frx, false)
        }
    };

    // First frame seeds the negotiated format. cosmic-comp typically
    // responds in <50ms; 5s matches the deadline screencast::start uses.
    let format = tokio::time::timeout(std::time::Duration::from_secs(5), fmt_rx)
        .await
        .map_err(|_| anyhow::anyhow!("screencopy did not produce a frame within 5s"))?
        .map_err(|_| anyhow::anyhow!("screencopy capture dropped before first frame"))?;
    tracing::info!(?format, "dmabuf stream format ready");

    let path = paths::resolve(
        args.common.file.clone(),
        paths::Kind::Recording,
        args.container.extension(),
    )?;

    let session = VideoSessionDmabuf::build(
        Box::new(capture),
        format,
        &path,
        args.fps,
        args.container,
        args.mic,
        args.system_audio,
        if allow_crop { crop } else { None },
    )?;

    session.run(stop_rx, frame_rx).await?;

    if args.common.notify {
        if let Err(e) = notify::saved(&path, "Recording").await {
            tracing::warn!(error = %e, "failed to send notification");
        }
    }
    Ok(path)
}

/// One overlapping output in a cross-screen region recording. The
/// geometry math (output-local physical crop + canvas-coord placement)
/// matches `gui::app::stitch_region_screenshot`'s overlap loop; both
/// callers use `gui::app::compute_region_parts` to derive these values.
#[derive(Clone, Debug)]
pub struct RegionPart {
    pub output_name: String,
    /// Source crop in this output's physical pixels.
    pub src_crop: CropRect,
    /// Top-left of this output's contribution within the composite
    /// canvas, in canvas pixels.
    pub dst_pos: (u32, u32),
    /// Size of this output's contribution within the composite canvas,
    /// in canvas pixels. Caller scales by `target_scale` to align cross-
    /// DPI displays.
    pub dst_size: (u32, u32),
}

/// Record a region that spans multiple outputs. Each overlapping output
/// is captured to its own dmabuf stream, cropped on the GPU via
/// `vapostproc` + `GstVideoCropMeta`, and composited by `vacompositor`
/// into a single VA surface before encoding.
pub async fn record_via_screencopy_multi(
    helper: WaylandHelper,
    parts: Vec<RegionPart>,
    args: RecordArgs,
    stop_rx: oneshot::Receiver<()>,
) -> Result<PathBuf> {
    anyhow::ensure!(
        !parts.is_empty(),
        "record_via_screencopy_multi called with no overlapping outputs"
    );
    tracing::info!(
        container = ?args.container,
        fps = args.fps,
        part_count = parts.len(),
        "record_via_screencopy_multi: starting"
    );

    let consumer = query_gst_consumer_formats("vapostproc")
        .context("query vapostproc dmabuf formats")?;

    // Start one dmabuf stream per overlapping output, then await each
    // first-frame format negotiation. The streams pace themselves to
    // `args.fps`; vacompositor handles per-pad timing alignment.
    let mut captures: Vec<Box<dyn std::any::Any + Send>> = Vec::with_capacity(parts.len());
    let mut frame_rxs = Vec::with_capacity(parts.len());
    let mut composite_parts = Vec::with_capacity(parts.len());
    for part in parts {
        let (capture, fmt_rx, frame_rx) = dmabuf_stream::start_dmabuf_for_output(
            helper.clone(),
            part.output_name.clone(),
            args.cursor,
            args.fps,
            consumer.clone(),
        )
        .with_context(|| format!("start dmabuf stream for {}", part.output_name))?;
        let format = tokio::time::timeout(std::time::Duration::from_secs(5), fmt_rx)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "stream {} did not negotiate format within 5s",
                    part.output_name
                )
            })?
            .map_err(|_| {
                anyhow::anyhow!("stream {} dropped before first frame", part.output_name)
            })?;
        tracing::info!(name = %part.output_name, ?format, "branch format negotiated");
        captures.push(Box::new(capture));
        frame_rxs.push(frame_rx);
        composite_parts.push(CompositePart {
            src_crop: part.src_crop,
            dst_pos: part.dst_pos,
            dst_size: part.dst_size,
            format,
        });
    }

    let path = paths::resolve(
        args.common.file.clone(),
        paths::Kind::Recording,
        args.container.extension(),
    )?;
    let session = VideoSessionDmabufMulti::build(
        captures,
        composite_parts,
        &path,
        args.fps,
        args.container,
        args.mic,
        args.system_audio,
    )?;
    session.run(stop_rx, frame_rxs).await?;

    if args.common.notify {
        if let Err(e) = notify::saved(&path, "Recording").await {
            tracing::warn!(error = %e, "failed to send notification");
        }
    }
    Ok(path)
}

pub async fn run(args: RecordArgs) -> Result<()> {
    // `_overlay` keeps the recording border visible until it's dropped at
    // the end of this function (after the gst pipeline tears down).
    let (crop, _overlay) = if args.full {
        (None, None)
    } else {
        match selector::run().await {
            Ok((sel, overlay)) => {
                tracing::info!(
                    output = %sel.output_name,
                    region = ?sel.region_physical(),
                    "region selected"
                );
                let (x, y, w, h) = sel.region_physical();
                (Some(CropRect { x, y, w, h }), Some(overlay))
            }
            Err(e) if e.downcast_ref::<SelectionCancelled>().is_some() => {
                tracing::info!("region selection cancelled");
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    };

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let dur = args.duration_secs;
    let stop_task = tokio::spawn(async move {
        match dur {
            Some(secs) => {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            None => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        let _ = stop_tx.send(());
    });

    let path = record_with_crop(args, crop, stop_rx).await?;
    let _ = stop_task.await;
    drop(_overlay);

    println!("{}", path.display());
    Ok(())
}
