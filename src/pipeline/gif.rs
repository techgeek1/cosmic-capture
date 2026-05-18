use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::sync::oneshot;

use crate::capture::wayland::WaylandHelper;
use crate::capture::{pipewire_capture, screencast, toplevel_capture};
use crate::cli::GifArgs;
use crate::encode::gif::GifSession;
use crate::encode::video::CropRect;
use crate::pipeline::record::ScreencopyTarget;
use crate::selector::{self, SelectionCancelled};
use crate::{notify, paths};

pub async fn gif_with_crop(
    args: GifArgs,
    crop: Option<CropRect>,
    stop_rx: oneshot::Receiver<()>,
) -> Result<PathBuf> {
    let stream = screencast::start(args.cursor).await?;
    tracing::info!(node = stream.node_id, size = ?stream.size, "ScreenCast started (gif)");

    // Same direct-libpipewire path as the record flow — bypasses
    // gst-plugin-pipewire's pwsrc caps-fixed assertion.
    let (capture, fmt_rx, frame_rx) =
        pipewire_capture::start(stream.fd, stream.node_id)?;
    let format = tokio::time::timeout(std::time::Duration::from_secs(5), fmt_rx)
        .await
        .map_err(|_| anyhow::anyhow!("pipewire stream did not negotiate format within 5s (gif)"))?
        .map_err(|_| anyhow::anyhow!("pipewire capture thread dropped before format arrived (gif)"))?;
    tracing::info!(?format, "pipewire format ready, building gif pipeline");

    let path = paths::resolve(args.common.file.clone(), paths::Kind::Recording, "gif")?;
    let session = GifSession::build(
        Box::new(capture),
        format,
        &path,
        args.fps,
        args.quality,
        args.max_width,
        crop,
    )?;
    session.run(stop_rx, frame_rx).await?;

    if args.common.notify {
        if let Err(e) = notify::saved(&path, "GIF").await {
            tracing::warn!(error = %e, "failed to send notification");
        }
    }
    Ok(path)
}

/// GIF counterpart to `record_via_screencopy`. Same routing rules: an
/// `Output` target may carry a crop (in output-local physical pixels);
/// a `Toplevel` target ignores any crop (the window's own bounds are
/// the crop).
pub async fn gif_via_screencopy(
    helper: WaylandHelper,
    target: ScreencopyTarget,
    args: GifArgs,
    crop: Option<CropRect>,
    stop_rx: oneshot::Receiver<()>,
) -> Result<PathBuf> {
    tracing::info!(?target, fps = args.fps, ?crop, "gif_via_screencopy: starting");

    let (capture, fmt_rx, frame_rx, allow_crop) = match &target {
        ScreencopyTarget::Output { output_name } => {
            let (c, f, frx) = toplevel_capture::start_for_output(
                helper,
                output_name.clone(),
                args.cursor,
                args.fps,
            )
            .context("start screencopy for output (gif)")?;
            (c, f, frx, true)
        }
        ScreencopyTarget::Toplevel { identifier } => {
            let (c, f, frx) = toplevel_capture::start(
                helper,
                identifier.clone(),
                args.cursor,
                args.fps,
            )
            .context("start screencopy for toplevel (gif)")?;
            (c, f, frx, false)
        }
    };

    let format = tokio::time::timeout(std::time::Duration::from_secs(5), fmt_rx)
        .await
        .map_err(|_| anyhow::anyhow!("screencopy did not produce a frame within 5s (gif)"))?
        .map_err(|_| anyhow::anyhow!("screencopy capture dropped before first frame (gif)"))?;
    tracing::info!(?format, "screencopy format ready (gif)");

    let path = paths::resolve(args.common.file.clone(), paths::Kind::Recording, "gif")?;
    let session = GifSession::build(
        Box::new(capture),
        format,
        &path,
        args.fps,
        args.quality,
        args.max_width,
        if allow_crop { crop } else { None },
    )?;
    session.run(stop_rx, frame_rx).await?;

    if args.common.notify {
        if let Err(e) = notify::saved(&path, "GIF").await {
            tracing::warn!(error = %e, "failed to send notification");
        }
    }
    Ok(path)
}

pub async fn run(args: GifArgs) -> Result<()> {
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

    // CLI path: bound the recording by either the requested duration or a
    // Ctrl-C. Send the stop signal from a sidecar task so the gif session
    // sees a clean stop event identical to the GUI's path.
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let dur = args.duration_secs;
    let stopper = tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(dur)) => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        let _ = stop_tx.send(());
    });

    let path = gif_with_crop(args, crop, stop_rx).await?;
    let _ = stopper.await;
    drop(_overlay);

    println!("{}", path.display());
    Ok(())
}
