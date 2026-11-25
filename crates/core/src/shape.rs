//! Annotation shapes: geometry, hit testing and editing handles.

use crate::geom::{Handle, Point, Rect, distance_to_segment, resize_rect_with_handle};
use crate::style::{ArrowStyle, CounterFormat, RedactEffect, Style, TextBackdrop, TextStyle, Tool};
use crate::text::{FontSet, Fonts, TextLayout};

#[derive(Clone, Debug, PartialEq)]
pub enum Geometry {
    Rect(Rect),
    Ellipse(Rect),
    Arrow { from: Point, to: Point, style: ArrowStyle },
    Pen(Vec<Point>),
    /// A translucent stroke through the points.
    Highlight(Vec<Point>),
    /// A numbered marker centered on `center`. `number` is its place among the
    /// counters (1-based), kept in order by the editor.
    Counter { center: Point, number: u32, format: CounterFormat },
    /// Hides what's under `area` with `effect`; `strength` is the 0..=1 slider
    /// value (see [`RedactEffect::amount`]).
    Redact { area: RedactShape, effect: RedactEffect, strength: f32 },
    /// `origin` is the top-left corner of the first line. When `max_width` is
    /// `Some`, lines soft-wrap to that inner width; `min_height` keeps the box
    /// at least that tall without clipping reflowed lines.
    Text {
        origin: Point,
        content: String,
        style: TextStyle,
        max_width: Option<f32>,
        min_height: Option<f32>,
    },
}

/// The area a redaction covers.
#[derive(Clone, Debug, PartialEq)]
pub enum RedactShape {
    /// A freehand stroke, as wide as the shape's brush size.
    Brush(Vec<Point>),
    Rect(Rect),
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
    pub fonts: &'a Fonts,
}

/// A draggable point that reshapes an annotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditHandle {
    Bounds(Handle),
    ArrowFrom,
    ArrowTo,
}

impl Shape {
    /// Canvas bounds of a rectangular redaction, before rasterization padding.
    pub fn redaction_rect(&self) -> Option<Rect> {
        match &self.geometry {
            Geometry::Redact { area: RedactShape::Rect(r), .. } => Some(*r),
            _ => None,
        }
    }

    pub fn tool(&self) -> Tool {
        match self.geometry {
            Geometry::Rect(_) => Tool::Rect,
            Geometry::Ellipse(_) => Tool::Ellipse,
            Geometry::Arrow { .. } => Tool::Arrow,
            Geometry::Pen(_) => Tool::Pen,
            Geometry::Highlight(_) => Tool::Highlighter,
            Geometry::Counter { .. } => Tool::Counter,
            Geometry::Redact { .. } => Tool::Redact,
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
            Geometry::Arrow { from, to, style } => Geometry::Arrow { from: *from + d, to: *to + d, style: *style },
            Geometry::Pen(pts) => Geometry::Pen(pts.iter().map(|p| *p + d).collect()),
            Geometry::Highlight(pts) => Geometry::Highlight(pts.iter().map(|p| *p + d).collect()),
            Geometry::Counter { center, number, format } => {
                Geometry::Counter { center: *center + d, number: *number, format: *format }
            }
            Geometry::Redact { area, effect, strength } => Geometry::Redact {
                area: match area {
                    RedactShape::Brush(pts) => RedactShape::Brush(pts.iter().map(|p| *p + d).collect()),
                    RedactShape::Rect(r) => RedactShape::Rect(r.translate(d)),
                },
                effect: *effect,
                strength: *strength,
            },
            Geometry::Text { origin, content, style, max_width, min_height } => Geometry::Text {
                origin: *origin + d,
                content: content.clone(),
                style: *style,
                max_width: *max_width,
                min_height: *min_height,
            },
        };
        Shape { geometry, style: self.style }
    }

    /// Scales coordinates by `k` (e.g. when combining captures from displays
    /// with different scale factors into one composition).
    pub fn scaled(&self, k: f32) -> Shape {
        if (k - 1.0).abs() <= f32::EPSILON {
            return self.clone();
        }
        let geometry = match &self.geometry {
            Geometry::Rect(r) => Geometry::Rect(r.scale(k)),
            Geometry::Ellipse(r) => Geometry::Ellipse(r.scale(k)),
            Geometry::Arrow { from, to, style } => Geometry::Arrow { from: *from * k, to: *to * k, style: *style },
            Geometry::Pen(pts) => Geometry::Pen(pts.iter().map(|p| *p * k).collect()),
            Geometry::Highlight(pts) => Geometry::Highlight(pts.iter().map(|p| *p * k).collect()),
            Geometry::Counter { center, number, format } => {
                Geometry::Counter { center: *center * k, number: *number, format: *format }
            }
            Geometry::Redact { area, effect, strength } => Geometry::Redact {
                area: match area {
                    RedactShape::Brush(pts) => RedactShape::Brush(pts.iter().map(|p| *p * k).collect()),
                    RedactShape::Rect(r) => RedactShape::Rect(r.scale(k)),
                },
                effect: *effect,
                strength: *strength,
            },
            Geometry::Text { origin, content, style, max_width, min_height } => Geometry::Text {
                origin: *origin * k,
                content: content.clone(),
                style: *style,
                max_width: max_width.map(|w| w * k),
                min_height: min_height.map(|h| h * k),
            },
        };
        Shape { geometry, style: self.style }
    }

    /// Area covered when rendered (including stroke width and arrow heads).
    pub fn bounds(&self, m: Metrics) -> Rect {
        let w = self.size_px(m.scale);
        match &self.geometry {
            Geometry::Rect(r) | Geometry::Ellipse(r) => r.inflate(w / 2.0 + 1.0),
            Geometry::Arrow { from, to, style } => {
                points_bounds(&ArrowPath::new(*from, *to, *style, w).flatten()).inflate(arrow_head_len(w) + 1.0)
            }
            Geometry::Pen(pts) | Geometry::Highlight(pts) | Geometry::Redact { area: RedactShape::Brush(pts), .. } => {
                points_bounds(pts).inflate(w / 2.0 + 1.0)
            }
            Geometry::Counter { center, number, format } => {
                counter_box(*center, &format.label(*number), w, m.fonts.sans()).inflate(1.0)
            }
            Geometry::Redact { area: RedactShape::Rect(r), .. } => *r,
            Geometry::Text { .. } => self.text_rect(m).expect("text geometry").inflate(1.0),
        }
    }

    /// Whether `p` grabs this shape. Outlines are hit only near the stroke so that
    /// one can still draw inside a rectangle or ellipse; redaction areas are
    /// filled, so anywhere inside counts.
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
            Geometry::Arrow { from, to, style } => {
                let tol = tol.max(arrow_head_len(w) / 2.0);
                ArrowPath::new(*from, *to, *style, w).flatten().windows(2).any(|s| distance_to_segment(p, s[0], s[1]) <= tol)
            }
            Geometry::Pen(pts) | Geometry::Highlight(pts) | Geometry::Redact { area: RedactShape::Brush(pts), .. } => {
                pts.windows(2).any(|s| distance_to_segment(p, s[0], s[1]) <= tol)
                    || pts.first().is_some_and(|q| q.distance(p) <= tol)
            }
            Geometry::Counter { .. } => self.bounds(m).inflate(2.0 * m.scale).contains(p),
            Geometry::Redact { area: RedactShape::Rect(r), .. } => r.inflate(5.0 * m.scale).contains(p),
            Geometry::Text { .. } => self.bounds(m).inflate(4.0 * m.scale).contains(p),
        }
    }

    /// Outer visual box of a text shape (glyphs/wrap box plus style padding).
    pub fn text_rect(&self, m: Metrics) -> Option<Rect> {
        let Geometry::Text { origin, content, style, max_width, min_height } = &self.geometry else {
            return None;
        };
        let w = self.size_px(m.scale);
        let layout = m.fonts.get(style.family()).layout_wrapped(content, w, *max_width);
        Some(text_box(*origin, &layout, w, *style, *max_width, *min_height))
    }

    /// Interactive frame rectangle for a text shape's border and 8 resize handles:
    /// slightly inflated around [`Self::text_rect`] so the border and handles sit
    /// clear of glyphs and the editing caret.
    pub fn text_frame_rect(&self, m: Metrics) -> Option<Rect> {
        Some(self.text_rect(m)?.inflate(4.0 * m.scale))
    }

    pub fn handles(&self, m: Metrics) -> Vec<(EditHandle, Point)> {
        match &self.geometry {
            Geometry::Rect(r) | Geometry::Ellipse(r) | Geometry::Redact { area: RedactShape::Rect(r), .. } => {
                Handle::ALL.iter().map(|h| (EditHandle::Bounds(*h), r.handle_point(*h))).collect()
            }
            Geometry::Text { .. } => {
                let Some(r) = self.text_frame_rect(m) else { return Vec::new() };
                Handle::ALL.iter().map(|h| (EditHandle::Bounds(*h), r.handle_point(*h))).collect()
            }
            Geometry::Arrow { from, to, .. } => vec![(EditHandle::ArrowFrom, *from), (EditHandle::ArrowTo, *to)],
            _ => Vec::new(),
        }
    }

    /// Copy of `self` with `handle` dragged by `delta`. When `constrain` (`Shift`)
    /// is `true`, all 8 box handles preserve aspect ratio and arrow endpoints snap
    /// to 45° angles. For text shapes, resizing adjusts the text box and reflows
    /// the wrapped lines without scaling the font size.
    pub fn with_handle_moved(&self, handle: EditHandle, delta: Point, constrain: bool, m: Metrics) -> Shape {
        let resize_box = |r: &Rect, h: Handle| {
            if constrain {
                resize_rect_with_handle(*r, h, delta, true, 4.0)
            } else {
                r.resized(h, delta)
            }
        };
        let geometry = match (&self.geometry, handle) {
            (Geometry::Rect(r), EditHandle::Bounds(h)) => Geometry::Rect(resize_box(r, h)),
            (Geometry::Ellipse(r), EditHandle::Bounds(h)) => Geometry::Ellipse(resize_box(r, h)),
            (Geometry::Redact { area: RedactShape::Rect(r), effect, strength }, EditHandle::Bounds(h)) => {
                Geometry::Redact { area: RedactShape::Rect(resize_box(r, h)), effect: *effect, strength: *strength }
            }
            (Geometry::Text { origin, content, style, max_width, min_height }, EditHandle::Bounds(h)) => {
                let font_size = self.size_px(m.scale);
                let font_set = m.fonts.get(style.family());
                let orig_layout = font_set.layout_wrapped(content, font_size, *max_width);
                let orig_box = text_box(*origin, &orig_layout, font_size, *style, *max_width, *min_height);
                let (pad_x, pad_y) = text_padding(*style, font_size);
                let min_inner_w = (font_size * 1.5).max(16.0 * m.scale);
                let min_inner_h = orig_layout.line_height.max(font_size);
                let min_box_w = min_inner_w + 2.0 * pad_x;
                let min_box_h = min_inner_h + 2.0 * pad_y;
                let target_box = if constrain {
                    resize_rect_with_handle(orig_box, h, delta, true, min_box_w.min(min_box_h))
                } else {
                    resize_rect_with_handle(orig_box, h, delta, false, 4.0)
                };
                let changes_x = matches!(
                    h,
                    Handle::Left | Handle::Right | Handle::TopLeft | Handle::TopRight | Handle::BottomLeft | Handle::BottomRight
                );
                let changes_y = matches!(
                    h,
                    Handle::Top | Handle::Bottom | Handle::TopLeft | Handle::TopRight | Handle::BottomLeft | Handle::BottomRight
                );
                let pins_right = matches!(h, Handle::Left | Handle::TopLeft | Handle::BottomLeft);
                let pins_bottom = matches!(h, Handle::Top | Handle::TopLeft | Handle::TopRight);

                let next_max_width = if changes_x || constrain {
                    Some((target_box.w - 2.0 * pad_x).max(min_inner_w))
                } else {
                    *max_width
                };
                let next_layout = font_set.layout_wrapped(content, font_size, next_max_width);
                let next_min_height = if changes_y || constrain {
                    let desired = (target_box.h - 2.0 * pad_y).max(min_inner_h);
                    (desired > next_layout.height() + 0.5).then_some(desired)
                } else {
                    *min_height
                };
                let inner_w = next_max_width.map_or(next_layout.width, |w| w.max(next_layout.width)).max(font_size / 2.0);
                let inner_h = next_layout.height().max(next_min_height.unwrap_or(0.0));
                let next_x = if pins_right {
                    orig_box.right() - pad_x - inner_w
                } else if constrain && !changes_x {
                    target_box.x + pad_x
                } else {
                    origin.x
                };
                let next_y = if pins_bottom {
                    orig_box.bottom() - pad_y - inner_h
                } else if constrain && !changes_y {
                    target_box.y + pad_y
                } else {
                    origin.y
                };
                Geometry::Text {
                    origin: Point::new(next_x, next_y),
                    content: content.clone(),
                    style: *style,
                    max_width: next_max_width,
                    min_height: next_min_height,
                }
            }
            (Geometry::Arrow { from, to, style }, EditHandle::ArrowFrom) => {
                let next_from = if constrain { snap_45(*to, *from + delta) } else { *from + delta };
                Geometry::Arrow { from: next_from, to: *to, style: *style }
            }
            (Geometry::Arrow { from, to, style }, EditHandle::ArrowTo) => {
                let next_to = if constrain { snap_45(*from, *to + delta) } else { *to + delta };
                Geometry::Arrow { from: *from, to: next_to, style: *style }
            }
            (g, _) => g.clone(),
        };
        Shape { geometry, style: self.style }
    }

    /// True for shapes too small to keep after the user releases the mouse.
    pub fn is_degenerate(&self) -> bool {
        match &self.geometry {
            Geometry::Rect(r) | Geometry::Ellipse(r) | Geometry::Redact { area: RedactShape::Rect(r), .. } => {
                r.w < 2.0 || r.h < 2.0
            }
            Geometry::Arrow { from, to, .. } => from.distance(*to) < 4.0,
            Geometry::Pen(pts) | Geometry::Highlight(pts) | Geometry::Redact { area: RedactShape::Brush(pts), .. } => {
                pts.len() < 2
            }
            Geometry::Counter { .. } => false,
            Geometry::Text { content, .. } => content.is_empty(),
        }
    }
}

/// The area a text annotation covers: its glyphs' box at `origin` (at least
/// `max_width` wide and `min_height` tall when explicitly sized, and at least
/// a little wide while empty so there's something to click) plus room for
/// the style's outline or backdrop. `size` is the font size.
pub fn text_box(
    origin: Point,
    layout: &TextLayout,
    size: f32,
    style: TextStyle,
    max_width: Option<f32>,
    min_height: Option<f32>,
) -> Rect {
    let w = max_width.map_or(layout.width, |mw| mw.max(layout.width)).max(size / 2.0);
    let h = layout.height().max(min_height.unwrap_or(0.0));
    let glyphs = Rect::new(origin.x, origin.y, w, h);
    let (px, py) = text_padding(style, size);
    Rect::new(glyphs.x - px, glyphs.y - py, glyphs.w + 2.0 * px, glyphs.h + 2.0 * py)
}

/// Horizontal and vertical room a style needs around the glyphs' box.
pub fn text_padding(style: TextStyle, size: f32) -> (f32, f32) {
    match style.backdrop() {
        TextBackdrop::None => (0.0, 0.0),
        TextBackdrop::Outline => (size * 0.09, size * 0.09),
        TextBackdrop::Box | TextBackdrop::Card => (size * 0.32, size * 0.1),
        TextBackdrop::Pill => (size * 0.5, size * 0.1),
    }
}

/// A counter's marker: a circle of `diameter` around `center`, widened into a
/// pill when the label (like "XVIII") doesn't fit.
pub fn counter_box(center: Point, label: &str, diameter: f32, fonts: &FontSet) -> Rect {
    let text = fonts.layout(label, counter_font_size(diameter)).width;
    let w = diameter.max(text + diameter * 0.45);
    Rect::new(center.x - w / 2.0, center.y - diameter / 2.0, w, diameter)
}

pub fn counter_font_size(diameter: f32) -> f32 {
    diameter * 0.62
}

pub fn arrow_head_len(stroke: f32) -> f32 {
    10.0 + stroke * 3.0
}

fn snap_45(anchor: Point, p: Point) -> Point {
    let d = p - anchor;
    let len = d.length();
    if len == 0.0 {
        return p;
    }
    let step = std::f32::consts::FRAC_PI_4;
    let angle = (d.y.atan2(d.x) / step).round() * step;
    Point::new(anchor.x + len * angle.cos(), anchor.y + len * angle.sin())
}

/// The line an arrow is drawn along, from its start to its tip.
#[derive(Clone, Debug, PartialEq)]
pub enum ArrowPath {
    /// Straight segments through the points (two points for a straight
    /// arrow, four for an elbow), with corners rounded to `radius`.
    Segments { points: Vec<Point>, radius: f32 },
    /// A cubic Bézier curve: start, two control points, tip.
    Cubic([Point; 4]),
}

impl ArrowPath {
    /// The path of an arrow in `style` with lines `stroke` wide.
    pub fn new(from: Point, to: Point, style: ArrowStyle, stroke: f32) -> ArrowPath {
        let d = to - from;
        // Elbows and curves leave the start and reach the tip along the longer
        // axis, turning halfway: right, down, right for a wide arrow.
        let horizontal = d.x.abs() >= d.y.abs();
        let (major, minor) = if horizontal { (d.x.abs(), d.y.abs()) } else { (d.y.abs(), d.x.abs()) };
        let turns = if horizontal {
            let x = from.x + d.x / 2.0;
            [Point::new(x, from.y), Point::new(x, to.y)]
        } else {
            let y = from.y + d.y / 2.0;
            [Point::new(from.x, y), Point::new(to.x, y)]
        };
        let straight = ArrowPath::Segments { points: vec![from, to], radius: 0.0 };
        match style {
            ArrowStyle::Straight => straight,
            ArrowStyle::Elbow => {
                // A jog shorter than this looks like a slip of the hand, and the end
                // legs must hold the head with some line before it; arrows too
                // small for either are drawn straight.
                let min_jog = 3.0 * stroke + 10.0;
                let min_leg = arrow_head_len(stroke) + 3.0 * stroke + 6.0;
                if minor < min_jog || major / 2.0 < min_leg {
                    return straight;
                }
                let radius = (2.0 * stroke + 8.0).min(minor / 2.0);
                ArrowPath::Segments { points: vec![from, turns[0], turns[1], to], radius }
            }
            ArrowStyle::Curved => ArrowPath::Cubic([from, turns[0], turns[1], to]),
        }
    }

    /// Points along the path, close enough together for hit testing and bounds.
    pub fn flatten(&self) -> Vec<Point> {
        match self {
            ArrowPath::Segments { points, .. } => points.clone(),
            ArrowPath::Cubic(c) => (0..=32).map(|i| cubic_point(c, i as f32 / 32.0)).collect(),
        }
    }

    /// Where the tip points (a unit vector), judged over the last `head`
    /// pixels so a head on a curve lines up with the curve under it, and the
    /// path cut short by `cut` pixels so the line ends inside the head.
    pub fn split_tip(&self, head: f32, cut: f32) -> (Point, ArrowPath) {
        match self {
            ArrowPath::Segments { points, radius } => {
                let n = points.len();
                let (prev, tip) = (points[n - 2], points[n - 1]);
                let dir = (tip - prev).normalized();
                let mut points = points.clone();
                points[n - 1] = tip - dir * cut.min(prev.distance(tip));
                (dir, ArrowPath::Segments { points, radius: *radius })
            }
            ArrowPath::Cubic(c) => {
                let base = cubic_point(c, cubic_t_back(c, head));
                let dir = (c[3] - base).normalized();
                (dir, ArrowPath::Cubic(cubic_until(c, cubic_t_back(c, cut))))
            }
        }
    }
}

fn cubic_point(c: &[Point; 4], t: f32) -> Point {
    let [p01, p12, p23] = [c[0].lerp(c[1], t), c[1].lerp(c[2], t), c[2].lerp(c[3], t)];
    p01.lerp(p12, t).lerp(p12.lerp(p23, t), t)
}

/// The part of the curve from t = 0 to `t` (de Casteljau).
fn cubic_until(c: &[Point; 4], t: f32) -> [Point; 4] {
    let [p01, p12, p23] = [c[0].lerp(c[1], t), c[1].lerp(c[2], t), c[2].lerp(c[3], t)];
    let (p012, p123) = (p01.lerp(p12, t), p12.lerp(p23, t));
    [c[0], p01, p012, p012.lerp(p123, t)]
}

/// The last t at which the curve is `distance` pixels from its tip.
fn cubic_t_back(c: &[Point; 4], distance: f32) -> f32 {
    const STEPS: u32 = 256;
    (0..=STEPS)
        .rev()
        .map(|i| i as f32 / STEPS as f32)
        .find(|&t| cubic_point(c, t).distance(c[3]) >= distance)
        .unwrap_or(0.0)
}

fn points_bounds(pts: &[Point]) -> Rect {
    let first = pts.first().copied().unwrap_or_default();
    pts.iter().fold(Rect::from_points(first, first), |r, p| r.union(&Rect::from_points(*p, *p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(fonts: &Fonts) -> Metrics<'_> {
        Metrics { scale: 1.0, fonts }
    }

    #[test]
    fn rect_is_hit_on_outline_only() {
        let fonts = Fonts::empty();
        let s = Shape { geometry: Geometry::Rect(Rect::new(0.0, 0.0, 200.0, 100.0)), style: Style::default() };
        assert!(s.hit(Point::new(1.0, 50.0), metrics(&fonts)));
        assert!(!s.hit(Point::new(100.0, 50.0), metrics(&fonts)));
    }

    #[test]
    fn ellipse_is_hit_near_perimeter() {
        let fonts = Fonts::empty();
        let s = Shape { geometry: Geometry::Ellipse(Rect::new(0.0, 0.0, 200.0, 100.0)), style: Style::default() };
        assert!(s.hit(Point::new(100.0, 1.0), metrics(&fonts)));
        assert!(!s.hit(Point::new(100.0, 50.0), metrics(&fonts)));
    }

    #[test]
    fn arrow_handles_move_independently() {
        let fonts = Fonts::empty();
        let s = Shape {
            geometry: Geometry::Arrow { from: Point::new(0.0, 0.0), to: Point::new(10.0, 10.0), style: ArrowStyle::Elbow },
            style: Style::default(),
        };
        let moved = s.with_handle_moved(EditHandle::ArrowTo, Point::new(5.0, 0.0), false, metrics(&fonts));
        let style = ArrowStyle::Elbow;
        assert_eq!(moved.geometry, Geometry::Arrow { from: Point::new(0.0, 0.0), to: Point::new(15.0, 10.0), style });
    }

    #[test]
    fn elbow_arrows_turn_twice_along_the_longer_axis() {
        let path = |to: (f32, f32)| ArrowPath::new(Point::new(0.0, 0.0), Point::new(to.0, to.1), ArrowStyle::Elbow, 4.0);
        let points = |to| match path(to) {
            ArrowPath::Segments { points, .. } => points,
            other => panic!("{other:?}"),
        };
        // Wide: right, down, right, turning halfway across.
        assert_eq!(points((-200.0, 80.0)), [(0.0, 0.0), (-100.0, 0.0), (-100.0, 80.0), (-200.0, 80.0)].map(|(x, y)| Point::new(x, y)));
        // Tall: down, across, down.
        assert_eq!(points((60.0, 300.0)), [(0.0, 0.0), (0.0, 150.0), (60.0, 150.0), (60.0, 300.0)].map(|(x, y)| Point::new(x, y)));
        // A jog of a few pixels, or legs too short for the head, are drawn straight.
        assert_eq!(points((300.0, 15.0)).len(), 2);
        assert_eq!(points((60.0, 40.0)).len(), 2);
        let ArrowPath::Segments { radius, .. } = path((-200.0, 80.0)) else { unreachable!() };
        assert_eq!(radius, 16.0);
    }

    #[test]
    fn curved_arrows_reach_the_tip_along_the_longer_axis() {
        let path = ArrowPath::new(Point::new(0.0, 0.0), Point::new(300.0, 120.0), ArrowStyle::Curved, 4.0);
        let pts = path.flatten();
        assert_eq!((pts[0], pts[pts.len() - 1]), (Point::new(0.0, 0.0), Point::new(300.0, 120.0)));
        // An S: flat at both ends, through the middle halfway.
        assert!((pts[16].x - 150.0).abs() < 0.01 && (pts[16].y - 60.0).abs() < 0.01);
        let (dir, line) = path.split_tip(22.0, 13.0);
        assert!(dir.x > 0.99, "{dir:?}");
        let ArrowPath::Cubic(c) = line else { panic!() };
        assert!((c[3].distance(Point::new(300.0, 120.0)) - 13.0).abs() < 1.0);
        // Hit along the curve, not along the straight line between the ends.
        let fonts = Fonts::empty();
        let s = Shape {
            geometry: Geometry::Arrow { from: Point::new(0.0, 0.0), to: Point::new(300.0, 120.0), style: ArrowStyle::Curved },
            style: Style::default(),
        };
        assert!(s.hit(pts[4], metrics(&fonts)));
        assert!(!s.hit(Point::new(75.0, 30.0), metrics(&fonts)));
    }

    #[test]
    fn counters_are_hit_inside_and_move() {
        let fonts = Fonts::empty();
        let s = Shape {
            geometry: Geometry::Counter { center: Point::new(50.0, 50.0), number: 3, format: CounterFormat::Decimal },
            style: Style::default(),
        };
        assert_eq!(s.bounds(metrics(&fonts)), Rect::new(40.0, 40.0, 20.0, 20.0).inflate(1.0));
        assert!(s.hit(Point::new(50.0, 50.0), metrics(&fonts)));
        assert!(!s.hit(Point::new(80.0, 50.0), metrics(&fonts)));
        assert!(!s.is_degenerate());
        let moved = s.translated(Point::new(10.0, 0.0));
        assert!(matches!(moved.geometry, Geometry::Counter { center, number: 3, .. } if center == Point::new(60.0, 50.0)));
    }

    #[test]
    fn redaction_areas_are_hit_inside_and_resize_like_rectangles() {
        let fonts = Fonts::empty();
        let s = Shape {
            geometry: Geometry::Redact {
                area: RedactShape::Rect(Rect::new(0.0, 0.0, 200.0, 100.0)),
                effect: RedactEffect::Blur,
                strength: 0.5,
            },
            style: Style::default(),
        };
        assert!(s.hit(Point::new(100.0, 50.0), metrics(&fonts)));
        assert_eq!(s.handles(metrics(&fonts)).len(), 8);
        let moved = s.with_handle_moved(EditHandle::Bounds(Handle::Right), Point::new(20.0, 0.0), false, metrics(&fonts));
        assert_eq!(moved.bounds(metrics(&fonts)), Rect::new(0.0, 0.0, 220.0, 100.0));
        let constrained = s.with_handle_moved(EditHandle::Bounds(Handle::Bottom), Point::new(0.0, 50.0), true, metrics(&fonts));
        assert_eq!(constrained.bounds(metrics(&fonts)), Rect::new(-50.0, 0.0, 300.0, 150.0));
    }

    #[test]
    fn text_box_exposes_eight_handles_and_reflows_without_scaling_font() {
        use crate::text::FontSource;
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../platform/assets/fonts/Nunito.ttf");
        let fonts = Fonts::sans_only(FontSet::from_sources(&[FontSource::file(path).weight(800.0)]));
        let m = metrics(&fonts);
        let s = Shape {
            geometry: Geometry::Text {
                origin: Point::new(100.0, 50.0),
                content: "one two three four five".into(),
                style: TextStyle::Boxed,
                max_width: None,
                min_height: None,
            },
            style: Style::default(),
        };
        assert_eq!(s.handles(m).len(), 8);
        let orig = s.text_rect(m).unwrap();
        // Narrowing from Right pins the left edge and wraps text taller while keeping font size fixed.
        let narrowed = s.with_handle_moved(EditHandle::Bounds(Handle::Right), Point::new(-orig.w * 0.55, 0.0), false, m);
        let nr = narrowed.text_rect(m).unwrap();
        assert_eq!(narrowed.style.size, s.style.size);
        assert!((nr.x - orig.x).abs() < 0.01);
        assert!(nr.w < orig.w * 0.6);
        assert!(nr.h > orig.h * 1.5);
        // Narrowing from Left pins the right edge.
        let from_left = s.with_handle_moved(EditHandle::Bounds(Handle::Left), Point::new(orig.w * 0.5, 0.0), false, m);
        let lr = from_left.text_rect(m).unwrap();
        assert!((lr.right() - orig.right()).abs() < 0.5, "{lr:?} vs {orig:?}");
        assert!(lr.h > orig.h);
        // Dragging Top upward pins the bottom edge and grows the box upward.
        let taller = s.with_handle_moved(EditHandle::Bounds(Handle::Top), Point::new(0.0, -40.0), false, m);
        let tr = taller.text_rect(m).unwrap();
        assert!((tr.bottom() - orig.bottom()).abs() < 0.5);
        assert!(tr.h >= orig.h + 39.0);
    }
}
