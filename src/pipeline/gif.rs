use std::path::PathBuf;

use anyhow::Result;

use crate::capture::screencast;
use crate::cli::GifArgs;
use crate::encode::gif::GifSession;
use crate::encode::video::CropRect;
use crate::selector::{self, SelectionCancelled};
use crate::{notify, paths};

pub async fn gif_with_crop(
    args: GifArgs,
    crop: Option<CropRect>,
) -> Result<PathBuf> {
    let stream = screencast::start(args.cursor).await?;
    tracing::info!(node = stream.node_id, size = ?stream.size, "ScreenCast started (gif)");

    let path = paths::resolve(args.common.file.clone(), paths::Kind::Image, "gif")?;
    let session = GifSession::build(
        stream,
        &path,
        args.fps,
        args.quality,
        args.max_width,
        crop,
    )?;
    session.run(args.duration_secs).await?;

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

    let path = gif_with_crop(args, crop).await?;
    drop(_overlay);

    println!("{}", path.display());
    Ok(())
}
