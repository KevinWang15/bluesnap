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

/// Bounds for the live scrolling-capture preview beside `sel`: bottom-aligned with
/// `sel` on the right (or on the left when the right side lacks room), growing upward
/// past `sel.y` as the stitched `image` (`(width, height)`) gets taller. Once its top
/// reaches `margin` from the top of `screen`, its height stops growing and its width
/// shrinks proportionally so the whole stitched image continues to fit.
pub fn place_scrolling_preview(sel: Rect, screen: Rect, image: (u32, u32), gap: f32, margin: f32) -> Option<Rect> {
    let (img_w, img_h) = (image.0 as f32, image.1 as f32);
    if img_w <= 0.0 || img_h <= 0.0 {
        return None;
    }
    let max_h = sel.bottom() - (screen.y + margin);
    if max_h < 24.0 {
        return None;
    }

    let right_avail = screen.right() - margin - (sel.right() + gap);
    let left_avail = (sel.x - gap) - (screen.x + margin);
    let ideal_w = (sel.w * 0.42).clamp(140.0, 240.0);
    let (on_right, avail) = if right_avail >= ideal_w.min(140.0) || right_avail >= left_avail {
        (true, right_avail)
    } else {
        (false, left_avail)
    };
    if avail < 48.0 {
        return None;
    }

    let max_init_h = (sel.h * 0.6).min(max_h * 0.7);
    let aspect_cap_w = max_init_h * (sel.w / sel.h.max(1.0));
    let base_w = ideal_w.min(avail).min(aspect_cap_w).max(24.0_f32.min(avail));
    let scale = (base_w / img_w).min(max_h / img_h);
    let w = (img_w * scale).clamp(4.0, avail);
    let h = (img_h * scale).clamp(4.0, max_h);
    let x = if on_right { sel.right() + gap } else { sel.x - gap - w };
    let y = sel.bottom() - h;
    Some(Rect::new(x, y, w, h))
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

    #[test]
    fn scrolling_preview_grows_past_selection_then_shrinks_width_at_top_margin() {
        let screen = Rect::new(0.0, 0.0, 1200.0, 800.0);
        let sel = Rect::new(50.0, 200.0, 600.0, 400.0);

        // Initial frame: on the right, bottom-aligned with selection, shorter than selection.
        let p0 = place_scrolling_preview(sel, screen, (600, 400), 10.0, 10.0).unwrap();
        assert_eq!(p0.x, 660.0);
        assert_eq!(p0.bottom(), sel.bottom());
        assert_eq!(p0.w, 240.0);
        assert_eq!(p0.h, 160.0);

        // Taller than selection: grows above `sel.y` without changing width or bottom edge.
        let p1 = place_scrolling_preview(sel, screen, (600, 1200), 10.0, 10.0).unwrap();
        assert_eq!(p1.x, 660.0);
        assert_eq!(p1.bottom(), sel.bottom());
        assert_eq!(p1.w, 240.0);
        assert_eq!(p1.h, 480.0);
        assert!(p1.y < sel.y);

        // Reached 10px from top of screen (`max_h = 600 - 10 = 590`): height pins at 590
        // (`y == 10.0`) and width shrinks proportionally as height keeps doubling.
        let p2 = place_scrolling_preview(sel, screen, (600, 1770), 10.0, 10.0).unwrap();
        assert!((p2.y - 10.0).abs() < 1e-4);
        assert!((p2.bottom() - sel.bottom()).abs() < 1e-4);
        assert!((p2.h - 590.0).abs() < 1e-4);
        assert!((p2.w - 200.0).abs() < 1e-4);

        let p3 = place_scrolling_preview(sel, screen, (600, 3540), 10.0, 10.0).unwrap();
        assert!((p3.y - 10.0).abs() < 1e-4);
        assert!((p3.h - 590.0).abs() < 1e-4);
        assert!((p3.w - 100.0).abs() < 1e-4);
        assert_eq!(p3.x, 660.0);
    }

    #[test]
    fn scrolling_preview_prefers_right_flips_left_and_hides_when_full_width() {
        // Slightly right of center still chooses right when right side fits comfortably.
        let right_pref = place_scrolling_preview(Rect::new(400.0, 150.0, 430.0, 400.0), SCREEN, (430, 400), 10.0, 10.0).unwrap();
        assert_eq!(right_pref.x, 840.0);

        // Selection near right edge flips to left and keeps right edge anchored next to selection as it shrinks.
        let sel_right = Rect::new(450.0, 200.0, 500.0, 400.0);
        let left_initial = place_scrolling_preview(sel_right, SCREEN, (500, 400), 10.0, 10.0).unwrap();
        assert!((left_initial.right() - 440.0).abs() < 1e-4);
        assert_eq!(left_initial.bottom(), sel_right.bottom());

        let left_shrunk = place_scrolling_preview(sel_right, SCREEN, (500, 3000), 10.0, 10.0).unwrap();
        assert!((left_shrunk.right() - 440.0).abs() < 1e-4);
        assert!((left_shrunk.y - 10.0).abs() < 1e-4);
        assert!(left_shrunk.w < left_initial.w);

        // Full-width selection has room on neither side: never overlap the captured hole.
        assert_eq!(place_scrolling_preview(Rect::new(10.0, 100.0, 980.0, 500.0), SCREEN, (980, 500), 10.0, 10.0), None);
    }
}
