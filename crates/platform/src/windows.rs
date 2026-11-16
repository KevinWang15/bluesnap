//! Windows backend: GDI capture, `EnumWindows` + DWM for window bounds.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::time::Instant;

use anyhow::{Context, bail};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, FALSE, HWND, LPARAM, POINT, RECT, TRUE};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    ClientToScreen, CombineRgn, CreateRectRgn, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetMonitorInfoW,
    HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, RGN_DIFF, ROP_CODE, ReleaseDC, SRCCOPY,
    SelectObject, SetWindowRgn,
};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI, SetThreadDpiAwarenessContext,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetCursorPos, GetWindowRect, GetWindowThreadProcessId, HWND_TOPMOST, IsIconic, IsWindowVisible,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetForegroundWindow, SetWindowPos,
};

use crate::{Capture, DisplayInfo, FontFamily, FontSource, Image, NativeRect, PixelRect, Platform, TextLine, Tray, TrayCommand};

pub struct Native;

/// Programs started at login, per user (under `HKEY_CURRENT_USER`).
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "bluesnap";

/// A NUL-terminated UTF-16 string.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

impl Platform for Native {
    const DEFAULT_HOTKEY: &'static str = "Ctrl+Alt+A";
    const OVERLAY_FULLSCREEN: bool = true;

    fn init(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn capture_display_under_cursor(&self) -> anyhow::Result<Capture> {
        // All coordinates below must be physical pixels. gpui's manifest already makes
        // the process per-monitor DPI aware; this guards against builds without it.
        let previous = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let result = capture_under_cursor();
        if !previous.is_null() {
            unsafe { SetThreadDpiAwarenessContext(previous) };
        }
        result
    }

    fn configure_overlay(&self, window: &dyn HasWindowHandle, display: &DisplayInfo) -> anyhow::Result<()> {
        let RawWindowHandle::Win32(handle) = window.window_handle().map_err(|e| anyhow::anyhow!("{e:?}"))?.as_raw() else {
            bail!("not a Win32 window");
        };
        let hwnd = handle.hwnd.get() as HWND;
        let _ = display; // gpui's fullscreen mode already covers exactly this monitor.
        // Always on top, above the taskbar. Position and size stay as gpui set them.
        let ok = unsafe { SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW) };
        if ok == FALSE {
            bail!("SetWindowPos failed: {}", std::io::Error::last_os_error());
        }
        // Allowed because the process just received the hotkey input.
        unsafe { SetForegroundWindow(hwnd) };
        Ok(())
    }

    fn raise_above_overlay(&self, window: &dyn HasWindowHandle) -> anyhow::Result<()> {
        let RawWindowHandle::Win32(handle) = window.window_handle().map_err(|e| anyhow::anyhow!("{e:?}"))?.as_raw() else {
            bail!("not a Win32 window");
        };
        let hwnd = handle.hwnd.get() as HWND;
        // Topmost too, and set later, so it sits above the overlay.
        let ok = unsafe { SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW) };
        if ok == FALSE {
            bail!("SetWindowPos failed: {}", std::io::Error::last_os_error());
        }
        unsafe { SetForegroundWindow(hwnd) };
        Ok(())
    }

    fn capture_region(&self, display: &DisplayInfo, rect: PixelRect) -> anyhow::Result<Image> {
        let previous = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let (x, y) = (display.bounds.x as i32 + rect.x, display.bounds.y as i32 + rect.y);
        let rc = RECT { left: x, top: y, right: x + rect.width, bottom: y + rect.height };
        // No CAPTUREBLT: with DWM it isn't needed, and it makes the cursor flicker
        // when called many times a second.
        let result = capture_rect(&rc, SRCCOPY);
        if !previous.is_null() {
            unsafe { SetThreadDpiAwarenessContext(previous) };
        }
        result
    }

    fn place_floating_window(
        &self,
        window: &dyn HasWindowHandle,
        display: &DisplayInfo,
        rect: PixelRect,
        hole: Option<PixelRect>,
    ) -> anyhow::Result<()> {
        let RawWindowHandle::Win32(handle) = window.window_handle().map_err(|e| anyhow::anyhow!("{e:?}"))?.as_raw()
        else {
            bail!("not a Win32 window");
        };
        let hwnd = handle.hwnd.get() as HWND;
        let (x, y) = (display.bounds.x as i32 + rect.x, display.bounds.y as i32 + rect.y);
        let flags = SWP_NOACTIVATE | SWP_SHOWWINDOW;
        unsafe {
            if SetWindowPos(hwnd, HWND_TOPMOST, x, y, rect.width, rect.height, flags) == FALSE {
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
            if (left, top, right, bottom) != (0, 0, 0, 0) {
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    x - left,
                    y - top,
                    rect.width + left + right,
                    rect.height + top + bottom,
                    SWP_NOACTIVATE,
                );
            }
            // Clip the window to its client area (the invisible borders would
            // otherwise swallow clicks next to it), minus the pass-through hole.
            let region = CreateRectRgn(left, top, left + rect.width, top + rect.height);
            if let Some(h) = hole {
                let inner = CreateRectRgn(left + h.x, top + h.y, left + h.x + h.width, top + h.y + h.height);
                CombineRgn(region, region, inner, RGN_DIFF);
                DeleteObject(inner);
            }
            // On success the system owns the region.
            if SetWindowRgn(hwnd, region, TRUE) == 0 {
                DeleteObject(region);
            }
        }
        Ok(())
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
