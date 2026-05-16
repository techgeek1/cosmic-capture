//! Pulls colors from the user's COSMIC theme via `cosmic-config` /
//! `cosmic-theme` — the same code path every other COSMIC app uses.
//!
//! All read failures fall back to sensible defaults so the selector remains
//! usable even on systems without a configured theme.

use cosmic_config::CosmicConfigEntry;
use cosmic_theme::{Theme, ThemeMode};

/// Returns the user's accent color packed as Wayland-compatible
/// ARGB8888 premultiplied at full alpha (suitable for shm buffer fills).
pub fn accent_argb() -> u32 {
    srgba_to_argb_premul(load().accent_color())
}

/// COSMIC's palette-defined "blue" — independent of whichever accent
/// the user has selected. Used for the recording-active indicator so it
/// reads as a distinct state regardless of accent choice.
pub fn palette_blue_argb() -> u32 {
    srgba_to_argb_premul(load().palette.accent_blue)
}

fn load() -> Theme {
    let is_dark = ThemeMode::config()
        .ok()
        .and_then(|c| ThemeMode::is_dark(&c).ok())
        .unwrap_or(true);

    let loaded = if is_dark {
        Theme::dark_config().ok().map(|c| Theme::get_entry(&c))
    } else {
        Theme::light_config().ok().map(|c| Theme::get_entry(&c))
    };

    match loaded {
        Some(Ok(t)) => t,
        Some(Err((errs, fallback))) => {
            tracing::warn!(?errs, "partial theme load; using fallback values");
            fallback
        }
        None => {
            tracing::debug!("no theme config; using built-in default");
            if is_dark {
                Theme::dark_default()
            } else {
                Theme::light_default()
            }
        }
    }
}

/// Convert a `palette::Srgba<f32>` (0..1) to premultiplied ARGB u32 packed
/// as `0xAARRGGBB`. In little-endian memory that lays out as `[B,G,R,A]`
/// which matches Wayland's `wl_shm::Format::Argb8888`.
fn srgba_to_argb_premul(c: cosmic_theme::palette::Srgba) -> u32 {
    let a_f = c.alpha.clamp(0.0, 1.0);
    let r = ((c.red * a_f).clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = ((c.green * a_f).clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = ((c.blue * a_f).clamp(0.0, 1.0) * 255.0).round() as u32;
    let a = (a_f * 255.0).round() as u32;
    (a << 24) | (r << 16) | (g << 8) | b
}
