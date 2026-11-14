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

/// Captured pixels in the layout every supported OS produces natively: BGRX,
/// row-major without padding, 4th byte undefined. Converting is left to the
/// caller so it happens once, in parallel (`bluesnap_core::pixels::split_bgrx`).
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub bgrx: Vec<u8>,
}

/// A line of recognized text and where it is, in image pixels.
pub struct TextLine {
    pub text: String,
    pub bounds: PixelRect,
}

pub struct Capture {
    pub display: DisplayInfo,
    pub image: Image,
    /// Visible top-level windows on that display, topmost first, clipped to it.
    pub windows: Vec<PixelRect>,
    /// The OS withheld window contents (macOS Screen Recording permission), so
    /// the image likely shows only the desktop. The UI should say so.
    pub permission_missing: bool,
}

/// Commands from the tray / menu bar icon's menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    Capture,
    EditSettings,
    Quit,
}

/// Keeps the tray / menu bar icon alive; dropping it removes the icon.
pub struct Tray(#[allow(dead_code)] Box<dyn std::any::Any>);

pub trait Platform {
    /// Hotkey used when the config file doesn't set one, in `global-hotkey` syntax.
    const DEFAULT_HOTKEY: &'static str;

    /// Whether the overlay should be opened in gpui's fullscreen mode. On Windows
    /// that is a borderless window exactly covering the monitor; a regular
    /// frameless window there keeps invisible resize borders, which shrink the
    /// client area and would scale the screenshot. On macOS fullscreen means a
    /// separate Space with an animation, so the overlay is sized by hand instead.
    const OVERLAY_FULLSCREEN: bool;

    /// One-time setup once the UI event loop is running (permissions, app policy).
    fn init(&self) -> anyhow::Result<()>;

    /// Captures the display under the mouse cursor, plus the window layout on it.
    /// Must run before the overlay window is shown so the overlay isn't captured.
    fn capture_display_under_cursor(&self) -> anyhow::Result<Capture>;

    /// Turns a freshly opened window into the capture overlay: exactly covering
    /// `display` (including menu bars/taskbars), above every other window, focused.
    fn configure_overlay(&self, window: &dyn HasWindowHandle, display: &DisplayInfo) -> anyhow::Result<()>;

    /// Adds the tray (Windows) or menu bar (macOS) icon. `hotkey` is shown in its menu.
    fn create_tray(&self, hotkey: &str, on_command: Box<dyn Fn(TrayCommand) + Send + Sync>) -> anyhow::Result<Tray>;

    /// Where the user grants the permission behind [`Capture::permission_missing`].
    fn permission_settings_url(&self) -> Option<&'static str>;

    /// Recognizes text in straight-alpha RGBA pixels. Blocking; call it off the
    /// UI thread. Lines come back in no particular order.
    fn recognize_text(&self, width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<Vec<TextLine>>;

    /// Font files for text annotations, best first. CJK-capable fonts are listed
    /// early so mixed-script text renders without tofu.
    fn font_paths(&self) -> Vec<PathBuf>;
}

/// The embedded PP-OCRv6 engine, for OSes without a good built-in OCR API.
#[cfg(not(target_os = "macos"))]
fn recognize_with_model(width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<Vec<TextLine>> {
    let lines = bluesnap_ocr::recognize(width, height, rgba)?;
    Ok(lines
        .into_iter()
        .map(|l| TextLine {
            text: l.text,
            bounds: PixelRect { x: l.x as i32, y: l.y as i32, width: l.width as i32, height: l.height as i32 },
        })
        .collect())
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod tray;

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
