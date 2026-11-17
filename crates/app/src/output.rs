//! Delivering finished captures and editor compositions: plain-image clipboard
//! copy ("Tick") or saving to disk as either `.png` (raster image) or `.bs`
//! (editable BSON composition).

use std::path::PathBuf;

use bluesnap_core::render::to_rgba_straight;
use bluesnap_core::{Composition, Pixmap};
use bluesnap_platform::{Platform, SaveFormat};
use gpui::App;

use crate::AppState;

/// How a capture or edit session ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    Copy,
    Save,
    Cancel,
}

/// Copies `image` as a plain raster image to the system clipboard.
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

pub type OnSaved = Box<dyn FnOnce(PathBuf, &mut App) + 'static>;

/// Prompts for a save path offering both `.png` (image) and `.bs` (editable
/// BSON composition), then writes the chosen format off the UI thread.
pub fn save_with_dialog(
    image: Pixmap,
    composition: Composition,
    hide_after: bool,
    on_saved: Option<OnSaved>,
    cx: &mut App,
) {
    let dir = cx.global::<AppState>().config.save_dir();
    let stem = chrono::Local::now().format("Screenshot %Y-%m-%d %H.%M.%S").to_string();
    // Native dialogs on macOS require the background app to be active.
    cx.activate(true);

    let platform = bluesnap_platform::native();
    cx.spawn(async move |cx| {
        let native_choice = cx
            .update(|_| platform.save_dialog(&dir, &stem))
            .unwrap_or_else(|e| Err(anyhow::anyhow!("{e:#}")));

        let chosen: Option<(PathBuf, SaveFormat)> = match native_choice {
            Ok(Some(res)) => res,
            Ok(None) => {
                // Fallback for platforms without a custom filter dialog (Linux).
                let suggested = format!("{stem}.png");
                let rx = match cx.update(|cx| cx.prompt_for_new_path(&dir, Some(&suggested))) {
                    Ok(rx) => rx,
                    Err(_) => return,
                };
                match rx.await {
                    Ok(Ok(Some(path))) => {
                        let fmt = SaveFormat::from_path(&path).unwrap_or(SaveFormat::Png);
                        let path = if path.extension().is_some() { path } else { path.with_extension(fmt.extension()) };
                        Some((path, fmt))
                    }
                    Ok(Ok(None)) => None,
                    Ok(Err(e)) => {
                        log::error!("save dialog failed: {e}");
                        None
                    }
                    Err(_) => None,
                }
            }
            Err(e) => {
                log::error!("save dialog failed: {e:#}");
                None
            }
        };

        let Some((path, format)) = chosen else {
            if hide_after {
                cx.update(|_| platform.restore_previous_app()).ok();
            }
            return;
        };
        let write_path = path.clone();
        let result = cx
            .background_executor()
            .spawn(async move {
                match format {
                    SaveFormat::Png => image.save_png(&write_path).map_err(|e| anyhow::anyhow!("{e}")),
                    SaveFormat::Bs => bluesnap_core::format::save_bs(&composition, &write_path).map_err(|e| anyhow::anyhow!("{e}")),
                }
            })
            .await;

        match result {
            Ok(()) => {
                log::info!("saved {}", path.display());
                if let Some(cb) = on_saved {
                    cx.update(|cx| cb(path, cx)).ok();
                }
            }
            Err(e) => log::error!("saving {} failed: {e:#}", path.display()),
        }
        if hide_after {
            cx.update(|_| platform.restore_previous_app()).ok();
        }
    })
    .detach();
}
