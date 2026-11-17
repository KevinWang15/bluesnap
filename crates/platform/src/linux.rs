//! Linux backend: not implemented yet.
//!
//! The app builds on Linux so the shared code can be developed and tested here,
//! but capturing fails with an error. A real backend would capture with X11
//! `GetImage` (or the XDG screenshot portal on Wayland) and list windows from
//! `_NET_CLIENT_LIST_STACKING`.
//!
//! Demo mode stands in for capturing so the real UI can be driven and
//! recorded (see `demo/`): with `BLUESNAP_DEMO_SCREEN` set, the "screen" is
//! that PNG. Optional:
//!
//! - `BLUESNAP_DEMO_SCALE`: its pixels per logical pixel (default 1);
//! - `BLUESNAP_DEMO_WINDOWS`: `x,y,w,h;…` window rectangles, topmost first;
//! - `BLUESNAP_DEMO_SCROLL`: `page.png@x,y,w,h`, a tall page shown in that
//!   rectangle, scrolled by the number of pixels in the file named by
//!   `BLUESNAP_DEMO_SCROLL_OFFSET` (read on every capture), for scrolling
//!   screenshots.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, bail};
use raw_window_handle::HasWindowHandle;

use crate::{AppRole, Capture, DisplayInfo, FontFamily, FontSource, Platform, TextLine, Tray, TrayCommand};

pub struct Native;

/// A PNG as opaque BGRX rows.
struct Raster {
    width: usize,
    height: usize,
    bgrx: Vec<u8>,
}

fn load_png(path: &Path) -> anyhow::Result<Raster> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().context("image too large")?];
    let info = reader.next_frame(&mut buf)?;
    let (width, height) = (info.width as usize, info.height as usize);
    let channels = info.line_size / width;
    let mut bgrx = Vec::with_capacity(width * height * 4);
    for px in buf[..info.line_size * height].chunks_exact(channels) {
        let (r, g, b) = if channels >= 3 { (px[0], px[1], px[2]) } else { (px[0], px[0], px[0]) };
        bgrx.extend_from_slice(&[b, g, r, 255]);
    }
    Ok(Raster { width, height, bgrx })
}

/// The demo "screen" (see the module docs).
struct Demo {
    screen: Raster,
    scale: f32,
    windows: Vec<crate::PixelRect>,
    scroll: Option<(Raster, crate::PixelRect)>,
}

fn rect(spec: &str) -> Option<crate::PixelRect> {
    let v: Vec<i32> = spec.split(',').map(|n| n.trim().parse().ok()).collect::<Option<_>>()?;
    let [x, y, width, height] = v[..] else { return None };
    Some(crate::PixelRect { x, y, width, height })
}

fn demo() -> anyhow::Result<&'static Demo> {
    static DEMO: OnceLock<Result<Demo, String>> = OnceLock::new();
    let demo = DEMO.get_or_init(|| {
        let load = || -> anyhow::Result<Demo> {
            let screen = std::env::var("BLUESNAP_DEMO_SCREEN").map_err(|_| anyhow::anyhow!("screen capture is not implemented on Linux yet"))?;
            let scale = std::env::var("BLUESNAP_DEMO_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
            let windows = std::env::var("BLUESNAP_DEMO_WINDOWS").unwrap_or_default().split(';').filter_map(rect).collect();
            let scroll = match std::env::var("BLUESNAP_DEMO_SCROLL") {
                Ok(spec) => {
                    let (page, at) = spec.split_once('@').context("BLUESNAP_DEMO_SCROLL is page.png@x,y,w,h")?;
                    Some((load_png(Path::new(page))?, rect(at).context("bad BLUESNAP_DEMO_SCROLL rectangle")?))
                }
                Err(_) => None,
            };
            Ok(Demo { screen: load_png(Path::new(&screen))?, scale, windows, scroll })
        };
        load().map_err(|e| format!("{e:#}"))
    });
    demo.as_ref().map_err(|e| anyhow::anyhow!("{e}"))
}

impl Demo {
    fn display(&self) -> DisplayInfo {
        let (w, h) = (self.screen.width as f64 / self.scale as f64, self.screen.height as f64 / self.scale as f64);
        DisplayInfo { id: 0, bounds: crate::NativeRect { x: 0.0, y: 0.0, width: w, height: h }, scale_factor: self.scale }
    }

    /// The screen now: the scrolling page drawn at its current offset.
    fn pixels(&self) -> Vec<u8> {
        let mut out = self.screen.bgrx.clone();
        if let Some((page, at)) = &self.scroll {
            let offset = std::env::var_os("BLUESNAP_DEMO_SCROLL_OFFSET")
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(0)
                .min(page.height.saturating_sub(at.height as usize));
            let w = (at.width as usize).min(page.width);
            for y in 0..(at.height as usize).min(page.height - offset) {
                let src = (offset + y) * page.width * 4;
                let dst = ((at.y as usize + y) * self.screen.width + at.x as usize) * 4;
                if dst + w * 4 <= out.len() {
                    out[dst..dst + w * 4].copy_from_slice(&page.bgrx[src..src + w * 4]);
                }
            }
        }
        out
    }
}

impl Platform for Native {
    const DEFAULT_HOTKEY: &'static str = "Ctrl+Alt+A";
    const TRAY_LOCATION: &'static str = "system tray";
    const OVERLAY_FULLSCREEN: bool = false;

    fn init(&self, _role: AppRole) -> anyhow::Result<()> {
        Ok(())
    }

    fn capture_display_under_cursor(&self) -> anyhow::Result<Capture> {
        let demo = demo()?;
        let image = crate::Image { width: demo.screen.width as u32, height: demo.screen.height as u32, bgrx: demo.pixels() };
        Ok(Capture { display: demo.display(), image, windows: demo.windows.clone(), permission_missing: false })
    }

    fn configure_overlay(&self, _window: &dyn HasWindowHandle, _display: &DisplayInfo) -> anyhow::Result<()> {
        Ok(())
    }

    fn capture_region(&self, _display: &DisplayInfo, rect: crate::PixelRect) -> anyhow::Result<crate::Image> {
        let demo = demo()?;
        let screen = demo.pixels();
        let (sw, sh) = (demo.screen.width as i32, demo.screen.height as i32);
        if rect.width <= 0 || rect.height <= 0 || rect.x < 0 || rect.y < 0 || rect.x + rect.width > sw || rect.y + rect.height > sh {
            bail!("region outside the demo screen");
        }
        let mut out = Vec::with_capacity((rect.width * rect.height * 4) as usize);
        for y in rect.y..rect.y + rect.height {
            let i = ((y * sw + rect.x) * 4) as usize;
            out.extend_from_slice(&screen[i..i + rect.width as usize * 4]);
        }
        Ok(crate::Image { width: rect.width as u32, height: rect.height as u32, bgrx: out })
    }

    fn place_floating_window(
        &self,
        _window: crate::WindowRef,
        _display: &DisplayInfo,
        _rect: crate::PixelRect,
        _hole: Option<crate::PixelRect>,
        _above: Option<crate::WindowRef>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn create_tray(&self, _hotkey: &str, _on_command: Box<dyn Fn(TrayCommand) + Send + Sync>) -> anyhow::Result<Tray> {
        bail!("tray icon is not implemented on Linux yet")
    }

    fn notification_area(&self) -> anyhow::Result<(DisplayInfo, crate::PixelRect)> {
        let demo = demo()?;
        let work = crate::PixelRect { x: 0, y: 0, width: demo.screen.width as i32, height: demo.screen.height as i32 };
        Ok((demo.display(), work))
    }

    fn permission_settings_url(&self) -> Option<&'static str> {
        None
    }

    fn recognize_text(&self, width: u32, height: u32, rgba: &[u8], scale: f32) -> anyhow::Result<Vec<TextLine>> {
        crate::recognize_with_model(width, height, rgba, scale)
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
