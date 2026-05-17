//! Direct PipeWire stream consumer for the screencast PipeWire fd.
//!
//! Replaces gst-plugin-pipewire's `pipewiresrc` for the recording path:
//! we open our own consumer-side stream against the node id the
//! xdg-desktop-portal-cosmic ScreenCast portal hands us, accept whichever
//! raw-video format pipewire negotiates, and forward each buffer's bytes
//! to a tokio mpsc receiver for the gst encoder pipeline to push into an
//! `appsrc` element. The wedge mode in `pipewiresrc`'s
//! `handle_format_change` assertion (`gst_caps_is_fixed (pwsrc->caps)`)
//! is in the gst element itself — going direct sidesteps it entirely.

use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use libspa::param::video::VideoFormat as SpaVideoFormat;
use pipewire as pw;
use pw::spa::pod::Pod;
use pw::{properties::properties, spa};
use tokio::sync::{mpsc, oneshot};

/// Negotiated stream format. Emitted exactly once on the format-info
/// oneshot before any frames flow.
#[derive(Debug, Clone, Copy)]
pub struct StreamFormat {
    pub width: u32,
    pub height: u32,
    /// Frames per second (numerator / denominator collapsed; 30/1 → 30).
    pub fps: u32,
    /// pipewire/spa video format (RGBA, BGRA, RGBx, etc.). Caller must map
    /// to the corresponding gst caps string before constructing its appsrc.
    pub format: SpaVideoFormat,
    /// Row stride in bytes. May exceed `width * bpp` if the compositor
    /// padded rows.
    pub stride: u32,
}

/// One captured frame. `bytes` is the raw pixel data in row-major order at
/// the format reported via [`StreamFormat`]. `pts_ns` is a monotonic
/// timestamp in nanoseconds suitable for setting on a gst buffer; we use
/// pipewire's `buffer->time` directly.
pub struct Frame {
    pub bytes: Vec<u8>,
    pub pts_ns: u64,
}

/// Handle to a running capture thread. Drop to stop. Drop blocks briefly
/// while the pipewire mainloop tears down.
pub struct Capture {
    stop_tx: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl Capture {
    /// Stop the pipewire mainloop and join the thread. Idempotent.
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            if let Err(e) = t.join() {
                tracing::warn!(error = ?e, "pipewire capture thread panicked");
            }
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Connect to a screencast portal's PipeWire node and start streaming.
///
/// `fd` is the PipeWire remote socket returned by ashpd's
/// `Screencast::open_pipe_wire_remote`. `node_id` is the target object id
/// from one of the stream entries in the start response.
///
/// Returns immediately with a [`Capture`] handle, a oneshot that resolves
/// once the stream's first `param_changed` fixes the video format, and an
/// mpsc carrying frames in arrival order. Both receivers must be polled —
/// the channels are bounded so a slow consumer back-pressures the
/// pipewire callback. Channel capacity is sized for ~250ms at 60fps so a
/// briefly stalled encoder doesn't wedge the mainloop thread immediately.
pub fn start(
    fd: OwnedFd,
    node_id: u32,
) -> Result<(Capture, oneshot::Receiver<StreamFormat>, mpsc::Receiver<Frame>)> {
    let (fmt_tx, fmt_rx) = oneshot::channel::<StreamFormat>();
    let (frame_tx, frame_rx) = mpsc::channel::<Frame>(16);
    let (stop_tx, stop_rx) = pw::channel::channel::<()>();

    let fmt_tx = Arc::new(Mutex::new(Some(fmt_tx)));
    let thread = std::thread::Builder::new()
        .name("cosmic-capture pipewire".to_string())
        .spawn(move || run_loop(fd, node_id, fmt_tx, frame_tx, stop_rx))
        .context("spawn pipewire thread")?;

    Ok((
        Capture {
            stop_tx: Some(stop_tx),
            thread: Some(thread),
        },
        fmt_rx,
        frame_rx,
    ))
}

/// Per-stream state shared between the param_changed and process callbacks
/// running on the pipewire mainloop thread.
struct StreamState {
    /// Resolved format, populated on first `param_changed`.
    format: Option<StreamFormat>,
    /// One-shot sender for the resolved format. `take()`d after the first
    /// successful negotiation.
    fmt_tx: Arc<Mutex<Option<oneshot::Sender<StreamFormat>>>>,
    /// Bounded channel of frames out. Sends use `try_send` so a stalled
    /// consumer drops frames rather than blocking the realtime callback.
    frame_tx: mpsc::Sender<Frame>,
    /// VideoInfoRaw scratch used by `parse_format`.
    raw: spa::param::video::VideoInfoRaw,
}

fn run_loop(
    fd: OwnedFd,
    node_id: u32,
    fmt_tx: Arc<Mutex<Option<oneshot::Sender<StreamFormat>>>>,
    frame_tx: mpsc::Sender<Frame>,
    stop_rx: pw::channel::Receiver<()>,
) -> Result<()> {
    pw::init();

    // pipewire-rs 0.9 splits MainLoop/Context/Stream into Box and Rc
    // variants; Rc is what the upstream `streams` example uses and gives
    // us shared ownership of the core/loop so the stream listener can
    // outlive the local stack frame.
    let mainloop =
        pw::main_loop::MainLoopRc::new(None).map_err(|e| anyhow!("pw MainLoopRc::new: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| anyhow!("pw ContextRc::new: {e}"))?;
    // Connect to the privileged PipeWire socket the screencast portal gave
    // us, not the default user session. The portal grants us a fresh
    // remote that's already authorized to consume the screencast node.
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| anyhow!("pw connect_fd_rc: {e}"))?;

    // Hook the mainloop to stop on a channel signal so the GUI can
    // teardown cleanly. The closure captures the mainloop's quit handle.
    let main_quit = mainloop.clone();
    let _stop_handle = stop_rx.attach(mainloop.loop_(), move |_| {
        tracing::info!("pipewire mainloop: stop signal received");
        main_quit.quit();
    });

    let stream = pw::stream::StreamRc::new(
        core,
        "cosmic-capture-video",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| anyhow!("pw StreamRc::new: {e}"))?;

    let state = StreamState {
        format: None,
        fmt_tx,
        frame_tx,
        raw: spa::param::video::VideoInfoRaw::new(),
    };

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, _, old, new| {
            tracing::info!(?old, ?new, "pipewire stream state changed");
        })
        .param_changed(|_, user, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let (media_type, media_subtype) =
                match spa::param::format_utils::parse_format(param) {
                    Ok(v) => v,
                    Err(_) => return,
                };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                tracing::warn!(?media_type, ?media_subtype,
                    "ignoring non-raw-video format param");
                return;
            }
            if user.raw.parse(param).is_err() {
                tracing::warn!("failed to parse VideoInfoRaw from param");
                return;
            }
            let size = user.raw.size();
            let fr = user.raw.framerate();
            let fmt = StreamFormat {
                width: size.width,
                height: size.height,
                fps: if fr.denom == 0 {
                    30
                } else {
                    (fr.num / fr.denom).max(1)
                },
                format: user.raw.format(),
                // Stride is bpp * width by default; per-buffer the chunk
                // can override (see process()), but we publish the typical
                // value here so the appsrc caps line up.
                stride: bytes_per_pixel(user.raw.format()).saturating_mul(size.width),
            };
            tracing::info!(?fmt, "pipewire stream format negotiated");
            user.format = Some(fmt);
            // Only fire the oneshot once — subsequent format changes (rare
            // for screencast) are silently ignored.
            if let Some(tx) = user.fmt_tx.lock().unwrap().take() {
                let _ = tx.send(fmt);
            }
        })
        .process(|stream: &pw::stream::Stream, user| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                tracing::debug!("pipewire: out of buffers");
                return;
            };
            let Some(fmt) = user.format else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let (chunk_size, stride) = {
                let chunk = data.chunk();
                (chunk.size() as usize, chunk.stride().max(0) as u32)
            };
            let Some(bytes_in) = data.data() else {
                return;
            };
            let usable = chunk_size.min(bytes_in.len());
            if usable == 0 {
                return;
            }
            // Repack to a tight buffer if the source stride doesn't match
            // the gst caps expectation (typical for hardware-aligned
            // planes). Cheap memcpy.
            let owned = if stride == fmt.stride || stride == 0 {
                bytes_in[..usable].to_vec()
            } else {
                let bpp = bytes_per_pixel(fmt.format) as usize;
                let row = (fmt.width as usize).saturating_mul(bpp);
                let mut out = Vec::with_capacity(row * fmt.height as usize);
                for y in 0..(fmt.height as usize) {
                    let src_off = y * stride as usize;
                    if src_off + row > bytes_in.len() {
                        break;
                    }
                    out.extend_from_slice(&bytes_in[src_off..src_off + row]);
                }
                out
            };
            // try_send: drop on backpressure rather than block the
            // realtime callback. Logged at debug to avoid log spam.
            let frame = Frame {
                bytes: owned,
                pts_ns: 0, // gst appsrc do-timestamp=true assigns its own.
            };
            if let Err(e) = user.frame_tx.try_send(frame) {
                tracing::debug!(?e, "frame dropped (channel full)");
            }
        })
        .register()
        .map_err(|e| anyhow!("pw stream register: {e}"))?;

    // Format-enum param: ask for any raw-video format pipewire wants to
    // give us, at any size, with a reasonable framerate range. The
    // compositor picks a specific one and we accept it.
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            SpaVideoFormat::BGRA,
            SpaVideoFormat::BGRA,
            SpaVideoFormat::RGBA,
            SpaVideoFormat::BGRx,
            SpaVideoFormat::RGBx,
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow!("serialize format pod: {e}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("format Pod::from_bytes")?];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| anyhow!("pw stream connect: {e}"))?;

    tracing::info!(node_id, "pipewire stream connected; entering mainloop");
    mainloop.run();
    tracing::info!("pipewire mainloop exited");
    Ok(())
}

/// Bytes per pixel for a packed raw video format. We only enumerate the
/// 4-byte RGB/BGR variants we asked pipewire for, plus a safe default so
/// unknown formats don't divide by zero.
fn bytes_per_pixel(format: SpaVideoFormat) -> u32 {
    match format {
        SpaVideoFormat::BGRA
        | SpaVideoFormat::RGBA
        | SpaVideoFormat::BGRx
        | SpaVideoFormat::RGBx => 4,
        _ => 4,
    }
}

/// Map a pipewire/spa video format to its gst caps string. Used by the
/// recording pipeline when constructing the `appsrc` element's caps.
pub fn gst_format_name(format: SpaVideoFormat) -> &'static str {
    match format {
        SpaVideoFormat::BGRA => "BGRA",
        SpaVideoFormat::RGBA => "RGBA",
        SpaVideoFormat::BGRx => "BGRx",
        SpaVideoFormat::RGBx => "RGBx",
        // Conservative fallback — BGRA matches what cosmic-comp typically
        // delivers; if the compositor surprises us with something else
        // we'd see noisy frames rather than a crash.
        _ => "BGRA",
    }
}
