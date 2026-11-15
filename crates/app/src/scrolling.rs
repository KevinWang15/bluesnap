//! Scrolling screenshots: after the user picks a region and presses the toolbar
//! button, the overlay closes so the real app underneath can be scrolled. The
//! screen stays dimmed except for a click-through hole over the region, a small
//! bar shows progress, and the region is captured ~10 times a second and
//! stitched into one tall image.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bluesnap_core::pixels::split_bgrx;
use bluesnap_core::stitch::{Progress, Stitcher};
use bluesnap_core::{Color, THEME_BLUE};
use bluesnap_platform::{DisplayInfo, PixelRect, Platform};
use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Context, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, SharedString, Styled, Task, Window, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, div, point, px, rgb, rgba, size, svg,
};

use crate::{AppState, output};

/// Time between captures. Fast enough that normal scrolling keeps an overlap.
const INTERVAL: Duration = Duration::from_millis(100);
/// Border around the hole and gap to the bar, in logical pixels.
const BORDER: f32 = 2.0;
const GAP: f32 = 8.0;
const BAR_SIZE: (f32, f32) = (430.0, 44.0);
/// Height cap: 30k rows, and at most ~300 MB of pixels.
const MAX_ROWS: u32 = 30_000;
const MAX_BYTES: u64 = 300 << 20;

fn hsla(c: Color) -> Hsla {
    rgb(c.to_hex()).into()
}

/// Dims the screen around the captured region (the "hole", in logical pixels)
/// and outlines it, like the capture overlay. The platform makes the hole
/// click-through, so the app underneath can be scrolled.
struct Dim {
    hole: Bounds<gpui::Pixels>,
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
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    Copy,
    Save,
    Cancel,
}

pub struct ScrollingCapture {
    display: DisplayInfo,
    rect: PixelRect,
    stitcher: Arc<Mutex<Option<Stitcher>>>,
    status: SharedString,
    warning: bool,
    height: u32,
    frame: AnyWindowHandle,
    bar: Option<AnyWindowHandle>,
    _task: Option<Task<()>>,
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
        show: true,
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
/// size in image pixels is `screen`).
pub fn start(display: DisplayInfo, screen: (i32, i32), rect: PixelRect, cx: &mut App) -> anyhow::Result<()> {
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
    let (bar_w, bar_h) = ((BAR_SIZE.0 * scale).round() as i32, (BAR_SIZE.1 * scale).round() as i32);
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
        |_, cx| cx.new(|_| Dim { hole }),
    )?;
    frame.update(cx, |_, window, _| {
        if let Err(e) = platform.place_floating_window(window, &display, screen_rect, Some(rect)) {
            log::warn!("scrolling dim layer setup: {e:#}");
        }
    })?;

    let frame_handle: AnyWindowHandle = frame.into();
    let bar = cx.open_window(
        helper_window_options(logical_bounds(origin, bar_rect, scale), gpui_display.id(), false),
        |_, cx| cx.new(|cx| ScrollingCapture::new(display.clone(), rect, frame_handle, cx)),
    )?;
    bar.update(cx, |this, window, _| {
        if let Err(e) = platform.place_floating_window(window, &this.display, bar_rect, None) {
            log::warn!("scrolling bar setup: {e:#}");
        }
        this.bar = Some(window.window_handle());
    })?;
    let entity = bar.update(cx, |_, _, cx| cx.entity().downgrade())?;
    cx.global_mut::<AppState>().scrolling = Some(entity);
    log::info!("scrolling capture of {}x{} started", rect.width, rect.height);
    Ok(())
}

impl ScrollingCapture {
    fn new(display: DisplayInfo, rect: PixelRect, frame: AnyWindowHandle, cx: &mut Context<Self>) -> Self {
        let stitcher = Arc::new(Mutex::new(None));
        let task = cx.spawn({
            let (display, stitcher) = (display.clone(), stitcher.clone());
            async move |this, cx| {
                // Let the overlay finish closing before the first frame.
                cx.background_executor().timer(Duration::from_millis(200)).await;
                loop {
                    let (display, stitcher) = (display.clone(), stitcher.clone());
                    let progress = cx
                        .background_executor()
                        .spawn(async move { capture_and_stitch(&display, rect, &stitcher) })
                        .await;
                    if this.update(cx, |this, cx| this.on_progress(progress, cx)).is_err() {
                        break; // the bar was closed
                    }
                    cx.background_executor().timer(INTERVAL).await;
                }
            }
        });
        Self {
            display,
            rect,
            stitcher,
            status: "Scroll down slowly to capture more".into(),
            warning: false,
            height: 0,
            frame,
            bar: None,
            _task: Some(task),
        }
    }

    fn on_progress(&mut self, progress: anyhow::Result<(Progress, u32)>, cx: &mut Context<Self>) {
        let (status, warning) = match progress {
            Ok((progress, height)) => {
                self.height = height;
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
        }
        cx.notify();
    }

    /// Ends the session; `Copy` and `Save` deliver the stitched image.
    pub fn finish(&mut self, how: Finish, cx: &mut Context<Self>) {
        self._task = None;
        let image = if how == Finish::Cancel {
            None
        } else {
            self.stitcher.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(Stitcher::finish)
        };
        cx.global_mut::<AppState>().scrolling = None;
        let windows = [Some(self.frame), self.bar];
        // Close after this update returns: the bar's own window is borrowed now.
        cx.defer(move |cx| {
            for handle in windows.into_iter().flatten() {
                handle.update(cx, |_, window, _| window.remove_window()).ok();
            }
        });
        match (how, image) {
            (Finish::Copy, Some(image)) => output::copy_to_clipboard(image, cx),
            (Finish::Save, Some(image)) => {
                let dir = cx.global::<AppState>().config.save_dir();
                output::save_with_dialog(image, dir, cx);
            }
            (Finish::Cancel, _) => {}
            (_, None) => log::warn!("scrolling capture produced no image"),
        }
        log::info!("scrolling capture of {}x{} finished", self.rect.width, self.height);
    }
}

/// One capture-and-stitch step, off the UI thread.
fn capture_and_stitch(
    display: &DisplayInfo,
    rect: PixelRect,
    stitcher: &Mutex<Option<Stitcher>>,
) -> anyhow::Result<(Progress, u32)> {
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
    Ok((progress, stitcher.height()))
}

fn bar_button(id: &'static str, icon: &'static str, color: Hsla) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(32.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(|s| s.bg(rgba(0x0000_000F)))
        .child(svg().path(icon).size(px(20.0)).text_color(color))
}

impl Render for ScrollingCapture {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let press = |how: Finish| {
            cx.listener(move |this: &mut Self, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                this.finish(how, cx);
            })
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(10.0))
            .bg(rgb(0xFFFFFF))
            .rounded(px(8.0))
            .border_1()
            .border_color(rgba(0x0000_0022))
            .text_size(px(13.0))
            .child(svg().path("icons/scroll.svg").size(px(18.0)).text_color(hsla(THEME_BLUE)))
            .child(
                div()
                    .flex_1()
                    .text_color(if self.warning { hsla(Color::rgb(0xD4380D)) } else { hsla(Color::rgb(0x333333)) })
                    .child(self.status.clone()),
            )
            .child(div().text_color(hsla(Color::rgb(0x888888))).child(format!("{} px", self.height)))
            .child(bar_button("cancel", "icons/cancel.svg", hsla(Color::rgb(0xF5222D))).on_mouse_down(MouseButton::Left, press(Finish::Cancel)))
            .child(bar_button("save", "icons/save.svg", hsla(Color::rgb(0x333333))).on_mouse_down(MouseButton::Left, press(Finish::Save)))
            .child(bar_button("done", "icons/done.svg", hsla(THEME_BLUE)).on_mouse_down(MouseButton::Left, press(Finish::Copy)))
    }
}
