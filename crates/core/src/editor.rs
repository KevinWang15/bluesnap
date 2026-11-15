//! The capture session state machine: window picking, selection editing and
//! annotation editing. Frontends feed it pointer/keyboard input in screenshot
//! pixel coordinates and draw what it reports.

use std::ops::Range;
use std::sync::Arc;

use tiny_skia::Pixmap;

use crate::geom::{CursorKind, Handle, Point, Rect};
use crate::render::{self, Document};
use crate::shape::{EditHandle, Geometry, Metrics, RedactShape, Shape};
use crate::style::{Color, CounterFormat, RedactArea, Redaction, SizeLevel, Style, TextStyle, Tool};
use crate::text::{Fonts, TextLayout};

/// Movement (in logical pixels) before a press turns into a drag.
const DRAG_THRESHOLD: f32 = 3.0;
/// Hit radius (in logical pixels) of the selection's and shapes' resize handles.
const HANDLE_HIT_RADIUS: f32 = 8.0;
/// Pointer samples closer than this (in logical pixels) are dropped from strokes.
const MIN_STROKE_STEP: f32 = 1.5;

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

/// How the selection behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
    /// Capture: starts empty, the user picks a window or drags a region, then can
    /// move and resize it.
    Pick,
    /// Document editing: the whole image is selected and stays that way.
    Fixed,
}

/// A brush's outline under the pointer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrushRing {
    pub center: Point,
    /// Half the brush width.
    pub radius: f32,
    /// A rounded square (the highlighter's tip) rather than a circle.
    pub square: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightClick {
    /// Nothing selected yet: the frontend should end the capture.
    Exit,
    Handled,
}

#[derive(Clone, Debug)]
enum Drag {
    /// Press while picking: becomes a manual selection once the pointer moves.
    Pick { start: Point, moved: bool },
    SelResize { handle: Handle, orig: Rect, start: Point },
    SelMove { orig: Rect, start: Point },
    Create { idx: usize, start: Point },
    ShapeMove { idx: usize, orig: Shape, start: Point, moved: bool },
    ShapeHandle { idx: usize, handle: EditHandle, orig: Shape, start: Point },
}

#[derive(Clone, Debug)]
struct TextEdit {
    idx: usize,
    caret: usize,
    marked: Option<Range<usize>>,
}

pub struct Editor {
    doc: Document,
    fonts: Arc<Fonts>,
    /// Content scale: screenshot pixels per logical pixel, for stroke widths and
    /// font sizes.
    scale: f32,
    /// Interaction scale: screenshot pixels per logical pixel *on screen right
    /// now*, for hit radii and drag thresholds. Differs from `scale` when the
    /// image is shown zoomed.
    ui_scale: f32,
    mode: SelectionMode,
    /// Candidate window rectangles, topmost first.
    windows: Vec<Rect>,
    pointer: Point,
    hover: Option<Rect>,
    selection: Option<Rect>,
    drag: Option<Drag>,
    shapes: Vec<Shape>,
    /// Snapshots of `shapes` taken before each change, for undo.
    history: Vec<Vec<Shape>>,
    selected: Option<usize>,
    tool: Option<Tool>,
    styles: [Style; Tool::ALL.len()],
    /// The redact tool's settings for new redactions.
    redaction: Redaction,
    /// Numbering for counters while there are none (existing ones set it otherwise).
    counter_format: CounterFormat,
    /// The text tool's style for new text.
    text_style: TextStyle,
    /// Inside `begin_continuous_edit` .. `end_continuous_edit`; `true` once an
    /// undo snapshot was taken for it.
    continuous: Option<bool>,
    text: Option<TextEdit>,
    revision: u64,
    /// Bumped when shapes other than the active one change (see [`Self::render_static_layer`]).
    static_revision: u64,
    /// Bumped when the active shape changes (see [`Self::render_active_layer`]).
    active_revision: u64,
}

impl Editor {
    /// `scale` is screenshot pixels per logical pixel (the display's scale factor).
    pub fn new(doc: Document, fonts: Arc<Fonts>, windows: Vec<Rect>, scale: f32) -> Self {
        let bounds = doc.bounds();
        let windows = windows.iter().filter_map(|w| w.intersect(&bounds)).collect();
        let mut editor = Self {
            doc,
            fonts,
            scale,
            ui_scale: scale,
            mode: SelectionMode::Pick,
            windows,
            pointer: Point::default(),
            hover: None,
            selection: None,
            drag: None,
            shapes: Vec::new(),
            history: Vec::new(),
            selected: None,
            tool: None,
            styles: Tool::ALL.map(Tool::default_style),
            redaction: Redaction::default(),
            counter_format: CounterFormat::default(),
            text_style: TextStyle::default(),
            continuous: None,
            text: None,
            revision: 0,
            static_revision: 0,
            active_revision: 0,
        };
        editor.update_hover();
        editor
    }

    /// An editor for a whole image (e.g. a stitched scrolling screenshot): the
    /// selection is the image and can't be changed, only annotated.
    pub fn for_document(doc: Document, fonts: Arc<Fonts>, scale: f32) -> Self {
        let mut editor = Self::new(doc, fonts, Vec::new(), scale);
        editor.mode = SelectionMode::Fixed;
        editor.selection = Some(editor.screen());
        editor.hover = None;
        editor
    }

    pub fn selection_mode(&self) -> SelectionMode {
        self.mode
    }

    /// Screenshot pixels per on-screen logical pixel, when the frontend shows the
    /// image zoomed. Keeps handles and thresholds the same size on screen.
    pub fn set_ui_scale(&mut self, ui_scale: f32) {
        self.ui_scale = ui_scale;
    }

    fn metrics(&self) -> Metrics<'_> {
        Metrics { scale: self.scale, fonts: &self.fonts }
    }

    fn touch(&mut self) {
        self.revision += 1;
    }

    /// Any shape, or which shape is active, may have changed.
    fn touch_shapes(&mut self) {
        self.touch();
        self.static_revision += 1;
        self.active_revision += 1;
    }

    /// Only the active shape changed: the hot path while dragging or typing.
    fn touch_active(&mut self) {
        self.touch();
        self.active_revision += 1;
    }

    /// The shape being dragged, created or typed into right now.
    fn active_index(&self) -> Option<usize> {
        match &self.drag {
            Some(Drag::Create { idx, .. } | Drag::ShapeMove { idx, .. } | Drag::ShapeHandle { idx, .. }) => Some(*idx),
            _ => self.text.as_ref().map(|t| t.idx),
        }
    }

    // ----- queries --------------------------------------------------------

    pub fn document(&self) -> &Document {
        &self.doc
    }

    pub fn screen(&self) -> Rect {
        self.doc.bounds()
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Changes whenever anything visible changes; frontends cache on it.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_picking(&self) -> bool {
        self.selection.is_none()
    }

    /// The highlighted region: hovered window, in-progress drag, or the selection.
    pub fn active_rect(&self) -> Option<Rect> {
        match (&self.drag, self.selection) {
            (Some(Drag::Pick { start, moved: true }), _) => Some(Rect::from_points(*start, self.pointer)),
            (_, Some(sel)) => Some(sel),
            _ => self.hover,
        }
    }

    pub fn selection(&self) -> Option<Rect> {
        self.selection
    }

    /// The toolbar is hidden while the selection itself is being dragged.
    pub fn toolbar_visible(&self) -> bool {
        self.selection.is_some()
            && !matches!(self.drag, Some(Drag::Pick { .. } | Drag::SelResize { .. } | Drag::SelMove { .. }))
    }

    pub fn tool(&self) -> Option<Tool> {
        self.tool
    }

    /// Tool and style the style bar should show: the selected shape's, else the active tool's.
    pub fn current_style(&self) -> Option<(Tool, Style)> {
        if let Some(shape) = self.selected.and_then(|i| self.shapes.get(i)) {
            return Some((shape.tool(), shape.style));
        }
        self.tool.map(|t| (t, self.styles[t.index()]))
    }

    /// The redaction settings the style bar should show: the selected
    /// redaction's, else the redact tool's when it is active.
    pub fn current_redaction(&self) -> Option<Redaction> {
        if let Some(shape) = self.selected.and_then(|i| self.shapes.get(i)) {
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

    /// Changes the redact settings, and the selected redaction's effect and
    /// strength. A shape's area kind can't change, so switching the area
    /// instead leaves the selection to draw new redactions with it. Inside a
    /// continuous edit, all changes share one undo step.
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
        let Geometry::Redact { effect, strength, .. } = &self.shapes[idx].geometry else { return self.touch() };
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
        if let Geometry::Redact { effect, strength, .. } = &mut self.shapes[idx].geometry {
            (*effect, *strength) = (changed.effect, changed.strength);
        }
        if self.continuous.is_none() {
            self.end_change();
        }
        self.touch_shapes();
    }

    /// The counters' numbering for the style bar: when the counter tool is active
    /// or a counter is selected.
    pub fn current_counter_format(&self) -> Option<CounterFormat> {
        let showing = match self.selected.and_then(|i| self.shapes.get(i)) {
            Some(shape) => shape.tool() == Tool::Counter,
            None => self.tool == Some(Tool::Counter),
        };
        showing.then(|| self.counter_format())
    }

    /// All counters share one numbering, so they read as one sequence.
    fn counter_format(&self) -> CounterFormat {
        self.shapes
            .iter()
            .find_map(|s| match s.geometry {
                Geometry::Counter { format, .. } => Some(format),
                _ => None,
            })
            .unwrap_or(self.counter_format)
    }

    /// The text style the style bar should show: the selected text's, else
    /// the text tool's when it is active.
    pub fn current_text_style(&self) -> Option<TextStyle> {
        match self.selected.and_then(|i| self.shapes.get(i)) {
            Some(shape) => match &shape.geometry {
                Geometry::Text { style, .. } => Some(*style),
                _ => None,
            },
            None => (self.tool == Some(Tool::Text)).then_some(self.text_style),
        }
    }

    /// Applies to the selected text if there is one (one undo step, or part
    /// of the edit while typing), and to new text.
    pub fn set_text_style(&mut self, new: TextStyle) {
        self.text_style = new;
        let Some(idx) = self.selected else { return self.touch() };
        if !matches!(self.shapes[idx].geometry, Geometry::Text { .. }) {
            return self.touch();
        }
        if self.text.is_none() {
            self.begin_change();
        }
        if let Geometry::Text { style, .. } = &mut self.shapes[idx].geometry {
            *style = new;
        }
        if self.text.is_none() {
            self.end_change();
        }
        self.touch_shapes();
    }

    /// Renumbers every counter with `format`, and new ones (one undo step).
    pub fn set_counter_format(&mut self, format: CounterFormat) {
        self.counter_format = format;
        self.begin_change();
        for shape in &mut self.shapes {
            if let Geometry::Counter { format: f, .. } = &mut shape.geometry {
                *f = format;
            }
        }
        self.end_change();
        self.touch_shapes();
    }

    /// Numbers counters 1, 2, 3 … in the order they were placed, so deleting
    /// one closes the gap.
    fn renumber_counters(&mut self) {
        let mut n = 0;
        for shape in &mut self.shapes {
            if let Geometry::Counter { number, .. } = &mut shape.geometry {
                n += 1;
                *number = n;
            }
        }
    }

    /// Adds area redactions over `rects` (found automatically), with the
    /// current redaction settings, as one undo step. Parts outside the
    /// selection are dropped. Returns how many were added.
    pub fn add_redactions(&mut self, rects: &[Rect]) -> usize {
        let Some(sel) = self.selection else { return 0 };
        self.finish_text();
        self.begin_change();
        let style = self.styles[Tool::Redact.index()];
        let before = self.shapes.len();
        for r in rects.iter().filter_map(|r| r.intersect(&sel)) {
            let geometry = Geometry::Redact {
                area: RedactShape::Rect(r),
                effect: self.redaction.effect,
                strength: self.redaction.strength,
            };
            let shape = Shape { geometry, style };
            if !shape.is_degenerate() {
                self.shapes.push(shape);
            }
        }
        self.end_change();
        self.selected = None;
        self.touch_shapes();
        self.shapes.len() - before
    }

    /// Starts a run of changes that should undo as one (a slider drag).
    pub fn begin_continuous_edit(&mut self) {
        self.continuous = Some(false);
    }

    pub fn end_continuous_edit(&mut self) {
        if self.continuous.take() == Some(true) {
            self.end_change();
        }
    }

    /// Where to show a wide brush's outline, when the next press would paint
    /// with it: the redact brush (round) and the highlighter (a chisel tip).
    pub fn brush_ring(&self) -> Option<BrushRing> {
        let painting = matches!(self.drag, None | Some(Drag::Create { .. }));
        let tool = self.tool?;
        let brush = match tool {
            Tool::Redact => self.redaction.area == RedactArea::Brush,
            Tool::Highlighter => true,
            _ => false,
        };
        let inside = self.selection.is_some_and(|s| s.contains(self.pointer));
        (painting && brush && inside && self.text.is_none()).then(|| BrushRing {
            center: self.pointer,
            radius: self.styles[tool.index()].size.logical_px(tool) * self.scale / 2.0,
            square: tool == Tool::Highlighter,
        })
    }

    pub fn can_undo(&self) -> bool {
        !self.history.is_empty()
    }

    pub fn has_shapes(&self) -> bool {
        !self.shapes.is_empty()
    }

    /// Bounds and handle positions of the selected shape, for drawing its edit frame.
    pub fn selected_frame(&self) -> Option<(Rect, Vec<Point>)> {
        let shape = self.shapes.get(self.selected?)?;
        if self.text.is_some() || matches!(self.drag, Some(Drag::Create { .. })) {
            return None;
        }
        let handles = shape.handles().into_iter().map(|(_, p)| p).collect();
        Some((shape.bounds(self.metrics()), handles))
    }

    pub fn cursor(&self) -> CursorKind {
        match &self.drag {
            Some(Drag::Pick { .. } | Drag::Create { .. }) => return CursorKind::Crosshair,
            Some(Drag::SelResize { handle, .. }) => return handle.cursor(),
            Some(Drag::SelMove { .. } | Drag::ShapeMove { .. }) => return CursorKind::Move,
            Some(Drag::ShapeHandle { handle, .. }) => {
                return match handle {
                    EditHandle::Bounds(h) => h.cursor(),
                    _ => CursorKind::Crosshair,
                };
            }
            None => {}
        }
        let p = self.pointer;
        let Some(sel) = self.selection else { return CursorKind::Crosshair };
        if let Some(edit) = &self.text
            && self.shapes[edit.idx].bounds(self.metrics()).inflate(4.0 * self.ui_scale).contains(p)
        {
            return CursorKind::Text;
        }
        if let Some((_, h)) = self.shape_handle_at(p) {
            return match h {
                EditHandle::Bounds(h) => h.cursor(),
                _ => CursorKind::Crosshair,
            };
        }
        if let Some(h) = self.selection_handle_at(sel, p) {
            return h.cursor();
        }
        if self.shape_at(p).is_some() {
            return CursorKind::Move;
        }
        match self.tool {
            Some(Tool::Text) if sel.contains(p) => CursorKind::Text,
            Some(_) if sel.contains(p) => CursorKind::Crosshair,
            None if sel.contains(p) && self.mode == SelectionMode::Pick => CursorKind::Move,
            _ => CursorKind::Default,
        }
    }

    fn shape_at(&self, p: Point) -> Option<usize> {
        let m = self.metrics();
        self.shapes.iter().rposition(|s| s.hit(p, m))
    }

    /// The selection's resize handle under `p`, if the selection can be resized.
    fn selection_handle_at(&self, sel: Rect, p: Point) -> Option<Handle> {
        match self.mode {
            SelectionMode::Pick => sel.handle_at(p, HANDLE_HIT_RADIUS * self.ui_scale),
            SelectionMode::Fixed => None,
        }
    }

    fn shape_handle_at(&self, p: Point) -> Option<(usize, EditHandle)> {
        let idx = self.selected?;
        let radius = HANDLE_HIT_RADIUS * self.ui_scale;
        self.shapes[idx]
            .handles()
            .into_iter()
            .find(|(_, hp)| hp.distance(p) <= radius)
            .map(|(h, _)| (idx, h))
    }

    fn update_hover(&mut self) {
        let p = self.pointer;
        self.hover = Some(self.windows.iter().find(|w| w.contains(p)).copied().unwrap_or(self.screen()));
    }

    // ----- pointer input --------------------------------------------------

    pub fn pointer_down(&mut self, p: Point) {
        self.pointer = p;
        self.touch();

        if let Some(edit) = self.text.clone() {
            let shape = &self.shapes[edit.idx];
            if shape.bounds(self.metrics()).inflate(4.0 * self.ui_scale).contains(p) {
                self.set_caret_from_point(p);
            } else {
                self.finish_text();
            }
            return;
        }

        let Some(sel) = self.selection else {
            self.update_hover();
            self.drag = Some(Drag::Pick { start: p, moved: false });
            return;
        };

        if let Some((idx, handle)) = self.shape_handle_at(p) {
            self.begin_change();
            self.drag = Some(Drag::ShapeHandle { idx, handle, orig: self.shapes[idx].clone(), start: p });
            self.touch_shapes();
            return;
        }
        if let Some(handle) = self.selection_handle_at(sel, p) {
            self.drag = Some(Drag::SelResize { handle, orig: sel, start: p });
            return;
        }
        if let Some(idx) = self.shape_at(p) {
            self.selected = Some(idx);
            self.begin_change();
            self.drag = Some(Drag::ShapeMove { idx, orig: self.shapes[idx].clone(), start: p, moved: false });
            self.touch_shapes();
            return;
        }
        self.selected = None;
        if !sel.contains(p) {
            return;
        }
        match self.tool {
            Some(tool) => self.start_shape(tool, p),
            None if self.mode == SelectionMode::Pick => self.drag = Some(Drag::SelMove { orig: sel, start: p }),
            None => {}
        }
    }

    fn start_shape(&mut self, tool: Tool, p: Point) {
        let style = self.styles[tool.index()];
        let geometry = match tool {
            Tool::Rect => Geometry::Rect(Rect::from_points(p, p)),
            Tool::Ellipse => Geometry::Ellipse(Rect::from_points(p, p)),
            Tool::Arrow => Geometry::Arrow { from: p, to: p },
            Tool::Pen => Geometry::Pen(vec![p]),
            Tool::Highlighter => Geometry::Highlight(vec![p]),
            Tool::Counter => Geometry::Counter {
                center: p,
                number: self.shapes.iter().filter(|s| s.tool() == Tool::Counter).count() as u32 + 1,
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
                // Center the first line vertically on the click.
                let size = style.size.logical_px(Tool::Text) * self.scale;
                let half_line = self.fonts.get(self.text_style.family()).layout("", size).line_height / 2.0;
                Geometry::Text { origin: Point::new(p.x, p.y - half_line), content: String::new(), style: self.text_style }
            }
        };
        self.begin_change();
        self.shapes.push(Shape { geometry, style });
        let idx = self.shapes.len() - 1;
        self.selected = Some(idx);
        if tool == Tool::Text {
            self.text = Some(TextEdit { idx, caret: 0, marked: None });
        } else {
            self.drag = Some(Drag::Create { idx, start: p });
        }
        self.touch_shapes();
    }

    /// `constrain` (usually Shift) keeps rectangles square and ellipses circular.
    pub fn pointer_move(&mut self, p: Point, constrain: bool) {
        let prev = self.pointer;
        self.pointer = p;
        let Some(drag) = &mut self.drag else {
            if self.selection.is_none() {
                let before = self.hover;
                self.update_hover();
                if self.hover != before {
                    self.touch();
                }
            }
            return;
        };
        let screen = self.doc.bounds();
        let scale = self.ui_scale;
        let shape_drag = matches!(drag, Drag::Create { .. } | Drag::ShapeMove { .. } | Drag::ShapeHandle { .. });
        match drag {
            Drag::Pick { start, moved } => {
                if !*moved && start.distance(p) > DRAG_THRESHOLD * scale {
                    *moved = true;
                }
            }
            Drag::SelResize { handle, orig, start } => {
                let r = orig.resized(*handle, p - *start);
                self.selection = Some(r.intersect(&screen).unwrap_or(Rect::new(r.x, r.y, 1.0, 1.0)));
            }
            Drag::SelMove { orig, start } => {
                self.selection = Some(orig.translate(p - *start).clamp_within(&screen));
            }
            Drag::Create { idx, start } => {
                let start = *start;
                let shape = &mut self.shapes[*idx];
                match &mut shape.geometry {
                    Geometry::Rect(r)
                    | Geometry::Ellipse(r)
                    | Geometry::Redact { area: RedactShape::Rect(r), .. } => {
                        *r = Rect::from_points(start, constrained(start, p, constrain))
                    }
                    Geometry::Arrow { to, .. } => *to = p,
                    // Placed where the press ends, so it can be positioned precisely.
                    Geometry::Counter { center, .. } => *center = p,
                    // A straight line while constrained, for highlighting a line of text.
                    Geometry::Highlight(pts) if constrain => *pts = vec![start, p],
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
            Drag::ShapeMove { idx, orig, start, moved } => {
                *moved |= start.distance(p) > DRAG_THRESHOLD * scale;
                if *moved {
                    self.shapes[*idx] = orig.translated(p - *start);
                }
            }
            Drag::ShapeHandle { idx, handle, orig, start } => {
                self.shapes[*idx] = orig.with_handle_moved(*handle, p - *start);
            }
        }
        if prev != p {
            if shape_drag { self.touch_active() } else { self.touch() }
        }
    }

    pub fn pointer_up(&mut self, p: Point) {
        self.pointer = p;
        let Some(drag) = self.drag.take() else { return };
        if matches!(drag, Drag::Create { .. } | Drag::ShapeMove { .. } | Drag::ShapeHandle { .. }) {
            self.touch_shapes();
        } else {
            self.touch();
        }
        match drag {
            Drag::Pick { start, moved } => {
                let r = Rect::from_points(start, p).intersect(&self.screen());
                let min = DRAG_THRESHOLD * self.ui_scale;
                self.selection = match r {
                    Some(r) if moved && r.w >= min && r.h >= min => Some(r.round()),
                    _ => self.hover,
                };
            }
            Drag::SelResize { .. } | Drag::SelMove { .. } => {
                self.selection = self.selection.map(|s| s.round());
            }
            Drag::Create { idx, .. } => {
                if self.shapes[idx].is_degenerate() {
                    self.shapes.remove(idx);
                    self.selected = None;
                }
                self.end_change();
            }
            Drag::ShapeMove { idx, moved, .. } => {
                self.end_change();
                if !moved && matches!(self.shapes[idx].geometry, Geometry::Text { .. }) {
                    self.edit_text(idx, p);
                }
            }
            Drag::ShapeHandle { .. } => self.end_change(),
        }
    }

    /// Returns true when the double click should copy the capture (Snipaste-style).
    pub fn double_click(&mut self, p: Point) -> bool {
        self.text.is_none()
            && self.tool.is_none()
            && self.selection.is_some_and(|s| s.contains(p))
            && self.shape_at(p).is_none()
    }

    pub fn right_click(&mut self) -> RightClick {
        self.touch();
        if self.text.is_some() {
            self.finish_text();
            return RightClick::Handled;
        }
        if self.drag.is_some() || !self.shapes.is_empty() || self.mode == SelectionMode::Fixed {
            return RightClick::Handled;
        }
        if self.selection.is_none() {
            return RightClick::Exit;
        }
        // Back to picking a window.
        self.selection = None;
        self.tool = None;
        self.selected = None;
        self.update_hover();
        RightClick::Handled
    }

    // ----- commands -------------------------------------------------------

    /// Selecting the active tool again deselects it.
    pub fn toggle_tool(&mut self, tool: Tool) {
        self.finish_text();
        self.tool = if self.tool == Some(tool) { None } else { Some(tool) };
        self.selected = None;
        self.touch();
    }

    pub fn set_color(&mut self, color: Color) {
        self.update_style(|s| s.color = color);
    }

    pub fn set_size(&mut self, size: SizeLevel) {
        self.update_style(|s| s.size = size);
    }

    /// Applies to the selected shape if there is one, and to the tool's future shapes.
    fn update_style(&mut self, f: impl Fn(&mut Style)) {
        let Some((tool, _)) = self.current_style() else { return };
        f(&mut self.styles[tool.index()]);
        if let Some(idx) = self.selected {
            if self.text.is_none() {
                self.begin_change();
            }
            f(&mut self.shapes[idx].style);
            if self.text.is_none() {
                self.end_change();
            }
        }
        self.touch_shapes();
    }

    pub fn undo(&mut self) -> bool {
        self.finish_text();
        self.drag = None;
        let Some(prev) = self.history.pop() else { return false };
        self.shapes = prev;
        self.selected = None;
        self.touch_shapes();
        true
    }

    pub fn delete_selected(&mut self) -> bool {
        if self.text.is_some() {
            return false;
        }
        let Some(idx) = self.selected.take() else { return false };
        self.begin_change();
        self.shapes.remove(idx);
        self.renumber_counters();
        self.touch_shapes();
        true
    }

    fn begin_change(&mut self) {
        self.history.push(self.shapes.clone());
    }

    /// Drops the snapshot from `begin_change` if nothing actually changed.
    fn end_change(&mut self) {
        if self.history.last() == Some(&self.shapes) {
            self.history.pop();
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
    }

    fn set_caret_from_point(&mut self, p: Point) {
        let Some(edit) = &self.text else { return };
        let shape = &self.shapes[edit.idx];
        let Some((origin, layout)) = self.text_layout(shape) else { return };
        let caret = layout.index_at(p - origin);
        if let Some(edit) = &mut self.text {
            edit.caret = caret;
            edit.marked = None;
        }
        self.touch_active();
    }

    /// Commits the text being edited; empty text boxes are removed.
    pub fn finish_text(&mut self) {
        let Some(edit) = self.text.take() else { return };
        if self.shapes[edit.idx].is_degenerate() {
            self.shapes.remove(edit.idx);
            self.selected = None;
        }
        self.end_change();
        self.touch_shapes();
    }

    pub fn text_content(&self) -> Option<&str> {
        let edit = self.text.as_ref()?;
        match &self.shapes[edit.idx].geometry {
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

    /// Replaces `range` (byte offsets; defaults to the marked text, else the caret) with
    /// `new_text`. With `mark`, the inserted text becomes IME composition text and
    /// `caret_in_new` positions the caret inside it.
    pub fn text_replace(&mut self, range: Option<Range<usize>>, new_text: &str, mark: bool, caret_in_new: Option<usize>) {
        let Some(edit) = self.text.clone() else { return };
        let Geometry::Text { content, .. } = &mut self.shapes[edit.idx].geometry else { return };
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
        let marked = (mark && !new_text.is_empty()).then_some(range.start..end);
        self.text = Some(TextEdit { caret, marked, ..edit });
        self.touch_active();
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

    /// A text shape's origin and layout, in its style's font family.
    fn text_layout(&self, shape: &Shape) -> Option<(Point, TextLayout)> {
        let Geometry::Text { origin, content, style } = &shape.geometry else { return None };
        Some((*origin, self.fonts.get(style.family()).layout(content, shape.size_px(self.scale))))
    }

    /// The color of the edited text's glyphs (for its caret).
    pub fn text_ink(&self) -> Option<Color> {
        let shape = &self.shapes[self.text.as_ref()?.idx];
        let Geometry::Text { style, .. } = &shape.geometry else { return None };
        Some(style.ink(shape.style.color))
    }

    /// Screen-space caret rectangle at byte offset `index` of the edited text.
    pub fn text_caret_rect_at(&self, index: usize) -> Option<Rect> {
        let edit = self.text.as_ref()?;
        let shape = &self.shapes[edit.idx];
        let (origin, layout) = self.text_layout(shape)?;
        Some(layout.caret_rect(index).translate(origin))
    }

    pub fn text_caret_rect(&self) -> Option<Rect> {
        self.text_caret_rect_at(self.text.as_ref()?.caret)
    }

    /// Byte offset in the edited text closest to screen point `p`.
    pub fn text_index_at(&self, p: Point) -> Option<usize> {
        let edit = self.text.as_ref()?;
        let shape = &self.shapes[edit.idx];
        let (origin, layout) = self.text_layout(shape)?;
        Some(layout.index_at(p - origin))
    }

    /// Box drawn around the text being edited.
    pub fn text_frame(&self) -> Option<Rect> {
        let edit = self.text.as_ref()?;
        Some(self.shapes[edit.idx].bounds(self.metrics()).inflate(3.0 * self.scale))
    }

    // ----- output ---------------------------------------------------------

    /// Changes whenever [`Self::render_static_layer`] would draw something different.
    pub fn static_layer_revision(&self) -> u64 {
        self.static_revision
    }

    /// Changes whenever [`Self::render_active_layer`] would draw something different.
    pub fn active_layer_revision(&self) -> u64 {
        self.active_revision
    }

    /// Every annotation except the active one, over the area they cover. This is
    /// independent of the selection (the frontend clips to it), so it only needs
    /// re-rendering when a shape is committed, deleted, restyled or undone.
    ///
    /// Only the part inside `clip` is rendered (pass [`Self::screen`] for all of
    /// it); tall documents pass the visible area to keep textures small.
    pub fn render_static_layer(&self, clip: Rect) -> Option<(Rect, Pixmap)> {
        let active = self.active_index();
        let shapes: Vec<&Shape> =
            self.shapes.iter().enumerate().filter(|(i, _)| Some(*i) != active).map(|(_, s)| s).collect();
        self.render_shapes(&shapes, None, clip)
    }

    /// The shape being drawn, dragged or typed into, over just its own bounds, so
    /// each mouse move or keystroke re-rasterizes only that shape. While active it
    /// is drawn above the others; it drops back to its place when released.
    /// Clipped to `clip` like [`Self::render_static_layer`].
    pub fn render_active_layer(&self, clip: Rect) -> Option<(Rect, Pixmap)> {
        let idx = self.active_index()?;
        let underline = self.text.as_ref().and_then(|e| Some((0, e.marked.clone()?)));
        self.render_shapes(&[&self.shapes[idx]], underline, clip)
    }

    fn render_shapes(&self, shapes: &[&Shape], underline: Option<render::Underline>, clip: Rect) -> Option<(Rect, Pixmap)> {
        let m = self.metrics();
        let bounds = shapes.iter().map(|s| s.bounds(m)).reduce(|a, b| a.union(&b))?;
        let region = bounds.inflate(1.0).round_out().intersect(&self.screen())?.intersect(&clip.round_out())?;
        let pixmap = render::render_layer(&self.doc, region, shapes, m, underline)?;
        Some((region, pixmap))
    }

    /// The selected region with all annotations, as the user sees it.
    pub fn export(&mut self) -> Option<Pixmap> {
        self.finish_text();
        render::export(&self.doc, self.selection?, &self.shapes, self.metrics())
    }
}

/// Snaps `p` so the box from `start` is square.
fn constrained(start: Point, p: Point, constrain: bool) -> Point {
    if !constrain {
        return p;
    }
    let d = p - start;
    let side = d.x.abs().max(d.y.abs());
    Point::new(start.x + side.copysign(d.x), start.y + side.copysign(d.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        let doc = Document::from_rgba(400, 300, vec![255; 400 * 300 * 4]).unwrap();
        let windows = vec![Rect::new(50.0, 50.0, 100.0, 100.0), Rect::new(0.0, 0.0, 300.0, 200.0)];
        Editor::new(doc, Arc::new(Fonts::empty()), windows, 1.0)
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
        assert_eq!(e.shapes.len(), 1);
        // Grab the outline (away from the handles) and move it.
        drag(&mut e, (50.0, 70.0), (60.0, 80.0));
        assert_eq!(e.shapes[0].geometry, Geometry::Rect(Rect::new(60.0, 60.0, 100.0, 70.0)));
        // Resize via the bottom-right handle of the selected shape.
        drag(&mut e, (160.0, 130.0), (170.0, 140.0));
        assert_eq!(e.shapes[0].geometry, Geometry::Rect(Rect::new(60.0, 60.0, 110.0, 80.0)));
        assert!(e.delete_selected());
        assert!(e.shapes.is_empty());
        assert!(e.undo());
        assert!(e.undo());
        assert!(e.undo());
        assert_eq!(e.shapes[0].geometry, Geometry::Rect(Rect::new(50.0, 50.0, 100.0, 70.0)));
    }

    #[test]
    fn tiny_shapes_are_discarded_without_history() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Arrow);
        click(&mut e, 100.0, 100.0);
        assert!(e.shapes.is_empty());
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
        // Clicking elsewhere commits.
        click(&mut e, 300.0, 250.0);
        assert!(!e.is_editing_text());
        assert_eq!(e.shapes.len(), 1);
        assert!(e.undo());
        assert!(e.shapes.is_empty());
    }

    #[test]
    fn empty_text_is_dropped() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Text);
        click(&mut e, 100.0, 100.0);
        e.finish_text();
        assert!(e.shapes.is_empty());
        assert!(!e.can_undo());
    }

    #[test]
    fn style_changes_apply_to_selected_shape() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Ellipse);
        drag(&mut e, (50.0, 50.0), (150.0, 120.0));
        e.set_color(crate::style::PALETTE[2]);
        assert_eq!(e.shapes[0].style.color, crate::style::PALETTE[2]);
        assert!(e.undo());
        assert_eq!(e.shapes[0].style.color, crate::style::PALETTE[0]);
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
        // The active layer covers only the stroke; the rectangle stays in the static one.
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
    fn document_selection_is_the_image_and_never_changes() {
        let mut e = document_editor();
        let image = Rect::new(0.0, 0.0, 400.0, 3000.0);
        assert_eq!(e.selection(), Some(image));
        assert!(e.toolbar_visible());
        // Dragging the body or an edge without a tool changes nothing.
        drag(&mut e, (200.0, 200.0), (260.0, 300.0));
        drag(&mut e, (400.0, 1500.0), (300.0, 1500.0));
        assert_eq!(e.selection(), Some(image));
        assert_eq!(e.cursor(), CursorKind::Default);
        assert_eq!(e.right_click(), RightClick::Handled);
        assert_eq!(e.selection(), Some(image));
    }

    #[test]
    fn document_annotations_export_with_the_whole_image() {
        let mut e = document_editor();
        e.toggle_tool(Tool::Rect);
        drag(&mut e, (50.0, 2500.0), (150.0, 2600.0));
        drag(&mut e, (60.0, 2510.0), (70.0, 2520.0)); // inside: draws a second one
        assert_eq!(e.shapes.len(), 2);
        let out = e.export().unwrap();
        assert_eq!((out.width(), out.height()), (400, 3000));
        let px = out.pixel(50, 2550).unwrap();
        assert_eq!((px.red(), px.green(), px.blue()), (0xF5, 0x22, 0x2D));
    }

    #[test]
    fn ui_scale_sets_handle_hit_radius_on_screen() {
        let mut e = editor();
        drag(&mut e, (10.0, 20.0), (110.0, 80.0));
        // 12 px from the corner: outside the 8 px radius at 1:1 ...
        e.pointer_move(Point::new(122.0, 80.0), false);
        assert_eq!(e.cursor(), CursorKind::Default);
        // ... inside it when one on-screen pixel is two image pixels.
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
        use crate::style::RedactEffect;
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.toggle_tool(Tool::Redact);
        assert_eq!(e.current_redaction(), Some(Redaction::default()));
        e.set_redaction(|r| (r.area, r.effect) = (RedactArea::Rect, RedactEffect::Blur));
        drag(&mut e, (50.0, 50.0), (150.0, 120.0));
        let Geometry::Redact { area: RedactShape::Rect(r), effect, .. } = &e.shapes[0].geometry else { panic!() };
        assert_eq!((*r, *effect), (Rect::new(50.0, 50.0, 100.0, 70.0), RedactEffect::Blur));
        // The new redaction is selected; a slider drag changes it, as one undo step.
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
        let Geometry::Redact { strength, .. } = &e.shapes[0].geometry else { panic!() };
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
        let before = e.shapes.clone();
        e.set_redaction(|r| r.area = RedactArea::Brush);
        assert_eq!((e.selected, e.tool), (None, Some(Tool::Redact)));
        assert_eq!(e.current_redaction().unwrap().area, RedactArea::Brush);
        assert_eq!(e.shapes, before);
    }

    fn counters(e: &Editor) -> Vec<(u32, CounterFormat)> {
        e.shapes
            .iter()
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
        // Clicking a counter grabs it instead of adding one.
        drag(&mut e, (120.0, 100.0), (120.0, 200.0));
        assert_eq!(numbers(&e), [1, 2, 3]);
        assert!(matches!(e.shapes[1].geometry, Geometry::Counter { center, .. } if center == Point::new(120.0, 200.0)));
        // Deleting the second renumbers the third; undo brings both back.
        assert!(e.delete_selected());
        assert_eq!(numbers(&e), [1, 2]);
        click(&mut e, 260.0, 100.0);
        assert_eq!(numbers(&e), [1, 2, 3]);
        e.undo();
        e.undo();
        assert_eq!(numbers(&e), [1, 2, 3]);
        assert!(matches!(e.shapes[1].geometry, Geometry::Counter { center, .. } if center == Point::new(120.0, 200.0)));
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
        // One undo step for the format change (after undoing the third counter).
        e.undo();
        e.undo();
        assert!(counters(&e).iter().all(|(_, f)| *f == CounterFormat::Decimal));
        assert_eq!(e.current_counter_format(), Some(CounterFormat::Decimal));
        // Other tools don't show the numbering.
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
        assert_eq!(e.shapes[0].geometry, Geometry::Highlight(vec![Point::new(50.0, 100.0), Point::new(160.0, 116.0)]));
        // The ring shows the highlighter's width.
        e.pointer_move(Point::new(300.0, 200.0), false);
        assert_eq!(e.brush_ring(), Some(BrushRing { center: Point::new(300.0, 200.0), radius: 9.0, square: true }));
    }

    #[test]
    fn found_redactions_are_one_undo_step_inside_the_selection() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (300.0, 200.0));
        e.toggle_tool(Tool::Redact);
        e.set_redaction(|r| r.effect = crate::style::RedactEffect::Gray);
        let rects = [Rect::new(10.0, 10.0, 50.0, 12.0), Rect::new(280.0, 190.0, 50.0, 20.0), Rect::new(500.0, 10.0, 5.0, 5.0)];
        // The second is clipped to the selection; the third is outside it.
        assert_eq!(e.add_redactions(&rects), 2);
        assert!(matches!(&e.shapes[1].geometry, Geometry::Redact { area: RedactShape::Rect(r), effect: crate::style::RedactEffect::Gray, .. } if *r == Rect::new(280.0, 190.0, 20.0, 10.0)));
        assert!(e.undo());
        assert!(e.shapes.is_empty() && !e.can_undo());
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
        assert!(matches!(e.shapes[0].geometry, Geometry::Text { style: TextStyle::Boxed, .. }));
        // Select it (a press and release on it), restyle: one undo step.
        click(&mut e, 101.0, 105.0);
        e.finish_text();
        e.selected = Some(0);
        e.set_text_style(TextStyle::Outlined);
        assert!(matches!(e.shapes[0].geometry, Geometry::Text { style: TextStyle::Outlined, .. }));
        assert!(e.undo());
        assert!(matches!(e.shapes[0].geometry, Geometry::Text { style: TextStyle::Boxed, .. }));
        // Other tools don't show text styles.
        e.toggle_tool(Tool::Rect);
        assert_eq!(e.current_text_style(), None);
    }

    #[test]
    fn brush_ring_shows_only_for_the_redact_brush() {
        let mut e = editor();
        drag(&mut e, (0.0, 0.0), (400.0, 300.0));
        e.pointer_move(Point::new(100.0, 100.0), false);
        assert_eq!(e.brush_ring(), None);
        e.toggle_tool(Tool::Redact);
        assert_eq!(e.brush_ring(), Some(BrushRing { center: Point::new(100.0, 100.0), radius: 12.0, square: false }));
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
}
