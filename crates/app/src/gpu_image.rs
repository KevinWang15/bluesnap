//! Getting images onto the GPU: (possibly very tall) screenshots and placed
//! composition images as tiles, and cached annotation layers.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use bluesnap_core::render::to_bgra_straight;
use bluesnap_core::{Editor, ImageAsset, ImageId, Pixmap, Rect as PxRect};
use gpui::{Bounds, Corners, Pixels, RenderImage, Window};
#[cfg(any(windows, test))]
use gpui::{DevicePixels, point, size};

use crate::viewport::Viewport;

/// Rows per tile. GPU textures are limited to 16,384 px; long screenshots go
/// beyond that, and smaller tiles let scrolling upload only what's visible.
const TILE_ROWS: u32 = 4096;

/// Wraps straight-alpha BGRA8 pixels in a gpui [`RenderImage`].
pub fn render_image(width: u32, height: u32, bgra: Vec<u8>) -> Option<Arc<RenderImage>> {
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
    width: u32,
    height: u32,
    tiles: Vec<(PxRect, Arc<RenderImage>)>,
}

impl ImageTiles {
    /// From opaque BGRA pixels (gpui's layout). A single tile takes the buffer as is.
    pub fn from_bgra(width: u32, height: u32, bgra: Vec<u8>) -> Option<Self> {
        if width == 0 || height == 0 || bgra.len() != width as usize * height as usize * 4 {
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
        Some(Self { width, height, tiles })
    }

    pub fn from_pixmap(pixmap: &Pixmap) -> Option<Self> {
        Self::from_bgra(pixmap.width(), pixmap.height(), to_bgra_straight(pixmap))
    }

    /// Tiles overlapping `visible` when the image sits at `(0, 0, width, height)`.
    pub fn visible(&self, viewport: &Viewport, visible: Bounds<Pixels>) -> Vec<(Bounds<Pixels>, Arc<RenderImage>)> {
        self.visible_in(PxRect::new(0.0, 0.0, self.width as f32, self.height as f32), viewport, visible)
    }

    /// Tiles overlapping `visible` when the image is placed/scaled into `placed`.
    pub fn visible_in(
        &self,
        placed: PxRect,
        viewport: &Viewport,
        visible: Bounds<Pixels>,
    ) -> Vec<(Bounds<Pixels>, Arc<RenderImage>)> {
        let sy = placed.h / (self.height.max(1) as f32);
        self.tiles
            .iter()
            .map(|(rect, image)| {
                let mapped = PxRect::new(placed.x, placed.y + rect.y * sy, placed.w, rect.h * sy);
                (viewport.to_window(mapped), image.clone())
            })
            .filter(|(b, _)| b.intersects(&visible))
            .collect()
    }

    /// Frees every tile texture from gpui's image atlas before closing the window.
    pub fn drop_images(&mut self, window: &mut Window) {
        for (_, image) in self.tiles.drain(..) {
            window.drop_image(image).ok();
        }
    }
}

/// Per-window GPU cache of [`ImageAsset`]s in a multi-image composition.
#[derive(Default)]
pub struct ImageAtlasCache {
    entries: HashMap<ImageId, ImageTiles>,
}

impl ImageAtlasCache {
    pub fn ensure(&mut self, asset: &ImageAsset) -> Option<&ImageTiles> {
        if let std::collections::hash_map::Entry::Vacant(e) = self.entries.entry(asset.id) {
            let tiles = ImageTiles::from_pixmap(asset.pixmap())?;
            e.insert(tiles);
        }
        self.entries.get(&asset.id)
    }

    pub fn prune(&mut self, live: &HashSet<ImageId>, window: &mut Window) {
        self.entries.retain(|id, tiles| {
            if live.contains(id) {
                true
            } else {
                tiles.drop_images(window);
                false
            }
        });
    }

    pub fn clear(&mut self, window: &mut Window) {
        for (_, mut tiles) in self.entries.drain() {
            tiles.drop_images(window);
        }
    }
}

/// Paints `tiles` (from [`ImageTiles::visible`] or [`ImageTiles::visible_in`]).
pub fn paint_tiles(tiles: Vec<(Bounds<Pixels>, Arc<RenderImage>)>, window: &mut Window) {
    for (bounds, image) in tiles {
        window.paint_image(bounds, bounds, Corners::default(), image, 0, false).ok();
    }
}

#[derive(PartialEq)]
pub struct LayerKey {
    pub revision: u64,
    pub range: Range<usize>,
    pub active: bool,
    pub clip: PxRect,
    pub split: bool,
}

/// Textures for a cached run of annotation shapes, in stacking order.
#[derive(Default)]
pub struct LayerCache {
    key: Option<LayerKey>,
    images: Vec<(PxRect, Arc<RenderImage>)>,
    #[cfg(windows)]
    coverage: Option<Vec<Bounds<DevicePixels>>>,
}

/// Texture-local rectangles covering every potentially nontransparent pixel.
/// Limit the number and total size of uploads for dense or overlapping masks.
#[cfg(any(windows, test))]
fn sparse_coverage(elements: &[bluesnap_core::Element], layer: PxRect) -> Option<Vec<Bounds<DevicePixels>>> {
    let layer_area = layer.w * layer.h;
    if layer_area < 128.0 * 128.0 || layer != layer.round_out() {
        return None;
    }
    let mut regions = Vec::new();
    let mut area = 0.0;
    for element in elements {
        let mask = element.as_shape()?.redaction_rect()?;
        // Match the antialiasing gutter and clipping used by Editor::render_band.
        if let Some(r) = mask.inflate(1.0).round_out().intersect(&layer) {
            area += r.w * r.h;
            if regions.len() == 128 || area > layer_area * 0.5 {
                return None;
            }
            regions.push(Bounds::new(
                point(DevicePixels((r.x - layer.x) as i32), DevicePixels((r.y - layer.y) as i32)),
                size(DevicePixels(r.w as i32), DevicePixels(r.h as i32)),
            ));
        }
    }
    (!regions.is_empty()).then_some(regions)
}

impl LayerCache {
    pub fn sync(
        &mut self,
        editor: &Editor,
        key: LayerKey,
        window: &mut Window,
    ) -> &[(PxRect, Arc<RenderImage>)] {
        if self.key.as_ref() != Some(&key) {
            #[cfg(not(windows))]
            self.drop_image(window);
            let mut next = Vec::new();
            editor.render_band_parts(key.range.clone(), key.active, key.clip, key.split, |rect, pixmap| {
                if let Some(image) = pixmap_image(&pixmap) {
                    next.push((rect, image));
                }
            });
            #[cfg(windows)]
            {
                let coverage = match next.as_slice() {
                    [(rect, _)] if !key.split && !key.active => sparse_coverage(&editor.elements()[key.range.clone()], *rect),
                    _ => None,
                };
                if let ([(old_rect, old)], [(rect, new)], Some(regions)) =
                    (self.images.as_slice(), next.as_slice(), coverage.as_ref())
                    && rect == old_rect && self.coverage.as_ref() == Some(regions)
                {
                    window.replace_image_regions(old, new, regions);
                }
                self.coverage = coverage;
            }
            for (_, old) in std::mem::replace(&mut self.images, next) {
                window.drop_image(old).ok();
            }
            self.key = Some(key);
        }
        &self.images
    }

    pub fn drop_image(&mut self, window: &mut Window) {
        self.key = None;
        #[cfg(windows)]
        {
            self.coverage = None;
        }
        for (_, image) in self.images.drain(..) {
            window.drop_image(image).ok();
        }
    }
}

/// Pool of [`LayerCache`]s sized dynamically to the current `VisualBand::Shapes` count.
#[derive(Default)]
pub struct LayerBandPool {
    bands: Vec<LayerCache>,
}

impl LayerBandPool {
    pub fn resize(&mut self, count: usize, window: &mut Window) {
        while self.bands.len() > count {
            if let Some(mut extra) = self.bands.pop() {
                extra.drop_image(window);
            }
        }
        while self.bands.len() < count {
            self.bands.push(LayerCache::default());
        }
    }

    pub fn get_mut(&mut self, index: usize) -> &mut LayerCache {
        &mut self.bands[index]
    }

    pub fn clear(&mut self, window: &mut Window) {
        self.resize(0, window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bluesnap_core::{Element, RedactEffect, Style};
    use bluesnap_core::shape::{Geometry, RedactShape, Shape};

    #[test]
    fn sparse_uploads_cover_clipped_masks_and_detect_layout_changes() {
        let shape = |geometry| Element::Shape(Shape { geometry, style: Style::default() });
        let mask = |x, y, w, h| shape(Geometry::Redact {
            area: RedactShape::Rect(PxRect::new(x, y, w, h)),
            effect: RedactEffect::Mosaic,
            strength: 0.5,
        });
        let layer = PxRect::new(100.0, 30.0, 600.0, 300.0);
        let first = mask(97.5, 42.25, 23.75, 10.5);
        let second = mask(301.5, 241.5, 80.0, 20.0);
        let coverage = sparse_coverage(&[first.clone(), second.clone()], layer).unwrap();
        assert_eq!(coverage[0], Bounds::new(point(0.into(), 11.into()), size(23.into(), 13.into())));
        assert!(sparse_coverage(std::slice::from_ref(&first), PxRect { x: 100.5, ..layer }).is_none());
        for changed in [
            vec![first.clone(), mask(315.0, 241.5, 80.0, 20.0)],
            vec![first.clone(), mask(301.5, 241.5, 25.0, 20.0)],
            vec![first.clone()],
        ] {
            assert_ne!(sparse_coverage(&changed, layer).unwrap(), coverage);
        }
        assert!(sparse_coverage(&[first, shape(Geometry::Ellipse(layer))], layer).is_none());
        assert!(sparse_coverage(&[mask(layer.x, layer.y, layer.w, layer.h)], layer).is_none());
    }

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
