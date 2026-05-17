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
use ashpd::enumflags2::BitFlags;
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

    // Ask the portal which source types it actually supports. Some
    // portal implementations refuse `SourceType::Window` outright and
    // will reject the whole `select_sources` call if it appears in the
    // requested bitmask, even though we'd happily accept `Monitor` only.
    let available = match proxy.available_source_types().await {
        Ok(types) => types,
        Err(e) => {
            tracing::warn!(error = %e, "available_source_types failed; falling back to Monitor");
            BitFlags::from(SourceType::Monitor)
        }
    };
    let requested = (SourceType::Monitor | SourceType::Window) & available;
    let requested = if requested.is_empty() {
        BitFlags::from(SourceType::Monitor)
    } else {
        requested
    };
    tracing::info!(?available, ?requested, "ScreenCast source types");

    // Some portals (notably xdg-desktop-portal-cosmic at time of writing)
    // don't implement `CursorMode::Embedded`. Ask the portal and pick the
    // best mode actually advertised, treating `cursor: true` as a hint
    // rather than a hard requirement.
    let available_cursor = match proxy.available_cursor_modes().await {
        Ok(modes) => modes,
        Err(e) => {
            tracing::warn!(error = %e, "available_cursor_modes failed; defaulting to Hidden");
            BitFlags::from(CursorMode::Hidden)
        }
    };
    let cursor_mode = if cursor {
        if available_cursor.contains(CursorMode::Embedded) {
            CursorMode::Embedded
        } else {
            // The other variant (Metadata) hands us a separate spa meta
            // stream which our pipewire consumer doesn't composite, so
            // claiming "cursor on" via Metadata would silently drop the
            // pointer. Prefer the honest behavior: log and turn it off.
            tracing::warn!(
                "portal does not support Embedded cursor mode; \
                 recording without cursor"
            );
            CursorMode::Hidden
        }
    } else if available_cursor.contains(CursorMode::Hidden) {
        CursorMode::Hidden
    } else {
        // Truly degenerate portal — just pick any advertised mode.
        available_cursor.iter().next().unwrap_or(CursorMode::Hidden)
    };
    tracing::info!(?available_cursor, ?cursor_mode, "ScreenCast cursor mode");

    // Try once with the stored restore_token (if any). If select_sources
    // or start fails, the token may be stale — wipe it and retry from
    // scratch so the user can re-pick a source rather than being stuck
    // forever.
    let session = proxy
        .create_session()
        .await
        .context("create Screencast session")?;

    let first_attempt = proxy
        .select_sources(
            &session,
            cursor_mode,
            requested,
            false,
            stored.restore_token.as_deref(),
            PersistMode::ExplicitlyRevoked,
        )
        .await;

    let (session, response) = match first_attempt {
        Ok(_) => {
            let response = proxy
                .start(&session, None)
                .await
                .context("Screencast start")?
                .response()
                .context("Screencast start response")?;
            (session, response)
        }
        Err(e) if stored.restore_token.is_some() => {
            tracing::warn!(error = %e, "select_sources failed with stored token; clearing and retrying");
            let _ = std::fs::remove_file(token_path()?);
            // The failed session is unusable — start over from a fresh one.
            let session2 = proxy
                .create_session()
                .await
                .context("create Screencast session (retry)")?;
            proxy
                .select_sources(
                    &session2,
                    cursor_mode,
                    requested,
                    false,
                    None,
                    PersistMode::ExplicitlyRevoked,
                )
                .await
                .context("Screencast select_sources (retry)")?;
            let response = proxy
                .start(&session2, None)
                .await
                .context("Screencast start (retry)")?
                .response()
                .context("Screencast start response (retry)")?;
            (session2, response)
        }
        Err(e) => return Err(anyhow::Error::new(e).context("Screencast select_sources")),
    };

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
