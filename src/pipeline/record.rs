use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::sync::oneshot;

use crate::capture::pipewire_capture;
use crate::capture::screencast;
use crate::capture::toplevel_capture;
use crate::capture::wayland::WaylandHelper;
use crate::cli::RecordArgs;
use crate::encode::video::{CropRect, VideoSession};
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
        args.audio,
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

/// Record a single toplevel by its stable `identifier`. Bypasses the
/// screencast portal entirely — frames come from cosmic-screencopy
/// driven by `toplevel_capture`, fed into the same VideoSession encoder
/// pipeline as the screen-record path. Crop is unsupported (the
/// toplevel's own bounds *are* the crop).
pub async fn record_toplevel(
    helper: WaylandHelper,
    identifier: String,
    args: RecordArgs,
    stop_rx: oneshot::Receiver<()>,
) -> Result<PathBuf> {
    tracing::info!(
        identifier = %identifier,
        container = ?args.container,
        fps = args.fps,
        encoder = ?args.encoder,
        "record_toplevel: starting"
    );

    let (capture, fmt_rx, frame_rx) =
        toplevel_capture::start(helper, identifier.clone(), args.cursor, args.fps)
            .context("start toplevel_capture")?;

    // First frame seeds the StreamFormat. cosmic-comp typically responds
    // in <50ms; 5s is the same generous deadline screencast::start uses
    // for portal-side format negotiation.
    let format = tokio::time::timeout(std::time::Duration::from_secs(5), fmt_rx)
        .await
        .map_err(|_| anyhow::anyhow!("toplevel screencopy did not produce a frame within 5s"))?
        .map_err(|_| anyhow::anyhow!("toplevel_capture dropped before first frame"))?;
    tracing::info!(?format, "toplevel screencopy format ready");

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
        args.audio,
        None,
    )?;

    session.run(stop_rx, frame_rx).await?;

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
