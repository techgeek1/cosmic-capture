//! Portal-mediated PipeWire screencast backend.
//!
//! Uses `ashpd::desktop::screencast::ScreenCast` to obtain a PipeWire node
//! id, then connects to that node either directly (`pipewire-rs`) or
//! through a `pipewiresrc` element inside a GStreamer pipeline — the latter
//! is how the [`crate::encode::video`] module will consume frames.
//!
//! For the "no portal dialog, native COSMIC UX" goal we still need ashpd's
//! initial session creation, but we can persist the session token across
//! invocations so the user only sees the dialog the first time.

use anyhow::Result;

use crate::capture::{CaptureRequest, CaptureSource, Frame};

pub struct PipeWireSource {
    pub node_id: u32,
    _request: CaptureRequest,
}

impl PipeWireSource {
    pub async fn open(_request: CaptureRequest) -> Result<Self> {
        // TODO: ashpd ScreenCast request + remember restore_token in
        // ~/.config/cosmic-capture/portal.toml so we don't re-prompt.
        anyhow::bail!("pipewire portal backend not yet implemented")
    }
}

#[async_trait::async_trait]
impl CaptureSource for PipeWireSource {
    async fn next_frame(&mut self) -> Result<Frame> {
        anyhow::bail!("pipewire: next_frame stub — driven via gstreamer pipeline instead")
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }
}
