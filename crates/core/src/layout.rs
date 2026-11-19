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
///
/// Room is always set aside for a style bar `reserve` tall (the tallest one any
/// tool shows), whether or not `style_bar` is showing now, so picking a tool
/// never makes both bars jump to the other side of the selection.
pub fn place_panels(
    sel: Rect,
    screen: Rect,
    toolbar: (f32, f32),
    style_bar: Option<(f32, f32)>,
    reserve: f32,
    gap: f32,
) -> PanelLayout {
    let (tw, th) = toolbar;
    let reserved = style_bar.map_or(reserve, |(_, h)| h.max(reserve));
    let extra = if reserved > 0.0 { reserved + gap } else { 0.0 };
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

/// Top-left corner for an in-capture toast of `size` centered over `sel`.
/// Prefers inside the top edge of `sel` (`inset` below `sel.y`). When that
/// would clip off `screen` or overlap any floating `panels` (toolbar / style bar),
/// tries above `sel` / above the panels, then below `sel` / below the panels,
/// and finally clamps within `screen`.
pub fn place_toast(
    sel: Rect,
    screen: Rect,
    size: (f32, f32),
    panels: &[Rect],
    inset: f32,
    gap: f32,
    margin: f32,
) -> Point {
    let (w, h) = size;
    let x = (sel.center().x - w / 2.0).clamp(screen.x + margin, (screen.right() - w - margin).max(screen.x + margin));
    let fits = |y: f32| y >= screen.y + margin && y + h <= screen.bottom() - margin;
    let clear = |y: f32| {
        let r = Rect::new(x, y, w, h);
        panels.iter().all(|p| r.intersect(p).is_none())
    };

    let inside = sel.y + inset;
    let mut candidates = [Some(inside), Some(sel.y - gap - h), None, Some(sel.bottom() + gap), None];
    if !panels.is_empty() {
        let min_y = panels.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
        let max_bottom = panels.iter().map(|p| p.bottom()).fold(f32::NEG_INFINITY, f32::max);
        candidates[2] = Some(min_y - gap - h);
        candidates[4] = Some(max_bottom + gap);
    }

    let y = candidates
        .into_iter()
        .flatten()
        .find(|&y| fits(y) && clear(y))
        .unwrap_or_else(|| inside.clamp(screen.y + margin, (screen.bottom() - h - margin).max(screen.y + margin)));
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
        let l = place_panels(Rect::new(100.0, 100.0, 500.0, 300.0), SCREEN, (300.0, 40.0), Some((200.0, 30.0)), 0.0, 8.0);
        assert_eq!(l.toolbar, Rect::new(300.0, 408.0, 300.0, 40.0));
        assert_eq!(l.style_bar, Some(Rect::new(300.0, 456.0, 200.0, 30.0)));
    }

    #[test]
    fn toolbar_flips_above_then_inside() {
        let l = place_panels(Rect::new(100.0, 300.0, 500.0, 480.0), SCREEN, (300.0, 40.0), None, 0.0, 8.0);
        assert_eq!(l.toolbar.y, 252.0);
        let full = place_panels(SCREEN, SCREEN, (300.0, 40.0), None, 0.0, 8.0);
        assert_eq!(full.toolbar.y, 752.0);
        assert_eq!(full.toolbar.x, 700.0);
    }

    #[test]
    fn showing_the_style_bar_never_moves_the_toolbar() {
        let (toolbar, style, gap) = ((300.0, 40.0), (200.0, 30.0), 8.0);
        // Selection bottoms that leave room below for the toolbar alone but not
        // both bars, both bars, or neither; and the same near the top edge.
        let cases = [
            Rect::new(100.0, 300.0, 500.0, 420.0), // 72 px below: toolbar fits, toolbar + style bar don't
            Rect::new(100.0, 300.0, 500.0, 300.0), // plenty below
            Rect::new(100.0, 30.0, 500.0, 750.0),  // no room above or below: inside
            Rect::new(100.0, 60.0, 500.0, 715.0),  // 52 px above: toolbar fits, both don't
        ];
        for sel in cases {
            let hidden = place_panels(sel, SCREEN, toolbar, None, style.1, gap);
            let shown = place_panels(sel, SCREEN, toolbar, Some(style), style.1, gap);
            assert_eq!(hidden.toolbar, shown.toolbar, "{sel:?}");
            // The style bar fits on screen and doesn't cover the toolbar.
            let s = shown.style_bar.unwrap();
            assert!(s.y >= SCREEN.y && s.bottom() <= SCREEN.bottom(), "{sel:?}: {s:?}");
            assert!(s.bottom() <= shown.toolbar.y || s.y >= shown.toolbar.bottom(), "{sel:?}");
        }
        // The first case is the reported one: everything goes above, even before a tool is picked.
        let l = place_panels(cases[0], SCREEN, toolbar, None, style.1, gap);
        assert_eq!(l.toolbar.y, 300.0 - gap - 40.0);
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

    #[test]
    fn toast_stays_inside_tall_selection_and_dodges_panels_or_edges_on_short_ones() {
        let toast = (260.0, 44.0);
        // Normal tall selection: stays inside the top of the selection.
        let sel = Rect::new(200.0, 100.0, 500.0, 400.0);
        let tb = Rect::new(300.0, 508.0, 400.0, 42.0);
        let p = place_toast(sel, SCREEN, toast, &[tb], 12.0, 8.0, 8.0);
        assert_eq!(p.y, 112.0);

        // Short single-line selection in the middle: `inside` would overlap toolbar below -> flips above selection.
        let short = Rect::new(200.0, 400.0, 500.0, 30.0);
        let tb = Rect::new(300.0, 438.0, 400.0, 42.0);
        let p = place_toast(short, SCREEN, toast, &[tb], 12.0, 8.0, 8.0);
        assert_eq!(p.y, 400.0 - 8.0 - 44.0);
        assert!(Rect::new(p.x, p.y, toast.0, toast.1).intersect(&tb).is_none());

        // Short selection near the top edge: no room above -> flips below toolbar.
        let top = Rect::new(200.0, 6.0, 500.0, 28.0);
        let tb = Rect::new(300.0, 42.0, 400.0, 42.0);
        let p = place_toast(top, SCREEN, toast, &[tb], 12.0, 8.0, 8.0);
        assert_eq!(p.y, tb.bottom() + 8.0);
        assert!(Rect::new(p.x, p.y, toast.0, toast.1).intersect(&tb).is_none());

        // Short selection at the bottom edge with toolbar & style bar above: never clips off-screen and clears panels.
        let bottom = Rect::new(200.0, 772.0, 500.0, 28.0);
        let tb = Rect::new(300.0, 722.0, 400.0, 42.0);
        let sb = Rect::new(300.0, 680.0, 320.0, 34.0);
        let p = place_toast(bottom, SCREEN, toast, &[tb, sb], 12.0, 8.0, 8.0);
        assert_eq!(p.y, sb.y - 8.0 - 44.0);
        assert!(p.y >= 8.0 && p.y + toast.1 <= SCREEN.bottom() - 8.0);
        assert!(Rect::new(p.x, p.y, toast.0, toast.1).intersect(&tb).is_none());
        assert!(Rect::new(p.x, p.y, toast.0, toast.1).intersect(&sb).is_none());
    }
}
