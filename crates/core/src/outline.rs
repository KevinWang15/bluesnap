//! The visible outline of a captured window, for exact "snap to window" output.
//!
//! Window managers report windows as rectangles, but Windows 11 and macOS draw
//! them with rounded corners, so a rectangular crop of a snapped window carries
//! slivers of whatever was behind its corners. [`WindowOutline::clip`] makes
//! those pixels transparent, anti-aliased like the compositor draws them.

use tiny_skia::{Pixmap, PremultipliedColorU8};

use crate::geom::{Point, Rect};

/// Supersamples per axis when computing how much of an edge pixel the window covers.
const SUBSAMPLES: u32 = 8;
/// How far inside the arc (in pixels) edge pixels take their color from, so
/// the background blended into them by the compositor doesn't show.
const INSET: f32 = 1.5;

/// Window picking only needs bounds on hover. Resolve the compositor's outline
/// once, on click, before the user starts annotating the selection.
pub trait PickableWindow: Send + Sync {
    fn bounds(&self) -> Rect;
    fn outline(&self) -> WindowOutline;
}

impl PickableWindow for Rect {
    fn bounds(&self) -> Rect {
        *self
    }

    fn outline(&self) -> WindowOutline {
        (*self).into()
    }
}

impl PickableWindow for WindowOutline {
    fn bounds(&self) -> Rect {
        self.bounds
    }

    fn outline(&self) -> WindowOutline {
        self.clone()
    }
}

/// A horizontal run of compositor pixels, in window-local coordinates.
#[derive(Clone, Debug, PartialEq)]
struct CornerRun {
    x: u32,
    y: u32,
    pixels: Vec<PremultipliedColorU8>,
}

/// Full window bounds and the compositor pixels or geometric radius for its corners.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowOutline {
    pub bounds: Rect,
    /// Corner radius in screenshot pixels; `0` for square corners.
    pub corner_radius: f32,
    corners: Vec<CornerRun>,
}

impl From<Rect> for WindowOutline {
    fn from(bounds: Rect) -> Self {
        Self::new(bounds, 0.0)
    }
}

impl WindowOutline {
    pub fn new(bounds: Rect, corner_radius: f32) -> Self {
        let corner_radius = if corner_radius.is_finite() { corner_radius.max(0.0) } else { 0.0 };
        Self { bounds, corner_radius, corners: Vec::new() }
    }

    /// Save the nonopaque pixels connected to each corner of a shadow-free
    /// window image. Keep their premultiplied colors as well as alpha: the
    /// desktop has already blended the background into anti-aliased edges.
    pub fn from_window_image(bounds: Rect, image: &Pixmap) -> Self {
        let mut outline = Self::from(bounds);
        let (w, h) = (image.width(), image.height());
        if (bounds.w, bounds.h) != (w as f32, h as f32) {
            return outline;
        }
        for bottom in [false, true] {
            for right in [false, true] {
                let x_at = |dx| if right { w - 1 - dx } else { dx };
                let y_at = |dy| if bottom { h - 1 - dy } else { dy };
                let half_width = if right { w / 2 } else { w.div_ceil(2) };
                let half_height = if bottom { h / 2 } else { h.div_ceil(2) };
                // A straight edge can itself be translucent. Stop where the
                // edge reaches its interior alpha, including on glass windows.
                let width = corner_extent(half_width, |dx| image.pixel(x_at(dx), y_at(0)).unwrap().alpha());
                let height = corner_extent(half_height, |dy| image.pixel(x_at(0), y_at(dy)).unwrap().alpha());
                for dy in 0..height {
                    let y = y_at(dy);
                    let start = y as usize * w as usize;
                    let row = &image.pixels()[start..start + w as usize];
                    let (x, len) = if right {
                        let n = row.iter().rev().take(width as usize).take_while(|p| p.alpha() < 255).count();
                        (row.len() - n, n)
                    } else {
                        (0, row.iter().take(width as usize).take_while(|p| p.alpha() < 255).count())
                    };
                    if len == 0 {
                        break;
                    }
                    outline.corners.push(CornerRun { x: x as u32, y, pixels: row[x..x + len].to_vec() });
                }
            }
        }
        outline
    }

    /// The radius actually drawn: never more than half the shorter side.
    fn radius(&self) -> f32 {
        self.corner_radius.min(self.bounds.w / 2.0).min(self.bounds.h / 2.0).max(0.0)
    }

    pub fn is_rounded(&self) -> bool {
        self.radius() > 0.0 || !self.corners.is_empty()
    }

    /// Makes the pixels of `pixmap` (whose top-left pixel is at `origin` in
    /// screenshot pixels) outside the rounded outline transparent. Pixels the
    /// arc passes through get partial alpha, colored from just inside the
    /// window. Corners outside `pixmap` (e.g. off-screen) are left alone, and
    /// only the corner squares are visited, so this costs nothing measurable.
    pub fn clip(&self, pixmap: &mut Pixmap, origin: Point) {
        let dx = self.bounds.x as i64 - origin.x as i64;
        let dy = self.bounds.y as i64 - origin.y as i64;
        let width = i64::from(pixmap.width());
        for run in &self.corners {
            let y = dy + i64::from(run.y);
            if !(0..i64::from(pixmap.height())).contains(&y) {
                continue;
            }
            let x = dx + i64::from(run.x);
            let left = x.max(0);
            let right = (x + run.pixels.len() as i64).min(width);
            if left < right {
                let dst = (y * width + left) as usize;
                let src = (left - x) as usize;
                let n = (right - left) as usize;
                pixmap.pixels_mut()[dst..dst + n].copy_from_slice(&run.pixels[src..src + n]);
            }
        }
        let r = self.radius();
        if r <= 0.0 {
            return;
        }
        let b = self.bounds;
        // Corner centers and the direction from each center towards its corner.
        let corners = [
            (Point::new(b.x + r, b.y + r), (-1.0, -1.0)),
            (Point::new(b.right() - r, b.y + r), (1.0, -1.0)),
            (Point::new(b.x + r, b.bottom() - r), (-1.0, 1.0)),
            (Point::new(b.right() - r, b.bottom() - r), (1.0, 1.0)),
        ];
        let (w, h) = (pixmap.width() as i64, pixmap.height() as i64);
        for (center, (sx, sy)) in corners {
            // The corner square, in pixmap pixels.
            let (ax, bx) = if sx < 0.0 { (center.x - r, center.x) } else { (center.x, center.x + r) };
            let (ay, by) = if sy < 0.0 { (center.y - r, center.y) } else { (center.y, center.y + r) };
            let x0 = ((ax - origin.x).floor() as i64).clamp(0, w);
            let x1 = ((bx - origin.x).ceil() as i64).clamp(0, w);
            let y0 = ((ay - origin.y).floor() as i64).clamp(0, h);
            let y1 = ((by - origin.y).ceil() as i64).clamp(0, h);
            for py in y0..y1 {
                for px in x0..x1 {
                    let cell = Point::new(origin.x + px as f32, origin.y + py as f32);
                    let covered = coverage(cell, center, (sx, sy), r);
                    if covered == SUBSAMPLES * SUBSAMPLES {
                        continue;
                    }
                    let color = if covered == 0 {
                        [0; 4]
                    } else {
                        let inner = inside_pixel(cell, center, r, origin, (w, h));
                        let src = pixel(pixmap, inner);
                        let a = (covered * 255 + SUBSAMPLES * SUBSAMPLES / 2) / (SUBSAMPLES * SUBSAMPLES);
                        // Premultiplied: scale the (opaque) inner color by the coverage.
                        src.map(|c| ((c as u32 * a + 127) / 255) as u8)
                    };
                    set_pixel(pixmap, (px, py), color);
                }
            }
        }
    }
}

fn corner_extent(limit: u32, alpha: impl Fn(u32) -> u8) -> u32 {
    // The first pixel at maximum coverage marks the start of the straight edge.
    // Fully opaque edges are common, so finish as soon as coverage reaches 255.
    let mut highest = 0;
    let mut extent = 0;
    for i in 0..limit {
        let a = alpha(i);
        if a > highest {
            highest = a;
            extent = i;
        }
        if a == 255 {
            break;
        }
    }
    // A completely clear outer edge can be padding around the entire window.
    if highest == 0 { limit } else { extent }
}

/// How many of the cell's subsamples fall inside the arc around `center` (the
/// outline is only rounded on the `dir` side of the center).
fn coverage(cell: Point, center: Point, dir: (f32, f32), r: f32) -> u32 {
    let step = 1.0 / SUBSAMPLES as f32;
    let mut inside = 0;
    for j in 0..SUBSAMPLES {
        let y = cell.y + (j as f32 + 0.5) * step;
        let dy = ((y - center.y) * dir.1).max(0.0);
        for i in 0..SUBSAMPLES {
            let x = cell.x + (i as f32 + 0.5) * step;
            let dx = ((x - center.x) * dir.0).max(0.0);
            if dx * dx + dy * dy <= r * r {
                inside += 1;
            }
        }
    }
    inside
}

/// The pixmap pixel `INSET` pixels inside the arc along the line from the
/// cell to the corner center: fully covered, so its color is the window's own.
fn inside_pixel(cell: Point, center: Point, r: f32, origin: Point, (w, h): (i64, i64)) -> (i64, i64) {
    let c = Point::new(cell.x + 0.5, cell.y + 0.5);
    let (vx, vy) = (c.x - center.x, c.y - center.y);
    let d = (vx * vx + vy * vy).sqrt();
    let target = (r - INSET).max(0.0);
    let (x, y) = if d > target && d > 0.0 { (center.x + vx * target / d, center.y + vy * target / d) } else { (c.x, c.y) };
    (((x - origin.x).floor() as i64).clamp(0, w - 1), ((y - origin.y).floor() as i64).clamp(0, h - 1))
}

fn pixel(pixmap: &Pixmap, (x, y): (i64, i64)) -> [u8; 4] {
    let i = (y as usize * pixmap.width() as usize + x as usize) * 4;
    let d = &pixmap.data()[i..i + 4];
    [d[0], d[1], d[2], d[3]]
}

fn set_pixel(pixmap: &mut Pixmap, (x, y): (i64, i64), color: [u8; 4]) {
    let i = (y as usize * pixmap.width() as usize + x as usize) * 4;
    pixmap.data_mut()[i..i + 4].copy_from_slice(&color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pixels::pixmap_from_rgba;

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> Pixmap {
        pixmap_from_rgba(w, h, (0..w * h).flat_map(|_| [rgb[0], rgb[1], rgb[2], 255]).collect()).unwrap()
    }

    fn alpha(p: &Pixmap, x: u32, y: u32) -> u8 {
        p.pixel(x, y).unwrap().alpha()
    }

    #[test]
    fn rounded_corners_become_transparent_and_edges_antialias() {
        let mut p = solid(100, 60, [200, 100, 50]);
        WindowOutline::new(Rect::new(0.0, 0.0, 100.0, 60.0), 10.0).clip(&mut p, Point::default());
        // Every corner pixel is gone; the middle of each edge and the center stay.
        for (x, y) in [(0, 0), (99, 0), (0, 59), (99, 59), (1, 1), (98, 58)] {
            assert_eq!(alpha(&p, x, y), 0, "({x}, {y})");
        }
        for (x, y) in [(50, 0), (0, 30), (99, 30), (50, 59), (50, 30), (10, 10), (89, 49)] {
            assert_eq!(alpha(&p, x, y), 255, "({x}, {y})");
        }
        // The arc passes through partially covered pixels.
        assert!((1..255).contains(&alpha(&p, 1, 4)));
        // The four corners are mirror images of each other.
        for y in 0..10 {
            for x in 0..10 {
                let a = alpha(&p, x, y);
                assert_eq!(a, alpha(&p, 99 - x, y));
                assert_eq!(a, alpha(&p, x, 59 - y));
                assert_eq!(a, alpha(&p, 99 - x, 59 - y));
            }
        }
    }

    #[test]
    fn edge_pixels_take_the_window_color_not_the_background() {
        // A window colored (40, 40, 40) with an 8 px corner radius; the
        // compositor blended a bright background into every pixel the arc touches.
        let mut p = solid(40, 40, [40, 40, 40]);
        for y in 0..8 {
            for x in 0..8 {
                let cell = Point::new(x as f32, y as f32);
                if coverage(cell, Point::new(8.0, 8.0), (-1.0, -1.0), 8.0) < SUBSAMPLES * SUBSAMPLES {
                    set_pixel(&mut p, (x, y), [250, 250, 250, 255]);
                }
            }
        }
        WindowOutline::new(Rect::new(0.0, 0.0, 40.0, 40.0), 8.0).clip(&mut p, Point::default());
        for y in 0..8 {
            for x in 0..8 {
                let px = p.pixel(x, y).unwrap();
                if px.alpha() > 0 {
                    // Demultiplied, every visible pixel is the window's own gray (give or take 8-bit rounding).
                    let c = px.demultiply();
                    assert!(c.red() <= 46 && c.green() <= 46, "({x}, {y}) = {c:?}");
                }
            }
        }
    }

    #[test]
    fn corners_outside_the_crop_are_untouched() {
        // A window hanging off the top-left of the screen, cropped to its visible part.
        let outline = WindowOutline::new(Rect::new(-50.0, -50.0, 150.0, 120.0), 10.0);
        let mut p = solid(100, 70, [10, 20, 30]);
        outline.clip(&mut p, Point::default());
        // The visible top-left is the middle of the window: opaque.
        assert_eq!(alpha(&p, 0, 0), 255);
        // The bottom-right corner is real and rounded.
        assert_eq!(alpha(&p, 99, 69), 0);
        assert_eq!(alpha(&p, 99, 0), 255);
        assert_eq!(alpha(&p, 0, 69), 255);
    }

    #[test]
    fn native_corners_preserve_window_edge_colors_and_frozen_content() {
        let mut native = solid(9, 7, [80, 90, 100]);
        let edge = [90, 0, 0, 100]; // premultiplied red, as captured in isolation
        set_pixel(&mut native, (0, 0), [0; 4]);
        set_pixel(&mut native, (1, 0), edge);
        set_pixel(&mut native, (0, 1), edge);
        set_pixel(&mut native, (8, 6), edge);
        set_pixel(&mut native, (4, 3), [0; 4]);
        let outline = WindowOutline::from_window_image(Rect::new(-1.0, 1.0, 9.0, 7.0), &native);
        let mut crop = solid(8, 7, [20, 30, 40]);
        outline.clip(&mut crop, Point::new(0.0, 1.0));
        assert_eq!(pixel(&crop, (0, 0)), edge);
        assert_eq!(pixel(&crop, (7, 6)), edge);
        assert_eq!(pixel(&crop, (7, 0)), [20, 30, 40, 255]); // square corner
        assert_eq!(pixel(&crop, (3, 3)), [20, 30, 40, 255]); // frozen desktop
    }

    #[test]
    fn uniform_translucent_square_keeps_its_frozen_backdrop() {
        let mut glass = solid(50, 32, [0, 0, 0]);
        for pixel in glass.data_mut().chunks_exact_mut(4) {
            pixel[3] = 128;
        }
        let outline = WindowOutline::from_window_image(Rect::new(0.0, 0.0, 50.0, 32.0), &glass);
        assert!(!outline.is_rounded());
    }

    #[test]
    fn square_or_degenerate_outlines_change_nothing() {
        let before = solid(20, 20, [1, 2, 3]);
        for outline in [
            WindowOutline::from(Rect::new(0.0, 0.0, 20.0, 20.0)),
            WindowOutline::new(Rect::new(0.0, 0.0, 20.0, 20.0), f32::NAN),
            WindowOutline::new(Rect::new(0.0, 0.0, 20.0, 20.0), -4.0),
        ] {
            assert!(!outline.is_rounded());
            let mut p = before.clone();
            outline.clip(&mut p, Point::default());
            assert_eq!(p.data(), before.data());
        }
        // A radius larger than the window is limited to half its shorter side.
        let mut p = solid(20, 10, [1, 2, 3]);
        WindowOutline::new(Rect::new(0.0, 0.0, 20.0, 10.0), 100.0).clip(&mut p, Point::default());
        assert_eq!(alpha(&p, 10, 0), 255);
        assert_eq!(alpha(&p, 0, 0), 0);
    }
}
