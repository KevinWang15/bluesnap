//! The full-screen capture overlay: draws the frozen screenshot and the editor
//! state with gpui, and forwards input to [`bluesnap_core::Editor`].
//!
//! The editor works in screenshot pixels; gpui works in logical pixels. `scale`
//! converts between them.

use std::ops::Range;
use std::sync::Arc;

use bluesnap_core::layout::{place_panels, place_size_label};
use bluesnap_core::reading_order::join_lines;
use bluesnap_core::render::{to_bgra_straight, to_rgba_straight};
use bluesnap_core::{CursorKind, Editor, Pixmap, Point as Px, Rect as PxRect, RightClick, THEME_BLUE, TextKey};
use bluesnap_platform::Platform;
use gpui::{
    App, Bounds, BorderStyle, ClipboardItem, Context, Corners, CursorStyle, DispatchPhase, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Hitbox, HitboxBehavior, Hsla, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, RenderImage,
    SharedString, Styled, Task, UTF16Selection, Window, canvas, div, fill, point, px, quad, rgb, rgba, size, transparent_black, white,
};

use crate::{AppState, output, toolbar};

/// Selection border width, in logical pixels.
const BORDER: f32 = 2.0;
const HANDLE_RADIUS: f32 = 4.0;
/// Gap between the selection and the floating panels, in logical pixels.
const PANEL_GAP: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    Cancel,
    Copy,
    Save,
}

pub struct Overlay {
    editor: Editor,
    focus: FocusHandle,
    background: Arc<RenderImage>,
    /// Annotations rendered for the current editor revision.
    layer: Option<(PxRect, Arc<RenderImage>)>,
    layer_revision: Option<u64>,
    /// Screenshot pixels per logical pixel.
    scale: f32,
    finished: bool,
    /// Message shown over the selection (text recognition progress/result).
    status: Option<SharedString>,
    /// Running text recognition, if any. Dropping it cancels the UI update.
    ocr: Option<Task<()>>,
}

/// Opaque RGBA screenshot pixels as a gpui image (which wants BGRA).
pub fn background_image(width: u32, height: u32, rgba: &[u8]) -> Option<RenderImage> {
    let mut bgra = rgba.to_vec();
    for px in bgra.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width, height, bgra)?;
    Some(RenderImage::new(vec![image::Frame::new(buffer)]))
}

fn pixmap_image(pixmap: &Pixmap) -> Option<RenderImage> {
    let buffer = image::RgbaImage::from_raw(pixmap.width(), pixmap.height(), to_bgra_straight(pixmap))?;
    Some(RenderImage::new(vec![image::Frame::new(buffer)]))
}

fn color(c: bluesnap_core::Color) -> Hsla {
    rgb(c.to_hex()).into()
}

impl Overlay {
    pub fn new(editor: Editor, background: Arc<RenderImage>, cx: &mut Context<Self>) -> Self {
        // However the window goes away (including OS-initiated closes), allow the next capture.
        cx.on_release(|_, cx| {
            log::debug!("overlay closed");
            cx.global_mut::<AppState>().overlay = None;
        })
        .detach();
        let scale = editor.scale();
        Self {
            editor,
            focus: cx.focus_handle(),
            background,
            layer: None,
            layer_revision: None,
            scale,
            finished: false,
            status: None,
            ocr: None,
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

    fn to_px(&self, p: Point<Pixels>) -> Px {
        Px::new(f32::from(p.x) * self.scale, f32::from(p.y) * self.scale)
    }

    fn to_logical(&self, r: PxRect) -> Bounds<Pixels> {
        let s = self.scale;
        Bounds::new(point(px(r.x / s), px(r.y / s)), size(px(r.w / s), px(r.h / s)))
    }

    fn logical_rect(&self, r: PxRect) -> PxRect {
        let s = self.scale;
        PxRect::new(r.x / s, r.y / s, r.w / s, r.h / s)
    }

    /// Re-rasterizes the annotation layer when the editor changed since last frame.
    fn sync_layer(&mut self, window: &mut Window) {
        let revision = self.editor.revision();
        if self.layer_revision == Some(revision) {
            return;
        }
        self.layer_revision = Some(revision);
        let new = self
            .editor
            .render_layer()
            .and_then(|(rect, pixmap)| Some((rect, Arc::new(pixmap_image(&pixmap)?))));
        if let Some((_, old)) = std::mem::replace(&mut self.layer, new) {
            // Each layer is a new texture; free the previous one from the atlas.
            window.drop_image(old).ok();
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
        match (how, image) {
            (Finish::Copy, Some(image)) => output::copy_to_clipboard(image, cx),
            (Finish::Save, Some(image)) => {
                let dir = cx.global::<AppState>().config.save_dir();
                output::save_with_dialog(image, dir, cx);
            }
            _ => {}
        }
    }

    pub fn is_recognizing(&self) -> bool {
        self.ocr.is_some()
    }

    /// Recognizes the text in the selection (as displayed, so mosaicked text stays
    /// hidden), copies it to the clipboard and closes the overlay.
    pub fn extract_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ocr.is_some() {
            return;
        }
        let Some(pixmap) = self.editor.export() else { return };
        let image = bluesnap_platform::Image { width: pixmap.width(), height: pixmap.height(), rgba: to_rgba_straight(&pixmap) };
        self.status = Some("Recognizing text…".into());
        let started = std::time::Instant::now();
        let recognized = cx
            .background_executor()
            .spawn(async move { bluesnap_platform::native().recognize_text(&image) });
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

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.ocr.is_none() {
            self.status = None;
        }
        let p = self.to_px(ev.position);
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
        let (revision, cursor) = (self.editor.revision(), self.editor.cursor());
        self.editor.pointer_move(self.to_px(ev.position), ev.modifiers.shift);
        if self.editor.revision() != revision || self.editor.cursor() != cursor {
            cx.notify();
        }
    }

    fn mouse_up(&mut self, ev: &MouseUpEvent, cx: &mut Context<Self>) {
        if ev.button == MouseButton::Left {
            self.editor.pointer_up(self.to_px(ev.position));
            cx.notify();
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

    fn scene(&self, entity: Entity<Self>) -> Scene {
        let e = &self.editor;
        let handles = match e.selection() {
            Some(sel) if e.toolbar_visible() || !e.is_picking() => {
                bluesnap_core::geom::Handle::ALL.iter().map(|h| sel.handle_point(*h)).collect()
            }
            _ => Vec::new(),
        };
        let to_point = |p: Px| point(px(p.x / self.scale), px(p.y / self.scale));
        Scene {
            entity,
            focus: self.focus.clone(),
            background: self.background.clone(),
            layer: self.layer.as_ref().map(|(r, img)| (self.to_logical(*r), img.clone())),
            active: e.active_rect().map(|r| self.to_logical(r)),
            handles: handles.into_iter().map(to_point).collect(),
            shape_frame: e
                .selected_frame()
                .map(|(r, hs)| (self.to_logical(r), hs.into_iter().map(to_point).collect())),
            text_frame: e.text_frame().map(|r| self.to_logical(r)),
            caret: e.text_caret_rect().map(|r| {
                let color = e.current_style().map_or(THEME_BLUE, |(_, s)| s.color);
                (self.to_logical(r), self::color(color))
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
}

impl Render for Overlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        self.scale = self.editor.screen().w / f32::from(viewport.width).max(1.0);
        self.sync_layer(window);

        let scene = self.scene(cx.entity());
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

        let screen = self.logical_rect(self.editor.screen());
        if let Some(active) = self.editor.active_rect() {
            let label = format!("{} × {}", active.w.round() as i32, active.h.round() as i32);
            let at = place_size_label(self.logical_rect(active), screen, (90.0, 20.0), 6.0);
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

        if let (Some(status), Some(sel)) = (&self.status, self.editor.selection()) {
            let b = self.to_logical(sel);
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

        if let (true, Some(sel)) = (self.editor.toolbar_visible(), self.editor.selection()) {
            let style = self.editor.current_style();
            let layout = place_panels(
                self.logical_rect(sel),
                screen,
                toolbar::toolbar_size(),
                style.map(|(tool, _)| toolbar::style_bar_size(tool)),
                PANEL_GAP,
            );
            root = root.child(toolbar::toolbar(self, layout.toolbar.x, layout.toolbar.y, cx));
            if let (Some((tool, style)), Some(at)) = (style, layout.style_bar) {
                root = root.child(toolbar::style_bar(tool, style.size, style.color, at.x, at.y, cx));
            }
        }
        root
    }
}

/// Everything the canvas paints, captured in logical pixels.
struct Scene {
    entity: Entity<Overlay>,
    focus: FocusHandle,
    background: Arc<RenderImage>,
    layer: Option<(Bounds<Pixels>, Arc<RenderImage>)>,
    active: Option<Bounds<Pixels>>,
    handles: Vec<Point<Pixels>>,
    shape_frame: Option<(Bounds<Pixels>, Vec<Point<Pixels>>)>,
    text_frame: Option<Bounds<Pixels>>,
    caret: Option<(Bounds<Pixels>, Hsla)>,
    cursor: CursorStyle,
    editing_text: bool,
}

impl Scene {
    fn paint(self, bounds: Bounds<Pixels>, hitbox: Hitbox, window: &mut Window, cx: &mut App) {
        let blue = color(THEME_BLUE);
        window.paint_image(bounds, Corners::default(), self.background.clone(), 0, false).ok();

        // Dim everything outside the highlighted region.
        let dim = rgba(0x0000_0073);
        match self.active {
            Some(a) => {
                let (l, t, r, b) = (a.left(), a.top(), a.right(), a.bottom());
                let (w, h) = (bounds.size.width, bounds.size.height);
                for (x0, y0, x1, y1) in [(px(0.), px(0.), w, t), (px(0.), b, w, h), (px(0.), t, l, b), (r, t, w, b)] {
                    if x1 > x0 && y1 > y0 {
                        window.paint_quad(fill(Bounds::from_corners(point(x0, y0), point(x1, y1)), dim));
                    }
                }
            }
            None => window.paint_quad(fill(bounds, dim)),
        }

        if let Some((b, image)) = self.layer {
            window.paint_image(b, Corners::default(), image, 0, false).ok();
        }

        if let Some(a) = self.active {
            // Outside the region, so the border never covers captured pixels.
            let border = a.dilate(px(BORDER));
            window.paint_quad(quad(border, px(0.), transparent_black(), px(BORDER), blue, BorderStyle::Solid));
        }
        for p in &self.handles {
            let r = px(HANDLE_RADIUS);
            let b = Bounds::centered_at(*p, size(r * 2., r * 2.));
            window.paint_quad(quad(b, r, blue, px(1.5), white(), BorderStyle::Solid));
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

        window.set_cursor_style(self.cursor, &hitbox);
        if self.editing_text {
            window.handle_input(&self.focus, ElementInputHandler::new(bounds, self.entity.clone()), cx);
        }

        // Presses only count on the canvas (panels above it occlude it); moves and
        // releases are tracked everywhere so drags keep working over the panels.
        let entity = self.entity.clone();
        window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                entity.update(cx, |this, cx| this.mouse_down(ev, window, cx));
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

impl EntityInputHandler for Overlay {
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
        let b = self.to_logical(caret);
        Some(Bounds::new(element.origin + b.origin, b.size))
    }

    fn character_index_for_point(&mut self, p: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        let index = self.editor.text_index_at(self.to_px(p))?;
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
