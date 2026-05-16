//! Screenshot pipeline: capture → optional crop → PNG → file or clipboard.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::time::{Duration, sleep};

use crate::capture::screencopy::{self, CapturedFrame};
use crate::capture::screenshot::{self, Target};
use crate::cli::ScreenshotArgs;
use crate::encode::video::CropRect;
use crate::{notify, paths};

/// Where to send the captured PNG.
pub enum Destination {
    File(Option<PathBuf>),
    Clipboard,
}

/// CLI entry — captures the full active output and saves to a default
/// Pictures path (or `--file`). No portal UI, no region picker.
pub async fn run(args: ScreenshotArgs) -> Result<()> {
    if args.delay_ms > 0 {
        sleep(Duration::from_millis(args.delay_ms)).await;
    }
    let target = first_output_name()?;
    let frame = screenshot::take(Target::OutputName(target), true).await?;

    if args.common.clipboard {
        encode_to_clipboard(&frame, None).await?;
        tracing::info!("copied to clipboard");
        if args.common.notify {
            let _ = notify::saved(std::path::Path::new("clipboard"), "Screenshot").await;
        }
        return Ok(());
    }

    let dest = paths::resolve(args.common.file.clone(), paths::Kind::Image, "png")?;
    encode_to_file(&frame, None, &dest)?;
    println!("{}", dest.display());
    if args.common.notify {
        if let Err(e) = notify::saved(&dest, "Screenshot").await {
            tracing::warn!(error = %e, "failed to send notification");
        }
    }
    Ok(())
}

/// GUI entry — caller chooses the source output, optional crop, and
/// destination. Returns the saved path (or a synthetic "clipboard" path
/// for clipboard captures, so notification code has something to show).
pub async fn capture_with(
    output_name: String,
    with_cursor: bool,
    crop: Option<CropRect>,
    destination: Destination,
    notify_user: bool,
) -> Result<PathBuf> {
    let frame = screenshot::take(Target::OutputName(output_name), with_cursor).await?;
    let crop = crop.map(|c| (c.x, c.y, c.w, c.h));

    match destination {
        Destination::Clipboard => {
            encode_to_clipboard(&frame, crop).await?;
            if notify_user {
                let _ = notify::saved(std::path::Path::new("clipboard"), "Screenshot").await;
            }
            Ok(PathBuf::from("clipboard"))
        }
        Destination::File(user_path) => {
            let dest = paths::resolve(user_path, paths::Kind::Image, "png")?;
            encode_to_file(&frame, crop, &dest)?;
            if notify_user {
                if let Err(e) = notify::saved(&dest, "Screenshot").await {
                    tracing::warn!(error = %e, "failed to send notification");
                }
            }
            Ok(dest)
        }
    }
}

fn encode_to_file(
    frame: &CapturedFrame,
    crop: Option<(i32, i32, u32, u32)>,
    dest: &std::path::Path,
) -> Result<()> {
    let (pixels, w, h) = resolve_crop(frame, crop)?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).ok();
    }
    let file = fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    write_png(std::io::BufWriter::new(file), &pixels, w, h)?;
    Ok(())
}

async fn encode_to_clipboard(
    frame: &CapturedFrame,
    crop: Option<(i32, i32, u32, u32)>,
) -> Result<()> {
    let (pixels, w, h) = resolve_crop(frame, crop)?;
    let mut png_bytes: Vec<u8> = Vec::with_capacity((w as usize) * (h as usize) * 2);
    write_png(&mut png_bytes, &pixels, w, h)?;
    tokio::task::spawn_blocking(move || copy_png_to_clipboard(png_bytes))
        .await
        .map_err(|e| anyhow::anyhow!("clipboard task join: {e}"))??;
    Ok(())
}

fn copy_png_to_clipboard(png_bytes: Vec<u8>) -> Result<()> {
    use wl_clipboard_rs::copy::{MimeType, Options, ServeRequests, Source};
    let mut opts = Options::new();
    // Foreground = false → daemonize the copy server so the bytes survive
    // after this process exits. ServeRequests::Unlimited so any number of
    // paste consumers can read the contents.
    opts.foreground(false);
    opts.serve_requests(ServeRequests::Unlimited);
    opts.copy(
        Source::Bytes(png_bytes.into_boxed_slice()),
        MimeType::Specific("image/png".to_string()),
    )
    .map_err(|e| anyhow::anyhow!("wl-clipboard copy: {e}"))?;
    Ok(())
}

fn resolve_crop(
    frame: &CapturedFrame,
    crop: Option<(i32, i32, u32, u32)>,
) -> Result<(Vec<u8>, u32, u32)> {
    let (cx, cy, cw, ch) = match crop {
        None => return Ok((frame.pixels.clone(), frame.width, frame.height)),
        Some(c) => c,
    };
    // Clip the rect into frame bounds so a user-drawn region that slipped
    // off the output edge still produces a valid image.
    let cx = cx.max(0) as u32;
    let cy = cy.max(0) as u32;
    let cw = cw.min(frame.width.saturating_sub(cx));
    let ch = ch.min(frame.height.saturating_sub(cy));
    if cw == 0 || ch == 0 {
        anyhow::bail!("crop is empty after clipping to output");
    }
    let pixels = screencopy::crop_rgba(frame, cx, cy, cw, ch)?;
    Ok((pixels, cw, ch))
}

fn write_png<W: std::io::Write>(out: W, rgba: &[u8], w: u32, h: u32) -> Result<()> {
    let mut encoder = png::Encoder::new(out, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("png write_header")?;
    writer.write_image_data(rgba).context("png write_image_data")?;
    Ok(())
}

fn first_output_name() -> Result<String> {
    // CLI mode: no GUI to pick the active output, so grab whichever output
    // wayland advertises first. Multi-output users can pass --file to pin
    // the destination; per-output selection is GUI-only for now.
    let conn = wayland_client::Connection::connect_to_env()
        .context("connect wayland for output enumeration")?;
    let (globals, mut q) = wayland_client::globals::registry_queue_init::<EnumData>(&conn)
        .context("registry_queue_init for outputs")?;
    let qh = q.handle();
    let registry_state = smithay_client_toolkit::registry::RegistryState::new(&globals);
    let output_state = smithay_client_toolkit::output::OutputState::new(&globals, &qh);
    let mut data = EnumData {
        registry_state,
        output_state,
    };
    q.roundtrip(&mut data).context("roundtrip for outputs")?;
    data.output_state
        .outputs()
        .next()
        .and_then(|o| data.output_state.info(&o)?.name)
        .context("no wayland outputs found")
}

struct EnumData {
    registry_state: smithay_client_toolkit::registry::RegistryState,
    output_state: smithay_client_toolkit::output::OutputState,
}

impl smithay_client_toolkit::registry::ProvidesRegistryState for EnumData {
    fn registry(&mut self) -> &mut smithay_client_toolkit::registry::RegistryState {
        &mut self.registry_state
    }
    smithay_client_toolkit::registry_handlers!();
}

impl smithay_client_toolkit::output::OutputHandler for EnumData {
    fn output_state(&mut self) -> &mut smithay_client_toolkit::output::OutputState {
        &mut self.output_state
    }
    fn new_output(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: wayland_client::protocol::wl_output::WlOutput,
    ) {
    }
    fn update_output(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: wayland_client::protocol::wl_output::WlOutput,
    ) {
    }
    fn output_destroyed(
        &mut self,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
        _: wayland_client::protocol::wl_output::WlOutput,
    ) {
    }
}

smithay_client_toolkit::delegate_output!(EnumData);
smithay_client_toolkit::delegate_registry!(EnumData);
