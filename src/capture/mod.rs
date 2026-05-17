//! Compositor-facing capture sources.
//!
//! * [`screencast`] — `org.freedesktop.portal.ScreenCast`. Returns a
//!   PipeWire node id + a remote socket fd, fed to a GStreamer
//!   `pipewiresrc` element. Used by video and GIF capture.
//!
//! * [`screencopy`] — `zwlr_screencopy_v1` via cosmic-client-toolkit.
//!   One-shot frame capture for the screenshot pipeline (no portal,
//!   no second process, no temp file).

pub mod pipewire_capture;
pub mod screencast;
pub mod screencopy;
pub mod screenshot;
pub mod toplevels;
