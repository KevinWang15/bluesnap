//! Annotation shapes: geometry, hit testing and editing handles.

use crate::geom::{Handle, Point, Rect, distance_to_segment};
use crate::style::{Style, Tool};
use crate::text::FontSet;

#[derive(Clone, Debug, PartialEq)]
pub enum Geometry {
    Rect(Rect),
    Ellipse(Rect),
    Arrow { from: Point, to: Point },
    Pen(Vec<Point>),
    Mosaic(Vec<Point>),
    /// `origin` is the top-left corner of the first line.
    Text { origin: Point, content: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    pub geometry: Geometry,
    pub style: Style,
}

/// What a size or hit test needs beyond the shape itself.
#[derive(Clone, Copy)]
pub struct Metrics<'a> {
    /// Screenshot pixels per logical pixel.
    pub scale: f32,
    pub fonts: &'a FontSet,
}

/// A draggable point that reshapes an annotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditHandle {
    Bounds(Handle),
    ArrowFrom,
    ArrowTo,
}

impl Shape {
    pub fn tool(&self) -> Tool {
        match self.geometry {
            Geometry::Rect(_) => Tool::Rect,
            Geometry::Ellipse(_) => Tool::Ellipse,
            Geometry::Arrow { .. } => Tool::Arrow,
            Geometry::Pen(_) => Tool::Pen,
            Geometry::Mosaic(_) => Tool::Mosaic,
            Geometry::Text { .. } => Tool::Text,
        }
    }

    /// Stroke width, brush width or font size in screenshot pixels.
    pub fn size_px(&self, scale: f32) -> f32 {
        self.style.size.logical_px(self.tool()) * scale
    }

    pub fn translated(&self, d: Point) -> Shape {
        let geometry = match &self.geometry {
            Geometry::Rect(r) => Geometry::Rect(r.translate(d)),
            Geometry::Ellipse(r) => Geometry::Ellipse(r.translate(d)),
            Geometry::Arrow { from, to } => Geometry::Arrow { from: *from + d, to: *to + d },
            Geometry::Pen(pts) => Geometry::Pen(pts.iter().map(|p| *p + d).collect()),
            Geometry::Mosaic(pts) => Geometry::Mosaic(pts.iter().map(|p| *p + d).collect()),
            Geometry::Text { origin, content } => Geometry::Text { origin: *origin + d, content: content.clone() },
        };
        Shape { geometry, style: self.style }
    }

    /// Area covered when rendered (including stroke width and arrow heads).
    pub fn bounds(&self, m: Metrics) -> Rect {
        let w = self.size_px(m.scale);
        match &self.geometry {
            Geometry::Rect(r) | Geometry::Ellipse(r) => r.inflate(w / 2.0 + 1.0),
            Geometry::Arrow { from, to } => Rect::from_points(*from, *to).inflate(arrow_head_len(w) + 1.0),
            Geometry::Pen(pts) | Geometry::Mosaic(pts) => points_bounds(pts).inflate(w / 2.0 + 1.0),
            Geometry::Text { origin, content } => {
                let layout = m.fonts.layout(content, w);
                Rect::new(origin.x, origin.y, layout.width.max(w / 2.0), layout.height())
            }
        }
    }

    /// Whether `p` grabs this shape. Outlines are hit only near the stroke so that
    /// one can still draw inside a rectangle or ellipse.
    pub fn hit(&self, p: Point, m: Metrics) -> bool {
        let w = self.size_px(m.scale);
        let tol = w / 2.0 + 5.0 * m.scale;
        match &self.geometry {
            Geometry::Rect(r) => {
                r.inflate(tol).contains(p) && !(r.w > 2.0 * tol && r.h > 2.0 * tol && r.inflate(-tol).contains(p))
            }
            Geometry::Ellipse(r) => {
                let (a, b) = (r.w / 2.0, r.h / 2.0);
                if a < 1.0 || b < 1.0 {
                    return r.inflate(tol).contains(p);
                }
                let c = r.center();
                let d = Point::new((p.x - c.x) / a, (p.y - c.y) / b).length();
                (d - 1.0).abs() * a.min(b) <= tol
            }
            Geometry::Arrow { from, to } => distance_to_segment(p, *from, *to) <= tol.max(arrow_head_len(w) / 2.0),
            Geometry::Pen(pts) | Geometry::Mosaic(pts) => {
                pts.windows(2).any(|s| distance_to_segment(p, s[0], s[1]) <= tol)
                    || pts.first().is_some_and(|q| q.distance(p) <= tol)
            }
            Geometry::Text { .. } => self.bounds(m).inflate(4.0 * m.scale).contains(p),
        }
    }

    pub fn handles(&self) -> Vec<(EditHandle, Point)> {
        match &self.geometry {
            Geometry::Rect(r) | Geometry::Ellipse(r) => {
                Handle::ALL.iter().map(|h| (EditHandle::Bounds(*h), r.handle_point(*h))).collect()
            }
            Geometry::Arrow { from, to } => vec![(EditHandle::ArrowFrom, *from), (EditHandle::ArrowTo, *to)],
            _ => Vec::new(),
        }
    }

    /// Copy of `self` with `handle` dragged by `delta`.
    pub fn with_handle_moved(&self, handle: EditHandle, delta: Point) -> Shape {
        let geometry = match (&self.geometry, handle) {
            (Geometry::Rect(r), EditHandle::Bounds(h)) => Geometry::Rect(r.resized(h, delta)),
            (Geometry::Ellipse(r), EditHandle::Bounds(h)) => Geometry::Ellipse(r.resized(h, delta)),
            (Geometry::Arrow { from, to }, EditHandle::ArrowFrom) => Geometry::Arrow { from: *from + delta, to: *to },
            (Geometry::Arrow { from, to }, EditHandle::ArrowTo) => Geometry::Arrow { from: *from, to: *to + delta },
            (g, _) => g.clone(),
        };
        Shape { geometry, style: self.style }
    }

    /// True for shapes too small to keep after the user releases the mouse.
    pub fn is_degenerate(&self) -> bool {
        match &self.geometry {
            Geometry::Rect(r) | Geometry::Ellipse(r) => r.w < 2.0 || r.h < 2.0,
            Geometry::Arrow { from, to } => from.distance(*to) < 4.0,
            Geometry::Pen(pts) | Geometry::Mosaic(pts) => pts.len() < 2,
            Geometry::Text { content, .. } => content.is_empty(),
        }
    }
}

pub fn arrow_head_len(stroke: f32) -> f32 {
    10.0 + stroke * 3.0
}

fn points_bounds(pts: &[Point]) -> Rect {
    let first = pts.first().copied().unwrap_or_default();
    pts.iter().fold(Rect::from_points(first, first), |r, p| r.union(&Rect::from_points(*p, *p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(fonts: &FontSet) -> Metrics<'_> {
        Metrics { scale: 1.0, fonts }
    }

    #[test]
    fn rect_is_hit_on_outline_only() {
        let fonts = FontSet::empty();
        let s = Shape { geometry: Geometry::Rect(Rect::new(0.0, 0.0, 200.0, 100.0)), style: Style::default() };
        assert!(s.hit(Point::new(1.0, 50.0), metrics(&fonts)));
        assert!(!s.hit(Point::new(100.0, 50.0), metrics(&fonts)));
    }

    #[test]
    fn ellipse_is_hit_near_perimeter() {
        let fonts = FontSet::empty();
        let s = Shape { geometry: Geometry::Ellipse(Rect::new(0.0, 0.0, 200.0, 100.0)), style: Style::default() };
        assert!(s.hit(Point::new(100.0, 1.0), metrics(&fonts)));
        assert!(!s.hit(Point::new(100.0, 50.0), metrics(&fonts)));
    }

    #[test]
    fn arrow_handles_move_independently() {
        let s = Shape {
            geometry: Geometry::Arrow { from: Point::new(0.0, 0.0), to: Point::new(10.0, 10.0) },
            style: Style::default(),
        };
        let moved = s.with_handle_moved(EditHandle::ArrowTo, Point::new(5.0, 0.0));
        assert_eq!(moved.geometry, Geometry::Arrow { from: Point::new(0.0, 0.0), to: Point::new(15.0, 10.0) });
    }
}
