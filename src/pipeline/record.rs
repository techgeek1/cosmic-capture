use std::path::PathBuf;

use anyhow::Result;
use tokio::sync::oneshot;

use crate::capture::screencast;
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
    let stream = screencast::start(args.cursor).await?;
    tracing::info!(
        node = stream.node_id,
        size = ?stream.size,
        "ScreenCast started"
    );

    let path = paths::resolve(
        args.common.file.clone(),
        paths::Kind::Video,
        args.container.extension(),
    )?;

    let session = VideoSession::build(
        stream,
        &path,
        args.fps,
        args.container,
        args.encoder,
        args.audio,
        crop,
    )?;

    session.run(stop_rx).await?;

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
