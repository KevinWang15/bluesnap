//! bluesnap: press the hotkey, pick a window or drag a region, annotate, copy or save.

// No console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod settings;
mod assets;
mod canvas_view;
mod capture;
mod clipboard;
mod config;
mod demo;
mod editor_window;
mod gpu_image;
mod loupe;
mod output;
mod run_mode;
mod scrolling;
mod text_field;
mod toast;
mod toolbar;
mod viewport;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bluesnap_core::{FontFamily, FontSet, Fonts};
use bluesnap_platform::{Native, Platform, Tray, TrayCommand};
use futures::StreamExt;
use global_hotkey::hotkey::{Code, HotKey};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Context, Global, IntoElement, ParentElement, Render,
    Styled, Window, WindowBounds, WindowKind, WindowOptions, div, point, px, rgb, size,
};

use crate::config::Config;
use crate::run_mode::RunMode;

pub struct AppState {
    pub(crate) mode: RunMode,
    pub config: Config,
    /// The optional AI endpoints (see [`settings`]).
    pub ai: Arc<bluesnap_ai::AiSettings>,
    /// The open AI settings window, if any.
    pub ai_window: Option<AnyWindowHandle>,
    pub settings_window: Option<AnyWindowHandle>,
    pub(crate) fonts: Arc<Fonts>,
    /// Invisible 1×1 window keeping GPUI's event loop alive across transient
    /// window transitions and async clipboard/save jobs.
    pub(crate) keepalive: Option<AnyWindowHandle>,
    /// Active background delivery / modal dialog jobs that must finish before
    /// a standalone editor session exits.
    pub(crate) pending_jobs: usize,
    /// The open capture overlay, if any. Cleared when its view is released.
    pub overlay: Option<AnyWindowHandle>,
    /// The open multi-image Composition Editor window, if any.
    pub(crate) editor: Option<gpui::WindowHandle<canvas_view::CanvasView>>,
    /// OS window handle of `editor`, cached so visibility/dialog calls never
    /// need to borrow `Window` re-entrantly.
    pub(crate) editor_ref: Option<bluesnap_platform::WindowRef>,
    /// True while the Editor window is temporarily hidden during an `"Add image"` capture.
    pub(crate) editor_hidden: bool,
    /// The running scrolling capture, if any.
    pub scrolling: Option<gpui::WeakEntity<scrolling::ScrollingCapture>>,
    /// The 3-second launch corner toast, if still visible.
    pub(crate) startup_toast: Option<AnyWindowHandle>,
    /// Global hotkey manager; kept alive for the lifetime of the app and shared
    /// with scrolling capture so it can register a session-scoped `Escape` cancel.
    pub(crate) hotkeys: Option<Rc<GlobalHotKeyManager>>,
    /// Removes the tray / menu bar icon when dropped.
    tray: Option<Tray>,
}

impl Global for AppState {}

pub(crate) fn escape_hotkey() -> HotKey {
    HotKey::new(None, Code::Escape)
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    record_panics();
    let config = Config::load();
    let mode = RunMode::from_env();
    let (url_tx, mut url_rx) = futures::channel::mpsc::unbounded::<Vec<String>>();

    let app = gpui_platform::application().with_assets(assets::Assets);
    app.on_open_urls(move |urls| {
        url_tx.unbounded_send(urls).ok();
    });
    app.run(move |cx: &mut App| {
        let platform = bluesnap_platform::native();
        if let Err(e) = platform.init(mode.app_role()) {
            log::error!("platform setup failed: {e:#}");
        }
        if let Err(e) = platform.register_file_associations(mode.overwrites_file_association()) {
            log::warn!("could not register .bs file association: {e:#}");
        }
        let keepalive = open_keepalive_window(cx);

        // In daemon mode, registers the global capture shortcut (warning on conflict);
        // in standalone `.bs` editor mode, skips global shortcut registration while
        // still wiring the manager for session-scoped `Esc` during scrolling capture.
        let (hotkeys, hotkey_active) = init_hotkeys(mode.capture_hotkey(&config.hotkey), cx);
        if mode.refreshes_login_item()
            && platform.start_at_login()
            && let Err(e) = platform.set_start_at_login(true)
        {
            log::warn!("could not update the login item: {e:#}");
        }
        let tray = if mode.uses_tray() {
            match create_tray(&platform, &config.hotkey, cx) {
                Ok(tray) => Some(tray),
                Err(e) => {
                    log::warn!("no tray icon: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        let tray_ready = tray.is_some();

        // Loading fonts reads a few MB from disk; do it once, up front.
        let [sans, rounded, mono] = FontFamily::ALL.map(|f| FontSet::from_sources(&platform.fonts(f)));
        let fonts = Arc::new(Fonts::new(sans, rounded, mono));
        if fonts.is_empty() {
            log::warn!("no usable font found; text annotations will be invisible");
        }
        gpui::components::init_components(cx);
        settings::bind_keys(cx);
        let ai = Arc::new(settings::load_ai());
        let startup_hotkey = hotkey_active.then(|| config.hotkey.clone());
        cx.set_global(AppState {
            mode: mode.clone(),
            config,
            ai,
            ai_window: None,
            settings_window: None,
            fonts,
            keepalive,
            pending_jobs: 0,
            overlay: None,
            editor: None,
            editor_ref: None,
            editor_hidden: false,
            scrolling: None,
            startup_toast: None,
            hotkeys,
            tray,
        });
        run_mode::install_lifecycle(cx);

        for path in mode.initial_files() {
            if let Err(e) = editor_window::open_file(path, cx) {
                show_error(format!("Could not open {}:\n\n{e:#}", path.display()), cx);
            }
        }
        if mode.exits_when_idle() {
            cx.defer(run_mode::exit_if_idle);
        }
        cx.spawn(async move |cx| {
            while let Some(urls) = url_rx.next().await {
                cx.update(|cx| {
                    for url in urls {
                        if let Some(path) = url_to_path(&url)
                            && let Err(e) = editor_window::open_file(&path, cx)
                        {
                            show_error(format!("Could not open {}:\n\n{e:#}", path.display()), cx);
                        }
                    }
                });
            }
        })
        .detach();

        if tray_ready && mode.shows_startup_toast() {
            toast::show_startup(startup_hotkey.as_deref(), cx);
        }
        if mode.runs_demo_capture() {
            start_demo_capture(cx);
        }
    });
}

fn url_to_path(url: &str) -> Option<PathBuf> {
    if let Some(rest) = url.strip_prefix("file://") {
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        // On Windows, `file:///C:/foo.bs` leaves `/C:/foo.bs`.
        let rest = if cfg!(windows) && rest.starts_with('/') && rest.as_bytes().get(2) == Some(&b':') {
            &rest[1..]
        } else {
            rest
        };
        let mut bytes = Vec::with_capacity(rest.len());
        let raw = rest.as_bytes();
        let mut i = 0;
        while i < raw.len() {
            if raw[i] == b'%'
                && i + 2 < raw.len()
                && let Ok(hex) = std::str::from_utf8(&raw[i + 1..i + 3])
                && let Ok(b) = u8::from_str_radix(hex, 16)
            {
                bytes.push(b);
                i += 3;
                continue;
            }
            bytes.push(raw[i]);
            i += 1;
        }
        String::from_utf8(bytes).ok().map(PathBuf::from)
    } else if !url.contains("://") {
        Some(PathBuf::from(url))
    } else {
        None
    }
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
        cx.update(capture::start_capture);
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
/// the app alive across transient window transitions and async delivery jobs.
fn open_keepalive_window(cx: &mut App) -> Option<AnyWindowHandle> {
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
    match cx.open_window(options, |_, cx| cx.new(|_| Empty)) {
        Ok(handle) => Some(handle.into()),
        Err(e) => {
            log::error!("could not open keep-alive window: {e:#}");
            None
        }
    }
}

fn init_hotkeys(capture_spec: Option<&str>, cx: &mut App) -> (Option<Rc<GlobalHotKeyManager>>, bool) {
    let report = |e: anyhow::Error, cx: &mut App| {
        let path = Config::path().map(|p| p.display().to_string()).unwrap_or_default();
        show_error(format!("{e:#}\n\nChange `hotkey` in {path}, then quit and restart bluesnap."), cx);
    };
    let manager = match GlobalHotKeyManager::new().context("global hotkeys unavailable") {
        Ok(m) => Rc::new(m),
        Err(e) => {
            if capture_spec.is_some() {
                report(e, cx);
            } else {
                log::debug!("session hotkeys unavailable in standalone mode: {e:#}");
            }
            return (None, false);
        }
    };
    let capture_id = capture_spec.and_then(|spec| {
        match spec
            .parse::<HotKey>()
            .with_context(|| format!("invalid hotkey {spec:?}"))
            .and_then(|hotkey| {
                manager
                    .register(hotkey)
                    .with_context(|| format!("could not register {spec:?}; another app may be using it"))?;
                Ok(hotkey.id())
            }) {
            Ok(id) => {
                log::info!("ready: press {spec} to capture");
                Some(id)
            }
            Err(e) => {
                report(e, cx);
                None
            }
        }
    });
    let hotkey_active = capture_id.is_some();

    // The handler runs inside the OS event dispatch; hop onto a gpui task.
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        tx.unbounded_send(event).ok();
    }));
    let escape_id = escape_hotkey().id();
    cx.spawn(async move |cx| {
        while let Some(event) = rx.next().await {
            if event.state != HotKeyState::Pressed {
                continue;
            }
            if Some(event.id) == capture_id {
                cx.update(capture::start_capture);
            } else if event.id == escape_id {
                cx.update(scrolling::cancel_active);
            }
        }
    })
    .detach();
    (Some(manager), hotkey_active)
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
                    cx.update(capture::start_capture);
                }
                TrayCommand::OpenEditor => {
                    cx.update(editor_window::open_or_focus);
                }
                TrayCommand::OpenBsFile => {
                    cx.update(|cx| {
                        let owner = cx.global::<AppState>().editor_ref;
                        editor_window::prompt_open_file(true, owner, cx);
                    });
                }
                TrayCommand::EditSettings => {
                    cx.update(|cx| settings::open(settings::Page::General, false, cx));
                }
                TrayCommand::AiSettings => {
                    cx.update(|cx| settings::open(settings::Page::Ai, false, cx));
                }
                TrayCommand::ToggleStartAtLogin => {
                    cx.update(|cx| {
                        let platform = bluesnap_platform::native();
                        if let Err(e) = platform.set_start_at_login(!platform.start_at_login()) {
                            show_error(format!("{e:#}"), cx);
                        }
                        if let Some(tray) = &cx.global::<AppState>().tray {
                            tray.show_start_at_login(platform.start_at_login());
                        }
                    });
                }
                TrayCommand::Quit => {
                    cx.update(|cx| cx.quit());
                }
            }
        }
    })
    .detach();
    Ok(tray)
}

/// Explains a problem the user has to fix (e.g. a hotkey another app owns).
pub(crate) fn show_error(message: String, cx: &mut App) {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::url_to_path;

    #[test]
    fn url_to_path_decodes_file_urls_and_plain_paths() {
        assert_eq!(url_to_path("file:///tmp/My%20Shot.bs"), Some(PathBuf::from("/tmp/My Shot.bs")));
        assert_eq!(url_to_path("file://localhost/tmp/test.bs"), Some(PathBuf::from("/tmp/test.bs")));
        assert_eq!(url_to_path("/var/tmp/plain.bs"), Some(PathBuf::from("/var/tmp/plain.bs")));
        assert_eq!(url_to_path("https://example.com/a.bs"), None);
    }
}
