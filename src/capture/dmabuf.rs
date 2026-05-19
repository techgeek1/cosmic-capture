//! GBM-backed dmabuf capture support for cosmic-screencopy.
//!
//! cosmic-comp's screencopy can target a dmabuf-backed `wl_buffer` instead
//! of an `wl_shm` buffer. We allocate a GBM buffer object on the same DRM
//! render node the compositor advertises, wrap it as a `wl_buffer` via
//! `linux-dmabuf-v1`, hand it to `session.capture(..)`, and the compositor
//! writes directly into the GPU buffer — no memfd, no CPU-side copy.
//!
//! For the production pipeline (Phase 4+) the same dmabuf fd is handed to
//! gst's `vapostproc`/`vacompositor` for zero-copy compositing + encoding.
//! For the Phase 1 debug subcommand we CPU-map the BO once for PNG
//! readback so we can verify the protocol round-trip is correct.
//!
//! The GBM device is keyed by the `dev_t` cosmic-comp reports in
//! `Formats.dmabuf_device`. In practice every output on a given machine
//! routes through the same render node — if a future cosmic-comp ever
//! reports per-output devices we'd need cross-device staging, but that's
//! a problem for the day it surfaces.

use std::collections::HashMap;
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};

/// One render-node-backed GBM device, lazily opened the first time we
/// observe a `Formats.dmabuf_device` from cosmic-screencopy. Wrapped in an
/// `Arc<Mutex<>>` so multiple capture tasks share the same device handle —
/// `gbm::Device` is `!Sync` because libgbm objects aren't internally
/// thread-safe, so we serialize allocation through the mutex.
pub struct GbmContext {
    pub device_path: PathBuf,
    pub device: gbm::Device<File>,
}

impl GbmContext {
    /// Open the gbm device backed by the DRM `dev_t` reported by
    /// cosmic-screencopy. Walks `/dev/dri` and matches by `rdev()` — the
    /// same approach the cosmic-protocols dma example uses.
    pub fn for_dev_id(dev: u64) -> Result<Self> {
        for entry in fs::read_dir("/dev/dri").context("read /dev/dri")? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.rdev() == dev {
                let path = entry.path();
                let file = File::options()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .with_context(|| format!("open {}", path.display()))?;
                let device = gbm::Device::new(file)
                    .map_err(|e| anyhow!("gbm::Device::new({}): {e}", path.display()))?;
                tracing::info!(path = %path.display(), "gbm device opened");
                return Ok(Self {
                    device_path: path,
                    device,
                });
            }
        }
        Err(anyhow!("no /dev/dri/* node matches rdev {dev:#x}"))
    }
}

/// Cache of GBM contexts keyed by render-node dev_id. Today there's
/// effectively only one entry per process — every output shares the same
/// render node — but the map shape future-proofs against per-output
/// devices without a refactor.
#[derive(Default)]
pub struct GbmRegistry {
    contexts: Mutex<HashMap<u64, Arc<Mutex<GbmContext>>>>,
}

impl GbmRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_open(&self, dev: u64) -> Result<Arc<Mutex<GbmContext>>> {
        let mut map = self.contexts.lock().unwrap();
        if let Some(existing) = map.get(&dev) {
            return Ok(existing.clone());
        }
        let ctx = GbmContext::for_dev_id(dev)?;
        let entry = Arc::new(Mutex::new(ctx));
        map.insert(dev, entry.clone());
        Ok(entry)
    }
}

/// Pick the format we ask cosmic-comp to write into, intersected with what
/// our consumer (the debug PNG readback or gst's vapostproc) can handle.
///
/// Returns `(format, modifiers)` where `modifiers` is the set of modifiers
/// cosmic-comp advertised for that format. The caller's allocator then
/// asks GBM to pick from that set — GBM consults the driver and produces a
/// BO with the most efficient layout it can.
///
/// **Modifier intersection policy**: cosmic-comp's advertised modifiers
/// are already filtered against what cosmic-comp can write. The downstream
/// (Mesa/vapostproc) is queried by GBM itself when we call
/// `create_buffer_object_with_modifiers2` — GBM only picks one the driver
/// can actually allocate. So "intersection of the GPU's advertised
/// modifiers with cosmic-comp's" is implicit in the GBM call; no manual
/// intersection needed at this layer.
///
/// **Format preference**: Argb8888 is what the toolkit's dma example uses
/// and what cosmic-comp definitely advertises today. BGRA in memory order
/// (cosmic-comp's wayland-side spelling = drm-side AR24 fourcc = gst-side
/// "BGRA" caps). Falls through to Xrgb8888 if Argb8888 is missing.
pub fn pick_format(
    advertised: &[(u32, Vec<u64>)],
    consumer: Option<&[(u32, Vec<u64>)]>,
) -> Result<(drm_fourcc::DrmFourcc, Vec<u64>)> {
    use drm_fourcc::DrmFourcc;
    for preferred in [DrmFourcc::Argb8888, DrmFourcc::Xrgb8888] {
        let Some((_, advertised_mods)) = advertised.iter().find(|(f, _)| *f == preferred as u32)
        else {
            continue;
        };
        let chosen: Vec<u64> = if let Some(consumer) = consumer {
            let Some((_, consumer_mods)) = consumer.iter().find(|(f, _)| *f == preferred as u32)
            else {
                continue;
            };
            advertised_mods
                .iter()
                .copied()
                .filter(|m| consumer_mods.contains(m))
                .collect()
        } else {
            advertised_mods.clone()
        };
        if !chosen.is_empty() {
            return Ok((preferred, chosen));
        }
    }
    Err(anyhow!(
        "no usable dmabuf format in intersection: compositor={:?}, consumer={:?}",
        advertised.iter().map(|(f, _)| f).collect::<Vec<_>>(),
        consumer.map(|c| c.iter().map(|(f, _)| f).collect::<Vec<_>>())
    ))
}

/// Description of a captured dmabuf, one set per plane. cosmic-comp's
/// supported formats are single-plane RGBA-class today; multi-plane (YUV)
/// would only matter if we ever capture pre-converted compositor output.
pub struct DmabufFrame {
    /// The GBM buffer object the compositor wrote into. Owns the GPU
    /// allocation; dropping it frees the BO. The associated `wl_buffer` is
    /// destroyed separately by the capture call site.
    pub bo: gbm::BufferObject<()>,
    pub fourcc: drm_fourcc::DrmFourcc,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Debug for DmabufFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmabufFrame")
            .field("fourcc", &self.fourcc)
            .field("modifier", &format!("{:#x}", self.modifier))
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride0", &self.bo.stride_for_plane(0))
            .field("offset0", &self.bo.offset(0))
            .field("plane_count", &self.bo.plane_count())
            .finish()
    }
}
