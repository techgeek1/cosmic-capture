//! Interactive region selector + persistent recording overlay.
//!
//! The selector has two phases sharing a single wayland session:
//!
//! 1. **Selecting**: full-output layer-shell surfaces with a dim cutout and
//!    rounded border, snappy `blocking_dispatch` event loop.
//! 2. **Recording** (post-commit): all surfaces destroyed except the one
//!    the drag landed on; that one is repainted with only the border (no
//!    dim), made input-transparent, and the loop switches to polled
//!    dispatch so the recording pipeline can tear it down on demand.
//!
//! Callers receive a [`Selection`] for the rect and an [`Overlay`] handle
//! that keeps the border visible for the lifetime of the handle.

mod app;
mod render;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;

#[derive(Clone, Debug)]
pub struct Selection {
    pub output_name: String,
    pub output_logical_pos: (i32, i32),
    pub output_logical_size: (u32, u32),
    pub region_logical: (i32, i32, u32, u32),
    pub scale: f64,
}

impl Selection {
    pub fn region_physical(&self) -> (i32, i32, u32, u32) {
        let s = self.scale;
        let (x, y, w, h) = self.region_logical;
        (
            (x as f64 * s).round() as i32,
            (y as f64 * s).round() as i32,
            (w as f64 * s).round().max(1.0) as u32,
            (h as f64 * s).round().max(1.0) as u32,
        )
    }
}

#[derive(Debug, thiserror::Error)]
#[error("region selection cancelled")]
pub struct SelectionCancelled;

/// Keeps the recording-mode border visible. Drop to tear it down.
pub struct Overlay {
    stop_flag: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            // Best-effort join. Wayland teardown should complete in < 100ms.
            let _ = t.join();
        }
    }
}

pub async fn run() -> Result<(Selection, Overlay)> {
    let (sel_tx, sel_rx) =
        std::sync::mpsc::sync_channel::<Result<Selection>>(1);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_thread = stop_flag.clone();
    let sel_tx_err = sel_tx.clone();

    let thread = std::thread::spawn(move || {
        if let Err(e) = app::run_blocking(sel_tx, stop_flag_thread) {
            // Surface fatal errors from the wayland thread back to caller.
            let _ = sel_tx_err.send(Err(e));
        }
    });

    let selection = tokio::task::spawn_blocking(move || {
        sel_rx
            .recv()
            .map_err(|e| anyhow::anyhow!("selector channel closed: {e}"))?
    })
    .await
    .map_err(|e| anyhow::anyhow!("selector recv task: {e}"))??;

    Ok((
        selection,
        Overlay { stop_flag, thread: Some(thread) },
    ))
}
