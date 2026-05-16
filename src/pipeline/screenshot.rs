use std::fs;
use std::os::unix::fs::MetadataExt;

use anyhow::{Context, Result};
use tokio::time::{sleep, Duration};

use crate::capture::screenshot::{self, CaptureCancelled};
use crate::cli::ScreenshotArgs;
use crate::{notify, paths};

pub async fn run(args: ScreenshotArgs) -> Result<()> {
    if args.delay_ms > 0 {
        sleep(Duration::from_millis(args.delay_ms)).await;
    }

    let captured = match screenshot::take(args.interactive, args.modal).await {
        Ok(c) => c,
        Err(e) if e.downcast_ref::<CaptureCancelled>().is_some() => {
            tracing::info!("cancelled by user");
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    if captured.clipboard {
        println!("(saved to clipboard)");
        return Ok(());
    }

    let dest = paths::resolve(args.common.file.clone(), paths::Kind::Image, "png")?;
    // Same-device rename, cross-device copy + remove — matches cosmic-screenshot's logic.
    let src_dev = fs::metadata(&captured.source_path)
        .with_context(|| format!("stat {}", captured.source_path.display()))?
        .dev();
    let dest_dev = fs::metadata(dest.parent().unwrap_or(std::path::Path::new("/")))
        .with_context(|| format!("stat {}", dest.display()))?
        .dev();
    if src_dev == dest_dev {
        fs::rename(&captured.source_path, &dest)
            .with_context(|| format!("rename {} → {}", captured.source_path.display(), dest.display()))?;
    } else {
        fs::copy(&captured.source_path, &dest)
            .with_context(|| format!("copy {} → {}", captured.source_path.display(), dest.display()))?;
        let _ = fs::remove_file(&captured.source_path);
    }

    println!("{}", dest.display());

    if args.common.clipboard {
        // TODO: wl-clipboard or wayland clipboard protocol. Deferred.
        tracing::warn!("--clipboard not yet implemented");
    }
    if args.common.notify {
        if let Err(e) = notify::saved(&dest, "Screenshot").await {
            tracing::warn!(error = %e, "failed to send notification");
        }
    }
    Ok(())
}
