//! Taking a screenshot: capture the display under the cursor and open the
//! editing overlay on it.

use std::time::Instant;

use anyhow::Context as _;
use bluesnap_core::{Document, Editor, Rect};
use bluesnap_platform::{Native, Platform};
use gpui::{App, AppContext, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions};

use crate::AppState;
use crate::editor_view::{EditorView, Surface};
use crate::gpu_image::ImageTiles;
use crate::output::Finish;

/// The hotkey / tray action: capture the display under the cursor and open the
/// overlay, or finish a running scrolling capture.
pub fn start_capture(cx: &mut App) {
    // During a scrolling capture the hotkey means "done".
    if let Some(session) = cx.global_mut::<AppState>().scrolling.take() {
        session.update(cx, |s, cx| s.finish(Finish::Copy, cx)).ok();
        return;
    }
    if cx.global::<AppState>().overlay.is_some() {
        log::debug!("hotkey ignored: a capture is already open");
        return;
    }
    if let Err(e) = open_overlay(cx) {
        log::error!("capture failed: {e:#}");
    }
}

fn open_overlay(cx: &mut App) -> anyhow::Result<()> {
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
    let image = ImageTiles::from_bgra(width, height, bgra).context("bad capture buffer")?;
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
    let surface = Surface::Capture { display: capture.display.clone(), permission_url };
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
    let handle = cx.open_window(options, |_, cx| cx.new(|cx| EditorView::new(editor, image, surface, cx)))?;
    stages.push(("open window", started.elapsed()));
    handle.update(cx, |view, window, _| {
        if let Err(e) = platform.configure_overlay(window, &capture.display) {
            log::warn!("overlay setup: {e:#}");
        }
        window.focus(view.focus_handle());
        window.activate_window();
        stages.push(("configure", started.elapsed()));
        let stages = std::mem::take(&mut stages);
        // Frame callbacks run before drawing, so the nested one runs once the
        // first frame (including the screenshot texture upload) is presented.
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
