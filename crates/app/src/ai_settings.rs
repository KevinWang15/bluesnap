//! The "AI Settings" window, opened from the tray menu: the optional AI
//! endpoints that features like the Redact tool's Auto button use.

use std::sync::Arc;

use bluesnap_ai::{AiSettings, Endpoint};
use bluesnap_platform::Platform;
use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyBinding, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task,
    TitlebarOptions, Window, WindowBounds, WindowKind, WindowOptions, actions, div, prelude::FluentBuilder, px, rgb,
    rgba, size,
};

use crate::AppState;
use crate::config::Config;
use crate::text_field::{TextField, truncate_chars};

actions!(ai_settings, [NextField, PreviousField, Save, Close]);

const CONTEXT: &str = "AiSettings";

/// Registers the window's keys; call once at startup.
pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("tab", NextField, c),
        KeyBinding::new("shift-tab", PreviousField, c),
        KeyBinding::new("enter", Save, c),
        KeyBinding::new("escape", Close, c),
    ]);
}

/// Where the AI settings are stored: beside the config file.
pub fn path() -> Option<std::path::PathBuf> {
    Some(Config::path()?.with_file_name("ai.json"))
}

/// Reads the saved settings; problems are logged and mean "not configured".
pub fn load() -> AiSettings {
    let Some(path) = path() else { return AiSettings::default() };
    AiSettings::load(&path).unwrap_or_else(|e| {
        log::error!("{e:#}");
        AiSettings::default()
    })
}

/// Opens the window, or brings the open one forward. `above_overlay` when
/// asked from the capture overlay, which otherwise covers it.
pub fn open(above_overlay: bool, cx: &mut App) {
    let raise = move |window: &mut Window| {
        if above_overlay && let Err(e) = bluesnap_platform::native().raise_above_overlay(window) {
            log::warn!("could not raise the AI settings window: {e:#}");
        }
    };
    if let Some(handle) = cx.global::<AppState>().ai_window
        && handle.update(cx, |_, window, _| {
            window.activate_window();
            raise(window);
        })
        .is_ok()
    {
        cx.activate(true);
        return;
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(600.), px(690.)), cx))),
        titlebar: Some(TitlebarOptions { title: Some("bluesnap: AI Settings".into()), ..Default::default() }),
        focus: true,
        show: true,
        kind: WindowKind::Normal,
        is_resizable: false,
        is_minimizable: false,
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| cx.new(|cx| SettingsView::new(window, cx)));
    match opened {
        Ok(handle) => {
            cx.global_mut::<AppState>().ai_window = Some(handle.into());
            handle.update(cx, |_, window, _| raise(window)).ok();
            cx.activate(true);
        }
        Err(e) => log::error!("could not open the AI settings: {e:#}"),
    }
}

/// The result of a section's Test button.
#[derive(Clone)]
enum Check {
    Idle,
    Running,
    Passed,
    Failed(SharedString),
}

/// One endpoint's fields.
struct Section {
    title: &'static str,
    api: &'static str,
    url: Entity<TextField>,
    token: Entity<TextField>,
    model: Entity<TextField>,
    check: Check,
    test: Option<Task<()>>,
}

impl Section {
    fn new(title: &'static str, api: &'static str, model_hint: &str, e: Option<&Endpoint>, cx: &mut App) -> Self {
        let e = e.cloned().unwrap_or_default();
        Self {
            title,
            api,
            url: cx.new(|cx| TextField::new(&e.url, "https://api.example.com", false, cx)),
            token: cx.new(|cx| TextField::new(&e.token, "Token (sent as a Bearer header)", true, cx)),
            model: cx.new(|cx| TextField::new(&e.model, model_hint, false, cx)),
            check: Check::Idle,
            test: None,
        }
    }

    /// `None` when every field is empty (the section is off).
    fn endpoint(&self, cx: &App) -> Option<Endpoint> {
        let get = |f: &Entity<TextField>| f.read(cx).text().trim().to_string();
        let e = Endpoint { url: get(&self.url), token: get(&self.token), model: get(&self.model) };
        (!e.url.is_empty() || !e.token.is_empty() || !e.model.is_empty()).then_some(e)
    }
}

pub struct SettingsView {
    focus: FocusHandle,
    sections: [Section; 2],
    error: Option<SharedString>,
}

impl SettingsView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let saved = cx.global::<AppState>().ai.clone();
        let sections = [
            Section::new("LLM", "OpenAI Responses API (/v1/responses)", "Model, e.g. gpt-5-mini", saved.llm.as_ref(), cx),
            Section::new("Decision model", "Decisions API (/v1/decisions)", "Model, e.g. decisions/d3-ultra", saved.decisions.as_ref(), cx),
        ];
        window.focus(&sections[0].url.read(cx).focus_handle(cx));
        cx.on_release(|_, cx| cx.global_mut::<AppState>().ai_window = None).detach();
        Self { focus: cx.focus_handle(), sections, error: None }
    }

    fn settings(&self, cx: &App) -> AiSettings {
        AiSettings { llm: self.sections[0].endpoint(cx), decisions: self.sections[1].endpoint(cx) }
    }

    fn save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.settings(cx);
        for e in [&settings.llm, &settings.decisions].into_iter().flatten() {
            if !e.is_complete() {
                self.error = Some("Fill in the URL and model, or clear the section to turn it off.".into());
                return cx.notify();
            }
        }
        let saved = path().ok_or_else(|| anyhow::anyhow!("no config directory")).and_then(|p| settings.save(&p));
        match saved {
            Ok(()) => {
                log::info!("AI settings saved (LLM: {}, decisions: {})", settings.llm().is_some(), settings.decisions().is_some());
                cx.global_mut::<AppState>().ai = Arc::new(settings);
                // Open editors show what's now available.
                cx.refresh_windows();
                close_soon(window, cx);
            }
            Err(e) => {
                log::error!("could not save the AI settings: {e:#}");
                self.error = Some(format!("Could not save: {}", truncate_chars(&format!("{e:#}"), 220)).into());
                cx.notify();
            }
        }
    }

    fn close(&mut self, _: &Close, window: &mut Window, cx: &mut Context<Self>) {
        close_soon(window, cx);
    }

    fn test(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(endpoint) = self.sections[i].endpoint(cx).filter(Endpoint::is_complete) else {
            self.sections[i].check = Check::Failed("Fill in the URL and model first.".into());
            return cx.notify();
        };
        self.sections[i].check = Check::Running;
        let run = cx.background_executor().spawn(async move {
            match i {
                0 => bluesnap_ai::test_llm(&endpoint),
                _ => bluesnap_ai::test_decisions(&endpoint),
            }
        });
        self.sections[i].test = Some(cx.spawn_in(window, async move |this, cx| {
            let result = run.await;
            this.update(cx, |this, cx| {
                this.sections[i].check = match result {
                    Ok(()) => Check::Passed,
                    Err(e) => {
                        log::warn!("AI endpoint test failed: {e:#}");
                        Check::Failed(truncate_chars(&format!("{e:#}"), 220).into())
                    }
                };
                this.sections[i].test = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn section(&self, i: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let s = &self.sections[i];
        let revealed = s.token.read(cx).is_revealed();
        let token = s.token.clone();
        let (check_text, check_color): (SharedString, u32) = match &s.check {
            Check::Idle => ("".into(), 0x666666),
            Check::Running => ("Testing…".into(), 0x666666),
            Check::Passed => ("✓ Works".into(), 0x389E0D),
            Check::Failed(e) => (format!("✕ {e}").into(), 0xCF1322),
        };
        let running = matches!(s.check, Check::Running);
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(14.))
            .rounded(px(8.))
            .bg(rgb(0xFFFFFF))
            .border_1()
            .border_color(rgba(0x0000_0018))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(8.))
                    .child(div().text_size(px(14.)).font_weight(gpui::FontWeight::SEMIBOLD).child(s.title))
                    .child(div().text_size(px(12.)).text_color(rgb(0x888888)).child(s.api)),
            )
            .child(row("URL", s.url.clone().into_any_element()))
            .child(row(
                "Token",
                div()
                    .flex()
                    .flex_1()
                    .gap(px(6.))
                    .child(s.token.clone())
                    .child(
                        small_button(("reveal", i), if revealed { "Hide" } else { "Show" }, false).on_click(
                            cx.listener(move |_, _, _, cx| token.update(cx, |t, cx| t.set_revealed(!revealed, cx))),
                        ),
                    )
                    .into_any_element(),
            ))
            .child(row("Model", s.model.clone().into_any_element()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .pl(px(64.))
                    .min_w(px(0.))
                    .child(
                        small_button(("test", i), if running { "Testing…" } else { "Test" }, running)
                            .when(!running, |b| b.on_click(cx.listener(move |this, _, window, cx| this.test(i, window, cx)))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .text_color(rgb(check_color))
                            .child(check_text),
                    ),
            )
    }
}

/// Closes the window once the current event is handled: removing it while the
/// OS is still delivering a key or IME event to it is a crash risk.
fn close_soon(window: &mut Window, cx: &mut Context<SettingsView>) {
    cx.defer_in(window, |_, window, _| window.remove_window());
}

/// A labeled form row.
fn row(label: &'static str, field: gpui::AnyElement) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(div().w(px(56.)).text_size(px(12.)).text_color(rgb(0x555555)).child(label))
        .child(div().flex().flex_1().child(field))
}

fn small_button(id: impl Into<gpui::ElementId>, label: &'static str, disabled: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .flex_none()
        .px(px(12.))
        .h(px(30.))
        .flex()
        .items_center()
        .rounded(px(6.))
        .border_1()
        .border_color(rgb(0xD0D0D5))
        .bg(rgb(0xFFFFFF))
        .text_size(px(12.))
        .text_color(if disabled { rgb(0xAAAAAA) } else { rgb(0x333333) })
        .when(!disabled, |d| d.cursor_pointer().hover(|s| s.bg(rgb(0xF2F3F5))))
        .child(label)
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let blue = rgb(0x1677FF);
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &NextField, window, _| window.focus_next()))
            .on_action(cx.listener(|_, _: &PreviousField, window, _| window.focus_prev()))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::close))
            .size_full()
            .flex()
            .flex_col()
            .gap(px(14.))
            .p(px(20.))
            .bg(rgb(0xF5F6F8))
            .text_color(rgb(0x1F1F1F))
            .child(div().text_size(px(18.)).font_weight(gpui::FontWeight::BOLD).child("AI Settings"))
            .child(
                div().text_size(px(13.)).text_color(rgb(0x555555)).line_height(px(19.)).child(
                    "Optional AI services for bluesnap's AI features. Nothing is sent anywhere until a \
                     service is set up here. Text is recognized on this computer; when an LLM is set up, \
                     a compact preview is analyzed in parallel to understand what is sensitive on screen \
                     before checking the recognized lines. Used by: Auto in the Redact tool.",
                ),
            )
            .child(self.section(0, cx))
            .child(self.section(1, cx))
            .child(div().text_size(px(12.)).text_color(rgb(0x777777)).child(
                "Set up either or both; leave a section empty to turn it off. Features use whichever \
                 is available, and both together when they can.",
            ))
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .text_color(rgb(0xCF1322))
                            .children(self.error.clone()),
                    )
                    .child(small_button("cancel", "Cancel", false).on_click(cx.listener(|this, _, window, cx| this.close(&Close, window, cx))))
                    .child(
                        div()
                            .id("save")
                            .px(px(18.))
                            .h(px(30.))
                            .flex()
                            .items_center()
                            .rounded(px(6.))
                            .bg(blue)
                            .text_color(rgb(0xFFFFFF))
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0x4096FF)))
                            .child("Save")
                            .on_click(cx.listener(|this, _, window, cx| this.save(&Save, window, cx))),
                    ),
            )
    }
}
