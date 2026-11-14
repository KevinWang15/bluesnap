//! Windows backend: GDI capture, `EnumWindows` + DWM for window bounds.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::time::Instant;

use anyhow::{Context, bail};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::{FALSE, HWND, LPARAM, POINT, RECT, TRUE};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST,
    MONITORINFO, MonitorFromPoint, ReleaseDC, SRCCOPY, SelectObject,
};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI, SetThreadDpiAwarenessContext,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetCursorPos, GetWindowRect, GetWindowThreadProcessId, HWND_TOPMOST, IsIconic, IsWindowVisible,
    SWP_SHOWWINDOW, SetForegroundWindow, SetWindowPos,
};

use crate::{Capture, DisplayInfo, Image, NativeRect, PixelRect, Platform};

pub struct Native;

impl Platform for Native {
    const DEFAULT_HOTKEY: &'static str = "Ctrl+Alt+A";

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
        let b = display.bounds;
        // Exact physical placement (gpui positions in logical pixels, which can be off
        // by a pixel after rounding) and always-on-top, above the taskbar.
        let ok = unsafe {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                b.x as i32,
                b.y as i32,
                b.width as i32,
                b.height as i32,
                SWP_SHOWWINDOW,
            )
        };
        if ok == FALSE {
            bail!("SetWindowPos failed: {}", std::io::Error::last_os_error());
        }
        // Allowed because the process just received the hotkey input.
        unsafe { SetForegroundWindow(hwnd) };
        Ok(())
    }

    fn font_paths(&self) -> Vec<PathBuf> {
        let windir = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let fonts = windir.join("Fonts");
        ["segoeui.ttf", "msyh.ttc", "simsun.ttc", "arial.ttf"].iter().map(|f| fonts.join(f)).collect()
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

    let image = capture_rect(&rc)?;
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
    Ok(Capture { display, image, windows })
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

/// BitBlt of the desktop into a top-down 32-bit DIB.
fn capture_rect(rc: &RECT) -> anyhow::Result<Image> {
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
            let ok = BitBlt(mem, 0, 0, w, h, screen, rc.left, rc.top, SRCCOPY | CAPTUREBLT);
            SelectObject(mem, old);
            if ok == FALSE {
                Err(anyhow::anyhow!("BitBlt failed: {}", std::io::Error::last_os_error()))
            } else {
                let len = w as usize * h as usize * 4;
                let mut rgba = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
                for px in rgba.as_chunks_mut::<4>().0 {
                    px.swap(0, 2); // BGRX -> RGBX
                    px[3] = 255; // GDI leaves alpha undefined
                }
                Ok(Image { width: w as u32, height: h as u32, rgba })
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
