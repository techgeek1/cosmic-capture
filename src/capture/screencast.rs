//! ScreenCast portal session: returns a PipeWire fd + node id ready for
//! `pipewiresrc fd=<fd> path=<node>` in a GStreamer pipeline.
//!
//! The restore_token is persisted to `~/.config/cosmic-capture/portal.toml`
//! so the source-picker dialog only appears the first time a user records
//! or makes a GIF — every subsequent invocation reuses the prior selection.

use std::os::fd::{AsRawFd, OwnedFd};

use anyhow::{Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Default, Serialize, Deserialize)]
struct StoredToken {
    restore_token: Option<String>,
}

pub struct PipeWireStream {
    pub fd: OwnedFd,
    pub node_id: u32,
    pub size: Option<(u32, u32)>,
}

impl PipeWireStream {
    pub fn raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}

pub async fn start(cursor: bool) -> Result<PipeWireStream> {
    let stored = load_token().unwrap_or_default();

    let proxy = Screencast::new().await.context("create Screencast proxy")?;
    let session = proxy.create_session().await.context("create Screencast session")?;

    let cursor_mode = if cursor {
        CursorMode::Embedded
    } else {
        CursorMode::Hidden
    };

    proxy
        .select_sources(
            &session,
            cursor_mode,
            SourceType::Monitor | SourceType::Window,
            false,
            stored.restore_token.as_deref(),
            PersistMode::ExplicitlyRevoked,
        )
        .await
        .context("Screencast select_sources")?;

    let response = proxy
        .start(&session, None)
        .await
        .context("Screencast start")?
        .response()
        .context("Screencast start response")?;

    if let Some(tok) = response.restore_token() {
        if stored.restore_token.as_deref() != Some(tok) {
            let _ = save_token(&StoredToken { restore_token: Some(tok.to_string()) });
        }
    }

    let stream = response
        .streams()
        .first()
        .cloned()
        .context("ScreenCast returned no streams")?;

    let fd = proxy
        .open_pipe_wire_remote(&session)
        .await
        .context("open_pipe_wire_remote")?;

    Ok(PipeWireStream {
        fd,
        node_id: stream.pipe_wire_node_id(),
        size: stream.size().map(|s| (s.0 as u32, s.1 as u32)),
    })
}

fn token_path() -> Result<std::path::PathBuf> {
    Ok(paths::config_dir()?.join("portal.toml"))
}

fn load_token() -> Result<StoredToken> {
    let path = token_path()?;
    if !path.exists() {
        return Ok(StoredToken::default());
    }
    let s = std::fs::read_to_string(&path)?;
    Ok(toml::from_str(&s)?)
}

fn save_token(tok: &StoredToken) -> Result<()> {
    let path = token_path()?;
    let s = toml::to_string(tok)?;
    std::fs::write(path, s)?;
    Ok(())
}
