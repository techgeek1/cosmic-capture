use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use ashpd::desktop::screenshot::Screenshot;

pub struct Captured {
    /// `file://` path the portal saved to (in a tmpdir).
    pub source_path: PathBuf,
    /// Was the result a clipboard handoff rather than a saved file?
    pub clipboard: bool,
}

pub async fn take(interactive: bool, modal: bool) -> Result<Captured> {
    let response = Screenshot::request()
        .interactive(interactive)
        .modal(modal)
        .send()
        .await
        .context("portal Screenshot::request send")?
        .response();

    let response = match response {
        Err(err) => {
            // ashpd surfaces user cancellation as an error; surface that as
            // a typed condition the caller can handle silently.
            if err.to_string().contains("Cancelled") {
                anyhow::bail!(CaptureCancelled);
            }
            return Err(anyhow!(err)).context("portal Screenshot");
        }
        Ok(r) => r,
    };

    let uri = response.uri();
    match uri.scheme() {
        "file" => {
            let path = uri
                .to_file_path()
                .map_err(|_| anyhow!("portal returned non-local file URI: {uri}"))?;
            Ok(Captured { source_path: path, clipboard: false })
        }
        "clipboard" => Ok(Captured { source_path: PathBuf::new(), clipboard: true }),
        other => Err(anyhow!("unsupported portal URI scheme: {other}")),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("capture cancelled by user")]
pub struct CaptureCancelled;
