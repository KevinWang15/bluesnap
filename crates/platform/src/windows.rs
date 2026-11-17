//! Windows backend: GDI capture, `EnumWindows` + DWM for window bounds.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, bail};
use bluesnap_core::CursorKind;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, FALSE, HWND, LPARAM, LRESULT, POINT, RECT, TRUE, WPARAM,
};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_TRANSITIONS_FORCEDISABLED, DwmFlush, DwmGetWindowAttribute,
    DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    ClientToScreen, CombineRgn, CreateRectRgn, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetMonitorInfoW,
    HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, RGN_DIFF,
    ROP_CODE, ReleaseDC, SRCCOPY, SelectObject, SetWindowRgn,
};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Controls::Dialogs::{
    CC_ANYCOLOR, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, ChooseColorW, GetOpenFileNameW, GetSaveFileNameW,
    OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI, SetThreadDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DestroyWindow, EnumWindows, GWL_EXSTYLE, GWLP_HWNDPARENT, GWLP_WNDPROC,
    GetClientRect, GetCursorPos, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, HCURSOR, HTCLIENT,
    HWND_TOPMOST, IDC_ARROW, IDC_CROSS, IDC_IBEAM, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENWSE, IDC_SIZENS, IDC_SIZEWE,
    IsIconic, IsWindowVisible, LoadCursorW, PostMessageW, SW_HIDE, SW_RESTORE, SW_SHOW, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetCursor, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, WM_NCDESTROY, WM_SETCURSOR, WNDPROC, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::{
    Capture, DisplayInfo, FontFamily, FontSource, Image, NativeRect, PixelRect, Platform, SaveFormat, TextLine, Tray,
    TrayCommand, WindowRef,
};

pub struct Native;

/// Programs started at login, per user (under `HKEY_CURRENT_USER`).
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "bluesnap";

/// A NUL-terminated UTF-16 string.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Runs `f` with per-monitor v2 DPI awareness so Win32 coordinates are physical
/// pixels even if the host manifest wasn't applied.
fn with_per_monitor_dpi<T>(f: impl FnOnce() -> T) -> T {
    let previous = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let result = f();
    if !previous.is_null() {
        unsafe { SetThreadDpiAwarenessContext(previous) };
    }
    result
}

fn raw_win32_hwnd(raw: RawWindowHandle) -> anyhow::Result<HWND> {
    let RawWindowHandle::Win32(handle) = raw else {
        bail!("not a Win32 window");
    };
    Ok(handle.hwnd.get() as HWND)
}

type CursorOverrideMap = HashMap<isize, (WNDPROC, Option<usize>)>;

/// Per-window client-area cursor overrides (`hwnd` -> `(orig_wndproc, override_hcursor)`),
/// supplying `IDC_SIZENWSE`, `IDC_SIZENESW`, and `IDC_SIZEALL` on Windows where gpui 0.2.2
/// falls back to `IDC_ARROW`.
static CURSOR_OVERRIDES: OnceLock<Mutex<CursorOverrideMap>> = OnceLock::new();

fn win32_cursor_handle(kind: CursorKind) -> usize {
    let id = match kind {
        CursorKind::Default => IDC_ARROW,
        CursorKind::Crosshair => IDC_CROSS,
        CursorKind::Move => IDC_SIZEALL,
        CursorKind::Text => IDC_IBEAM,
        CursorKind::ResizeNwSe => IDC_SIZENWSE,
        CursorKind::ResizeNeSw => IDC_SIZENESW,
        CursorKind::ResizeNs => IDC_SIZENS,
        CursorKind::ResizeEw => IDC_SIZEWE,
    };
    unsafe { LoadCursorW(null_mut(), id) as usize }
}

unsafe extern "system" fn cursor_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let key = hwnd as isize;
    if msg == WM_NCDESTROY {
        let orig = CURSOR_OVERRIDES
            .get()
            .and_then(|m| m.lock().unwrap_or_else(|e| e.into_inner()).remove(&key))
            .and_then(|(proc, _)| proc);
        return match orig {
            Some(proc) => unsafe { CallWindowProcW(Some(proc), hwnd, msg, wparam, lparam) },
            None => 0,
        };
    }
    let entry = CURSOR_OVERRIDES
        .get()
        .and_then(|m| m.lock().unwrap_or_else(|e| e.into_inner()).get(&key).copied());
    if let Some((orig, override_cursor)) = entry {
        if msg == WM_SETCURSOR
            && (lparam as usize & 0xFFFF) == HTCLIENT as usize
            && let Some(raw_cursor) = override_cursor
        {
            unsafe {
                SetCursor(raw_cursor as HCURSOR);
            }
            return 1;
        }
        if let Some(proc) = orig {
            return unsafe { CallWindowProcW(Some(proc), hwnd, msg, wparam, lparam) };
        }
    }
    0
}

fn win32_hwnd(window: &dyn HasWindowHandle) -> anyhow::Result<HWND> {
    raw_win32_hwnd(window.window_handle().map_err(|e| anyhow::anyhow!("{e:?}"))?.as_raw())
}

/// Ensures Win32 common file dialogs (`GetSaveFileNameW` / `GetOpenFileNameW`)
/// always appear in front of the user:
/// - when `owner` is `Some` (e.g. the Editor window), brings it forward and
///   parents the modal dialog directly to it;
/// - when `owner` is `None` (e.g. right after the fullscreen capture overlay
///   closed, or from the tray menu), creates a zero-pixel topmost foreground
///   owner window on the cursor's monitor so Windows never buries an unowned
///   dialog behind other apps, and destroys it on drop.
struct DialogOwner {
    hwnd: HWND,
    destroy_on_drop: bool,
}

impl DialogOwner {
    fn acquire(owner: Option<WindowRef>) -> Self {
        if let Some(win_ref) = owner
            && let Ok(hwnd) = raw_win32_hwnd(win_ref.0)
        {
            unsafe {
                SetForegroundWindow(hwnd);
            }
            return Self { hwnd, destroy_on_drop: false };
        }
        with_per_monitor_dpi(|| unsafe {
            let mut cursor = POINT { x: 0, y: 0 };
            GetCursorPos(&mut cursor);
            let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
            let mut info: MONITORINFO = zeroed();
            info.cbSize = size_of::<MONITORINFO>() as u32;
            let (x, y) = if GetMonitorInfoW(monitor, &mut info) != FALSE {
                (info.rcWork.left + 120, info.rcWork.top + 80)
            } else {
                (120, 80)
            };
            let class = wide("STATIC");
            let title = wide("");
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                x,
                y,
                0,
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                null(),
            );
            if !hwnd.is_null() {
                ShowWindow(hwnd, SW_SHOW);
                SetForegroundWindow(hwnd);
            }
            Self { hwnd, destroy_on_drop: !hwnd.is_null() }
        })
    }
}

impl Drop for DialogOwner {
    fn drop(&mut self) {
        if self.destroy_on_drop && !self.hwnd.is_null() {
            unsafe {
                DestroyWindow(self.hwnd);
            }
        }
    }
}

fn set_topmost_and_focus(window: &dyn HasWindowHandle) -> anyhow::Result<()> {
    let hwnd = win32_hwnd(window)?;
    let ok = unsafe { SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW) };
    if ok == FALSE {
        bail!("SetWindowPos failed: {}", std::io::Error::last_os_error());
    }
    unsafe { SetForegroundWindow(hwnd) };
    Ok(())
}

impl Platform for Native {
    const DEFAULT_HOTKEY: &'static str = "Ctrl+Alt+A";
    const TRAY_LOCATION: &'static str = "system tray";
    const OVERLAY_FULLSCREEN: bool = true;

    fn init(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn capture_display_under_cursor(&self) -> anyhow::Result<Capture> {
        with_per_monitor_dpi(capture_under_cursor)
    }

    fn configure_overlay(&self, window: &dyn HasWindowHandle, display: &DisplayInfo) -> anyhow::Result<()> {
        let _ = display; // gpui's fullscreen mode already covers exactly this monitor.
    const HAS_COLOR_DIALOG: bool = true;
        // Always on top, above the taskbar. Allowed to focus because the process
        // just received the hotkey input.
        set_topmost_and_focus(window)
    }

    fn raise_above_overlay(&self, window: &dyn HasWindowHandle) -> anyhow::Result<()> {
        // Topmost too, and set later, so it sits above the overlay.
        set_topmost_and_focus(window)
    }

    fn capture_region(&self, display: &DisplayInfo, rect: PixelRect) -> anyhow::Result<Image> {
        with_per_monitor_dpi(|| {
            let (x, y) = (display.bounds.x as i32 + rect.x, display.bounds.y as i32 + rect.y);
            let rc = RECT { left: x, top: y, right: x + rect.width, bottom: y + rect.height };
            // No CAPTUREBLT: with DWM it isn't needed, and it makes the cursor flicker
            // when called many times a second.
            capture_rect(&rc, SRCCOPY)
        })
    }

    fn place_floating_window(
        &self,
        window: WindowRef,
        display: &DisplayInfo,
        rect: PixelRect,
        hole: Option<PixelRect>,
        above: Option<WindowRef>,
    ) -> anyhow::Result<()> {
        let hwnd = raw_win32_hwnd(window.0)?;
        let owner = above.map(|w| raw_win32_hwnd(w.0)).transpose()?;
        with_per_monitor_dpi(|| {
            let (x, y) = (display.bounds.x as i32 + rect.x, display.bounds.y as i32 + rect.y);
            unsafe {
                // Helper windows never take focus from the scrolled app, and the dim
                // layer (`hole.is_some()`) ignores all mouse input so clicking the
                // dimmed area passes straight through instead of raising the dim layer.
                let mut ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) | WS_EX_NOACTIVATE as isize;
                if hole.is_some() {
                    ex_style |= (WS_EX_LAYERED | WS_EX_TRANSPARENT) as isize;
                }
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_style);

                // Make the bar owned by the dim layer so Win32 enforces as a window-
                // manager invariant that the bar stays above the dim layer in Z-order.
                if let Some(owner) = owner {
                    SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, owner as isize);
                }

                if SetWindowPos(hwnd, HWND_TOPMOST, x, y, rect.width, rect.height, SWP_NOACTIVATE | SWP_FRAMECHANGED)
                    == FALSE
                {
                    bail!("SetWindowPos failed: {}", std::io::Error::last_os_error());
                }
                // gpui keeps invisible resize borders around frameless windows; grow the
                // window by them so the client area (what gpui draws) lands on `rect`.
                let (mut outer, mut client) = (zeroed::<RECT>(), zeroed::<RECT>());
                let mut origin = POINT { x: 0, y: 0 };
                GetWindowRect(hwnd, &mut outer);
                GetClientRect(hwnd, &mut client);
                ClientToScreen(hwnd, &mut origin);
                let (left, top) = (origin.x - outer.left, origin.y - outer.top);
                let right = outer.right - (origin.x + client.right);
                let bottom = outer.bottom - (origin.y + client.bottom);
                // Clip the window to its client area (the invisible borders would
                // otherwise swallow clicks next to it), minus the pass-through hole,
                // before `SWP_SHOWWINDOW` makes the window visible.
                let region = CreateRectRgn(left, top, left + rect.width, top + rect.height);
                if !region.is_null() {
                    if let Some(h) = hole {
                        let inner = CreateRectRgn(left + h.x, top + h.y, left + h.x + h.width, top + h.y + h.height);
                        if !inner.is_null() {
                            CombineRgn(region, region, inner, RGN_DIFF);
                            DeleteObject(inner);
                        }
                    }
                    // On success the system owns the region.
                    if SetWindowRgn(hwnd, region, TRUE) == 0 {
                        DeleteObject(region);
                    }
                }
                if SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    x - left,
                    y - top,
                    rect.width + left + right,
                    rect.height + top + bottom,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                ) == FALSE
                {
                    bail!("SetWindowPos failed: {}", std::io::Error::last_os_error());
                }
            }
            Ok(())
        })
    }

    fn set_window_visible(&self, window: WindowRef, visible: bool) -> anyhow::Result<()> {
        let hwnd = raw_win32_hwnd(window.0)?;
        unsafe {
            if visible {
                ShowWindow(hwnd, if IsIconic(hwnd) != FALSE { SW_RESTORE } else { SW_SHOW });
                SetForegroundWindow(hwnd);
                let disable: i32 = FALSE;
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_TRANSITIONS_FORCEDISABLED as u32,
                    (&disable as *const i32).cast(),
                    size_of::<i32>() as u32,
                );
            } else {
                // Disable Windows 10/11 DWM's ~250ms fade-and-shrink close/hide
                // transition before `SW_HIDE`, then wait two DWM vblanks so the
                // flip-model desktop frontbuffer has no translucent ghost of the
                // Editor when `BitBlt` captures the screen.
                let disable: i32 = TRUE;
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_TRANSITIONS_FORCEDISABLED as u32,
                    (&disable as *const i32).cast(),
                    size_of::<i32>() as u32,
                );
                ShowWindow(hwnd, SW_HIDE);
                let _ = DwmFlush();
                let _ = DwmFlush();
            }
        }
        Ok(())
    }

    fn set_canvas_cursor(&self, window: WindowRef, cursor: Option<CursorKind>) {
        let Ok(hwnd) = raw_win32_hwnd(window.0) else { return };
        let key = hwnd as isize;
        let wanted = cursor.map(win32_cursor_handle).filter(|&h| h != 0);
        let map = CURSOR_OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
        let prev = if let Some(entry) = guard.get_mut(&key) {
            std::mem::replace(&mut entry.1, wanted)
        } else {
            if wanted.is_none() {
                return;
            }
            let prev_proc_raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
            if prev_proc_raw == 0 {
                return;
            }
            let prev_proc: WNDPROC = unsafe { std::mem::transmute::<isize, WNDPROC>(prev_proc_raw) };
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, cursor_wndproc as *const () as usize as isize);
            }
            guard.insert(key, (prev_proc, wanted));
            None
        };
        drop(guard);

        if prev != wanted {
            unsafe {
                if let Some(h) = wanted {
                    SetCursor(h as HCURSOR);
                } else {
                    let _ = PostMessageW(hwnd, WM_SETCURSOR, hwnd as usize, HTCLIENT as isize);
                }
            }
        }
    }

    fn save_dialog(
        &self,
        owner: Option<WindowRef>,
        dir: &std::path::Path,
        stem: &str,
    ) -> anyhow::Result<Option<Option<(PathBuf, SaveFormat)>>> {
        let dialog_owner = DialogOwner::acquire(owner);
        let filter: Vec<u16> = "PNG Image (*.png)\0*.png\0BlueSnap Editable (*.bs)\0*.bs\0\0".encode_utf16().collect();
        let def_ext = wide("png");
        let init_dir: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        let mut file_buf = vec![0u16; 32768];
        for (dst, ch) in file_buf.iter_mut().zip(format!("{stem}.png").encode_utf16()) {
            *dst = ch;
        }
        let mut ofn: OPENFILENAMEW = unsafe { zeroed() };
        ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = dialog_owner.hwnd;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.nFilterIndex = 1;
        ofn.lpstrFile = file_buf.as_mut_ptr();
        ofn.nMaxFile = file_buf.len() as u32;
        ofn.lpstrInitialDir = init_dir.as_ptr();
        ofn.lpstrDefExt = def_ext.as_ptr();
        ofn.Flags = OFN_EXPLORER | OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
        let ok = unsafe { GetSaveFileNameW(&mut ofn) };
        drop(dialog_owner);
        if ok == FALSE {
            return Ok(Some(None));
        }
        let len = file_buf.iter().position(|&c| c == 0).unwrap_or(file_buf.len());
        let raw_path = PathBuf::from( <std::ffi::OsString as std::os::windows::ffi::OsStringExt>::from_wide(&file_buf[..len]) );
        let format = match SaveFormat::from_path(&raw_path) {
            Some(fmt) => fmt,
            None if ofn.nFilterIndex == 2 => SaveFormat::Bs,
            None => SaveFormat::Png,
        };
        let final_path = if SaveFormat::from_path(&raw_path) == Some(format) {
            raw_path
        } else {
            raw_path.with_extension(format.extension())
        };
        Ok(Some(Some((final_path, format))))
    }

    fn open_dialog(
        &self,
        owner: Option<WindowRef>,
        dir: &std::path::Path,
        bs_only: bool,
    ) -> anyhow::Result<Option<Option<PathBuf>>> {
        let dialog_owner = DialogOwner::acquire(owner);
        let filter_str = if bs_only {
            "BlueSnap Editable (*.bs)\0*.bs\0All Files (*.*)\0*.*\0\0"
        } else {
            "BlueSnap & Image Files (*.bs;*.png;*.jpg;*.jpeg;*.webp)\0*.bs;*.png;*.jpg;*.jpeg;*.webp\0BlueSnap Editable (*.bs)\0*.bs\0Image Files (*.png;*.jpg;*.jpeg;*.webp)\0*.png;*.jpg;*.jpeg;*.webp\0\0"
        };
        let filter: Vec<u16> = filter_str.encode_utf16().collect();
        let init_dir: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        let mut file_buf = vec![0u16; 32768];
        let mut ofn: OPENFILENAMEW = unsafe { zeroed() };
        ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = dialog_owner.hwnd;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.nFilterIndex = 1;
        ofn.lpstrFile = file_buf.as_mut_ptr();
        ofn.nMaxFile = file_buf.len() as u32;
        ofn.lpstrInitialDir = init_dir.as_ptr();
        ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
        let ok = unsafe { GetOpenFileNameW(&mut ofn) };
        drop(dialog_owner);
        if ok == FALSE {
            return Ok(Some(None));
        }
        let len = file_buf.iter().position(|&c| c == 0).unwrap_or(file_buf.len());
        let path = PathBuf::from( <std::ffi::OsString as std::os::windows::ffi::OsStringExt>::from_wide(&file_buf[..len]) );
        Ok(Some(Some(path)))
    }

    fn escape_pressed(&self) -> bool {
        (unsafe { GetAsyncKeyState(VK_ESCAPE as i32) } as u16 & 0x8000) != 0
    }

    fn notification_area(&self) -> anyhow::Result<(DisplayInfo, PixelRect)> {
        with_per_monitor_dpi(|| {
            let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
            let mut info: MONITORINFO = unsafe { zeroed() };
            info.cbSize = size_of::<MONITORINFO>() as u32;
            if unsafe { GetMonitorInfoW(monitor, &mut info) } == FALSE {
                bail!("GetMonitorInfoW failed: {}", std::io::Error::last_os_error());
            }
            let (rc, work) = (info.rcMonitor, info.rcWork);
            let display = DisplayInfo {
                id: monitor_index(monitor).context("monitor not found in EnumDisplayMonitors")?,
    fn color_dialog(
        &self,
        owner: Option<WindowRef>,
        initial: bluesnap_core::Color,
    ) -> anyhow::Result<Option<bluesnap_core::Color>> {
        static CUSTOM_COLORS: OnceLock<Mutex<[u32; 16]>> = OnceLock::new();
        let mut custom = *CUSTOM_COLORS
            .get_or_init(|| Mutex::new([0x00FF_FFFF; 16]))
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let dialog_owner = DialogOwner::acquire(owner);
        let (r, g, b) = if initial.is_transparent() { (255, 255, 255) } else { (initial.r, initial.g, initial.b) };
        let mut cc: CHOOSECOLORW = unsafe { zeroed() };
        cc.lStructSize = size_of::<CHOOSECOLORW>() as u32;
        cc.hwndOwner = dialog_owner.hwnd;
        cc.rgbResult = (r as u32) | ((g as u32) << 8) | ((b as u32) << 16);
        cc.lpCustColors = custom.as_mut_ptr();
        cc.Flags = CC_FULLOPEN | CC_RGBINIT | CC_ANYCOLOR;
        let ok = unsafe { ChooseColorW(&mut cc) };
        drop(dialog_owner);
        *CUSTOM_COLORS.get().expect("initialized").lock().unwrap_or_else(|e| e.into_inner()) = custom;
        if ok == FALSE {
            return Ok(None);
        }
        Ok(Some(bluesnap_core::Color {
            r: (cc.rgbResult & 0xFF) as u8,
            g: ((cc.rgbResult >> 8) & 0xFF) as u8,
            b: ((cc.rgbResult >> 16) & 0xFF) as u8,
            a: 255,
        }))
    }

                bounds: NativeRect {
                    x: rc.left as f64,
                    y: rc.top as f64,
                    width: (rc.right - rc.left) as f64,
                    height: (rc.bottom - rc.top) as f64,
                },
                scale_factor: monitor_scale(monitor),
            };
            let work_rect = PixelRect {
                x: work.left - rc.left,
                y: work.top - rc.top,
                width: (work.right - work.left).max(1),
                height: (work.bottom - work.top).max(1),
            };
            Ok((display, work_rect))
        })
    }

    fn create_tray(&self, hotkey: &str, on_command: Box<dyn Fn(TrayCommand) + Send + Sync>) -> anyhow::Result<Tray> {
        let style = crate::tray::Style {
            icon_png: include_bytes!("../assets/tray-windows-32.png"),
            template: false,
            left_click_captures: true,
            start_at_login: ("Start with Windows", self.start_at_login()),
        };
        crate::tray::create(style, hotkey, on_command)
    }

    /// A value in the user's Run key, the same list Task Manager's Startup apps shows.
    fn start_at_login(&self) -> bool {
        let (key, name) = (wide(RUN_KEY), wide(RUN_VALUE));
        let status = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), null_mut())
        };
        status == ERROR_SUCCESS
    }

    fn set_start_at_login(&self, enabled: bool) -> anyhow::Result<()> {
        let (key, name) = (wide(RUN_KEY), wide(RUN_VALUE));
        let status = if enabled {
            let exe = std::env::current_exe().context("finding bluesnap.exe")?;
            let mut command: Vec<u16> = "\"".encode_utf16().collect();
            command.extend(exe.as_os_str().encode_wide());
            command.extend("\"\0".encode_utf16());
            let bytes = (command.len() * size_of::<u16>()) as u32;
            unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_SZ, command.as_ptr().cast(), bytes) }
        } else {
            match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) } {
                ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
                status => status,
            }
        };
        if status != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(status as i32)).context("updating the Run registry key");
        }
        Ok(())
    }

    fn permission_settings_url(&self) -> Option<&'static str> {
        None
    }

    /// Windows.Media.Ocr only knows languages whose language pack is installed, and
    /// its accuracy on CJK is poor, so use the embedded model instead.
    fn recognize_text(&self, width: u32, height: u32, rgba: &[u8], scale: f32) -> anyhow::Result<Vec<TextLine>> {
        crate::recognize_with_model(width, height, rgba, scale)
    }

    fn fonts(&self, family: FontFamily) -> Vec<FontSource> {
        let windir = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let dir = windir.join("Fonts");
        let file = |name: &str| FontSource::file(dir.join(name));
        // Segoe UI Semibold, Microsoft YaHei Bold, then regular faces.
        let cjk = ["msyhbd.ttc", "msyh.ttc", "simsun.ttc", "arial.ttf"].map(file);
        let primary = match family {
            FontFamily::Sans => vec![file("seguisb.ttf"), file("segoeuib.ttf"), file("segoeui.ttf")],
            FontFamily::Rounded => vec![crate::bundled_rounded()],
            FontFamily::Mono => vec![file("consolab.ttf"), file("consola.ttf")],
        };
        primary.into_iter().chain(cjk).collect()
    }
}

fn capture_under_cursor() -> anyhow::Result<Capture> {
    let started = Instant::now();
    let mut cursor = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut cursor) } == FALSE {
        bail!("GetCursorPos failed: {}", std::io::Error::last_os_error());
    }
    let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
    let mut info: MONITORINFO = unsafe { zeroed() };
    info.cbSize = size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == FALSE {
        bail!("GetMonitorInfoW failed: {}", std::io::Error::last_os_error());
    }
    let rc = info.rcMonitor;
    let display = DisplayInfo {
        id: monitor_index(monitor).context("monitor not found in EnumDisplayMonitors")?,
        bounds: NativeRect {
            x: rc.left as f64,
            y: rc.top as f64,
            width: (rc.right - rc.left) as f64,
            height: (rc.bottom - rc.top) as f64,
        },
        scale_factor: monitor_scale(monitor),
    };

    let image = capture_rect(&rc, SRCCOPY | CAPTUREBLT)?;
    let captured = started.elapsed();
    let windows = visible_windows(&rc);
    log::debug!(
        "captured {}x{} in {:?}, {} windows in {:?}",
        image.width,
        image.height,
        captured,
        windows.len(),
        started.elapsed() - captured
    );
    Ok(Capture { display, image, windows, permission_missing: false })
}

/// gpui identifies monitors by their position in `EnumDisplayMonitors` order.
fn monitor_index(target: HMONITOR) -> Option<u32> {
    unsafe extern "system" fn collect(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> i32 {
        unsafe { (*(data as *mut Vec<HMONITOR>)).push(m) };
        TRUE
    }
    let mut monitors: Vec<HMONITOR> = Vec::new();
    unsafe { EnumDisplayMonitors(null_mut(), null(), Some(collect), &mut monitors as *mut _ as LPARAM) };
    monitors.iter().position(|m| *m == target).map(|i| i as u32)
}

fn monitor_scale(monitor: HMONITOR) -> f32 {
    let (mut dx, mut dy) = (96u32, 96u32);
    unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
    dx as f32 / 96.0
}

/// BitBlt of the desktop into a top-down 32-bit DIB, which is BGRX.
fn capture_rect(rc: &RECT, rop: ROP_CODE) -> anyhow::Result<Image> {
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    if w <= 0 || h <= 0 {
        bail!("empty monitor rectangle");
    }
    unsafe {
        let screen = GetDC(null_mut());
        if screen.is_null() {
            bail!("GetDC failed");
        }
        let mem = CreateCompatibleDC(screen);
        if mem.is_null() {
            ReleaseDC(null_mut(), screen);
            bail!("CreateCompatibleDC failed");
        }
        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // negative: top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..zeroed()
        };
        let mut bits: *mut c_void = null_mut();
        let bitmap = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let result = if bitmap.is_null() || bits.is_null() {
            Err(anyhow::anyhow!("CreateDIBSection failed"))
        } else {
            let old = SelectObject(mem, bitmap);
            let ok = BitBlt(mem, 0, 0, w, h, screen, rc.left, rc.top, rop);
            SelectObject(mem, old);
            if ok == FALSE {
                Err(anyhow::anyhow!("BitBlt failed: {}", std::io::Error::last_os_error()))
            } else {
                let len = w as usize * h as usize * 4;
                let bgrx = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
                Ok(Image { width: w as u32, height: h as u32, bgrx })
            }
        };
        if !bitmap.is_null() {
            DeleteObject(bitmap);
        }
        DeleteDC(mem);
        ReleaseDC(null_mut(), screen);
        result
    }
}

/// Visible, non-minimized, non-cloaked top-level windows of other processes,
/// in Z order (topmost first), relative to `monitor`.
fn visible_windows(monitor: &RECT) -> Vec<PixelRect> {
    unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> i32 {
        unsafe { (*(data as *mut Vec<HWND>)).push(hwnd) };
        TRUE
    }
    let mut hwnds: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(collect), &mut hwnds as *mut _ as LPARAM) };

    let own_pid = unsafe { GetCurrentProcessId() };
    hwnds
        .into_iter()
        .filter(|&hwnd| unsafe { IsWindowVisible(hwnd) != FALSE && IsIconic(hwnd) == FALSE })
        .filter(|&hwnd| {
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
            pid != own_pid
        })
        .filter(|&hwnd| !is_cloaked(hwnd))
        .filter_map(frame_bounds)
        .filter_map(|r| {
            let l = r.left.max(monitor.left);
            let t = r.top.max(monitor.top);
            let rr = r.right.min(monitor.right);
            let b = r.bottom.min(monitor.bottom);
            (rr - l > 1 && b - t > 1).then(|| PixelRect {
                x: l - monitor.left,
                y: t - monitor.top,
                width: rr - l,
                height: b - t,
            })
        })
        .collect()
}

/// Windows on other virtual desktops and suspended UWP apps report as visible
/// but are "cloaked" by DWM.
fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let hr = unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, &mut cloaked as *mut _ as *mut c_void, size_of::<u32>() as u32)
    };
    hr == 0 && cloaked != 0
}

/// Visible frame, excluding the invisible resize borders that `GetWindowRect` includes.
fn frame_bounds(hwnd: HWND) -> Option<RECT> {
    let mut rect: RECT = unsafe { zeroed() };
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut rect as *mut _ as *mut c_void,
            size_of::<RECT>() as u32,
        )
    };
    if hr != 0 && unsafe { GetWindowRect(hwnd, &mut rect) } == FALSE {
        return None;
    }
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
}
