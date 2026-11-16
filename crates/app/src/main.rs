//! bluesnap: press the hotkey, pick a window or drag a region, annotate, copy or save.

// No console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod ai_settings;
mod assets;
mod capture;
mod config;
mod demo;
mod document;
mod editor_view;
mod gpu_image;
mod loupe;
mod output;
mod scrolling;
mod text_field;
mod toast;
mod toolbar;
mod viewport;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bluesnap_core::{FontFamily, FontSet, Fonts};
use bluesnap_platform::{Native, Platform, Tray, TrayCommand};
use futures::StreamExt;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use gpui::{
    AnyWindowHandle, App, AppContext, Application, Bounds, Context, Global, IntoElement, ParentElement, Render,
    Styled, Window, WindowBounds, WindowKind, WindowOptions, div, point, px, rgb, size,
};

use crate::config::Config;

pub struct AppState {
    pub config: Config,
    /// The optional AI endpoints (see [`ai_settings`]).
    pub ai: Arc<bluesnap_ai::AiSettings>,
    /// The open AI settings window, if any.
    pub ai_window: Option<AnyWindowHandle>,
    pub(crate) fonts: Arc<Fonts>,
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
    record_panics();
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
        let [sans, rounded, mono] = FontFamily::ALL.map(|f| FontSet::from_sources(&platform.fonts(f)));
        let fonts = Arc::new(Fonts::new(sans, rounded, mono));
        if fonts.is_empty() {
            log::warn!("no usable font found; text annotations will be invisible");
        }
        text_field::bind_keys(cx);
        ai_settings::bind_keys(cx);
        let ai = Arc::new(ai_settings::load());
        cx.set_global(AppState {
            config,
            ai,
            ai_window: None,
            fonts,
            overlay: None,
            scrolling: None,
            _hotkeys: hotkeys,
            _tray: tray,
        });
        start_demo_capture(cx);
    });
}

/// Demo mode (Linux, see the platform crate): `BLUESNAP_DEMO_CAPTURE_AFTER_MS`
/// starts a capture that long after launch, so recording scripts don't depend
/// on the global hotkey.
fn start_demo_capture(cx: &mut App) {
    if !cfg!(target_os = "linux") {
        return;
    }
    let Some(ms) = std::env::var("BLUESNAP_DEMO_CAPTURE_AFTER_MS").ok().and_then(|v| v.parse().ok()) else { return };
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_millis(ms)).await;
        cx.update(capture::start_capture).ok();
    })
    .detach();
}

/// Appends panics (with a backtrace) to `crash.log` beside the config file, as
/// well as the log: release builds on Windows have no console to show them.
fn record_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let report = format!("{} {info}\n{backtrace}\n", chrono::Local::now().to_rfc3339());
        log::error!("{report}");
        if let Some(path) = Config::path().map(|p| p.with_file_name("crash.log")) {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                file.write_all(report.as_bytes()).ok();
            }
        }
        default(info);
    }));
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
                cx.update(capture::start_capture).ok();
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
                    cx.update(capture::start_capture).ok();
                }
                TrayCommand::EditSettings => {
                    cx.update(|cx| match Config::path() {
                        // `Config::load` created the file on startup.
                        Some(path) => cx.open_with_system(&path),
                        None => log::error!("no config directory on this system"),
                    })
                    .ok();
                }
                TrayCommand::AiSettings => {
                    cx.update(|cx| ai_settings::open(false, cx)).ok();
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
