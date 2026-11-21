//! Shortcuts as `global-hotkey` specs (`Ctrl+Alt+Space`): from a key press in
//! the recorder, and for display.

use std::rc::Rc;

use anyhow::{Context as _, Result};
use global_hotkey::{GlobalHotKeyManager, hotkey::HotKey};
use gpui::components::ActiveTheme;
use gpui::{Context, IntoElement, KeyDownEvent, Keystroke, Window, div, prelude::*, px};

use super::Settings;
use crate::{AppState, config::Config};

/// The old OS registration is released while recording so the same combination can
/// be recorded again without starting a capture.
pub(super) struct Recording {
    manager: Rc<GlobalHotKeyManager>,
    previous: Option<HotKey>,
}

impl Recording {
    fn start(state: &mut AppState) -> Result<Self> {
        let manager = state
            .hotkeys
            .clone()
            .context("Global shortcuts are unavailable.")?;
        let previous = state.capture_hotkey;
        if let Some(old) = previous {
            manager
                .unregister(old)
                .context("Could not pause the capture shortcut")?;
        }
        state.capture_hotkey = None;
        Ok(Self { manager, previous })
    }

    pub(super) fn restore(self, state: &mut AppState) -> Result<()> {
        if let Some(old) = self.previous {
            self.manager
                .register(old)
                .with_context(|| format!("Could not restore {}", state.config.hotkey))?;
            state.capture_hotkey = Some(old);
        }
        Ok(())
    }

    fn apply(&self, spec: String, state: &mut AppState) -> Result<()> {
        let key: HotKey = spec.parse().context("Unsupported shortcut")?;
        if let Err(e) = self.manager.register(key) {
            log::warn!("could not register {spec}: {e}");
            anyhow::bail!(
                "Could not register {}. It may already be in use.",
                display_shortcut(&spec)
            );
        }
        let config = Config {
            hotkey: spec,
            save_dir: state.config.save_dir.clone(),
        };
        if let Err(e) = config.save() {
            self.manager.unregister(key).with_context(|| {
                format!("Could not release the new shortcut after saving failed: {e:#}")
            })?;
            return Err(e.context("Could not save the shortcut"));
        }
        state.capture_hotkey = Some(key);
        if let Some(tray) = &state.tray {
            tray.show_hotkey(&config.hotkey);
        }
        state.config = config;
        Ok(())
    }
}

impl Settings {
    pub(super) fn cancel_recording(&mut self, cx: &mut Context<Self>) {
        if let Some(recording) = self.recording.take() {
            if let Err(e) = recording.restore(cx.global_mut::<AppState>()) {
                self.shortcut_error = Some(format!("{e:#}").into());
            }
            cx.notify();
        }
    }

    fn toggle_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.recording.is_some() {
            return self.cancel_recording(cx);
        }
        self.shortcut_error = None;
        match Recording::start(cx.global_mut::<AppState>()) {
            Ok(recording) => {
                self.recording = Some(recording);
                window.focus(&self.recorder_focus, cx);
            }
            Err(e) => self.shortcut_error = Some(format!("{e:#}").into()),
        }
        cx.notify();
    }

    fn recorder_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        if self.recording.is_none() {
            if !event.is_held
                && !key.modifiers.modified()
                && matches!(key.key.as_str(), "space" | "enter")
            {
                cx.stop_propagation();
                self.toggle_recording(window, cx);
            }
            return;
        }
        cx.stop_propagation();
        if !key.modifiers.modified() && key.key == "escape" {
            return self.cancel_recording(cx);
        }
        let Some(spec) = spec_from_keystroke(key) else {
            return;
        };
        let recording = self.recording.take().expect("active recorder");
        let state = cx.global_mut::<AppState>();
        if let Err(e) = recording.apply(spec, state) {
            let message = match recording.restore(state) {
                Ok(()) => format!("{e:#}"),
                Err(restore) => format!("{e:#}. {restore:#}"),
            };
            self.shortcut_error = Some(message.into());
        }
        cx.notify();
    }

    pub(super) fn shortcut_recorder(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let recording = self.recording.is_some();
        let label = if recording {
            "Press the new shortcut…".into()
        } else {
            display_shortcut(&cx.global::<AppState>().config.hotkey)
        };
        div()
            .id("capture-shortcut")
            .track_focus(&self.recorder_focus)
            .on_key_down(cx.listener(Self::recorder_key))
            .on_click(cx.listener(|this, _, window, cx| this.toggle_recording(window, cx)))
            .h(px(32.))
            .min_w(px(170.))
            .px(px(12.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme.radius_md)
            .border_1()
            .text_size(px(13.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .cursor_pointer()
            .border_color(theme.input_border)
            .bg(theme.input_bg)
            .hover(|s| s.border_color(theme.input_border_hover))
            .focus_visible(|s| s.border_color(theme.primary))
            .when(recording, |d| {
                d.border_color(theme.primary)
                    .bg(theme.primary_soft)
                    .text_color(theme.primary_soft_foreground)
            })
            .child(label)
    }
}

/// A key press as a `global-hotkey` spec: `Ctrl+Alt+Space`, `Alt+Super+C`.
/// `None` for a modifier alone, a key `global-hotkey` doesn't name here, or
/// a plain key (it would fire while typing anywhere; F-keys may stand alone).
fn spec_from_keystroke(k: &Keystroke) -> Option<String> {
    let key = k.key.as_str();
    let named = match key {
        "space" => "Space",
        "enter" => "Enter",
        "tab" => "Tab",
        "`" => "Backquote",
        "-" => "Minus",
        "=" => "Equal",
        "," => "Comma",
        "." => "Period",
        "/" => "Slash",
        ";" => "Semicolon",
        "'" => "Quote",
        "[" => "BracketLeft",
        "]" => "BracketRight",
        "\\" => "Backslash",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        _ => "",
    };
    let function_key = key
        .strip_prefix('f')
        .filter(|n| !n.is_empty() && n.len() <= 2 && n.bytes().all(|b| b.is_ascii_digit()));
    let key = if !named.is_empty() {
        named.to_string()
    } else if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() {
        key.to_ascii_uppercase()
    } else {
        format!("F{}", function_key?)
    };
    let m = &k.modifiers;
    let mods: Vec<&str> = [
        (m.control, "Ctrl"),
        (m.alt, "Alt"),
        (m.shift, "Shift"),
        (m.platform, "Super"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    if mods.is_empty() && !key.starts_with('F') {
        return None;
    }
    if mods.is_empty() && key.len() == 1 {
        // A lone "F" key press, not a function key.
        return None;
    }
    Some(
        mods.into_iter()
            .chain([key.as_str()])
            .collect::<Vec<_>>()
            .join("+"),
    )
}

/// How a spec reads: `Ctrl + Alt + Space`, or `⌥⌘C` on macOS.
fn display_shortcut(spec: &str) -> String {
    let parts = spec.split('+');
    if cfg!(target_os = "macos") {
        parts
            .map(|p| match p {
                "Ctrl" => "⌃",
                "Alt" => "⌥",
                "Shift" => "⇧",
                "Super" | "Cmd" => "⌘",
                other => other,
            })
            .collect()
    } else {
        parts
            .map(|p| if p == "Super" { "Win" } else { p })
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(s: &'static str) -> Option<String> {
        spec_from_keystroke(&Keystroke::parse(s).unwrap())
    }

    #[test]
    fn key_presses_to_specs() {
        assert_eq!(spec("alt-space").as_deref(), Some("Alt+Space"));
        assert_eq!(spec("ctrl-alt-c").as_deref(), Some("Ctrl+Alt+C"));
        assert_eq!(spec("ctrl-shift-1").as_deref(), Some("Ctrl+Shift+1"));
        assert_eq!(spec("super-alt-up").as_deref(), Some("Alt+Super+ArrowUp"));
        assert_eq!(spec("ctrl-`").as_deref(), Some("Ctrl+Backquote"));
        assert_eq!(spec("f5").as_deref(), Some("F5"));
        assert_eq!(spec("ctrl-f12").as_deref(), Some("Ctrl+F12"));
        // Plain keys would fire while typing.
        assert_eq!(spec("a"), None);
        assert_eq!(spec("f"), None);
        assert_eq!(spec("space"), None);
    }

    #[test]
    fn shown() {
        if !cfg!(target_os = "macos") {
            assert_eq!(display_shortcut("Ctrl+Alt+Space"), "Ctrl + Alt + Space");
            assert_eq!(display_shortcut("Alt+Super+C"), "Alt + Win + C");
        }
    }
}
