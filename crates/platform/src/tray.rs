//! System tray (Windows) / menu bar (macOS) icon, shared by both backends via the
//! `tray-icon` crate.

use std::sync::Arc;

use anyhow::Context;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use crate::{Tray, TrayCommand};

pub(crate) struct Style {
    pub icon_png: &'static [u8],
    /// macOS template image: drawn in the menu bar's text color. Ignored elsewhere.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub template: bool,
    /// Windows convention: left click acts, right click opens the menu.
    pub left_click_captures: bool,
    /// The start-at-login item, in the OS's words, and whether it's on.
    pub start_at_login: (&'static str, bool),
}

fn decode_png(data: &[u8]) -> anyhow::Result<Icon> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let mut rgba = vec![0; reader.output_buffer_size().context("icon too large")?];
    let info = reader.next_frame(&mut rgba)?;
    anyhow::ensure!(info.color_type == png::ColorType::Rgba, "tray icon must be RGBA");
    rgba.truncate(info.buffer_size());
    Ok(Icon::from_rgba(rgba, info.width, info.height)?)
}

pub(crate) fn create(style: Style, hotkey: &str, on_command: Box<dyn Fn(TrayCommand) + Send + Sync>) -> anyhow::Result<Tray> {
    let capture = MenuItem::with_id("capture", format!("Take Screenshot ({hotkey})"), true, None);
    let open_editor = MenuItem::with_id("open_editor", "Open Editor", true, None);
    let open_bs = MenuItem::with_id("open_bs", "Open .bs File…", true, None);
    let settings = MenuItem::with_id("settings", "Edit Settings…", true, None);
    let ai = MenuItem::with_id("ai", "AI Settings…", true, None);
    let (login_label, login_on) = style.start_at_login;
    let login = CheckMenuItem::with_id("login", login_label, true, login_on, None);
    let quit = MenuItem::with_id("quit", "Quit bluesnap", true, None);
    let menu = Menu::new();
    menu.append_items(&[
        &capture,
        &open_editor,
        &open_bs,
        &PredefinedMenuItem::separator(),
        &settings,
        &ai,
        &login,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;

    let on_command: Arc<dyn Fn(TrayCommand) + Send + Sync> = Arc::from(on_command);
    MenuEvent::set_event_handler(Some({
        let on_command = on_command.clone();
        move |event: MenuEvent| match event.id.0.as_str() {
            "capture" => on_command(TrayCommand::Capture),
            "open_editor" => on_command(TrayCommand::OpenEditor),
            "open_bs" => on_command(TrayCommand::OpenBsFile),
            "settings" => on_command(TrayCommand::EditSettings),
            "ai" => on_command(TrayCommand::AiSettings),
            "login" => on_command(TrayCommand::ToggleStartAtLogin),
            "quit" => on_command(TrayCommand::Quit),
            _ => {}
        }
    }));
    if style.left_click_captures {
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                on_command(TrayCommand::Capture);
            }
        }));
    }

    let icon = decode_png(style.icon_png)?;
    let builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!("bluesnap ({hotkey})"))
        .with_menu_on_left_click(!style.left_click_captures);
    #[cfg(target_os = "macos")]
    let builder = if style.template { builder.with_icon_templated(icon) } else { builder.with_icon(icon) };
    #[cfg(not(target_os = "macos"))]
    let builder = builder.with_icon(icon);
    let tray = builder.build().context("creating tray icon")?;
    // The menu checks or unchecks the item itself when clicked; the app sets
    // it again once it knows whether the change worked.
    Ok(Tray {
        update_hotkey: Box::new(move |spec| {
            capture.set_text(format!("Take Screenshot ({spec})"));
            if let Err(e) = tray.set_tooltip(Some(format!("bluesnap ({spec})"))) {
                log::warn!("could not update the tray tooltip: {e}");
            }
        }),
        show_start_at_login: Box::new(move |on| login.set_checked(on)),
    })
}
