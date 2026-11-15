//! Editing a whole image in a normal window: used for stitched scrolling
//! screenshots, which are too tall for the full-screen overlay.

use anyhow::Context as _;
use bluesnap_core::pixels::swap_red_blue;
use bluesnap_core::{Document, Editor, Pixmap};
use gpui::{App, AppContext, Bounds, DisplayId, TitlebarOptions, WindowBounds, WindowKind, WindowOptions, px, size};

use crate::AppState;
use crate::editor_view::{EditorView, Surface};
use crate::gpu_image::ImageTiles;

/// Room around the image in the window (toolbar above, margins), in logical pixels.
const CHROME: (f32, f32) = (32.0, 96.0);

/// Opens an editor window for `image`, an opaque screenshot whose pixels are
/// `scale` times its logical size, on `display` (else the primary display).
pub fn open(image: Pixmap, scale: f32, display: Option<DisplayId>, cx: &mut App) -> anyhow::Result<()> {
    let (width, height) = (image.width(), image.height());
    // Opaque, so premultiplied and straight alpha are the same bytes.
    let rgba = image.take();
    let tiles = ImageTiles::from_bgra(width, height, swap_red_blue(&rgba)).context("bad image")?;
    let doc = Document::from_rgba(width, height, rgba).context("bad image")?;
    let fonts = cx.global::<AppState>().fonts.clone();
    let editor = Editor::for_document(doc, fonts, scale);

    // Big enough to show the image at its native size, within 85% of the display.
    let screen = display
        .and_then(|id| cx.displays().into_iter().find(|d| d.id() == id))
        .or_else(|| cx.primary_display())
        .map(|d| d.bounds().size)
        .context("no display")?;
    let (max_w, max_h) = (f32::from(screen.width) * 0.85, f32::from(screen.height) * 0.85);
    let w = (width as f32 / scale + CHROME.0).clamp(560.0, max_w.max(560.0));
    let h = (height as f32 / scale + CHROME.1).clamp(400.0, max_h.max(400.0));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(display, size(px(w), px(h)), cx))),
        titlebar: Some(TitlebarOptions { title: Some("bluesnap: scrolling screenshot".into()), ..Default::default() }),
        focus: true,
        show: true,
        kind: WindowKind::Normal,
        is_resizable: true,
        display_id: display,
        window_min_size: Some(size(px(560.), px(400.))),
        ..Default::default()
    };
    let handle = cx.open_window(options, |_, cx| cx.new(|cx| EditorView::new(editor, tiles, Surface::Document, cx)))?;
    handle.update(cx, |view, window, _| {
        window.focus(view.focus_handle());
        window.activate_window();
    })?;
    // A menu bar app isn't active by itself; bring the new window forward.
    cx.activate(true);
    log::info!("editing a {width}x{height} image");
    Ok(())
}
