//! CPU painting of the selector overlay.
//!
//! Buffer is ARGB8888 premultiplied. Selection rect uses rounded corners
//! (12px logical, scaled by buffer scale) to match COSMIC's UI rounding.
//!
//! Hot path: this runs once per displayed frame, against a buffer the size
//! of an entire output (potentially 4K = 33 MB). Two design choices keep
//! it fast:
//!  * Background fill iterates the buffer as `u32` chunks, which LLVM
//!    lowers to wide vector stores.
//!  * The cutout's straight-middle band (the majority of pixel area) uses
//!    `slice::fill` for whole-scanline writes. Only the rounded-corner
//!    bands — top `r_outer` rows and bottom `r_outer` rows — pay the
//!    per-pixel distance test.

/// Corner radius for the selection rect, in physical pixels at scale 1.
/// Painters scale this up implicitly because they're working in buffer pixels.
const ROUND_RADIUS: i32 = 12;
const BORDER_THICKNESS: i32 = 2;

/// 60% black overlay (premultiplied) — used during the Selecting phase.
pub const DIM_BACKGROUND: u32 = 0x99_00_00_00;
/// Fully transparent — used during the Recording phase so only the
/// rounded border ring is visible and apps underneath show through.
pub const TRANSPARENT_BACKGROUND: u32 = 0x00_00_00_00;

pub fn paint(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    cutout: Option<(i32, i32, u32, u32)>,
    border: u32,
    background: u32,
) {
    let needed = (width as usize) * (height as usize) * 4;
    debug_assert!(
        pixels.len() >= needed,
        "buffer too small: have {}, need {}",
        pixels.len(),
        needed
    );
    let pixels = &mut pixels[..needed];

    fill_argb(pixels, background);

    let Some((cx, cy, cw, ch)) = cutout else { return };
    if cw == 0 || ch == 0 {
        return;
    }

    let x0 = cx.max(0);
    let y0 = cy.max(0);
    let x1 = (cx + cw as i32).min(width as i32);
    let y1 = (cy + ch as i32).min(height as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }

    // Adapt corner radius to selection size — don't try to round a tiny rect.
    let max_radius = ((x1 - x0).min(y1 - y0) / 2 - 1).max(0);
    let r_outer = ROUND_RADIUS.min(max_radius);
    let r_inner = (r_outer - BORDER_THICKNESS).max(0);
    let r_outer_sq = r_outer * r_outer;
    let r_inner_sq = r_inner * r_inner;

    // The "inner rectangle" is the cutout inset by the corner radius.
    // In rows inside [inner_y0, inner_y1] we're vertically between the
    // corner caps, so the row layout is just [border | interior | border]
    // and we can use scanline fills. Outside that vertical band, we're
    // in a corner row and need the per-pixel rounded test.
    let inner_x0 = x0 + r_outer;
    let inner_x1 = (x1 - 1 - r_outer).max(inner_x0);
    let inner_y0 = y0 + r_outer;
    let inner_y1 = (y1 - 1 - r_outer).max(inner_y0);

    let border_word = border;
    let stride_bytes = (width as usize) * 4;
    let bt = BORDER_THICKNESS as usize;

    // ─── Middle band: whole-scanline writes, no per-pixel work ────────────
    // For a 2000-row middle band this dominates the cutout work, so making
    // it scanline-fast is the main win over the previous per-pixel loop.
    for py in inner_y0..=inner_y1 {
        let row_start = (py as usize) * stride_bytes;
        let x0u = x0 as usize;
        let x1u = x1 as usize;

        if x1u <= x0u + 2 * bt {
            // Selection too narrow for interior; whole row is border.
            fill_u32_range(pixels, row_start + x0u * 4, row_start + x1u * 4, border_word);
            continue;
        }

        // Left border.
        fill_u32_range(
            pixels,
            row_start + x0u * 4,
            row_start + (x0u + bt) * 4,
            border_word,
        );
        // Interior (fully transparent — premultiplied 0).
        pixels[row_start + (x0u + bt) * 4..row_start + (x1u - bt) * 4].fill(0);
        // Right border.
        fill_u32_range(
            pixels,
            row_start + (x1u - bt) * 4,
            row_start + x1u * 4,
            border_word,
        );
    }

    // ─── Corner bands: per-pixel rounded test ─────────────────────────────
    // Top band: rows [y0, inner_y0). Bottom band: rows (inner_y1, y1).
    // Width is ≤ r_outer (~12 px) per band, so per-pixel cost is bounded.
    paint_corner_band(
        pixels, stride_bytes, y0..inner_y0,
        x0, x1, inner_x0, inner_x1, inner_y0, inner_y1,
        r_inner_sq, r_outer_sq, border_word,
    );
    paint_corner_band(
        pixels, stride_bytes, (inner_y1 + 1)..y1,
        x0, x1, inner_x0, inner_x1, inner_y0, inner_y1,
        r_inner_sq, r_outer_sq, border_word,
    );
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn paint_corner_band(
    pixels: &mut [u8],
    stride_bytes: usize,
    py_range: std::ops::Range<i32>,
    x0: i32, x1: i32,
    inner_x0: i32, inner_x1: i32,
    inner_y0: i32, inner_y1: i32,
    r_inner_sq: i32, r_outer_sq: i32,
    border_word: u32,
) {
    let border_bytes = border_word.to_le_bytes();
    for py in py_range {
        let cy = py.clamp(inner_y0, inner_y1);
        let dy = py - cy;
        let dy2 = dy * dy;
        let row_start = (py as usize) * stride_bytes;
        for px in x0..x1 {
            let cx = px.clamp(inner_x0, inner_x1);
            let dx = px - cx;
            let d2 = dx * dx + dy2;
            let i = row_start + (px as usize) * 4;
            if d2 <= r_inner_sq {
                pixels[i..i + 4].fill(0);
            } else if d2 <= r_outer_sq {
                pixels[i..i + 4].copy_from_slice(&border_bytes);
            }
        }
    }
}

/// Fills `[start..end]` (in u8 indices) of `pixels` with `word` (treated as
/// the ARGB u32 to repeat). Both ends MUST be 4-byte-aligned indices.
#[inline]
fn fill_u32_range(pixels: &mut [u8], start: usize, end: usize, word: u32) {
    debug_assert!(start % 4 == 0 && end % 4 == 0);
    let slice = &mut pixels[start..end];
    let (head, body, tail) = unsafe { slice.align_to_mut::<u32>() };
    body.fill(word);
    // Stragglers from misalignment — shouldn't happen given the start/end
    // contract above, but handle correctly.
    let bytes = word.to_le_bytes();
    for chunk in head.chunks_exact_mut(4).chain(tail.chunks_exact_mut(4)) {
        chunk.copy_from_slice(&bytes);
    }
}

fn fill_argb(buf: &mut [u8], argb: u32) {
    let (head, body, tail) = unsafe { buf.align_to_mut::<u32>() };
    body.fill(argb);
    let bytes = argb.to_le_bytes();
    for chunk in head.chunks_exact_mut(4).chain(tail.chunks_exact_mut(4)) {
        chunk.copy_from_slice(&bytes);
    }
}
