//! OS integration for bluesnap.
//!
//! Every OS-specific API call lives in this crate, one module per OS, behind the
//! [`Platform`] trait. The rest of the app only sees the plain data types below.
//! Adding an OS means adding a module that implements [`Platform`] and wiring it
//! into the `cfg` list at the bottom of this file.


pub use bluesnap_core::{FontFamily, FontSource};
use raw_window_handle::HasWindowHandle;

/// An opaque handle to an already-open OS window, extracted from gpui's
/// `Window` so [`Platform::place_floating_window`] can run after releasing
/// gpui's borrow on `App` (OS resize/show callbacks re-enter gpui).
#[derive(Clone, Copy, Debug)]
pub struct WindowRef(#[cfg(not(target_os = "linux"))] pub(crate) raw_window_handle::RawWindowHandle);

// Linux's platform operations are stubs and do not consume a native handle.
// GPUI's X11 backend panics if asked for one, including during editor paint.
#[cfg(target_os = "linux")]
pub fn window_ref(_: &dyn HasWindowHandle) -> anyhow::Result<WindowRef> {
    Ok(WindowRef())
}

#[cfg(not(target_os = "linux"))]
pub fn window_ref(window: &dyn HasWindowHandle) -> anyhow::Result<WindowRef> {
    Ok(WindowRef(window.window_handle().map_err(|e| anyhow::anyhow!("{e:?}"))?.as_raw()))
}

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
///
/// Colors are sRGB on every OS (macOS converts from the display's profile),
/// so saved files and the clipboard look the same on any screen, and windows
/// showing them must be marked with [`Platform::present_as_srgb`].
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

/// A visible window on the captured display.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CapturedWindow {
    /// Visible frame in image pixels, not clipped to the display (a window
    /// partly off-screen keeps its real size, so only its on-screen corners
    /// get rounded).
    pub bounds: PixelRect,
    /// Geometric corner radius in image pixels for platforms that supply one.
    pub corner_radius: f32,
    #[cfg(target_os = "macos")]
    window_id: u32,
    #[cfg(target_os = "macos")]
    point_size: (f64, f64),
}

impl bluesnap_core::outline::PickableWindow for CapturedWindow {
    fn bounds(&self) -> bluesnap_core::Rect {
        let b = self.bounds;
        bluesnap_core::Rect::new(b.x as f32, b.y as f32, b.width as f32, b.height as f32)
    }

    fn outline(&self) -> bluesnap_core::WindowOutline {
        #[cfg(target_os = "macos")]
        if let Some(image) = macos::window_image(self) {
            return bluesnap_core::WindowOutline::from_window_image(self.bounds(), &image);
        }
        bluesnap_core::WindowOutline::new(self.bounds(), self.corner_radius)
    }
}

pub struct Capture {
    pub display: DisplayInfo,
    pub image: Image,
    /// Visible top-level windows on that display, topmost first.
    pub windows: Vec<CapturedWindow>,
    /// The OS withheld window contents (macOS Screen Recording permission), so
    /// the image likely shows only the desktop. The UI should say so.
    pub permission_missing: bool,
}

/// Commands from the tray / menu bar icon's menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    Capture,
    OpenEditor,
    OpenBsFile,
    EditSettings,
    AiSettings,
    /// The start-at-login item was clicked (see [`Platform::set_start_at_login`]).
    ToggleStartAtLogin,
    Quit,
}

/// Output format chosen in the Save dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveFormat {
    Png,
    Bs,
}

impl SaveFormat {
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "bs" => Some(SaveFormat::Bs),
            "png" => Some(SaveFormat::Png),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            SaveFormat::Png => "png",
            SaveFormat::Bs => "bs",
        }
    }
}

/// Role of the running process, controlling OS application policy at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppRole {
    /// Resident background tray / menu bar utility.
    BackgroundUtility,
    /// Standalone `.bs` Composition Editor window without a tray icon.
    StandaloneEditor,
}

/// Keeps the tray / menu bar icon alive; dropping it removes the icon.
pub struct Tray {
    _icon: Box<dyn std::any::Any>,
    show_start_at_login: Box<dyn Fn(bool)>,
}

impl Tray {
    /// Sets the check mark of the start-at-login item.
    pub fn show_start_at_login(&self, enabled: bool) {
        (self.show_start_at_login)(enabled)
    }
}

pub trait Platform {
    /// Hotkey used when the config file doesn't set one, in `global-hotkey` syntax.
    const DEFAULT_HOTKEY: &'static str;

    /// Human-readable name of the OS area where the background icon lives
    /// (`"system tray"` on Windows, `"menu bar"` on macOS).
    const TRAY_LOCATION: &'static str;

    /// Whether corner notifications belong at the top-right (`true`, macOS)
    /// rather than the bottom-right (`false`, Windows).
    const NOTIFY_TOP_RIGHT: bool = false;

    /// Whether the overlay should be opened in gpui's fullscreen mode. On Windows
    /// that is a borderless window exactly covering the monitor; a regular
    /// frameless window there keeps invisible resize borders, which shrink the
    /// client area and would scale the screenshot. On macOS fullscreen means a
    /// separate Space with an animation, so the overlay is sized by hand instead.
    const OVERLAY_FULLSCREEN: bool;

    /// Whether [`Platform::color_dialog`] opens a native custom color picker.
    const HAS_COLOR_DIALOG: bool = false;

    /// One-time setup once the UI event loop is running (permissions, app policy).
    fn init(&self, role: AppRole) -> anyhow::Result<()>;

    /// Best-effort per-user registration of `.bs` files with this executable.
    /// When `overwrite_existing` is `false` (e.g. a standalone `bluesnap file.bs`
    /// invocation), existing Bluesnap registrations are left untouched so a
    /// temporary binary doesn't hijack an installed copy's path.
    fn register_file_associations(&self, _overwrite_existing: bool) -> anyhow::Result<()> {
        Ok(())
    }

    /// Captures the display under the mouse cursor, plus the window layout on it.
    /// Must run before the overlay window is shown so the overlay isn't captured.
    fn capture_display_under_cursor(&self) -> anyhow::Result<Capture>;

    /// Turns a freshly opened window into the capture overlay: exactly covering
    /// `display` (including menu bars/taskbars), above every other window, focused.
    fn configure_overlay(&self, window: &dyn HasWindowHandle, display: &DisplayInfo) -> anyhow::Result<()>;

    /// Yields active application focus back to whichever app was frontmost before
    /// the capture overlay opened (macOS; no-op on Windows where closing the
    /// overlay automatically restores focus to the underlying window).
    fn restore_previous_app(&self) {}

    /// Puts a normal window (like settings opened from the capture overlay)
    /// above the overlay, and focuses it.
    fn raise_above_overlay(&self, window: &dyn HasWindowHandle) -> anyhow::Result<()> {
        let _ = window;
        Ok(())
    }

    /// Captures `rect` (in `display`'s image pixels) from the live screen, for
    /// scrolling screenshots. Called repeatedly, so it avoids anything that
    /// flickers the cursor.
    fn capture_region(&self, display: &DisplayInfo, rect: PixelRect) -> anyhow::Result<Image>;

    /// Places a helper window (the scrolling-capture dim layer or bar) exactly at
    /// `rect` (in `display`'s image pixels), above normal windows, without taking
    /// focus. When `hole` is `Some` (the dim layer), the window ignores all mouse
    /// input so clicks anywhere on or outside `hole` pass straight through to the
    /// desktop and apps below instead of raising the dim layer. When `above` is
    /// `Some` (the scrolling bar), the OS keeps this window ordered above `above`.
    fn place_floating_window(
        &self,
        window: WindowRef,
        display: &DisplayInfo,
        rect: PixelRect,
        hole: Option<PixelRect>,
        above: Option<WindowRef>,
    ) -> anyhow::Result<()>;

    /// Hides (`false`) or restores and focuses (`true`) a normal window (used
    /// while capturing a new screenshot from inside the Editor).
    fn set_window_visible(&self, _window: WindowRef, _visible: bool) -> anyhow::Result<()> {
        Ok(())
    }

    /// Declares that `window` draws sRGB pixels, the color space captures are
    /// delivered in, so the compositor color-matches them to the display just
    /// as it does the rest of the desktop. Call it on every window that shows
    /// captured pixels, before its first frame. Only macOS color-manages
    /// windows; elsewhere the pixels already go to the display unchanged.
    fn present_as_srgb(&self, _window: WindowRef) {}

    /// Sets the active canvas cursor for `window` when `Some` (supplying native
    /// diagonal resize cursors `IDC_SIZENWSE` / `IDC_SIZENESW` and `IDC_SIZEALL`
    /// that gpui 0.2.2 omits on Windows), or yields back to gpui when `None`
    /// (e.g. while hovering floating toolbar buttons).
    fn set_canvas_cursor(&self, _window: WindowRef, _cursor: Option<bluesnap_core::CursorKind>) {}

    /// Native Save dialog offering `PNG Image (.png)` and `BlueSnap Editable (.bs)`.
    /// `owner` is the parent Editor window when saving from the Editor, or `None`
    /// when saving after a capture overlay closes.
    /// Returns `Ok(None)` when delegating to gpui's fallback file dialog.
    fn save_dialog(
        &self,
        _owner: Option<WindowRef>,
        _dir: &std::path::Path,
        _stem: &str,
    ) -> anyhow::Result<Option<Option<(std::path::PathBuf, SaveFormat)>>> {
        Ok(None)
    }

    /// Native Open dialog for `.bs` files (or `.bs` + common images when `bs_only` is `false`).
    /// Returns `Ok(None)` when delegating to gpui's fallback file dialog.
    fn open_dialog(
        &self,
        _owner: Option<WindowRef>,
        _dir: &std::path::Path,
        _bs_only: bool,
    ) -> anyhow::Result<Option<Option<std::path::PathBuf>>> {
        Ok(None)
    }

    /// Native custom color picker dialog parented to `owner`.
    /// Returns `Ok(None)` when cancelled or unsupported.
    fn color_dialog(
        &self,
        _owner: Option<WindowRef>,
        _initial: bluesnap_core::Color,
    ) -> anyhow::Result<Option<bluesnap_core::Color>> {
        Ok(None)
    }

    /// Whether the physical Escape key is currently held down, used as a fallback
    /// cancel check when global hotkey registration is unavailable or blocked.
    fn escape_pressed(&self) -> bool {
        false
    }

    /// Primary display and its usable work area (excluding taskbars, menu bars,
    /// and docks, in `DisplayInfo`-local image pixels) for corner notifications.
    fn notification_area(&self) -> anyhow::Result<(DisplayInfo, PixelRect)>;

    /// Adds the tray (Windows) or menu bar (macOS) icon. `hotkey` is shown in its menu.
    fn create_tray(&self, hotkey: &str, on_command: Box<dyn Fn(TrayCommand) + Send + Sync>) -> anyhow::Result<Tray>;

    /// Whether bluesnap is set to start when the user logs in.
    fn start_at_login(&self) -> bool {
        false
    }

    /// Makes this executable start when the user logs in, or stops that.
    fn set_start_at_login(&self, _enabled: bool) -> anyhow::Result<()> {
        anyhow::bail!("starting at login is not supported on this OS")
    }

    /// Where the user grants the permission behind [`Capture::permission_missing`].
    fn permission_settings_url(&self) -> Option<&'static str>;

    /// Loads text recognition resources before the first explicit request.
    fn prewarm_text_recognition(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Recognizes text in straight-alpha RGBA pixels. Blocking; call it off the
    /// UI thread. Lines come back in no particular order.
    fn recognize_text(&self, width: u32, height: u32, rgba: &[u8], scale: f32) -> anyhow::Result<Vec<TextLine>>;

    /// Fonts for text in `family`, best first, bold-ish like annotation text
    /// in other tools. Each list ends with CJK-capable fallbacks so mixed-script
    /// text renders without tofu.
    fn fonts(&self, family: FontFamily) -> Vec<FontSource>;
}

/// The embedded PP-OCRv6 engine, for OSes without a good built-in OCR API.
#[cfg(not(target_os = "macos"))]
fn recognize_with_model(width: u32, height: u32, rgba: &[u8], scale: f32) -> anyhow::Result<Vec<TextLine>> {
    let lines = bluesnap_ocr::recognize(width, height, rgba, scale)?;
    Ok(lines
        .into_iter()
        .map(|l| TextLine {
            text: l.text,
            bounds: PixelRect {
                x: l.x.round() as i32,
                y: l.y.round() as i32,
                width: l.width.round() as i32,
                height: l.height.round() as i32,
            },
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

/// A rounded face for platforms without one (OFL, see assets/fonts).
#[cfg(not(target_os = "macos"))]
pub(crate) fn bundled_rounded() -> FontSource {
    FontSource::bundled(include_bytes!("../assets/fonts/Nunito.ttf")).weight(800.0)
}

/// The implementation for the OS being compiled for.
pub fn native() -> Native {
    Native
}
