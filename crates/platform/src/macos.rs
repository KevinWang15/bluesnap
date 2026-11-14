//! macOS backend: CoreGraphics capture and window list, AppKit overlay setup.

use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, bail};
use objc2::rc::Retained;
use objc2::{AllocAnyThread, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSView, NSWindowStyleMask};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreateImage, CGColorSpace, CGImageByteOrderInfo, CGContext, CGDirectDisplayID, CGDisplayBounds, CGError, CGEvent, CGGetDisplaysWithPoint, CGImage,
    CGImageAlphaInfo, CGMainDisplayID, CGPreflightScreenCaptureAccess, CGRectMakeWithDictionaryRepresentation,
    CGRequestScreenCaptureAccess, CGWindowListCopyWindowInfo, CGWindowListOption, kCGWindowAlpha, kCGWindowBounds,
    kCGWindowLayer, kCGWindowOwnerPID,
};
use objc2_foundation::{NSArray, NSDictionary, NSString};
use objc2_vision::{VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::{Capture, DisplayInfo, Image, NativeRect, PixelRect, Platform, TextLine, Tray, TrayCommand};

pub struct Native;

// A prewarm and an explicit recognition must not initialize Vision concurrently.
static VISION_LOCK: Mutex<()> = Mutex::new(());
static VISION_PREWARMED: OnceLock<()> = OnceLock::new();

impl Platform for Native {
    const DEFAULT_HOTKEY: &'static str = "Ctrl+Cmd+A";
    const OVERLAY_FULLSCREEN: bool = false;

    fn init(&self) -> anyhow::Result<()> {
        let mtm = MainThreadMarker::new().context("init must run on the main thread")?;
        // Background utility: no Dock icon or menu bar.
        NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        if !CGPreflightScreenCaptureAccess() {
            // Shows the system prompt once; captures contain only the wallpaper until granted.
            log::warn!("screen recording permission not granted; requesting it");
            CGRequestScreenCaptureAccess();
        }
        Ok(())
    }

    fn capture_display_under_cursor(&self) -> anyhow::Result<Capture> {
        let started = Instant::now();
        let cursor = CGEvent::location(CGEvent::new(None).as_deref());
        let mut id: CGDirectDisplayID = 0;
        let mut count = 0u32;
        let err = unsafe { CGGetDisplaysWithPoint(cursor, 1, &mut id, &mut count) };
        if err != CGError::Success || count == 0 {
            id = CGMainDisplayID();
        }
        let bounds = CGDisplayBounds(id);

        #[allow(deprecated)] // ScreenCaptureKit is async and much slower for a single frame.
        let image = objc2_core_graphics::CGDisplayCreateImage(id).context("CGDisplayCreateImage failed")?;
        let image = to_bgrx(&image)?;
        let captured = started.elapsed();

        let scale = image.width as f64 / bounds.size.width;
        let display = DisplayInfo {
            id,
            bounds: NativeRect {
                x: bounds.origin.x,
                y: bounds.origin.y,
                width: bounds.size.width,
                height: bounds.size.height,
            },
            scale_factor: scale as f32,
        };
        let windows = visible_windows(&bounds, scale);
        // Without Screen Recording permission macOS still returns an image, but
        // with only the wallpaper (or plain gray) where windows should be.
        let permission_missing = !CGPreflightScreenCaptureAccess();
        log::debug!(
            "captured {}x{} in {:?}, {} windows in {:?}",
            image.width,
            image.height,
            captured,
            windows.len(),
            started.elapsed() - captured
        );
        Ok(Capture { display, image, windows, permission_missing })
    }

    fn configure_overlay(&self, window: &dyn HasWindowHandle, display: &DisplayInfo) -> anyhow::Result<()> {
        let ns_window = ns_window(window)?;

        // gpui creates a titled panel; a titled window keeps its traffic lights and is
        // pushed below the menu bar. Borderless covers the whole display.
        ns_window.setStyleMask(NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel);
        // GPUI leaves this disabled for pop-up windows; window picking needs hover events.
        ns_window.setAcceptsMouseMovedEvents(true);
        ns_window.setHidesOnDeactivate(false);

        // Cocoa frames are bottom-left based, measured from the menu-bar display.
        let b = display.bounds;
        let primary_height = CGDisplayBounds(CGMainDisplayID()).size.height;
        let frame = CGRect {
            origin: CGPoint { x: b.x, y: primary_height - (b.y + b.height) },
            size: CGSize { width: b.width, height: b.height },
        };
        ns_window.setFrame_display(frame, true);
        ns_window.makeKeyAndOrderFront(None);
        Ok(())
    }

    fn capture_region(&self, display: &DisplayInfo, rect: PixelRect) -> anyhow::Result<Image> {
        // The rect is in display-local points; the image comes back in pixels.
        let s = display.scale_factor as f64;
        let r = CGRect {
            origin: CGPoint { x: rect.x as f64 / s, y: rect.y as f64 / s },
            size: CGSize { width: rect.width as f64 / s, height: rect.height as f64 / s },
        };
        #[allow(deprecated)] // see capture_display_under_cursor
        let image = objc2_core_graphics::CGDisplayCreateImageForRect(display.id, r)
            .context("CGDisplayCreateImageForRect failed")?;
        to_bgrx(&image)
    }

    fn place_floating_window(
        &self,
        window: &dyn HasWindowHandle,
        display: &DisplayInfo,
        rect: PixelRect,
        hole: Option<PixelRect>,
    ) -> anyhow::Result<()> {
        let ns_window = ns_window(window)?;
        ns_window.setStyleMask(NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel);
        ns_window.setHidesOnDeactivate(false);
        // The frame has nothing to click; letting all input through is simplest.
        ns_window.setIgnoresMouseEvents(hole.is_some());
        ns_window.setAcceptsMouseMovedEvents(true);
        let s = display.scale_factor as f64;
        let b = display.bounds;
        let primary_height = CGDisplayBounds(CGMainDisplayID()).size.height;
        let (w, h) = (rect.width as f64 / s, rect.height as f64 / s);
        let top = b.y + rect.y as f64 / s;
        let frame = CGRect {
            origin: CGPoint { x: b.x + rect.x as f64 / s, y: primary_height - (top + h) },
            size: CGSize { width: w, height: h },
        };
        ns_window.setFrame_display(frame, true);
        ns_window.orderFrontRegardless();
        Ok(())
    }

    fn create_tray(&self, hotkey: &str, on_command: Box<dyn Fn(TrayCommand) + Send + Sync>) -> anyhow::Result<Tray> {
        let style = crate::tray::Style {
            icon_png: include_bytes!("../assets/tray-macos-template-36.png"),
            template: true,
            left_click_captures: false,
        };
        crate::tray::create(style, hotkey, on_command)
    }

    fn permission_settings_url(&self) -> Option<&'static str> {
        Some("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
    }

    fn prewarm_text_recognition(&self) -> anyhow::Result<()> {
        prewarm_vision()
    }

    /// Apple's Vision framework: no bundled model, and good at Chinese.
    fn recognize_text(&self, width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<Vec<TextLine>> {
        recognize_with_vision(width, height, rgba)
    }

    fn font_paths(&self) -> Vec<PathBuf> {
        [
            "/System/Library/Fonts/Helvetica.ttc",
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/STHeiti Medium.ttc",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
            "/Library/Fonts/Arial Unicode.ttf",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    // Not exposed by objc2-core-graphics.
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: Option<&CGColorSpace>,
        bitmap_info: u32,
    ) -> Option<NonNull<CGContext>>;
}

fn ns_window(window: &dyn HasWindowHandle) -> anyhow::Result<Retained<objc2_app_kit::NSWindow>> {
    let handle = window.window_handle().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        bail!("not an AppKit window");
    };
    // SAFETY: the handle comes from a live window on the main thread.
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    view.window().context("view has no window")
}

fn prewarm_vision() -> anyhow::Result<()> {
    if VISION_PREWARMED.get().is_some() {
        return Ok(());
    }
    let (width, height, rgba) = decode_prewarm_image()?;
    let started = Instant::now();
    let result = objc2::rc::autoreleasepool(|_| {
        let _guard = VISION_LOCK.lock().expect("Vision lock poisoned");
        if VISION_PREWARMED.get().is_some() {
            return Ok(());
        }
        let result = recognize_with_vision_locked(width, height, &rgba);
        if result.is_ok() {
            VISION_PREWARMED.set(()).ok();
        }
        result.map(|_| ())
    });
    if result.is_ok() {
        log::debug!("Vision prewarm took {:?}", started.elapsed());
    }
    result
}

fn decode_prewarm_image() -> anyhow::Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(include_bytes!("../assets/vision-prewarm.png")));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let mut pixels = vec![0; reader.output_buffer_size().context("prewarm image too large")?];
    let info = reader.next_frame(&mut pixels)?;
    pixels.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => pixels,
        png::ColorType::Rgb => pixels.as_chunks::<3>().0.iter().flat_map(|&[r, g, b]| [r, g, b, 255]).collect(),
        _ => bail!("prewarm image must be RGB or RGBA"),
    };
    Ok((info.width, info.height, rgba))
}

fn recognize_with_vision(width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<Vec<TextLine>> {
    objc2::rc::autoreleasepool(|_| {
        let _guard = VISION_LOCK.lock().expect("Vision lock poisoned");
        let result = recognize_with_vision_locked(width, height, rgba);
        if result.is_ok() {
            VISION_PREWARMED.set(()).ok();
        }
        result
    })
}

fn recognize_with_vision_locked(width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<Vec<TextLine>> {
    let (w, h) = (width as usize, height as usize);
    anyhow::ensure!(rgba.len() == w * h * 4, "buffer size mismatch");
    let mut rgba = rgba.to_vec();
    let space = CGColorSpace::new_device_rgb();
    // SAFETY: `rgba` outlives the context; the CGImage made from it is a copy.
    let context = unsafe {
        CGBitmapContextCreate(rgba.as_mut_ptr().cast(), w, h, 8, w * 4, space.as_deref(), CGImageAlphaInfo::NoneSkipLast.0)
    }
    .context("CGBitmapContextCreate failed")?;
    let context = unsafe { CFRetained::from_raw(context) };
    let cg_image = CGBitmapContextCreateImage(Some(&context)).context("CGBitmapContextCreateImage failed")?;
    drop(context);

    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    // Chinese can only be combined with English in Vision; listing Chinese first
    // makes it the priority, and its model reads Latin text too.
    let languages = ["zh-Hans", "zh-Hant", "en-US"].map(NSString::from_str);
    request.setRecognitionLanguages(&NSArray::from_retained_slice(&languages));

    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(VNImageRequestHandler::alloc(), &cg_image, &NSDictionary::new())
    };
    let requests: Retained<NSArray<VNRequest>> = NSArray::from_slice(&[&**request]);
    handler.performRequests_error(&requests).map_err(|e| anyhow::anyhow!("Vision: {}", e.localizedDescription()))?;

    let mut lines = Vec::new();
    for observation in request.results().iter().flat_map(|r| r.iter()) {
        let Some(best) = observation.topCandidates(1).firstObject() else { continue };
        // Normalized coordinates with a bottom-left origin.
        let b = unsafe { observation.boundingBox() };
        let (x, y) = (b.origin.x * w as f64, (1.0 - b.origin.y - b.size.height) * h as f64);
        lines.push(TextLine {
            text: best.string().to_string(),
            bounds: PixelRect {
                x: x as i32,
                y: y as i32,
                width: (b.size.width * w as f64) as i32,
                height: (b.size.height * h as f64) as i32,
            },
        });
    }
    Ok(lines)
}

/// Draws `image` into a BGRX buffer we own: the display's native layout, so with
/// the image's own color space this is a plain copy, with no color conversion.
fn to_bgrx(image: &CGImage) -> anyhow::Result<Image> {
    let (w, h) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    let mut bgrx = vec![0u8; w * h * 4];
    // Little-endian 32-bit "XRGB" is B, G, R, X in memory.
    let info = CGImageAlphaInfo::NoneSkipFirst.0 | CGImageByteOrderInfo::Order32Little.0;
    let space = CGImage::color_space(Some(image)).or_else(CGColorSpace::new_device_rgb);
    // SAFETY: `bgrx` outlives the context, which is released at the end of this function.
    let context = unsafe { CGBitmapContextCreate(bgrx.as_mut_ptr().cast(), w, h, 8, w * 4, space.as_deref(), info) }
        .or_else(|| {
            let srgb = CGColorSpace::new_device_rgb();
            unsafe { CGBitmapContextCreate(bgrx.as_mut_ptr().cast(), w, h, 8, w * 4, srgb.as_deref(), info) }
        })
    .context("CGBitmapContextCreate failed")?;
    // SAFETY: Create functions return a +1 reference.
    let context = unsafe { CFRetained::from_raw(context) };
    let rect = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: w as f64, height: h as f64 } };
    CGContext::draw_image(Some(&context), rect, Some(image));
    drop(context);
    Ok(Image { width: w as u32, height: h as u32, bgrx })
}

fn is_pickable_window_layer(layer: i32) -> bool {
    // The Dock can report a display-sized layer-20 window, and layers above it
    // include the menu bar and other system UI rather than pickable app windows.
    (0..20).contains(&layer)
}

/// On-screen windows of other apps in front-to-back order, relative to `display`
/// and converted from points to image pixels.
fn visible_windows(display: &CGRect, scale: f64) -> Vec<PixelRect> {
    let options = CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
    let Some(list) = CGWindowListCopyWindowInfo(options, 0) else { return Vec::new() };
    let own_pid = std::process::id() as i32;

    let mut out = Vec::new();
    for i in 0..list.count() {
        // SAFETY: the window list contains CFDictionaries and outlives these borrows.
        let dict: &CFDictionary = unsafe { &*list.value_at_index(i).cast::<CFDictionary>() };
        let number = |key: &CFString| -> Option<&CFNumber> {
            let v = unsafe { dict.value((key as *const CFString).cast()) };
            (!v.is_null()).then(|| unsafe { &*v.cast::<CFNumber>() })
        };
        let (layer, pid, alpha) = unsafe {
            (
                number(kCGWindowLayer).and_then(|n| n.as_i32()).unwrap_or(0),
                number(kCGWindowOwnerPID).and_then(|n| n.as_i32()).unwrap_or(0),
                number(kCGWindowAlpha).and_then(|n| n.as_f64()).unwrap_or(1.0),
            )
        };
        if pid == own_pid || alpha <= 0.0 || !is_pickable_window_layer(layer) {
            continue;
        }
        let mut r = CGRect::default();
        let ok = unsafe {
            let v = dict.value((kCGWindowBounds as *const CFString).cast());
            !v.is_null() && CGRectMakeWithDictionaryRepresentation(Some(&*v.cast::<CFDictionary>()), &mut r)
        };
        if !ok {
            continue;
        }
        let l = r.origin.x.max(display.origin.x);
        let t = r.origin.y.max(display.origin.y);
        let rr = (r.origin.x + r.size.width).min(display.origin.x + display.size.width);
        let b = (r.origin.y + r.size.height).min(display.origin.y + display.size.height);
        if rr - l < 2.0 || b - t < 2.0 {
            continue;
        }
        let px = |v: f64| (v * scale).round() as i32;
        let (x0, y0) = (px(l - display.origin.x), px(t - display.origin.y));
        let (x1, y1) = (px(rr - display.origin.x), px(b - display.origin.y));
        out.push(PixelRect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 });
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn dock_and_system_ui_layers_are_not_pickable() {
        assert!(super::is_pickable_window_layer(0));
        assert!(super::is_pickable_window_layer(8));
        assert!(super::is_pickable_window_layer(19));
        assert!(!super::is_pickable_window_layer(-1));
        assert!(!super::is_pickable_window_layer(20));
        assert!(!super::is_pickable_window_layer(24));
        assert!(!super::is_pickable_window_layer(25));
    }

    #[test]
    fn prewarm_image_decodes_to_rgba() {
        let (width, height, rgba) = super::decode_prewarm_image().unwrap();
        assert_eq!((width, height), (1000, 180));
        assert_eq!(rgba.len(), width as usize * height as usize * 4);
        assert!(rgba.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255));
    }
}
