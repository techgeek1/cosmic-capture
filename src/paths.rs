use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Local;

pub enum Kind {
    /// Still image — lands in `~/Pictures/Screenshots/Screenshot-<stamp>.<ext>`.
    Screenshot,
    /// Recording (video or animated GIF) — lands in
    /// `~/Videos/Captures/Capture-<stamp>.<ext>`. GIF is a recording even
    /// though its extension would suggest "image"; it has temporal content
    /// and users look for it where they look for clips.
    Recording,
}

pub fn resolve(user: Option<PathBuf>, kind: Kind, ext: &str) -> Result<PathBuf> {
    if let Some(p) = user {
        return Ok(p);
    }
    let (base, sub, prefix) = match kind {
        Kind::Screenshot => (dirs::picture_dir(), "Screenshots", "Screenshot"),
        Kind::Recording => (dirs::video_dir(), "Captures", "Capture"),
    };
    let dir = base
        .or_else(dirs::home_dir)
        .context("could not locate home directory")?
        .join(sub);
    std::fs::create_dir_all(&dir).ok();
    let stamp = Local::now().format("%Y-%m-%d_%H-%M-%S");
    Ok(dir.join(format!("{prefix}-{stamp}.{ext}")))
}

pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("no XDG config dir")?;
    let dir = base.join("cosmic-capture");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create config dir {}", dir.display()))?;
    Ok(dir)
}
