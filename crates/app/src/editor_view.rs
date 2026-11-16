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
    IntoElement, KeyDownEvent, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Point, Render, RenderImage, ScrollWheelEvent, Styled, Task, UTF16Selection, Window, canvas, div,
    fill, point, px, quad, rgb, rgba, size, transparent_black, white,
};

use crate::gpu_image::{ImageTiles, LayerCache, paint_tiles};
use crate::output::{self, Finish};
use crate::toast::{self, Toast};
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
    /// Progress and results of text recognition and AI redaction.
    toast: Option<Toast>,
    /// Running text recognition, if any. Dropping it cancels the UI update.
    ocr: Option<Task<()>>,
    /// Running AI redaction, if any. Dropping it cancels the UI update.
    auto: Option<Task<()>>,
    /// The magnifier beside the cursor while picking a region.
    loupe: crate::loupe::Loupe,
    /// The text style menu is open.
    text_menu: bool,
    /// Its previews, for the text color and display scale they were drawn in.
    text_previews: Option<(bluesnap_core::Color, f32, Vec<toolbar::TextPreview>)>,
    /// While the redact strength slider is dragged: its style bar's window x.
    slider: Option<f32>,
}

fn color(c: bluesnap_core::Color) -> Hsla {
    rgb(c.to_hex()).into()
}

/// An error's message, short enough for a toast.
fn short_error(e: &anyhow::Error) -> String {
    truncate(&format!("{e:#}"), 200)
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// "1 line", "5 lines".
fn plural(n: usize, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

/// The servers AI requests go to, for telling the user.
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
            toast: None,
            ocr: None,
            auto: None,
            loupe: Default::default(),
            text_menu: false,
            text_previews: None,
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

    /// Closes the overlay and starts capturing the selection while the user
    /// scrolls. Annotations come along: the result starts with the selection,
    /// so they stay on the content they were made on.
    fn start_scrolling_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Surface::Capture { display, .. } = &self.surface else { return };
        let Some(sel) = self.editor.selection() else { return };
        let sel = sel.round();
        let rect = bluesnap_platform::PixelRect { x: sel.x as i32, y: sel.y as i32, width: sel.w as i32, height: sel.h as i32 };
        let screen = self.editor.screen();
        let (display, screen) = (display.clone(), (screen.w as i32, screen.h as i32));
        let shapes = self.editor.shapes_in_selection();
        self.finish(Finish::Cancel, window, cx);
        cx.defer(move |cx| {
            if let Err(e) = crate::scrolling::start(display, screen, rect, shapes, cx) {
                log::error!("could not start scrolling capture: {e:#}");
            }
        });
    }

    pub fn is_recognizing(&self) -> bool {
        self.ocr.is_some()
    }

    /// Shows `toast` until its lifetime ends or another replaces it.
    fn show_toast(&mut self, toast: Toast, window: &mut Window, cx: &mut Context<Self>) {
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
                let above_overlay = matches!(self.surface, Surface::Capture { .. });
                cx.defer(move |cx| crate::ai_settings::open(above_overlay, cx));
            }
        }
        cx.notify();
    }

    /// Recognizes the text in the selection (as displayed, so redacted text stays
    /// hidden), copies it to the clipboard and closes the view.
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
                log::info!("copied {} characters of recognized text", text.chars().count());
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.finished = true;
                window.remove_window();
                return;
            }
            Ok(_) => Toast::new(toast::Kind::Info, "No text found").detail("Select an area with text, then try again."),
            Err(e) => {
                log::error!("text recognition failed: {e:#}");
                Toast::new(toast::Kind::Error, "Text recognition failed").detail(short_error(&e))
            }
        };
        self.show_toast(toast, window, cx);
    }

    /// Where the loupe goes, and the image pixel under the cursor: shown while
    /// picking a region on the capture overlay, gone once it's selected.
    fn loupe_target(&self, window: &Window) -> Option<(Point<Pixels>, (i32, i32))> {
        if !matches!(self.surface, Surface::Capture { .. }) || !self.editor.is_picking() {
            return None;
        }
        let cursor = window.mouse_position();
        let p = self.viewport.to_image(cursor);
        Some((cursor, (p.x.floor() as i32, p.y.floor() as i32)))
    }

    /// Copies the color under the cursor as `#RRGGBB` (the I key while picking).
    fn copy_color(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some((_, (x, y))) = self.loupe_target(window) else { return };
        let Some(color) = self.editor.document().color_at(x, y) else { return };
        let hex = crate::loupe::hex(color);
        log::info!("copied color {hex}");
        cx.write_to_clipboard(ClipboardItem::new_string(hex));
        self.loupe.copied();
        // Back to the hint once "Copied" has shown.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(crate::loupe::COPIED_FOR).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    /// Demo mode: where the controls are (see [`crate::demo`]).
    fn report_layout(&self, window: &Window) {
        let mut items = Vec::new();
        let (toolbar_at, style_at) = self.panel_positions(window);
        let groups = match self.surface {
            Surface::Capture { .. } => toolbar::CAPTURE_TOOLBAR,
            Surface::Document => toolbar::DOCUMENT_TOOLBAR,
        };
        if self.editor.toolbar_visible() {
            items.extend(toolbar::row_items(&toolbar::toolbar_slots(groups, 0.0), 32.0, (toolbar_at.x, toolbar_at.y)));
        }
        if let (true, Some(at), Some((tool, _))) = (self.editor.toolbar_visible(), style_at, self.editor.current_style()) {
            let style = toolbar::row_items(&toolbar::style_slots(tool), 24.0, (at.x, at.y));
            items.extend(style.into_iter().map(|(name, r)| (format!("style:{name}"), r)));
            if self.text_menu {
                let (x, y, w) = self.text_menu_origin(at, window);
                for (i, style) in bluesnap_core::TextStyle::ALL.into_iter().enumerate() {
                    let row = toolbar::text_style_menu_row(i);
                    items.push((format!("menu:{style:?}"), (x + row.0, y + row.1, w - 2.0 * row.0, row.3)));
                }
            }
        }
        if let Some(sel) = self.editor.selection() {
            let r = self.viewport.to_window_rect(sel);
            items.push(("selection".into(), (r.x, r.y, r.w, r.h)));
        }
        let name = match self.surface {
            Surface::Capture { .. } => "capture",
            Surface::Document => "document",
        };
        crate::demo::report(name, window.scale_factor(), items);
    }

    pub fn toggle_text_styles(&mut self) {
        self.text_menu = !self.text_menu;
    }

    pub fn choose_text_style(&mut self, style: bluesnap_core::TextStyle) {
        self.editor.set_text_style(style);
        self.text_menu = false;
    }

    /// The text style menu under the style bar's style button (above the bar
    /// when there's no room below).
    fn text_menu(
        &mut self,
        current: bluesnap_core::TextStyle,
        color: bluesnap_core::Color,
        bar: Point<f32>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let scale = window.scale_factor();
        if !self.text_previews.as_ref().is_some_and(|(c, s, _)| *c == color && *s == scale) {
            let fonts = cx.global::<AppState>().fonts.clone();
            let previews = bluesnap_core::TextStyle::ALL
                .into_iter()
                .filter_map(|style| {
                    // Drawn at device resolution, shown at 15 logical px.
                    let sample = bluesnap_core::render::text_sample(&fonts, style.label(), style, color, 15.0 * scale)?;
                    let size = (sample.width() as f32 / scale, sample.height() as f32 / scale);
                    Some((style, crate::gpu_image::pixmap_image(&sample)?, size))
                })
                .collect();
            self.text_previews = Some((color, scale, previews));
        }
        let previews = self.text_previews.as_ref().expect("just made").2.clone();
        let (x, y, _) = self.text_menu_origin(bar, window);
        toolbar::text_style_menu(&previews, current, color.is_light(), x, y, cx)
    }

    /// The text style menu's top-left corner and width, for a style bar at `bar`.
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

    /// The redact bar's Auto button, and whether it's running.
    fn auto_state(&self) -> toolbar::Auto {
        if self.auto.is_some() { toolbar::Auto::Running } else { toolbar::Auto::Ready }
    }

    /// Finds sensitive text in the selection (as displayed, so what's already
    /// redacted isn't found again) and covers it with area redactions in the
    /// current effect, as one undo step. Text is recognized locally; only the
    /// text goes to the configured AI endpoints. Each stage shows in a toast.
    pub fn auto_redact(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.auto.is_some() {
            return;
        }
        let settings = cx.global::<AppState>().ai.clone();
        if !settings.is_configured() {
            let toast = Toast::new(toast::Kind::Info, "Set up AI first")
                .detail("Auto finds sensitive text with an LLM or a decision model. Add one in AI Settings; only recognized text is sent, never the image.")
                .action("AI Settings…", toast::Action::OpenAiSettings);
            return self.show_toast(toast, window, cx);
        }
        let (Some(sel), Some(pixmap)) = (self.editor.selection(), self.editor.export()) else { return };
        let origin = bluesnap_core::Point::new(sel.round().x, sel.round().y);
        let fonts = cx.global::<AppState>().fonts.clone();
        let scale = self.editor.scale();
        self.show_toast(Toast::new(toast::Kind::Busy, "Reading the text…"), window, cx);
        let started = std::time::Instant::now();
        self.auto = Some(cx.spawn_in(window, async move |this, cx| {
            // Tall images (scrolling screenshots) are read in bands: memory
            // stays bounded, the toast shows progress, and each band's text is
            // checked by the AI while the next band is being read.
            let image = Arc::new(pixmap);
            let bands = bluesnap_core::reading_order::bands(image.height(), 1200, 64);
            let mut checks = Vec::new();
            let mut count = 0;
            for (i, (read, own)) in bands.iter().cloned().enumerate() {
                if bands.len() > 1 {
                    let progress = Toast::new(toast::Kind::Busy, format!("Reading the text… {} of {}", i + 1, bands.len()));
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
                let lines: Vec<(PxRect, String)> = match recognized {
                    Ok(lines) => lines
                        .into_iter()
                        .map(|l| {
                            let b = l.bounds;
                            (PxRect::new(b.x as f32, b.y as f32 + read.start as f32, b.width as f32, b.height as f32), l.text)
                        })
                        // A line read twice in an overlap belongs to one band.
                        .filter(|(r, _)| own.contains(&(r.center().y.max(0.0) as u32)))
                        .collect(),
                    Err(e) => {
                        log::error!("auto redact: text recognition failed: {e:#}");
                        let toast = Toast::new(toast::Kind::Error, "Couldn't read the text").detail(short_error(&e));
                        this.update_in(cx, |this, window, cx| this.auto_finished(toast, window, cx)).ok();
                        return;
                    }
                };
                if lines.is_empty() {
                    continue;
                }
                count += lines.len();
                let (settings, image, fonts) = (settings.clone(), image.clone(), fonts.clone());
                checks.push(cx.background_executor().spawn(async move {
                    bluesnap_ai::locate_sensitive(&settings, &image, &lines, fonts.sans())
                }));
            }
            if count == 0 {
                let toast = Toast::new(toast::Kind::Info, "No text found").detail("There's nothing to redact in the selection.");
                this.update_in(cx, |this, window, cx| this.auto_finished(toast, window, cx)).ok();
                return;
            }
            let ocr_time = started.elapsed();
            let checking = Toast::new(toast::Kind::Busy, format!("Checking {} with AI…", plural(count, "line")))
                .detail(format!("Sending the text, not the image, to {}", hosts(&settings)));
            this.update_in(cx, |this, window, cx| this.show_toast(checking, window, cx)).ok();

            // All bands' results, or the first error if every band failed.
            let mut found: anyhow::Result<(Vec<PxRect>, bluesnap_ai::Report)> = Ok((Vec::new(), bluesnap_ai::Report::default()));
            let mut errors = Vec::new();
            for check in checks {
                match (check.await, &mut found) {
                    (Ok((rects, report)), Ok((all, total))) => {
                        all.extend(rects);
                        total.failed += report.failed;
                        total.requests += report.requests;
                        total.first_error = total.first_error.take().or(report.first_error);
                    }
                    (Err(e), _) => errors.push(e),
                    _ => {}
                }
            }
            if let Some(e) = errors.into_iter().next() {
                match &mut found {
                    // Some bands failed entirely: count them as failed requests.
                    Ok((all, total)) if !all.is_empty() || total.requests > 0 => {
                        total.failed += 1;
                        total.first_error = total.first_error.take().or(Some(format!("{e:#}")));
                    }
                    _ => found = Err(e),
                }
            }
            log::info!("auto redact: {count} lines, OCR {ocr_time:?}, total {:?}", started.elapsed());
            this.update_in(cx, |this, window, cx| {
                let toast = match found {
                    Ok((rects, report)) => {
                        // Found in the selection's pixels; placed on the screenshot.
                        let rects: Vec<PxRect> = rects.into_iter().map(|r| r.translate(origin)).collect();
                        let added = this.editor.add_redactions(&rects);
                        let toast = match added {
                            0 => Toast::new(toast::Kind::Done, "Nothing sensitive found")
                                .detail(format!("Checked {}.", plural(count, "line"))),
                            n => Toast::new(toast::Kind::Done, format!("Redacted {}", plural(n, "item")))
                                .detail("Undo removes them all; click one to change or delete it."),
                        };
                        // Some lines went unchecked: say so rather than imply all is covered.
                        match (report.failed, report.first_error) {
                            (0, _) | (_, None) => toast,
                            (failed, Some(e)) => Toast::new(toast::Kind::Error, format!("{} · {} failed", toast.title, plural(failed, "request")))
                                .detail(format!("Some text wasn't checked: {}", truncate(&e, 160)))
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
        if std::mem::take(&mut self.text_menu) {
            return cx.notify(); // a click outside the menu just closes it
        }
        if self.toast.as_ref().is_some_and(Toast::dismissable) {
            self.toast = None;
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
        if self.editor.is_picking() && matches!(self.surface, Surface::Capture { .. }) {
            cx.notify(); // the loupe follows the cursor
        }
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
            "escape" if self.text_menu => self.text_menu = false,
            "i" if !editing && self.loupe_target(window).is_some() => self.copy_color(window, cx),
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
            brush_ring: e.brush_ring().map(|b| {
                let z = |d: f32| px(d * v.zoom);
                (v.point_to_window(b.center), size(z(b.half_width), z(b.half_height)), z(b.corner))
            }),
            caret: e.text_caret_rect().map(|r| {
                let color = e.text_ink().unwrap_or(THEME_BLUE);
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

        if let Some((cursor, pixel)) = self.loupe_target(window) {
            let (doc, selection) = (self.editor.document(), self.editor.active_rect());
            let loupe = self.loupe.element(doc, pixel, cursor, selection, window.viewport_size(), window);
            root = root.child(loupe);
        }

        if let Some(toast) = &self.toast {
            // At the top of the visible selection (the document window: under its toolbar).
            let win = window.viewport_size();
            let full = Bounds::new(point(px(0.), px(0.)), win);
            let (center_x, top) = match (&self.surface, self.editor.selection()) {
                (Surface::Capture { .. }, Some(sel)) => {
                    let b = self.viewport.to_window(sel).intersect(&full);
                    (f32::from(b.center().x), f32::from(b.origin.y) + 12.0)
                }
                _ => {
                    // Under the toolbar and the style bar, which wraps below
                    // it in a narrow window.
                    let (_, style_at) = self.panel_positions(window);
                    let style_bottom = match (style_at, self.editor.current_style()) {
                        (Some(at), Some((tool, _))) => at.y + toolbar::style_bar_size(tool).1,
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
            let groups = match self.surface {
                Surface::Capture { .. } => toolbar::CAPTURE_TOOLBAR,
                Surface::Document => toolbar::DOCUMENT_TOOLBAR,
            };
            let (at, style_at) = self.panel_positions(window);
            let auto = self.auto_state();
            root = root.child(toolbar::toolbar(self, groups, at.x, at.y, cx));
            if let (Some((tool, style)), Some(at)) = (self.editor.current_style(), style_at) {
                root = root.child(match self.editor.current_redaction() {
                    Some(redaction) => toolbar::redact_bar(redaction, style.size, auto, at.x, at.y, cx),
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
                        toolbar::style_bar(tool, style.size, style.color, extra, at.x, at.y, cx)
                    }
                });
                if let Some(current) = self.editor.current_text_style().filter(|_| self.text_menu) {
                    let menu = self.text_menu(current, style.color, at, window, cx);
                    root = root.child(menu);
                } else {
                    self.text_menu = false;
                }
            } else {
                self.text_menu = false;
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
    /// A brush's outline at the pointer: center, half size, corner radius.
    brush_ring: Option<(Point<Pixels>, gpui::Size<Pixels>, Pixels)>,
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
        if let Some((center, half, corner)) = self.brush_ring.filter(|_| hitbox.is_hovered(window)) {
            let ring = Bounds::centered_at(center, size(half.width * 2., half.height * 2.));
            // A white ring between two dark ones shows on any background.
            window.paint_quad(quad(ring.dilate(px(1.)), corner + px(1.), transparent_black(), px(3.5), rgba(0x0000_0059), BorderStyle::Solid));
            window.paint_quad(quad(ring, corner, transparent_black(), px(1.5), white(), BorderStyle::Solid));
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
