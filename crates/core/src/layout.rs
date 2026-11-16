//! Placement of the floating UI (toolbar, style bar, size label) around the selection.
//! All values are in the same units as the inputs, so callers may use logical pixels.

use crate::geom::{Point, Rect};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelLayout {
    pub toolbar: Rect,
    pub style_bar: Option<Rect>,
}

/// Puts the toolbar under the selection, right-aligned with it. Falls back to above
/// the selection, then to inside its bottom edge, when there is no room on screen.
/// The style bar sits on the far side of the toolbar from the selection.
pub fn place_panels(sel: Rect, screen: Rect, toolbar: (f32, f32), style_bar: Option<(f32, f32)>, gap: f32) -> PanelLayout {
    let (tw, th) = toolbar;
    let extra = style_bar.map_or(0.0, |(_, h)| h + gap);
    let x = (sel.right() - tw).min(screen.right() - tw).max(screen.x);

    let below = sel.bottom() + gap;
    let above = sel.y - gap - th;
    let (ty, style_below) = if below + th + extra <= screen.bottom() {
        (below, true)
    } else if above - extra >= screen.y {
        (above, false)
    } else {
        ((sel.bottom() - gap - th).min(screen.bottom() - th - extra).max(screen.y), true)
    };

    let toolbar = Rect::new(x, ty, tw, th);
    let style_bar = style_bar.map(|(sw, sh)| {
        let sx = x.min(screen.right() - sw).max(screen.x);
        let sy = if style_below { ty + th + gap } else { ty - gap - sh };
        Rect::new(sx, sy, sw, sh)
    });
    PanelLayout { toolbar, style_bar }
}

/// Top-left corner of the loupe (a tooltip of `size` beside the cursor):
/// below and right of it, each axis flipping to the other side when there's
/// no room on `screen`, then kept on screen.
pub fn place_loupe(cursor: Point, size: (f32, f32), screen: Rect, gap: f32) -> Point {
    let (w, h) = size;
    let x = if cursor.x + gap + w <= screen.right() { cursor.x + gap } else { cursor.x - gap - w };
    let y = if cursor.y + gap + h <= screen.bottom() { cursor.y + gap } else { cursor.y - gap - h };
    Point::new(x.min(screen.right() - w).max(screen.x), y.min(screen.bottom() - h).max(screen.y))
}

/// Top-left corner for the "W × H" label: above the selection, or inside it at the top.
pub fn place_size_label(sel: Rect, screen: Rect, size: (f32, f32), gap: f32) -> Point {
    let x = sel.x.min(screen.right() - size.0).max(screen.x);
    let above = sel.y - gap - size.1;
    let y = if above >= screen.y { above } else { sel.y + gap };
    Point::new(x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 800.0 };

    #[test]
    fn loupe_flips_to_whichever_sides_have_room() {
        let size = (150.0, 220.0);
        let at = |x, y| place_loupe(Point::new(x, y), size, SCREEN, 20.0);
        assert_eq!(at(100.0, 100.0), Point::new(120.0, 120.0)); // below right
        assert_eq!(at(950.0, 100.0), Point::new(780.0, 120.0)); // below left
        assert_eq!(at(100.0, 700.0), Point::new(120.0, 460.0)); // above right
        assert_eq!(at(950.0, 700.0), Point::new(780.0, 460.0)); // above left
        // Too big for either side: kept on screen.
        assert_eq!(place_loupe(Point::new(10.0, 10.0), (990.0, 790.0), SCREEN, 20.0), Point::new(0.0, 0.0));
    }

    #[test]
    fn toolbar_goes_below_when_room() {
        let l = place_panels(Rect::new(100.0, 100.0, 500.0, 300.0), SCREEN, (300.0, 40.0), Some((200.0, 30.0)), 8.0);
        assert_eq!(l.toolbar, Rect::new(300.0, 408.0, 300.0, 40.0));
        assert_eq!(l.style_bar, Some(Rect::new(300.0, 456.0, 200.0, 30.0)));
    }

    #[test]
    fn toolbar_flips_above_then_inside() {
        let l = place_panels(Rect::new(100.0, 300.0, 500.0, 480.0), SCREEN, (300.0, 40.0), None, 8.0);
        assert_eq!(l.toolbar.y, 252.0);
        let full = place_panels(SCREEN, SCREEN, (300.0, 40.0), None, 8.0);
        assert_eq!(full.toolbar.y, 752.0);
        assert_eq!(full.toolbar.x, 700.0);
    }
}
