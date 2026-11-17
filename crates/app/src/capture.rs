//! Taking a screenshot: reusable capture entry point supporting both standalone
//! captures (`CaptureMode::Standard`) and in-editor `"Add image"` captures
//! (`CaptureMode::ForEditor`).

use std::time::{Duration, Instant};

use anyhow::Context as _;
use bluesnap_core::shape::Shape;
use bluesnap_core::{Document, Editor, Pixmap, Rect};
use bluesnap_platform::{Native, Platform};
use gpui::{App, AppContext, DisplayId, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions};

use crate::AppState;
use crate::canvas_view::{CanvasView, Surface};
use crate::gpu_image::ImageTiles;
use crate::output::Finish;

/// Caller-configurable behavior for a screenshot capture session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CaptureMode {
    /// Standalone capture (hotkey / tray): shows `Add to editor`, `Save`,
    /// `Cancel`, and `Done` (copy to clipboard).
    #[default]
    Standard,
    /// Triggered by the Editor's `"Add image"` button: hides `Add to editor`,
    /// overrides `Done` ("Tick") to append the captured slice to the Editor,
    /// and restores the hidden Editor window when finished or cancelled.
    ForEditor,
}

/// Unflattened result of a capture (raw cropped pixels + live annotation shapes).
pub struct CapturedSlice {
    pub image: Pixmap,
    pub scale: f32,
    pub shapes: Vec<Shape>,
    pub display: Option<DisplayId>,
}

/// Default hotkey / tray action: start a standalone capture, or finish a
/// running scrolling capture.
pub fn start_capture(cx: &mut App) {
    if let Some(session) = cx.global_mut::<AppState>().scrolling.take() {
        session.update(cx, |s, cx| s.finish(Finish::Copy, cx)).ok();
        return;
    }
    start_with_mode(CaptureMode::Standard, cx);
}

/// Starts a screenshot capture in `mode`.
pub fn start_with_mode(mode: CaptureMode, cx: &mut App) {
    if cx.global::<AppState>().overlay.is_some() {
        log::debug!("capture ignored: an overlay is already open");
        if mode == CaptureMode::ForEditor {
            crate::editor_window::restore_if_hidden(cx);
        }
        return;
    }
    if crate::toast::dismiss_startup(cx) {
        // Let the OS unmap the startup toast before grabbing desktop pixels.
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(40)).await;
            cx.update(|cx| launch_overlay(mode, cx)).ok();
        })
        .detach();
        return;
    }
    launch_overlay(mode, cx);
}

fn launch_overlay(mode: CaptureMode, cx: &mut App) {
    if cx.global::<AppState>().overlay.is_some() {
        return;
    }
    if let Err(e) = open_overlay(mode, cx) {
        log::error!("capture failed: {e:#}");
        if mode == CaptureMode::ForEditor {
            crate::editor_window::restore_if_hidden(cx);
        }
    }
}

fn open_overlay(mode: CaptureMode, cx: &mut App) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut stages = Vec::new();
    let platform = bluesnap_platform::native();
    let capture = platform.capture_display_under_cursor()?;
    stages.push(("capture", started.elapsed()));

    let display = cx
        .displays()
        .into_iter()
        .find(|d| u32::from(d.id()) == capture.display.id)
        .context("captured display not known to gpui")?;

    let (width, height) = (capture.image.width, capture.image.height);
    let mut bgra = capture.image.bgrx;
    anyhow::ensure!(bgra.len() == width as usize * height as usize * 4, "bad capture buffer");
    let rgba = bluesnap_core::pixels::split_bgrx(&mut bgra);
    let screenshot = ImageTiles::from_bgra(width, height, bgra).context("bad capture buffer")?;
    stages.push(("gpu image", started.elapsed()));
    let scale = capture.display.scale_factor;
    let windows = capture
        .windows
        .iter()
        .map(|w| Rect::new(w.x as f32, w.y as f32, w.width as f32, w.height as f32))
        .collect();
    let doc = Document::from_rgba(width, height, rgba).context("bad capture buffer")?;
    let fonts = cx.global::<AppState>().fonts.clone();
    let editor = Editor::new(doc, fonts, windows, scale);
    let permission_url = capture.permission_missing.then(|| platform.permission_settings_url()).flatten();
    let surface = Surface::Capture {
        display: capture.display.clone(),
        gpui_display: display.id(),
        permission_url,
        mode,
        screenshot,
    };
    stages.push(("editor", started.elapsed()));

    let options = WindowOptions {
        window_bounds: Some(if Native::OVERLAY_FULLSCREEN {
            WindowBounds::Fullscreen(display.bounds())
        } else {
            WindowBounds::Windowed(display.bounds())
        }),
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        display_id: Some(display.id()),
        window_background: WindowBackgroundAppearance::Opaque,
        ..Default::default()
    };
    let handle = cx.open_window(options, |_, cx| cx.new(|cx| CanvasView::new(editor, surface, cx)))?;
    stages.push(("open window", started.elapsed()));
    handle.update(cx, |view, window, _| {
        if let Err(e) = platform.configure_overlay(window, &capture.display) {
            log::warn!("overlay setup: {e:#}");
        }
        window.focus(view.focus_handle());
        window.activate_window();
        stages.push(("configure", started.elapsed()));
        let stages = std::mem::take(&mut stages);
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |_, _| {
                let timeline: Vec<String> = stages.iter().map(|(name, t)| format!("{name} {t:.1?}")).collect();
                log::debug!("overlay timeline: {}, first frame presented {:.1?}", timeline.join(", "), started.elapsed());
            });
        });
    })?;
    cx.global_mut::<AppState>().overlay = Some(handle.into());
    Ok(())
}
