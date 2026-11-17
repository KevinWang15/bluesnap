//! macOS backend: CoreGraphics capture and window list, AppKit overlay setup.

use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, bail};
use objc2::rc::Retained;
use objc2::{AllocAnyThread, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy, NSModalResponseOK, NSOpenPanel,
    NSPopUpButton, NSRunningApplication, NSSavePanel, NSScreen, NSView, NSWindowAnimationBehavior, NSWindowStyleMask,
    NSWorkspace,
};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreateImage, CGColorSpace, CGImageByteOrderInfo, CGContext, CGDirectDisplayID, CGDisplayBounds, CGError, CGEvent, CGGetDisplaysWithPoint, CGImage,
    CGImageAlphaInfo, CGMainDisplayID, CGPreflightScreenCaptureAccess, CGRectMakeWithDictionaryRepresentation,
    CGRequestScreenCaptureAccess, CGWindowListCopyWindowInfo, CGWindowListOption, kCGWindowAlpha, kCGWindowBounds,
    kCGWindowLayer, kCGWindowOwnerPID,
};
use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
use objc2_vision::{VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::{
    Capture, DisplayInfo, FontFamily, FontSource, Image, NativeRect, PixelRect, Platform, SaveFormat, TextLine, Tray,
    TrayCommand, WindowRef,
};

pub struct Native;

/// gpui puts pop-up windows at `NSPopUpMenuWindowLevel` (101); helper bars and
/// dialogs that must sit above the overlay/dim layer use one level higher.
const POPUP_LEVEL: isize = 101;
const ABOVE_OVERLAY: isize = 102;

// A prewarm and an explicit recognition must not initialize Vision concurrently.
static VISION_LOCK: Mutex<()> = Mutex::new(());
static VISION_PREWARMED: OnceLock<()> = OnceLock::new();
/// Frontmost application before a screenshot capture started, so closing the
/// capture overlay returns keyboard focus straight to the user's app.
static PREVIOUS_APP: Mutex<Option<Retained<NSRunningApplication>>> = Mutex::new(None);

/// The bundle identifier, so the login item reads as bluesnap's.
const LAUNCH_AGENT_LABEL: &str = "com.kevinwang15.bluesnap";

fn launch_agent_path() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(home.join("Library/LaunchAgents").join(format!("{LAUNCH_AGENT_LABEL}.plist")))
}

impl Platform for Native {
    const DEFAULT_HOTKEY: &'static str = "Ctrl+Cmd+A";
    const TRAY_LOCATION: &'static str = "menu bar";
    const NOTIFY_TOP_RIGHT: bool = true;
    const OVERLAY_FULLSCREEN: bool = false;

    fn init(&self) -> anyhow::Result<()> {
    const HAS_COLOR_DIALOG: bool = true;
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
        let current_app = NSRunningApplication::currentApplication();
        let prev_app = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .filter(|app| **app != *current_app);
        *PREVIOUS_APP.lock().unwrap_or_else(|e| e.into_inner()) = prev_app;

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

        anyhow::ensure!(bounds.size.width > 0.0 && bounds.size.height > 0.0, "empty display bounds");
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
        let mtm = MainThreadMarker::new().context("configure_overlay must run on the main thread")?;
        let (view, ns_window) = ns_view_and_window(window)?;

        // gpui creates a titled non-activating panel; a titled window keeps its
        // traffic lights and is pushed below the menu bar. Borderless (without
        // `NonactivatingPanel`) covers the whole display and lets the overlay own
        // keyboard and CJK IME (`NSTextInputContext`) focus.
        ns_window.setStyleMask(NSWindowStyleMask::Borderless);
        ns_window.setInitialFirstResponder(Some(&view));
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
        #[allow(deprecated)] // Direct activation from a global hotkey; `activate()` alone is cooperative on macOS 14+.
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        ns_window.makeKeyAndOrderFront(None);
        // `setStyleMask` rebuilds the frame view hierarchy and resets `firstResponder`
        // to the window itself; since `GPUIView` does not override `acceptsFirstResponder`,
        // restore `view` explicitly so `keyDown:` reaches `GPUIView` instead of `NSBeep()`.
        ns_window.makeFirstResponder(Some(&view));
        Ok(())
    }

    fn restore_previous_app(&self) {
        let Some(prev) = PREVIOUS_APP.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        if prev.isTerminated() {
            return;
        }
        if let Some(mtm) = MainThreadMarker::new() {
            NSApplication::sharedApplication(mtm).yieldActivationToApplication(&prev);
        }
        #[allow(deprecated)] // Needed on macOS < 14; pairs with `yieldActivationToApplication` on macOS 14+.
        prev.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
    }

    fn raise_above_overlay(&self, window: &dyn HasWindowHandle) -> anyhow::Result<()> {
        let (view, ns_window) = ns_view_and_window(window)?;
        ns_window.setLevel(ABOVE_OVERLAY);
        ns_window.makeKeyAndOrderFront(None);
        ns_window.makeFirstResponder(Some(&view));
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
        window: WindowRef,
        display: &DisplayInfo,
        rect: PixelRect,
        hole: Option<PixelRect>,
        above: Option<WindowRef>,
    ) -> anyhow::Result<()> {
        let ns_window = raw_ns_window(window.0)?;
        ns_window.setStyleMask(NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel);
        ns_window.setHidesOnDeactivate(false);
        // The dim layer has nothing to click; letting all input through is simplest.
        ns_window.setIgnoresMouseEvents(hole.is_some());
        // Window shadows follow transparency, so one would be drawn inside the
        // hole, over the content being captured.
        ns_window.setHasShadow(false);
        ns_window.setAcceptsMouseMovedEvents(true);
        ns_window.setLevel(if above.is_some() { ABOVE_OVERLAY } else { POPUP_LEVEL });
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

    fn set_window_visible(&self, window: WindowRef, visible: bool) -> anyhow::Result<()> {
        let (view, ns_window) = raw_ns_view_and_window(window.0)?;
        if visible {
            ns_window.makeKeyAndOrderFront(None);
            ns_window.makeFirstResponder(Some(&view));
            ns_window.setAnimationBehavior(NSWindowAnimationBehavior::DocumentWindow);
        } else {
            ns_window.setAnimationBehavior(NSWindowAnimationBehavior::None);
            ns_window.orderOut(None);
        }
        Ok(())
    }

    fn save_dialog(
        &self,
        _owner: Option<WindowRef>,
        dir: &std::path::Path,
        stem: &str,
    ) -> anyhow::Result<Option<Option<(PathBuf, SaveFormat)>>> {
        let mtm = MainThreadMarker::new().context("save_dialog must run on the main thread")?;
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        let panel = NSSavePanel::savePanel(mtm);
        panel.setLevel(ABOVE_OVERLAY);
        if let Some(dir_str) = dir.to_str() {
            let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(dir_str), true);
            panel.setDirectoryURL(Some(&url));
        }
        panel.setNameFieldStringValue(&NSString::from_str(&format!("{stem}.png")));

        let popup = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: 260.0, height: 28.0 } },
            false,
        );
        popup.addItemWithTitle(&NSString::from_str("PNG Image (.png)"));
        popup.addItemWithTitle(&NSString::from_str("BlueSnap Editable (.bs)"));
        panel.setAccessoryView(Some(&popup));

        if panel.runModal() != NSModalResponseOK {
            return Ok(Some(None));
        }
        let Some(url) = panel.URL() else { return Ok(Some(None)) };
        let Some(ns_path) = url.path() else { return Ok(Some(None)) };
        let raw_path = PathBuf::from(ns_path.to_string());
        let format = match SaveFormat::from_path(&raw_path) {
            Some(SaveFormat::Bs) => SaveFormat::Bs,
            _ if popup.indexOfSelectedItem() == 1 => SaveFormat::Bs,
            _ => SaveFormat::Png,
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
        _owner: Option<WindowRef>,
        dir: &std::path::Path,
        _bs_only: bool,
    ) -> anyhow::Result<Option<Option<PathBuf>>> {
        let mtm = MainThreadMarker::new().context("open_dialog must run on the main thread")?;
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setLevel(ABOVE_OVERLAY);
        panel.setCanChooseFiles(true);
        panel.setCanChooseDirectories(false);
        panel.setAllowsMultipleSelection(false);
        if let Some(dir_str) = dir.to_str() {
            let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(dir_str), true);
            panel.setDirectoryURL(Some(&url));
        }
        if panel.runModal() != NSModalResponseOK {
            return Ok(Some(None));
        }
        let Some(url) = panel.URLs().firstObject() else { return Ok(Some(None)) };
        let Some(ns_path) = url.path() else { return Ok(Some(None)) };
        Ok(Some(Some(PathBuf::from(ns_path.to_string()))))
    }

    fn escape_pressed(&self) -> bool {
        // CombinedSessionState = 0, kVK_Escape = 0x35 (53).
        unsafe { CGEventSourceKeyState(0, 0x35) }
    }

    fn notification_area(&self) -> anyhow::Result<(DisplayInfo, PixelRect)> {
        let mtm = MainThreadMarker::new().context("notification_area must run on the main thread")?;
        let screen = NSScreen::screens(mtm).firstObject().context("no screen available")?;
        let id = CGMainDisplayID();
        let bounds = CGDisplayBounds(id);
        anyhow::ensure!(bounds.size.width > 0.0 && bounds.size.height > 0.0, "empty display bounds");
        let scale = screen.backingScaleFactor().max(1.0);
        let frame = screen.frame();
        let vis = screen.visibleFrame();
        // Cocoa screen frames use a bottom-left origin; convert `visibleFrame`
        // (which excludes the menu bar and Dock) to top-left display-local pixels.
        let left = (vis.origin.x - frame.origin.x).max(0.0);
        let top = ((frame.origin.y + frame.size.height) - (vis.origin.y + vis.size.height)).max(0.0);
        let px = |v: f64| (v * scale).round() as i32;
        let work = PixelRect {
            x: px(left),
            y: px(top),
            width: px(vis.size.width).max(1),
            height: px(vis.size.height).max(1),
        };
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
        Ok((display, work))
    }

    fn create_tray(&self, hotkey: &str, on_command: Box<dyn Fn(TrayCommand) + Send + Sync>) -> anyhow::Result<Tray> {
        let style = crate::tray::Style {
    fn color_dialog(
        &self,
        _owner: Option<WindowRef>,
        initial: bluesnap_core::Color,
    ) -> anyhow::Result<Option<bluesnap_core::Color>> {
        let (r, g, b) = if initial.is_transparent() { (255, 255, 255) } else { (initial.r, initial.g, initial.b) };
        let script = format!(
            "choose color default color {{{}, {}, {}}}",
            (r as u32) * 257,
            (g as u32) * 257,
            (b as u32) * 257
        );
        let out = std::process::Command::new("osascript").args(["-e", &script]).output()?;
        if !out.status.success() {
            return Ok(None);
        }
        let s = String::from_utf8_lossy(&out.stdout);
        let mut parts = s.trim().split(',').filter_map(|p| p.trim().parse::<u32>().ok());
        let (Some(cr), Some(cg), Some(cb)) = (parts.next(), parts.next(), parts.next()) else {
            return Ok(None);
        };
        Ok(Some(bluesnap_core::Color {
            r: ((cr + 128) / 257).min(255) as u8,
            g: ((cg + 128) / 257).min(255) as u8,
            b: ((cb + 128) / 257).min(255) as u8,
            a: 255,
        }))
    }

            icon_png: include_bytes!("../assets/tray-macos-template-36.png"),
            template: true,
            left_click_captures: false,
            start_at_login: ("Open at Login", self.start_at_login()),
        };
        crate::tray::create(style, hotkey, on_command)
    }

    /// A LaunchAgent, which works on every supported macOS (`SMAppService`
    /// needs macOS 13). launchd reads it at the next login, so writing it
    /// doesn't start a second copy now.
    fn start_at_login(&self) -> bool {
        launch_agent_path().is_some_and(|path| path.exists())
    }

    fn set_start_at_login(&self, enabled: bool) -> anyhow::Result<()> {
        let path = launch_agent_path().context("no home directory")?;
        if !enabled {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).context(format!("removing {}", path.display())),
                _ => Ok(()),
            };
        }
        let exe = std::env::current_exe().context("finding the bluesnap executable")?;
        let exe = exe.to_str().context("the bluesnap executable's path isn't UTF-8")?;
        let escaped = exe.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>Label</key>\n\t<string>{LAUNCH_AGENT_LABEL}</string>\n\
             \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{escaped}</string>\n\t</array>\n\
             \t<key>RunAtLoad</key>\n\t<true/>\n\
             \t<key>ProcessType</key>\n\t<string>Interactive</string>\n\
             \t<key>LimitLoadToSessionType</key>\n\t<string>Aqua</string>\n\
             </dict>\n\
             </plist>\n"
        );
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&path, plist).with_context(|| format!("writing {}", path.display()))
    }

    fn permission_settings_url(&self) -> Option<&'static str> {
        Some("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
    }

    fn prewarm_text_recognition(&self) -> anyhow::Result<()> {
        prewarm_vision()
    }

    /// Apple's Vision framework: no bundled model, and good at Chinese.
    fn recognize_text(&self, width: u32, height: u32, rgba: &[u8], _scale: f32) -> anyhow::Result<Vec<TextLine>> {
        recognize_with_vision(width, height, rgba)
    }

    fn fonts(&self, family: FontFamily) -> Vec<FontSource> {
        let file = |path: &str| FontSource::file(path);
        // SF Pro, SF Pro Rounded and SF Mono are variable fonts: semibold.
        let primary = match family {
            FontFamily::Sans => vec![file("/System/Library/Fonts/SFNS.ttf").weight(600.0)],
            FontFamily::Rounded => vec![file("/System/Library/Fonts/SFNSRounded.ttf").weight(600.0)],
            FontFamily::Mono => vec![
                file("/System/Library/Fonts/SFNSMono.ttf").weight(600.0),
                file("/System/Library/Fonts/Menlo.ttc").face(1), // Menlo Bold
            ],
        };
        let fallbacks = [
            file("/System/Library/Fonts/Helvetica.ttc").face(1), // Helvetica Bold
            file("/System/Library/Fonts/PingFang.ttc"),
            file("/System/Library/Fonts/Hiragino Sans GB.ttc"),
            file("/System/Library/Fonts/STHeiti Medium.ttc"),
            file("/System/Library/Fonts/Supplemental/Arial Unicode.ttf"),
            file("/Library/Fonts/Arial Unicode.ttf"),
        ];
        primary.into_iter().chain(fallbacks).collect()
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    // Not exposed by objc2-core-graphics with our minimal feature set.
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: Option<&CGColorSpace>,
        bitmap_info: u32,
    ) -> Option<NonNull<CGContext>>;
    fn CGEventSourceKeyState(state_id: i32, key: u16) -> bool;
}

fn raw_ns_view_and_window(raw: RawWindowHandle) -> anyhow::Result<(Retained<NSView>, Retained<objc2_app_kit::NSWindow>)> {
    let RawWindowHandle::AppKit(handle) = raw else {
        bail!("not an AppKit window");
    };
    // SAFETY: the handle comes from a live window on the main thread.
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    let win = view.window().context("view has no window")?;
    Ok((view.retain(), win))
}

fn raw_ns_window(raw: RawWindowHandle) -> anyhow::Result<Retained<objc2_app_kit::NSWindow>> {
    Ok(raw_ns_view_and_window(raw)?.1)
}

fn ns_view_and_window(
    window: &dyn HasWindowHandle,
) -> anyhow::Result<(Retained<NSView>, Retained<objc2_app_kit::NSWindow>)> {
    raw_ns_view_and_window(window.window_handle().map_err(|e| anyhow::anyhow!("{e:?}"))?.as_raw())
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
                x: x.round() as i32,
                y: y.round() as i32,
                width: (b.size.width * w as f64).round() as i32,
                height: (b.size.height * h as f64).round() as i32,
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
