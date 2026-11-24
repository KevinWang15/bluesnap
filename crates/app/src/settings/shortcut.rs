//! Global capture shortcut recording via GPUI's [`ShortcutRecorderState`] and
//! [`GlobalHotKey`], bridged to `global-hotkey` across Windows, macOS, and Linux.

use std::rc::Rc;

use anyhow::{Context as _, Result};
use global_hotkey::{GlobalHotKeyManager, hotkey::HotKey};
use gpui::components::{ShortcutRecorderEvent, ShortcutRecorderState};
use gpui::{App, AppContext, Context, Entity, GlobalHotKey, SharedString};

use super::{Draft, Settings};
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

    fn apply(&self, hotkey: &GlobalHotKey, state: &mut AppState) -> Result<()> {
        let spec = hotkey.to_spec();
        let key: HotKey = spec.parse().context("Unsupported shortcut")?;
        if let Err(e) = self.manager.register(key) {
            log::warn!("could not register {spec}: {e}");
            anyhow::bail!(
                "Could not register {}. It may already be in use.",
                hotkey.display_platform()
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

pub(super) fn new_recorder(hotkey: &str, cx: &mut App) -> Entity<ShortcutRecorderState> {
    let initial = GlobalHotKey::parse(hotkey).ok();
    let fallback_placeholder = (initial.is_none() && !hotkey.trim().is_empty())
        .then(|| SharedString::from(hotkey.to_string()));
    cx.new(|cx| {
        let mut state = ShortcutRecorderState::new(initial, cx)
            .clearable(false)
            .recording_prompt("Press the new shortcut…");
        if let Some(placeholder) = fallback_placeholder {
            state = state.placeholder(placeholder);
        }
        state
    })
}

impl Settings {
    pub(super) fn cancel_recording(&mut self, cx: &mut Context<Self>) {
        let recorder = match &self.draft {
            Draft::General { recorder, .. } => Some(recorder.clone()),
            Draft::Ai(_) => None,
        };
        if let Some(ref recorder) = recorder {
            recorder.update(cx, |r, cx| r.cancel_recording(cx));
        }
        self.restore_recording(recorder.as_ref(), cx);
    }

    fn restore_recording(
        &mut self,
        recorder: Option<&Entity<ShortcutRecorderState>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(recording) = self.recording.take() {
            if let Err(e) = recording.restore(cx.global_mut::<AppState>())
                && let Some(recorder) = recorder
            {
                let msg: SharedString = format!("{e:#}").into();
                recorder.update(cx, |r, cx| r.set_error(Some(msg), cx));
            }
            cx.notify();
        }
    }

    pub(super) fn handle_recorder_event(
        &mut self,
        recorder: Entity<ShortcutRecorderState>,
        event: &ShortcutRecorderEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ShortcutRecorderEvent::StartRecording => {
                if self.recording.is_some() {
                    return;
                }
                match Recording::start(cx.global_mut::<AppState>()) {
                    Ok(recording) => {
                        self.recording = Some(recording);
                        cx.notify();
                    }
                    Err(e) => {
                        let msg: SharedString = format!("{e:#}").into();
                        recorder.update(cx, |r, cx| {
                            r.cancel_recording(cx);
                            r.set_error(Some(msg), cx);
                        });
                    }
                }
            }
            ShortcutRecorderEvent::CancelRecording => {
                self.restore_recording(Some(&recorder), cx);
            }
            ShortcutRecorderEvent::Change(Some(next)) => {
                let Some(recording) = self.recording.take() else {
                    return;
                };
                let state = cx.global_mut::<AppState>();
                if let Err(e) = recording.apply(next, state) {
                    let message: SharedString = match recording.restore(state) {
                        Ok(()) => format!("{e:#}"),
                        Err(restore) => format!("{e:#}. {restore:#}"),
                    }
                    .into();
                    let previous = GlobalHotKey::parse(&state.config.hotkey).ok();
                    recorder.update(cx, |r, cx| {
                        r.set_shortcut(previous, cx);
                        r.set_error(Some(message), cx);
                    });
                }
                cx.notify();
            }
            ShortcutRecorderEvent::Change(None) => {
                self.restore_recording(Some(&recorder), cx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use global_hotkey::hotkey::HotKey;
    use gpui::{GlobalHotKey, Keystroke};

    fn spec(s: &'static str) -> Option<String> {
        GlobalHotKey::from_keystroke(&Keystroke::parse(s).unwrap()).map(|hk| hk.to_spec())
    }

    #[test]
    fn key_presses_to_specs_compatible_with_global_hotkey() {
        for (input, expected) in [
            ("alt-space", "Alt+Space"),
            ("ctrl-alt-c", "Ctrl+Alt+C"),
            ("ctrl-shift-1", "Ctrl+Shift+1"),
            ("super-alt-up", "Alt+Super+ArrowUp"),
            ("ctrl-`", "Ctrl+Backquote"),
            ("f5", "F5"),
            ("ctrl-f12", "Ctrl+F12"),
        ] {
            let actual = spec(input).expect("valid chord");
            assert_eq!(actual, expected);
            assert!(
                actual.parse::<HotKey>().is_ok(),
                "global-hotkey should parse {actual}"
            );
        }
        // Plain keys would fire while typing.
        assert_eq!(spec("a"), None);
        assert_eq!(spec("f"), None);
        assert_eq!(spec("space"), None);
    }

    #[test]
    fn default_hotkeys_parse_in_gpui() {
        for default in ["Ctrl+Alt+A", "Ctrl+Cmd+A"] {
            let parsed = GlobalHotKey::parse(default).expect("default hotkey parses");
            assert!(parsed.to_spec().parse::<HotKey>().is_ok());
        }
    }
}
