//! Linux backend: not implemented yet.
//!
//! The app builds on Linux so the shared code can be developed and tested here,
//! but capturing fails with an error. A real backend would capture with X11
//! `GetImage` (or the XDG screenshot portal on Wayland) and list windows from
//! `_NET_CLIENT_LIST_STACKING`.

use std::path::PathBuf;

use anyhow::bail;
use raw_window_handle::HasWindowHandle;

use crate::{Capture, DisplayInfo, Platform, TextLine};

pub struct Native;

impl Platform for Native {
    const DEFAULT_HOTKEY: &'static str = "Ctrl+Alt+A";

    fn init(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn capture_display_under_cursor(&self) -> anyhow::Result<Capture> {
        bail!("screen capture is not implemented on Linux yet")
    }

    fn configure_overlay(&self, _window: &dyn HasWindowHandle, _display: &DisplayInfo) -> anyhow::Result<()> {
        Ok(())
    }

    fn recognize_text(&self, width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<Vec<TextLine>> {
        crate::recognize_with_model(width, height, rgba)
    }

    fn font_paths(&self) -> Vec<PathBuf> {
        [
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }
}
