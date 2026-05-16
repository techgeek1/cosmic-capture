use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Local;

pub enum Kind {
    Image,
    Video,
}

pub fn resolve(user: Option<PathBuf>, kind: Kind, ext: &str) -> Result<PathBuf> {
    if let Some(p) = user {
        return Ok(p);
    }
    let dir = match kind {
        Kind::Image => dirs::picture_dir(),
        Kind::Video => dirs::video_dir(),
    }
    .or_else(|| dirs::home_dir())
    .context("could not locate home directory")?;
    std::fs::create_dir_all(&dir).ok();
    let stamp = Local::now().format("%Y%m%d-%H%M%S");
    Ok(dir.join(format!("cosmic-capture-{stamp}.{ext}")))
}

pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("no XDG config dir")?;
    let dir = base.join("cosmic-capture");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create config dir {}", dir.display()))?;
    Ok(dir)
}
