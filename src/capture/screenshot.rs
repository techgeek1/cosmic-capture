//! Screenshot capture entry-point.
//!
//! Just a thin wrapper around [`crate::capture::screencopy`] — runs the
//! blocking screencopy call on a tokio blocking thread so it doesn't park
//! the GUI event loop.

use anyhow::Result;

use super::screencopy::{self, CapturedFrame};

pub use super::screencopy::Target;

pub async fn take(target: Target, with_cursor: bool) -> Result<CapturedFrame> {
    tokio::task::spawn_blocking(move || screencopy::capture(target, with_cursor))
        .await
        .map_err(|e| anyhow::anyhow!("screencopy task join: {e}"))?
}
