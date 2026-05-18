//! Continuous cosmic-screencopy → frame stream for both toplevels and
//! outputs.
//!
//! Plugs into `VideoSession` / `GifSession` the same way
//! `pipewire_capture::start` does: returns a drop-to-stop handle, a
//! oneshot that resolves once we know the frame format, and an mpsc
//! carrying frames in arrival order.
//!
//! Implementation: a tokio task drives `WaylandHelper::capture_source_shm`
//! in a loop, paced to roughly `fps`. Each call creates and tears down a
//! `CaptureSession` — wasteful vs. one session many frames, but simple
//! and correct against the cosmic-screencopy state machine and lets us
//! reuse all the format-negotiation + memfd plumbing already in
//! `WaylandHelper`.
//!
//! Used for window recording AND for display/region recording: the
//! xdg-desktop-portal `ScreenCast` path cannot be told which monitor to
//! pick (the portal caches the user's prior pick in a restore token),
//! which surfaces as "Region recording records the wrong display". Going
//! direct to screencopy lets us name the exact output ourselves.
//!
//! The cosmic-toolkit protocol *does* allow one session many frames; a
//! follow-up can hoist `create_session` / `capture_session.capture(..)`
//! into a per-recording loop for lower overhead.

use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use cosmic_client_toolkit::screencopy::CaptureSource;
use libspa::param::video::VideoFormat as SpaVideoFormat;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use wayland_client::protocol::wl_output;

use super::pipewire_capture::{Frame, StreamFormat};
use super::wayland::WaylandHelper;

/// Drop to stop recording. Sends the stop signal and aborts the task.
/// (The task itself checks the stop flag between captures; aborting on
/// drop is the belt to the suspenders so a wedged session can't outlive
/// the recording.)
pub struct Capture {
    stop_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Capture {
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start a continuous capture against the toplevel identified by `id`.
/// `fps` paces the loop; cosmic-comp emits frames on demand so we ask
/// for one every ~1/fps seconds. The first successful capture seeds the
/// returned `StreamFormat`.
pub fn start(
    helper: WaylandHelper,
    id: String,
    overlay_cursor: bool,
    fps: u32,
) -> Result<(Capture, oneshot::Receiver<StreamFormat>, mpsc::Receiver<Frame>)> {
    let handle = helper
        .toplevel_handle_for_identifier(&id)
        .ok_or_else(|| anyhow!("no toplevel with identifier {id:?}"))?;
    Ok(start_inner(
        helper,
        CaptureSource::Toplevel(handle),
        "toplevel",
        overlay_cursor,
        fps,
    ))
}

/// Start a continuous capture against the named output (display). Same
/// shape as `start` for toplevels. Used instead of the screencast portal
/// so the caller can pin recording to a specific monitor — the portal's
/// restore-token machinery silently substitutes whichever output the
/// user picked the first time, which is the wrong-display bug.
pub fn start_for_output(
    helper: WaylandHelper,
    output_name: String,
    overlay_cursor: bool,
    fps: u32,
) -> Result<(Capture, oneshot::Receiver<StreamFormat>, mpsc::Receiver<Frame>)> {
    let output: wl_output::WlOutput = helper
        .output_for_name(&output_name)
        .ok_or_else(|| anyhow!("no output named {output_name:?}"))?;
    Ok(start_inner(
        helper,
        CaptureSource::Output(output),
        "output",
        overlay_cursor,
        fps,
    ))
}

fn start_inner(
    helper: WaylandHelper,
    source: CaptureSource,
    label: &'static str,
    overlay_cursor: bool,
    fps: u32,
) -> (Capture, oneshot::Receiver<StreamFormat>, mpsc::Receiver<Frame>) {
    let (fmt_tx, fmt_rx) = oneshot::channel::<StreamFormat>();
    let (frame_tx, frame_rx) = mpsc::channel::<Frame>(16);
    let (stop_tx, stop_rx) = oneshot::channel::<()>();

    let fps = fps.max(1);
    let frame_budget = std::time::Duration::from_nanos(1_000_000_000 / fps as u64);

    let task = tokio::spawn(run_loop(
        helper,
        source,
        label,
        overlay_cursor,
        fps,
        frame_budget,
        fmt_tx,
        frame_tx,
        stop_rx,
    ));

    (
        Capture {
            stop_tx: Some(stop_tx),
            task: Some(task),
        },
        fmt_rx,
        frame_rx,
    )
}

async fn run_loop(
    helper: WaylandHelper,
    source: CaptureSource,
    label: &'static str,
    overlay_cursor: bool,
    fps: u32,
    frame_budget: std::time::Duration,
    fmt_tx: oneshot::Sender<StreamFormat>,
    frame_tx: mpsc::Sender<Frame>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    let started = Instant::now();
    let mut fmt_tx = Some(fmt_tx);
    let mut next_deadline = Instant::now();

    loop {
        // Check stop signal between frames so the loop exits promptly
        // when the GUI drops the Capture handle.
        if matches!(stop_rx.try_recv(), Ok(()) | Err(oneshot::error::TryRecvError::Closed)) {
            tracing::info!(label, "screencopy_capture: stop signalled");
            return;
        }

        let source = source.clone();
        let frame_start = Instant::now();
        let captured = match helper.capture_source_shm(source, overlay_cursor).await {
            Some(c) => c,
            None => {
                // A single failed capture isn't fatal — toplevel could
                // be temporarily unmapped (a workspace switch, drag-
                // resize, etc.). Sleep one frame budget and retry.
                tokio::time::sleep(frame_budget).await;
                continue;
            }
        };

        if let Some(tx) = fmt_tx.take() {
            let fmt = StreamFormat {
                width: captured.width,
                height: captured.height,
                fps,
                // cosmic-screencopy delivers Abgr8888 — in memory that's
                // R, G, B, A bytes, which gst calls "RGBA".
                format: SpaVideoFormat::RGBA,
                stride: captured.stride,
            };
            if tx.send(fmt).is_err() {
                tracing::warn!("toplevel_capture: fmt rx dropped before first frame");
                return;
            }
        }

        let pts_ns = started.elapsed().as_nanos() as u64;
        let frame = Frame {
            bytes: captured.pixels,
            pts_ns,
        };
        if frame_tx.send(frame).await.is_err() {
            tracing::info!("toplevel_capture: frame rx dropped — stopping");
            return;
        }

        // Pace to ~fps. cosmic-comp returns capture frames as fast as it
        // can; without this we'd burn CPU producing more frames than
        // we can encode.
        next_deadline += frame_budget;
        let now = Instant::now();
        if next_deadline > now {
            tokio::time::sleep(next_deadline - now).await;
        } else {
            // Fell behind — resync the deadline to "now" so we don't
            // accumulate catch-up captures.
            next_deadline = now;
        }
        let _ = frame_start; // keep for future per-frame timing
    }
}

#[allow(dead_code)]
fn _result_marker() -> Result<()> {
    Err(anyhow!("never called")).context("link Result + anyhow into the dep graph")
}
