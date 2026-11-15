//! Linux backend: not implemented yet.
//!
//! The app builds on Linux so the shared code can be developed and tested here,
//! but capturing fails with an error. A real backend would capture with X11
//! `GetImage` (or the XDG screenshot portal on Wayland) and list windows from
//! `_NET_CLIENT_LIST_STACKING`.


use anyhow::bail;
use raw_window_handle::HasWindowHandle;

use crate::{Capture, DisplayInfo, FontFamily, FontSource, Platform, TextLine, Tray, TrayCommand};

pub struct Native;

impl Platform for Native {
    const DEFAULT_HOTKEY: &'static str = "Ctrl+Alt+A";
    const OVERLAY_FULLSCREEN: bool = false;

    fn init(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn capture_display_under_cursor(&self) -> anyhow::Result<Capture> {
        bail!("screen capture is not implemented on Linux yet")
    }

    fn configure_overlay(&self, _window: &dyn HasWindowHandle, _display: &DisplayInfo) -> anyhow::Result<()> {
        Ok(())
    }

    fn capture_region(&self, _display: &DisplayInfo, _rect: crate::PixelRect) -> anyhow::Result<crate::Image> {
        bail!("screen capture is not implemented on Linux yet")
    }

    fn place_floating_window(
        &self,
        _window: &dyn HasWindowHandle,
        _display: &DisplayInfo,
        _rect: crate::PixelRect,
        _hole: Option<crate::PixelRect>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn create_tray(&self, _hotkey: &str, _on_command: Box<dyn Fn(TrayCommand) + Send + Sync>) -> anyhow::Result<Tray> {
        bail!("tray icon is not implemented on Linux yet")
    }

    fn permission_settings_url(&self) -> Option<&'static str> {
        None
    }

    fn recognize_text(&self, width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<Vec<TextLine>> {
        crate::recognize_with_model(width, height, rgba)
    }

    fn fonts(&self, family: FontFamily) -> Vec<FontSource> {
        let file = |path: &str| FontSource::file(path);
        let primary = match family {
            FontFamily::Sans => vec![file("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf")],
            FontFamily::Rounded => vec![crate::bundled_rounded()],
            FontFamily::Mono => vec![file("/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf")],
        };
        let cjk = [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        ]
        .map(file);
        primary.into_iter().chain(cjk).collect()
    }
}
