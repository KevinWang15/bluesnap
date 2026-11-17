//! Geometry in screenshot pixel space (physical pixels, origin top-left).

use std::ops::{Add, Mul, Sub};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn distance(self, other: Point) -> f32 {
        (self - other).length()
    }

    pub fn length(self) -> f32 {
        self.x.hypot(self.y)
    }

    /// This vector scaled to length 1 (zero stays zero).
    pub fn normalized(self) -> Point {
        let len = self.length();
        if len == 0.0 { self } else { self * (1.0 / len) }
    }

    /// The point a fraction `t` of the way from `self` to `other`.
    pub fn lerp(self, other: Point, t: f32) -> Point {
        self + (other - self) * t
    }
}

impl Mul<f32> for Point {
    type Output = Point;
    fn mul(self, k: f32) -> Point {
        Point::new(self.x * k, self.y * k)
    }
}

impl Add for Point {
    type Output = Point;
    fn add(self, o: Point) -> Point {
        Point::new(self.x + o.x, self.y + o.y)
    }
}

impl Sub for Point {
    type Output = Point;
    fn sub(self, o: Point) -> Point {
        Point::new(self.x - o.x, self.y - o.y)
    }
}

/// Axis-aligned rectangle. Constructors normalize so `w` and `h` are never negative.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self::from_edges(x, y, x + w, y + h)
    }

    pub fn from_points(a: Point, b: Point) -> Self {
        Self::from_edges(a.x, a.y, b.x, b.y)
    }

    pub fn from_edges(l: f32, t: f32, r: f32, b: f32) -> Self {
        let (l, r) = if l <= r { (l, r) } else { (r, l) };
        let (t, b) = if t <= b { (t, b) } else { (b, t) };
        Self { x: l, y: t, w: r - l, h: b - t }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn center(&self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x <= self.right() && p.y >= self.y && p.y <= self.bottom()
    }

    pub fn inflate(&self, d: f32) -> Rect {
        Rect::from_edges(self.x - d, self.y - d, self.right() + d, self.bottom() + d)
    }

    pub fn translate(&self, d: Point) -> Rect {
        Rect { x: self.x + d.x, y: self.y + d.y, ..*self }
    }

    pub fn scale(&self, k: f32) -> Rect {
        Rect::new(self.x * k, self.y * k, self.w * k, self.h * k)
    }

    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let l = self.x.max(o.x);
        let t = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        (r > l && b > t).then(|| Rect::from_edges(l, t, r, b))
    }

    pub fn union(&self, o: &Rect) -> Rect {
        Rect::from_edges(
            self.x.min(o.x),
            self.y.min(o.y),
            self.right().max(o.right()),
            self.bottom().max(o.bottom()),
        )
    }

    /// Snaps outward to whole pixels.
    pub fn round_out(&self) -> Rect {
        Rect::from_edges(self.x.floor(), self.y.floor(), self.right().ceil(), self.bottom().ceil())
    }

    /// Snaps each edge to the nearest whole pixel.
    pub fn round(&self) -> Rect {
        Rect::from_edges(self.x.round(), self.y.round(), self.right().round(), self.bottom().round())
    }

    /// Moves the rectangle (keeping its size) so it lies inside `bounds` where possible.
    pub fn clamp_within(&self, bounds: &Rect) -> Rect {
        let x = self.x.min(bounds.right() - self.w).max(bounds.x);
        let y = self.y.min(bounds.bottom() - self.h).max(bounds.y);
        Rect { x, y, ..*self }
    }

    pub fn handle_point(&self, h: Handle) -> Point {
        let (fx, fy) = h.factors();
        Point::new(self.x + self.w * fx, self.y + self.h * fy)
    }

    /// The handle within `radius` of `p`, if any. Corners win over edges.
    pub fn handle_at(&self, p: Point, radius: f32) -> Option<Handle> {
        Handle::ALL
            .into_iter()
            .filter(|h| self.handle_point(*h).distance(p) <= radius)
            .min_by(|a, b| {
                let da = self.handle_point(*a).distance(p);
                let db = self.handle_point(*b).distance(p);
                da.total_cmp(&db).then_with(|| b.is_corner().cmp(&a.is_corner()))
            })
    }

    /// Returns `self` with the edges attached to `h` moved by `delta`. The result is
    /// normalized, so dragging an edge past its opposite flips the rectangle.
    pub fn resized(&self, h: Handle, delta: Point) -> Rect {
        let (mut l, mut t, mut r, mut b) = (self.x, self.y, self.right(), self.bottom());
        let (fx, fy) = h.factors();
        if fx == 0.0 {
            l += delta.x;
        } else if fx == 1.0 {
            r += delta.x;
        }
        if fy == 0.0 {
            t += delta.y;
        } else if fy == 1.0 {
            b += delta.y;
        }
        Rect::from_edges(l, t, r, b)
    }
}

/// One of the eight resize handles on a rectangle's border.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

impl Handle {
    pub const ALL: [Handle; 8] = [
        Handle::TopLeft,
        Handle::Top,
        Handle::TopRight,
        Handle::Right,
        Handle::BottomRight,
        Handle::Bottom,
        Handle::BottomLeft,
        Handle::Left,
    ];

    /// Position along the rectangle as fractions of width and height.
    fn factors(self) -> (f32, f32) {
        match self {
            Handle::TopLeft => (0.0, 0.0),
            Handle::Top => (0.5, 0.0),
            Handle::TopRight => (1.0, 0.0),
            Handle::Right => (1.0, 0.5),
            Handle::BottomRight => (1.0, 1.0),
            Handle::Bottom => (0.5, 1.0),
            Handle::BottomLeft => (0.0, 1.0),
            Handle::Left => (0.0, 0.5),
        }
    }

    pub fn is_corner(self) -> bool {
        matches!(self, Handle::TopLeft | Handle::TopRight | Handle::BottomRight | Handle::BottomLeft)
    }

    pub fn cursor(self) -> CursorKind {
        match self {
            Handle::TopLeft | Handle::BottomRight => CursorKind::ResizeNwSe,
            Handle::TopRight | Handle::BottomLeft => CursorKind::ResizeNeSw,
            Handle::Top | Handle::Bottom => CursorKind::ResizeNs,
            Handle::Left | Handle::Right => CursorKind::ResizeEw,
        }
    }
}

/// Mouse cursor the frontend should show; mapped to native cursors by the UI layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorKind {
    Default,
    Crosshair,
    Move,
    Text,
    ResizeNwSe,
    ResizeNeSw,
    ResizeNs,
    ResizeEw,
}

/// Distance from `p` to the segment `a`-`b`.
pub fn distance_to_segment(p: Point, a: Point, b: Point) -> f32 {
    let ab = b - a;
    let len2 = ab.x * ab.x + ab.y * ab.y;
    if len2 == 0.0 {
        return p.distance(a);
    }
    let ap = p - a;
    let t = ((ap.x * ab.x + ap.y * ab.y) / len2).clamp(0.0, 1.0);
    p.distance(Point::new(a.x + ab.x * t, a.y + ab.y * t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_flips_when_dragged_past_opposite_edge() {
        let r = Rect::new(10.0, 10.0, 100.0, 50.0);
        let flipped = r.resized(Handle::Left, Point::new(150.0, 0.0));
        assert_eq!(flipped, Rect::from_edges(110.0, 10.0, 160.0, 60.0));
    }

    #[test]
    fn edge_handle_only_moves_its_edge() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert_eq!(r.resized(Handle::Top, Point::new(30.0, 20.0)), Rect::new(0.0, 20.0, 100.0, 80.0));
    }

    #[test]
    fn handle_hit_prefers_nearest() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert_eq!(r.handle_at(Point::new(1.0, 1.0), 6.0), Some(Handle::TopLeft));
        // Equidistant between `Top` (5, 0) and `TopRight` (10, 0): corner wins.
        assert_eq!(r.handle_at(Point::new(7.5, 0.0), 6.0), Some(Handle::TopRight));
        assert_eq!(r.handle_at(Point::new(50.0, 50.0), 6.0), None);
    }
}
