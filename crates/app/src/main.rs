//! bluesnap: press the hotkey, pick a window or drag a region, annotate, copy or save.

// No console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod assets;
mod config;
mod output;
mod overlay;
mod scrolling;
mod toolbar;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bluesnap_core::{Document, Editor, FontSet, Rect};
use bluesnap_platform::{Native, Platform, Tray, TrayCommand};
use futures::StreamExt;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use gpui::{
    AnyWindowHandle, App, AppContext, Application, Bounds, Context, Global, IntoElement, ParentElement, Render,
    Styled, Window, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, px, rgb, size,
};

use crate::config::Config;
use crate::overlay::Overlay;

pub struct AppState {
    pub config: Config,
    fonts: Arc<FontSet>,
    /// The open capture overlay, if any. Cleared when its view is released.
    pub overlay: Option<AnyWindowHandle>,
    /// The running scrolling capture, if any.
    pub scrolling: Option<gpui::WeakEntity<scrolling::ScrollingCapture>>,
    /// Unregisters the hotkey when dropped, so it must live as long as the app.
    _hotkeys: Option<GlobalHotKeyManager>,
    /// Removes the tray / menu bar icon when dropped.
    _tray: Option<Tray>,
}

impl Global for AppState {}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let config = Config::load();

    Application::new().with_assets(assets::Assets).run(move |cx: &mut App| {
        let platform = bluesnap_platform::native();
        if let Err(e) = platform.init() {
            log::error!("platform setup failed: {e:#}");
        }
        open_keepalive_window(cx);

        // Without the hotkey the tray menu still works, so this isn't fatal.
        let hotkeys = match register_hotkey(&config.hotkey, cx) {
            Ok(manager) => {
                log::info!("ready: press {} to capture", config.hotkey);
                Some(manager)
            }
            Err(e) => {
                let path = Config::path().map(|p| p.display().to_string()).unwrap_or_default();
                show_error(format!("{e:#}\n\nChange `hotkey` in {path}, then quit and restart bluesnap."), cx);
                None
            }
        };
        let tray = match create_tray(&platform, &config.hotkey, cx) {
            Ok(tray) => Some(tray),
            Err(e) => {
                log::warn!("no tray icon: {e:#}");
                None
            }
        };

        // Loading fonts reads a few MB from disk; do it once, up front.
        let fonts = Arc::new(FontSet::load(&platform.font_paths()));
        if fonts.is_empty() {
            log::warn!("no usable font found; text annotations will be invisible");
        }
        cx.set_global(AppState { config, fonts, overlay: None, scrolling: None, _hotkeys: hotkeys, _tray: tray });
    });
}

/// gpui quits on Windows when its last window closes. An invisible window keeps
/// the app alive between captures.
fn open_keepalive_window(cx: &mut App) {
    struct Empty;
    impl Render for Empty {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(0.), px(0.)), size(px(1.), px(1.))))),
        titlebar: None,
        focus: false,
        show: false,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        ..Default::default()
    };
    if let Err(e) = cx.open_window(options, |_, cx| cx.new(|_| Empty)) {
        log::error!("could not open keep-alive window: {e:#}");
    }
}

fn register_hotkey(spec: &str, cx: &mut App) -> anyhow::Result<GlobalHotKeyManager> {
    let hotkey: HotKey = spec.parse().with_context(|| format!("invalid hotkey {spec:?}"))?;
    let manager = GlobalHotKeyManager::new().context("global hotkeys unavailable")?;
    manager
        .register(hotkey)
        .with_context(|| format!("could not register {spec:?}; another app may be using it"))?;

    // The handler runs inside the OS event dispatch; hop onto a gpui task.
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        tx.unbounded_send(event).ok();
    }));
    let id = hotkey.id();
    cx.spawn(async move |cx| {
        while let Some(event) = rx.next().await {
            if event.id == id && event.state == HotKeyState::Pressed {
                cx.update(start_capture).ok();
            }
        }
    })
    .detach();
    Ok(manager)
}

fn create_tray(platform: &Native, hotkey: &str, cx: &mut App) -> anyhow::Result<Tray> {
    // Like the hotkey handler, menu events arrive inside OS event dispatch.
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let tray = platform.create_tray(hotkey, Box::new(move |command| {
        tx.unbounded_send(command).ok();
    }))?;
    cx.spawn(async move |cx| {
        while let Some(command) = rx.next().await {
            match command {
                TrayCommand::Capture => {
                    // Let the menu finish closing so it isn't in the screenshot.
                    cx.background_executor().timer(Duration::from_millis(200)).await;
                    cx.update(start_capture).ok();
                }
                TrayCommand::EditSettings => {
                    cx.update(|cx| match Config::path() {
                        // `Config::load` created the file on startup.
                        Some(path) => cx.open_with_system(&path),
                        None => log::error!("no config directory on this system"),
                    })
                    .ok();
                }
                TrayCommand::Quit => {
                    cx.update(|cx| cx.quit()).ok();
                }
            }
        }
    })
    .detach();
    Ok(tray)
}

fn start_capture(cx: &mut App) {
    // During a scrolling capture the hotkey means "done".
    if let Some(session) = cx.global_mut::<AppState>().scrolling.take() {
        session.update(cx, |s, cx| s.finish(scrolling::Finish::Copy, cx)).ok();
        return;
    }
    if cx.global::<AppState>().overlay.is_some() {
        log::debug!("hotkey ignored: a capture is already open");
        return;
    }
    if let Err(e) = open_overlay(cx) {
        log::error!("capture failed: {e:#}");
    }
}

fn open_overlay(cx: &mut App) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut stages = Vec::new();
    let platform = bluesnap_platform::native();
    let capture = platform.capture_display_under_cursor()?;
    stages.push(("capture", started.elapsed()));

    let display = cx
        .displays()
        .into_iter()
        .find(|d| u32::from(d.id()) == capture.display.id)
        .context("captured display not known to gpui")?;

    let (width, height) = (capture.image.width, capture.image.height);
    let mut bgra = capture.image.bgrx;
    anyhow::ensure!(bgra.len() == width as usize * height as usize * 4, "bad capture buffer");
    let rgba = bluesnap_core::pixels::split_bgrx(&mut bgra);
    let background = Arc::new(overlay::background_image(width, height, bgra).context("bad capture buffer")?);
    stages.push(("gpu image", started.elapsed()));
    let scale = capture.display.scale_factor;
    let mosaic_block = (8.0 * scale).round() as u32;
    let windows = capture
        .windows
        .iter()
        .map(|w| Rect::new(w.x as f32, w.y as f32, w.width as f32, w.height as f32))
        .collect();
    let doc = Document::from_rgba(width, height, rgba, mosaic_block).context("bad capture buffer")?;
    let fonts = cx.global::<AppState>().fonts.clone();
    let editor = Editor::new(doc, fonts, windows, scale);
    let permission_url = capture.permission_missing.then(|| platform.permission_settings_url()).flatten();
    stages.push(("editor", started.elapsed()));

    let options = WindowOptions {
        window_bounds: Some(if Native::OVERLAY_FULLSCREEN {
            WindowBounds::Fullscreen(display.bounds())
        } else {
            WindowBounds::Windowed(display.bounds())
        }),
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        display_id: Some(display.id()),
        window_background: WindowBackgroundAppearance::Opaque,
        ..Default::default()
    };
    let handle = cx.open_window(options, |_, cx| cx.new(|cx| Overlay::new(editor, background, permission_url, capture.display.clone(), cx)))?;
    stages.push(("open window", started.elapsed()));
    handle.update(cx, |view, window, _| {
        if let Err(e) = platform.configure_overlay(window, &capture.display) {
            log::warn!("overlay setup: {e:#}");
        }
        window.focus(view.focus_handle());
        window.activate_window();
        stages.push(("configure", started.elapsed()));
        let stages = std::mem::take(&mut stages);
        // Frame callbacks run before drawing, so the nested one runs once the
        // first frame (including the screenshot texture upload) is presented.
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |_, _| {
                let timeline: Vec<String> = stages.iter().map(|(name, t)| format!("{name} {t:.1?}")).collect();
                log::debug!("overlay timeline: {}, first frame presented {:.1?}", timeline.join(", "), started.elapsed());
            });
        });
    })?;
    cx.global_mut::<AppState>().overlay = Some(handle.into());
    Ok(())
}

/// Explains a problem the user has to fix (e.g. a hotkey another app owns).
fn show_error(message: String, cx: &mut App) {
    log::error!("{message}");
    struct ErrorView(String);
    impl Render for ErrorView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .p(px(20.))
                .bg(rgb(0xFFFFFF))
                .text_color(rgb(0x222222))
                .text_size(px(14.))
                .child(self.0.clone())
        }
    }
    let bounds = Bounds::centered(None, size(px(520.), px(200.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions { title: Some("bluesnap".into()), ..Default::default() }),
        ..Default::default()
    };
    if let Err(e) = cx.open_window(options, |_, cx| cx.new(|_| ErrorView(message))) {
        log::error!("could not show error window: {e:#}");
    }
}
