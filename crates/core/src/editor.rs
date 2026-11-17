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
    Composition, Element, LayerOrder, PlacedImage, renumber_counters_in, reorder_element,
};
use crate::geom::{CursorKind, Handle, Point, Rect, resize_rect_with_handle};
use crate::outline::WindowOutline;
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
    Left,
    Right,
    Home,
    End,
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
}

#[derive(Clone, Debug)]
struct TextEdit {
    idx: usize,
    caret: usize,
    marked: Option<Range<usize>>,
}

#[derive(Clone, Debug, PartialEq)]
struct Snapshot {
    canvas: (u32, u32),
    background: Option<Color>,
    elements: Vec<Element>,
}

enum Workspace {
    Capture {
        doc: Document,
        /// Candidate windows, topmost first: (bounds clipped to the screenshot, full outline).
        windows: Vec<(Rect, WindowOutline)>,
        hover: Option<Rect>,
        /// Outline of the window under the pointer, when `hover` is a window.
        hover_window: Option<WindowOutline>,
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
        windows: impl IntoIterator<Item = impl Into<WindowOutline>>,
        scale: f32,
    ) -> Self {
        let bounds = doc.bounds();
        let windows = windows
            .into_iter()
            .map(Into::into)
            .filter_map(|w: WindowOutline| Some((w.bounds.intersect(&bounds)?, w)))
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
    pub fn for_document(doc: Document, fonts: Arc<Fonts>, scale: f32) -> Self {
        Self::from_composition(Composition::from_slice(doc.into_pixmap(), scale, Vec::new()), fonts)
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
        self.finish_text();
        self.drag = None;
        let prev = self.current_snapshot();
        let Workspace::Canvas { width, height, background, backdrop } = &mut self.workspace else {
            return None;
        };
        let mut comp = Composition {
            width: *width,
            height: *height,
            scale: self.scale,
            background: *background,
            elements: std::mem::take(&mut self.elements),
        };
        let img_idx = comp.append_slice(image, slice_scale, shapes);
        *width = comp.width;
        *height = comp.height;
        self.elements = comp.elements;
        *backdrop.borrow_mut() = None;
        self.push_snapshot(prev);
        self.end_change();
        self.tool = None;
        self.selected = Some(img_idx);
        self.touch_shapes();
        Some(img_idx)
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
        self.finish_text();
        match &self.workspace {
            Workspace::Capture { doc, selection, snapped, .. } => {
                let sel = (*selection)?.round().intersect(&doc.bounds())?;
                let mut raw = crop(doc.image(), sel.x as u32, sel.y as u32, sel.w as u32, sel.h as u32)?;
                if let Some(outline) = snapped {
                    outline.clip(&mut raw, Point::new(sel.x, sel.y));
                }
                let to_origin = Point::new(-sel.x, -sel.y);
                let shapes = self
                    .elements
                    .iter()
                    .filter_map(Element::as_shape)
                    .map(|s| s.translated(to_origin))
                    .collect();
                Some((raw, self.scale, shapes))
            }
            Workspace::Canvas { .. } => {
                let rendered = self.export()?;
                Some((rendered, self.scale, Vec::new()))
            }
        }
    }

    /// Builds a lossless editable [`Composition`] from either Capture mode (raw
    /// cropped selection + unflattened annotations) or Canvas mode (full scene).
    pub fn to_composition(&mut self) -> Option<Composition> {
        self.finish_text();
        match &self.workspace {
            Workspace::Capture { .. } => {
                let (raw, scale, shapes) = self.extract_slice()?;
                Some(Composition::from_slice(raw, scale, shapes))
            }
            Workspace::Canvas { width, height, background, .. } => Some(Composition {
                width: *width,
                height: *height,
                scale: self.scale,
                background: *background,
                elements: self.elements.clone(),
            }),
        }
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
        self.touch_shapes();
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
        !self.history.is_empty()
    }

    pub fn undo_depth(&self) -> usize {
        self.history.len().min(MAX_HISTORY)
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
        let handles = el.handles().into_iter().map(|(_, p)| p).collect();
        Some((el.bounds(self.metrics()), handles))
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
            None => {}
        }
        let p = self.pointer;
        let Some(sel) = self.selection() else { return CursorKind::Crosshair };
        if let Some(edit) = &self.text
            && self.elements.get(edit.idx).is_some_and(|e| e.bounds(self.metrics()).inflate(4.0 * self.ui_scale).contains(p))
        {
            return CursorKind::Text;
        }
        if let Some((_, h)) = self.element_handle_at(p) {
            return match h {
                EditHandle::Bounds(h) => h.cursor(),
                _ => CursorKind::Crosshair,
            };
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
        let radius = HANDLE_HIT_RADIUS * self.ui_scale;
        let is_corner = |h: EditHandle| matches!(h, EditHandle::Bounds(b) if b.is_corner());
        self.elements
            .get(idx)?
            .handles()
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
            let found = windows.iter().find(|(clipped, _)| clipped.contains(p));
            *hover = Some(found.map_or(bounds, |(clipped, _)| *clipped));
            *hover_window = found.map(|(_, outline)| *outline);
        }
    }

    // ----- pointer input --------------------------------------------------

    pub fn pointer_down(&mut self, p: Point) {
        if self.drag.is_some() {
            self.pointer_up(self.pointer);
        }
        self.pointer = p;
        self.touch();

        if let Some(edit) = self.text.clone() {
            if self.elements.get(edit.idx).is_some_and(|e| e.bounds(self.metrics()).inflate(4.0 * self.ui_scale).contains(p)) {
                self.set_caret_from_point(p);
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
                Geometry::Text { origin: Point::new(p.x, p.y - half_line), content: String::new(), style: self.text_style }
            }
        };
        self.begin_change();
        self.elements.push(Element::Shape(Shape { geometry, style }));
        let idx = self.elements.len() - 1;
        self.selected = Some(idx);
        if tool == Tool::Text {
            self.text = Some(TextEdit { idx, caret: 0, marked: None });
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
        let mut shape_drag: Option<usize> = None;
        let mut split_static = false;
        let mut image_changed: Option<usize> = None;

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
                *current = Rect::new(r.x.round(), r.y.round(), r.w.round().max(min_side), r.h.round().max(min_side));
            }
            Drag::Create { idx, start } => {
                let elem_idx = *idx;
                shape_drag = Some(elem_idx);
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
                    if matches!(next, Element::Image(_)) {
                        image_changed = Some(elem_idx);
                    } else {
                        shape_drag = Some(elem_idx);
                        split_static = !was_moved;
                    }
                    self.elements[elem_idx] = next;
                }
            }
            Drag::ElementHandle { idx, handle, orig, start } => {
                let elem_idx = *idx;
                let next = orig.with_handle_moved(*handle, p - *start, constrain);
                if matches!(next, Element::Image(_)) {
                    image_changed = Some(elem_idx);
                } else {
                    shape_drag = Some(elem_idx);
                }
                self.elements[elem_idx] = next;
            }
        }
        if let Some(idx) = image_changed {
            self.invalidate_backdrop_from(idx);
            if self.has_effect_redaction_above(idx) {
                self.touch_shapes();
            } else {
                self.touch();
            }
        } else if let Some(idx) = shape_drag {
            self.invalidate_backdrop_from(idx);
            if split_static || (is_canvas && self.has_effect_redaction_above(idx)) {
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
                if let Workspace::Capture { selection, hover, hover_window, snapped, .. } = &mut self.workspace {
                    (*selection, *snapped) = match r {
                        Some(r) if moved && r.w >= min && r.h >= min => (Some(r.round()), None),
                        // A click picks the window under the pointer, outline and all.
                        _ => (*hover, hover_window.filter(WindowOutline::is_rounded)),
                    };
                }
            }
            Drag::SelResize { orig, .. } | Drag::SelMove { orig, .. } => {
                self.touch();
                if let Workspace::Capture { selection, snapped, .. } = &mut self.workspace {
                    *selection = selection.map(|s| s.round());
                    // Any change to a snapped selection makes it a plain region.
                    if *selection != Some(orig.round()) {
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
                self.end_change();
                if matches!(self.elements[idx], Element::Shape(_)) || (changed && self.has_effect_redaction_above(idx)) {
                    self.touch_shapes();
                } else {
                    self.touch();
                }
            }
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
        *selection = None;
        *snapped = None;
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

    pub fn undo(&mut self) -> bool {
        self.finish_text();
        self.drag = None;
        let Some(prev) = self.history.pop_back() else { return false };
        if let Workspace::Canvas { width, height, background, backdrop } = &mut self.workspace {
            let old_state = (*width, *height, *background);
            (*width, *height) = prev.canvas;
            if let Some(bg) = prev.background {
                *background = bg;
            }
            if old_state != (*width, *height, *background) {
                *backdrop.borrow_mut() = None;
            }
        }
        self.elements = prev.elements;
        self.invalidate_backdrop();
        self.selected = None;
        self.touch_shapes();
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
        self.text = Some(TextEdit { idx, caret: 0, marked: None });
        self.set_caret_from_point(p);
        self.touch_shapes();
    }

    fn set_caret_from_point(&mut self, p: Point) {
        let Some(edit) = &self.text else { return };
        let Some(shape) = self.elements.get(edit.idx).and_then(Element::as_shape) else { return };
        let Some((origin, layout)) = self.text_layout(shape) else { return };
        let caret = layout.index_at(p - origin);
        if let Some(edit) = &mut self.text {
            edit.caret = caret;
            edit.marked = None;
        }
        self.touch_active();
    }

    pub fn finish_text(&mut self) {
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

    pub fn text_marked(&self) -> Option<Range<usize>> {
        self.text.as_ref()?.marked.clone()
    }

    pub fn text_unmark(&mut self) {
        if let Some(edit) = &mut self.text {
            edit.marked = None;
            self.touch_active();
        }
    }

    pub fn text_replace(&mut self, range: Option<Range<usize>>, new_text: &str, mark: bool, caret_in_new: Option<usize>) {
        let Some(edit) = self.text.clone() else { return };
        let Some(Shape { geometry: Geometry::Text { content, .. }, .. }) =
            self.elements.get_mut(edit.idx).and_then(Element::as_shape_mut)
        else {
            return;
        };
        let range = range.or(edit.marked.clone()).unwrap_or(edit.caret..edit.caret);
        if range.start > range.end || !content.is_char_boundary(range.start) || !content.is_char_boundary(range.end) {
            return;
        }
        content.replace_range(range.clone(), new_text);
        let end = range.start + new_text.len();
        let caret = caret_in_new
            .map(|c| range.start + c.min(new_text.len()))
            .filter(|c| content.is_char_boundary(*c))
            .unwrap_or(end);
        let idx = edit.idx;
        let marked = (mark && !new_text.is_empty()).then_some(range.start..end);
        self.text = Some(TextEdit { caret, marked, ..edit });
        self.invalidate_backdrop_from(idx);
        if matches!(self.workspace, Workspace::Canvas { .. }) && self.has_effect_redaction_above(idx) {
            self.touch_shapes();
        } else {
            self.touch_active();
        }
    }

    pub fn text_key(&mut self, key: TextKey) {
        let Some(edit) = self.text.clone() else { return };
        let Some(content) = self.text_content() else { return };
        let c = edit.caret;
        let prev = content[..c].char_indices().next_back().map_or(0, |(i, _)| i);
        let next = content[c..].chars().next().map_or(c, |ch| c + ch.len_utf8());
        let line_start = content[..c].rfind('\n').map_or(0, |i| i + 1);
        let line_end = content[c..].find('\n').map_or(content.len(), |i| c + i);
        match key {
            TextKey::Backspace if c > 0 => self.text_replace(Some(prev..c), "", false, None),
            TextKey::Delete if next > c => self.text_replace(Some(c..next), "", false, None),
            TextKey::Newline => self.text_replace(Some(c..c), "\n", false, None),
            TextKey::Left | TextKey::Right | TextKey::Home | TextKey::End => {
                let caret = match key {
                    TextKey::Left => prev,
                    TextKey::Right => next,
                    TextKey::Home => line_start,
                    _ => line_end,
                };
                self.text = Some(TextEdit { caret, marked: None, ..edit });
                self.touch_active();
            }
            _ => {}
        }
    }

    fn text_layout(&self, shape: &Shape) -> Option<(Point, TextLayout)> {
        let Geometry::Text { origin, content, style } = &shape.geometry else { return None };
        Some((*origin, self.fonts.get(style.family()).layout(content, shape.size_px(self.scale))))
    }

    pub fn text_ink(&self) -> Option<Color> {
        let shape = self.elements.get(self.text.as_ref()?.idx)?.as_shape()?;
        let Geometry::Text { style, .. } = &shape.geometry else { return None };
        Some(style.ink(shape.style.color))
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

    pub fn text_frame(&self) -> Option<Rect> {
        let edit = self.text.as_ref()?;
        Some(self.elements.get(edit.idx)?.bounds(self.metrics()).inflate(3.0 * self.scale))
    }

    // ----- output ---------------------------------------------------------

    pub fn static_layer_revision(&self) -> u64 {
        self.static_revision
    }

    pub fn active_layer_revision(&self) -> u64 {
        self.active_revision
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
        Editor::for_document(doc, Arc::new(Fonts::empty()), 1.0)
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
        for i in 0..10 {
            e.set_redaction(|r| r.strength = i as f32 / 10.0);
        }
        e.end_continuous_edit();
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
        assert_eq!((ring.center, ring.half_height), (Point::new(300.0, 200.0), 9.0));
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
        let d = Editor::for_document(doc, Arc::new(Fonts::empty()), 1.0).with_shapes(shapes);
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
                let comp = Composition::from_slice(Pixmap::new(400, 300).unwrap(), 1.0, Vec::new());
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
                        e.undo();
                    }
                    14 => {
                        e.delete_selected();
                    }
                    15 => e.text_replace(None, ["a", "密", "😀", "", "\n"][next(5) as usize], next(3) == 0, None),
                    16 => e.text_key([TextKey::Backspace, TextKey::Delete, TextKey::Left, TextKey::Right, TextKey::Home, TextKey::End, TextKey::Newline][next(7) as usize]),
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
        assert_eq!(e.brush_ring(), Some(BrushRing { center: Point::new(100.0, 100.0), half_width: 12.0, half_height: 12.0, corner: 12.0 }));
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
        let comp = Composition::from_slice(img, 1.0, Vec::new());
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
        let mut comp = Composition::from_slice(Pixmap::new(100, 80).unwrap(), 1.0, Vec::new());
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

        // Two undos restore Dark, then initial White (#FFFFFF).
        assert!(e.undo());
        assert_eq!(e.background_color(), Some(dark));
        assert!(e.undo());
        assert_eq!(e.background_color(), Some(Color::rgb(0xFFFFFF)));
    }
}
