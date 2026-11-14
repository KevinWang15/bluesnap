//! Delivering the finished capture: clipboard or PNG file.

use std::path::PathBuf;

use bluesnap_core::Pixmap;
use bluesnap_core::render::to_rgba_straight;
use gpui::App;

pub fn copy_to_clipboard(image: Pixmap, cx: &mut App) {
    // Off the UI thread: large images take tens of milliseconds to hand over.
    cx.background_executor()
        .spawn(async move {
            let data = arboard::ImageData {
                width: image.width() as usize,
                height: image.height() as usize,
                bytes: to_rgba_straight(&image).into(),
            };
            match arboard::Clipboard::new().and_then(|mut c| c.set_image(data)) {
                Ok(()) => log::info!("copied {}x{} to clipboard", image.width(), image.height()),
                Err(e) => log::error!("copy to clipboard failed: {e}"),
            }
        })
        .detach();
}

pub fn save_with_dialog(image: Pixmap, dir: PathBuf, cx: &mut App) {
    let name = chrono::Local::now().format("Screenshot %Y-%m-%d %H.%M.%S.png").to_string();
    // The dialog needs the app to be active (we're a background app on macOS).
    cx.activate(true);
    let picked = cx.prompt_for_new_path(&dir, Some(&name));
    cx.spawn(async move |cx| {
        let path = match picked.await {
            Ok(Ok(Some(path))) => path,
            Ok(Ok(None)) => return,
            Ok(Err(e)) => return log::error!("save dialog failed: {e}"),
            Err(_) => return,
        };
        let path = if path.extension().is_some() { path } else { path.with_extension("png") };
        let result = cx.background_executor().spawn({
            let path = path.clone();
            async move { image.save_png(&path) }
        });
        match result.await {
            Ok(()) => log::info!("saved {}", path.display()),
            Err(e) => log::error!("saving {} failed: {e}", path.display()),
        }
        // Hand focus back to whatever app the user was in (no-op where unsupported).
        cx.update(|cx| cx.hide()).ok();
    })
    .detach();
}
