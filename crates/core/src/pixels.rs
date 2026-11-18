//! Whole-buffer pixel conversions. A 4K screenshot is 33 MB, so these split the
//! work across cores and avoid per-pixel branches and divisions.

use tiny_skia::{IntSize, Pixmap};

/// Below this many bytes, threads cost more than they save.
const PARALLEL_MIN_BYTES: usize = 1 << 20;

fn threads_for(len: usize) -> usize {
    if len < PARALLEL_MIN_BYTES {
        return 1;
    }
    threads()
}

/// Threads to split work across.
fn threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get()).min(8)
}

/// `f` over each of `items`, in parallel, results in order.
pub fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    if items.len() < 2 {
        return items.iter().map(f).collect();
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let chunk = items.len().div_ceil(cores);
    std::thread::scope(|scope| {
        let f = &f;
        let handles: Vec<_> =
            items.chunks(chunk).map(|part| scope.spawn(move || part.iter().map(f).collect::<Vec<_>>())).collect();
        handles.into_iter().flat_map(|h| h.join().expect("worker thread panicked")).collect()
    })
}

/// Runs `f(i, band)` over the `band_len`-byte bands of `out`, on threads when
/// there are several (a thread costs tens of microseconds to start).
fn par_bands(out: &mut [u8], band_len: usize, f: impl Fn(usize, &mut [u8]) + Sync) {
    if out.len() <= band_len {
        return f(0, out);
    }
    std::thread::scope(|scope| {
        for (i, band) in out.chunks_mut(band_len).enumerate() {
            let f = &f;
            scope.spawn(move || f(i, band));
        }
    });
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

/// Copies the `w`x`h` block at (`x`, `y`) of `src`, returning `None` if empty
/// or out of bounds.
pub fn crop(src: &Pixmap, x: u32, y: u32, w: u32, h: u32) -> Option<Pixmap> {
    if x.checked_add(w).is_none_or(|r| r > src.width()) || y.checked_add(h).is_none_or(|b| b > src.height()) {
        return None;
    }
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

/// Box widths whose three successive passes approximate a gaussian of `sigma`
/// (Kovesi, "Fast almost-Gaussian filtering").
fn gaussian_boxes(sigma: f32) -> [usize; 3] {
    let n = 3.0;
    let ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut lower = ideal.floor() as i32;
    if lower % 2 == 0 {
        lower -= 1;
    }
    let lower = lower.max(1);
    let wl = lower as f32;
    let m = ((12.0 * sigma * sigma - n * wl * wl - 4.0 * n * wl - 3.0 * n) / (-4.0 * wl - 4.0)).round() as i32;
    std::array::from_fn(|i| if (i as i32) < m { lower as usize } else { lower as usize + 2 })
}

/// Rows of context a gaussian blur of `sigma` needs on each side.
pub fn blur_margin(sigma: f32) -> usize {
    gaussian_boxes(sigma).iter().map(|w| w / 2).sum::<usize>() + 1
}

/// Rounded division by a box window's size, as a multiply and shift.
#[derive(Clone, Copy)]
struct Mean {
    reciprocal: u32,
    half: u32,
}

impl Mean {
    const SHIFT: u32 = 16;

    fn new(radius: usize) -> Self {
        let window = (2 * radius + 1) as u32;
        Self { reciprocal: (1u32 << Self::SHIFT).div_ceil(window), half: window / 2 }
    }

    /// `sum / window`, rounded; `sum` is at most 255 × window.
    fn of(self, sum: u32) -> u8 {
        (((sum + self.half) * self.reciprocal) >> Self::SHIFT).min(255) as u8
    }
}

/// `src`, `width` × `height` pixels, flipped over its diagonal: row `y` of the
/// result is column `y` of `src`. In tiles that stay in cache, and in bands of
/// output rows across threads.
fn transpose(src: &[u8], width: usize, height: usize) -> Vec<u8> {
    const TILE: usize = 32;
    let src = src.as_chunks::<4>().0;
    let mut out = vec![0u8; width * height * 4];
    let threads = threads_for(out.len());
    let band = width.div_ceil(threads).div_ceil(TILE).max(1) * TILE;
    par_bands(&mut out, band * height * 4, |i, rows| {
        let rows = rows.as_chunks_mut::<4>().0;
        let x0 = i * band;
        let x1 = (x0 + rows.len() / height).min(width);
        for tx in (x0..x1).step_by(TILE) {
            for ty in (0..height).step_by(TILE) {
                for x in tx..(tx + TILE).min(x1) {
                    let row = &mut rows[(x - x0) * height..(x - x0 + 1) * height];
                    for y in ty..(ty + TILE).min(height) {
                        row[y] = src[y * width + x];
                    }
                }
            }
        }
    });
    out
}

/// Box blur of `radius` down each column of `src` into `dst`, edges extended:
/// running sums over a band of rows' worth of columns at once, so memory is
/// read sequentially. Bands of output rows are independent and split across
/// threads (each starts its sums from its own first row).
fn box_columns(src: &[u8], dst: &mut [u8], width: usize, height: usize, radius: usize) {
    let stride = width * 4;
    let threads = threads_for(dst.len());
    let band = height.div_ceil(threads).max(1);
    #[cfg(target_arch = "x86_64")]
    let avx2 = std::arch::is_x86_feature_detected!("avx2");
    par_bands(dst, band * stride, |i, out| {
        let y0 = i * band;
        #[cfg(target_arch = "x86_64")]
        if avx2 {
            // SAFETY: the CPU has AVX2, checked above.
            return unsafe { box_columns_band_avx2(src, out, stride, height, y0, radius) };
        }
        box_columns_band(src, out, stride, height, y0, radius)
    });
}

/// [`box_columns`] for the output rows from `y0` on, in `out`.
#[inline(always)]
fn box_columns_band(src: &[u8], out: &mut [u8], stride: usize, height: usize, y0: usize, radius: usize) {
    let mean = Mean::new(radius);
    let row = |y: isize| {
        let y = y.clamp(0, height as isize - 1) as usize;
        &src[y * stride..(y + 1) * stride]
    };
    let (y0, r) = (y0 as isize, radius as isize);
    let mut sum = vec![0u32; stride];
    for y in y0 - r..=y0 + r {
        sum.iter_mut().zip(row(y)).for_each(|(s, v)| *s += *v as u32);
    }
    for (dy, line) in out.chunks_exact_mut(stride).enumerate() {
        line.iter_mut().zip(&sum).for_each(|(o, s)| *o = mean.of(*s));
        let y = y0 + dy as isize;
        let (add, sub) = (row(y + r + 1), row(y - r));
        sum.iter_mut().zip(add).zip(sub).for_each(|((s, a), b)| *s = *s + *a as u32 - *b as u32);
    }
}

/// [`box_columns_band`] compiled for AVX2, which multiplies eight 32-bit
/// lanes at once (baseline x86-64 has no 32-bit vector multiply): about a
/// third faster.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn box_columns_band_avx2(src: &[u8], out: &mut [u8], stride: usize, height: usize, y0: usize, radius: usize) {
    box_columns_band(src, out, stride, height, y0, radius)
}

/// Approximately gaussian blur of `sigma` pixels, edges extended.
pub fn gaussian_blur(src: &Pixmap, sigma: f32) -> Pixmap {
    let (w, h) = (src.width() as usize, src.height() as usize);
    let mut a = src.data().to_vec();
    let mut b = vec![0u8; a.len()];
    let boxes = gaussian_boxes(sigma);
    // Box blurs are separable and commute: all horizontal passes, then all
    // vertical. The horizontal ones run down the columns of the transposed
    // image: the column pass updates whole rows of sums at once, which
    // vectorizes, where sliding along a row goes one pixel at a time.
    let radii: Vec<usize> = boxes.into_iter().filter(|b| *b > 1).map(|b| b / 2).collect();
    if !radii.is_empty() {
        a = transpose(&a, w, h);
        b.resize(a.len(), 0);
        for &r in &radii {
            box_columns(&a, &mut b, h, w, r);
            std::mem::swap(&mut a, &mut b);
        }
        a = transpose(&a, h, w);
    }
    for &r in &radii {
        box_columns(&a, &mut b, w, h, r);
        std::mem::swap(&mut a, &mut b);
    }
    pixmap_from_rgba(src.width(), src.height(), a).expect("same size as the source")
}

/// Wraps `rgba` (premultiplied, or opaque) as a pixmap.
pub fn pixmap_from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Option<Pixmap> {
    Pixmap::from_vec(rgba, IntSize::from_wh(width, height)?)
}

/// Imports straight-alpha pixels from a decoded image into the renderer.
pub fn pixmap_from_straight_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Option<Pixmap> {
    let mut pixmap = pixmap_from_rgba(width, height, rgba)?;
    for [r, g, b, a] in pixmap.data_mut().as_chunks_mut::<4>().0 {
        if *a < 255 {
            let alpha = u16::from(*a);
            for channel in [r, g, b] {
                *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
            }
        }
    }
    Some(pixmap)
}

/// The pixels around `center` magnified for a loupe: (2·`radius` + 1)² pixels,
/// each drawn as a `cell`-sized square (nearest neighbor, so pixels stay
/// sharp), with a faint grid between them. Pixels outside the image are dark.
pub fn magnify(image: &Pixmap, center: (i32, i32), radius: u32, cell: u32) -> Option<Pixmap> {
    let n = 2 * radius + 1;
    let size = n * cell;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for gy in 0..n {
        for gx in 0..n {
            let (x, y) = (center.0 + gx as i32 - radius as i32, center.1 + gy as i32 - radius as i32);
            let rgb = (x >= 0 && y >= 0)
                .then(|| image.pixel(x as u32, y as u32))
                .flatten()
                .map_or([0x26, 0x26, 0x2B], |p| [p.red(), p.green(), p.blue()]);
            for cy in 0..cell {
                let row = ((gy * cell + cy) * size + gx * cell) as usize * 4;
                for cx in 0..cell {
                    // The grid: each cell's last row and column, darkened a little.
                    let edge = cell >= 4 && (cx == cell - 1 || cy == cell - 1);
                    let px = &mut out[row + cx as usize * 4..row + cx as usize * 4 + 4];
                    for (o, c) in px.iter_mut().zip(rgb) {
                        *o = if edge { (c as u16 * 7 / 8) as u8 } else { c };
                    }
                    px[3] = 255;
                }
            }
        }
    }
    pixmap_from_rgba(size, size, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnify_scales_pixels_up_sharp_and_darkens_outside() {
        // 2x2 image: red, green / blue, white.
        let image = pixmap_from_rgba(2, 2, [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255], [255; 4]].concat()).unwrap();
        let zoomed = magnify(&image, (0, 0), 1, 4).unwrap();
        assert_eq!((zoomed.width(), zoomed.height()), (12, 12));
        let px = |x, y| {
            let p = zoomed.pixel(x, y).unwrap();
            (p.red(), p.green(), p.blue())
        };
        // The center cell is (0, 0), red; right of it green, below it blue.
        assert_eq!(px(5, 5), (255, 0, 0));
        assert_eq!(px(9, 5), (0, 255, 0));
        assert_eq!(px(5, 9), (0, 0, 255));
        // Cell edges are the grid, a little darker; left and above is outside the image.
        assert_eq!(px(7, 5), (223, 0, 0));
        assert_eq!(px(1, 1), (0x26, 0x26, 0x2B));
    }

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
    fn blur_smooths_but_keeps_the_average() {
        let src = noise(120, 90);
        let out = gaussian_blur(&src, 4.0);
        let stats = |p: &Pixmap| {
            let v: Vec<f64> = p.data().chunks(4).map(|c| c[0] as f64).collect();
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            let var = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / v.len() as f64;
            (mean, var)
        };
        let ((m0, v0), (m1, v1)) = (stats(&src), stats(&out));
        assert!((m0 - m1).abs() < 2.0, "mean {m0} -> {m1}");
        assert!(v1 < v0 / 10.0, "variance {v0} -> {v1}");
        assert!(out.data().chunks(4).all(|c| c[3] == 255));
    }

    /// The blur's horizontal passes as they were before going through a
    /// transpose: sliding along each row.
    fn box_rows_reference(src: &[u8], dst: &mut [u8], width: usize, radius: usize) {
        let mean = Mean::new(radius);
        let stride = width * 4;
        let px = |row: &[u8], i: isize| -> [u32; 4] {
            let p = i.clamp(0, width as isize - 1) as usize * 4;
            [row[p] as u32, row[p + 1] as u32, row[p + 2] as u32, row[p + 3] as u32]
        };
        par_zip(dst, src, stride, |dst, src| {
            for (out, row) in dst.chunks_exact_mut(stride).zip(src.chunks_exact(stride)) {
                let r = radius as isize;
                let mut sum = [0u32; 4];
                for i in -r..=r {
                    let p = px(row, i);
                    (0..4).for_each(|c| sum[c] += p[c]);
                }
                for (x, o) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    (0..4).for_each(|c| o[c] = mean.of(sum[c]));
                    let (add, sub) = (x as isize + r + 1, x as isize - r);
                    if sub >= 0 && (add as usize) < width {
                        // Interior: no clamping.
                        let (a, b) = (&row[add as usize * 4..add as usize * 4 + 4], &row[sub as usize * 4..sub as usize * 4 + 4]);
                        (0..4).for_each(|c| sum[c] = sum[c] + a[c] as u32 - b[c] as u32);
                    } else {
                        let (a, b) = (px(row, add), px(row, sub));
                        (0..4).for_each(|c| sum[c] = sum[c] + a[c] - b[c]);
                    }
                }
            }
        });
    }

    #[test]
    fn horizontal_passes_match_sliding_along_rows() {
        let (w, h) = (197usize, 61usize);
        let src: Vec<u8> = (0..w * h * 4).map(|i| ((i * 7919) % 251) as u8).collect();
        for r in [1, 3, 10, 120] {
            let mut expected = vec![0u8; src.len()];
            box_rows_reference(&src, &mut expected, w, r);
            let t = transpose(&src, w, h);
            let mut blurred = vec![0u8; t.len()];
            box_columns(&t, &mut blurred, h, w, r);
            assert!(transpose(&blurred, h, w) == expected, "radius {r}");
        }
    }

    #[test]
    fn blurring_a_band_with_its_margin_matches_the_whole_image() {
        let src = noise(80, 200);
        let sigma = 3.5;
        let whole = gaussian_blur(&src, sigma);
        let margin = blur_margin(sigma) as u32;
        let (b0, b1) = (60u32, 130u32);
        let (s0, s1) = (b0 - margin, (b1 + margin).min(200));
        let band = gaussian_blur(&crop(&src, 0, s0, 80, s1 - s0).unwrap(), sigma);
        let got = crop(&band, 0, b0 - s0, 80, b1 - b0).unwrap();
        assert!(got.data() == crop(&whole, 0, b0, 80, b1 - b0).unwrap().data());
    }

    #[test]
    fn crop_copies_rows() {
        let src = noise(50, 40);
        let c = crop(&src, 10, 5, 20, 7).unwrap();
        assert_eq!(c.pixel(0, 0), src.pixel(10, 5));
        assert_eq!(c.pixel(19, 6), src.pixel(29, 11));
        assert!(crop(&src, 40, 5, 20, 7).is_none());
        assert!(crop(&src, 10, 35, 20, 7).is_none());
    }
}
