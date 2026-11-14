//! Whole-buffer pixel conversions. A 4K screenshot is 33 MB, so these split the
//! work across cores and avoid per-pixel branches and divisions.

use tiny_skia::{IntSize, Pixmap};

/// Below this many bytes, threads cost more than they save.
const PARALLEL_MIN_BYTES: usize = 1 << 20;

fn threads_for(len: usize) -> usize {
    if len < PARALLEL_MIN_BYTES {
        return 1;
    }
    std::thread::available_parallelism().map_or(1, |n| n.get()).min(8)
}

/// Runs `f` over matching pieces of `dst` and `src`, in parallel for large buffers.
/// Pieces are whole multiples of `unit` bytes (a pixel, or a band of rows).
fn par_zip(dst: &mut [u8], src: &[u8], unit: usize, f: impl Fn(&mut [u8], &[u8]) + Sync) {
    debug_assert_eq!(dst.len(), src.len());
    let threads = threads_for(dst.len());
    if threads == 1 {
        return f(dst, src);
    }
    let units = dst.len().div_ceil(unit);
    let chunk = units.div_ceil(threads) * unit;
    std::thread::scope(|s| {
        for (d, sr) in dst.chunks_mut(chunk).zip(src.chunks(chunk)) {
            let f = &f;
            s.spawn(move || f(d, sr));
        }
    });
}

/// Takes a capture in the OS's native BGRX layout (4th byte undefined). Sets
/// alpha to opaque in place, so `bgrx` becomes valid BGRA for the GPU without a
/// copy, and returns the RGBA copy the editor and exporter work on. One pass.
pub fn split_bgrx(bgrx: &mut [u8]) -> Vec<u8> {
    let mut rgba = vec![0u8; bgrx.len()];
    par_zip_mut(&mut rgba, bgrx, |d, s| {
        for (d, s) in d.as_chunks_mut::<4>().0.iter_mut().zip(s.as_chunks_mut::<4>().0) {
            s[3] = 255;
            *d = [s[2], s[1], s[0], 255];
        }
    });
    rgba
}

/// Like [`par_zip`], with the source mutable too. Pieces are whole pixels.
fn par_zip_mut(dst: &mut [u8], src: &mut [u8], f: impl Fn(&mut [u8], &mut [u8]) + Sync) {
    let threads = threads_for(dst.len());
    if threads == 1 {
        return f(dst, src);
    }
    let chunk = (dst.len() / 4).div_ceil(threads) * 4;
    std::thread::scope(|s| {
        for (d, sr) in dst.chunks_mut(chunk).zip(src.chunks_mut(chunk)) {
            let f = &f;
            s.spawn(move || f(d, sr));
        }
    });
}

/// Swaps the red and blue channels of 8-bit, 4-channel pixels (RGBA <-> BGRA).
pub fn swap_red_blue(pixels: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; pixels.len()];
    par_zip(&mut out, pixels, 4, |d, s| {
        for (d, s) in d.as_chunks_mut::<4>().0.iter_mut().zip(s.as_chunks::<4>().0) {
            *d = [s[2], s[1], s[0], s[3]];
        }
    });
    out
}

/// Exact rounding demultiply. Only anti-aliased edge pixels take this path,
/// so the division is cheap overall.
#[inline]
fn demultiply(c: u8, a: u8) -> u8 {
    ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
}

/// Premultiplied RGBA to straight-alpha `out` (BGRA if `swap`, else RGBA).
fn unpremultiply(src: &[u8], swap: bool) -> Vec<u8> {
    // Zeroed allocations are lazily mapped, so mostly transparent layers stay cheap.
    let mut out = vec![0u8; src.len()];
    par_zip(&mut out, src, 4, |d, s| {
        for (d, s) in d.as_chunks_mut::<4>().0.iter_mut().zip(s.as_chunks::<4>().0) {
            let [r, g, b, a] = *s;
            let (r, b) = if swap { (b, r) } else { (r, b) };
            *d = match a {
                0 => continue,
                255 => [r, g, b, 255],
                a => [demultiply(r, a), demultiply(g, a), demultiply(b, a), a],
            };
        }
    });
    out
}

/// Converts premultiplied RGBA to straight-alpha BGRA, the layout most GPU
/// texture uploads (and gpui) expect.
pub fn to_bgra_straight(pixmap: &Pixmap) -> Vec<u8> {
    unpremultiply(pixmap.data(), true)
}

/// Converts premultiplied RGBA to straight-alpha RGBA (what clipboards and PNG want).
pub fn to_rgba_straight(pixmap: &Pixmap) -> Vec<u8> {
    unpremultiply(pixmap.data(), false)
}

/// Copies the `w`x`h` block at (`x`, `y`) of `src` (which must contain it).
pub fn crop(src: &Pixmap, x: u32, y: u32, w: u32, h: u32) -> Option<Pixmap> {
    let mut out = Pixmap::new(w, h)?;
    let (stride, row) = (src.width() as usize * 4, w as usize * 4);
    let start = y as usize * stride + x as usize * 4;
    let data = src.data();
    for (i, dst) in out.data_mut().chunks_exact_mut(row).enumerate() {
        let from = start + i * stride;
        dst.copy_from_slice(&data[from..from + row]);
    }
    Some(out)
}

/// Replaces every `block`x`block` cell with its average color.
pub fn pixelate(src: &Pixmap, block: u32) -> Pixmap {
    let (w, h) = (src.width(), src.height());
    let mut out = Pixmap::new(w, h).expect("same size as an existing pixmap");
    let stride = w as usize * 4;
    let band = block as usize * stride;
    par_zip(out.data_mut(), src.data(), band, |dst, src| {
        let cells = (w as usize).div_ceil(block as usize);
        let mut sums = vec![0u32; cells * 4];
        let mut averaged = vec![0u8; stride];
        for (dst, src) in dst.chunks_mut(band).zip(src.chunks(band)) {
            let rows = src.len() / stride;
            sums.fill(0);
            // Row-major accumulation keeps memory access sequential.
            for row in src.chunks_exact(stride) {
                for (cell, pixels) in row.chunks(block as usize * 4).enumerate() {
                    let sum = &mut sums[cell * 4..cell * 4 + 4];
                    for px in pixels.as_chunks::<4>().0 {
                        for c in 0..4 {
                            sum[c] += px[c] as u32;
                        }
                    }
                }
            }
            for (cell, pixels) in averaged.chunks_mut(block as usize * 4).enumerate() {
                let n = (rows * pixels.len() / 4) as u32;
                let avg: [u8; 4] = std::array::from_fn(|c| (sums[cell * 4 + c] / n) as u8);
                for px in pixels.as_chunks_mut::<4>().0 {
                    *px = avg;
                }
            }
            for row in dst.chunks_exact_mut(stride) {
                row.copy_from_slice(&averaged);
            }
        }
    });
    out
}

/// Wraps `rgba` (premultiplied, or opaque) as a pixmap.
pub fn pixmap_from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Option<Pixmap> {
    Pixmap::from_vec(rgba, IntSize::from_wh(width, height)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(w: u32, h: u32) -> Pixmap {
        let mut seed = 7u32;
        let data = (0..w * h)
            .flat_map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let [r, g, b, _] = seed.to_le_bytes();
                [r, g, b, 255]
            })
            .collect();
        pixmap_from_rgba(w, h, data).unwrap()
    }

    /// The straightforward per-block implementation the optimized one replaced.
    #[allow(clippy::needless_range_loop)]
    fn pixelate_reference(src: &Pixmap, block: u32) -> Vec<u8> {
        let (w, h) = (src.width(), src.height());
        let mut out = src.data().to_vec();
        let stride = w as usize * 4;
        for by in (0..h).step_by(block as usize) {
            for bx in (0..w).step_by(block as usize) {
                let (x1, y1) = ((bx + block).min(w), (by + block).min(h));
                let mut sum = [0u32; 4];
                for y in by..y1 {
                    for x in bx..x1 {
                        let i = y as usize * stride + x as usize * 4;
                        for c in 0..4 {
                            sum[c] += src.data()[i + c] as u32;
                        }
                    }
                }
                let n = (x1 - bx) * (y1 - by);
                for y in by..y1 {
                    for x in bx..x1 {
                        let i = y as usize * stride + x as usize * 4;
                        for c in 0..4 {
                            out[i + c] = (sum[c] / n) as u8;
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn pixelate_matches_reference_including_partial_cells() {
        // Big enough to take the parallel path, with sizes that aren't block multiples.
        let src = noise(1001, 703);
        assert_eq!(pixelate(&src, 16).data(), &pixelate_reference(&src, 16)[..]);
        let small = noise(37, 23);
        assert_eq!(pixelate(&small, 8).data(), &pixelate_reference(&small, 8)[..]);
    }

    #[test]
    fn demultiply_matches_exact_division() {
        for a in 1..=255u32 {
            for c in 0..=a {
                let exact = ((c * 255 + a / 2) / a).min(255) as u8;
                assert_eq!(demultiply(c as u8, a as u8), exact, "c={c} a={a}");
            }
        }
    }

    #[test]
    fn conversions_handle_alpha_and_parallel_split() {
        let mut p = Pixmap::new(700, 700).unwrap(); // > 1 MB: parallel path
        p.data_mut()[..8].copy_from_slice(&[64, 0, 0, 128, 10, 20, 30, 255]);
        let bgra = to_bgra_straight(&p);
        assert_eq!(&bgra[..8], &[0, 0, 128, 128, 30, 20, 10, 255]);
        assert_eq!(&to_rgba_straight(&p)[..8], &[128, 0, 0, 128, 10, 20, 30, 255]);
        assert!(bgra[8..].iter().all(|b| *b == 0));
        assert_eq!(swap_red_blue(&[1, 2, 3, 4, 5, 6, 7, 8]), vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn split_bgrx_fixes_alpha_and_swaps() {
        let mut bgrx = vec![0u8; 700 * 700 * 4]; // parallel path
        bgrx[..8].copy_from_slice(&[1, 2, 3, 0, 4, 5, 6, 99]);
        let rgba = split_bgrx(&mut bgrx);
        assert_eq!(&bgrx[..8], &[1, 2, 3, 255, 4, 5, 6, 255]);
        assert_eq!(&rgba[..8], &[3, 2, 1, 255, 6, 5, 4, 255]);
        assert!(bgrx.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
    }

    #[test]
    fn crop_copies_rows() {
        let src = noise(50, 40);
        let c = crop(&src, 10, 5, 20, 7).unwrap();
        assert_eq!(c.pixel(0, 0), src.pixel(10, 5));
        assert_eq!(c.pixel(19, 6), src.pixel(29, 11));
    }
}
