//! The loupe beside the crosshair while picking a region: the pixels under the
//! cursor magnified, then where the cursor is, the size of what would be
//! selected, and the color under it (press I to copy it).

use std::sync::Arc;
use std::time::{Duration, Instant};

use bluesnap_core::layout::place_loupe;
use bluesnap_core::{Color, Document, Point as PxPoint, Rect as PxRect};
use gpui::{
    AnyElement, IntoElement, ParentElement, Pixels, Point, RenderImage, SharedString, Size, Styled, Window, div,
    prelude::FluentBuilder, px, rgb, rgba, white,
};

use crate::gpu_image::pixmap_image;

/// Pixels on each side of the one under the cursor (so 17 × 17 are shown).
const RADIUS: u32 = 8;
/// Logical size of each magnified pixel.
const CELL: f32 = 8.0;
const MAGNIFIER: f32 = (2 * RADIUS + 1) as f32 * CELL;
/// Size of the whole loupe, magnifier and info.
const SIZE: (f32, f32) = (MAGNIFIER, MAGNIFIER + 96.0);
/// Distance from the cursor.
const GAP: f32 = 20.0;
/// How long "Copied" shows after pressing I.
pub const COPIED_FOR: Duration = Duration::from_millis(500);

/// The magnified image, re-made only when the pixel under the cursor (or the
/// display scale) changes.
#[derive(Default)]
pub struct Loupe {
    key: Option<(i32, i32, u32)>,
    image: Option<Arc<RenderImage>>,
    copied_at: Option<Instant>,
}

/// `#RRGGBB`.
pub fn hex(c: Color) -> String {
    format!("#{:06X}", c.to_hex())
}

impl Loupe {
    /// Notes that the color was just copied, for the hint.
    pub fn copied(&mut self) {
        self.copied_at = Some(Instant::now());
    }

    pub fn drop_image(&mut self, window: &mut Window) {
        self.key = None;
        if let Some(old) = self.image.take() {
            window.drop_image(old).ok();
        }
    }

    /// The loupe for the cursor at `cursor` (window coordinates) over image
    /// pixel `pixel` of `doc`. `selection` is the size of what's highlighted
    /// (hovered window or region being dragged), `screen` the window size.
    pub fn element(
        &mut self,
        doc: &Document,
        pixel: (i32, i32),
        cursor: Point<Pixels>,
        selection: Option<PxRect>,
        screen: Size<Pixels>,
        window: &mut Window,
    ) -> AnyElement {
        // Drawn at device resolution so magnified pixels have crisp edges.
        let cell = (CELL * window.scale_factor()).round().max(1.0) as u32;
        let key = (pixel.0, pixel.1, cell);
        if self.key != Some(key) {
            self.key = Some(key);
            let image = bluesnap_core::pixels::magnify(doc.image(), pixel, RADIUS, cell).and_then(|p| pixmap_image(&p));
            if let Some(old) = std::mem::replace(&mut self.image, image) {
                // Each version is a new texture; free the previous one.
                window.drop_image(old).ok();
            }
        }
        let screen = PxRect::new(0.0, 0.0, f32::from(screen.width), f32::from(screen.height));
        let at = place_loupe(PxPoint::new(f32::from(cursor.x), f32::from(cursor.y)), SIZE, screen, GAP);
        let color = doc.color_at(pixel.0, pixel.1);
        let copied = self.copied_at.is_some_and(|t| t.elapsed() < COPIED_FOR);
        let center = RADIUS as f32 * CELL;

        let row = |label: &'static str, value: SharedString| {
            div()
                .flex()
                .justify_between()
                .child(div().text_color(rgba(0xFFFF_FF8C)).child(label))
                .child(div().child(value))
        };
        let swatch = color.map_or(rgb(0x000000).into(), |c| gpui::Hsla::from(rgb(c.to_hex())));
        div()
            .absolute()
            .left(px(at.x))
            .top(px(at.y))
            .w(px(SIZE.0))
            .h(px(SIZE.1))
            .rounded(px(8.))
            .overflow_hidden()
            .bg(rgba(0x1F1F_1FF2))
            .border_1()
            .border_color(rgba(0xFFFF_FF26))
            .shadow_lg()
            .text_color(white())
            .text_size(px(12.))
            .child(
                div()
                    .relative()
                    .size(px(MAGNIFIER))
                    .when_some(self.image.clone(), |d, image| d.child(gpui::img(image).size(px(MAGNIFIER))))
                    // The center row and column, tinted, and the center pixel outlined.
                    .child(div().absolute().left_0().top(px(center)).w(px(MAGNIFIER)).h(px(CELL)).bg(rgba(0x1677_FF2E)))
                    .child(div().absolute().top_0().left(px(center)).h(px(MAGNIFIER)).w(px(CELL)).bg(rgba(0x1677_FF2E)))
                    .child(
                        div()
                            .absolute()
                            .left(px(center - 1.))
                            .top(px(center - 1.))
                            .size(px(CELL + 2.))
                            .border_1()
                            .border_color(white())
                            .child(div().size_full().border_1().border_color(rgba(0x0000_0099))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .px(px(8.))
                    .py(px(6.))
                    .child(row("Position", format!("{}, {}", pixel.0, pixel.1).into()))
                    .when_some(selection, |d, r| {
                        d.child(row("Size", format!("{} × {}", r.w.round() as i32, r.h.round() as i32).into()))
                    })
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(div().text_color(rgba(0xFFFF_FF8C)).child("Color"))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.))
                                    .child(div().size(px(10.)).rounded(px(2.)).bg(swatch).border_1().border_color(rgba(0xFFFF_FF4D)))
                                    .child(color.map_or("—".to_string(), hex)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(if copied { rgb(0x73D13D).into() } else { gpui::Hsla::from(rgba(0xFFFF_FF73)) })
                            .child(if copied { "✓ Copied" } else { "Press I to copy color" }),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_print_as_hex() {
        assert_eq!(hex(Color::rgb(0x1677FF)), "#1677FF");
        assert_eq!(hex(Color::rgb(0x00000A)), "#00000A");
    }
}
