//! First-class reusable canvas surface (`CanvasView`): renders and drives an
//! [`Editor`] across both the full-screen Capture overlay (`Surface::Capture`)
//! and the multi-image resizable-canvas Composition Editor (`Surface::Editor`).

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use bluesnap_core::layout::{place_panels, place_size_label};
use bluesnap_core::reading_order::join_lines;
use bluesnap_core::render::to_rgba_straight;
use bluesnap_core::shape::Shape;
use bluesnap_core::{
    Composition, CursorKind, Editor, ImageId, LayerOrder, Pixmap, Rect as PxRect, RightClick, SelectionMode,
    THEME_BLUE, TextKey, VisualBand,
};
use bluesnap_platform::{DisplayInfo, Platform};
use gpui::{
    App, BorderStyle, Bounds, ClipboardItem, ContentMask, Context, Corners, CursorStyle, DispatchPhase, DisplayId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Hitbox, HitboxBehavior, Hsla, InteractiveElement,
    IntoElement, KeyDownEvent, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, Render, RenderImage, ScrollWheelEvent, Styled, Task, UTF16Selection, Window, canvas,
    div, fill, point, px, quad, rgb, rgba, size, transparent_black, white,
};

use crate::AppState;
use crate::capture::{CaptureMode, CapturedSlice};
use crate::gpu_image::{ImageAtlasCache, ImageTiles, LayerBandPool, LayerKey, paint_tiles};
use crate::output::{self, Finish};
use crate::text_field::{byte_to_utf16, range_from_utf16, range_to_utf16, truncate_chars, utf16_to_byte};
use crate::toast::{self, Toast};
use crate::toolbar::{self, Action, hsla};
use crate::viewport::Viewport;

const BORDER: f32 = 2.0;
const HANDLE_RADIUS: f32 = 4.0;
const PANEL_GAP: f32 = 8.0;
const DOC_INSET: f32 = 12.0;
const CANVAS_MARGIN: f32 = 36.0;
const LAYER_SNAP: f32 = 512.0;

pub enum Surface {
    Capture {
        display: DisplayInfo,
        gpui_display: DisplayId,
        permission_url: Option<&'static str>,
        mode: CaptureMode,
        screenshot: ImageTiles,
    },
    Editor {
        images: ImageAtlasCache,
        /// Scroll offset from the centered fit origin, in logical window pixels.
        pan: Point<f32>,
        /// Custom zoom multiplier (`None` = auto-fit within the workspace).
        zoom_override: Option<f32>,
    },
}

pub struct CanvasView {
    editor: Editor,
    surface: Surface,
    focus: FocusHandle,
    layers: LayerBandPool,
    viewport: Viewport,
    top_bar: f32,
    finished: bool,
    toast: Option<Toast>,
    ocr: Option<Task<()>>,
    auto: Option<Task<()>>,
    loupe: crate::loupe::Loupe,
    text_menu: bool,
    bg_menu: Option<Point<f32>>,
    text_previews: Option<(bluesnap_core::Color, f32, Vec<toolbar::TextPreview>)>,
    slider: Option<f32>,
    canvas_hovered: bool,
}

fn short_error(e: &anyhow::Error) -> String {
    truncate_chars(&format!("{e:#}"), 200)
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

fn hosts(settings: &bluesnap_ai::AiSettings) -> String {
    let host = |e: &bluesnap_ai::Endpoint| {
        let url = e.url.trim();
        let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
        rest.split(['/', '?']).next().unwrap_or(rest).to_string()
    };
    let mut hosts: Vec<String> = [settings.llm(), settings.decisions()].into_iter().flatten().map(host).collect();
    hosts.dedup();
    hosts.join(" and ")
}

fn permission_banner(url: &'static str, cx: &mut Context<CanvasView>) -> impl IntoElement {
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
                    .bg(hsla(THEME_BLUE))
                    .cursor_pointer()
                    .child("Open Settings")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.finish(Finish::Cancel, window, cx);
                            cx.open_url(url);
                        }),
                    ),
            ),
    )
}

impl CanvasView {
    pub fn new(editor: Editor, surface: Surface, cx: &mut Context<Self>) -> Self {
        match &surface {
            Surface::Capture { mode, .. } => {
                let is_for_editor = *mode == CaptureMode::ForEditor;
                cx.on_release(move |this, cx| {
                    log::debug!("capture overlay released");
                    cx.global_mut::<AppState>().overlay = None;
                    // Safety net: if closed externally (e.g. Alt+F4) before `finish`
                    // handled `ForEditor`, always un-hide the Editor window.
                    if is_for_editor && !this.finished {
                        cx.defer(crate::editor_window::restore_if_hidden);
                    }
                })
                .detach();
            }
            Surface::Editor { .. } => {
                cx.on_release(|_, cx| {
                    log::debug!("editor window released");
                    let state = cx.global_mut::<AppState>();
                    state.editor = None;
                    state.editor_ref = None;
                    state.editor_hidden = false;
                })
                .detach();
            }
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
            layers: LayerBandPool::default(),
            viewport,
            top_bar: 0.0,
            finished: false,
            toast: None,
            ocr: None,
            auto: None,
            loupe: Default::default(),
            text_menu: false,
            bg_menu: None,
            text_previews: None,
            slider: None,
            canvas_hovered: true,
        }
    }

    pub fn new_editor(editor: Editor, cx: &mut Context<Self>) -> Self {
        Self::new(
            editor,
            Surface::Editor { images: ImageAtlasCache::default(), pan: Point { x: 0.0, y: 0.0 }, zoom_override: None },
            cx,
        )
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

    /// Appends a newly captured slice into this Editor session.
    pub fn append_slice(&mut self, image: Pixmap, scale: f32, shapes: Vec<Shape>, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.append_slice(image, scale, shapes);
        self.show_toast(
            Toast::new(toast::Kind::Done, "Added to editor").detail("Drag to arrange, drag handles to resize, or drag canvas edges."),
            window,
            cx,
        );
    }

    /// Replaces the entire composition (e.g. when loading a `.bs` file).
    pub fn replace_composition(&mut self, comp: Composition, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.replace_composition(comp);
        self.bg_menu = None;
        self.text_menu = false;
        if let Surface::Editor { images, pan, zoom_override } = &mut self.surface {
            images.clear(window);
            *pan = Point { x: 0.0, y: 0.0 };
            *zoom_override = None;
        }
        self.layers.clear(window);
        cx.notify();
    }

    fn is_capture(&self) -> bool {
        matches!(self.surface, Surface::Capture { .. })
    }

    fn capture_mode(&self) -> Option<CaptureMode> {
        match self.surface {
            Surface::Capture { mode, .. } => Some(mode),
            Surface::Editor { .. } => None,
        }
    }

    fn toolbar_groups(&self) -> &'static [&'static [Action]] {
        match &self.surface {
            Surface::Capture { mode: CaptureMode::Standard, .. } => toolbar::CAPTURE_TOOLBAR,
            Surface::Capture { mode: CaptureMode::ForEditor, .. } => toolbar::CAPTURE_FOR_EDITOR_TOOLBAR,
            Surface::Editor { .. } => toolbar::EDITOR_TOOLBAR,
        }
    }

    fn fit_zoom(&self, window: &Window) -> (f32, f32) {
        let native = Viewport::native(window.scale_factor());
        let win = window.viewport_size();
        let (w, h) = (f32::from(win.width), f32::from(win.height));
        let top = DOC_INSET + toolbar::panel_size(toolbar::EDITOR_TOOLBAR, 0.0).1 + DOC_INSET;
        let avail_w = (w - 2.0 * CANVAS_MARGIN).max(64.0);
        let avail_h = (h - top - 2.0 * CANVAS_MARGIN).max(64.0);
        let canvas = self.editor.screen();
        let fit_w = avail_w / canvas.w.max(1.0);
        // Tall scrolling screenshots fit width so text remains readable and scrollable;
        // regular compositions fit both width and height so all 4 canvas edges show.
        let fit = if canvas.h > canvas.w * 2.2 { fit_w } else { fit_w.min(avail_h / canvas.h.max(1.0)) };
        (native.zoom.min(fit).max(0.02), top)
    }

    fn layout(&mut self, window: &Window) {
        let native = Viewport::native(window.scale_factor());
        match &mut self.surface {
            Surface::Capture { .. } => {
                self.viewport = native;
            }
            Surface::Editor { pan, zoom_override, .. } => {
                // Freeze the viewport while dragging a canvas edge so Left/Top
                // edge drags never fight coordinate re-centering mid-drag.
                if self.editor.is_resizing_canvas() {
                    self.editor.set_ui_scale(self.viewport.ui_scale());
                    return;
                }
                let win = window.viewport_size();
                let (w, h) = (f32::from(win.width), f32::from(win.height));
                let top = DOC_INSET + toolbar::panel_size(toolbar::EDITOR_TOOLBAR, 0.0).1 + DOC_INSET;
                self.top_bar = top - DOC_INSET / 2.0;
                let canvas = self.editor.screen();
                let avail_w = (w - 2.0 * CANVAS_MARGIN).max(64.0);
                let avail_h = (h - top - 2.0 * CANVAS_MARGIN).max(64.0);
                let fit_w = avail_w / canvas.w.max(1.0);
                let fit = if canvas.h > canvas.w * 2.2 { fit_w } else { fit_w.min(avail_h / canvas.h.max(1.0)) };
                let default_zoom = native.zoom.min(fit).max(0.02);
                let zoom = zoom_override.unwrap_or(default_zoom).clamp(0.02, native.zoom * 4.0);

                let (cw, ch) = (canvas.w * zoom, canvas.h * zoom);
                let base_x = ((w - cw) / 2.0).max(CANVAS_MARGIN);
                let base_y = if ch <= avail_h { top + (avail_h + 2.0 * CANVAS_MARGIN - ch) / 2.0 } else { top + CANVAS_MARGIN };

                let max_pan_x = (cw + 2.0 * CANVAS_MARGIN - w).max(0.0);
                let max_pan_y = (top + ch + CANVAS_MARGIN - h).max(0.0);
                pan.x = pan.x.clamp(-CANVAS_MARGIN, max_pan_x + CANVAS_MARGIN);
                pan.y = pan.y.clamp(-CANVAS_MARGIN, max_pan_y + CANVAS_MARGIN);

                self.viewport = Viewport { origin: point(px(base_x - pan.x), px(base_y - pan.y)), zoom };
            }
        }
        self.editor.set_ui_scale(self.viewport.ui_scale());
    }

    fn layer_clip(&self, window: &Window) -> PxRect {
        let screen = self.editor.active_canvas_rect();
        match self.surface {
            Surface::Capture { .. } => screen,
            Surface::Editor { .. } => {
                let v = self.viewport.visible_image_rect(window.viewport_size());
                let x0 = ((v.x - v.w) / LAYER_SNAP).floor() * LAYER_SNAP;
                let y0 = ((v.y - v.h) / LAYER_SNAP).floor() * LAYER_SNAP;
                let x1 = ((v.right() + v.w) / LAYER_SNAP).ceil() * LAYER_SNAP;
                let y1 = ((v.bottom() + v.h) / LAYER_SNAP).ceil() * LAYER_SNAP;
                PxRect::from_edges(x0.max(screen.x), y0.max(screen.y), x1.min(screen.right()), y1.min(screen.bottom()))
            }
        }
    }

    fn sync_gpu_resources(&mut self, window: &mut Window) -> Vec<SceneBand> {
        let clip = self.layer_clip(window);
        // GPUI snaps each sprite's origin and size to device pixels. At a
        // fractional zoom, a single texture preserves the band's sampling grid.
        let device_zoom = f64::from(self.viewport.zoom) * f64::from(window.scale_factor());
        let split = device_zoom.fract() == 0.0;
        let window_bounds = Bounds::new(point(px(0.), px(0.)), window.viewport_size());
        let bands = self.editor.visual_bands();

        if let Surface::Editor { images, .. } = &mut self.surface {
            let live: HashSet<ImageId> = bands
                .iter()
                .filter_map(|b| match b {
                    VisualBand::Image(img) => Some(img.asset.id),
                    VisualBand::Shapes { .. } => None,
                })
                .collect();
            images.prune(&live, window);
            for b in &bands {
                if let VisualBand::Image(img) = b {
                    images.ensure(&img.asset);
                }
            }
        }

        let shape_band_count = bands.iter().filter(|b| matches!(b, VisualBand::Shapes { .. })).count();
        self.layers.resize(shape_band_count, window);

        let mut shape_slot = 0usize;
        let mut out = Vec::with_capacity(bands.len());
        for band in bands {
            match band {
                VisualBand::Image(placed) => {
                    if let Surface::Editor { images, .. } = &mut self.surface
                        && let Some(tiles) = images.ensure(&placed.asset)
                    {
                        out.push(SceneBand::Image(tiles.visible_in(placed.bounds, &self.viewport, window_bounds)));
                    }
                }
                VisualBand::Shapes { range, revision, active } => {
                    let key = LayerKey { revision, range, active, clip, split };
                    let cache = self.layers.get_mut(shape_slot);
                    for (r, img) in cache.sync(&self.editor, key, window) {
                        out.push(SceneBand::Layer(self.viewport.to_window(*r), img.clone()));
                    }
                    shape_slot += 1;
                }
            }
        }
        out
    }

    pub fn perform(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) {
            window.focus(&self.focus);
        }
        self.bg_menu = None;
        match action {
            Action::AddImage => cx.defer(crate::editor_window::request_add_image),
            Action::OpenFile => {
                let owner = bluesnap_platform::window_ref(window).ok();
                cx.defer(move |cx| crate::editor_window::prompt_open_file(false, owner, cx));
            }
            Action::Tool(tool) => self.editor.toggle_tool(tool),
            Action::Undo => {
                self.editor.undo();
            }
            Action::Scrolling => self.start_scrolling_capture(window, cx),
            Action::ToText => self.extract_text(window, cx),
            Action::AddToEditor => self.send_capture_to_editor(window, cx),
            Action::Save | Action::Cancel | Action::Done => {
                if let Some(how) = action.finish() {
                    self.finish(how, window, cx);
                }
            }
        }
    }

    fn drop_gpu_images(&mut self, window: &mut Window) {
        match &mut self.surface {
            Surface::Capture { screenshot, .. } => screenshot.drop_images(window),
            Surface::Editor { images, .. } => images.clear(window),
        }
        self.layers.clear(window);
        self.loupe.drop_image(window);
        if let Some((_, _, old)) = self.text_previews.take() {
            for (_, img, _) in old {
                window.drop_image(img).ok();
            }
        }
    }

    fn send_capture_to_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        let display = match &self.surface {
            Surface::Capture { gpui_display, .. } => Some(*gpui_display),
            Surface::Editor { .. } => None,
        };
        let slice = self.editor.extract_slice().map(|(image, scale, shapes)| CapturedSlice { image, scale, shapes, display });
        self.drop_gpu_images(window);
        window.remove_window();
        if let Some(slice) = slice {
            cx.defer(move |cx| crate::editor_window::add_slice(slice, cx));
        } else {
            cx.defer(crate::editor_window::restore_if_hidden);
        }
    }

    pub fn finish(&mut self, how: Finish, window: &mut Window, cx: &mut Context<Self>) {
        // In the Composition Editor, `Save` keeps the session open so the user
        // can save a `.bs` project and continue editing or click `Tick` to copy.
        if !self.is_capture() && how == Finish::Save {
            let (Some(image), Some(comp)) = (self.editor.export(), self.editor.to_composition()) else {
                return;
            };
            let entity = cx.entity().downgrade();
            let handle = window.window_handle();
            let owner = bluesnap_platform::window_ref(window).ok();
            cx.defer(move |cx| {
                output::save_with_dialog(
                    image,
                    comp,
                    owner,
                    None,
                    false,
                    Some(Box::new(move |path, cx| {
                        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
                        let is_bs = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("bs"));
                        handle
                            .update(cx, |_, window, cx| {
                                if is_bs {
                                    window.set_window_title(&format!("bluesnap editor — {name}"));
                                }
                                if let Some(view) = entity.upgrade() {
                                    view.update(cx, |this, cx| {
                                        this.show_toast(Toast::new(toast::Kind::Done, format!("Saved {name}")), window, cx);
                                    });
                                }
                            })
                            .ok();
                    })),
                    cx,
                );
            });
            return;
        }

        // In `CaptureMode::ForEditor`, `Done` ("Tick") sends the captured slice
        // straight into the open Editor window, and `Cancel` restores it.
        if self.capture_mode() == Some(CaptureMode::ForEditor) {
            match how {
                Finish::Copy => return self.send_capture_to_editor(window, cx),
                Finish::Cancel => {
                    if std::mem::replace(&mut self.finished, true) {
                        return;
                    }
                    self.drop_gpu_images(window);
                    window.remove_window();
                    cx.defer(crate::editor_window::restore_if_hidden);
                    return;
                }
                Finish::Save => {}
            }
        }

        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        let mode = self.capture_mode();
        match how {
            Finish::Cancel => {
                self.drop_gpu_images(window);
                window.remove_window();
                if mode == Some(CaptureMode::Standard) {
                    bluesnap_platform::native().restore_previous_app();
                }
            }
            Finish::Copy => {
                let image = self.editor.export();
                self.drop_gpu_images(window);
                window.remove_window();
                if mode == Some(CaptureMode::Standard) {
                    bluesnap_platform::native().restore_previous_app();
                }
                if let Some(image) = image {
                    output::copy_to_clipboard(image, cx);
                }
            }
            Finish::Save => {
                let pair = self.editor.export().zip(self.editor.to_composition());
                let closing = bluesnap_platform::window_ref(window).ok();
                self.drop_gpu_images(window);
                window.remove_window();
                if mode == Some(CaptureMode::ForEditor) {
                    cx.defer(crate::editor_window::restore_if_hidden);
                }
                if let Some((image, comp)) = pair {
                    // Defer until after `window.remove_window()` drops `Window`
                    // so `DestroyWindow(overlay_hwnd)` is queued ahead of the
                    // modal `GetSaveFileNameW` task on Windows.
                    cx.defer(move |cx| {
                        let owner = (mode == Some(CaptureMode::ForEditor))
                            .then(|| cx.global::<AppState>().editor_ref)
                            .flatten();
                        output::save_with_dialog(
                            image,
                            comp,
                            owner,
                            closing,
                            mode != Some(CaptureMode::ForEditor),
                            None,
                            cx,
                        );
                    });
                } else if mode == Some(CaptureMode::Standard) {
                    bluesnap_platform::native().restore_previous_app();
                }
            }
        }
    }

    fn start_scrolling_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Surface::Capture { display, mode, .. } = &self.surface else { return };
        let Some(sel) = self.editor.selection() else { return };
        let sel = sel.round();
        let rect = bluesnap_platform::PixelRect {
            x: sel.x as i32,
            y: sel.y as i32,
            width: sel.w as i32,
            height: sel.h as i32,
        };
        let screen = self.editor.screen();
        let (display, screen, mode) = (display.clone(), (screen.w as i32, screen.h as i32), *mode);
        let shapes = self.editor.shapes_in_selection();
        // Mark `finished = true` so `on_release` doesn't un-hide the Editor
        // prematurely while scrolling capture takes over the `ForEditor` session.
        self.finished = true;
        self.drop_gpu_images(window);
        window.remove_window();
        bluesnap_platform::native().restore_previous_app();
        cx.defer(move |cx| {
            if let Err(e) = crate::scrolling::start(display, screen, rect, shapes, mode, cx) {
                log::error!("could not start scrolling capture: {e:#}");
                if mode == CaptureMode::ForEditor {
                    crate::editor_window::restore_if_hidden(cx);
                }
            }
        });
    }

    pub fn is_recognizing(&self) -> bool {
        self.ocr.is_some()
    }

    pub fn show_toast(&mut self, toast: Toast, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(lifetime) = toast.lifetime() {
            let shown = toast.clone();
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(lifetime).await;
                this.update(cx, |this, cx| {
                    if this.toast.as_ref() == Some(&shown) {
                        this.toast = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        self.toast = Some(toast);
        cx.notify();
    }

    fn toast_action(&mut self, action: toast::Action, cx: &mut Context<Self>) {
        match action {
            toast::Action::OpenAiSettings => {
                self.toast = None;
                let above_overlay = self.is_capture();
                cx.defer(move |cx| crate::ai_settings::open(above_overlay, cx));
            }
        }
        cx.notify();
    }

    fn extract_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ocr.is_some() {
            return;
        }
        let Some(pixmap) = self.editor.export() else { return };
        let (width, height, rgba) = (pixmap.width(), pixmap.height(), to_rgba_straight(&pixmap));
        let scale = self.editor.scale();
        self.show_toast(Toast::new(toast::Kind::Busy, "Recognizing text…"), window, cx);
        let started = std::time::Instant::now();
        let recognized = cx
            .background_executor()
            .spawn(async move { bluesnap_platform::native().recognize_text(width, height, &rgba, scale) });
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
        let toast = match text {
            Ok(text) if !text.trim().is_empty() => {
                let chars = text.chars().count();
                log::info!("copied {chars} characters of recognized text");
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                if self.is_capture() {
                    let was_for_editor = self.capture_mode() == Some(CaptureMode::ForEditor);
                    self.finished = true;
                    self.drop_gpu_images(window);
                    window.remove_window();
                    if was_for_editor {
                        cx.defer(crate::editor_window::restore_if_hidden);
                    } else {
                        bluesnap_platform::native().restore_previous_app();
                    }
                    return;
                }
                Toast::new(toast::Kind::Done, format!("Copied {chars} characters"))
            }
            Ok(_) => Toast::new(toast::Kind::Info, "No text found").detail("Select an area with text, then try again."),
            Err(e) => {
                log::error!("text recognition failed: {e:#}");
                Toast::new(toast::Kind::Error, "Text recognition failed").detail(short_error(&e))
            }
        };
        self.show_toast(toast, window, cx);
    }

    fn loupe_target(&self, window: &Window) -> Option<(Point<Pixels>, (i32, i32))> {
        if !self.is_capture() || !self.editor.is_picking() {
            return None;
        }
        let cursor = window.mouse_position();
        let p = self.viewport.to_image(cursor);
        Some((cursor, (p.x.floor() as i32, p.y.floor() as i32)))
    }

    fn copy_color(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some((_, (x, y))) = self.loupe_target(window) else { return };
        let Some(doc) = self.editor.capture_document() else { return };
        let Some(color) = doc.color_at(x, y) else { return };
        let hex = crate::loupe::hex(color);
        log::info!("copied color {hex}");
        cx.write_to_clipboard(ClipboardItem::new_string(hex));
        self.loupe.copied();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(crate::loupe::COPIED_FOR).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    fn report_layout(&self, window: &Window) {
        let mut items = Vec::new();
        let (toolbar_at, style_at) = self.panel_positions(window);
        let groups = self.toolbar_groups();
        if self.editor.toolbar_visible() {
            items.extend(toolbar::row_items(&toolbar::toolbar_slots(groups, 0.0), 32.0, (toolbar_at.x, toolbar_at.y)));
        }
        let layer_ops = self.layer_ops();
        if let (true, Some(at)) = (self.editor.toolbar_visible(), style_at) {
            if let Some((tool, _)) = self.editor.current_style() {
                let style = toolbar::row_items(&toolbar::style_slots_ex(tool, layer_ops.is_some()), 24.0, (at.x, at.y));
                items.extend(style.into_iter().map(|(name, r)| (format!("style:{name}"), r)));
                if self.text_menu {
                    let (x, y, w) = self.text_menu_origin(at, window);
                    for (i, style) in bluesnap_core::TextStyle::ALL.into_iter().enumerate() {
                        let row = toolbar::text_style_menu_row(i);
                        items.push((format!("menu:{style:?}"), (x + row.0, y + row.1, w - 2.0 * row.0, row.3)));
                    }
                }
            } else if self.editor.selected_is_image() {
                let bar = toolbar::row_items(&toolbar::image_bar_slots(), 24.0, (at.x, at.y));
                items.extend(bar.into_iter().map(|(name, r)| (format!("style:{name}"), r)));
            }
        }
        if let Some(sel) = self.editor.selection() {
            let r = self.viewport.to_window_rect(sel);
            items.push(("selection".into(), (r.x, r.y, r.w, r.h)));
        }
        let name = if self.is_capture() { "capture" } else { "editor" };
        crate::demo::report(name, window.scale_factor(), items);
    }

    pub fn toggle_text_styles(&mut self) {
        self.text_menu = !self.text_menu;
    }

    pub fn choose_text_style(&mut self, style: bluesnap_core::TextStyle) {
        self.editor.set_text_style(style);
        self.text_menu = false;
    }

    pub fn choose_background_color(&mut self, color: bluesnap_core::Color) {
        self.editor.set_background_color(color);
        self.bg_menu = None;
    }

    pub fn pick_custom_background_color(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.bg_menu = None;
        let initial = self.editor.background_color().unwrap_or(bluesnap_core::Color::rgb(0xFFFFFF));
        let owner = bluesnap_platform::window_ref(window).ok();
        let entity = cx.entity();
        cx.defer(move |cx| {
            cx.activate(true);
            let platform = bluesnap_platform::native();
            cx.spawn(async move |cx| {
                match platform.color_dialog(owner, initial) {
                    Ok(Some(color)) => {
                        cx.update(|cx| {
                            entity.update(cx, |this, cx| {
                                this.editor.set_background_color(color);
                                cx.notify();
                            });
                        })
                        .ok();
                    }
                    Ok(None) => {}
                    Err(e) => log::error!("color dialog failed: {e:#}"),
                }
            })
            .detach();
        });
    }

    fn bg_menu_origin(&self, anchor: Point<f32>, window: &Window) -> (f32, f32) {
        let (w, h) = toolbar::background_menu_size();
        let win = window.viewport_size();
        let win_w = f32::from(win.width);
        let win_h = f32::from(win.height);
        let x = anchor.x.min(win_w - w - 8.0).max(8.0);
        let below = anchor.y + 2.0;
        let y = if below + h <= win_h - 8.0 {
            below
        } else {
            (anchor.y - h - 2.0).max(self.top_bar + 4.0)
        };
        (x, y)
    }

    fn text_menu(
        &mut self,
        current: bluesnap_core::TextStyle,
        color: bluesnap_core::Color,
        bar: Point<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let scale = window.scale_factor();
        if !self.text_previews.as_ref().is_some_and(|(c, s, _)| *c == color && *s == scale) {
            let fonts = cx.global::<AppState>().fonts.clone();
            let previews = bluesnap_core::TextStyle::ALL
                .into_iter()
                .filter_map(|style| {
                    let sample = bluesnap_core::render::text_sample(&fonts, style.label(), style, color, 15.0 * scale)?;
                    let size = (sample.width() as f32 / scale, sample.height() as f32 / scale);
                    Some((style, crate::gpu_image::pixmap_image(&sample)?, size))
                })
                .collect();
            if let Some((_, _, old)) = self.text_previews.replace((color, scale, previews)) {
                for (_, img, _) in old {
                    window.drop_image(img).ok();
                }
            }
        }
        let previews = self.text_previews.as_ref().expect("just made").2.clone();
        let (x, y, _) = self.text_menu_origin(bar, window);
        toolbar::text_style_menu(&previews, current, color.is_light(), x, y, cx)
    }

    fn text_menu_origin(&self, bar: Point<f32>, window: &Window) -> (f32, f32, f32) {
        let (w, h) = toolbar::text_style_menu_size(bluesnap_core::TextStyle::ALL.len());
        let (button_x, _) = toolbar::text_style_button(bar.x);
        let bar_h = toolbar::style_bar_size(bluesnap_core::Tool::Text).1;
        let win = window.viewport_size();
        let x = button_x.min(f32::from(win.width) - w - 8.0).max(8.0);
        let below = bar.y + bar_h + 4.0;
        let y = if below + h <= f32::from(win.height) - 8.0 { below } else { (bar.y - h - 4.0).max(8.0) };
        (x, y, w)
    }

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

    fn auto_state(&self) -> toolbar::Auto {
        if self.auto.is_some() { toolbar::Auto::Running } else { toolbar::Auto::Ready }
    }

    pub fn auto_redact(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.auto.is_some() {
            return;
        }
        let settings = cx.global::<AppState>().ai.clone();
        if !settings.is_configured() {
            let toast = Toast::new(toast::Kind::Info, "Set up AI first")
                .detail("Auto finds sensitive text with an LLM and/or a decision model. Add one in AI Settings.")
                .action("AI Settings…", toast::Action::OpenAiSettings);
            return self.show_toast(toast, window, cx);
        }
        let (Some(sel), Some(pixmap)) = (self.editor.selection(), self.editor.export()) else { return };
        let origin = bluesnap_core::Point::new(sel.round().x, sel.round().y);
        let fonts = cx.global::<AppState>().fonts.clone();
        let scale = self.editor.scale();
        let initial_title = if settings.llm().is_some() {
            "Reading text & analyzing screen…"
        } else {
            "Reading the text…"
        };
        self.show_toast(Toast::new(toast::Kind::Busy, initial_title), window, cx);
        let started = std::time::Instant::now();
        self.auto = Some(cx.spawn_in(window, async move |this, cx| {
            let image = Arc::new(pixmap);
            // Run the LLM screen-context step in parallel with local OCR ("find text")
            // so the decision model and span extractor know what the page is and which
            // values are confidential without adding serial OCR + context latency.
            let context_task = settings.llm().cloned().map(|llm_ep| {
                let (ep, img) = (llm_ep.clone(), image.clone());
                let task = cx.background_executor().spawn(async move {
                    let t0 = std::time::Instant::now();
                    (bluesnap_ai::describe_image_context(&ep, &img), t0.elapsed())
                });
                (llm_ep, task)
            });

            let bands = bluesnap_core::reading_order::bands(image.height(), 1200, 64);
            let mut all_lines: Vec<(PxRect, String)> = Vec::new();
            for (i, (read, own)) in bands.iter().cloned().enumerate() {
                if bands.len() > 1 {
                    let progress = Toast::new(toast::Kind::Busy, format!("{initial_title} {} of {}", i + 1, bands.len()));
                    this.update_in(cx, |this, window, cx| this.show_toast(progress, window, cx)).ok();
                }
                let band = image.clone();
                let recognized = cx
                    .background_executor()
                    .spawn(async move {
                        let part = bluesnap_core::pixels::crop(&band, 0, read.start, band.width(), read.end - read.start)
                            .ok_or_else(|| anyhow::anyhow!("empty band"))?;
                        bluesnap_platform::native().recognize_text(part.width(), part.height(), &to_rgba_straight(&part), scale)
                    })
                    .await;
                match recognized {
                    Ok(lines) => all_lines.extend(
                        lines
                            .into_iter()
                            .map(|l| {
                                let b = l.bounds;
                                (PxRect::new(b.x as f32, b.y as f32 + read.start as f32, b.width as f32, b.height as f32), l.text)
                            })
                            .filter(|(r, _)| own.contains(&(r.center().y.max(0.0) as u32))),
                    ),
                    Err(e) => {
                        log::error!("auto redact: text recognition failed: {e:#}");
                        let toast = Toast::new(toast::Kind::Error, "Couldn't read the text").detail(short_error(&e));
                        this.update_in(cx, |this, window, cx| this.auto_finished(toast, window, cx)).ok();
                        return;
                    }
                }
            }
            let count = all_lines.len();
            if count == 0 {
                let toast = Toast::new(toast::Kind::Info, "No text found").detail("There's nothing to redact in the selection.");
                this.update_in(cx, |this, window, cx| this.auto_finished(toast, window, cx)).ok();
                return;
            }
            let ocr_time = started.elapsed();

            let (screen_context, context_time) = match context_task {
                Some((llm_ep, task)) => {
                    let analyzing = Toast::new(toast::Kind::Busy, "Analyzing screen context…")
                        .detail(format!("Summarizing what is sensitive on this screen with {}", hosts(&settings)));
                    this.update_in(cx, |this, window, cx| this.show_toast(analyzing, window, cx)).ok();
                    let (outcome, elapsed) = task.await;
                    match outcome {
                        bluesnap_ai::ContextOutcome::Ready(ctx) => (Some(ctx), elapsed),
                        bluesnap_ai::ContextOutcome::UnsupportedImage => {
                            let boxes: Vec<PxRect> = all_lines.iter().map(|(r, _)| *r).collect();
                            let texts: Vec<String> = bluesnap_core::reading_order::rows(&boxes)
                                .into_iter()
                                .flatten()
                                .map(|i| all_lines[i].1.clone())
                                .collect();
                            let t0 = std::time::Instant::now();
                            let ctx = cx
                                .background_executor()
                                .spawn(async move { bluesnap_ai::describe_text_context(&llm_ep, &texts) })
                                .await;
                            (ctx, elapsed + t0.elapsed())
                        }
                        bluesnap_ai::ContextOutcome::Failed(err) => {
                            log::warn!("auto redact: screen context failed: {err}");
                            (None, elapsed)
                        }
                    }
                }
                None => (None, std::time::Duration::ZERO),
            };

            let checking = Toast::new(toast::Kind::Busy, format!("Checking {} with AI…", plural(count, "line")))
                .detail(format!("Using {}", hosts(&settings)));
            this.update_in(cx, |this, window, cx| this.show_toast(checking, window, cx)).ok();

            let found = {
                let (settings, image, fonts) = (settings.clone(), image.clone(), fonts.clone());
                cx.background_executor()
                    .spawn(async move {
                        bluesnap_ai::locate_sensitive_with_context(
                            &settings,
                            &image,
                            &all_lines,
                            fonts.sans(),
                            screen_context.as_deref(),
                        )
                    })
                    .await
            };
            log::info!(
                "auto redact: {count} lines, OCR {ocr_time:?}, context {context_time:?}, total {:?}",
                started.elapsed()
            );
            this.update_in(cx, |this, window, cx| {
                let toast = match found {
                    Ok((rects, report)) => {
                        let rects: Vec<PxRect> = rects.into_iter().map(|r| r.translate(origin)).collect();
                        let added = this.editor.add_redactions(&rects);
                        let toast = match added {
                            0 => Toast::new(toast::Kind::Done, "Nothing sensitive found")
                                .detail(format!("Checked {}.", plural(count, "line"))),
                            n => Toast::new(toast::Kind::Done, format!("Redacted {}", plural(n, "item")))
                                .detail("Undo removes them all; click one to change or delete it."),
                        };
                        match (report.failed, report.first_error) {
                            (0, _) | (_, None) => toast,
                            (failed, Some(e)) => Toast::new(toast::Kind::Error, format!("{} · {} failed", toast.title, plural(failed, "request")))
                                .detail(format!("Some text wasn't checked: {}", truncate_chars(&e, 160)))
                                .action("AI Settings…", toast::Action::OpenAiSettings),
                        }
                    }
                    Err(e) => {
                        log::error!("auto redact failed: {e:#}");
                        Toast::new(toast::Kind::Error, "AI redaction failed")
                            .detail(short_error(&e))
                            .action("AI Settings…", toast::Action::OpenAiSettings)
                    }
                };
                this.auto_finished(toast, window, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    fn auto_finished(&mut self, toast: Toast, window: &mut Window, cx: &mut Context<Self>) {
        self.auto = None;
        self.show_toast(toast, window, cx);
    }

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) {
            window.focus(&self.focus);
        }
        if std::mem::take(&mut self.text_menu) {
            return cx.notify();
        }
        if ev.button == MouseButton::Left && self.bg_menu.take().is_some() {
            return cx.notify();
        }
        if self.toast.as_ref().is_some_and(Toast::dismissable) {
            self.toast = None;
        }
        if !self.is_capture() && f32::from(ev.position.y) < self.top_bar {
            self.bg_menu = None;
            return cx.notify();
        }
        let p = self.viewport.to_image(ev.position);
        match ev.button {
            MouseButton::Left => {
                if ev.click_count >= 2 && self.editor.double_click(p) {
                    return self.finish(Finish::Copy, window, cx);
                }
                self.editor.pointer_down(p);
            }
            MouseButton::Right => match self.editor.right_click_at(p) {
                RightClick::Exit => return self.finish(Finish::Cancel, window, cx),
                RightClick::BackgroundMenu => {
                    self.bg_menu = Some(Point { x: f32::from(ev.position.x), y: f32::from(ev.position.y) });
                }
                RightClick::Handled => {
                    self.bg_menu = None;
                }
            },
            _ => return,
        }
        cx.notify();
    }

    fn mouse_move(&mut self, ev: &MouseMoveEvent, canvas_hovered: bool, cx: &mut Context<Self>) {
        if self.editor.is_picking() && self.is_capture() {
            self.canvas_hovered = canvas_hovered;
            cx.notify();
        }
        if self.slider.is_some() {
            self.drag_slider(ev.position.x);
            self.canvas_hovered = canvas_hovered;
            return cx.notify();
        }
        let before = (self.editor.revision(), self.editor.cursor(), self.editor.brush_ring(), self.canvas_hovered);
        self.editor.pointer_move(self.viewport.to_image(ev.position), ev.modifiers.shift);
        self.canvas_hovered = canvas_hovered;
        if (self.editor.revision(), self.editor.cursor(), self.editor.brush_ring(), self.canvas_hovered) != before {
            cx.notify();
        }
    }

    fn mouse_up(&mut self, ev: &MouseUpEvent, cx: &mut Context<Self>) {
        if ev.button == MouseButton::Left && self.slider.take().is_some() {
            self.editor.end_continuous_edit();
            cx.notify();
        } else if ev.button == MouseButton::Left {
            let zoom = self.viewport.zoom;
            self.editor.pointer_up(self.viewport.to_image(ev.position));
            let shift = self.editor.take_canvas_origin_shift();
            if shift != bluesnap_core::Point::default()
                && let Surface::Editor { pan, .. } = &mut self.surface
            {
                pan.x -= shift.x * zoom;
                pan.y -= shift.y * zoom;
            }
            cx.notify();
        }
    }

    fn scroll_wheel(&mut self, ev: &ScrollWheelEvent, window: &Window, cx: &mut Context<Self>) {
        self.bg_menu = None;
        let delta = ev.delta.pixel_delta(window.line_height());
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        let cmd = ev.modifiers.secondary();
        let shift = ev.modifiers.shift;
        if cmd && !self.is_capture() {
            let (fit, _) = self.fit_zoom(window);
            let native_zoom = Viewport::native(window.scale_factor()).zoom;
            if let Surface::Editor { zoom_override, .. } = &mut self.surface {
                let cur = zoom_override.unwrap_or(fit);
                let factor = if dy > 0.0 { 1.12 } else if dy < 0.0 { 1.0 / 1.12 } else { 1.0 };
                *zoom_override = Some((cur * factor).clamp(0.05, native_zoom * 4.0));
                cx.notify();
            }
            return;
        }
        if let Surface::Editor { pan, .. } = &mut self.surface {
            if shift && dx == 0.0 {
                pan.x -= dy;
            } else {
                pan.x -= dx;
                pan.y -= dy;
            }
            cx.notify();
        }
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.secondary();
        let shift = ks.modifiers.shift;
        let editing = self.editor.is_editing_text();
        if editing && self.editor.text_marked().is_some() {
            return;
        }
        match ks.key.as_str() {
            "escape" if self.text_menu => self.text_menu = false,
            "escape" if self.bg_menu.is_some() => self.bg_menu = None,
            "i" if !editing && self.loupe_target(window).is_some() => self.copy_color(window, cx),
            "escape" if editing => self.editor.finish_text(),
            "escape" if !self.is_capture() && self.editor.clear_tool_or_selection() => {}
            "escape" if self.is_capture() => return self.finish(Finish::Cancel, window, cx),
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
            "[" if !editing && !self.is_capture() => {
                let order = if shift { LayerOrder::ToBack } else { LayerOrder::Backward };
                self.editor.reorder_selected(order);
            }
            "]" if !editing && !self.is_capture() => {
                let order = if shift { LayerOrder::ToFront } else { LayerOrder::Forward };
                self.editor.reorder_selected(order);
            }
            "z" if cmd => {
                self.editor.undo();
            }
            "o" if cmd && !self.is_capture() => {
                let owner = bluesnap_platform::window_ref(window).ok();
                cx.defer(move |cx| crate::editor_window::prompt_open_file(false, owner, cx));
            }
            "0" if cmd && !self.is_capture() => {
                if let Surface::Editor { pan, zoom_override, .. } = &mut self.surface {
                    *zoom_override = None;
                    *pan = Point { x: 0.0, y: 0.0 };
                }
            }
            "=" | "+" if cmd && !self.is_capture() => {
                let (fit, _) = self.fit_zoom(window);
                let max_z = Viewport::native(window.scale_factor()).zoom * 4.0;
                if let Surface::Editor { zoom_override, .. } = &mut self.surface {
                    *zoom_override = Some((zoom_override.unwrap_or(fit) * 1.18).min(max_z));
                }
            }
            "-" if cmd && !self.is_capture() => {
                let (fit, _) = self.fit_zoom(window);
                if let Surface::Editor { zoom_override, .. } = &mut self.surface {
                    *zoom_override = Some((zoom_override.unwrap_or(fit) / 1.18).max(0.05));
                }
            }
            "c" if cmd && !editing => {
                if self.is_capture() {
                    return self.finish(Finish::Copy, window, cx);
                }
                if let Some(image) = self.editor.export() {
                    output::copy_to_clipboard(image, cx);
                    self.show_toast(Toast::new(toast::Kind::Done, "Copied image to clipboard"), window, cx);
                }
            }
            "s" if cmd => return self.finish(Finish::Save, window, cx),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn layer_ops(&self) -> Option<(bool, bool)> {
        if self.is_capture() {
            None
        } else {
            self.editor.can_reorder_selected()
        }
    }

    fn style_panel_size(&self) -> Option<(f32, f32)> {
        let layer_ops = self.layer_ops();
        if let Some((tool, _)) = self.editor.current_style() {
            Some(toolbar::style_bar_size_ex(tool, layer_ops.is_some()))
        } else if !self.is_capture() && self.editor.selected_is_image() && layer_ops.is_some() {
            Some(toolbar::image_bar_size())
        } else {
            None
        }
    }

    fn panel_positions(&self, window: &Window) -> (Point<f32>, Option<Point<f32>>) {
        let style = self.style_panel_size();
        let groups = self.toolbar_groups();
        if self.is_capture() {
            let sel = self.editor.selection().unwrap_or_default();
            let screen = self.viewport.to_window_rect(self.editor.screen());
            let layout = place_panels(
                self.viewport.to_window_rect(sel),
                screen,
                toolbar::panel_size(groups, 0.0),
                style,
                toolbar::tallest_style_bar(),
                PANEL_GAP,
            );
            let at = |r: PxRect| Point { x: r.x, y: r.y };
            (at(layout.toolbar), layout.style_bar.map(at))
        } else {
            let (tw, th) = toolbar::panel_size(groups, 0.0);
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

    fn scene(&self, entity: Entity<Self>, bands: Vec<SceneBand>, window: &Window) -> Scene {
        let e = &self.editor;
        let v = &self.viewport;
        let window_bounds = Bounds::new(point(px(0.), px(0.)), window.viewport_size());
        let (dim, selection_frame, backdrop, canvas_surface, canvas_handles, capture_tiles) = match &self.surface {
            Surface::Capture { screenshot, .. } => {
                let active = e.active_rect().map(|r| v.to_window(r));
                let handles = match e.selection() {
                    Some(sel) if e.selection_mode() == SelectionMode::Pick => bluesnap_core::geom::Handle::ALL
                        .iter()
                        .map(|h| v.point_to_window(sel.handle_point(*h)))
                        .collect(),
                    _ => Vec::new(),
                };
                (Some(active), active.map(|a| (a, handles)), None, None, Vec::new(), screenshot.visible(v, window_bounds))
            }
            Surface::Editor { .. } => {
                let canvas_bg = e.background_color().unwrap_or(bluesnap_core::Color::rgb(0xFFFFFF));
                let handles = e.canvas_handles().into_iter().map(|p| v.point_to_window(p)).collect();
                (None, None, Some(rgb(0xE5E7EB).into()), Some(canvas_bg), handles, Vec::new())
            }
        };
        let top_bar = (!self.is_capture()).then_some(px(self.top_bar));
        Scene {
            entity,
            focus: self.focus.clone(),
            backdrop,
            canvas_surface,
            top_bar,
            canvas_bounds: v.to_window(e.active_canvas_rect()),
            canvas_handles,
            capture_tiles,
            bands,
            clip: e.selection().map(|r| v.to_window(r)),
            dim,
            selection_frame,
            shape_frame: e
                .selected_frame()
                .map(|(r, hs)| (v.to_window(r), hs.into_iter().map(|p| v.point_to_window(p)).collect())),
            text_frame: e.text_frame().map(|r| v.to_window(r)),
            brush_ring: e.brush_ring().map(|b| {
                let z = |d: f32| px(d * v.zoom);
                (v.point_to_window(b.center), size(z(b.half_width), z(b.half_height)), z(b.corner))
            }),
            caret: e.text_caret_rect().map(|r| {
                let color = e.text_ink().unwrap_or(THEME_BLUE);
                (v.to_window(r), hsla(color))
            }),
            editor_cursor: e.cursor(),
            dragging: e.is_dragging(),
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
}

impl Render for CanvasView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.layout(window);
        let bands = self.sync_gpu_resources(window);

        let scene = self.scene(cx.entity(), bands, window);
        let mut root = div()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_modifiers_changed(cx.listener(|this, ev: &ModifiersChangedEvent, _, cx| {
                let before = this.editor.revision();
                this.editor.constrain_changed(ev.modifiers.shift);
                if this.editor.revision() != before {
                    cx.notify();
                }
            }))
            .child(
                canvas(
                    |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    move |bounds, hitbox, window, cx| scene.paint(bounds, hitbox, window, cx),
                )
                .absolute()
                .size_full(),
            );

        // Size badge: shown on Capture selection, or while resizing the Editor canvas.
        let show_size_badge = self.is_capture() || self.editor.is_resizing_canvas();
        if show_size_badge
            && let Some(active) = self.editor.active_rect()
        {
            let label = format!("{} × {}", active.w.round() as i32, active.h.round() as i32);
            let win_size = window.viewport_size();
            let bounds_rect = PxRect::new(0.0, self.top_bar, f32::from(win_size.width), (f32::from(win_size.height) - self.top_bar).max(40.0));
            let at = place_size_label(self.viewport.to_window_rect(active), bounds_rect, (96.0, 20.0), 8.0);
            root = root.child(
                div()
                    .absolute()
                    .left(px(at.x))
                    .top(px(at.y))
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(px(4.0))
                    .bg(rgba(0x0000_00BF))
                    .text_color(white())
                    .text_size(px(12.0))
                    .child(label),
            );
        }

        if let Surface::Capture { permission_url: Some(url), .. } = self.surface {
            root = root.child(permission_banner(url, cx));
        }

        if let Some((cursor, pixel)) = self.loupe_target(window)
            && let Some(doc) = self.editor.capture_document()
        {
            let selection = self.editor.active_rect();
            let loupe = self.loupe.element(doc, pixel, cursor, selection, window.viewport_size(), window);
            root = root.child(loupe);
        }

        if let Some(toast) = &self.toast {
            let win = window.viewport_size();
            let full = Bounds::new(point(px(0.), px(0.)), win);
            let (center_x, top) = match (&self.surface, self.editor.selection()) {
                (Surface::Capture { .. }, Some(sel)) => {
                    let b = self.viewport.to_window(sel).intersect(&full);
                    (f32::from(b.center().x), f32::from(b.origin.y) + 12.0)
                }
                _ => {
                    let (_, style_at) = self.panel_positions(window);
                    let style_bottom = match (style_at, self.style_panel_size()) {
                        (Some(at), Some((_, sh))) => at.y + sh,
                        _ => 0.0,
                    };
                    (f32::from(win.width) / 2.0, self.top_bar.max(style_bottom) + 8.0)
                }
            };
            let entity = cx.entity();
            let close = cx.entity();
            root = root.child(toast::render(
                toast,
                center_x,
                top,
                f32::from(win.width),
                move |action, _, cx| entity.update(cx, |this, cx| this.toast_action(action, cx)),
                move |_, cx| {
                    close.update(cx, |this, cx| {
                        this.toast = None;
                        cx.notify();
                    })
                },
            ));
        }

        if crate::demo::enabled() {
            self.report_layout(window);
        }
        if self.editor.toolbar_visible() {
            let groups = self.toolbar_groups();
            let done_adds_to_editor = self.capture_mode() == Some(CaptureMode::ForEditor);
            let (at, style_at) = self.panel_positions(window);
            let auto = self.auto_state();
            let layer_ops = self.layer_ops();
            root = root.child(toolbar::toolbar(self, groups, done_adds_to_editor, at.x, at.y, cx));
            if let (Some((tool, style)), Some(at)) = (self.editor.current_style(), style_at) {
                root = root.child(match self.editor.current_redaction() {
                    Some(redaction) => toolbar::redact_bar(redaction, style.size, auto, layer_ops, at.x, at.y, cx),
                    None => {
                        let extra = if let Some(numbering) = self.editor.current_counter_format() {
                            toolbar::Extra::Numbering(numbering)
                        } else if let Some(text) = self.editor.current_text_style() {
                            toolbar::Extra::TextStyle(text, self.text_menu)
                        } else if let Some(arrow) = self.editor.current_arrow_style() {
                            toolbar::Extra::ArrowStyle(arrow)
                        } else {
                            toolbar::Extra::None
                        };
                        toolbar::style_bar(tool, style, extra, layer_ops, at.x, at.y, cx)
                    }
                });
                if let Some(current) = self.editor.current_text_style().filter(|_| self.text_menu) {
                    let menu = self.text_menu(current, style.color, at, window, cx);
                    root = root.child(menu);
                } else {
                    self.text_menu = false;
                }
            } else if let (true, Some(ops), Some(at)) = (self.editor.selected_is_image(), layer_ops, style_at) {
                self.text_menu = false;
                root = root.child(toolbar::image_bar(ops, at.x, at.y, cx));
            } else {
                self.text_menu = false;
            }
        }
        if let (Some(anchor), Some(current_bg)) = (self.bg_menu, self.editor.background_color()) {
            let (mx, my) = self.bg_menu_origin(anchor, window);
            let has_custom = bluesnap_platform::Native::HAS_COLOR_DIALOG;
            root = root.child(toolbar::background_menu(current_bg, has_custom, mx, my, cx));
        }
        root
    }
}

enum SceneBand {
    Image(Vec<(Bounds<Pixels>, Arc<RenderImage>)>),
    Layer(Bounds<Pixels>, Arc<RenderImage>),
}

struct Scene {
    entity: Entity<CanvasView>,
    focus: FocusHandle,
    backdrop: Option<Hsla>,
    canvas_surface: Option<bluesnap_core::Color>,
    top_bar: Option<Pixels>,
    canvas_bounds: Bounds<Pixels>,
    canvas_handles: Vec<Point<Pixels>>,
    capture_tiles: Vec<(Bounds<Pixels>, Arc<RenderImage>)>,
    bands: Vec<SceneBand>,
    clip: Option<Bounds<Pixels>>,
    dim: Option<Option<Bounds<Pixels>>>,
    selection_frame: Option<(Bounds<Pixels>, Vec<Point<Pixels>>)>,
    shape_frame: Option<(Bounds<Pixels>, Vec<Point<Pixels>>)>,
    text_frame: Option<Bounds<Pixels>>,
    brush_ring: Option<(Point<Pixels>, gpui::Size<Pixels>, Pixels)>,
    caret: Option<(Bounds<Pixels>, Hsla)>,
    editor_cursor: CursorKind,
    dragging: bool,
    cursor: CursorStyle,
    editing_text: bool,
}

impl Scene {
    fn paint(self, bounds: Bounds<Pixels>, hitbox: Hitbox, window: &mut Window, cx: &mut App) {
        let blue = hsla(THEME_BLUE);
        if let Some(backdrop) = self.backdrop {
            window.paint_quad(fill(bounds, backdrop));
            if let Some(bg) = self.canvas_surface {
                if bg.is_transparent() {
                    window.paint_quad(fill(self.canvas_bounds, white()));
                    let vis = self.canvas_bounds.intersect(&bounds);
                    if vis.size.width > px(0.) && vis.size.height > px(0.) {
                        window.with_content_mask(Some(ContentMask { bounds: vis }), |window| {
                            let cell = 14.0f32;
                            let ox = f32::from(self.canvas_bounds.origin.x);
                            let oy = f32::from(self.canvas_bounds.origin.y);
                            let c0 = ((f32::from(vis.left()) - ox) / cell).floor().max(0.0) as i32;
                            let c1 = ((f32::from(vis.right()) - ox) / cell).ceil().max(0.0) as i32;
                            let r0 = ((f32::from(vis.top()) - oy) / cell).floor().max(0.0) as i32;
                            let r1 = ((f32::from(vis.bottom()) - oy) / cell).ceil().max(0.0) as i32;
                            let checker: Hsla = rgb(0xCBD5E1).into();
                            for r in r0..r1 {
                                for c in c0..c1 {
                                    if (r + c) % 2 == 0 {
                                        let q = Bounds::new(
                                            point(px(ox + c as f32 * cell), px(oy + r as f32 * cell)),
                                            size(px(cell), px(cell)),
                                        );
                                        window.paint_quad(fill(q, checker));
                                    }
                                }
                            }
                        });
                    }
                } else {
                    window.paint_quad(fill(self.canvas_bounds, hsla(bg)));
                }
            }
            let frame = self.canvas_bounds.dilate(px(1.));
            window.paint_quad(quad(frame, px(0.), transparent_black(), px(1.), rgba(0x0000_0029), BorderStyle::Solid));
        }

        if !self.capture_tiles.is_empty() {
            paint_tiles(self.capture_tiles, window);
        }

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
                for band in self.bands {
                    match band {
                        SceneBand::Image(tiles) => paint_tiles(tiles, window),
                        SceneBand::Layer(b, image) => {
                            window.paint_image(b, Corners::default(), image, 0, false).ok();
                        }
                    }
                }
            });
        }

        // Outer canvas resize handles in the Composition Editor.
        for p in &self.canvas_handles {
            let b = Bounds::centered_at(*p, size(px(8.), px(8.)));
            window.paint_quad(quad(b, px(2.), white(), px(1.5), rgba(0x6474_8BCC), BorderStyle::Solid));
        }

        if let Some((center, half, corner)) = self.brush_ring.filter(|_| hitbox.is_hovered(window)) {
            let ring = Bounds::centered_at(center, size(half.width * 2., half.height * 2.));
            window.paint_quad(quad(ring.dilate(px(1.)), corner + px(1.), transparent_black(), px(3.5), rgba(0x0000_0059), BorderStyle::Solid));
            window.paint_quad(quad(ring, corner, transparent_black(), px(1.5), white(), BorderStyle::Solid));
        }

        if let Some((region, handles)) = &self.selection_frame {
            let border = region.dilate(px(BORDER));
            window.paint_quad(quad(border, px(0.), transparent_black(), px(BORDER), blue, BorderStyle::Solid));
            for p in handles {
                let r = px(HANDLE_RADIUS);
                let b = Bounds::centered_at(*p, size(r * 2., r * 2.));
                window.paint_quad(quad(b, r, blue, px(1.5), white(), BorderStyle::Solid));
            }
        }

        if let Some((frame, handles)) = &self.shape_frame {
            window.paint_quad(quad(*frame, px(0.), transparent_black(), px(1.5), blue, BorderStyle::Dashed));
            for p in handles {
                let b = Bounds::centered_at(*p, size(px(7.), px(7.)));
                window.paint_quad(quad(b, px(1.), white(), px(1.5), blue, BorderStyle::Solid));
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

        let in_top_bar = self.top_bar.is_some_and(|h| window.mouse_position().y < h) && !self.dragging;
        let effective_kind = if in_top_bar { CursorKind::Default } else { self.editor_cursor };
        let effective_style = if in_top_bar { CursorStyle::Arrow } else { self.cursor };
        window.set_cursor_style(effective_style, &hitbox);
        if let Ok(win_ref) = bluesnap_platform::window_ref(window) {
            let native_cursor = (hitbox.is_hovered(window) || self.dragging).then_some(effective_kind);
            bluesnap_platform::native().set_canvas_cursor(win_ref, native_cursor);
        }
        if self.editing_text {
            window.handle_input(&self.focus, ElementInputHandler::new(bounds, self.entity.clone()), cx);
        }

        let entity = self.entity.clone();
        let pressed = hitbox.clone();
        window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && pressed.is_hovered(window) {
                entity.update(cx, |this, cx| this.mouse_down(ev, window, cx));
            }
        });
        let entity = self.entity.clone();
        let wheel_hitbox = hitbox.clone();
        window.on_mouse_event(move |ev: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && wheel_hitbox.is_hovered(window) {
                entity.update(cx, |this, cx| this.scroll_wheel(ev, window, cx));
            }
        });
        let entity = self.entity.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                let hovered = hitbox.is_hovered(window);
                entity.update(cx, |this, cx| this.mouse_move(ev, hovered, cx));
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

impl EntityInputHandler for CanvasView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.editor.text_content()?;
        let bytes = range_from_utf16(text, &range);
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
        let range = range.map(|r| range_from_utf16(content, &r));
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
        let range = range.map(|r| range_from_utf16(content, &r));
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
        assert_eq!(utf16_to_byte(s, 0), 0);
        assert_eq!(utf16_to_byte(s, 2), 4);
        assert_eq!(utf16_to_byte(s, 4), 8);
        assert_eq!(utf16_to_byte(s, 99), s.len());
        assert_eq!(byte_to_utf16(s, 8), 4);
        assert_eq!(range_to_utf16(s, &(1..8)), 1..4);
        #[allow(clippy::reversed_empty_ranges)]
        let reversed = 4..1;
        assert_eq!(range_from_utf16(s, &reversed), 1..8);
    }
}
