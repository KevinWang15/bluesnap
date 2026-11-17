//! Multi-image Composition Editor window manager:
//! - creates a new Editor session or appends into the existing open Editor,
//! - temporarily hides the Editor while capturing a new screenshot (`Add image`),
//! - opens `.bs` BSON compositions and raster image files from disk.

use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use bluesnap_core::pixels::pixmap_from_rgba;
use bluesnap_core::{Composition, Editor};
use bluesnap_platform::{Platform, WindowRef};
use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, DisplayId, PathPromptOptions, TitlebarOptions, WindowBounds,
    WindowHandle, WindowKind, WindowOptions, px, size,
};

use crate::{AppState, run_mode};
use crate::canvas_view::CanvasView;
use crate::capture::{self, CaptureMode, CapturedSlice};
use crate::toast::{self, Toast};

/// Extra window chrome around the initial canvas (toolbar + workspace margins).
const CHROME: (f32, f32) = (96.0, 148.0);
const MIN_WIN: (f32, f32) = (680.0, 460.0);
const DEFAULT_EMPTY_CANVAS: (u32, u32) = (1200, 800);

/// Adds `slice` to the open Editor session, or creates and opens a new Editor
/// session sized to `slice` if none is open.
pub fn add_slice(slice: CapturedSlice, cx: &mut App) {
    let was_hidden = cx.global::<AppState>().editor_hidden;
    let CapturedSlice { image, scale, shapes, display } = slice;
    let mut pending = Some((image, shapes));
    if let Some(existing) = live_editor(cx) {
        let updated = existing.update(cx, |view, window, cx| {
            if let Some((image, shapes)) = pending.take() {
                view.append_slice(image, scale, shapes, window, cx);
            }
            if !was_hidden {
                window.focus(view.focus_handle());
                window.activate_window();
            }
        });
        if updated.is_ok() {
            if was_hidden {
                restore_if_hidden(cx);
            } else {
                cx.activate(true);
            }
            return;
        }
    }
    cx.global_mut::<AppState>().editor_hidden = false;
    if let Some((image, shapes)) = pending {
        let comp = Composition::from_slice(image, scale, shapes);
        if let Err(e) = open_window_with(comp, display, "bluesnap editor", cx) {
            log::error!("could not open editor window: {e:#}");
        }
    }
}

/// Opens an empty Editor window, or brings the existing Editor window forward
/// (invoked from the tray / menu bar `"Open Editor"` item).
pub fn open_or_focus(cx: &mut App) {
    if cx.global::<AppState>().editor_hidden && live_editor(cx).is_some() {
        restore_if_hidden(cx);
        return;
    }
    if let Some(existing) = live_editor(cx) {
        let ok = existing
            .update(cx, |view, window, _| {
                window.focus(view.focus_handle());
                window.activate_window();
            })
            .is_ok();
        if ok {
            cx.activate(true);
            return;
        }
    }
    let scale = cx
        .primary_display()
        .and_then(|_| bluesnap_platform::native().notification_area().ok().map(|(d, _)| d.scale_factor))
        .unwrap_or(1.0);
    let comp = Composition::new_empty(
        (DEFAULT_EMPTY_CANVAS.0 as f32 * scale).round() as u32,
        (DEFAULT_EMPTY_CANVAS.1 as f32 * scale).round() as u32,
        scale,
    );
    if let Err(e) = open_window_with(comp, None, "bluesnap editor", cx) {
        log::error!("could not open editor window: {e:#}");
    }
}

/// Loads a `.bs` composition (or raster image file) from `path` into the Editor.
pub fn open_file(path: &Path, cx: &mut App) -> anyhow::Result<()> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let title = match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => format!("bluesnap editor — {name}"),
        None => "bluesnap editor".to_string(),
    };

    if ext == "bs" {
        let comp = bluesnap_core::format::load_bs(path).with_context(|| format!("reading {}", path.display()))?;
        return show_composition(comp, &title, cx);
    }

    // Try `.bs` first if extensionless, else decode as a raster image and append/open.
    if ext.is_empty()
        && let Ok(comp) = bluesnap_core::format::load_bs(path)
    {
        return show_composition(comp, &title, cx);
    }

    let dyn_img = image::open(path).with_context(|| format!("opening image {}", path.display()))?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let pixmap = pixmap_from_rgba(w, h, rgba.into_raw()).context("invalid image dimensions")?;

    let was_hidden = cx.global::<AppState>().editor_hidden;
    if let Some(existing) = live_editor(cx) {
        let mut pending = Some(pixmap);
        let ok = existing
            .update(cx, |view, window, cx| {
                let scale = view.editor().scale();
                if let Some(img) = pending.take() {
                    view.append_slice(img, scale, Vec::new(), window, cx);
                }
                if !was_hidden {
                    window.focus(view.focus_handle());
                    window.activate_window();
                }
            })
            .is_ok();
        if ok {
            if was_hidden {
                restore_if_hidden(cx);
            } else {
                cx.activate(true);
            }
            return Ok(());
        }
        if let Some(pixmap) = pending {
            cx.global_mut::<AppState>().editor_hidden = false;
            let comp = Composition::from_slice(pixmap, 1.0, Vec::new());
            return open_window_with(comp, None, &title, cx);
        }
    } else {
        cx.global_mut::<AppState>().editor_hidden = false;
        let comp = Composition::from_slice(pixmap, 1.0, Vec::new());
        return open_window_with(comp, None, &title, cx);
    }
    Ok(())
}

/// Prompts the user to pick a `.bs` (or image) file and opens it in the Editor.
pub fn prompt_open_file(bs_only: bool, owner: Option<WindowRef>, cx: &mut App) {
    let dir = cx.global::<AppState>().config.save_dir();
    cx.activate(true);
    run_mode::begin_job(cx);
    let platform = bluesnap_platform::native();
    cx.spawn(async move |cx| {
        async {
            // Run `open_dialog` outside `cx.update` so `RefCell<App>` is NOT borrowed
            // while the OS modal file dialog runs its message loop.
            let native_choice = platform.open_dialog(owner, &dir, bs_only);

            let picked = match native_choice {
                Ok(Some(res)) => res,
                Ok(None) => {
                    let rx = match cx.update(|cx| {
                        cx.prompt_for_paths(PathPromptOptions {
                            files: true,
                            directories: false,
                            multiple: false,
                            prompt: Some(if bs_only { "Open .bs File".into() } else { "Open .bs or Image".into() }),
                        })
                    }) {
                        Ok(rx) => rx,
                        Err(_) => return,
                    };
                    match rx.await {
                        Ok(Ok(Some(mut paths))) => paths.pop(),
                        _ => None,
                    }
                }
                Err(e) => {
                    log::error!("open dialog failed: {e:#}");
                    None
                }
            };

            let Some(path) = picked else { return };
            cx.update(|cx| {
                if let Err(e) = open_file(&path, cx) {
                    log::error!("could not open {}: {e:#}", path.display());
                    if let Some(ed) = live_editor(cx) {
                        ed.update(cx, |view, window, cx| {
                            view.show_toast(
                                Toast::new(toast::Kind::Error, "Could not open file").detail(format!("{e:#}")),
                                window,
                                cx,
                            );
                        })
                        .ok();
                    } else {
                        crate::show_error(format!("Could not open {}:\n\n{e:#}", path.display()), cx);
                    }
                }
            })
            .ok();
        }
        .await;
        cx.update(run_mode::end_job).ok();
    })
    .detach();
}

/// Hides the Editor window temporarily and triggers a `CaptureMode::ForEditor`
/// capture so the user can add another screenshot without capturing the Editor.
pub fn request_add_image(cx: &mut App) {
    let win_ref = cx.global::<AppState>().editor_ref;
    let platform = bluesnap_platform::native();
    cx.spawn(async move |cx| {
        // Let mouse-up / input capture release finish before hiding the window.
        cx.background_executor().timer(Duration::from_millis(16)).await;
        // Run `set_window_visible` outside `cx.update` so Win32/AppKit activation
        // callbacks never re-enter GPUI while `RefCell<App>` is borrowed.
        if let Some(win_ref) = win_ref {
            if let Err(e) = platform.set_window_visible(win_ref, false) {
                log::warn!("could not hide editor window before capture: {e:#}");
            } else {
                cx.update(|cx| cx.global_mut::<AppState>().editor_hidden = true).ok();
            }
        }
        // Let WindowServer / DWM finish any remaining frame flip before grabbing pixels.
        cx.background_executor().timer(Duration::from_millis(30)).await;
        cx.update(|cx| capture::start_with_mode(CaptureMode::ForEditor, cx)).ok();
    })
    .detach();
}

/// Unhides and focuses the Editor window if it was hidden for an `"Add image"` capture.
pub fn restore_if_hidden(cx: &mut App) {
    if !std::mem::replace(&mut cx.global_mut::<AppState>().editor_hidden, false) {
        return;
    }
    let win_ref = cx.global::<AppState>().editor_ref;
    let handle = live_editor(cx);
    let platform = bluesnap_platform::native();
    cx.spawn(async move |cx| {
        // Run `set_window_visible(true)` outside `cx.update` so Win32 `WM_SHOWWINDOW`
        // (`draw_window` -> `on_request_frame`) can borrow `App` cleanly.
        if let Some(win_ref) = win_ref
            && let Err(e) = platform.set_window_visible(win_ref, true)
        {
            log::warn!("could not restore editor window: {e:#}");
        }
        if let Some(handle) = handle {
            cx.update(|cx| {
                handle
                    .update(cx, |view, window, _| {
                        window.focus(view.focus_handle());
                        window.activate_window();
                    })
                    .ok();
                cx.activate(true);
            })
            .ok();
        }
    })
    .detach();
}

fn show_composition(comp: Composition, title: &str, cx: &mut App) -> anyhow::Result<()> {
    let was_hidden = cx.global::<AppState>().editor_hidden;
    if let Some(existing) = live_editor(cx) {
        let title_owned = title.to_string();
        let mut pending = Some(comp);
        if existing
            .update(cx, |view, window, cx| {
                if let Some(c) = pending.take() {
                    view.replace_composition(c, window, cx);
                }
                window.set_window_title(&title_owned);
                if !was_hidden {
                    window.focus(view.focus_handle());
                    window.activate_window();
                }
            })
            .is_ok()
        {
            if was_hidden {
                restore_if_hidden(cx);
            } else {
                cx.activate(true);
            }
            return Ok(());
        }
        if let Some(comp) = pending {
            cx.global_mut::<AppState>().editor_hidden = false;
            return open_window_with(comp, None, title, cx);
        }
    } else {
        cx.global_mut::<AppState>().editor_hidden = false;
        return open_window_with(comp, None, title, cx);
    }
    Ok(())
}

fn live_editor(cx: &mut App) -> Option<WindowHandle<CanvasView>> {
    let handle = cx.global::<AppState>().editor?;
    let any = AnyWindowHandle::from(handle);
    if cx.windows().contains(&any) {
        Some(handle)
    } else {
        let state = cx.global_mut::<AppState>();
        state.editor = None;
        state.editor_ref = None;
        state.editor_hidden = false;
        None
    }
}

fn open_window_with(comp: Composition, display: Option<DisplayId>, title: &str, cx: &mut App) -> anyhow::Result<()> {
    let (width, height, scale) = (comp.width, comp.height, comp.scale);
    let fonts = cx.global::<AppState>().fonts.clone();
    let editor = Editor::from_composition(comp, fonts);

    let screen = display
        .and_then(|id| cx.displays().into_iter().find(|d| d.id() == id))
        .or_else(|| cx.primary_display())
        .map(|d| d.bounds().size)
        .context("no display")?;
    let (max_w, max_h) = (f32::from(screen.width) * 0.88, f32::from(screen.height) * 0.88);
    let w = (width as f32 / scale + CHROME.0).clamp(MIN_WIN.0, max_w.max(MIN_WIN.0));
    let h = (height as f32 / scale + CHROME.1).clamp(MIN_WIN.1, max_h.max(MIN_WIN.1));

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(display, size(px(w), px(h)), cx))),
        titlebar: Some(TitlebarOptions { title: Some(title.to_string().into()), ..Default::default() }),
        focus: true,
        show: true,
        kind: WindowKind::Normal,
        is_resizable: true,
        display_id: display,
        window_min_size: Some(size(px(MIN_WIN.0), px(MIN_WIN.1))),
        ..Default::default()
    };
    let handle = cx.open_window(options, |_, cx| cx.new(|cx| CanvasView::new_editor(editor, cx)))?;
    let win_ref = handle.update(cx, |view, window, _| {
        window.focus(view.focus_handle());
        window.activate_window();
        bluesnap_platform::window_ref(window).ok()
    })?;
    let state = cx.global_mut::<AppState>();
    state.editor = Some(handle);
    state.editor_ref = win_ref;
    state.editor_hidden = false;
    cx.activate(true);
    log::info!("opened editor window ({width}x{height})");
    Ok(())
}
