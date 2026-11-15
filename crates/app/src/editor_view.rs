//! The editing view: draws an [`Editor`]'s image and annotations with gpui and
//! forwards input to it. It is hosted on two surfaces:
//!
//! - [`Surface::Capture`]: the full-screen overlay over the frozen screenshot,
//!   shown 1:1 in device pixels, with window picking, a dimmed surrounding and
//!   the toolbar following the selection;
//! - [`Surface::Document`]: a normal window editing a whole image (a stitched
//!   scrolling screenshot), fitted to the window width, scrollable, with the
//!   toolbar at the top.
//!
//! The editor works in image pixels; [`Viewport`] maps them to the window.

use std::ops::Range;
use std::sync::Arc;

use bluesnap_core::layout::{place_panels, place_size_label};
use bluesnap_core::reading_order::join_lines;
use bluesnap_core::render::to_rgba_straight;
use bluesnap_core::{CursorKind, Editor, Rect as PxRect, RightClick, SelectionMode, THEME_BLUE, TextKey};
use bluesnap_platform::{DisplayInfo, Platform};
use gpui::{
    App, BorderStyle, Bounds, ClipboardItem, ContentMask, Context, Corners, CursorStyle, DispatchPhase,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Hitbox, HitboxBehavior, Hsla, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Point, Render, RenderImage, ScrollWheelEvent, SharedString, Styled, Task, UTF16Selection, Window, canvas, div,
    fill, point, px, quad, rgb, rgba, size, transparent_black, white,
};

use crate::gpu_image::{ImageTiles, LayerCache, paint_tiles};
use crate::output::{self, Finish};
use crate::toolbar::{self, Action};
use crate::viewport::Viewport;
use crate::AppState;

/// Selection border width, in logical pixels.
const BORDER: f32 = 2.0;
const HANDLE_RADIUS: f32 = 4.0;
/// Gap between the selection and the floating panels, in logical pixels.
const PANEL_GAP: f32 = 8.0;
/// Document surface: inset of the toolbar and of the image, in logical pixels.
const DOC_INSET: f32 = 12.0;
const DOC_MARGIN: f32 = 16.0;
/// Document surface: annotation layers are rendered for the visible rows plus
/// this many screens above and below, snapped to `LAYER_SNAP` rows, so
/// scrolling only re-renders occasionally.
const LAYER_SNAP: f32 = 512.0;

/// Where the editor is shown; see the module docs.
pub enum Surface {
    Capture {
        /// The display this overlay covers, for starting a scrolling capture.
        display: DisplayInfo,
        /// Set when the OS withheld window contents; where to grant the permission.
        permission_url: Option<&'static str>,
    },
    Document,
}

pub struct EditorView {
    editor: Editor,
    surface: Surface,
    focus: FocusHandle,
    image: ImageTiles,
    /// Finished annotations, re-rasterized only when they change.
    static_layer: LayerCache,
    /// The annotation being drawn, dragged or typed into.
    active_layer: LayerCache,
    viewport: Viewport,
    /// Document surface: how far the image is scrolled, in logical pixels.
    scroll: f32,
    /// Document surface: height of the toolbar area above the image.
    top_bar: f32,
    finished: bool,
    /// Message shown over the selection (text recognition progress/result).
    status: Option<SharedString>,
    /// Running text recognition, if any. Dropping it cancels the UI update.
    ocr: Option<Task<()>>,
    /// While the redact strength slider is dragged: its style bar's window x.
    slider: Option<f32>,
}

fn color(c: bluesnap_core::Color) -> Hsla {
    rgb(c.to_hex()).into()
}

/// Shown when macOS returned a capture without window contents.
fn permission_banner(url: &'static str, cx: &mut Context<EditorView>) -> impl IntoElement {
    div().absolute().top(px(16.0)).left_0().right_0().flex().justify_center().child(
        div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .px(px(14.0))
            .py(px(10.0))
            .rounded(px(8.0))
            .bg(rgba(0x0000_00E0))
            .text_color(white())
            .text_size(px(13.0))
            .occlude()
            .child("bluesnap needs Screen Recording permission to capture windows. Grant it, then restart bluesnap.")
            .child(
                div()
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded(px(5.0))
                    .bg(color(THEME_BLUE))
                    .cursor_pointer()
                    .child("Open Settings")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            cx.open_url(url);
                            this.finish(Finish::Cancel, window, cx);
                        }),
                    ),
            ),
    )
}

impl EditorView {
    pub fn new(editor: Editor, image: ImageTiles, surface: Surface, cx: &mut Context<Self>) -> Self {
        if let Surface::Capture { .. } = surface {
            // However the window goes away (including OS-initiated closes), allow the next capture.
            cx.on_release(|_, cx| {
                log::debug!("overlay closed");
                cx.global_mut::<AppState>().overlay = None;
            })
            .detach();
        }
        cx.background_executor()
            .spawn(async {
                if let Err(e) = bluesnap_platform::native().prewarm_text_recognition() {
                    log::warn!("text recognition prewarm failed: {e:#}");
                }
            })
            .detach();
        let viewport = Viewport::native(editor.scale());
        Self {
            editor,
            surface,
            focus: cx.focus_handle(),
            image,
            static_layer: LayerCache::default(),
            active_layer: LayerCache::default(),
            viewport,
            scroll: 0.0,
            top_bar: 0.0,
            finished: false,
            status: None,
            ocr: None,
            slider: None,
        }
    }

    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    pub fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Places the image in the window for this frame.
    fn layout(&mut self, window: &Window) {
        let native = Viewport::native(window.scale_factor());
        self.viewport = match self.surface {
            Surface::Capture { .. } => native,
            Surface::Document => {
                let win = window.viewport_size();
                let (w, h) = (f32::from(win.width), f32::from(win.height));
                let image = self.editor.screen();
                // Fit the width, never above the image's native resolution.
                let zoom = native.zoom.min((w - 2.0 * DOC_MARGIN) / image.w).max(0.01);
                let top = DOC_INSET + toolbar::panel_size(toolbar::DOCUMENT_TOOLBAR, 0.0).1 + DOC_INSET;
                self.top_bar = top - DOC_INSET / 2.0;
                let max_scroll = (top + image.h * zoom + DOC_MARGIN - h).max(0.0);
                self.scroll = self.scroll.clamp(0.0, max_scroll);
                let x = ((w - image.w * zoom) / 2.0).max(DOC_MARGIN);
                Viewport { origin: point(px(x), px(top - self.scroll)), zoom }
            }
        };
        self.editor.set_ui_scale(self.viewport.ui_scale());
    }

    /// The image area annotation layers are rendered for.
    fn layer_clip(&self, window: &Window) -> PxRect {
        let screen = self.editor.screen();
        match self.surface {
            Surface::Capture { .. } => screen,
            Surface::Document => {
                let v = self.viewport.visible_image_rect(window.viewport_size());
                let y0 = ((v.y - v.h) / LAYER_SNAP).floor() * LAYER_SNAP;
                let y1 = ((v.bottom() + v.h) / LAYER_SNAP).ceil() * LAYER_SNAP;
                PxRect::from_edges(0.0, y0.max(0.0), screen.w, y1.min(screen.h))
            }
        }
    }

    /// Re-rasterizes the annotation layers whose content or clip changed.
    fn sync_layers(&mut self, window: &mut Window) {
        let clip = self.layer_clip(window);
        let e = &self.editor;
        self.static_layer.sync(e.static_layer_revision(), clip, || e.render_static_layer(clip), window);
        self.active_layer.sync(e.active_layer_revision(), clip, || e.render_active_layer(clip), window);
    }

    /// Runs a toolbar action.
    pub fn perform(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            Action::Tool(tool) => self.editor.toggle_tool(tool),
            Action::Undo => {
                self.editor.undo();
            }
            Action::Scrolling => self.start_scrolling_capture(window, cx),
            Action::ToText => self.extract_text(window, cx),
            Action::Edit => {}
            Action::Save | Action::Cancel | Action::Done => {
                if let Some(how) = action.finish() {
                    self.finish(how, window, cx);
                }
            }
        }
    }

    pub fn finish(&mut self, how: Finish, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        let image = match how {
            Finish::Cancel => None,
            Finish::Copy | Finish::Save => self.editor.export(),
        };
        window.remove_window();
        output::deliver(how, image, cx);
    }

    /// Closes the overlay and starts capturing the selection while the user scrolls.
    /// Annotations aren't carried over: the result is assembled from live frames.
    fn start_scrolling_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Surface::Capture { display, .. } = &self.surface else { return };
        let Some(sel) = self.editor.selection() else { return };
        let sel = sel.round();
        let rect = bluesnap_platform::PixelRect { x: sel.x as i32, y: sel.y as i32, width: sel.w as i32, height: sel.h as i32 };
        let screen = self.editor.screen();
        let (display, screen) = (display.clone(), (screen.w as i32, screen.h as i32));
        self.finish(Finish::Cancel, window, cx);
        cx.defer(move |cx| {
            if let Err(e) = crate::scrolling::start(display, screen, rect, cx) {
                log::error!("could not start scrolling capture: {e:#}");
            }
        });
    }

    pub fn is_recognizing(&self) -> bool {
        self.ocr.is_some()
    }

    /// Recognizes the text in the selection (as displayed, so redacted text stays
    /// hidden), copies it to the clipboard and closes the view.
    fn extract_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ocr.is_some() {
            return;
        }
        let Some(pixmap) = self.editor.export() else { return };
        let (width, height, rgba) = (pixmap.width(), pixmap.height(), to_rgba_straight(&pixmap));
        self.status = Some("Recognizing text…".into());
        let started = std::time::Instant::now();
        let recognized = cx
            .background_executor()
            .spawn(async move { bluesnap_platform::native().recognize_text(width, height, &rgba) });
        self.ocr = Some(cx.spawn_in(window, async move |this, cx| {
            let result = recognized.await;
            log::debug!("text recognition took {:?}", started.elapsed());
            this.update_in(cx, |this, window, cx| this.text_recognized(result, window, cx)).ok();
        }));
    }

    fn text_recognized(
        &mut self,
        result: anyhow::Result<Vec<bluesnap_platform::TextLine>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ocr = None;
        let text = result.map(|lines| {
            join_lines(
                lines
                    .into_iter()
                    .map(|l| {
                        let b = l.bounds;
                        (PxRect::new(b.x as f32, b.y as f32, b.width as f32, b.height as f32), l.text)
                    })
                    .collect(),
            )
        });
        match text {
            Ok(text) if !text.trim().is_empty() => {
                log::info!("copied {} characters of recognized text", text.chars().count());
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.finished = true;
                window.remove_window();
                return;
            }
            Ok(_) => self.status = Some("No text found".into()),
            Err(e) => {
                log::error!("text recognition failed: {e:#}");
                self.status = Some("Text recognition failed".into());
            }
        }
        cx.notify();
    }

    /// A press on the redact strength slider: jumps to the pointer and follows
    /// it until release, as one undo step.
    pub fn press_slider(&mut self, x: Pixels, window: &Window) {
        let Some(bar) = self.panel_positions(window).1 else { return };
        self.editor.begin_continuous_edit();
        self.slider = Some(bar.x);
        self.drag_slider(x);
    }

    fn drag_slider(&mut self, x: Pixels) {
        if let Some(bar_x) = self.slider {
            let strength = toolbar::slider_value(bar_x, f32::from(x));
            self.editor.set_redaction(|r| r.strength = strength);
        }
    }

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.ocr.is_none() {
            self.status = None;
        }
        if let Surface::Document = self.surface
            && f32::from(ev.position.y) < self.top_bar
        {
            return; // the toolbar area, over content scrolled out of view
        }
        let p = self.viewport.to_image(ev.position);
        match ev.button {
            MouseButton::Left => {
                if ev.click_count >= 2 && self.editor.double_click(p) {
                    return self.finish(Finish::Copy, window, cx);
                }
                self.editor.pointer_down(p);
            }
            MouseButton::Right => {
                if self.editor.right_click() == RightClick::Exit {
                    return self.finish(Finish::Cancel, window, cx);
                }
            }
            _ => return,
        }
        cx.notify();
    }

    fn mouse_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.slider.is_some() {
            self.drag_slider(ev.position.x);
            return cx.notify();
        }
        let before = (self.editor.revision(), self.editor.cursor(), self.editor.brush_ring());
        self.editor.pointer_move(self.viewport.to_image(ev.position), ev.modifiers.shift);
        if (self.editor.revision(), self.editor.cursor(), self.editor.brush_ring()) != before {
            cx.notify();
        }
    }

    fn mouse_up(&mut self, ev: &MouseUpEvent, cx: &mut Context<Self>) {
        if ev.button == MouseButton::Left && self.slider.take().is_some() {
            self.editor.end_continuous_edit();
            cx.notify();
        } else if ev.button == MouseButton::Left {
            self.editor.pointer_up(self.viewport.to_image(ev.position));
            cx.notify();
        }
    }

    fn scroll_wheel(&mut self, ev: &ScrollWheelEvent, window: &Window, cx: &mut Context<Self>) {
        if let Surface::Document = self.surface {
            self.scroll -= f32::from(ev.delta.pixel_delta(window.line_height()).y);
            cx.notify(); // clamped in `layout`
        }
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.secondary(); // Cmd on macOS, Ctrl elsewhere
        let editing = self.editor.is_editing_text();
        if editing && self.editor.text_marked().is_some() {
            return; // the IME owns the keyboard while composing
        }
        match ks.key.as_str() {
            "escape" if editing => self.editor.finish_text(),
            "escape" => return self.finish(Finish::Cancel, window, cx),
            "enter" if editing => self.editor.text_key(TextKey::Newline),
            "enter" => return self.finish(Finish::Copy, window, cx),
            "backspace" if editing => self.editor.text_key(TextKey::Backspace),
            "delete" if editing => self.editor.text_key(TextKey::Delete),
            "left" if editing => self.editor.text_key(TextKey::Left),
            "right" if editing => self.editor.text_key(TextKey::Right),
            "home" if editing => self.editor.text_key(TextKey::Home),
            "end" if editing => self.editor.text_key(TextKey::End),
            "backspace" | "delete" => {
                self.editor.delete_selected();
            }
            "z" if cmd => {
                self.editor.undo();
            }
            "c" if cmd && !editing => return self.finish(Finish::Copy, window, cx),
            "s" if cmd => return self.finish(Finish::Save, window, cx),
            // Unhandled keys propagate, so typed characters reach the text input handler.
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn scene(&self, entity: Entity<Self>, window: &Window) -> Scene {
        let e = &self.editor;
        let v = &self.viewport;
        let window_bounds = Bounds::new(point(px(0.), px(0.)), window.viewport_size());
        let (dim, selection_frame, backdrop) = match self.surface {
            Surface::Capture { .. } => {
                let active = e.active_rect().map(|r| v.to_window(r));
                let handles = match e.selection() {
                    Some(sel) if e.selection_mode() == SelectionMode::Pick => bluesnap_core::geom::Handle::ALL
                        .iter()
                        .map(|h| v.point_to_window(sel.handle_point(*h)))
                        .collect(),
                    _ => Vec::new(),
                };
                (Some(active), active.map(|a| (a, handles)), None)
            }
            Surface::Document => (None, None, Some(rgb(0xE9EAEE).into())),
        };
        let top_bar = match self.surface {
            Surface::Capture { .. } => None,
            Surface::Document => Some(px(self.top_bar)),
        };
        Scene {
            entity,
            focus: self.focus.clone(),
            backdrop,
            top_bar,
            image_bounds: v.to_window(e.screen()),
            tiles: self.image.visible(v, window_bounds),
            layers: [&self.static_layer, &self.active_layer]
                .into_iter()
                .filter_map(|l| l.image().map(|(r, img)| (v.to_window(*r), img.clone())))
                .collect(),
            clip: e.selection().map(|r| v.to_window(r)),
            dim,
            selection_frame,
            shape_frame: e
                .selected_frame()
                .map(|(r, hs)| (v.to_window(r), hs.into_iter().map(|p| v.point_to_window(p)).collect())),
            text_frame: e.text_frame().map(|r| v.to_window(r)),
            brush_ring: e.brush_ring().map(|(c, r)| (v.point_to_window(c), px(r * v.zoom))),
            caret: e.text_caret_rect().map(|r| {
                let color = e.current_style().map_or(THEME_BLUE, |(_, s)| s.color);
                (v.to_window(r), self::color(color))
            }),
            cursor: match e.cursor() {
                CursorKind::Default => CursorStyle::Arrow,
                CursorKind::Crosshair => CursorStyle::Crosshair,
                CursorKind::Move => CursorStyle::OpenHand,
                CursorKind::Text => CursorStyle::IBeam,
                CursorKind::ResizeNwSe => CursorStyle::ResizeUpLeftDownRight,
                CursorKind::ResizeNeSw => CursorStyle::ResizeUpRightDownLeft,
                CursorKind::ResizeNs => CursorStyle::ResizeUpDown,
                CursorKind::ResizeEw => CursorStyle::ResizeLeftRight,
            },
            editing_text: e.is_editing_text(),
        }
    }

    /// Where the toolbar and style bar go, in window coordinates.
    fn panel_positions(&self, window: &Window) -> (Point<f32>, Option<Point<f32>>) {
        let style = self.editor.current_style().map(|(tool, _)| toolbar::style_bar_size(tool));
        match self.surface {
            Surface::Capture { .. } => {
                let sel = self.editor.selection().unwrap_or_default();
                let screen = self.viewport.to_window_rect(self.editor.screen());
                let layout = place_panels(
                    self.viewport.to_window_rect(sel),
                    screen,
                    toolbar::panel_size(toolbar::CAPTURE_TOOLBAR, 0.0),
                    style,
                    PANEL_GAP,
                );
                let at = |r: PxRect| Point { x: r.x, y: r.y };
                (at(layout.toolbar), layout.style_bar.map(at))
            }
            Surface::Document => {
                // Top-left, with the style bar beside the toolbar, or below if narrow.
                let (tw, th) = toolbar::panel_size(toolbar::DOCUMENT_TOOLBAR, 0.0);
                let width = f32::from(window.viewport_size().width);
                let toolbar = Point { x: DOC_INSET, y: DOC_INSET };
                let style = style.map(|(sw, sh)| {
                    let beside = DOC_INSET + tw + PANEL_GAP;
                    if beside + sw + DOC_INSET <= width {
                        Point { x: beside, y: DOC_INSET + (th - sh) / 2.0 }
                    } else {
                        Point { x: DOC_INSET, y: DOC_INSET + th + PANEL_GAP }
                    }
                });
                (toolbar, style)
            }
        }
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.layout(window);
        self.sync_layers(window);

        let scene = self.scene(cx.entity(), window);
        let mut root = div()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .child(
                canvas(
                    |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    move |bounds, hitbox, window, cx| scene.paint(bounds, hitbox, window, cx),
                )
                .absolute()
                .size_full(),
            );

        if let Surface::Capture { permission_url, .. } = self.surface {
            if let Some(active) = self.editor.active_rect() {
                let label = format!("{} × {}", active.w.round() as i32, active.h.round() as i32);
                let screen = self.viewport.to_window_rect(self.editor.screen());
                let at = place_size_label(self.viewport.to_window_rect(active), screen, (90.0, 20.0), 6.0);
                root = root.child(
                    div()
                        .absolute()
                        .left(px(at.x))
                        .top(px(at.y))
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded(px(4.0))
                        .bg(rgba(0x0000_00B3))
                        .text_color(white())
                        .text_size(px(12.0))
                        .child(label),
                );
            }
            if let Some(url) = permission_url {
                root = root.child(permission_banner(url, cx));
            }
        }

        if let (Some(status), Some(sel)) = (&self.status, self.editor.selection()) {
            // Centered on the visible part of the selection.
            let full = Bounds::new(point(px(0.), px(0.)), window.viewport_size());
            let b = self.viewport.to_window(sel).intersect(&full);
            root = root.child(
                div()
                    .absolute()
                    .left(b.origin.x)
                    .top(b.origin.y)
                    .w(b.size.width)
                    .h(b.size.height)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .px(px(12.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .bg(rgba(0x0000_00CC))
                            .text_color(white())
                            .text_size(px(14.0))
                            .child(status.clone()),
                    ),
            );
        }

        if self.editor.toolbar_visible() {
            let groups = match self.surface {
                Surface::Capture { .. } => toolbar::CAPTURE_TOOLBAR,
                Surface::Document => toolbar::DOCUMENT_TOOLBAR,
            };
            let (at, style_at) = self.panel_positions(window);
            root = root.child(toolbar::toolbar(self, groups, at.x, at.y, cx));
            if let (Some((tool, style)), Some(at)) = (self.editor.current_style(), style_at) {
                root = root.child(match self.editor.current_redaction() {
                    Some(redaction) => toolbar::redact_bar(redaction, style.size, at.x, at.y, cx),
                    None => toolbar::style_bar(tool, style.size, style.color, at.x, at.y, cx),
                });
            }
        }
        root
    }
}

/// Everything the canvas paints, captured in window coordinates.
struct Scene {
    entity: Entity<EditorView>,
    focus: FocusHandle,
    /// Document surface: color behind the image.
    backdrop: Option<Hsla>,
    /// Document surface: the toolbar area, covered so scrolled content doesn't
    /// show around the toolbars.
    top_bar: Option<Pixels>,
    image_bounds: Bounds<Pixels>,
    tiles: Vec<(Bounds<Pixels>, Arc<RenderImage>)>,
    /// Annotation layers, bottom to top, and the selection they are clipped to.
    layers: Vec<(Bounds<Pixels>, Arc<RenderImage>)>,
    clip: Option<Bounds<Pixels>>,
    /// Capture surface: dim everything outside this (or everything, if `None`).
    dim: Option<Option<Bounds<Pixels>>>,
    /// Capture surface: the highlighted region's border and its resize handles.
    selection_frame: Option<(Bounds<Pixels>, Vec<Point<Pixels>>)>,
    shape_frame: Option<(Bounds<Pixels>, Vec<Point<Pixels>>)>,
    text_frame: Option<Bounds<Pixels>>,
    /// The redact brush's outline at the pointer: center and radius.
    brush_ring: Option<(Point<Pixels>, Pixels)>,
    caret: Option<(Bounds<Pixels>, Hsla)>,
    cursor: CursorStyle,
    editing_text: bool,
}

impl Scene {
    fn paint(self, bounds: Bounds<Pixels>, hitbox: Hitbox, window: &mut Window, cx: &mut App) {
        let blue = color(THEME_BLUE);
        if let Some(backdrop) = self.backdrop {
            window.paint_quad(fill(bounds, backdrop));
            let frame = self.image_bounds.dilate(px(1.));
            window.paint_quad(quad(frame, px(0.), transparent_black(), px(1.), rgba(0x0000_0026), BorderStyle::Solid));
        }
        paint_tiles(self.tiles, window);

        // Dim everything outside the highlighted region.
        let dim = rgba(0x0000_0073);
        match self.dim {
            Some(Some(a)) => {
                let (l, t, r, b) = (a.left(), a.top(), a.right(), a.bottom());
                let (w, h) = (bounds.size.width, bounds.size.height);
                for (x0, y0, x1, y1) in [(px(0.), px(0.), w, t), (px(0.), b, w, h), (px(0.), t, l, b), (r, t, w, b)] {
                    if x1 > x0 && y1 > y0 {
                        window.paint_quad(fill(Bounds::from_corners(point(x0, y0), point(x1, y1)), dim));
                    }
                }
            }
            Some(None) => window.paint_quad(fill(bounds, dim)),
            None => {}
        }

        if let Some(clip) = self.clip {
            window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                for (b, image) in self.layers {
                    window.paint_image(b, Corners::default(), image, 0, false).ok();
                }
            });
        }

        // Over the canvas only, not over the panels.
        if let Some((center, r)) = self.brush_ring.filter(|_| hitbox.is_hovered(window)) {
            let ring = Bounds::centered_at(center, size(r * 2., r * 2.));
            // A white ring between two dark ones shows on any background.
            window.paint_quad(quad(ring.dilate(px(1.)), r + px(1.), transparent_black(), px(3.5), rgba(0x0000_0059), BorderStyle::Solid));
            window.paint_quad(quad(ring, r, transparent_black(), px(1.5), white(), BorderStyle::Solid));
        }

        if let Some((region, handles)) = &self.selection_frame {
            // Outside the region, so the border never covers captured pixels.
            let border = region.dilate(px(BORDER));
            window.paint_quad(quad(border, px(0.), transparent_black(), px(BORDER), blue, BorderStyle::Solid));
            for p in handles {
                let r = px(HANDLE_RADIUS);
                let b = Bounds::centered_at(*p, size(r * 2., r * 2.));
                window.paint_quad(quad(b, r, blue, px(1.5), white(), BorderStyle::Solid));
            }
        }

        if let Some((frame, handles)) = &self.shape_frame {
            window.paint_quad(quad(*frame, px(0.), transparent_black(), px(1.), blue, BorderStyle::Dashed));
            for p in handles {
                let b = Bounds::centered_at(*p, size(px(7.), px(7.)));
                window.paint_quad(quad(b, px(1.), white(), px(1.), blue, BorderStyle::Solid));
            }
        }
        if let Some(frame) = self.text_frame {
            window.paint_quad(quad(frame, px(0.), transparent_black(), px(1.), blue, BorderStyle::Dashed));
        }
        if let Some((caret, color)) = self.caret {
            window.paint_quad(fill(caret, color));
        }
        if let (Some(height), Some(backdrop)) = (self.top_bar, self.backdrop) {
            let bar = Bounds::new(bounds.origin, size(bounds.size.width, height));
            window.paint_quad(fill(bar, backdrop));
        }

        window.set_cursor_style(self.cursor, &hitbox);
        if self.editing_text {
            window.handle_input(&self.focus, ElementInputHandler::new(bounds, self.entity.clone()), cx);
        }

        // Presses and wheel only count on the canvas (panels above it occlude it);
        // moves and releases are tracked everywhere so drags keep working over the panels.
        let entity = self.entity.clone();
        let pressed = hitbox.clone();
        window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && pressed.is_hovered(window) {
                entity.update(cx, |this, cx| this.mouse_down(ev, window, cx));
            }
        });
        let entity = self.entity.clone();
        window.on_mouse_event(move |ev: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                entity.update(cx, |this, cx| this.scroll_wheel(ev, window, cx));
            }
        });
        let entity = self.entity.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                entity.update(cx, |this, cx| this.mouse_move(ev, cx));
            }
        });
        let entity = self.entity;
        window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                entity.update(cx, |this, cx| this.mouse_up(ev, cx));
            }
        });
    }
}

// ----- IME / text input ----------------------------------------------------
//
// gpui speaks UTF-16 offsets (the platform IME convention); the editor uses
// UTF-8 byte offsets into the text being edited.

fn utf16_to_byte(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (i, c) in text.char_indices() {
        if units >= offset {
            return i;
        }
        units += c.len_utf16();
    }
    text.len()
}

fn byte_to_utf16(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].chars().map(char::len_utf16).sum()
}

fn range_to_bytes(text: &str, r: &Range<usize>) -> Range<usize> {
    utf16_to_byte(text, r.start)..utf16_to_byte(text, r.end)
}

fn range_to_utf16(text: &str, r: &Range<usize>) -> Range<usize> {
    byte_to_utf16(text, r.start)..byte_to_utf16(text, r.end)
}

impl EntityInputHandler for EditorView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.editor.text_content()?;
        let bytes = range_to_bytes(text, &range);
        adjusted.replace(range_to_utf16(text, &bytes));
        Some(text[bytes].to_string())
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        let text = self.editor.text_content()?;
        let caret = byte_to_utf16(text, self.editor.text_caret()?);
        Some(UTF16Selection { range: caret..caret, reversed: false })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        let text = self.editor.text_content()?;
        Some(range_to_utf16(text, &self.editor.text_marked()?))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.editor.text_unmark();
        cx.notify();
    }

    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        let Some(content) = self.editor.text_content() else { return };
        let range = range.map(|r| range_to_bytes(content, &r));
        self.editor.text_replace(range, text, false, None);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(content) = self.editor.text_content() else { return };
        let range = range.map(|r| range_to_bytes(content, &r));
        let caret = new_selected.map(|r| utf16_to_byte(new_text, r.end));
        self.editor.text_replace(range, new_text, true, caret);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        element: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let text = self.editor.text_content()?;
        let caret = self.editor.text_caret_rect_at(utf16_to_byte(text, range.start))?;
        let b = self.viewport.to_window(caret);
        Some(Bounds::new(element.origin + b.origin, b.size))
    }

    fn character_index_for_point(&mut self, p: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        let index = self.editor.text_index_at(self.viewport.to_image(p))?;
        Some(byte_to_utf16(self.editor.text_content()?, index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets_round_trip() {
        let s = "a你😀b";
        // a=1 unit, 你=1 unit, 😀=2 units, b=1 unit
        assert_eq!(utf16_to_byte(s, 0), 0);
        assert_eq!(utf16_to_byte(s, 2), 4);
        assert_eq!(utf16_to_byte(s, 4), 8);
        assert_eq!(utf16_to_byte(s, 99), s.len());
        assert_eq!(byte_to_utf16(s, 8), 4);
        assert_eq!(range_to_utf16(s, &(1..8)), 1..4);
    }
}
