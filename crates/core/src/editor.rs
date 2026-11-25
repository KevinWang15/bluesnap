//! Unified editing and annotation engine (`Editor`): powers both the full-screen
//! capture overlay (`SelectionMode::Pick`) and the multi-image resizable-canvas
//! composition editor (`SelectionMode::Canvas`).
//!
//! Frontends feed it pointer/keyboard input in canvas/screenshot pixel
//! coordinates and render what it reports.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

use tiny_skia::Pixmap;

use crate::composition::{
    Composition, Element, ImageAsset, LayerOrder, PlacedImage, renumber_counters_in, reorder_element,
};
use crate::geom::{CursorKind, Handle, Point, Rect, resize_rect_with_handle};
use crate::outline::{PickableWindow, WindowOutline};
use crate::pixels::crop;
use crate::render::{self, CanvasBackdrop, Document};
use crate::shape::{EditHandle, Geometry, Metrics, RedactShape, Shape};
use crate::style::{ArrowStyle, Color, CounterFormat, RedactArea, RedactEffect, Redaction, SizeLevel, Style, TextStyle, Tool};
use crate::text::{Fonts, TextLayout};

/// Movement (in logical pixels) before a press turns into a drag.
const DRAG_THRESHOLD: f32 = 3.0;
/// Hit radius (in logical pixels) of resize handles.
const HANDLE_HIT_RADIUS: f32 = 8.0;
/// Pointer samples closer than this (in logical pixels) are dropped from strokes.
const MIN_STROKE_STEP: f32 = 1.5;
/// Canvas mode: distance (in logical pixels) outside the canvas border where
/// the 8 canvas resize handles sit so they never collide with interior elements.
pub const CANVAS_HANDLE_OFFSET: f32 = 6.0;
/// Canvas mode: outer/inner grab tolerance (in logical pixels) along any canvas edge.
const CANVAS_EDGE_OUTER: f32 = 14.0;
const CANVAS_EDGE_INNER: f32 = 4.0;
/// Minimum canvas width/height in logical pixels.
const MIN_CANVAS_LOGICAL: f32 = 32.0;
/// Maximum undo snapshots retained per editing session.
pub const MAX_HISTORY: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextKey {
    Backspace,
    Delete,
    DeleteWordLeft,
    DeleteWordRight,
    Left,
    Right,
    Up,
    Down,
    WordLeft,
    WordRight,
    Home,
    End,
    DocStart,
    DocEnd,
    SelectLeft,
    SelectRight,
    SelectUp,
    SelectDown,
    SelectWordLeft,
    SelectWordRight,
    SelectHome,
    SelectEnd,
    SelectDocStart,
    SelectDocEnd,
    SelectAll,
    Newline,
}

/// How the workspace bounds behave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
    /// Capture overlay: starts empty, the user picks a window or drags a region,
    /// then can move and resize that selection.
    Pick,
    /// Composition editor: resizable multi-image canvas where images and shapes
    /// are first-class movable, resizable, rearrangable elements.
    Canvas,
}

/// A brush's outline under the pointer: a rounded rectangle (a circle for
/// the round redact brush, the nib for the highlighter).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrushRing {
    pub center: Point,
    pub half_width: f32,
    pub half_height: f32,
    pub corner: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightClick {
    /// Nothing selected yet in capture mode: the frontend should exit capture.
    Exit,
    /// Right-clicked on exposed background space in Canvas mode: show the
    /// background color context menu under the cursor.
    BackgroundMenu,
    Handled,
}

/// One GPU rendering slice of the scene in Z-order (bottom to top).
#[derive(Clone, Debug, PartialEq)]
pub enum VisualBand {
    Image(PlacedImage),
    Shapes { range: Range<usize>, revision: u64, active: bool },
}

#[derive(Clone, Debug)]
enum Drag {
    /// Press while picking: becomes a manual selection once the pointer moves.
    Pick { start: Point, moved: bool },
    SelResize { handle: Handle, orig: Rect, start: Point },
    SelMove { orig: Rect, start: Point },
    CanvasResize { handle: Handle, orig: Rect, current: Rect, start: Point },
    Create { idx: usize, start: Point },
    ElementMove { idx: usize, orig: Element, start: Point, moved: bool },
    ElementHandle { idx: usize, handle: EditHandle, orig: Element, start: Point },
    TextSelect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TextSnapshot {
    content: String,
    anchor: usize,
    caret: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextEditKind {
    Insert,
    Delete,
    Other,
}

#[derive(Clone, Debug)]
struct TextEdit {
    idx: usize,
    /// Fixed end of the selection (equals `caret` when nothing is selected).
    anchor: usize,
    /// Active end of the selection where the caret sits.
    caret: usize,
    marked: Option<Range<usize>>,
    /// Preserved horizontal position across consecutive Up/Down moves.
    preferred_x: Option<f32>,
    /// Scoped text-content undo history for the active editing session.
    undo: VecDeque<TextSnapshot>,
    /// Scoped text-content redo history for the active editing session.
    redo: VecDeque<TextSnapshot>,
    /// Coalescing state for consecutive typing / deletion edits.
    last_edit: Option<TextEditKind>,
}

impl TextEdit {
    fn new(idx: usize) -> Self {
        Self {
            idx,
            anchor: 0,
            caret: 0,
            marked: None,
            preferred_x: None,
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            last_edit: None,
        }
    }

    fn selection(&self) -> Range<usize> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }

    fn has_selection(&self) -> bool {
        self.anchor != self.caret
    }

    fn is_reversed(&self) -> bool {
        self.caret < self.anchor
    }

    fn push_undo(&mut self, snap: TextSnapshot) {
        while self.undo.len() >= MAX_HISTORY {
            self.undo.pop_front();
        }
        self.undo.push_back(snap);
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Snapshot {
    canvas: (u32, u32),
    background: Option<Color>,
    elements: Vec<Element>,
}

/// An immutable save request. Capture PNGs sample effects in screen coordinates,
/// while editable captures store a cropped image and translated vector shapes.
pub enum SaveSnapshot {
    Canvas(Composition),
    Capture { doc: Document, region: Rect, outline: Option<WindowOutline>, shapes: Vec<Shape>, scale: f32 },
}

impl SaveSnapshot {
    pub fn into_pixmap(self, fonts: &Fonts) -> Option<Pixmap> {
        match self {
            Self::Canvas(comp) => render::render_composition(&comp, fonts),
            Self::Capture { doc, region, outline, shapes, scale } => {
                if region == doc.bounds() && outline.is_none() && shapes.is_empty() {
                    Some(doc.into_pixmap())
                } else {
                    render::export_within(&doc, region, outline.as_ref(), &shapes, Metrics { scale, fonts })
                }
            }
        }
    }

    pub fn into_composition(self) -> Option<Composition> {
        match self {
            Self::Canvas(comp) => Some(comp),
            Self::Capture { doc, region, outline, shapes, scale } => {
                let (raw, shapes) = capture_slice(doc, region, outline, shapes)?;
                Composition::from_capture(raw, scale, shapes).ok()
            }
        }
    }
}

fn capture_slice(doc: Document, region: Rect, outline: Option<WindowOutline>, shapes: Vec<Shape>) -> Option<(Pixmap, Vec<Shape>)> {
    let sel = region.round().intersect(&doc.bounds())?;
    let mut raw = if sel == doc.bounds() {
        doc.into_pixmap()
    } else {
        crop(doc.image(), sel.x as u32, sel.y as u32, sel.w as u32, sel.h as u32)?
    };
    if let Some(outline) = outline {
        outline.clip(&mut raw, Point::new(sel.x, sel.y));
    }
    let offset = Point::new(-sel.x, -sel.y);
    let shapes = if offset == Point::default() { shapes } else { shapes.iter().map(|s| s.translated(offset)).collect() };
    Some((raw, shapes))
}

enum Workspace {
    Capture {
        doc: Document,
        /// Candidate windows, topmost first: (bounds clipped to the screenshot, full outline).
        windows: Vec<(Rect, Box<dyn PickableWindow>)>,
        hover: Option<Rect>,
        /// Outline of the window under the pointer, when `hover` is a window.
        hover_window: Option<usize>,
        selection: Option<Rect>,
        /// Set when `selection` came from a single click on a window and hasn't
        /// been resized or moved since: output is clipped to its exact outline.
        snapped: Option<WindowOutline>,
    },
    Canvas {
        width: u32,
        height: u32,
        background: Color,
        /// Region-clipped Z-order backdrop + tile cache for Mosaic/Blur redactions.
        backdrop: RefCell<Option<CanvasBackdrop>>,
    },
}

pub struct Editor {
    workspace: Workspace,
    fonts: Arc<Fonts>,
    /// Content scale: canvas/screenshot pixels per logical pixel.
    scale: f32,
    /// Interaction scale: canvas pixels per logical pixel *on screen right now*,
    /// for hit radii and drag thresholds when zoomed.
    ui_scale: f32,
    pointer: Point,
    constrain: bool,
    drag: Option<Drag>,
    elements: Vec<Element>,
    /// Snapshots taken before each undoable change (bounded by [`MAX_HISTORY`]).
    history: VecDeque<Snapshot>,
    /// Snapshots undone by [`Editor::undo`] that can be reapplied by [`Editor::redo`].
    redo_history: VecDeque<Snapshot>,
    selected: Option<usize>,
    tool: Option<Tool>,
    styles: [Style; Tool::ALL.len()],
    redaction: Redaction,
    counter_format: CounterFormat,
    text_style: TextStyle,
    arrow_style: ArrowStyle,
    continuous: Option<bool>,
    text: Option<TextEdit>,
    /// Accumulated canvas-origin shift from normalizing a Left/Top canvas resize.
    pending_origin_shift: Point,
    revision: u64,
    content_revision: u64,
    static_revision: u64,
    active_revision: u64,
}

impl Editor {
    /// Full-screen capture session over `doc`. `scale` is screenshot pixels per
    /// logical pixel (the display's scale factor). `windows` are the pickable
    /// windows, topmost first: plain rectangles or [`WindowOutline`]s with
    /// rounded corners.
    pub fn new(
        doc: Document,
        fonts: Arc<Fonts>,
        windows: impl IntoIterator<Item = impl PickableWindow + 'static>,
        scale: f32,
    ) -> Self {
        let bounds = doc.bounds();
        let windows = windows
            .into_iter()
            .filter_map(|w| Some((w.bounds().intersect(&bounds)?, Box::new(w) as Box<dyn PickableWindow>)))
            .collect();
        let mut editor = Self::with_workspace(
            Workspace::Capture { doc, windows, hover: None, hover_window: None, selection: None, snapped: None },
            Vec::new(),
            fonts,
            scale,
        );
        editor.update_hover();
        editor
    }

    /// Multi-image composition editor over `comp`.
    pub fn from_composition(comp: Composition, fonts: Arc<Fonts>) -> Self {
        let scale = comp.scale;
        let selected = comp.elements.iter().rposition(|e| matches!(e, Element::Image(_)));
        let mut editor = Self::with_workspace(
            Workspace::Canvas {
                width: comp.width.max(1),
                height: comp.height.max(1),
                background: comp.background,
                backdrop: RefCell::new(None),
            },
            comp.elements,
            fonts,
            scale,
        );
        // Start unselected when opening a single-image composition so all 4
        // canvas edges are immediately uncluttered; select when multiple exist.
        if editor.elements.iter().filter(|e| matches!(e, Element::Image(_))).count() > 1 {
            editor.selected = selected;
        }
        editor.touch_shapes();
        editor
    }

    /// Convenience constructor wrapping a single `Document` in a composition.
    pub fn for_document(doc: Document, fonts: Arc<Fonts>, scale: f32) -> Result<Self, crate::composition::CanvasSizeError> {
        Ok(Self::from_composition(Composition::from_slice(doc.into_pixmap(), scale, Vec::new())?, fonts))
    }

    /// Appends initial `shapes` without recording an undo step.
    pub fn with_shapes(mut self, shapes: Vec<Shape>) -> Self {
        self.elements.extend(shapes.into_iter().map(Element::Shape));
        renumber_counters_in(&mut self.elements);
        self.touch_shapes();
        self
    }

    fn with_workspace(workspace: Workspace, elements: Vec<Element>, fonts: Arc<Fonts>, scale: f32) -> Self {
        let scale = if scale > 0.0 && scale.is_finite() { scale } else { 1.0 };
        Self {
            workspace,
            fonts,
            scale,
            ui_scale: scale,
            pointer: Point::default(),
            constrain: false,
            drag: None,
            elements,
            history: VecDeque::new(),
            redo_history: VecDeque::new(),
            selected: None,
            tool: None,
            styles: Tool::ALL.map(Tool::default_style),
            redaction: Redaction::default(),
            counter_format: CounterFormat::default(),
            text_style: TextStyle::default(),
            arrow_style: ArrowStyle::default(),
            continuous: None,
            text: None,
            pending_origin_shift: Point::default(),
            revision: 0,
            content_revision: 0,
            static_revision: 0,
            active_revision: 0,
        }
    }

    /// Replaces the entire canvas composition (e.g. when opening a `.bs` file).
    pub fn replace_composition(&mut self, comp: Composition) {
        let fonts = self.fonts.clone();
        *self = Self::from_composition(comp, fonts);
    }

    /// Appends a captured `image` and its unflattened `shapes` into a canvas
    /// editor as one undoable step, selecting the newly added image.
    pub fn append_slice(&mut self, image: Pixmap, slice_scale: f32, shapes: Vec<Shape>) -> Option<usize> {
        if !matches!(self.workspace, Workspace::Canvas { .. }) {
            return None;
        }
        // Committing a lone, empty text box will leave an empty canvas.
        let layers = if self.elements.len() == 1 && self.text_content() == Some("") {
            &[][..]
        } else {
            &self.elements
        };
        let placement = crate::composition::SlicePlacement::new(self.canvas_dims(), self.scale, layers, &image, slice_scale).ok()?;
        self.finish_text();
        self.drag = None;
        self.begin_change();
        if let Workspace::Canvas { width, height, .. } = &mut self.workspace {
            (*width, *height) = placement.canvas;
        }
        let img_idx = placement.insert(&mut self.elements, image, shapes);
        self.invalidate_backdrop();
        self.end_change();
        self.tool = None;
        self.selected = Some(img_idx);
        self.touch_shapes();
        Some(img_idx)
    }

    /// Copies the selected item, or the complete canvas when nothing is selected.
    /// If a later redaction covers the item, publish only the visible composite.
    pub fn copy_canvas_item(&mut self) -> Option<(Pixmap, Option<Element>)> {
        if !matches!(self.workspace, Workspace::Canvas { .. }) || self.drag.is_some() {
            return None;
        }
        let Some(idx) = self.selected else { return Some((self.export()?, None)) };
        let item = self.elements.get(idx)?;
        let bounds = item.bounds(self.metrics()).round_out().intersect(&self.screen())?;
        if self.elements[idx + 1..].iter().filter_map(Element::as_shape).any(|s| {
            s.tool() == Tool::Redact && s.bounds(self.metrics()).intersect(&bounds).is_some()
        }) {
            let visible = self.export()?;
            return Some((crop(&visible, bounds.x as u32, bounds.y as u32, bounds.w as u32, bounds.h as u32)?, None));
        }
        match item {
            Element::Shape(_) => {
                let (_, pixels) = self.render_band(idx..idx + 1, false, self.screen())?;
                Some((pixels, Some(item.clone())))
            }
            Element::Image(image) => {
                let mut pixels = Pixmap::new(bounds.w as u32, bounds.h as u32)?;
                render::draw_placed_image(&mut pixels, image, Point::new(bounds.x, bounds.y));
                // A resized image needs its original pixels if pasted back into
                // BlueSnap. A cropped image only publishes the visible portion.
                let native = image.bounds.round_out() == bounds && (image.bounds != bounds
                    || pixels.width() != image.asset.width() || pixels.height() != image.asset.height());
                Some((pixels, native.then(|| item.clone())))
            }
        }
    }

    /// Places a copied item just past its original location, keeping it on
    /// the destination canvas when pasting between documents or display scales.
    pub fn paste_element(&mut self, element: Element, source_scale: f32) -> bool {
        if !source_scale.is_finite() || source_scale <= 0.0 {
            return false;
        }
        let k = self.scale / source_scale;
        let element = match element {
            Element::Shape(shape) => Element::Shape(shape.scaled(k)),
            Element::Image(image) => Element::Image(image.scaled(k)),
        };
        let bounds = element.bounds(self.metrics());
        let step = 16.0 * self.scale;
        self.paste_element_at(element, Point::new(bounds.x + step, bounds.y + step))
    }

    /// A clipboard bitmap is already in image pixels. Place it pixel-aligned in
    /// the visible canvas, adopting its native dimensions for the first image.
    pub fn paste_image(&mut self, image: Pixmap, center: Point) -> bool {
        let x = (center.x - image.width() as f32 / 2.0).round();
        let y = (center.y - image.height() as f32 / 2.0).round();
        let image = PlacedImage::at_origin(ImageAsset::new(image));
        self.paste_element_at(Element::Image(image), Point::new(x, y))
    }

    fn paste_element_at(&mut self, element: Element, position: Point) -> bool {
        if !matches!(self.workspace, Workspace::Canvas { .. }) || self.drag.is_some() {
            return false;
        }
        if let Element::Image(image) = &element
            && !crate::format::valid_canvas_size(image.asset.width(), image.asset.height())
        {
            return false;
        }
        let bounds = element.bounds(self.metrics());
        let first_image = self.elements.is_empty() && matches!(element, Element::Image(_));
        let (width, height) = if first_image { (0, 0) } else { self.canvas_dims() };
        let dimensions = (width.max(bounds.w.ceil() as u32), height.max(bounds.h.ceil() as u32));
        if !crate::format::valid_canvas_size(dimensions.0, dimensions.1) {
            return false;
        }
        let x = position.x.clamp(0.0, (dimensions.0 as f32 - bounds.w).max(0.0));
        let y = position.y.clamp(0.0, (dimensions.1 as f32 - bounds.h).max(0.0));
        self.finish_text();
        self.begin_change();
        if let Workspace::Canvas { width, height, .. } = &mut self.workspace {
            (*width, *height) = dimensions;
        }
        self.tool = None;
        self.selected = Some(self.elements.len());
        self.elements.push(element.translated(Point::new(x - bounds.x, y - bounds.y)));
        renumber_counters_in(&mut self.elements);
        self.invalidate_backdrop();
        self.end_change();
        self.touch_shapes();
        true
    }

    pub fn paste_text(&mut self, text: String, position: Point) -> bool {
        if self.is_editing_text() {
            self.text_replace(None, &text, false, None);
            true
        } else if !text.is_empty() {
            let geometry = Geometry::Text {
                origin: position,
                content: text,
                style: self.text_style,
                max_width: None,
                min_height: None,
            };
            self.paste_element_at(Element::Shape(Shape { geometry, style: self.styles[Tool::Text.index()] }), position)
        } else {
            true
        }
    }

    /// Annotations inside the selection, translated relative to the selection's
    /// top-left corner, with any active text committed.
    pub fn shapes_in_selection(&mut self) -> Vec<Shape> {
        self.finish_text();
        let Some(sel) = self.selection() else { return Vec::new() };
        let to_origin = Point::new(-sel.x, -sel.y);
        self.elements
            .iter()
            .filter_map(Element::as_shape)
            .map(|s| s.translated(to_origin))
            .collect()
    }

    /// Extracts `(raw_cropped_image, scale, unflattened_shapes)` from a capture
    /// selection so annotations stay editable when sent to the Editor or `.bs`.
    pub fn extract_slice(&mut self) -> Option<(Pixmap, f32, Vec<Shape>)> {
        match self.save_snapshot()? {
            SaveSnapshot::Capture { doc, region, outline, shapes, scale } => {
                let (raw, shapes) = capture_slice(doc, region, outline, shapes)?;
                Some((raw, scale, shapes))
            }
            SaveSnapshot::Canvas(comp) => Some((render::render_composition(&comp, &self.fonts)?, comp.scale, Vec::new())),
        }
    }

    /// Captures document state for saving without rasterizing or copying image pixels.
    pub fn save_snapshot(&mut self) -> Option<SaveSnapshot> {
        self.finish_text();
        match &self.workspace {
            Workspace::Capture { doc, selection, snapped, .. } => Some(SaveSnapshot::Capture {
                doc: doc.snapshot(),
                region: (*selection)?,
                outline: snapped.clone(),
                shapes: self.elements.iter().filter_map(Element::as_shape).cloned().collect(),
                scale: self.scale,
            }),
            Workspace::Canvas { width, height, background, .. } => Some(SaveSnapshot::Canvas(Composition {
                width: *width,
                height: *height,
                scale: self.scale,
                background: *background,
                elements: self.elements.clone(),
            })),
        }
    }

    /// Builds the editable document with the captured image and its vector annotations.
    pub fn to_composition(&mut self) -> Option<Composition> {
        self.save_snapshot()?.into_composition()
    }

    pub fn selection_mode(&self) -> SelectionMode {
        match self.workspace {
            Workspace::Capture { .. } => SelectionMode::Pick,
            Workspace::Canvas { .. } => SelectionMode::Canvas,
        }
    }

    pub fn set_ui_scale(&mut self, ui_scale: f32) {
        self.ui_scale = ui_scale.max(0.01);
    }

    fn metrics(&self) -> Metrics<'_> {
        Metrics { scale: self.scale, fonts: &self.fonts }
    }

    fn canvas_dims(&self) -> (u32, u32) {
        match &self.workspace {
            Workspace::Capture { doc, .. } => (doc.width(), doc.height()),
            Workspace::Canvas { width, height, .. } => (*width, *height),
        }
    }

    fn invalidate_backdrop(&mut self) {
        self.invalidate_backdrop_from(0);
    }

    /// Invalidates cached redaction backdrop pixels iff the cached `upto` prefix
    /// includes `first_changed` (`upto > first_changed`).
    fn invalidate_backdrop_from(&self, first_changed: usize) {
        if let Workspace::Canvas { backdrop, .. } = &self.workspace
            && backdrop.borrow().as_ref().is_some_and(|b| b.upto() > first_changed)
        {
            *backdrop.borrow_mut() = None;
        }
    }

    fn has_effect_redaction_above(&self, idx: usize) -> bool {
        self.elements.get(idx.saturating_add(1)..).is_some_and(|above| {
            above.iter().any(|el| {
                matches!(
                    el,
                    Element::Shape(Shape {
                        geometry: Geometry::Redact { effect: RedactEffect::Mosaic | RedactEffect::Blur, .. },
                        ..
                    })
                )
            })
        })
    }

    /// Only upper effects that sample changed pixels can change the static layer.
    /// Include the previous position so moving a shape out of a filter updates it.
    fn edit_affects_upper_effect(&self, idx: usize, previous: &Element) -> bool {
        let m = self.metrics();
        let canvas = self.screen();
        let mut damage = None;
        self.elements[idx + 1..].iter().filter_map(Element::as_shape).any(|shape| {
            let Geometry::Redact { effect, strength, .. } = shape.geometry else { return false };
            if effect == RedactEffect::Gray {
                return false;
            }
            let Some(area) = shape.bounds(m).round_out().intersect(&canvas) else { return false };
            let changed = damage.get_or_insert_with(|| previous.bounds(m).union(&self.elements[idx].bounds(m)).round_out());
            render::effect_sample_bounds(self.canvas_dims(), effect, effect.amount(strength, m.scale), area)
                .is_some_and(|sample| sample.intersect(changed).is_some())
        })
    }

    fn has_any_effect_redaction(&self) -> bool {
        self.elements.iter().any(|el| {
            matches!(
                el,
                Element::Shape(Shape {
                    geometry: Geometry::Redact { effect: RedactEffect::Mosaic | RedactEffect::Blur, .. },
                    ..
                })
            )
        })
    }

    fn touch(&mut self) {
        self.revision += 1;
    }

    fn touch_shapes(&mut self) {
        self.touch();
        self.static_revision += 1;
        self.active_revision += 1;
    }

    fn touch_active(&mut self) {
        self.touch();
        self.active_revision += 1;
    }

    fn active_index(&self) -> Option<usize> {
        match &self.drag {
            Some(Drag::Create { idx, .. } | Drag::ElementHandle { idx, .. } | Drag::ElementMove { idx, moved: true, .. }) => {
                self.elements.get(*idx).and_then(|e| e.as_shape().map(|_| *idx))
            }
            _ if self.continuous == Some(true) && self.text.is_none() => self.selected,
            _ => self.text.as_ref().map(|t| t.idx),
        }
    }

    // ----- queries --------------------------------------------------------

    /// Capture mode's frozen screenshot, if in Capture mode.
    pub fn capture_document(&self) -> Option<&Document> {
        match &self.workspace {
            Workspace::Capture { doc, .. } => Some(doc),
            Workspace::Canvas { .. } => None,
        }
    }

    pub fn background_color(&self) -> Option<Color> {
        match &self.workspace {
            Workspace::Capture { .. } => None,
            Workspace::Canvas { background, .. } => Some(*background),
        }
    }

    pub fn screen(&self) -> Rect {
        let (w, h) = self.canvas_dims();
        Rect::new(0.0, 0.0, w as f32, h as f32)
    }

    /// Live canvas bounds (reflecting an in-progress edge drag before release).
    pub fn active_canvas_rect(&self) -> Rect {
        if let Some(Drag::CanvasResize { current, .. }) = &self.drag {
            return *current;
        }
        self.screen()
    }

    pub fn is_resizing_canvas(&self) -> bool {
        matches!(self.drag, Some(Drag::CanvasResize { .. }))
    }

    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Outer resize handle positions of the canvas in `SelectionMode::Canvas`.
    pub fn canvas_handles(&self) -> Vec<Point> {
        if self.selection_mode() != SelectionMode::Canvas {
            return Vec::new();
        }
        let r = self.active_canvas_rect().inflate(CANVAS_HANDLE_OFFSET * self.ui_scale);
        Handle::ALL.into_iter().map(|h| r.handle_point(h)).collect()
    }

    /// Returns and clears any coordinate shift applied when a Left/Top canvas
    /// resize was normalized back to `(0, 0)` on release.
    pub fn take_canvas_origin_shift(&mut self) -> Point {
        std::mem::take(&mut self.pending_origin_shift)
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Version of the exported pixels and selection. An in-progress edit has no
    /// stable version. Hover, pan, and changes to the active tool don't affect it.
    pub fn settled_content_revision(&self) -> Option<u64> {
        (self.drag.is_none() && self.text.is_none() && self.continuous != Some(true)).then_some(self.content_revision)
    }

    pub fn is_picking(&self) -> bool {
        matches!(&self.workspace, Workspace::Capture { selection: None, .. })
    }

    pub fn active_rect(&self) -> Option<Rect> {
        match &self.workspace {
            Workspace::Capture { selection, hover, .. } => match (&self.drag, *selection) {
                (Some(Drag::Pick { start, moved: true }), _) => Some(Rect::from_points(*start, self.pointer)),
                (_, Some(sel)) => Some(sel),
                _ => *hover,
            },
            Workspace::Canvas { .. } => Some(self.active_canvas_rect()),
        }
    }

    pub fn selection(&self) -> Option<Rect> {
        match &self.workspace {
            Workspace::Capture { selection, .. } => *selection,
            Workspace::Canvas { .. } => Some(self.active_canvas_rect()),
        }
    }

    pub fn toolbar_visible(&self) -> bool {
        self.selection().is_some()
            && !matches!(self.drag, Some(Drag::Pick { .. } | Drag::SelResize { .. } | Drag::SelMove { .. }))
    }

    pub fn tool(&self) -> Option<Tool> {
        self.tool
    }

    pub fn elements(&self) -> &[Element] {
        &self.elements
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub fn selected_is_image(&self) -> bool {
        self.selected.and_then(|i| self.elements.get(i)).is_some_and(|e| matches!(e, Element::Image(_)))
    }

    /// `(can_send_backward, can_bring_forward)` for the currently selected element.
    pub fn can_reorder_selected(&self) -> Option<(bool, bool)> {
        let idx = self.selected?;
        if idx >= self.elements.len() || self.text.is_some() {
            return None;
        }
        let n = self.elements.len();
        Some((idx > 0, idx + 1 < n))
    }

    /// Moves the selected element in Z-order (`Backward`, `Forward`, `ToBack`, `ToFront`).
    pub fn reorder_selected(&mut self, order: LayerOrder) -> bool {
        if self.text.is_some() || self.drag.is_some() {
            return false;
        }
        let Some(idx) = self.selected else { return false };
        self.begin_change();
        let Some(new_idx) = reorder_element(&mut self.elements, idx, order) else {
            self.end_change();
            return false;
        };
        self.selected = Some(new_idx);
        renumber_counters_in(&mut self.elements);
        self.invalidate_backdrop();
        self.end_change();
        self.touch_shapes();
        true
    }

    pub fn current_style(&self) -> Option<(Tool, Style)> {
        if let Some(shape) = self.selected.and_then(|i| self.elements.get(i)).and_then(Element::as_shape) {
            return Some((shape.tool(), shape.style));
        }
        self.tool.map(|t| (t, self.styles[t.index()]))
    }

    pub fn current_redaction(&self) -> Option<Redaction> {
        if let Some(shape) = self.selected.and_then(|i| self.elements.get(i)).and_then(Element::as_shape) {
            return match &shape.geometry {
                Geometry::Redact { area, effect, strength } => Some(Redaction {
                    effect: *effect,
                    area: match area {
                        RedactShape::Brush(_) => RedactArea::Brush,
                        RedactShape::Rect(_) => RedactArea::Rect,
                    },
                    strength: *strength,
                }),
                _ => None,
            };
        }
        (self.tool == Some(Tool::Redact)).then_some(self.redaction)
    }

    pub fn set_redaction(&mut self, f: impl Fn(&mut Redaction)) {
        let area = self.redaction.area;
        f(&mut self.redaction);
        if self.redaction.area != area && self.selected.is_some() {
            self.finish_text();
            self.selected = None;
            self.tool = Some(Tool::Redact);
            return self.touch();
        }
        let Some(idx) = self.selected else { return self.touch() };
        let Some(Shape { geometry: Geometry::Redact { effect, strength, .. }, .. }) =
            self.elements.get(idx).and_then(Element::as_shape)
        else {
            return self.touch();
        };
        let mut changed = Redaction { effect: *effect, area: self.redaction.area, strength: *strength };
        f(&mut changed);
        let record = match self.continuous {
            Some(true) => false,
            Some(false) => {
                self.continuous = Some(true);
                true
            }
            None => true,
        };
        if record {
            self.begin_change();
        }
        if let Some(Shape { geometry: Geometry::Redact { effect, strength, .. }, .. }) =
            self.elements.get_mut(idx).and_then(Element::as_shape_mut)
        {
            (*effect, *strength) = (changed.effect, changed.strength);
        }
        self.invalidate_backdrop_from(idx);
        if self.continuous.is_none() {
            self.end_change();
        }
        // The first slider step splits this shape into its own band. Later
        // steps can reuse the other bands unless a higher effect samples it.
        if !record && self.active_index() == Some(idx)
            && (self.selection_mode() != SelectionMode::Canvas || !self.edit_affects_upper_effect(idx, &self.elements[idx]))
        {
            self.touch_active();
        } else {
            self.touch_shapes();
        }
    }

    pub fn current_counter_format(&self) -> Option<CounterFormat> {
        let showing = match self.selected.and_then(|i| self.elements.get(i)) {
            Some(Element::Shape(shape)) => shape.tool() == Tool::Counter,
            Some(Element::Image(_)) => false,
            None => self.tool == Some(Tool::Counter),
        };
        showing.then(|| self.counter_format())
    }

    fn counter_format(&self) -> CounterFormat {
        self.elements
            .iter()
            .filter_map(Element::as_shape)
            .find_map(|s| match s.geometry {
                Geometry::Counter { format, .. } => Some(format),
                _ => None,
            })
            .unwrap_or(self.counter_format)
    }

    pub fn current_text_style(&self) -> Option<TextStyle> {
        match self.selected.and_then(|i| self.elements.get(i)) {
            Some(Element::Shape(shape)) => match &shape.geometry {
                Geometry::Text { style, .. } => Some(*style),
                _ => None,
            },
            Some(Element::Image(_)) => None,
            None => (self.tool == Some(Tool::Text)).then_some(self.text_style),
        }
    }

    pub fn set_text_style(&mut self, new: TextStyle) {
        self.text_style = new;
        let Some(idx) = self.selected else { return self.touch() };
        if !self.elements.get(idx).and_then(Element::as_shape).is_some_and(|s| matches!(s.geometry, Geometry::Text { .. })) {
            return self.touch();
        }
        if self.text.is_none() {
            self.begin_change();
        }
        if let Some(Shape { geometry: Geometry::Text { style, .. }, .. }) =
            self.elements.get_mut(idx).and_then(Element::as_shape_mut)
        {
            *style = new;
        }
        self.invalidate_backdrop_from(idx);
        if self.text.is_none() {
            self.end_change();
        }
        self.touch_shapes();
    }

    pub fn current_arrow_style(&self) -> Option<ArrowStyle> {
        match self.selected.and_then(|i| self.elements.get(i)) {
            Some(Element::Shape(shape)) => match &shape.geometry {
                Geometry::Arrow { style, .. } => Some(*style),
                _ => None,
            },
            Some(Element::Image(_)) => None,
            None => (self.tool == Some(Tool::Arrow)).then_some(self.arrow_style),
        }
    }

    pub fn set_arrow_style(&mut self, new: ArrowStyle) {
        self.arrow_style = new;
        let Some(idx) = self.selected.filter(|&i| {
            self.elements.get(i).and_then(Element::as_shape).is_some_and(|s| matches!(s.geometry, Geometry::Arrow { .. }))
        }) else {
            return self.touch();
        };
        self.begin_change();
        if let Some(Shape { geometry: Geometry::Arrow { style, .. }, .. }) =
            self.elements.get_mut(idx).and_then(Element::as_shape_mut)
        {
            *style = new;
        }
        self.invalidate_backdrop_from(idx);
        self.end_change();
        self.touch_shapes();
    }

    pub fn set_counter_format(&mut self, format: CounterFormat) {
        self.counter_format = format;
        self.begin_change();
        for el in &mut self.elements {
            if let Element::Shape(Shape { geometry: Geometry::Counter { format: f, .. }, .. }) = el {
                *f = format;
            }
        }
        self.invalidate_backdrop();
        self.end_change();
        self.touch_shapes();
    }

    pub fn add_redactions(&mut self, rects: &[Rect]) -> usize {
        let Some(sel) = self.selection() else { return 0 };
        self.finish_text();
        self.begin_change();
        let style = self.styles[Tool::Redact.index()];
        let before = self.elements.len();
        for r in rects.iter().filter_map(|r| r.intersect(&sel)) {
            let geometry = Geometry::Redact {
                area: RedactShape::Rect(r),
                effect: self.redaction.effect,
                strength: self.redaction.strength,
            };
            let shape = Shape { geometry, style };
            if !shape.is_degenerate() {
                self.elements.push(Element::Shape(shape));
            }
        }
        self.end_change();
        self.selected = None;
        self.touch_shapes();
        self.elements.len() - before
    }

    pub fn begin_continuous_edit(&mut self) {
        self.continuous = Some(false);
    }

    pub fn end_continuous_edit(&mut self) {
        if self.continuous.take() == Some(true) {
            self.end_change();
            self.touch_shapes();
        }
    }

    pub fn brush_ring(&self) -> Option<BrushRing> {
        let painting = matches!(self.drag, None | Some(Drag::Create { .. }));
        let tool = self.tool?;
        let brush = match tool {
            Tool::Redact => self.redaction.area == RedactArea::Brush,
            Tool::Highlighter => true,
            _ => false,
        };
        let inside = self.selection().is_some_and(|s| s.contains(self.pointer));
        let w = self.styles[tool.index()].size.logical_px(tool) * self.scale;
        let (half_width, half_height, corner) = match tool {
            Tool::Highlighter => render::highlighter_nib(w),
            _ => (w / 2.0, w / 2.0, w / 2.0),
        };
        (painting && brush && inside && self.text.is_none())
            .then_some(BrushRing { center: self.pointer, half_width, half_height, corner })
    }

    pub fn can_undo(&self) -> bool {
        match &self.text {
            Some(edit) => !edit.undo.is_empty(),
            None => !self.history.is_empty(),
        }
    }

    pub fn can_redo(&self) -> bool {
        match &self.text {
            Some(edit) => !edit.redo.is_empty(),
            None => !self.redo_history.is_empty(),
        }
    }

    pub fn undo_depth(&self) -> usize {
        match &self.text {
            Some(edit) => edit.undo.len().min(MAX_HISTORY),
            None => self.history.len().min(MAX_HISTORY),
        }
    }

    pub fn redo_depth(&self) -> usize {
        match &self.text {
            Some(edit) => edit.redo.len().min(MAX_HISTORY),
            None => self.redo_history.len().min(MAX_HISTORY),
        }
    }

    pub fn has_shapes(&self) -> bool {
        self.elements.iter().any(|e| matches!(e, Element::Shape(_)))
    }

    /// Bounds and handle positions of the selected element (image or shape).
    pub fn selected_frame(&self) -> Option<(Rect, Vec<Point>)> {
        let el = self.elements.get(self.selected?)?;
        if self.text.is_some() || matches!(self.drag, Some(Drag::Create { .. } | Drag::CanvasResize { .. })) {
            return None;
        }
        let m = self.metrics();
        let frame = el.as_shape().and_then(|s| s.text_frame_rect(m)).unwrap_or_else(|| el.bounds(m));
        let handles = el.handles(m).into_iter().map(|(_, p)| p).collect();
        Some((frame, handles))
    }

    pub fn cursor(&self) -> CursorKind {
        match &self.drag {
            Some(Drag::Pick { .. } | Drag::Create { .. }) => return CursorKind::Crosshair,
            Some(Drag::SelResize { handle, .. } | Drag::CanvasResize { handle, .. }) => return handle.cursor(),
            Some(Drag::SelMove { .. } | Drag::ElementMove { .. }) => return CursorKind::Move,
            Some(Drag::ElementHandle { handle, .. }) => {
                return match handle {
                    EditHandle::Bounds(h) => h.cursor(),
                    _ => CursorKind::Crosshair,
                };
            }
            Some(Drag::TextSelect) => return CursorKind::Text,
            None => {}
        }
        let p = self.pointer;
        let Some(sel) = self.selection() else { return CursorKind::Crosshair };
        if let Some((_, h)) = self.element_handle_at(p) {
            return match h {
                EditHandle::Bounds(h) => h.cursor(),
                _ => CursorKind::Crosshair,
            };
        }
        if let Some(edit) = &self.text
            && self.elements.get(edit.idx).is_some_and(|e| e.bounds(self.metrics()).inflate(4.0 * self.ui_scale).contains(p))
        {
            return CursorKind::Text;
        }
        if let Some(h) = self.selection_handle_at(sel, p) {
            return h.cursor();
        }
        if let Some(h) = self.canvas_handle_at(p) {
            return h.cursor();
        }
        if self.element_at(p, self.tool.is_none()).is_some() {
            return CursorKind::Move;
        }
        match self.tool {
            Some(Tool::Text) if sel.contains(p) => CursorKind::Text,
            Some(_) if sel.contains(p) => CursorKind::Crosshair,
            None if sel.contains(p) && self.selection_mode() == SelectionMode::Pick => CursorKind::Move,
            _ => CursorKind::Default,
        }
    }

    fn element_at(&self, p: Point, allow_images: bool) -> Option<usize> {
        let m = self.metrics();
        self.elements.iter().rposition(|e| e.hit(p, m, allow_images))
    }

    fn selection_handle_at(&self, sel: Rect, p: Point) -> Option<Handle> {
        match self.selection_mode() {
            SelectionMode::Pick => sel.handle_at(p, HANDLE_HIT_RADIUS * self.ui_scale),
            SelectionMode::Canvas => None,
        }
    }

    /// Hit-tests the 8 outer canvas resize handles and the canvas edge perimeter
    /// in `SelectionMode::Canvas`.
    fn canvas_handle_at(&self, p: Point) -> Option<Handle> {
        if self.selection_mode() != SelectionMode::Canvas {
            return None;
        }
        let c = self.active_canvas_rect();
        let outer_ring = c.inflate(CANVAS_HANDLE_OFFSET * self.ui_scale);
        if let Some(h) = outer_ring.handle_at(p, HANDLE_HIT_RADIUS * self.ui_scale) {
            return Some(h);
        }
        let outer = c.inflate(CANVAS_EDGE_OUTER * self.ui_scale);
        let inner_inset = if self.tool.is_some() { 0.0 } else { CANVAS_EDGE_INNER * self.ui_scale };
        let inside_inner = c.w > 2.0 * inner_inset && c.h > 2.0 * inner_inset && c.inflate(-inner_inset).contains(p);
        if !outer.contains(p) || inside_inner {
            return None;
        }
        let corner_zone = 14.0 * self.ui_scale;
        let near_l = (p.x - c.x).abs() <= corner_zone;
        let near_r = (p.x - c.right()).abs() <= corner_zone;
        let near_t = (p.y - c.y).abs() <= corner_zone;
        let near_b = (p.y - c.bottom()).abs() <= corner_zone;
        match (near_l, near_r, near_t, near_b) {
            (true, _, true, _) => return Some(Handle::TopLeft),
            (_, true, true, _) => return Some(Handle::TopRight),
            (_, true, _, true) => return Some(Handle::BottomRight),
            (true, _, _, true) => return Some(Handle::BottomLeft),
            _ => {}
        }
        let candidates = [
            (Handle::Left, (p.x - c.x).abs()),
            (Handle::Right, (p.x - c.right()).abs()),
            (Handle::Top, (p.y - c.y).abs()),
            (Handle::Bottom, (p.y - c.bottom()).abs()),
        ];
        candidates.into_iter().min_by(|a, b| a.1.total_cmp(&b.1)).map(|(h, _)| h)
    }

    fn element_handle_at(&self, p: Point) -> Option<(usize, EditHandle)> {
        let idx = self.selected?;
        let m = self.metrics();
        let el = self.elements.get(idx)?;
        let inside_editing_text = self.text.as_ref().is_some_and(|t| {
            t.idx == idx && el.as_shape().and_then(|s| s.text_rect(m)).is_some_and(|r| r.contains(p))
        });
        let radius = if inside_editing_text {
            3.5 * self.ui_scale
        } else {
            HANDLE_HIT_RADIUS * self.ui_scale
        };
        let is_corner = |h: EditHandle| matches!(h, EditHandle::Bounds(b) if b.is_corner());
        el.handles(m)
            .into_iter()
            .filter(|(_, hp)| hp.distance(p) <= radius)
            .min_by(|(ha, pa), (hb, pb)| {
                pa.distance(p).total_cmp(&pb.distance(p)).then_with(|| is_corner(*hb).cmp(&is_corner(*ha)))
            })
            .map(|(h, _)| (idx, h))
    }

    fn update_hover(&mut self) {
        let p = self.pointer;
        if let Workspace::Capture { doc, windows, hover, hover_window, .. } = &mut self.workspace {
            let bounds = doc.bounds();
            *hover_window = windows.iter().position(|(clipped, _)| clipped.contains(p));
            *hover = Some(hover_window.map_or(bounds, |i| windows[i].0));
        }
    }

    // ----- pointer input --------------------------------------------------

    pub fn pointer_down(&mut self, p: Point) {
        if self.drag.is_some() {
            self.pointer_up(self.pointer);
        }
        self.pointer = p;
        self.touch();

        if let Some(edit_idx) = self.text.as_ref().map(|t| t.idx) {
            if self.start_text_handle_drag(p) {
                return;
            }
            if self.elements.get(edit_idx).is_some_and(|e| e.bounds(self.metrics()).inflate(4.0 * self.ui_scale).contains(p)) {
                self.set_caret_from_point(p, false);
                self.drag = Some(Drag::TextSelect);
            } else {
                self.finish_text();
            }
            return;
        }

        let Some(sel) = self.selection() else {
            self.update_hover();
            self.drag = Some(Drag::Pick { start: p, moved: false });
            return;
        };

        if let Some((idx, handle)) = self.element_handle_at(p) {
            self.begin_change();
            let is_shape = self.elements[idx].as_shape().is_some();
            self.drag = Some(Drag::ElementHandle { idx, handle, orig: self.elements[idx].clone(), start: p });
            if is_shape {
                self.touch_shapes();
            }
            return;
        }
        if let Some(handle) = self.selection_handle_at(sel, p) {
            self.drag = Some(Drag::SelResize { handle, orig: sel, start: p });
            return;
        }
        if let Some(handle) = self.canvas_handle_at(p) {
            let orig = self.screen();
            self.begin_change();
            self.drag = Some(Drag::CanvasResize { handle, orig, current: orig, start: p });
            return;
        }
        if let Some(idx) = self.element_at(p, self.tool.is_none()) {
            self.selected = Some(idx);
            self.begin_change();
            self.drag = Some(Drag::ElementMove { idx, orig: self.elements[idx].clone(), start: p, moved: false });
            return;
        }
        self.selected = None;
        if !sel.contains(p) {
            return;
        }
        match self.tool {
            Some(tool) => self.start_shape(tool, p),
            None if self.selection_mode() == SelectionMode::Pick => {
                self.drag = Some(Drag::SelMove { orig: sel, start: p });
            }
            None => {}
        }
    }

    fn start_shape(&mut self, tool: Tool, p: Point) {
        let style = self.styles[tool.index()];
        let geometry = match tool {
            Tool::Rect => Geometry::Rect(Rect::from_points(p, p)),
            Tool::Ellipse => Geometry::Ellipse(Rect::from_points(p, p)),
            Tool::Arrow => Geometry::Arrow { from: p, to: p, style: self.arrow_style },
            Tool::Pen => Geometry::Pen(vec![p]),
            Tool::Highlighter => Geometry::Highlight(vec![p]),
            Tool::Counter => Geometry::Counter {
                center: p,
                number: self.elements.iter().filter_map(Element::as_shape).filter(|s| s.tool() == Tool::Counter).count() as u32 + 1,
                format: self.counter_format(),
            },
            Tool::Redact => Geometry::Redact {
                area: match self.redaction.area {
                    RedactArea::Brush => RedactShape::Brush(vec![p]),
                    RedactArea::Rect => RedactShape::Rect(Rect::from_points(p, p)),
                },
                effect: self.redaction.effect,
                strength: self.redaction.strength,
            },
            Tool::Text => {
                let size = style.size.logical_px(Tool::Text) * self.scale;
                let half_line = self.fonts.get(self.text_style.family()).layout("", size).line_height / 2.0;
                Geometry::Text {
                    origin: Point::new(p.x, p.y - half_line),
                    content: String::new(),
                    style: self.text_style,
                    max_width: None,
                    min_height: None,
                }
            }
        };
        self.begin_change();
        self.elements.push(Element::Shape(Shape { geometry, style }));
        let idx = self.elements.len() - 1;
        self.selected = Some(idx);
        if tool == Tool::Text {
            self.text = Some(TextEdit::new(idx));
        } else {
            self.drag = Some(Drag::Create { idx, start: p });
        }
        self.touch_active();
    }

    pub fn pointer_move(&mut self, p: Point, constrain: bool) {
        let prev = self.pointer;
        self.pointer = p;
        self.constrain = constrain;
        if self.drag.is_none() {
            if self.is_picking() {
                let before = self.active_rect();
                self.update_hover();
                if self.active_rect() != before {
                    self.touch();
                }
            }
            return;
        }
        let screen = self.screen();
        let scale = self.ui_scale;
        let content_scale = self.scale;
        let is_canvas = matches!(self.workspace, Workspace::Canvas { .. });
        let mut created = None;
        let mut split_static = false;
        let mut moved_element = None;
        let mut text_select = false;

        let metrics = Metrics { scale: content_scale, fonts: &self.fonts };
        match self.drag.as_mut().expect("checked") {
            Drag::Pick { start, moved } => {
                if !*moved && start.distance(p) > DRAG_THRESHOLD * scale {
                    *moved = true;
                }
            }
            Drag::SelResize { handle, orig, start } => {
                let r = if constrain {
                    resize_rect_with_handle(*orig, *handle, p - *start, true, 4.0)
                } else {
                    orig.resized(*handle, p - *start)
                };
                let clamped = r.intersect(&screen).unwrap_or_else(|| Rect::new(r.x, r.y, 1.0, 1.0).clamp_within(&screen));
                if let Workspace::Capture { selection, .. } = &mut self.workspace {
                    *selection = Some(clamped);
                }
            }
            Drag::SelMove { orig, start } => {
                let moved = orig.translate(p - *start).clamp_within(&screen);
                if let Workspace::Capture { selection, .. } = &mut self.workspace {
                    *selection = Some(moved);
                }
            }
            Drag::CanvasResize { handle, orig, current, start } => {
                let min_side = (MIN_CANVAS_LOGICAL * content_scale).max(8.0);
                let r = resize_rect_with_handle(*orig, *handle, p - *start, constrain, min_side);
                let next = Rect::new(r.x.round(), r.y.round(), r.w.round().max(min_side), r.h.round().max(min_side));
                if crate::format::valid_canvas_size(next.w.round() as u32, next.h.round() as u32) {
                    *current = next;
                }
            }
            Drag::Create { idx, start } => {
                let elem_idx = *idx;
                created = Some(elem_idx);
                let start = *start;
                if let Some(Shape { geometry, .. }) = self.elements.get_mut(elem_idx).and_then(Element::as_shape_mut) {
                    match geometry {
                        Geometry::Rect(r) | Geometry::Ellipse(r) | Geometry::Redact { area: RedactShape::Rect(r), .. } => {
                            *r = Rect::from_points(start, constrained(start, p, constrain))
                        }
                        Geometry::Arrow { to, .. } => *to = snapped(start, p, constrain),
                        Geometry::Counter { center, .. } => *center = p,
                        Geometry::Pen(pts)
                        | Geometry::Highlight(pts)
                        | Geometry::Redact { area: RedactShape::Brush(pts), .. }
                            if constrain =>
                        {
                            *pts = vec![start, snapped(start, p, true)]
                        }
                        Geometry::Pen(pts)
                        | Geometry::Highlight(pts)
                        | Geometry::Redact { area: RedactShape::Brush(pts), .. } => {
                            if pts.last().is_none_or(|last| last.distance(p) >= MIN_STROKE_STEP * scale) {
                                pts.push(p);
                            }
                        }
                        Geometry::Text { .. } => {}
                    }
                }
            }
            Drag::ElementMove { idx, orig, start, moved } => {
                let was_moved = *moved;
                *moved |= start.distance(p) > DRAG_THRESHOLD * scale;
                if *moved {
                    let elem_idx = *idx;
                    let next = orig.translated(p - *start);
                    split_static = !was_moved && matches!(next, Element::Shape(_));
                    moved_element = Some((elem_idx, std::mem::replace(&mut self.elements[elem_idx], next)));
                }
            }
            Drag::ElementHandle { idx, handle, orig, start } => {
                let elem_idx = *idx;
                let next = orig.with_handle_moved(*handle, p - *start, constrain, metrics);
                moved_element = Some((elem_idx, std::mem::replace(&mut self.elements[elem_idx], next)));
            }
            Drag::TextSelect => {
                text_select = true;
            }
        }
        if text_select {
            self.set_caret_from_point(p, true);
        } else if let Some((idx, previous)) = moved_element {
            self.invalidate_backdrop_from(idx);
            if split_static || (is_canvas && self.edit_affects_upper_effect(idx, &previous)) {
                self.touch_shapes();
            } else if self.elements[idx].as_shape().is_some() {
                self.touch_active();
            } else {
                self.touch();
            }
        } else if let Some(idx) = created {
            self.invalidate_backdrop_from(idx);
            if is_canvas && self.has_effect_redaction_above(idx) {
                self.touch_shapes();
            } else {
                self.touch_active();
            }
        } else if prev != p {
            self.touch();
        }
    }

    pub fn constrain_changed(&mut self, constrain: bool) {
        self.constrain = constrain;
        if matches!(
            self.drag,
            Some(Drag::Create { .. } | Drag::ElementHandle { .. } | Drag::CanvasResize { .. } | Drag::SelResize { .. })
        ) {
            self.pointer_move(self.pointer, constrain);
            self.touch();
        }
    }

    pub fn pointer_up(&mut self, p: Point) {
        self.pointer = p;
        let Some(drag) = self.drag.take() else { return };
        match drag {
            Drag::Pick { start, moved } => {
                self.touch();
                let screen = self.screen();
                let r = Rect::from_points(start, p).intersect(&screen);
                let min = DRAG_THRESHOLD * self.ui_scale;
                if let Workspace::Capture { selection, hover, hover_window, snapped, windows, .. } = &mut self.workspace {
                    self.content_revision += 1;
                    (*selection, *snapped) = match r {
                        Some(r) if moved && r.w >= min && r.h >= min => (Some(r.round()), None),
                        // A click picks the window under the pointer, outline and all.
                        _ => (*hover, hover_window.map(|i| windows[i].1.outline()).filter(WindowOutline::is_rounded)),
                    };
                }
            }
            Drag::SelResize { orig, .. } | Drag::SelMove { orig, .. } => {
                self.touch();
                if let Workspace::Capture { selection, snapped, .. } = &mut self.workspace {
                    *selection = selection.map(|s| s.round());
                    // Any change to a snapped selection makes it a plain region.
                    if *selection != Some(orig.round()) {
                        self.content_revision += 1;
                        *snapped = None;
                    }
                }
            }
            Drag::CanvasResize { current, .. } => {
                let r = current.round();
                let new_w = (r.w.round() as u32).max(1);
                let new_h = (r.h.round() as u32).max(1);
                let shift = Point::new(-r.x, -r.y);
                if let Workspace::Canvas { width, height, backdrop, .. } = &mut self.workspace {
                    *width = new_w;
                    *height = new_h;
                    *backdrop.borrow_mut() = None;
                }
                if shift != Point::default() {
                    for el in &mut self.elements {
                        *el = el.translated(shift);
                    }
                    self.pending_origin_shift = self.pending_origin_shift + Point::new(r.x, r.y);
                }
                self.end_change();
                self.touch_shapes();
            }
            Drag::Create { idx, .. } => {
                let degenerate = self.elements[idx].is_degenerate();
                if degenerate {
                    self.elements.remove(idx);
                    self.invalidate_backdrop_from(idx);
                    self.selected = None;
                }
                self.end_change();
                if degenerate && idx == self.elements.len() {
                    self.touch_active();
                } else {
                    self.touch_shapes();
                }
            }
            Drag::ElementMove { idx, moved, .. } => {
                self.end_change();
                if !moved {
                    if self
                        .elements
                        .get(idx)
                        .and_then(Element::as_shape)
                        .is_some_and(|s| matches!(s.geometry, Geometry::Text { .. }))
                    {
                        self.edit_text(idx, p);
                    } else {
                        self.touch();
                    }
                } else {
                    self.invalidate_backdrop_from(idx);
                    if matches!(self.elements[idx], Element::Shape(_)) || self.has_effect_redaction_above(idx) {
                        self.touch_shapes();
                    } else {
                        self.touch();
                    }
                }
            }
            Drag::ElementHandle { idx, orig, .. } => {
                let changed = self.elements[idx] != orig;
                if changed {
                    self.invalidate_backdrop_from(idx);
                }
                if self.text.is_none() {
                    self.end_change();
                    if matches!(self.elements[idx], Element::Shape(_)) || (changed && self.has_effect_redaction_above(idx)) {
                        self.touch_shapes();
                    } else {
                        self.touch();
                    }
                } else if changed && self.has_effect_redaction_above(idx) {
                    self.touch_shapes();
                } else {
                    self.touch_active();
                }
            }
            Drag::TextSelect => {}
        }
    }

    /// Returns true when the double click should copy the capture (Capture mode only).
    pub fn double_click(&mut self, p: Point) -> bool {
        self.selection_mode() == SelectionMode::Pick
            && self.text.is_none()
            && self.tool.is_none()
            && self.selection().is_some_and(|s| s.contains(p))
            && self.element_at(p, false).is_none()
    }

    pub fn right_click(&mut self) -> RightClick {
        self.right_click_at(self.pointer)
    }

    pub fn right_click_at(&mut self, p: Point) -> RightClick {
        if self.drag.is_some() {
            self.pointer_up(self.pointer);
            return RightClick::Handled;
        }
        self.pointer = p;
        self.touch();
        if self.text.is_some() {
            self.finish_text();
            return RightClick::Handled;
        }
        if self.selection_mode() == SelectionMode::Canvas {
            // In the Editor, right-click exits an active drawing tool first.
            if self.tool.take().is_some() {
                self.selected = None;
                return RightClick::Handled;
            }
            // Right-clicking exposed background space (not hitting any placed image
            // or annotation) opens the background color context menu.
            if self.element_at(p, true).is_none() {
                self.selected = None;
                return RightClick::BackgroundMenu;
            }
            return RightClick::Handled;
        }
        if !self.elements.is_empty() {
            return RightClick::Handled;
        }
        let Workspace::Capture { selection, snapped, .. } = &mut self.workspace else {
            return RightClick::Handled;
        };
        if selection.is_none() {
            return RightClick::Exit;
        }
        self.content_revision += 1;
        *selection = None;
        *snapped = None;
        self.history.clear();
        self.redo_history.clear();
        self.tool = None;
        self.selected = None;
        self.update_hover();
        RightClick::Handled
    }

    // ----- commands -------------------------------------------------------

    /// Sets the canvas background color in Canvas mode as one undoable step.
    pub fn set_background_color(&mut self, color: Color) -> bool {
        if !matches!(self.workspace, Workspace::Canvas { .. }) {
            return false;
        }
        self.finish_text();
        if self.background_color() == Some(color) {
            return false;
        }
        self.begin_change();
        if let Workspace::Canvas { background, backdrop, .. } = &mut self.workspace {
            *background = color;
            *backdrop.borrow_mut() = None;
        }
        self.end_change();
        if self.has_any_effect_redaction() {
            self.touch_shapes();
        } else {
            self.touch();
        }
        true
    }

    pub fn toggle_tool(&mut self, tool: Tool) {
        self.finish_text();
        self.tool = if self.tool == Some(tool) { None } else { Some(tool) };
        self.selected = None;
        self.touch();
    }

    /// Clears the active tool or selection in Canvas mode (used by `Escape` so
    /// switching from drawing back to moving/resizing images is instant).
    pub fn clear_tool_or_selection(&mut self) -> bool {
        if self.tool.take().is_some() {
            self.touch();
            return true;
        }
        if self.selected.take().is_some() {
            self.touch();
            return true;
        }
        false
    }

    pub fn set_color(&mut self, color: Color) {
        if let Some((tool, _)) = self.current_style()
            && tool.uses_color()
            && tool != Tool::Highlighter
        {
            for t in Tool::ALL {
                if t.uses_color() && t != Tool::Highlighter {
                    self.styles[t.index()].color = color;
                }
            }
        }
        self.update_style(|s| s.color = color);
    }

    pub fn set_size(&mut self, size: SizeLevel) {
        self.update_style(|s| s.size = size);
    }

    fn update_style(&mut self, f: impl Fn(&mut Style)) {
        let Some((tool, _)) = self.current_style() else { return };
        f(&mut self.styles[tool.index()]);
        if let Some(idx) = self.selected
            && self.elements.get(idx).is_some_and(|e| matches!(e, Element::Shape(_)))
        {
            if self.text.is_none() {
                self.begin_change();
            }
            if let Some(shape) = self.elements.get_mut(idx).and_then(Element::as_shape_mut) {
                f(&mut shape.style);
            }
            self.invalidate_backdrop_from(idx);
            if self.text.is_none() {
                self.end_change();
            }
        }
        self.touch_shapes();
    }

    fn settle_in_flight(&mut self) {
        if self.drag.is_some() {
            self.pointer_up(self.pointer);
        }
        self.finish_text();
        self.end_continuous_edit();
    }

    fn apply_snapshot(&mut self, snap: Snapshot) {
        if let Workspace::Canvas { width, height, background, backdrop } = &mut self.workspace {
            let old_state = (*width, *height, *background);
            (*width, *height) = snap.canvas;
            if let Some(bg) = snap.background {
                *background = bg;
            }
            if old_state != (*width, *height, *background) {
                *backdrop.borrow_mut() = None;
            }
        }
        self.elements = snap.elements;
        self.content_revision += 1;
        self.invalidate_backdrop();
        self.selected = None;
        self.touch_shapes();
    }

    pub fn undo(&mut self) -> bool {
        if self.text.is_some() {
            return self.text_undo();
        }
        self.settle_in_flight();
        let Some(prev) = self.history.pop_back() else { return false };
        let current = self.current_snapshot();
        while self.redo_history.len() >= MAX_HISTORY {
            self.redo_history.pop_front();
        }
        self.redo_history.push_back(current);
        self.apply_snapshot(prev);
        true
    }

    pub fn redo(&mut self) -> bool {
        if self.text.is_some() {
            return self.text_redo();
        }
        self.settle_in_flight();
        let Some(next) = self.redo_history.pop_back() else { return false };
        let current = self.current_snapshot();
        while self.history.len() >= MAX_HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(current);
        self.apply_snapshot(next);
        true
    }

    pub fn delete_selected(&mut self) -> bool {
        let active_drag = matches!(
            self.drag,
            Some(Drag::Create { .. } | Drag::ElementMove { .. } | Drag::ElementHandle { .. } | Drag::CanvasResize { .. })
        );
        if self.text.is_some() || active_drag {
            return false;
        }
        let Some(idx) = self.selected.take() else { return false };
        self.begin_change();
        self.elements.remove(idx);
        self.invalidate_backdrop();
        renumber_counters_in(&mut self.elements);
        self.end_change();
        self.touch_shapes();
        true
    }

    fn current_snapshot(&self) -> Snapshot {
        Snapshot { canvas: self.canvas_dims(), background: self.background_color(), elements: self.elements.clone() }
    }

    fn push_snapshot(&mut self, snap: Snapshot) {
        // Allow one in-flight tentative snapshot (`MAX_HISTORY + 1`) so a no-op
        // click reverted by `end_change()` never evicts the oldest undo entry.
        while self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(snap);
    }

    fn begin_change(&mut self) {
        self.push_snapshot(self.current_snapshot());
    }

    fn end_change(&mut self) {
        let unchanged = self.history.back().is_some_and(|last| {
            last.canvas == self.canvas_dims()
                && last.background == self.background_color()
                && last.elements == self.elements
        });
        if unchanged {
            self.history.pop_back();
        } else {
            self.redo_history.clear();
            self.content_revision += 1;
            while self.history.len() > MAX_HISTORY {
                self.history.pop_front();
            }
        }
    }

    // ----- text editing ---------------------------------------------------

    pub fn is_editing_text(&self) -> bool {
        self.text.is_some()
    }

    fn edit_text(&mut self, idx: usize, p: Point) {
        self.begin_change();
        self.selected = Some(idx);
        self.text = Some(TextEdit::new(idx));
        self.set_caret_from_point(p, false);
        self.touch_shapes();
    }

    fn set_caret_from_point(&mut self, p: Point, extend: bool) {
        let Some(edit) = &self.text else { return };
        let Some(shape) = self.elements.get(edit.idx).and_then(Element::as_shape) else { return };
        let Some((origin, layout)) = self.text_layout(shape) else { return };
        let caret = layout.index_at(p - origin);
        if let Some(edit) = &mut self.text {
            if !extend {
                edit.anchor = caret;
            }
            edit.caret = caret;
            edit.marked = None;
            edit.preferred_x = None;
            edit.last_edit = None;
        }
        self.touch_active();
    }

    fn set_text_selection(&mut self, anchor: usize, caret: usize) {
        if let Some(edit) = &mut self.text {
            edit.anchor = anchor;
            edit.caret = caret;
            edit.marked = None;
            edit.preferred_x = None;
            edit.last_edit = None;
            self.touch_active();
        }
    }

    fn start_text_handle_drag(&mut self, p: Point) -> bool {
        let Some(edit_idx) = self.text.as_ref().map(|t| t.idx) else {
            return false;
        };
        let Some((idx, handle)) = self.element_handle_at(p) else {
            return false;
        };
        if idx != edit_idx {
            return false;
        }
        self.drag = Some(Drag::ElementHandle { idx, handle, orig: self.elements[idx].clone(), start: p });
        self.touch_active();
        true
    }

    /// Handles a left mouse press while editing text, supporting 8-point box
    /// resize handles, Shift+click selection extension, drag-to-select,
    /// double-click word selection, and triple-click line selection. Returns
    /// `false` if not currently editing text.
    pub fn text_pointer_down(&mut self, p: Point, extend: bool, click_count: usize) -> bool {
        let Some(edit_idx) = self.text.as_ref().map(|t| t.idx) else {
            return false;
        };
        if self.drag.is_some() {
            self.pointer_up(self.pointer);
        }
        self.pointer = p;
        self.touch();
        if self.start_text_handle_drag(p) {
            return true;
        }
        if !self.elements.get(edit_idx).is_some_and(|e| e.bounds(self.metrics()).inflate(4.0 * self.ui_scale).contains(p)) {
            self.finish_text();
            return true;
        }
        if click_count >= 3 {
            if let Some(index) = self.text_index_at(p)
                && let Some(content) = self.text_content()
            {
                let start = content[..index].rfind('\n').map_or(0, |i| i + 1);
                let end = content[index..].find('\n').map_or(content.len(), |i| index + i);
                self.set_text_selection(start, end);
            }
        } else if click_count == 2 {
            if let Some(index) = self.text_index_at(p)
                && let Some(content) = self.text_content()
            {
                let range = word_range_at(content, index);
                self.set_text_selection(range.start, range.end);
            }
        } else {
            self.set_caret_from_point(p, extend);
            self.drag = Some(Drag::TextSelect);
        }
        true
    }

    pub fn finish_text(&mut self) {
        if matches!(self.drag, Some(Drag::TextSelect | Drag::ElementHandle { .. })) {
            self.drag = None;
        }
        let Some(edit) = self.text.take() else { return };
        let removed_tail = if self.elements.get(edit.idx).is_some_and(Element::is_degenerate) {
            self.elements.remove(edit.idx);
            self.invalidate_backdrop_from(edit.idx);
            self.selected = None;
            edit.idx == self.elements.len()
        } else {
            false
        };
        self.end_change();
        if removed_tail {
            self.touch_active();
        } else {
            self.touch_shapes();
        }
    }

    fn text_undo(&mut self) -> bool {
        if matches!(self.drag, Some(Drag::TextSelect)) {
            self.drag = None;
        } else if matches!(self.drag, Some(Drag::ElementHandle { .. })) {
            self.pointer_up(self.pointer);
        }
        let Some(edit) = self.text.as_mut() else { return false };
        let Some(prev) = edit.undo.pop_back() else { return false };
        let idx = edit.idx;
        let Some(Shape { geometry: Geometry::Text { content, .. }, .. }) =
            self.elements.get_mut(idx).and_then(Element::as_shape_mut)
        else {
            return false;
        };
        let current = TextSnapshot {
            content: std::mem::replace(content, prev.content),
            anchor: edit.anchor,
            caret: edit.caret,
        };
        while edit.redo.len() >= MAX_HISTORY {
            edit.redo.pop_front();
        }
        edit.redo.push_back(current);
        edit.anchor = prev.anchor.min(content.len());
        edit.caret = prev.caret.min(content.len());
        edit.marked = None;
        edit.preferred_x = None;
        edit.last_edit = None;
        self.invalidate_backdrop_from(idx);
        if matches!(self.workspace, Workspace::Canvas { .. }) && self.has_effect_redaction_above(idx) {
            self.touch_shapes();
        } else {
            self.touch_active();
        }
        true
    }

    fn text_redo(&mut self) -> bool {
        if matches!(self.drag, Some(Drag::TextSelect)) {
            self.drag = None;
        } else if matches!(self.drag, Some(Drag::ElementHandle { .. })) {
            self.pointer_up(self.pointer);
        }
        let Some(edit) = self.text.as_mut() else { return false };
        let Some(next) = edit.redo.pop_back() else { return false };
        let idx = edit.idx;
        let Some(Shape { geometry: Geometry::Text { content, .. }, .. }) =
            self.elements.get_mut(idx).and_then(Element::as_shape_mut)
        else {
            return false;
        };
        let current = TextSnapshot {
            content: std::mem::replace(content, next.content),
            anchor: edit.anchor,
            caret: edit.caret,
        };
        while edit.undo.len() >= MAX_HISTORY {
            edit.undo.pop_front();
        }
        edit.undo.push_back(current);
        edit.anchor = next.anchor.min(content.len());
        edit.caret = next.caret.min(content.len());
        edit.marked = None;
        edit.preferred_x = None;
        edit.last_edit = None;
        self.invalidate_backdrop_from(idx);
        if matches!(self.workspace, Workspace::Canvas { .. }) && self.has_effect_redaction_above(idx) {
            self.touch_shapes();
        } else {
            self.touch_active();
        }
        true
    }

    pub fn text_content(&self) -> Option<&str> {
        let edit = self.text.as_ref()?;
        match &self.elements.get(edit.idx)?.as_shape()?.geometry {
            Geometry::Text { content, .. } => Some(content),
            _ => None,
        }
    }

    pub fn text_caret(&self) -> Option<usize> {
        self.text.as_ref().map(|e| e.caret)
    }

    pub fn text_selection(&self) -> Option<Range<usize>> {
        self.text.as_ref().map(TextEdit::selection)
    }

    pub fn text_selection_reversed(&self) -> bool {
        self.text.as_ref().is_some_and(TextEdit::is_reversed)
    }

    pub fn text_selected(&self) -> Option<&str> {
        let range = self.text_selection()?;
        if range.is_empty() {
            return None;
        }
        self.text_content()?.get(range)
    }

    pub fn text_marked(&self) -> Option<Range<usize>> {
        self.text.as_ref()?.marked.clone()
    }

    pub fn text_unmark(&mut self) {
        if let Some(edit) = &mut self.text {
            edit.marked = None;
            edit.last_edit = None;
            self.touch_active();
        }
    }

    pub fn text_replace(&mut self, range: Option<Range<usize>>, new_text: &str, mark: bool, caret_in_new: Option<usize>) {
        let Some(edit) = self.text.as_mut() else { return };
        let idx = edit.idx;
        let Some(Shape { geometry: Geometry::Text { content, .. }, .. }) =
            self.elements.get_mut(idx).and_then(Element::as_shape_mut)
        else {
            return;
        };
        let was_marked = edit.marked.is_some();
        let range = range.or_else(|| edit.marked.clone()).unwrap_or_else(|| edit.selection());
        if range.start > range.end || !content.is_char_boundary(range.start) || !content.is_char_boundary(range.end) {
            return;
        }
        if !mark && !was_marked && range.is_empty() && new_text.is_empty() {
            return;
        }

        if mark {
            if !was_marked {
                edit.push_undo(TextSnapshot { content: content.clone(), anchor: edit.anchor, caret: edit.caret });
            }
            edit.redo.clear();
            edit.last_edit = Some(TextEditKind::Other);
        } else if !was_marked {
            let removed = &content[range.clone()];
            let single_insert =
                range.is_empty() && range.start == edit.caret && !edit.has_selection() && new_text.chars().count() == 1;
            let single_delete = new_text.is_empty()
                && !edit.has_selection()
                && removed.chars().count() == 1
                && (range.end == edit.caret || range.start == edit.caret)
                && removed != "\n";
            let (coalesce, next_kind) = if single_insert {
                let ch = new_text.chars().next().expect("single char");
                if ch == '\n' {
                    (false, Some(TextEditKind::Other))
                } else if ch.is_whitespace() {
                    // Attach trailing space to the current word, then start a new undo group on the next word.
                    (edit.last_edit == Some(TextEditKind::Insert), None)
                } else {
                    (edit.last_edit == Some(TextEditKind::Insert), Some(TextEditKind::Insert))
                }
            } else if single_delete {
                (edit.last_edit == Some(TextEditKind::Delete), Some(TextEditKind::Delete))
            } else {
                (false, Some(TextEditKind::Other))
            };
            if !coalesce {
                edit.push_undo(TextSnapshot { content: content.clone(), anchor: edit.anchor, caret: edit.caret });
            }
            edit.redo.clear();
            edit.last_edit = next_kind;
        } else {
            // Committing (or cancelling) an active IME composition: the pre-composition
            // snapshot is already on top of `edit.undo`.
            edit.redo.clear();
            edit.last_edit = Some(TextEditKind::Other);
        }

        content.replace_range(range.clone(), new_text);
        if was_marked && !mark && edit.undo.back().is_some_and(|s| s.content == *content) {
            edit.undo.pop_back();
            edit.last_edit = None;
        }
        let end = range.start + new_text.len();
        let caret = caret_in_new
            .map(|c| range.start + c.min(new_text.len()))
            .filter(|c| content.is_char_boundary(*c))
            .unwrap_or(end);
        edit.anchor = caret;
        edit.caret = caret;
        edit.marked = (mark && !new_text.is_empty()).then_some(range.start..end);
        edit.preferred_x = None;
        self.invalidate_backdrop_from(idx);
        if matches!(self.workspace, Workspace::Canvas { .. }) && self.has_effect_redaction_above(idx) {
            self.touch_shapes();
        } else {
            self.touch_active();
        }
    }

    pub fn text_key(&mut self, key: TextKey) {
        let Some((idx, anchor, c, sel, has_sel, preferred_x)) = self
            .text
            .as_ref()
            .map(|e| (e.idx, e.anchor, e.caret, e.selection(), e.has_selection(), e.preferred_x))
        else {
            return;
        };
        let Some(content) = self.text_content() else { return };
        let prev = content[..c].char_indices().next_back().map_or(0, |(i, _)| i);
        let next = content[c..].chars().next().map_or(c, |ch| c + ch.len_utf8());
        let (line_start, line_end) = self
            .elements
            .get(idx)
            .and_then(Element::as_shape)
            .and_then(|s| self.text_layout(s))
            .map_or_else(
                || {
                    (
                        content[..c].rfind('\n').map_or(0, |i| i + 1),
                        content[c..].find('\n').map_or(content.len(), |i| c + i),
                    )
                },
                |(_, layout)| {
                    let r = layout.visual_line_range(c);
                    (r.start, r.end)
                },
            );
        let content_len = content.len();
        let vertical = |from: usize, delta: isize, preferred_x: Option<f32>| -> (usize, Option<f32>) {
            self.elements
                .get(idx)
                .and_then(Element::as_shape)
                .and_then(|s| self.text_layout(s))
                .map_or((from, None), |(_, layout)| layout.vertical_move(from, delta, preferred_x))
        };
        match key {
            TextKey::Backspace => {
                if has_sel {
                    self.text_replace(Some(sel), "", false, None);
                } else if c > 0 {
                    self.text_replace(Some(prev..c), "", false, None);
                }
            }
            TextKey::Delete => {
                if has_sel {
                    self.text_replace(Some(sel), "", false, None);
                } else if next > c {
                    self.text_replace(Some(c..next), "", false, None);
                }
            }
            TextKey::DeleteWordLeft => {
                if has_sel {
                    self.text_replace(Some(sel), "", false, None);
                } else if c > 0 {
                    let target = prev_word_boundary(content, c);
                    self.text_replace(Some(target..c), "", false, None);
                }
            }
            TextKey::DeleteWordRight => {
                if has_sel {
                    self.text_replace(Some(sel), "", false, None);
                } else if c < content_len {
                    let target = next_word_boundary(content, c);
                    self.text_replace(Some(c..target), "", false, None);
                }
            }
            TextKey::Newline => self.text_replace(None, "\n", false, None),
            TextKey::SelectAll => {
                self.set_text_selection(0, content_len);
            }
            TextKey::Left => {
                let caret = if has_sel { sel.start } else { prev };
                self.set_text_selection(caret, caret);
            }
            TextKey::Right => {
                let caret = if has_sel { sel.end } else { next };
                self.set_text_selection(caret, caret);
            }
            TextKey::Up | TextKey::Down => {
                let delta = if key == TextKey::Up { -1 } else { 1 };
                let (origin, pref) = if has_sel {
                    (if delta < 0 { sel.start } else { sel.end }, None)
                } else {
                    (c, preferred_x)
                };
                let (caret, next_pref) = vertical(origin, delta, pref);
                if let Some(edit) = self.text.as_mut() {
                    edit.anchor = caret;
                    edit.caret = caret;
                    edit.marked = None;
                    edit.preferred_x = next_pref;
                    edit.last_edit = None;
                }
                self.touch_active();
            }
            TextKey::WordLeft => {
                let caret = prev_word_boundary(content, c);
                self.set_text_selection(caret, caret);
            }
            TextKey::WordRight => {
                let caret = next_word_boundary(content, c);
                self.set_text_selection(caret, caret);
            }
            TextKey::Home => self.set_text_selection(line_start, line_start),
            TextKey::End => self.set_text_selection(line_end, line_end),
            TextKey::DocStart => self.set_text_selection(0, 0),
            TextKey::DocEnd => self.set_text_selection(content_len, content_len),
            TextKey::SelectLeft => self.set_text_selection(anchor, prev),
            TextKey::SelectRight => self.set_text_selection(anchor, next),
            TextKey::SelectUp | TextKey::SelectDown => {
                let delta = if key == TextKey::SelectUp { -1 } else { 1 };
                let (caret, next_pref) = vertical(c, delta, preferred_x);
                if let Some(edit) = self.text.as_mut() {
                    edit.anchor = anchor;
                    edit.caret = caret;
                    edit.marked = None;
                    edit.preferred_x = next_pref;
                    edit.last_edit = None;
                }
                self.touch_active();
            }
            TextKey::SelectWordLeft => {
                let caret = prev_word_boundary(content, c);
                self.set_text_selection(anchor, caret);
            }
            TextKey::SelectWordRight => {
                let caret = next_word_boundary(content, c);
                self.set_text_selection(anchor, caret);
            }
            TextKey::SelectHome => self.set_text_selection(anchor, line_start),
            TextKey::SelectEnd => self.set_text_selection(anchor, line_end),
            TextKey::SelectDocStart => self.set_text_selection(anchor, 0),
            TextKey::SelectDocEnd => self.set_text_selection(anchor, content_len),
        }
    }

    fn text_layout(&self, shape: &Shape) -> Option<(Point, TextLayout)> {
        let Geometry::Text { origin, content, style, max_width, .. } = &shape.geometry else { return None };
        Some((*origin, self.fonts.get(style.family()).layout_wrapped(content, shape.size_px(self.scale), *max_width)))
    }

    pub fn text_ink(&self) -> Option<Color> {
        let shape = self.elements.get(self.text.as_ref()?.idx)?.as_shape()?;
        let Geometry::Text { style, .. } = &shape.geometry else { return None };
        Some(style.ink(shape.style.color))
    }

    pub fn text_box_color(&self) -> Option<Color> {
        let shape = self.elements.get(self.text.as_ref()?.idx)?.as_shape()?;
        let Geometry::Text { style, .. } = &shape.geometry else { return None };
        matches!(style, TextStyle::Boxed | TextStyle::RoundedBoxed).then_some(shape.style.color)
    }

    pub fn text_selection_rects(&self) -> Vec<Rect> {
        let Some(edit) = self.text.as_ref() else { return Vec::new() };
        if !edit.has_selection() {
            return Vec::new();
        }
        let Some(shape) = self.elements.get(edit.idx).and_then(Element::as_shape) else { return Vec::new() };
        let Some((origin, layout)) = self.text_layout(shape) else { return Vec::new() };
        layout.selection_rects(edit.selection()).into_iter().map(|r| r.translate(origin)).collect()
    }

    pub fn text_caret_rect_at(&self, index: usize) -> Option<Rect> {
        let edit = self.text.as_ref()?;
        let shape = self.elements.get(edit.idx)?.as_shape()?;
        let (origin, layout) = self.text_layout(shape)?;
        Some(layout.caret_rect(index).translate(origin))
    }

    pub fn text_caret_rect(&self) -> Option<Rect> {
        self.text_caret_rect_at(self.text.as_ref()?.caret)
    }

    pub fn text_index_at(&self, p: Point) -> Option<usize> {
        let edit = self.text.as_ref()?;
        let shape = self.elements.get(edit.idx)?.as_shape()?;
        let (origin, layout) = self.text_layout(shape)?;
        Some(layout.index_at(p - origin))
    }

    pub fn text_frame(&self) -> Option<(Rect, Vec<Point>)> {
        let edit = self.text.as_ref()?;
        let el = self.elements.get(edit.idx)?;
        let m = self.metrics();
        let frame = el.as_shape()?.text_frame_rect(m)?;
        let handles = el.handles(m).into_iter().map(|(_, p)| p).collect();
        Some((frame, handles))
    }

    // ----- output ---------------------------------------------------------

    pub fn static_layer_revision(&self) -> u64 {
        self.static_revision
    }

    pub fn active_layer_revision(&self) -> u64 {
        self.active_revision
    }

    fn rectangular_redaction_bounds(&self, range: Range<usize>) -> Option<Rect> {
        let rect = |element: &Element| element.as_shape()?.redaction_rect();
        let (first, rest) = self.elements.get(range)?.split_first()?;
        let bounds = rest.iter().try_fold(rect(first)?, |bounds, el| Some(bounds.union(&rect(el)?)))?;
        // Include the same antialiasing gutter as render_shapes.
        Some(bounds.inflate(1.0).round_out())
    }

    /// Rasterize disjoint groups of rectangular redactions separately when it
    /// reduces the upload. The frontend can opt in when texture placement maps
    /// canvas pixels to whole device pixels, preserving sampling across groups.
    pub fn render_band_parts(
        &self,
        range: Range<usize>,
        active: bool,
        clip: Rect,
        split: bool,
        mut emit: impl FnMut(Rect, Pixmap),
    ) {
        if split && !active && self.selection_mode() == SelectionMode::Canvas {
            if let Some(visible) = clip.round_out().intersect(&self.active_canvas_rect()) {
                self.render_sparse_band(range, visible, &mut emit);
            }
        } else if let Some((rect, pixmap)) = self.render_band(range, active, clip) {
            emit(rect, pixmap);
        }
    }

    fn render_sparse_band(&self, range: Range<usize>, clip: Rect, emit: &mut impl FnMut(Rect, Pixmap)) {
        // Balance another texture and draw call against unused BGRA upload bytes.
        const MIN_SAVED_PIXELS: f32 = 128.0 * 128.0;
        if range.len() >= 2 {
            let mid = range.start + range.len() / 2;
            if let Some(a) = self.rectangular_redaction_bounds(range.start..mid).and_then(|r| r.intersect(&clip))
                && let Some(b) = self.rectangular_redaction_bounds(mid..range.end).and_then(|r| r.intersect(&clip))
                && a.intersect(&b).is_none()
            {
                let whole = a.union(&b);
                if whole.w * whole.h - a.w * a.h - b.w * b.h >= MIN_SAVED_PIXELS {
                    self.render_sparse_band(range.start..mid, clip, emit);
                    self.render_sparse_band(mid..range.end, clip, emit);
                    return;
                }
            }
        }
        if let Some((rect, pixmap)) = self.render_band(range, false, clip) {
            emit(rect, pixmap);
        }
    }

    /// Z-ordered GPU rendering bands: placed images are drawn directly by GPUI,
    /// while contiguous runs of shapes are rasterized into cached GPU layers.
    pub fn visual_bands(&self) -> Vec<VisualBand> {
        let active = self.active_index();
        let mut bands = Vec::new();
        let mut shape_run: Option<Range<usize>> = None;
        let flush_run = |run: &mut Option<Range<usize>>, bands: &mut Vec<VisualBand>| {
            if let Some(range) = run.take() {
                bands.push(VisualBand::Shapes { range, revision: self.static_revision, active: false });
            }
        };
        for (i, el) in self.elements.iter().enumerate() {
            match el {
                Element::Image(placed) => {
                    flush_run(&mut shape_run, &mut bands);
                    bands.push(VisualBand::Image(placed.clone()));
                }
                Element::Shape(_) if Some(i) == active => {
                    flush_run(&mut shape_run, &mut bands);
                    bands.push(VisualBand::Shapes { range: i..i + 1, revision: self.active_revision, active: true });
                }
                Element::Shape(_) => match &mut shape_run {
                    Some(r) => r.end = i + 1,
                    None => shape_run = Some(i..i + 1),
                },
            }
        }
        flush_run(&mut shape_run, &mut bands);
        bands
    }

    pub fn render_band(&self, range: Range<usize>, active: bool, clip: Rect) -> Option<(Rect, Pixmap)> {
        let start = range.start;
        let shapes: Vec<(usize, &Shape)> = self
            .elements
            .get(range)?
            .iter()
            .enumerate()
            .filter_map(|(off, el)| Some((start + off, el.as_shape()?)))
            .collect();
        if shapes.is_empty() {
            return None;
        }
        let underline = if active {
            self.text.as_ref().and_then(|e| Some((0, e.marked.clone()?)))
        } else {
            None
        };
        self.render_shapes(&shapes, underline, clip)
    }

    pub fn render_static_layer(&self, clip: Rect) -> Option<(Rect, Pixmap)> {
        let active = self.active_index();
        let shapes: Vec<(usize, &Shape)> = self
            .elements
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != active)
            .filter_map(|(i, el)| Some((i, el.as_shape()?)))
            .collect();
        self.render_shapes(&shapes, None, clip)
    }

    pub fn render_active_layer(&self, clip: Rect) -> Option<(Rect, Pixmap)> {
        let idx = self.active_index()?;
        let shape = self.elements.get(idx)?.as_shape()?;
        let underline = self.text.as_ref().and_then(|e| Some((0, e.marked.clone()?)));
        self.render_shapes(&[(idx, shape)], underline, clip)
    }

    fn render_shapes(
        &self,
        shapes: &[(usize, &Shape)],
        underline: Option<render::Underline>,
        clip: Rect,
    ) -> Option<(Rect, Pixmap)> {
        if shapes.is_empty() {
            return None;
        }
        let m = self.metrics();
        let bounds = shapes.iter().map(|(_, s)| s.bounds(m)).reduce(|a, b| a.union(&b))?;
        let canvas = self.active_canvas_rect();
        let region = bounds.inflate(1.0).round_out().intersect(&canvas)?.intersect(&clip.round_out())?;

        match &self.workspace {
            Workspace::Capture { doc, .. } => {
                let plain: Vec<&Shape> = shapes.iter().map(|(_, s)| *s).collect();
                let pixmap = render::render_layer(doc, region, &plain, m, underline)?;
                Some((region, pixmap))
            }
            Workspace::Canvas { width, height, background, backdrop } => {
                let pixmap = render::render_canvas_layer(
                    (*width, *height),
                    *background,
                    &self.elements,
                    backdrop,
                    region,
                    shapes,
                    m,
                    underline,
                )?;
                Some((region, pixmap))
            }
        }
    }

    /// Exports the final composited image (cropped to `selection` in Capture mode,
    /// or the full canvas in Canvas mode).
    pub fn export(&mut self) -> Option<Pixmap> {
        self.finish_text();
        match &self.workspace {
            Workspace::Capture { doc, selection, snapped, .. } => {
                let shapes: Vec<Shape> = self.elements.iter().filter_map(Element::as_shape).cloned().collect();
                render::export_within(doc, (*selection)?, snapped.as_ref(), &shapes, self.metrics())
            }
            Workspace::Canvas { .. } => {
                let comp = self.to_composition()?;
                render::render_composition(&comp, &self.fonts)
            }
        }
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn prev_word_boundary(text: &str, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    let mut iter = text[..offset].char_indices().rev().peekable();
    if let Some(&(i, '\n')) = iter.peek() {
        return i;
    }
    let mut pos = offset;
    while let Some(&(i, c)) = iter.peek() {
        if c.is_whitespace() && c != '\n' {
            pos = i;
            iter.next();
        } else {
            break;
        }
    }
    if let Some(&(_, '\n')) = iter.peek() {
        return pos;
    }
    if let Some((i, first)) = iter.next() {
        pos = i;
        let word = is_word_char(first);
        while let Some(&(j, c)) = iter.peek() {
            if c.is_whitespace() || is_word_char(c) != word {
                break;
            }
            pos = j;
            iter.next();
        }
    }
    pos
}

fn next_word_boundary(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    let mut iter = text[offset..].char_indices().peekable();
    if let Some(&(_, '\n')) = iter.peek() {
        return offset + 1;
    }
    let mut rel = 0;
    while let Some(&(i, c)) = iter.peek() {
        if c.is_whitespace() && c != '\n' {
            rel = i + c.len_utf8();
            iter.next();
        } else {
            break;
        }
    }
    if let Some(&(_, '\n')) = iter.peek() {
        return offset + rel;
    }
    if let Some((i, first)) = iter.next() {
        rel = i + first.len_utf8();
        let word = is_word_char(first);
        while let Some(&(j, c)) = iter.peek() {
            if c.is_whitespace() || is_word_char(c) != word {
                break;
            }
            rel = j + c.len_utf8();
            iter.next();
        }
    }
    offset + rel
}

fn word_range_at(text: &str, offset: usize) -> Range<usize> {
    if text.is_empty() {
        return 0..0;
    }
    let cls = |c: char| -> u8 {
        if c == '\n' {
            0
        } else if c.is_whitespace() {
            1
        } else if is_word_char(c) {
            2
        } else {
            3
        }
    };
    let anchor = if offset < text.len() && !text[offset..].starts_with('\n') {
        offset
    } else if offset > 0 {
        text[..offset].char_indices().next_back().map_or(0, |(i, _)| i)
    } else {
        0
    };
    let Some(ch) = text[anchor..].chars().next() else {
        return anchor..anchor;
    };
    let target = cls(ch);
    if target == 0 {
        return anchor..anchor;
    }
    let mut start = anchor;
    for (i, c) in text[..anchor].char_indices().rev() {
        if cls(c) != target {
            break;
        }
        start = i;
    }
    let mut end = anchor + ch.len_utf8();
    for (i, c) in text[end..].char_indices() {
        if cls(c) != target {
            break;
        }
        end = anchor + ch.len_utf8() + i + c.len_utf8();
    }
    start..end
}

fn constrained(start: Point, p: Point, constrain: bool) -> Point {
    if !constrain {
        return p;
    }
    let d = p - start;
    let side = d.x.abs().max(d.y.abs());
    Point::new(start.x + side.copysign(d.x), start.y + side.copysign(d.y))
}

fn snapped(start: Point, p: Point, constrain: bool) -> Point {
    let d = p - start;
    if !constrain || d.length() == 0.0 {
        return p;
    }
    const D: f32 = std::f32::consts::FRAC_1_SQRT_2;
    const DIRECTIONS: [(f32, f32); 8] = [(1.0, 0.0), (D, D), (0.0, 1.0), (-D, D), (-1.0, 0.0), (-D, -D), (0.0, -1.0), (D, -D)];
    let octant = (d.y.atan2(d.x) / std::f32::consts::FRAC_PI_4).round().rem_euclid(8.0) as usize;
    let (dx, dy) = DIRECTIONS[octant];
    let along = d.x * dx + d.y * dy;
    Point::new(start.x + along * dx, start.y + along * dy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        let doc = Document::from_rgba(400, 300, vec![255; 400 * 300 * 4]).unwrap();
        let windows = vec![Rect::new(50.0, 50.0, 100.0, 100.0), Rect::new(0.0, 0.0, 300.0, 200.0)];
        Editor::new(doc, Arc::new(Fonts::empty()), windows, 1.0)
    }

    fn shapes_of(e: &Editor) -> Vec<Shape> {
        e.elements.iter().filter_map(Element::as_shape).cloned().collect()
    }

    fn click(e: &mut Editor, x: f32, y: f32) {
        e.pointer_down(Point::new(x, y));
        e.pointer_up(Point::new(x, y));
    }

    fn drag(e: &mut Editor, from: (f32, f32), to: (f32, f32)) {
        e.pointer_down(Point::new(from.0, from.1));
        e.pointer_move(Point::new((from.0 + to.0) / 2.0, (from.1 + to.1) / 2.0), false);
        e.pointer_move(Point::new(to.0, to.1), false);
        e.pointer_up(Point::new(to.0, to.1));
    }

    #[test]
    fn hover_follows_topmost_window_and_click_selects_it() {
        let mut e = editor();
        e.pointer_move(Point::new(60.0, 60.0), false);
        assert_eq!(e.active_rect(), Some(Rect::new(50.0, 50.0, 100.0, 100.0)));
        e.pointer_move(Point::new(250.0, 60.0), false);
        assert_eq!(e.active_rect(), Some(Rect::new(0.0, 0.0, 300.0, 200.0)));
        e.pointer_move(Point::new(350.0, 250.0), false);
        assert_eq!(e.active_rect(), Some(e.screen()));
        click(&mut e, 60.0, 60.0);
        assert_eq!(e.selection(), Some(Rect::new(50.0, 50.0, 100.0, 100.0)));
    }

    #[test]
    fn capture_save_snapshot_keeps_original_effect_sampling_and_rounded_corners() {
        let mut pixmap = Pixmap::new(200, 150).unwrap();
        for (i, px) in pixmap.data_mut().as_chunks_mut::<4>().0.iter_mut().enumerate() {
            px.copy_from_slice(&[(i % 251) as u8, (i % 139) as u8, (i % 97) as u8, 255]);
        }
        let bounds = Rect::new(33.0, 17.0, 120.0, 90.0);
        let outline = WindowOutline::new(bounds, 11.0);
        let fonts = Arc::new(Fonts::empty());
        let mut editor = Editor::new(Document::from_pixmap(pixmap), fonts.clone(), vec![outline], 1.0);
        click(&mut editor, 70.0, 50.0);
        editor.set_redaction(|r| r.effect = RedactEffect::Mosaic);
        editor.add_redactions(&[Rect::new(46.0, 35.0, 63.0, 35.0)]);
        let expected_png = editor.export().unwrap();
        let png = editor.save_snapshot().unwrap();
        let snapshot = editor.save_snapshot().unwrap();
        assert!(editor.undo());
        let png = png.into_pixmap(&fonts).unwrap();
        assert_eq!(png.data(), expected_png.data());
        assert_eq!(png.pixel(0, 0).unwrap().alpha(), 0);
        let editable = snapshot.into_composition().unwrap();
        assert_eq!(editable.elements.len(), 2);
        assert_eq!(editable.elements[0].as_image().unwrap().asset.pixmap().pixel(0, 0).unwrap().alpha(), 0);
    }

    #[test]
    fn drag_creates_manual_selection() {
        let mut e = editor();
        drag(&mut e, (10.0, 20.0), (110.0, 80.0));
        assert_eq!(e.selection(), Some(Rect::new(10.0, 20.0, 100.0, 60.0)));
    }

    #[test]
    fn clicking_a_rounded_window_exports_only_its_outline_until_the_selection_changes() {
        let doc = Document::from_rgba(400, 300, vec![255; 400 * 300 * 4]).unwrap();
        let windows = [
            WindowOutline::new(Rect::new(50.0, 40.0, 200.0, 120.0), 10.0),
            // Hangs off the screen's bottom-right: only its top-left corner is visible.
            WindowOutline::new(Rect::new(300.0, 200.0, 200.0, 200.0), 10.0),
        ];
        let corner_alpha = |e: &mut Editor| {
            let image = e.export().unwrap();
            let (w, h) = (image.width(), image.height());
            [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)].map(|(x, y)| image.pixel(x, y).unwrap().alpha())
        };
        let mut e = Editor::new(doc, Arc::new(Fonts::empty()), windows, 1.0);

        // A click snaps to the window: exactly its bounds, corners transparent,
        // in both the image and an editable slice (Add to editor, .bs).
        click(&mut e, 100.0, 100.0);
        assert_eq!(e.selection(), Some(Rect::new(50.0, 40.0, 200.0, 120.0)));
        assert_eq!(corner_alpha(&mut e), [0; 4]);
        assert_eq!(e.export().unwrap().pixel(100, 60).unwrap().alpha(), 255);
        let (raw, _, _) = e.extract_slice().unwrap();
        assert_eq!(raw.pixel(0, 0).unwrap().alpha(), 0);
        let comp = e.to_composition().unwrap();
        let mut reopened = Editor::from_composition(comp, e.fonts.clone());
        assert_eq!(corner_alpha(&mut reopened), [0; 4]);
        // A click inside that doesn't move it keeps the snap.
        click(&mut e, 120.0, 90.0);
        assert_eq!(corner_alpha(&mut e), [0; 4]);

        // Moving the selection makes it a plain rectangle again.
        drag(&mut e, (120.0, 90.0), (130.0, 95.0));
        assert_eq!(corner_alpha(&mut e), [255; 4]);
        // So does resizing a fresh snap, even back to the same size.
        e.right_click_at(Point::new(10.0, 10.0));
        click(&mut e, 100.0, 100.0);
        assert_eq!(corner_alpha(&mut e), [0; 4]);
        drag(&mut e, (250.0, 160.0), (260.0, 170.0));
        assert_eq!(e.selection(), Some(Rect::new(50.0, 40.0, 210.0, 130.0)));
        assert_eq!(corner_alpha(&mut e), [255; 4]);

        // Dragging out a region never clips, even over a rounded window.
        e.right_click_at(Point::new(10.0, 10.0));
        drag(&mut e, (50.0, 40.0), (250.0, 160.0));
        assert_eq!(corner_alpha(&mut e), [255; 4]);

        // A window cut off by the screen edge only rounds the corner on screen.
        e.right_click_at(Point::new(10.0, 10.0));
        click(&mut e, 350.0, 250.0);
        assert_eq!(e.selection(), Some(Rect::new(300.0, 200.0, 100.0, 100.0)));
        assert_eq!(corner_alpha(&mut e), [0, 255, 255, 255]);

        // The desktop (no window under the pointer) is never clipped.
        e.right_click_at(Point::new(10.0, 10.0));
        click(&mut e, 10.0, 290.0);
        assert_eq!(e.selection(), Some(e.screen()));
        assert_eq!(corner_alpha(&mut e), [255; 4]);
    }

    #[test]
    fn handles_resize_selection_and_body_moves_it() {
        let mut e = editor();
        drag(&mut e, (10.0, 20.0), (110.0, 80.0));
        drag(&mut e, (110.0, 80.0), (210.0, 130.0));
        assert_eq!(e.selection(), Some(Rect::new(10.0, 20.0, 200.0, 110.0)));
        drag(&mut e, (100.0, 50.0), (120.0, 60.0));
        assert_eq!(e.selection(), Some(Rect::new(30.0, 30.0, 200.0, 110.0)));
    }

    #[test]
    fn shapes_can_be_moved_deleted_and_undone() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Rect);
        drag(&mut e, (50.0, 50.0), (150.0, 120.0));
        assert_eq!(shapes_of(&e).len(), 1);
        drag(&mut e, (50.0, 70.0), (60.0, 80.0));
        assert_eq!(shapes_of(&e)[0].geometry, Geometry::Rect(Rect::new(60.0, 60.0, 100.0, 70.0)));
        drag(&mut e, (160.0, 130.0), (170.0, 140.0));
        assert_eq!(shapes_of(&e)[0].geometry, Geometry::Rect(Rect::new(60.0, 60.0, 110.0, 80.0)));
        assert!(e.delete_selected());
        assert!(shapes_of(&e).is_empty());
        assert!(e.undo());
        assert!(e.undo());
        assert!(e.undo());
        assert_eq!(shapes_of(&e)[0].geometry, Geometry::Rect(Rect::new(50.0, 50.0, 100.0, 70.0)));
    }

    #[test]
    fn tiny_shapes_are_discarded_without_history() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Arrow);
        click(&mut e, 100.0, 100.0);
        assert!(shapes_of(&e).is_empty());
        assert!(!e.can_undo());
    }

    #[test]
    fn text_editing_with_ime_composition() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Text);
        click(&mut e, 100.0, 100.0);
        assert!(e.is_editing_text());
        e.text_replace(None, "hi", false, None);
        e.text_replace(None, "ni", true, Some(2));
        assert_eq!(e.text_marked(), Some(2..4));
        e.text_replace(None, "你", false, None);
        assert_eq!(e.text_content(), Some("hi你"));
        e.text_key(TextKey::Backspace);
        e.text_key(TextKey::Left);
        e.text_key(TextKey::Newline);
        assert_eq!(e.text_content(), Some("h\ni"));
        click(&mut e, 300.0, 250.0);
        assert!(!e.is_editing_text());
        assert_eq!(shapes_of(&e).len(), 1);
        assert!(e.undo());
        assert!(shapes_of(&e).is_empty());
    }

    #[test]
    fn text_selection_shift_arrows_delete_and_replace() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Text);
        click(&mut e, 100.0, 100.0);
        e.text_replace(None, "hello world\nsecond line", false, None);
        assert_eq!(e.text_caret(), Some("hello world\nsecond line".len()));
        assert_eq!(e.text_selected(), None);

        // Shift+Left selects backward and Delete removes the selection.
        for _ in 0..4 {
            e.text_key(TextKey::SelectLeft);
        }
        assert_eq!(e.text_selected(), Some("line"));
        assert!(e.text_selection_reversed());
        e.text_key(TextKey::Delete);
        assert_eq!(e.text_content(), Some("hello world\nsecond "));
        assert_eq!(e.text_selected(), None);

        // Shift+Home selects to line start, typing replaces the selection.
        e.text_key(TextKey::SelectHome);
        assert_eq!(e.text_selected(), Some("second "));
        e.text_replace(None, "next", false, None);
        assert_eq!(e.text_content(), Some("hello world\nnext"));
        assert_eq!(e.text_selected(), None);

        // Shift+Up selects onto the previous line, Backspace deletes across the newline.
        e.text_key(TextKey::SelectUp);
        assert!(e.text_selected().is_some_and(|s| s.contains('\n')));
        e.text_key(TextKey::Backspace);
        assert_eq!(e.text_content(), Some("hell"));

        // Plain Left / Right collapse a selection to its start / end.
        e.text_key(TextKey::SelectAll);
        assert_eq!(e.text_selected(), Some("hell"));
        assert!(!e.text_selection_reversed());
        e.text_key(TextKey::Left);
        assert_eq!(e.text_selected(), None);
        assert_eq!(e.text_caret(), Some(0));

        e.text_key(TextKey::SelectRight);
        e.text_key(TextKey::SelectRight);
        assert_eq!(e.text_selected(), Some("he"));
        e.text_key(TextKey::Right);
        assert_eq!(e.text_selected(), None);
        assert_eq!(e.text_caret(), Some(2));

        // Paste and Newline replace active selection; multi-byte Unicode stays on boundaries.
        e.text_key(TextKey::SelectEnd);
        assert_eq!(e.text_selected(), Some("ll"));
        assert!(e.paste_text("密😀".into(), Point::new(0.0, 0.0)));
        assert_eq!(e.text_content(), Some("he密😀"));
        e.text_key(TextKey::SelectLeft);
        assert_eq!(e.text_selected(), Some("😀"));
        e.text_key(TextKey::Newline);
        assert_eq!(e.text_content(), Some("he密\n"));

        // Word selection and word deletion.
        e.text_key(TextKey::SelectAll);
        e.text_replace(None, "alpha beta gamma", false, None);
        e.text_key(TextKey::SelectWordLeft);
        assert_eq!(e.text_selected(), Some("gamma"));
        e.text_replace(None, "delta", false, None);
        assert_eq!(e.text_content(), Some("alpha beta delta"));
        e.text_key(TextKey::DeleteWordLeft);
        assert_eq!(e.text_content(), Some("alpha beta "));
    }

    #[test]
    fn empty_text_is_dropped() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Text);
        click(&mut e, 100.0, 100.0);
        e.finish_text();
        assert!(shapes_of(&e).is_empty());
        assert!(!e.can_undo());
    }

    #[test]
    fn each_tool_starts_small_and_accepts_other_sizes() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        for tool in Tool::ALL {
            e.toggle_tool(tool);
            assert_eq!(e.current_style().unwrap().1.size, SizeLevel::Small, "{tool:?}");
            for size in [SizeLevel::Medium, SizeLevel::Large] {
                e.set_size(size);
                assert_eq!(e.current_style().unwrap().1.size, size, "{tool:?}");
            }
        }
    }

    #[test]
    fn style_changes_apply_to_selected_shape() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Ellipse);
        drag(&mut e, (50.0, 50.0), (150.0, 120.0));
        e.set_color(crate::style::PALETTE[2]);
        assert_eq!(shapes_of(&e)[0].style.color, crate::style::PALETTE[2]);
        assert!(e.undo());
        assert_eq!(shapes_of(&e)[0].style.color, crate::style::PALETTE[0]);
    }

    #[test]
    fn chosen_color_carries_across_tools_within_a_single_session() {
        let green = crate::style::PALETTE[2];
        let blue = crate::style::PALETTE[3];
        let yellow = crate::style::PALETTE[1];
        let red = crate::style::PALETTE[0];

        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        // Change a drawn circle's color to green -> switching to Text (and other ink tools) keeps green.
        e.toggle_tool(Tool::Ellipse);
        drag(&mut e, (50.0, 50.0), (150.0, 120.0));
        e.set_color(green);
        e.toggle_tool(Tool::Text);
        assert_eq!(e.current_style().unwrap().1.color, green);
        e.toggle_tool(Tool::Arrow);
        assert_eq!(e.current_style().unwrap().1.color, green);

        // Changing color before drawing also carries over to other ink tools, while Highlighter keeps yellow.
        e.set_color(blue);
        e.toggle_tool(Tool::Rect);
        assert_eq!(e.current_style().unwrap().1.color, blue);
        e.toggle_tool(Tool::Highlighter);
        assert_eq!(e.current_style().unwrap().1.color, yellow);

        // A new session starts fresh with default colors.
        let mut fresh = editor();
        drag(&mut fresh, (0.0, 0.0), (400.0, 300.0));
        fresh.toggle_tool(Tool::Text);
        assert_eq!(fresh.current_style().unwrap().1.color, red);
    }

    #[test]
    fn dragging_a_shape_only_rerenders_the_active_layer() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Rect);
        drag(&mut e, (20.0, 20.0), (60.0, 60.0));
        e.toggle_tool(Tool::Pen);
        e.pointer_down(Point::new(200.0, 200.0));
        let (static_rev, active_rev) = (e.static_layer_revision(), e.active_layer_revision());
        e.pointer_move(Point::new(220.0, 210.0), false);
        e.pointer_move(Point::new(240.0, 230.0), false);
        assert_eq!(e.static_layer_revision(), static_rev);
        assert!(e.active_layer_revision() > active_rev);
        let (active, _) = e.render_active_layer(e.screen()).unwrap();
        assert!(active.x > 150.0 && active.w < 100.0);
        let (fixed, _) = e.render_static_layer(e.screen()).unwrap();
        assert!(fixed.right() < 100.0);
        e.pointer_up(Point::new(240.0, 230.0));
        assert!(e.render_active_layer(e.screen()).is_none());
        assert!(e.render_static_layer(e.screen()).unwrap().0.right() > 200.0);
    }

    #[test]
    fn moving_the_selection_keeps_both_layers() {
        let mut e = editor();
        drag(&mut e, (50.0, 50.0), (250.0, 200.0));
        e.toggle_tool(Tool::Rect);
        drag(&mut e, (80.0, 80.0), (120.0, 120.0));
        e.toggle_tool(Tool::Rect);
        let revs = (e.static_layer_revision(), e.active_layer_revision());
        drag(&mut e, (150.0, 150.0), (170.0, 160.0));
        assert_eq!(e.selection(), Some(Rect::new(70.0, 60.0, 200.0, 150.0)));
        assert_eq!((e.static_layer_revision(), e.active_layer_revision()), revs);
    }

    fn document_editor() -> Editor {
        let doc = Document::from_rgba(400, 3000, vec![255; 400 * 3000 * 4]).unwrap();
        Editor::for_document(doc, Arc::new(Fonts::empty()), 1.0).unwrap()
    }

    #[test]
    fn dragging_invalidates_upper_effects_when_entering_and_leaving_their_sample_area() {
        let mut e = document_editor();
        e.toggle_tool(Tool::Redact);
        e.set_redaction(|r| (r.area, r.effect) = (RedactArea::Rect, RedactEffect::Gray));
        drag(&mut e, (20.0, 100.0), (40.0, 120.0));
        e.toggle_tool(Tool::Redact);
        e.set_redaction(|r| (r.effect, r.strength) = (RedactEffect::Mosaic, 1.0));
        assert_eq!(e.add_redactions(&[Rect::new(189.0, 100.0, 8.0, 8.0)]), 1);
        e.pointer_down(Point::new(30.0, 110.0));
        e.pointer_move(Point::new(50.0, 110.0), false); // split off the moving shape
        let rev = e.static_layer_revision();
        e.pointer_move(Point::new(70.0, 110.0), false);
        assert_eq!(e.static_layer_revision(), rev);
        // The mosaic reads the entire [160, 200) cell. Neither rectangle
        // touches the other, but the moving shape changes its average.
        e.pointer_move(Point::new(165.0, 110.0), false);
        assert!(e.static_layer_revision() > rev);
        let rev = e.static_layer_revision();
        e.pointer_move(Point::new(70.0, 110.0), false);
        assert!(e.static_layer_revision() > rev);
    }

    #[test]
    fn oversized_appends_and_resizes_keep_the_original_document_and_undo_history() {
        let strip = || Pixmap::new(1, 65_530).unwrap();
        let comp = Composition::from_slice(strip(), 1.0, Vec::new()).unwrap();
        let mut editor = Editor::from_composition(comp.clone(), Arc::new(Fonts::empty()));
        assert_eq!(editor.append_slice(strip(), 1.0, vec![]), None);
        assert_eq!(editor.to_composition().unwrap(), comp);
        assert!(!editor.can_undo());

        let mut editor = document_editor();
        let before = editor.to_composition().unwrap();
        let start = Point::new(400.0 + CANVAS_HANDLE_OFFSET, 1500.0);
        editor.pointer_down(start);
        editor.pointer_move(Point::new(70_000.0, start.y), false);
        assert_eq!(editor.active_canvas_rect(), before.bounds());
        editor.pointer_up(Point::new(70_000.0, start.y));
        assert_eq!(editor.to_composition().unwrap(), before);
        assert!(!editor.can_undo());

        let bytes = crate::format::encode_bs(&comp).unwrap();
        assert_eq!(crate::format::decode_bs(&bytes).unwrap(), comp);
        let mut invalid = comp;
        invalid.height += 24;
        assert!(crate::format::encode_bs(&invalid).is_err());
        assert!(Composition::new_empty(32_768, 32_768, 1.0).is_err());
    }

    #[test]
    fn canvas_editor_moves_resizes_images_and_resizes_canvas_with_undo() {
        let mut e = document_editor();
        assert_eq!(e.screen(), Rect::new(0.0, 0.0, 400.0, 3000.0));
        assert!(e.toolbar_visible());

        // Dragging the interior moves the placed image.
        drag(&mut e, (200.0, 200.0), (250.0, 260.0));
        assert_eq!(e.elements()[0].as_image().unwrap().bounds, Rect::new(50.0, 60.0, 400.0, 3000.0));
        assert!(e.undo());
        assert_eq!(e.elements()[0].as_image().unwrap().bounds, Rect::new(0.0, 0.0, 400.0, 3000.0));

        // Dragging the left outer canvas edge (-6, 1500) outward by -100 expands the
        // canvas to 500 wide and shifts the image to x = +100 so it stays anchored.
        drag(&mut e, (-6.0, 1500.0), (-106.0, 1500.0));
        assert_eq!(e.screen(), Rect::new(0.0, 0.0, 500.0, 3000.0));
        assert_eq!(e.elements()[0].as_image().unwrap().bounds.x, 100.0);
        assert_eq!(e.take_canvas_origin_shift(), Point::new(-100.0, 0.0));

        // Undo restores both canvas size and image position.
        assert!(e.undo());
        assert_eq!(e.screen(), Rect::new(0.0, 0.0, 400.0, 3000.0));
        assert_eq!(e.elements()[0].as_image().unwrap().bounds.x, 0.0);
    }

    #[test]
    fn pasted_annotations_keep_their_style_and_can_be_undone_individually() {
        let original = Shape {
            geometry: Geometry::Rect(Rect::new(10.0, 20.0, 30.0, 40.0)),
            style: Style { color: crate::PALETTE[2], size: SizeLevel::Large },
        };
        let comp = Composition::new_empty(300, 300, 2.0).unwrap();
        let mut editor = Editor::from_composition(comp, Arc::new(Fonts::empty()));
        let bytes = crate::format::encode_clipboard_element(&Element::Shape(original.clone()), 1.0).unwrap();
        let (element, scale) = crate::format::decode_clipboard_element(&bytes).unwrap();
        assert!(editor.paste_element(element, scale));
        assert_eq!(editor.selected_index(), Some(0));
        let pasted = original.scaled(2.0).translated(Point::new(32.0, 32.0));
        let (_, editable) = editor.copy_canvas_item().unwrap();
        assert_eq!(editable, Some(Element::Shape(pasted)));
        assert!(editor.undo());
        assert!(editor.elements().is_empty());
    }

    #[test]
    fn copied_resized_image_keeps_source_pixels_but_a_cropped_image_only_copies_visible_pixels() {
        let source = crate::pixels::pixmap_from_rgba(80, 40, [30, 20, 10, 255].repeat(80 * 40)).unwrap();
        let asset = ImageAsset::new(source);
        let bounds = Rect::new(10.0, 20.0, 40.0, 20.0);
        let original = PlacedImage::new(asset.clone(), bounds);
        let mut comp = Composition::new_empty(100, 80, 1.0).unwrap();
        comp.elements.push(Element::Image(original));
        let mut editor = Editor::from_composition(comp, Arc::new(Fonts::empty()));
        click(&mut editor, 30.0, 30.0);
        let (raster, editable) = editor.copy_canvas_item().unwrap();
        assert_eq!((raster.width(), raster.height()), (40, 20));
        let bytes = crate::format::encode_clipboard_element(&editable.unwrap(), 1.0).unwrap();
        let (item, scale) = crate::format::decode_clipboard_element(&bytes).unwrap();
        let restored = item.as_image().unwrap();
        assert_eq!(restored.bounds, bounds);
        assert_eq!(restored.asset.pixmap(), asset.pixmap());
        assert_ne!(restored.asset.id, asset.id);
        assert!(editor.paste_element(item, scale));
        assert_eq!(editor.selected_index(), Some(1));
        assert!(editor.undo());
        drag(&mut editor, (30.0, 30.0), (10.0, 30.0));
        let (cropped, editable) = editor.copy_canvas_item().unwrap();
        assert_eq!(cropped.width(), 30);
        assert!(editable.is_none());
    }

    #[test]
    fn pasting_the_first_bitmap_sizes_the_canvas_and_undo_restores_it() {
        let mut editor = Editor::from_composition(Composition::new_empty(500, 300, 2.0).unwrap(), Arc::new(Fonts::empty()));
        assert!(editor.paste_image(Pixmap::new(90, 55).unwrap(), Point::new(200.0, 100.0)));
        assert_eq!(editor.screen(), Rect::new(0.0, 0.0, 90.0, 55.0));
        assert_eq!(editor.elements()[0].as_image().unwrap().bounds, editor.screen());
        assert_eq!(editor.selected_index(), Some(0));
        assert!(editor.undo());
        assert!(editor.elements().is_empty());
        assert_eq!(editor.screen(), Rect::new(0.0, 0.0, 500.0, 300.0));
    }

    #[test]
    fn copying_an_item_covered_by_a_redaction_only_copies_visible_pixels() {
        let mask = Shape {
            geometry: Geometry::Redact {
                area: RedactShape::Rect(Rect::new(20.0, 20.0, 30.0, 30.0)),
                effect: RedactEffect::Gray,
                strength: 0.5,
            },
            style: Style::default(),
        };
        let image = crate::pixels::pixmap_from_rgba(80, 80, [255, 0, 0, 255].repeat(80 * 80)).unwrap();
        let comp = Composition::from_slice(image, 1.0, vec![mask]).unwrap();
        let mut editor = Editor::from_composition(comp, Arc::new(Fonts::empty()));
        click(&mut editor, 10.0, 10.0);
        assert_eq!(editor.selected_index(), Some(0));
        let (pixels, editable) = editor.copy_canvas_item().unwrap();
        assert!(editable.is_none());
        assert_eq!(pixels.pixel(30, 30).unwrap().red(), crate::REDACT_GRAY.r);
        assert_eq!(pixels, editor.export().unwrap());
    }

    #[test]
    fn canvas_editor_appends_slices_rearranges_and_exports_unflattened() {
        let mut e = editor();
        drag(&mut e, (50.0, 40.0), (250.0, 190.0));
        e.toggle_tool(Tool::Rect);
        drag(&mut e, (70.0, 60.0), (130.0, 110.0));

        // Capture -> Composition keeps the raw cropped image AND the rectangle separately.
        let comp = e.to_composition().unwrap();
        assert_eq!((comp.width, comp.height), (200, 150));
        assert_eq!(comp.elements.len(), 2);

        let mut ce = Editor::from_composition(comp, Arc::new(Fonts::empty()));
        let second = Pixmap::new(120, 80).unwrap();
        let idx = ce.append_slice(second, 1.0, Vec::new()).unwrap();
        assert_eq!(idx, 2);
        assert_eq!(ce.selected_index(), Some(2));
        assert!(ce.reorder_selected(LayerOrder::ToBack));
        assert_eq!(ce.selected_index(), Some(0));

        let out = ce.export().unwrap();
        assert_eq!((out.width(), out.height()), (200, 150));
        // Red rectangle carried over from capture is still rendered on export.
        let px = out.pixel(20, 40).unwrap();
        assert_eq!((px.red(), px.green(), px.blue()), (0xF5, 0x22, 0x2D));
    }

    #[test]
    fn document_annotations_export_with_the_whole_image() {
        let mut e = document_editor();
        e.toggle_tool(Tool::Rect);
        drag(&mut e, (50.0, 2500.0), (150.0, 2600.0));
        drag(&mut e, (60.0, 2510.0), (70.0, 2520.0));
        assert_eq!(shapes_of(&e).len(), 2);
        let out = e.export().unwrap();
        assert_eq!((out.width(), out.height()), (400, 3000));
        let px = out.pixel(50, 2550).unwrap();
        assert_eq!((px.red(), px.green(), px.blue()), (0xF5, 0x22, 0x2D));
    }

    #[test]
    fn ui_scale_sets_handle_hit_radius_on_screen() {
        let mut e = editor();
        drag(&mut e, (10.0, 20.0), (110.0, 80.0));
        e.pointer_move(Point::new(122.0, 80.0), false);
        assert_eq!(e.cursor(), CursorKind::Default);
        e.set_ui_scale(2.0);
        e.pointer_move(Point::new(122.0, 80.5), false);
        assert_eq!(e.cursor(), CursorKind::ResizeNwSe);
    }

    #[test]
    fn layers_are_clipped() {
        let mut e = document_editor();
        e.toggle_tool(Tool::Pen);
        drag(&mut e, (100.0, 100.0), (100.0, 2900.0));
        let (region, pixmap) = e.render_static_layer(Rect::new(0.0, 1000.0, 400.0, 500.0)).unwrap();
        assert!(region.y >= 1000.0 && region.bottom() <= 1500.0);
        assert_eq!(pixmap.height() as f32, region.h);
    }

    #[test]
    fn redact_areas_follow_the_settings_and_slider_drags_undo_once() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Redact);
        assert_eq!(e.current_redaction(), Some(Redaction::default()));
        e.set_redaction(|r| (r.area, r.effect) = (RedactArea::Rect, RedactEffect::Blur));
        drag(&mut e, (50.0, 50.0), (150.0, 120.0));
        let shapes = shapes_of(&e);
        let Geometry::Redact { area: RedactShape::Rect(r), effect, .. } = &shapes[0].geometry else { panic!() };
        assert_eq!((*r, *effect), (Rect::new(50.0, 50.0, 100.0, 70.0), RedactEffect::Blur));
        let undo_depth = e.history.len();
        e.begin_continuous_edit();
        e.set_redaction(|r| r.strength = 0.0);
        let static_rev = e.static_layer_revision();
        for i in 1..10 {
            e.set_redaction(|r| r.strength = i as f32 / 10.0);
            assert_eq!(e.static_layer_revision(), static_rev);
        }
        e.end_continuous_edit();
        assert!(e.static_layer_revision() > static_rev);
        assert_eq!(e.history.len(), undo_depth + 1);
        assert_eq!(e.current_redaction().unwrap().strength, 0.9);
        assert!(e.undo());
        assert_eq!(e.current_redaction(), Some(Redaction { area: RedactArea::Rect, effect: RedactEffect::Blur, strength: 0.9 }));
        let shapes = shapes_of(&e);
        let Geometry::Redact { strength, .. } = &shapes[0].geometry else { panic!() };
        assert_eq!(*strength, Redaction::default().strength);
    }

    #[test]
    fn switching_area_with_a_redaction_selected_leaves_it_alone() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Redact);
        e.set_redaction(|r| r.area = RedactArea::Rect);
        drag(&mut e, (50.0, 50.0), (150.0, 120.0));
        assert!(e.selected.is_some());
        let before = shapes_of(&e);
        e.set_redaction(|r| r.area = RedactArea::Brush);
        assert_eq!((e.selected, e.tool), (None, Some(Tool::Redact)));
        assert_eq!(e.current_redaction().unwrap().area, RedactArea::Brush);
        assert_eq!(shapes_of(&e), before);
    }

    fn counters(e: &Editor) -> Vec<(u32, CounterFormat)> {
        shapes_of(e)
            .into_iter()
            .filter_map(|s| match s.geometry {
                Geometry::Counter { number, format, .. } => Some((number, format)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn counters_number_in_order_and_close_gaps() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Counter);
        assert_eq!(e.current_counter_format(), Some(CounterFormat::Decimal));
        for x in [50.0, 120.0, 190.0] {
            click(&mut e, x, 100.0);
        }
        let numbers = |e: &Editor| counters(e).into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(numbers(&e), [1, 2, 3]);
        drag(&mut e, (120.0, 100.0), (120.0, 200.0));
        assert_eq!(numbers(&e), [1, 2, 3]);
        assert!(matches!(shapes_of(&e)[1].geometry, Geometry::Counter { center, .. } if center == Point::new(120.0, 200.0)));
        assert!(e.delete_selected());
        assert_eq!(numbers(&e), [1, 2]);
        click(&mut e, 260.0, 100.0);
        assert_eq!(numbers(&e), [1, 2, 3]);
        e.undo();
        e.undo();
        assert_eq!(numbers(&e), [1, 2, 3]);
        assert!(matches!(shapes_of(&e)[1].geometry, Geometry::Counter { center, .. } if center == Point::new(120.0, 200.0)));
    }

    #[test]
    fn counter_format_applies_to_the_whole_sequence() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Counter);
        click(&mut e, 50.0, 100.0);
        click(&mut e, 120.0, 100.0);
        e.set_counter_format(CounterFormat::Roman);
        click(&mut e, 190.0, 100.0);
        assert!(counters(&e).iter().all(|(_, f)| *f == CounterFormat::Roman));
        e.undo();
        e.undo();
        assert!(counters(&e).iter().all(|(_, f)| *f == CounterFormat::Decimal));
        assert_eq!(e.current_counter_format(), Some(CounterFormat::Decimal));
        e.toggle_tool(Tool::Pen);
        assert_eq!(e.current_counter_format(), None);
    }

    #[test]
    fn highlighter_draws_yellow_strokes_straight_with_shift() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Highlighter);
        assert_eq!(e.current_style().unwrap().1.color, crate::style::PALETTE[1]);
        e.pointer_down(Point::new(50.0, 100.0));
        for x in [80.0, 120.0, 160.0] {
            e.pointer_move(Point::new(x, 100.0 + x / 10.0), true);
        }
        e.pointer_up(Point::new(160.0, 116.0));
        assert_eq!(shapes_of(&e)[0].geometry, Geometry::Highlight(vec![Point::new(50.0, 100.0), Point::new(160.0, 100.0)]));
        e.pointer_move(Point::new(300.0, 200.0), false);
        let ring = e.brush_ring().unwrap();
        assert_eq!((ring.center, ring.half_height), (Point::new(300.0, 200.0), 5.0));
        assert!(ring.half_width < ring.half_height);
    }

    #[test]
    fn shift_locks_lines_to_45_degrees_and_squares_boxes() {
        let start = Point::new(100.0, 100.0);
        let draw = |tool: Tool, end: (f32, f32), shift: bool| {
            let mut e = editor();
            drag(&mut e, (0.0, 0.0), (400.0, 300.0));
            e.toggle_tool(tool);
            e.pointer_down(start);
            e.pointer_move(Point::new(130.0, 110.0), false);
            e.pointer_move(Point::new(end.0, end.1), shift);
            e.pointer_up(Point::new(end.0, end.1));
            shapes_of(&e)[0].geometry.clone()
        };
        let diagonal = Point::new(150.0, 150.0);
        assert_eq!(draw(Tool::Arrow, (148.0, 152.0), true), Geometry::Arrow { from: start, to: diagonal, style: ArrowStyle::Straight });
        assert_eq!(draw(Tool::Arrow, (148.0, 152.0), false), Geometry::Arrow { from: start, to: Point::new(148.0, 152.0), style: ArrowStyle::Straight });
        assert_eq!(draw(Tool::Pen, (148.0, 152.0), true), Geometry::Pen(vec![start, diagonal]));
        assert_eq!(draw(Tool::Pen, (100.0, 40.0), true), Geometry::Pen(vec![start, Point::new(100.0, 40.0)]));
        assert_eq!(draw(Tool::Pen, (20.0, 97.0), true), Geometry::Pen(vec![start, Point::new(20.0, 100.0)]));
        assert_eq!(draw(Tool::Pen, (148.0, 152.0), false), Geometry::Pen(vec![start, Point::new(130.0, 110.0), Point::new(148.0, 152.0)]));
        let Geometry::Redact { area: RedactShape::Brush(pts), .. } = draw(Tool::Redact, (148.0, 152.0), true) else { panic!() };
        assert_eq!(pts, vec![start, diagonal]);
        assert_eq!(draw(Tool::Rect, (160.0, 120.0), true), Geometry::Rect(Rect::new(100.0, 100.0, 60.0, 60.0)));
        assert_eq!(draw(Tool::Ellipse, (40.0, 130.0), true), Geometry::Ellipse(Rect::new(40.0, 100.0, 60.0, 60.0)));

        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Arrow);
        e.pointer_down(start);
        e.pointer_move(Point::new(148.0, 152.0), false);
        let revision = e.revision();
        e.constrain_changed(true);
        assert_eq!(shapes_of(&e)[0].geometry, Geometry::Arrow { from: start, to: diagonal, style: ArrowStyle::Straight });
        assert_ne!(e.revision(), revision);
        e.constrain_changed(false);
        assert_eq!(shapes_of(&e)[0].geometry, Geometry::Arrow { from: start, to: Point::new(148.0, 152.0), style: ArrowStyle::Straight });
    }

    #[test]
    fn arrow_style_applies_to_new_and_selected_arrows_in_one_undo_step() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        assert_eq!(e.current_arrow_style(), None);
        e.toggle_tool(Tool::Arrow);
        assert_eq!(e.current_arrow_style(), Some(ArrowStyle::Straight));
        e.set_arrow_style(ArrowStyle::Elbow);
        assert!(!e.can_undo());
        drag(&mut e, (50.0, 50.0), (250.0, 150.0));
        assert!(matches!(shapes_of(&e)[0].geometry, Geometry::Arrow { style: ArrowStyle::Elbow, .. }));
        e.selected = Some(0);
        e.set_arrow_style(ArrowStyle::Curved);
        assert!(matches!(shapes_of(&e)[0].geometry, Geometry::Arrow { style: ArrowStyle::Curved, .. }));
        e.undo();
        assert!(matches!(shapes_of(&e)[0].geometry, Geometry::Arrow { style: ArrowStyle::Elbow, .. }));
    }

    #[test]
    fn found_redactions_are_one_undo_step_inside_the_selection() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (300.0, 200.0));
        e.toggle_tool(Tool::Redact);
        e.set_redaction(|r| r.effect = RedactEffect::Gray);
        let rects = [Rect::new(10.0, 10.0, 50.0, 12.0), Rect::new(280.0, 190.0, 50.0, 20.0), Rect::new(500.0, 10.0, 5.0, 5.0)];
        assert_eq!(e.add_redactions(&rects), 2);
        assert!(matches!(&shapes_of(&e)[1].geometry, Geometry::Redact { area: RedactShape::Rect(r), effect: RedactEffect::Gray, .. } if *r == Rect::new(280.0, 190.0, 20.0, 10.0)));
        assert!(e.undo());
        assert!(shapes_of(&e).is_empty() && !e.can_undo());
    }

    #[test]
    fn text_style_applies_to_new_and_selected_text() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Text);
        assert_eq!(e.current_text_style(), Some(TextStyle::Standard));
        e.set_text_style(TextStyle::Boxed);
        click(&mut e, 100.0, 100.0);
        e.text_replace(None, "hi", false, None);
        e.finish_text();
        assert!(matches!(shapes_of(&e)[0].geometry, Geometry::Text { style: TextStyle::Boxed, .. }));
        click(&mut e, 101.0, 105.0);
        e.finish_text();
        e.selected = Some(0);
        e.set_text_style(TextStyle::Outlined);
        assert!(matches!(shapes_of(&e)[0].geometry, Geometry::Text { style: TextStyle::Outlined, .. }));
        assert!(e.undo());
        assert!(matches!(shapes_of(&e)[0].geometry, Geometry::Text { style: TextStyle::Boxed, .. }));
        e.toggle_tool(Tool::Rect);
        assert_eq!(e.current_text_style(), None);
    }

    #[test]
    fn annotations_carry_over_to_an_image_of_the_selection() {
        let mut e = editor();
        drag(&mut e, (100.0, 50.0), (300.0, 250.0));
        e.toggle_tool(Tool::Rect);
        drag(&mut e, (120.0, 70.0), (160.0, 90.0));
        e.toggle_tool(Tool::Text);
        click(&mut e, 200.0, 200.0);
        e.text_replace(None, "typing", false, None);
        let shapes = e.shapes_in_selection();
        assert!(!e.is_editing_text());
        assert_eq!(shapes.len(), 2);
        assert_eq!(shapes[0].geometry, Geometry::Rect(Rect::new(20.0, 20.0, 40.0, 20.0)));
        let doc = Document::from_rgba(200, 600, vec![255; 200 * 600 * 4]).unwrap();
        let d = Editor::for_document(doc, Arc::new(Fonts::empty()), 1.0).unwrap().with_shapes(shapes);
        assert!(d.has_shapes() && !d.can_undo());
        assert!(d.render_static_layer(d.screen()).is_some());
    }

    #[test]
    fn delete_during_a_drag_is_ignored() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Rect);
        e.pointer_down(Point::new(50.0, 50.0));
        e.pointer_move(Point::new(90.0, 80.0), false);
        assert!(!e.delete_selected());
        e.pointer_move(Point::new(100.0, 90.0), false);
        e.pointer_up(Point::new(100.0, 90.0));
        assert_eq!(shapes_of(&e).len(), 1);
        assert!(e.delete_selected());
    }

    #[test]
    fn a_press_after_a_lost_release_finishes_the_drag_first() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Rect);
        e.pointer_down(Point::new(260.0, 16.0));
        e.toggle_tool(Tool::Text);
        e.pointer_down(Point::new(300.0, 24.0));
        e.pointer_up(Point::new(290.0, 120.0));
        e.text_replace(None, "x", false, None);
        assert!(e.render_active_layer(e.screen()).is_some());
        assert_eq!(e.text_content(), Some("x"));
    }

    #[test]
    fn random_input_never_panics() {
        let mut seed = 0x9E37_79B9u32;
        let mut next = move |n: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed % n.max(1)
        };
        for run in 0..400 {
            let mut e = if run % 2 == 0 {
                editor()
            } else {
                let comp = Composition::from_slice(Pixmap::new(400, 300).unwrap(), 1.0, Vec::new()).unwrap();
                Editor::from_composition(comp, Arc::new(Fonts::empty()))
            };
            for _ in 0..80 {
                let p = Point::new(next(440) as f32 - 20.0, next(340) as f32 - 20.0);
                match next(26) {
                    0..=3 => e.pointer_down(p),
                    4..=8 => e.pointer_move(p, next(4) == 0),
                    9..=11 => e.pointer_up(p),
                    12 => e.toggle_tool(Tool::ALL[next(Tool::ALL.len() as u32) as usize]),
                    13 => {
                        if next(2) == 0 {
                            e.undo();
                        } else {
                            e.redo();
                        }
                    }
                    14 => {
                        e.delete_selected();
                    }
                    15 => e.text_replace(None, ["a", "密", "😀", "", "\n"][next(5) as usize], next(3) == 0, None),
                    16 => {
                        const KEYS: &[TextKey] = &[
                            TextKey::Backspace,
                            TextKey::Delete,
                            TextKey::DeleteWordLeft,
                            TextKey::DeleteWordRight,
                            TextKey::Left,
                            TextKey::Right,
                            TextKey::Up,
                            TextKey::Down,
                            TextKey::WordLeft,
                            TextKey::WordRight,
                            TextKey::Home,
                            TextKey::End,
                            TextKey::DocStart,
                            TextKey::DocEnd,
                            TextKey::SelectLeft,
                            TextKey::SelectRight,
                            TextKey::SelectUp,
                            TextKey::SelectDown,
                            TextKey::SelectWordLeft,
                            TextKey::SelectWordRight,
                            TextKey::SelectHome,
                            TextKey::SelectEnd,
                            TextKey::SelectDocStart,
                            TextKey::SelectDocEnd,
                            TextKey::SelectAll,
                            TextKey::Newline,
                        ];
                        e.text_key(KEYS[next(KEYS.len() as u32) as usize]);
                    }
                    17 => e.finish_text(),
                    18 => {
                        e.right_click();
                        e.double_click(p);
                    }
                    19 => e.set_size(SizeLevel::ALL[next(3) as usize]),
                    20 => {
                        let (effect, area) = (RedactEffect::ALL[next(3) as usize], RedactArea::ALL[next(2) as usize]);
                        e.set_redaction(|r| (r.effect, r.area) = (effect, area));
                    }
                    21 => {
                        e.set_text_style(TextStyle::ALL[next(7) as usize]);
                        e.set_counter_format(CounterFormat::ALL[next(4) as usize]);
                        e.set_arrow_style(ArrowStyle::ALL[next(3) as usize]);
                    }
                    22 => {
                        e.add_redactions(&[Rect::new(p.x, p.y, 30.0, 12.0)]);
                    }
                    23 => {
                        let order = [LayerOrder::Backward, LayerOrder::Forward, LayerOrder::ToBack, LayerOrder::ToFront][next(4) as usize];
                        e.reorder_selected(order);
                    }
                    24 => {
                        e.append_slice(Pixmap::new(60, 40).unwrap(), 1.0, Vec::new());
                    }
                    _ => {
                        let strength = next(100) as f32 / 100.0;
                        e.begin_continuous_edit();
                        e.set_redaction(|r| r.strength = strength);
                        e.end_continuous_edit();
                    }
                }
                let screen = e.active_canvas_rect();
                for band in e.visual_bands() {
                    if let VisualBand::Shapes { range, active, .. } = band {
                        e.render_band(range, active, screen);
                    }
                }
                e.cursor();
                e.selected_frame();
                e.text_caret_rect();
                e.brush_ring();
            }
            e.export();
            e.to_composition();
        }
    }

    #[test]
    fn brush_ring_shows_only_for_the_redact_brush() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.pointer_move(Point::new(100.0, 100.0), false);
        assert_eq!(e.brush_ring(), None);
        e.toggle_tool(Tool::Redact);
        assert_eq!(e.brush_ring(), Some(BrushRing { center: Point::new(100.0, 100.0), half_width: 6.0, half_height: 6.0, corner: 6.0 }));
        e.set_redaction(|r| r.area = RedactArea::Rect);
        assert_eq!(e.brush_ring(), None);
    }

    #[test]
    fn right_click_returns_to_picking_then_exits() {
        let mut e = editor();
        click(&mut e, 60.0, 60.0);
        assert_eq!(e.right_click(), RightClick::Handled);
        assert!(e.is_picking());
        assert_eq!(e.right_click(), RightClick::Exit);
    }

    #[test]
    fn overlapping_shape_handles_pick_nearest() {
        let mut e = editor();
        click(&mut e, 60.0, 60.0);
        e.toggle_tool(Tool::Arrow);
        drag(&mut e, (80.0, 80.0), (86.0, 80.0));
        drag(&mut e, (86.0, 80.0), (120.0, 80.0));
        let (_, handles) = e.selected_frame().unwrap();
        assert_eq!(handles, vec![Point::new(80.0, 80.0), Point::new(120.0, 80.0)]);
    }

    #[test]
    fn shift_locks_aspect_ratio_on_bottom_and_side_dots_for_canvas_and_images_and_cursors_match() {
        let fonts = Arc::new(Fonts::empty());
        let img = Pixmap::new(200, 100).unwrap();
        let comp = Composition::from_slice(img, 1.0, Vec::new()).unwrap();
        let mut e = Editor::from_composition(comp, fonts);

        // Check all 8 outer canvas dots report their exact resize cursor (including 4 diagonals).
        let outer = e.screen().inflate(CANVAS_HANDLE_OFFSET);
        assert_eq!(
            Handle::ALL.map(|h| {
                e.pointer_move(outer.handle_point(h), false);
                e.cursor()
            }),
            [
                CursorKind::ResizeNwSe,
                CursorKind::ResizeNs,
                CursorKind::ResizeNeSw,
                CursorKind::ResizeEw,
                CursorKind::ResizeNwSe,
                CursorKind::ResizeNs,
                CursorKind::ResizeNeSw,
                CursorKind::ResizeEw,
            ]
        );

        // Select the 200x100 placed image and drag its Bottom dot down by +50 with Shift held.
        click(&mut e, 100.0, 50.0);
        let bottom_dot = Point::new(100.0, 100.0);
        e.pointer_move(bottom_dot, true);
        assert_eq!(e.cursor(), CursorKind::ResizeNs);
        e.pointer_down(bottom_dot);
        e.pointer_move(Point::new(100.0, 150.0), true);
        e.pointer_up(Point::new(100.0, 150.0));
        let img_bounds = e.elements()[0].as_image().unwrap().bounds;
        assert_eq!(img_bounds, Rect::new(-50.0, 0.0, 300.0, 150.0));

        // Deselect image, then Shift-drag the canvas's own Bottom dot down by +40:
        // 200x100 (2:1) grows to 280x140!
        e.clear_tool_or_selection();
        let canvas_bottom = outer.handle_point(Handle::Bottom);
        e.pointer_move(canvas_bottom, true);
        e.pointer_down(canvas_bottom);
        e.pointer_move(Point::new(canvas_bottom.x, canvas_bottom.y + 40.0), true);
        e.pointer_up(Point::new(canvas_bottom.x, canvas_bottom.y + 40.0));
        assert_eq!(e.screen(), Rect::new(0.0, 0.0, 280.0, 140.0));
    }

    #[test]
    fn background_right_click_opens_menu_and_background_color_changes_undo_export_and_save_bs() {
        use crate::format::{decode_bs, encode_bs};

        let fonts = Arc::new(Fonts::empty());
        let mut comp = Composition::from_slice(Pixmap::new(100, 80).unwrap(), 1.0, Vec::new()).unwrap();
        // Expand canvas to 200x140 with the 100x80 image shifted to (50, 0), leaving left/bottom background space.
        comp.width = 200;
        comp.height = 140;
        comp.elements[0] = comp.elements[0].translated(Point::new(50.0, 0.0));
        let mut e = Editor::from_composition(comp, fonts.clone());

        // Right-clicking on the placed image (80, 40) does NOT open the background menu.
        assert_eq!(e.right_click_at(Point::new(80.0, 40.0)), RightClick::Handled);
        // Right-clicking on the exposed left strip (20, 40) or bottom strip (100, 110) opens BackgroundMenu!
        assert_eq!(e.right_click_at(Point::new(20.0, 40.0)), RightClick::BackgroundMenu);
        assert_eq!(e.right_click_at(Point::new(100.0, 110.0)), RightClick::BackgroundMenu);

        // Change background to Dark (#191919), verify export & .bs round-trip, then undo back to White.
        let dark = Color::rgb(0x191919);
        assert!(e.set_background_color(dark));
        assert!(!e.set_background_color(dark), "setting identical color is a no-op");
        assert_eq!(e.background_color(), Some(dark));

        let exported = e.export().unwrap();
        let p = exported.pixel(10, 10).unwrap();
        assert_eq!((p.red(), p.green(), p.blue(), p.alpha()), (0x19, 0x19, 0x19, 255));

        let bs = encode_bs(&e.to_composition().unwrap()).unwrap();
        let reopened = decode_bs(&bs).unwrap();
        assert_eq!(reopened.background, dark);

        // Transparent background also exports alpha = 0 and round-trips through .bs.
        assert!(e.set_background_color(Color::TRANSPARENT));
        let trans_export = e.export().unwrap();
        assert_eq!(trans_export.pixel(10, 10).unwrap().alpha(), 0);
        let trans_bs = decode_bs(&encode_bs(&e.to_composition().unwrap()).unwrap()).unwrap();
        assert_eq!(trans_bs.background, Color::TRANSPARENT);

        // Two undos restore Dark, then initial White (#FFFFFF), and two redos replay them.
        assert!(e.undo());
        assert_eq!(e.background_color(), Some(dark));
        assert!(e.undo());
        assert_eq!(e.background_color(), Some(Color::rgb(0xFFFFFF)));
        assert!(!e.can_undo());
        assert!(e.can_redo());
        assert!(e.redo());
        assert_eq!(e.background_color(), Some(dark));
        assert!(e.redo());
        assert_eq!(e.background_color(), Some(Color::TRANSPARENT));
        assert!(!e.can_redo());
    }

    #[test]
    fn capture_and_editor_undo_redo_cover_shapes_text_canvas_and_branching() {
        // 1. On-screen Capture mode: shapes, text (including mid-edit undo), no-op click preserves redo, new edit clears redo.
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Rect);
        drag(&mut e, (20.0, 20.0), (120.0, 90.0));
        e.toggle_tool(Tool::Text);
        click(&mut e, 160.0, 120.0);
        e.text_replace(None, "hello", false, None);
        // Undo while text edit is still open stays scoped to text content and keeps focus.
        assert!(e.undo());
        assert!(e.is_editing_text());
        assert_eq!(e.text_content(), Some(""));
        assert!(!e.can_undo());
        assert!(e.can_redo());
        assert!(e.redo());
        assert!(e.is_editing_text());
        assert_eq!(e.text_content(), Some("hello"));
        // Finish text editing, then canvas-level undo removes the text box and enables redo.
        e.finish_text();
        assert!(!e.is_editing_text());
        assert!(e.undo());
        assert_eq!(shapes_of(&e).len(), 1);
        assert!(e.can_redo());
        // No-op click selecting the rectangle does not wipe redo history.
        e.toggle_tool(Tool::Text);
        click(&mut e, 20.0, 50.0);
        assert!(e.can_redo());
        // Redo restores the text box.
        assert!(e.redo());
        assert_eq!(shapes_of(&e).len(), 2);
        assert!(!e.can_redo());
        // Undo both, redo one, then make a new edit which clears the remaining redo branch.
        assert!(e.undo());
        assert!(e.undo());
        assert_eq!(shapes_of(&e).len(), 0);
        assert_eq!(e.redo_depth(), 2);
        assert!(e.redo());
        assert_eq!(shapes_of(&e).len(), 1);
        assert_eq!(e.redo_depth(), 1);
        e.toggle_tool(Tool::Ellipse);
        drag(&mut e, (200.0, 150.0), (280.0, 220.0));
        assert!(!e.can_redo());
        assert_eq!(shapes_of(&e).len(), 2);
        assert!(e.undo());
        assert_eq!(shapes_of(&e).len(), 1);
        assert!(e.redo());
        assert_eq!(shapes_of(&e).len(), 2);

        // 2. Composition Editor mode: canvas resize, annotation, undo & redo all the way back and forth.
        let comp = Composition::from_slice(Pixmap::new(200, 150).unwrap(), 1.0, Vec::new()).unwrap();
        let mut ce = Editor::from_composition(comp, Arc::new(Fonts::empty()));
        // Resize canvas wider from the right edge.
        drag(&mut ce, (200.0, 75.0), (300.0, 75.0));
        assert_eq!(ce.screen().w, 300.0);
        // Draw an annotation on the canvas.
        ce.toggle_tool(Tool::Rect);
        drag(&mut ce, (10.0, 10.0), (80.0, 60.0));
        assert_eq!(shapes_of(&ce).len(), 1);
        // Undo annotation, then undo canvas resize.
        assert!(ce.undo());
        assert_eq!(shapes_of(&ce).len(), 0);
        assert_eq!(ce.screen().w, 300.0);
        assert!(ce.undo());
        assert_eq!(ce.screen().w, 200.0);
        assert!(!ce.can_undo());
        // Redo canvas resize, then redo annotation.
        assert!(ce.redo());
        assert_eq!(ce.screen().w, 300.0);
        assert_eq!(shapes_of(&ce).len(), 0);
        assert!(ce.redo());
        assert_eq!(shapes_of(&ce).len(), 1);
        assert!(!ce.can_redo());
    }

    #[test]
    fn in_text_undo_redo_stays_scoped_and_keeps_text_focus() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        // Draw a rectangle first so canvas undo history is non-empty.
        e.toggle_tool(Tool::Rect);
        drag(&mut e, (20.0, 20.0), (100.0, 80.0));
        assert_eq!(shapes_of(&e).len(), 1);
        assert!(e.can_undo());

        // Start a text box: text undo stack starts empty, so undo is a no-op and never blurs or removes the rect.
        e.toggle_tool(Tool::Text);
        click(&mut e, 160.0, 120.0);
        assert!(e.is_editing_text());
        assert!(!e.can_undo());
        assert!(!e.undo());
        assert!(e.is_editing_text());
        assert_eq!(shapes_of(&e).len(), 2);

        // Type two words character-by-character: each word coalesces into its own undo step.
        for ch in "hello world".chars() {
            let s = ch.to_string();
            e.text_replace(None, &s, false, None);
        }
        assert_eq!(e.text_content(), Some("hello world"));
        assert!(e.undo());
        assert!(e.is_editing_text());
        assert_eq!(e.text_content(), Some("hello "));
        assert!(e.undo());
        assert!(e.is_editing_text());
        assert_eq!(e.text_content(), Some(""));
        // Further undo stays focused inside the text editor and leaves the rectangle untouched.
        assert!(!e.undo());
        assert!(e.is_editing_text());
        assert_eq!(shapes_of(&e).len(), 2);

        // Redo brings back both words, then IME composition + commit undo as one step.
        assert!(e.redo());
        assert_eq!(e.text_content(), Some("hello "));
        assert!(e.redo());
        assert_eq!(e.text_content(), Some("hello world"));
        assert!(!e.can_redo());

        e.text_replace(None, "n", true, Some(1));
        e.text_replace(None, "ni", true, Some(2));
        e.text_replace(None, "你", false, None);
        assert_eq!(e.text_content(), Some("hello world你"));
        assert!(e.undo());
        assert!(e.is_editing_text());
        assert_eq!(e.text_content(), Some("hello world"));
        assert!(e.redo());
        assert_eq!(e.text_content(), Some("hello world你"));

        // Committing the text box returns to canvas-level undo/redo.
        e.finish_text();
        assert!(!e.is_editing_text());
        assert_eq!(shapes_of(&e).len(), 2);
        assert!(e.undo());
        assert_eq!(shapes_of(&e).len(), 1);
        assert!(e.redo());
        assert_eq!(shapes_of(&e).len(), 2);
    }

    #[test]
    fn text_box_eight_point_handles_resize_and_reflow_while_editing_and_selected() {
        use crate::text::{FontSet, FontSource};

        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../platform/assets/fonts/Nunito.ttf");
        let fonts = Arc::new(Fonts::sans_only(FontSet::from_sources(&[FontSource::file(path).weight(800.0)])));
        let comp = Composition::from_slice(Pixmap::new(500, 300).unwrap(), 1.0, Vec::new()).unwrap();
        let mut e = Editor::from_composition(comp, fonts);

        e.toggle_tool(Tool::Text);
        click(&mut e, 60.0, 60.0);
        e.text_replace(None, "one two three four five six", false, None);
        assert!(e.is_editing_text());

        // Active text box exposes 8 border resize handles and reports resize cursors on hover.
        let (orig_frame, handles) = e.text_frame().unwrap();
        assert_eq!(handles.len(), 8);
        let right_handle = handles[3];
        e.pointer_move(right_handle, false);
        assert_eq!(e.cursor(), CursorKind::ResizeEw);

        // Dragging the Right handle inward while editing text narrows the box, reflows text taller,
        // keeps text focus open, and preserves font size.
        assert!(e.text_pointer_down(right_handle, false, 1));
        let target_x = orig_frame.x + orig_frame.w * 0.45;
        e.pointer_move(Point::new(target_x, right_handle.y), false);
        e.pointer_up(Point::new(target_x, right_handle.y));
        assert!(e.is_editing_text());
        let (narrow_frame, narrow_handles) = e.text_frame().unwrap();
        assert_eq!(narrow_handles.len(), 8);
        assert!(narrow_frame.w < orig_frame.w * 0.6);
        assert!(narrow_frame.h > orig_frame.h * 1.5);
        assert_eq!(e.text_content(), Some("one two three four five six"));

        // Finishing text editing keeps the same 8 handles on `selected_frame`, and dragging a handle
        // while selected is a canvas-undoable resize.
        e.finish_text();
        assert!(!e.is_editing_text());
        let (sel_frame, sel_handles) = e.selected_frame().unwrap();
        assert_eq!(sel_frame, narrow_frame);
        assert_eq!(sel_handles.len(), 8);
        let right = sel_handles[3];
        drag(&mut e, (right.x, right.y), (right.x + 120.0, right.y));
        let (wider_frame, _) = e.selected_frame().unwrap();
        assert!(wider_frame.w > narrow_frame.w + 100.0);
        assert!(wider_frame.h < narrow_frame.h);

        // Canvas undo restores the narrower wrapped box.
        assert!(e.undo());
        let restored = e.elements()[1].as_shape().unwrap().text_frame_rect(e.metrics()).unwrap();
        assert!((restored.w - narrow_frame.w).abs() < 0.5);
    }
}
