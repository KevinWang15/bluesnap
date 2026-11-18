//! First-class multi-image composition model: a resizable canvas holding placed
//! images and vector/text/redaction annotations in unified Z-order.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use tiny_skia::Pixmap;

use crate::geom::{Handle, Point, Rect, resize_rect_with_handle};
use crate::shape::{EditHandle, Geometry, Metrics, Shape};
use crate::style::Color;

static NEXT_IMAGE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageId(pub u64);

impl ImageId {
    pub fn next() -> Self {
        Self(NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Ensures future generated IDs never collide with an ID loaded from a `.bs` file.
    pub fn observe(self) {
        let _ = NEXT_IMAGE_ID.fetch_max(self.0.saturating_add(1), Ordering::Relaxed);
    }
}

/// Shared immutable raster image asset. Equality compares the asset ID and
/// dimensions so undo snapshots are O(1) without scanning megabytes of pixels.
#[derive(Clone)]
pub struct ImageAsset {
    pub id: ImageId,
    pixmap: Arc<Pixmap>,
    opaque: bool,
    png_cache: Arc<OnceLock<Vec<u8>>>,
}

impl std::fmt::Debug for ImageAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageAsset")
            .field("id", &self.id)
            .field("width", &self.width())
            .field("height", &self.height())
            .field("opaque", &self.opaque)
            .finish()
    }
}

impl PartialEq for ImageAsset {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.width() == other.width() && self.height() == other.height()
    }
}

impl Eq for ImageAsset {}

impl ImageAsset {
    pub fn new(pixmap: Pixmap) -> Self {
        Self::with_id(ImageId::next(), pixmap)
    }

    pub fn with_id(id: ImageId, pixmap: Pixmap) -> Self {
        id.observe();
        let opaque = pixmap.data().as_chunks::<4>().0.iter().all(|px| px[3] == 255);
        Self { id, pixmap: Arc::new(pixmap), opaque, png_cache: Arc::new(OnceLock::new()) }
    }

    pub fn with_id_and_png(id: ImageId, pixmap: Pixmap, png: Vec<u8>) -> Self {
        let asset = Self::with_id(id, pixmap);
        let _ = asset.png_cache.set(png);
        asset
    }

    pub fn pixmap(&self) -> &Pixmap {
        &self.pixmap
    }

    pub fn is_opaque(&self) -> bool {
        self.opaque
    }

    pub(crate) fn cached_png(&self) -> Option<&[u8]> {
        self.png_cache.get().map(Vec::as_slice)
    }

    pub(crate) fn cache_png(&self, png: Vec<u8>) {
        let _ = self.png_cache.set(png);
    }

    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }

    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }
}

const MIN_IMAGE_SIDE: f32 = 8.0;

/// An [`ImageAsset`] placed at `bounds` on a composition canvas.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedImage {
    pub asset: ImageAsset,
    pub bounds: Rect,
}

impl PlacedImage {
    pub fn new(asset: ImageAsset, bounds: Rect) -> Self {
        Self { asset, bounds }
    }

    pub fn at_origin(asset: ImageAsset) -> Self {
        let (w, h) = (asset.width() as f32, asset.height() as f32);
        Self { asset, bounds: Rect::new(0.0, 0.0, w, h) }
    }

    pub fn translated(&self, d: Point) -> Self {
        Self { asset: self.asset.clone(), bounds: self.bounds.translate(d) }
    }

    pub fn scaled(&self, k: f32) -> Self {
        Self { asset: self.asset.clone(), bounds: self.bounds.scale(k) }
    }

    pub fn handles(&self) -> Vec<(EditHandle, Point)> {
        Handle::ALL.into_iter().map(|h| (EditHandle::Bounds(h), self.bounds.handle_point(h))).collect()
    }

    /// Resizes `self.bounds` by dragging `handle` by `delta`. When `constrain`
    /// (`Shift`) is held on any of the 8 handles, preserves the current aspect
    /// ratio.
    pub fn with_handle_moved(&self, handle: EditHandle, delta: Point, constrain: bool) -> Self {
        let EditHandle::Bounds(h) = handle else { return self.clone() };
        let next = resize_rect_with_handle(self.bounds, h, delta, constrain, MIN_IMAGE_SIDE);
        Self { asset: self.asset.clone(), bounds: next }
    }
}

/// One item on the canvas in Z-order (index `0` is at the bottom).
#[derive(Clone, Debug, PartialEq)]
pub enum Element {
    Image(PlacedImage),
    Shape(Shape),
}

impl Element {
    pub fn as_shape(&self) -> Option<&Shape> {
        match self {
            Element::Shape(s) => Some(s),
            Element::Image(_) => None,
        }
    }

    pub fn as_shape_mut(&mut self) -> Option<&mut Shape> {
        match self {
            Element::Shape(s) => Some(s),
            Element::Image(_) => None,
        }
    }

    pub fn as_image(&self) -> Option<&PlacedImage> {
        match self {
            Element::Image(img) => Some(img),
            Element::Shape(_) => None,
        }
    }

    pub fn bounds(&self, m: Metrics) -> Rect {
        match self {
            Element::Image(img) => img.bounds,
            Element::Shape(s) => s.bounds(m),
        }
    }

    pub fn hit(&self, p: Point, m: Metrics, allow_images: bool) -> bool {
        match self {
            Element::Image(img) => allow_images && img.bounds.contains(p),
            Element::Shape(s) => s.hit(p, m),
        }
    }

    pub fn handles(&self) -> Vec<(EditHandle, Point)> {
        match self {
            Element::Image(img) => img.handles(),
            Element::Shape(s) => s.handles(),
        }
    }

    pub fn translated(&self, d: Point) -> Element {
        match self {
            Element::Image(img) => Element::Image(img.translated(d)),
            Element::Shape(s) => Element::Shape(s.translated(d)),
        }
    }

    pub fn with_handle_moved(&self, handle: EditHandle, delta: Point, constrain: bool) -> Element {
        match self {
            Element::Image(img) => Element::Image(img.with_handle_moved(handle, delta, constrain)),
            Element::Shape(s) => Element::Shape(s.with_handle_moved(handle, delta, constrain)),
        }
    }

    pub fn is_degenerate(&self) -> bool {
        match self {
            Element::Image(img) => img.bounds.w < 2.0 || img.bounds.h < 2.0,
            Element::Shape(s) => s.is_degenerate(),
        }
    }
}

/// Z-order rearrangement actions for a selected element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerOrder {
    Backward,
    Forward,
    ToBack,
    ToFront,
}

/// Moves `elements[idx]` according to `order`; returns its new index if it moved.
pub fn reorder_element(elements: &mut Vec<Element>, idx: usize, order: LayerOrder) -> Option<usize> {
    let len = elements.len();
    if idx >= len || len <= 1 {
        return None;
    }
    let target = match order {
        LayerOrder::Backward => idx.checked_sub(1)?,
        LayerOrder::Forward if idx + 1 < len => idx + 1,
        LayerOrder::Forward => return None,
        LayerOrder::ToBack if idx > 0 => 0,
        LayerOrder::ToBack => return None,
        LayerOrder::ToFront if idx + 1 < len => len - 1,
        LayerOrder::ToFront => return None,
    };
    let item = elements.remove(idx);
    elements.insert(target, item);
    Some(target)
}

/// Renumbers all counter shapes 1, 2, 3 … in Z-order.
pub fn renumber_counters_in(elements: &mut [Element]) {
    let mut n = 0;
    for el in elements {
        if let Element::Shape(Shape { geometry: Geometry::Counter { number, .. }, .. }) = el {
            n += 1;
            *number = n;
        }
    }
}

const MAX_CANVAS_SIDE: u32 = 65_535;
pub(crate) const MAX_IMAGE_PIXELS: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub struct CanvasSizeError(u32, u32);

impl std::fmt::Display for CanvasSizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unsupported image or canvas dimensions {} × {}", self.0, self.1)
    }
}

impl std::error::Error for CanvasSizeError {}

pub fn check_canvas_size(width: u32, height: u32) -> Result<(), CanvasSizeError> {
    if (1..=MAX_CANVAS_SIDE).contains(&width)
        && (1..=MAX_CANVAS_SIDE).contains(&height)
        && u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS
    {
        Ok(())
    } else {
        Err(CanvasSizeError(width, height))
    }
}

/// Computes the canvas expansion before changing layers or their undo history.
pub(crate) struct SlicePlacement {
    pub canvas: (u32, u32),
    bounds: Rect,
    scale_ratio: f32,
}

impl SlicePlacement {
    pub fn new(
        canvas: (u32, u32), scale: f32, elements: &[Element], image: &Pixmap, slice_scale: f32,
    ) -> Result<Self, CanvasSizeError> {
        check_canvas_size(image.width(), image.height())?;
        let k = if slice_scale > 0.0 && slice_scale.is_finite() { scale / slice_scale } else { 1.0 };
        let width = (image.width() as f32 * k).round().max(1.0);
        let height = (image.height() as f32 * k).round().max(1.0);
        let (shift, canvas) = if elements.is_empty() {
            (0.0, (width as u32, height as u32))
        } else {
            let step = (24.0 * scale).round().max(8.0);
            let n = elements.iter().filter(|e| matches!(e, Element::Image(_))).count();
            let shift = ((n % 6) as f32).max(1.0) * step;
            (shift, (canvas.0.max((shift + width).ceil() as u32), canvas.1.max((shift + height).ceil() as u32)))
        };
        check_canvas_size(canvas.0, canvas.1)?;
        Ok(Self { canvas, bounds: Rect::new(shift, shift, width, height), scale_ratio: k })
    }

    pub fn insert(self, elements: &mut Vec<Element>, image: Pixmap, shapes: Vec<Shape>) -> usize {
        let index = elements.len();
        let offset = Point::new(self.bounds.x, self.bounds.y);
        elements.push(Element::Image(PlacedImage::new(ImageAsset::new(image), self.bounds)));
        elements.extend(shapes.into_iter().map(|s| Element::Shape(s.scaled(self.scale_ratio).translated(offset))));
        renumber_counters_in(elements);
        index
    }
}

/// An editable multi-image composition (saved/loaded as `.bs` BSON).
#[derive(Clone, Debug, PartialEq)]
pub struct Composition {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub background: Color,
    pub elements: Vec<Element>,
}

impl Composition {
    pub fn new_empty(width: u32, height: u32, scale: f32) -> Result<Self, CanvasSizeError> {
        let (width, height) = (width.max(1), height.max(1));
        check_canvas_size(width, height)?;
        Ok(Self {
            width,
            height,
            scale: if scale > 0.0 && scale.is_finite() { scale } else { 1.0 },
            background: Color::rgb(0xFFFFFF),
            elements: Vec::new(),
        })
    }

    /// Creates a canvas with the captured image and its editable shapes.
    pub fn from_slice(image: Pixmap, scale: f32, shapes: Vec<Shape>) -> Result<Self, CanvasSizeError> {
        let mut comp = Self::new_empty(image.width(), image.height(), scale)?;
        comp.append_slice(image, scale, shapes)?;
        Ok(comp)
    }

    /// Appends a slice, normalizing its display scale. A size error leaves the composition intact.
    pub fn append_slice(&mut self, image: Pixmap, slice_scale: f32, shapes: Vec<Shape>) -> Result<usize, CanvasSizeError> {
        let placement = SlicePlacement::new((self.width, self.height), self.scale, &self.elements, &image, slice_scale)?;
        (self.width, self.height) = placement.canvas;
        Ok(placement.insert(&mut self.elements, image, shapes))
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f32, self.height as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_pixmap(w: u32, h: u32) -> Pixmap {
        Pixmap::new(w, h).unwrap()
    }

    #[test]
    fn first_slice_sets_canvas_size_and_second_cascades_and_expands_if_needed() {
        let mut comp = Composition::from_slice(dummy_pixmap(400, 300), 1.0, Vec::new()).unwrap();
        assert_eq!((comp.width, comp.height), (400, 300));
        assert_eq!(comp.elements[0].as_image().unwrap().bounds, Rect::new(0.0, 0.0, 400.0, 300.0));

        let idx = comp.append_slice(dummy_pixmap(500, 200), 1.0, Vec::new()).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(comp.elements[1].as_image().unwrap().bounds, Rect::new(24.0, 24.0, 500.0, 200.0));
        assert_eq!((comp.width, comp.height), (524, 300));
    }

    #[test]
    fn constrained_corner_resize_preserves_image_aspect_ratio() {
        let img = PlacedImage::new(ImageAsset::new(dummy_pixmap(200, 100)), Rect::new(10.0, 20.0, 200.0, 100.0));
        let resized = img.with_handle_moved(EditHandle::Bounds(Handle::BottomRight), Point::new(100.0, 10.0), true);
        assert_eq!(resized.bounds.x, 10.0);
        assert_eq!(resized.bounds.y, 20.0);
        assert!(((resized.bounds.w / resized.bounds.h) - 2.0).abs() < 1e-4);
        assert_eq!(resized.bounds.w, 300.0);
        assert_eq!(resized.bounds.h, 150.0);
    }

    #[test]
    fn reorder_element_moves_backward_forward_front_and_back() {
        let mk = |w| Element::Image(PlacedImage::at_origin(ImageAsset::new(dummy_pixmap(w, 10))));
        let mut els = vec![mk(10), mk(20), mk(30)];
        assert_eq!(reorder_element(&mut els, 0, LayerOrder::Backward), None);
        assert_eq!(reorder_element(&mut els, 0, LayerOrder::Forward), Some(1));
        assert_eq!(els[1].as_image().unwrap().bounds.w, 10.0);
        assert_eq!(reorder_element(&mut els, 1, LayerOrder::ToFront), Some(2));
        assert_eq!(els[2].as_image().unwrap().bounds.w, 10.0);
        assert_eq!(reorder_element(&mut els, 2, LayerOrder::ToBack), Some(0));
        assert_eq!(els[0].as_image().unwrap().bounds.w, 10.0);
    }
}
