//! OS integration for bluesnap.
//!
//! Every OS-specific API call lives in this crate, one module per OS, behind the
//! [`Platform`] trait. The rest of the app only sees the plain data types below.
//! Adding an OS means adding a module that implements [`Platform`] and wiring it
//! into the `cfg` list at the bottom of this file.

use std::path::PathBuf;

use raw_window_handle::HasWindowHandle;

/// Rectangle in captured-image pixels (origin at the image's top-left).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Rectangle in the OS's own desktop coordinate space: physical pixels on
/// Windows, points ("global display coordinates") on macOS.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug)]
pub struct DisplayInfo {
    /// Same value gpui uses for its `DisplayId` on this OS.
    pub id: u32,
    pub bounds: NativeRect,
    /// Physical pixels per logical pixel.
    pub scale_factor: f32,
}

/// Opaque, straight-alpha RGBA8 pixels, row-major without padding.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct Capture {
    pub display: DisplayInfo,
    pub image: Image,
    /// Visible top-level windows on that display, topmost first, clipped to it.
    pub windows: Vec<PixelRect>,
}

pub trait Platform {
    /// Hotkey used when the config file doesn't set one, in `global-hotkey` syntax.
    const DEFAULT_HOTKEY: &'static str;

    /// One-time setup once the UI event loop is running (permissions, app policy).
    fn init(&self) -> anyhow::Result<()>;

    /// Captures the display under the mouse cursor, plus the window layout on it.
    /// Must run before the overlay window is shown so the overlay isn't captured.
    fn capture_display_under_cursor(&self) -> anyhow::Result<Capture>;

    /// Turns a freshly opened window into the capture overlay: exactly covering
    /// `display` (including menu bars/taskbars), above every other window, focused.
    fn configure_overlay(&self, window: &dyn HasWindowHandle, display: &DisplayInfo) -> anyhow::Result<()>;

    /// Font files for text annotations, best first. CJK-capable fonts are listed
    /// early so mixed-script text renders without tofu.
    fn font_paths(&self) -> Vec<PathBuf>;
}

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::Native;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Native;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Native;

/// The implementation for the OS being compiled for.
pub fn native() -> Native {
    Native
}
