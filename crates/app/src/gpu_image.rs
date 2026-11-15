//! Getting images onto the GPU: the (possibly very tall) screenshot as tiles, and
//! cached annotation layers.

use std::ops::Range;
use std::sync::Arc;

use bluesnap_core::Rect as PxRect;
use bluesnap_core::render::to_bgra_straight;
use bluesnap_core::Pixmap;
use gpui::{Bounds, Corners, Pixels, RenderImage, Window};

use crate::viewport::Viewport;

/// Rows per tile. GPU textures are limited to 16,384 px; long screenshots go
/// beyond that, and smaller tiles let scrolling upload only what's visible.
const TILE_ROWS: u32 = 4096;

fn render_image(width: u32, height: u32, bgra: Vec<u8>) -> Option<Arc<RenderImage>> {
    let buffer = image::RgbaImage::from_raw(width, height, bgra)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

/// A premultiplied pixmap as a gpui image (straight-alpha BGRA).
pub fn pixmap_image(pixmap: &Pixmap) -> Option<Arc<RenderImage>> {
    render_image(pixmap.width(), pixmap.height(), to_bgra_straight(pixmap))
}

/// Row ranges of the tiles for an image `height` rows tall.
fn tile_rows(height: u32) -> Vec<Range<u32>> {
    (0..height).step_by(TILE_ROWS as usize).map(|y| y..(y + TILE_ROWS).min(height)).collect()
}

/// An opaque image split into horizontal tiles.
pub struct ImageTiles {
    tiles: Vec<(PxRect, Arc<RenderImage>)>,
}

impl ImageTiles {
    /// From opaque BGRA pixels (gpui's layout). A single tile takes the buffer as is.
    pub fn from_bgra(width: u32, height: u32, bgra: Vec<u8>) -> Option<Self> {
        if bgra.len() != width as usize * height as usize * 4 {
            return None;
        }
        let rows = tile_rows(height);
        let stride = width as usize * 4;
        let tiles = if rows.len() == 1 {
            vec![(PxRect::new(0.0, 0.0, width as f32, height as f32), render_image(width, height, bgra)?)]
        } else {
            rows.into_iter()
                .map(|r| {
                    let bytes = bgra[r.start as usize * stride..r.end as usize * stride].to_vec();
                    let rect = PxRect::new(0.0, r.start as f32, width as f32, r.len() as f32);
                    Some((rect, render_image(width, r.len() as u32, bytes)?))
                })
                .collect::<Option<_>>()?
        };
        Some(Self { tiles })
    }

    /// The tiles overlapping `visible` (window coordinates), in window coordinates.
    pub fn visible(&self, viewport: &Viewport, visible: Bounds<Pixels>) -> Vec<(Bounds<Pixels>, Arc<RenderImage>)> {
        self.tiles
            .iter()
            .map(|(rect, image)| (viewport.to_window(*rect), image.clone()))
            .filter(|(b, _)| b.intersects(&visible))
            .collect()
    }
}

/// Paints `tiles` (from [`ImageTiles::visible`]).
pub fn paint_tiles(tiles: Vec<(Bounds<Pixels>, Arc<RenderImage>)>, window: &mut Window) {
    for (bounds, image) in tiles {
        window.paint_image(bounds, Corners::default(), image, 0, false).ok();
    }
}

/// One annotation layer on the GPU, tagged with what it shows: the editor
/// revision and the clip region it was rendered for.
#[derive(Default)]
pub struct LayerCache {
    key: Option<(u64, [i32; 4])>,
    image: Option<(PxRect, Arc<RenderImage>)>,
}

impl LayerCache {
    pub fn sync(&mut self, revision: u64, clip: PxRect, render: impl FnOnce() -> Option<(PxRect, Pixmap)>, window: &mut Window) {
        let key = (revision, [clip.x as i32, clip.y as i32, clip.w as i32, clip.h as i32]);
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);
        let new = render().and_then(|(rect, pixmap)| Some((rect, pixmap_image(&pixmap)?)));
        if let Some((_, old)) = std::mem::replace(&mut self.image, new) {
            // Each version is a new texture; free the previous one from the atlas.
            window.drop_image(old).ok();
        }
    }

    pub fn image(&self) -> Option<&(PxRect, Arc<RenderImage>)> {
        self.image.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tall_images_split_into_texture_sized_tiles() {
        let rows = tile_rows(30_000);
        assert_eq!(rows.len(), 8);
        assert_eq!(rows[0], 0..4096);
        assert_eq!(rows[7], 28_672..30_000);
        assert!(rows.windows(2).all(|w| w[0].end == w[1].start));
        assert_eq!(tile_rows(900), vec![0..900]);
    }
}
