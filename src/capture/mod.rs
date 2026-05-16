//! Portal-mediated capture sources.
//!
//! Two surface areas:
//!
//! * [`screenshot`] — `org.freedesktop.portal.Screenshot`. The compositor
//!   owns the entire interactive UX (region drag, output picker, freeze)
//!   and hands us back a saved PNG URI.
//!
//! * [`screencast`] — `org.freedesktop.portal.ScreenCast`. Returns a
//!   PipeWire node id + a remote socket fd, which we feed to a GStreamer
//!   `pipewiresrc` element. Used by both video and GIF capture.

pub mod screencast;
pub mod screenshot;
