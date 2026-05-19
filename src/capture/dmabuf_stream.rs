//! Continuous dmabuf-backed screencopy stream — the dmabuf-path
//! counterpart to `toplevel_capture::start_for_output`.
//!
//! Each iteration of the inner loop allocates a fresh GBM BO, hands it
//! to cosmic-comp via linux-dmabuf-v1, awaits the screencopy `ready`
//! callback, then forwards the BO + metadata as a `DmabufStreamFrame`
//! over a tokio mpsc to the encoder pipeline. The encoder side wraps
//! the BO's fd in a `DmaBufAllocator` GstMemory — zero CPU touch from
//! capture to encode.
//!
//! Pool reuse is *not* implemented yet: every frame allocates a new
//! BO and destroys the wl_buffer right after the capture roundtrip.
//! GBM allocation on AMD is ~hundreds of microseconds, fine for the
//! current target of 30-60 fps but worth optimizing if we ever push
//! that range.

use std::time::Instant;

use anyhow::{Result, anyhow};
use cosmic_client_toolkit::screencopy::CaptureSource;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use wayland_client::protocol::wl_output;

use super::dmabuf::DmabufFrame;
use super::wayland::WaylandHelper;

/// Drop-to-stop handle. Same shape as `toplevel_capture::Capture` so
/// callers can hold either erased behind `Box<dyn Any + Send>` without
/// caring which capture variant produced them.
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

/// First-frame negotiated format. Sent over the `oneshot` once the
/// initial screencopy capture returns; subsequent frames are assumed to
/// keep the same format/modifier and are bailed if they don't (cosmic-
/// comp doesn't change mid-stream during normal operation).
#[derive(Debug, Clone)]
pub struct DmabufStreamFormat {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub fps: u32,
}

pub struct DmabufStreamFrame {
    pub frame: DmabufFrame,
    /// Presentation timestamp in nanoseconds since stream start. The gst
    /// pump translates this into `GstBuffer::set_pts`.
    pub pts_ns: u64,
}

pub fn start_dmabuf_for_output(
    helper: WaylandHelper,
    output_name: String,
    overlay_cursor: bool,
    fps: u32,
    consumer_formats: Vec<(u32, Vec<u64>)>,
) -> Result<(
    Capture,
    oneshot::Receiver<DmabufStreamFormat>,
    mpsc::Receiver<DmabufStreamFrame>,
)> {
    let output: wl_output::WlOutput = helper
        .output_for_name(&output_name)
        .ok_or_else(|| anyhow!("no output named {output_name:?}"))?;
    Ok(start_inner(
        helper,
        CaptureSource::Output(output),
        "output",
        overlay_cursor,
        fps,
        consumer_formats,
    ))
}

pub fn start_dmabuf_for_toplevel(
    helper: WaylandHelper,
    identifier: String,
    overlay_cursor: bool,
    fps: u32,
    consumer_formats: Vec<(u32, Vec<u64>)>,
) -> Result<(
    Capture,
    oneshot::Receiver<DmabufStreamFormat>,
    mpsc::Receiver<DmabufStreamFrame>,
)> {
    let handle = helper
        .toplevel_handle_for_identifier(&identifier)
        .ok_or_else(|| anyhow!("no toplevel with identifier {identifier:?}"))?;
    Ok(start_inner(
        helper,
        CaptureSource::Toplevel(handle),
        "toplevel",
        overlay_cursor,
        fps,
        consumer_formats,
    ))
}

fn start_inner(
    helper: WaylandHelper,
    source: CaptureSource,
    label: &'static str,
    overlay_cursor: bool,
    fps: u32,
    consumer_formats: Vec<(u32, Vec<u64>)>,
) -> (
    Capture,
    oneshot::Receiver<DmabufStreamFormat>,
    mpsc::Receiver<DmabufStreamFrame>,
) {
    let (fmt_tx, fmt_rx) = oneshot::channel();
    let (frame_tx, frame_rx) = mpsc::channel(16);
    let (stop_tx, stop_rx) = oneshot::channel();

    let fps = fps.max(1);
    let frame_budget = std::time::Duration::from_nanos(1_000_000_000 / fps as u64);

    let task = tokio::spawn(run_loop(
        helper,
        source,
        label,
        overlay_cursor,
        fps,
        frame_budget,
        consumer_formats,
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
    consumer_formats: Vec<(u32, Vec<u64>)>,
    fmt_tx: oneshot::Sender<DmabufStreamFormat>,
    frame_tx: mpsc::Sender<DmabufStreamFrame>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    let started = Instant::now();
    let mut fmt_tx = Some(fmt_tx);
    let mut next_deadline = Instant::now();
    let mut expected_format: Option<(u32, u64)> = None;

    loop {
        if matches!(
            stop_rx.try_recv(),
            Ok(()) | Err(oneshot::error::TryRecvError::Closed)
        ) {
            tracing::info!(label, "dmabuf_stream: stop signalled");
            return;
        }

        let source = source.clone();
        let captured = match helper
            .capture_source_dmabuf(source, overlay_cursor, Some(&consumer_formats))
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(label, error = %e, "dmabuf_stream: capture failed, retrying");
                tokio::time::sleep(frame_budget).await;
                continue;
            }
        };

        // Format-drift guard: cosmic-comp shouldn't switch modifier mid-
        // stream, but if it ever does the encoder pipeline can't
        // renegotiate (the appsrc caps are fixed at build time).
        // Surface as a stop instead of producing garbage frames.
        let current = (captured.fourcc as u32, captured.modifier);
        match expected_format {
            None => expected_format = Some(current),
            Some(prev) if prev != current => {
                tracing::error!(
                    label,
                    prev = ?prev,
                    current = ?current,
                    "dmabuf_stream: format/modifier drift, stopping"
                );
                return;
            }
            _ => {}
        }

        if let Some(tx) = fmt_tx.take() {
            let fmt = DmabufStreamFormat {
                width: captured.width,
                height: captured.height,
                fourcc: captured.fourcc as u32,
                modifier: captured.modifier,
                fps,
            };
            if tx.send(fmt).is_err() {
                tracing::warn!(label, "dmabuf_stream: fmt rx dropped");
                return;
            }
        }

        let pts_ns = started.elapsed().as_nanos() as u64;
        if frame_tx
            .send(DmabufStreamFrame {
                frame: captured,
                pts_ns,
            })
            .await
            .is_err()
        {
            tracing::info!(label, "dmabuf_stream: frame rx dropped, stopping");
            return;
        }

        next_deadline += frame_budget;
        let now = Instant::now();
        if next_deadline > now {
            tokio::time::sleep(next_deadline - now).await;
        } else {
            next_deadline = now;
        }
    }
}
