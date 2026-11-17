//! Scrolling screenshots: after the user picks a region and presses the toolbar
//! button, the overlay closes so the real app underneath can be scrolled. The
//! screen stays dimmed except for a click-through hole over the region, a live
//! preview beside the selection grows upward as content is stitched, and a small
//! bar shows status and actions.

use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bluesnap_core::layout::place_scrolling_preview;
use bluesnap_core::pixels::split_bgrx;
use bluesnap_core::shape::Shape;
use bluesnap_core::stitch::{Progress, Stitcher};
use bluesnap_core::{Color, Composition, Rect as PxRect, THEME_BLUE};
use bluesnap_platform::{DisplayInfo, PixelRect, Platform};
use global_hotkey::GlobalHotKeyManager;
use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ObjectFit, ParentElement, Render, RenderImage, SharedString, Styled, StyledImage,
    Task, WeakEntity, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div,
    point, prelude::FluentBuilder, px, rgba, size, svg,
};

use crate::AppState;
use crate::capture::{CaptureMode, CapturedSlice};
use crate::output::{self, Finish};
use crate::toolbar::{self, Action, hsla};

/// Time between captures. Fast enough that normal wheel/trackpad scrolling keeps an overlap.
const INTERVAL: Duration = Duration::from_millis(60);
/// Delay before the first capture so the closing selection overlay leaves the screen.
const SETTLE_SLICE: Duration = Duration::from_millis(50);
const SETTLE_SLICES: usize = 4;
/// Border around the hole and gap to the bar / preview, in logical pixels.
const BORDER: f32 = 2.0;
const GAP: f32 = 8.0;
/// Spacing kept between the live preview and the edges of the display, in logical pixels.
const PREVIEW_MARGIN: f32 = 10.0;
/// Width of the bar's status text, in logical pixels.
const STATUS_WIDTH: f32 = 330.0;
/// Height cap: 30k rows, and at most ~300 MB of pixels.
const MAX_ROWS: u32 = 30_000;
const MAX_BYTES: u64 = 300 << 20;

/// Session-scoped global `Escape` hotkey registration: unregisters on drop so
/// `Escape` returns to normal apps the instant scrolling capture ends.
struct EscapeGuard(Option<Rc<GlobalHotKeyManager>>);

impl EscapeGuard {
    fn register(cx: &App) -> Self {
        let Some(manager) = cx.try_global::<AppState>().and_then(|s| s.hotkeys.clone()) else {
            return Self(None);
        };
        if let Err(e) = manager.register(crate::escape_hotkey()) {
            log::debug!("global Escape hotkey unavailable for scrolling capture (polling fallback active): {e:#}");
            return Self(None);
        }
        Self(Some(manager))
    }
}

impl Drop for EscapeGuard {
    fn drop(&mut self) {
        if let Some(manager) = self.0.take() {
            manager.unregister(crate::escape_hotkey()).ok();
        }
    }
}

/// Rising-edge detector for [`Platform::escape_pressed`] so an `Escape` key
/// still held from a prior interaction does not immediately cancel a newly
/// started scrolling capture.
struct EscapeEdge {
    was_down: bool,
}

impl EscapeEdge {
    fn new(initially_down: bool) -> Self {
        Self { was_down: initially_down }
    }

    fn poll(&mut self, down: bool) -> bool {
        let pressed = down && !self.was_down;
        self.was_down = down;
        pressed
    }
}

/// Cancels the active scrolling capture, if any (invoked by global `Escape`).
pub fn cancel_active(cx: &mut App) {
    if let Some(session) = cx.global_mut::<AppState>().scrolling.take() {
        session.update(cx, |s, cx| s.finish(Finish::Cancel, cx)).ok();
    }
}

/// Dims the screen around the captured region (the "hole", in logical pixels),
/// outlines it, and renders the live stitched preview beside it. The platform
/// makes the entire dim layer click-through so clicking anywhere outside (or
/// inside) the hole passes straight through without raising it above the bar.
struct Dim {
    hole: Bounds<gpui::Pixels>,
    preview: Option<(PxRect, Arc<RenderImage>)>,
    session: Option<WeakEntity<ScrollingCapture>>,
}

impl Dim {
    fn set_preview(&mut self, next: Option<(PxRect, Arc<RenderImage>)>, window: &mut Window) {
        if let Some((_, old)) = std::mem::replace(&mut self.preview, next) {
            window.drop_image(old).ok();
        }
    }
}

impl Render for Dim {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let h = self.hole;
        let dim = || div().absolute().bg(rgba(0x0000_0073));
        div()
            .size_full()
            .relative()
            .child(dim().left_0().right_0().top_0().h(h.top()))
            .child(dim().left_0().right_0().top(h.bottom()).bottom_0())
            .child(dim().left_0().w(h.left()).top(h.top()).h(h.size.height))
            .child(dim().left(h.right()).right_0().top(h.top()).h(h.size.height))
            .child(
                div()
                    .absolute()
                    .left(h.left() - px(BORDER))
                    .top(h.top() - px(BORDER))
                    .w(h.size.width + px(2.0 * BORDER))
                    .h(h.size.height + px(2.0 * BORDER))
                    .border(px(BORDER))
                    .border_color(hsla(THEME_BLUE)),
            )
            .when_some(self.preview.clone(), |el, (r, image)| {
                el.child(
                    div()
                        .absolute()
                        .left(px(r.x - 1.0))
                        .top(px(r.y - 1.0))
                        .w(px(r.w + 2.0))
                        .h(px(r.h + 2.0))
                        .border_1()
                        .border_color(rgba(0xFFFF_FF4D))
                        .shadow_lg()
                        .child(gpui::img(image).w(px(r.w)).h(px(r.h)).object_fit(ObjectFit::Fill)),
                )
            })
    }
}

pub struct ScrollingCapture {
    display: DisplayInfo,
    rect: PixelRect,
    mode: CaptureMode,
    stitcher: Arc<Mutex<Option<Stitcher>>>,
    status: SharedString,
    warning: bool,
    height: u32,
    frame: WindowHandle<Dim>,
    bar: Option<AnyWindowHandle>,
    focus: FocusHandle,
    stopped: bool,
    /// Annotations made before the capture started, relative to the region's
    /// top-left: the stitched image starts with the region, so they keep
    /// their place on its content however far it's scrolled.
    shapes: Vec<Shape>,
    _escape: Option<EscapeGuard>,
    _task: Option<Task<()>>,
}

fn bar_groups(mode: CaptureMode) -> &'static [&'static [Action]] {
    match mode {
        CaptureMode::Standard => toolbar::SCROLLING_BAR,
        CaptureMode::ForEditor => toolbar::SCROLLING_FOR_EDITOR_BAR,
    }
}

/// Device-pixel rectangle -> gpui bounds (logical, in gpui's coordinate space
/// for windows on that display).
fn logical_bounds(display_origin: gpui::Point<gpui::Pixels>, r: PixelRect, scale: f32) -> Bounds<gpui::Pixels> {
    Bounds::new(
        point(display_origin.x + px(r.x as f32 / scale), display_origin.y + px(r.y as f32 / scale)),
        size(px(r.width as f32 / scale), px(r.height as f32 / scale)),
    )
}

fn helper_window_options(bounds: Bounds<gpui::Pixels>, display: gpui::DisplayId, transparent: bool) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        // Keyboard focus stays with the app being scrolled.
        focus: false,
        // On Windows and macOS, `place_floating_window` shows the window after
        // configuring click-through, Z-order, and region clipping — avoiding
        // focus theft (`SW_SHOWNORMAL` on Windows) and unstyled flashes.
        show: !cfg!(any(target_os = "windows", target_os = "macos")),
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        display_id: Some(display),
        window_background: if transparent { WindowBackgroundAppearance::Transparent } else { WindowBackgroundAppearance::Opaque },
        ..Default::default()
    }
}

/// Starts a scrolling capture of `rect` (image pixels of `display`, whose full
/// size in image pixels is `screen`), keeping `shapes` (annotations relative
/// to `rect`) on the result.
pub fn start(
    display: DisplayInfo,
    screen: (i32, i32),
    rect: PixelRect,
    shapes: Vec<Shape>,
    mode: CaptureMode,
    cx: &mut App,
) -> anyhow::Result<()> {
    cancel_active(cx);
    let platform = bluesnap_platform::native();
    let gpui_display = cx
        .displays()
        .into_iter()
        .find(|d| u32::from(d.id()) == display.id)
        .ok_or_else(|| anyhow::anyhow!("display is gone"))?;
    // Where gpui places windows for this display (global on Windows, display-
    // relative on macOS). `place_floating_window` then sets the exact position.
    let origin = gpui_display.bounds().origin;
    let scale = display.scale_factor;
    let (screen_w, screen_h) = screen;
    let border = (BORDER * scale).round() as i32;
    let bar_size = toolbar::panel_size(bar_groups(mode), STATUS_WIDTH);
    let (bar_w, bar_h) = ((bar_size.0 * scale).round() as i32, (bar_size.1 * scale).round() as i32);
    let gap = (GAP * scale).round() as i32;

    // The bar goes below the frame, else above it, else inside the region's
    // bottom edge with the captured area shortened so the bar isn't captured.
    let mut rect = rect;
    let bar_x = (rect.x + rect.width - bar_w).clamp(0, (screen_w - bar_w).max(0));
    let below = rect.y + rect.height + border + gap;
    let above = rect.y - border - gap - bar_h;
    let bar_y = if below + bar_h <= screen_h {
        below
    } else if above >= 0 {
        above
    } else {
        rect.height = (rect.height - bar_h - 2 * gap).max(1);
        rect.y + rect.height + gap
    };
    let bar_rect = PixelRect { x: bar_x, y: bar_y, width: bar_w, height: bar_h };
    // The dim layer covers the whole display; only the region is a hole.
    let screen_rect = PixelRect { x: 0, y: 0, width: screen_w, height: screen_h };
    let hole = Bounds::new(
        point(px(rect.x as f32 / scale), px(rect.y as f32 / scale)),
        size(px(rect.width as f32 / scale), px(rect.height as f32 / scale)),
    );
    let frame = cx.open_window(
        helper_window_options(logical_bounds(origin, screen_rect, scale), gpui_display.id(), true),
        |_, cx| {
            cx.new(|cx| {
                cx.on_release(|dim: &mut Dim, cx| {
                    if let Some(session) = dim.session.take() {
                        cx.defer(move |cx| {
                            session.update(cx, |s, cx| s.finish(Finish::Cancel, cx)).ok();
                        });
                    }
                })
                .detach();
                Dim { hole, preview: None, session: None }
            })
        },
    )?;

    let setup = (|| -> anyhow::Result<_> {
        let frame_ref = frame.update(cx, |_, window, _| bluesnap_platform::window_ref(window))??;
        let bar = cx.open_window(
            helper_window_options(logical_bounds(origin, bar_rect, scale), gpui_display.id(), false),
            |_, cx| cx.new(|cx| ScrollingCapture::new(display.clone(), screen, rect, frame, shapes, mode, cx)),
        )?;
        let (bar_ref, entity) = bar.update(cx, |this, window, cx| -> anyhow::Result<_> {
            this.bar = Some(window.window_handle());
            Ok((bluesnap_platform::window_ref(window)?, cx.entity().downgrade()))
        })??;
        frame.update(cx, |dim, _, _| dim.session = Some(entity.clone()))?;
        Ok((frame_ref, bar, bar_ref, entity))
    })();
    let (frame_ref, bar, bar_ref, entity) = match setup {
        Ok(ok) => ok,
        Err(e) => {
            frame.update(cx, |_, window, _| window.remove_window()).ok();
            return Err(e);
        }
    };

    cx.global_mut::<AppState>().scrolling = Some(entity);
    // Run `place_floating_window` on the UI foreground executor: on Windows this
    // queues right after `DestroyWindow` of the closing selection overlay (so the
    // OS restores focus to the scrolled app), and on all platforms `App` is not
    // borrowed while OS resize/show callbacks re-enter gpui.
    cx.spawn(async move |cx| {
        let alive = bar.read_with(cx, |this, _| !this.stopped).unwrap_or(false)
            && frame.read_with(cx, |_, _| true).unwrap_or(false);
        if !alive {
            return;
        }
        if let Err(e) = platform.place_floating_window(frame_ref, &display, screen_rect, Some(rect), None) {
            log::warn!("scrolling dim layer setup: {e:#}");
        }
        if let Err(e) = platform.place_floating_window(bar_ref, &display, bar_rect, None, Some(frame_ref)) {
            log::warn!("scrolling bar setup: {e:#}");
        }
    })
    .detach();

    log::info!("scrolling capture of {}x{} started", rect.width, rect.height);
    Ok(())
}

struct Step {
    progress: Progress,
    height: u32,
    preview: Option<(PxRect, Arc<RenderImage>)>,
}

impl ScrollingCapture {
    fn new(
        display: DisplayInfo,
        screen: (i32, i32),
        rect: PixelRect,
        frame: WindowHandle<Dim>,
        shapes: Vec<Shape>,
        mode: CaptureMode,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let escape = EscapeGuard::register(cx);
        let id = cx.entity_id();
        cx.on_release(move |this, cx| {
            this._task = None;
            this._escape = None;
            if !std::mem::replace(&mut this.stopped, true) {
                if cx.try_global::<AppState>().and_then(|s| s.scrolling.as_ref()).is_some_and(|s| s.entity_id() == id) {
                    cx.global_mut::<AppState>().scrolling = None;
                }
                let frame = this.frame;
                let was_for_editor = this.mode == CaptureMode::ForEditor;
                cx.defer(move |cx| {
                    frame
                        .update(cx, |dim, window, _| {
                            dim.session = None;
                            dim.set_preview(None, window);
                            window.remove_window();
                        })
                        .ok();
                    if was_for_editor {
                        crate::editor_window::restore_if_hidden(cx);
                    }
                });
            }
        })
        .detach();

        let stitcher = Arc::new(Mutex::new(None));
        let task = cx.spawn({
            let (display, stitcher) = (display.clone(), stitcher.clone());
            async move |this, cx| {
                let mut esc = EscapeEdge::new(bluesnap_platform::native().escape_pressed());
                // Let the overlay finish closing before the first frame, while
                // already watching `Escape` so cancel feels instantaneous.
                for _ in 0..SETTLE_SLICES {
                    cx.background_executor().timer(SETTLE_SLICE).await;
                    if esc.poll(bluesnap_platform::native().escape_pressed()) {
                        this.update(cx, |this, cx| this.finish(Finish::Cancel, cx)).ok();
                        return;
                    }
                }
                loop {
                    if esc.poll(bluesnap_platform::native().escape_pressed()) {
                        this.update(cx, |this, cx| this.finish(Finish::Cancel, cx)).ok();
                        break;
                    }
                    let (display, stitcher) = (display.clone(), stitcher.clone());
                    let step = cx
                        .background_executor()
                        .spawn(async move { capture_and_stitch(&display, screen, rect, &stitcher) })
                        .await;
                    if esc.poll(bluesnap_platform::native().escape_pressed()) {
                        this.update(cx, |this, cx| this.finish(Finish::Cancel, cx)).ok();
                        break;
                    }
                    if this.update(cx, |this, cx| this.on_progress(step, cx)).is_err() {
                        break; // the bar was closed
                    }
                    cx.background_executor().timer(INTERVAL).await;
                }
            }
        });
        Self {
            display,
            rect,
            mode,
            stitcher,
            status: "Scroll down slowly to capture more".into(),
            warning: false,
            height: 0,
            frame,
            bar: None,
            focus,
            stopped: false,
            shapes,
            _escape: Some(escape),
            _task: Some(task),
        }
    }

    fn on_progress(&mut self, step: anyhow::Result<Step>, cx: &mut Context<Self>) {
        let mut changed = false;
        let (status, warning) = match step {
            Ok(Step { progress, height, preview }) => {
                if self.height != height {
                    self.height = height;
                    changed = true;
                }
                if let Some(preview) = preview {
                    self.frame
                        .update(cx, |dim, window, cx| {
                            dim.set_preview(Some(preview), window);
                            cx.notify();
                        })
                        .ok();
                }
                match progress {
                    Progress::Started | Progress::Unchanged | Progress::Appended(_) => {
                        ("Scroll down slowly to capture more", false)
                    }
                    Progress::ScrolledUp => ("Scrolled up: keep scrolling down", false),
                    Progress::LostTrack => ("Too fast: scroll back up a little", true),
                    Progress::Full => ("Maximum length reached: press Done", true),
                }
            }
            Err(e) => {
                log::error!("scrolling capture failed: {e:#}");
                ("Capture failed", true)
            }
        };
        if self.status.as_ref() != status || self.warning != warning {
            (self.status, self.warning) = (status.into(), warning);
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// Stops capturing and closes the bar and dim layer; returns the stitched image.
    fn stop(&mut self, cx: &mut Context<Self>) -> Option<bluesnap_core::Pixmap> {
        if std::mem::replace(&mut self.stopped, true) {
            return None;
        }
        self._task = None;
        self._escape = None;
        let image = self.stitcher.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(Stitcher::finish);
        let id = cx.entity_id();
        if cx.try_global::<AppState>().and_then(|s| s.scrolling.as_ref()).is_some_and(|s| s.entity_id() == id) {
            cx.global_mut::<AppState>().scrolling = None;
        }
        let (frame, bar) = (self.frame, self.bar);
        // Close after this update returns (the bar's own window may be borrowed now),
        // removing the owned bar window before its owner dim window on Windows.
        cx.defer(move |cx| {
            if let Some(bar) = bar {
                bar.update(cx, |_, window, _| window.remove_window()).ok();
            }
            frame
                .update(cx, |dim, window, _| {
                    dim.session = None;
                    dim.set_preview(None, window);
                    window.remove_window();
                })
                .ok();
        });
        log::info!("scrolling capture of {}x{} finished", self.rect.width, self.height);
        image
    }

    /// Ends the session; in `Standard` mode `Copy` and `Save` deliver the
    /// stitched image, while in `ForEditor` mode `Copy` ("Tick") appends the
    /// unflattened stitched slice into the Editor and restores it.
    pub fn finish(&mut self, how: Finish, cx: &mut Context<Self>) {
        if self.stopped {
            return;
        }
        if self.mode == CaptureMode::ForEditor {
            match how {
                Finish::Copy => return self.send_to_editor(cx),
                Finish::Cancel => {
                    self.stop(cx);
                    cx.defer(crate::editor_window::restore_if_hidden);
                    return;
                }
                Finish::Save => {
                    let raw = self.stop(cx);
                    cx.defer(crate::editor_window::restore_if_hidden);
                    if let Some(raw) = raw {
                        let annotated = self.annotate(raw.clone(), cx);
                        let shapes = std::mem::take(&mut self.shapes);
                        let comp = Composition::from_slice(raw, self.display.scale_factor, shapes);
                        output::save_with_dialog(annotated, comp, false, None, cx);
                    }
                    return;
                }
            }
        }
        match how {
            Finish::Cancel => {
                self.stop(cx);
            }
            Finish::Copy => {
                if let Some(raw) = self.stop(cx) {
                    let annotated = self.annotate(raw, cx);
                    output::copy_to_clipboard(annotated, cx);
                }
            }
            Finish::Save => {
                if let Some(raw) = self.stop(cx) {
                    let annotated = self.annotate(raw.clone(), cx);
                    let shapes = std::mem::take(&mut self.shapes);
                    let comp = Composition::from_slice(raw, self.display.scale_factor, shapes);
                    output::save_with_dialog(annotated, comp, true, None, cx);
                }
            }
        }
    }

    /// `image` with the carried-over annotations drawn on it.
    fn annotate(&self, image: bluesnap_core::Pixmap, cx: &App) -> bluesnap_core::Pixmap {
        if self.shapes.is_empty() {
            return image;
        }
        let fonts = cx.global::<AppState>().fonts.clone();
        let doc = bluesnap_core::Document::from_pixmap(image);
        let m = bluesnap_core::shape::Metrics { scale: self.display.scale_factor, fonts: &fonts };
        bluesnap_core::render::export(&doc, doc.bounds(), &self.shapes, m).unwrap_or_else(|| doc.image().clone())
    }

    /// Ends the session and adds the unflattened stitched image + shapes to the Editor.
    fn send_to_editor(&mut self, cx: &mut Context<Self>) {
        if self.stopped {
            return;
        }
        let Some(image) = self.stop(cx) else {
            log::warn!("scrolling capture produced no image");
            if self.mode == CaptureMode::ForEditor {
                cx.defer(crate::editor_window::restore_if_hidden);
            }
            return;
        };
        let (scale, id) = (self.display.scale_factor, self.display.id);
        let display = cx.displays().into_iter().find(|d| u32::from(d.id()) == id).map(|d| d.id());
        let shapes = std::mem::take(&mut self.shapes);
        cx.defer(move |cx| {
            crate::editor_window::add_slice(CapturedSlice { image, scale, shapes, display }, cx);
        });
    }

    fn perform(&mut self, action: Action, cx: &mut Context<Self>) {
        match action {
            Action::AddToEditor => self.send_to_editor(cx),
            other => {
                if let Some(how) = other.finish() {
                    self.finish(how, cx);
                }
            }
        }
    }
}

/// Builds the live preview frame for the current stitched image, snapped to
/// whole device pixels so its bottom edge and scaling stay rock-steady.
fn preview_frame(
    stitcher: &Stitcher,
    screen: (i32, i32),
    rect: PixelRect,
    scale: f32,
) -> Option<(PxRect, Arc<RenderImage>)> {
    let dims = stitcher.dimensions()?;
    let sel = PxRect::new(
        rect.x as f32 / scale,
        rect.y as f32 / scale,
        rect.width as f32 / scale,
        rect.height as f32 / scale,
    );
    let screen_rect = PxRect::new(0.0, 0.0, screen.0 as f32 / scale, screen.1 as f32 / scale);
    let bounds = place_scrolling_preview(sel, screen_rect, dims, BORDER + GAP, PREVIEW_MARGIN)?;
    let tw = (bounds.w * scale).round().max(1.0) as u32;
    let th = (bounds.h * scale).round().max(1.0) as u32;
    let snapped = PxRect::new(
        (bounds.x * scale).round() / scale,
        ((rect.y + rect.height) as f32 - th as f32) / scale,
        tw as f32 / scale,
        th as f32 / scale,
    );
    let bgra = stitcher.thumbnail_bgra(tw, th)?;
    let image = crate::gpu_image::render_image(tw, th, bgra)?;
    Some((snapped, image))
}

/// One capture-and-stitch step, off the UI thread.
fn capture_and_stitch(
    display: &DisplayInfo,
    screen: (i32, i32),
    rect: PixelRect,
    stitcher: &Mutex<Option<Stitcher>>,
) -> anyhow::Result<Step> {
    let image = bluesnap_platform::native().capture_region(display, rect)?;
    let mut bgrx = image.bgrx;
    let rgba = split_bgrx(&mut bgrx);
    // For diagnosing stitching problems on real apps: BLUESNAP_DEBUG_FRAMES=<dir>
    // saves every frame, named by sequence number and outcome.
    let debug_copy = std::env::var_os("BLUESNAP_DEBUG_FRAMES").map(|dir| (dir, rgba.clone()));
    let mut stitcher = stitcher.lock().unwrap_or_else(|e| e.into_inner());
    let stitcher = stitcher.get_or_insert_with(|| {
        let by_memory = (MAX_BYTES / (image.width as u64 * 4).max(1)) as u32;
        Stitcher::new(image.width, MAX_ROWS.min(by_memory))
    });
    if stitcher.width() != image.width {
        anyhow::bail!("frame size changed");
    }
    let progress = stitcher.push(image.height, rgba);
    if let Some((dir, rgba)) = debug_copy {
        static FRAME: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = FRAME.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::path::Path::new(&dir).join(format!("frame-{n:04}-{progress:?}.png"));
        if let Some(pixmap) = bluesnap_core::pixels::pixmap_from_rgba(image.width, image.height, rgba) {
            pixmap.save_png(&path).ok();
        }
    }
    let preview = if matches!(progress, Progress::Started | Progress::Appended(_) | Progress::Full) {
        preview_frame(stitcher, screen, rect, display.scale_factor)
    } else {
        None
    };
    Ok(Step { progress, height: stitcher.height(), preview })
}

impl Render for ScrollingCapture {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let groups = bar_groups(self.mode);
        let done_adds_to_editor = self.mode == CaptureMode::ForEditor;
        if crate::demo::enabled() {
            let items = toolbar::row_items(&toolbar::toolbar_slots(groups, STATUS_WIDTH), 32.0, (0.0, 0.0));
            crate::demo::report("scrolling-bar", window.scale_factor(), items);
        }
        let status_color = if self.warning { hsla(Color::rgb(0xD4380D)) } else { hsla(Color::rgb(0x333333)) };
        let mut bar = toolbar::panel()
            .size_full()
            .text_size(px(13.0))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| match ev.keystroke.key.as_str() {
                "escape" => {
                    cx.stop_propagation();
                    this.finish(Finish::Cancel, cx);
                }
                "enter" => {
                    cx.stop_propagation();
                    this.finish(Finish::Copy, cx);
                }
                _ => {}
            }))
            .child(
            div()
                .w(px(STATUS_WIDTH))
                .flex()
                .items_center()
                .gap(px(8.0))
                .pl(px(6.0))
                .child(svg().path("icons/scroll.svg").size(px(18.0)).text_color(hsla(THEME_BLUE)))
                .child(div().flex_1().overflow_hidden().text_color(status_color).child(self.status.clone()))
                .child(div().flex_none().text_color(hsla(Color::rgb(0x888888))).child(format!("{} px", self.height))),
        );
        for group in groups {
            bar = bar.child(toolbar::separator());
            for &action in *group {
                bar = bar.child(toolbar::action_button(action, false, true, done_adds_to_editor).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut Self, _: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.perform(action, cx);
                    }),
                ));
            }
        }
        bar
    }
}

#[cfg(test)]
mod tests {
    use super::EscapeEdge;

    #[test]
    fn escape_edge_ignores_already_held_key_and_fires_on_new_press() {
        let mut edge = EscapeEdge::new(true);
        assert!(!edge.poll(true), "still held from startup must not cancel");
        assert!(!edge.poll(false), "releasing must not cancel");
        assert!(edge.poll(true), "fresh press must cancel");
        assert!(!edge.poll(true), "holding must not re-fire");
        assert!(!edge.poll(false));
        assert!(edge.poll(true), "second press must fire");
    }

    #[test]
    fn escape_hotkey_has_stable_id_and_matches_spec() {
        let hk = crate::escape_hotkey();
        let parsed: global_hotkey::hotkey::HotKey = "Escape".parse().unwrap();
        assert_eq!(hk.id(), parsed.id());
        assert_eq!(hk.id(), crate::escape_hotkey().id());
    }
}
