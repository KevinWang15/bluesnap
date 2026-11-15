//! Mapping between image pixels (what the editor works in) and window
//! coordinates (gpui logical pixels).

use bluesnap_core::{Point as Px, Rect as PxRect};
use gpui::{Bounds, Pixels, Point, Size, point, px, size};

/// Where the image sits in the window and how large it is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    /// Window position of image pixel (0, 0).
    pub origin: Point<Pixels>,
    /// Logical pixels per image pixel.
    pub zoom: f32,
}

impl Viewport {
    /// The image at its native resolution from the window's top-left corner:
    /// one image pixel per device pixel.
    pub fn native(scale_factor: f32) -> Self {
        Self { origin: point(px(0.), px(0.)), zoom: 1.0 / scale_factor }
    }

    /// Image pixels per logical pixel on screen (for hit-testing tolerances).
    pub fn ui_scale(self) -> f32 {
        1.0 / self.zoom
    }

    pub fn to_image(self, p: Point<Pixels>) -> Px {
        Px::new(f32::from(p.x - self.origin.x) / self.zoom, f32::from(p.y - self.origin.y) / self.zoom)
    }

    pub fn point_to_window(self, p: Px) -> Point<Pixels> {
        point(self.origin.x + px(p.x * self.zoom), self.origin.y + px(p.y * self.zoom))
    }

    pub fn to_window(self, r: PxRect) -> Bounds<Pixels> {
        Bounds::new(self.point_to_window(Px::new(r.x, r.y)), size(px(r.w * self.zoom), px(r.h * self.zoom)))
    }

    /// `r` in window coordinates as a plain rectangle (for layout helpers).
    pub fn to_window_rect(self, r: PxRect) -> PxRect {
        let b = self.to_window(r);
        PxRect::new(f32::from(b.origin.x), f32::from(b.origin.y), f32::from(b.size.width), f32::from(b.size.height))
    }

    /// The part of the image (in image pixels) a window of `window` size shows.
    pub fn visible_image_rect(self, window: Size<Pixels>) -> PxRect {
        let a = self.to_image(point(px(0.), px(0.)));
        let b = self.to_image(point(window.width, window.height));
        PxRect::from_points(a, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_viewport_maps_device_pixels() {
        let v = Viewport::native(2.0);
        assert_eq!(v.to_image(point(px(10.), px(20.))), Px::new(20.0, 40.0));
        assert_eq!(v.to_window(PxRect::new(20.0, 40.0, 100.0, 50.0)), Bounds::new(point(px(10.), px(20.)), size(px(50.), px(25.))));
        assert_eq!(v.ui_scale(), 2.0);
    }

    #[test]
    fn scrolled_zoomed_viewport_round_trips() {
        // Image drawn at half size, 16 px from the left, scrolled so image row 400
        // is at the window's top edge plus a 60 px bar.
        let v = Viewport { origin: point(px(16.), px(60. - 200.)), zoom: 0.5 };
        let p = Px::new(100.0, 400.0);
        assert_eq!(v.to_image(v.point_to_window(p)), p);
        assert_eq!(v.point_to_window(p), point(px(66.), px(60.)));
        let visible = v.visible_image_rect(size(px(800.), px(600.)));
        assert_eq!((visible.y, visible.bottom()), (280.0, 1480.0));
    }
}
