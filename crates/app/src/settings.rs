//! Preferences opened from the tray or the capture overlay.
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use bluesnap_ai::{AiSettings, Endpoint};
use bluesnap_platform::Platform;
use global_hotkey::hotkey::HotKey;
use gpui::components::*;
use gpui::prelude::*;
use gpui::{
    AnyWindowHandle, App, Bounds, Context, Entity, FocusHandle, Focusable, KeyBinding,
    PathPromptOptions, SharedString, Task, TitlebarOptions, Window, WindowBounds, WindowOptions,
    actions, div, px, size,
};

use crate::{AppState, config::Config, text_field::truncate_chars};

actions!(settings, [NextField, PreviousField, Save, Close]);

pub fn bind_keys(cx: &mut App) {
    let c = Some("BlueSnapSettings");
    cx.bind_keys([
        KeyBinding::new("tab", NextField, c),
        KeyBinding::new("shift-tab", PreviousField, c),
        KeyBinding::new(
            if cfg!(target_os = "macos") {
                "cmd-s"
            } else {
                "ctrl-s"
            },
            Save,
            c,
        ),
        KeyBinding::new("escape", Close, c),
    ]);
}

fn ai_path() -> Result<PathBuf> {
    Ok(Config::path()
        .context("No config directory available")?
        .with_file_name("ai.json"))
}

pub fn load_ai() -> AiSettings {
    ai_path()
        .and_then(|path| AiSettings::load(&path))
        .unwrap_or_else(|e| {
            log::error!("{e:#}");
            AiSettings::default()
        })
}

#[derive(Clone, Copy)]
pub enum Page {
    General,
    Ai,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::General => "General Settings",
            Self::Ai => "AI Settings",
        }
    }

    fn handle(self, state: &mut AppState) -> &mut Option<AnyWindowHandle> {
        match self {
            Self::General => &mut state.settings_window,
            Self::Ai => &mut state.ai_window,
        }
    }
}

pub fn open(page: Page, above_overlay: bool, cx: &mut App) {
    let raise = move |window: &mut Window| {
        if above_overlay && let Err(e) = bluesnap_platform::native().raise_above_overlay(window) {
            log::warn!("could not raise settings above the capture overlay: {e:#}");
        }
    };
    if let Some(handle) = *page.handle(cx.global_mut::<AppState>())
        && handle
            .update(cx, |_, window, _| {
                window.activate_window();
                raise(window);
            })
            .is_ok()
    {
        cx.activate(true);
        return;
    }
    let height = match page {
        Page::General => 440.,
        Page::Ai => 720.,
    };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(610.), px(height)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some(format!("BlueSnap · {}", page.title()).into()),
            ..Default::default()
        }),
        window_min_size: Some(size(px(400.), px(320.))),
        ..Default::default()
    };
    match cx.open_window(options, |window, cx| {
        cx.new(|cx| Settings::new(page, window, cx))
    }) {
        Ok(handle) => {
            *page.handle(cx.global_mut::<AppState>()) = Some(handle.into());
            handle.update(cx, |_, window, _| raise(window)).ok();
            cx.activate(true);
        }
        Err(e) => log::error!("could not open settings: {e:#}"),
    }
}

type InputField = Entity<InputState>;

fn new_input(
    value: String,
    hint: impl Into<SharedString>,
    masked: bool,
    cx: &mut App,
) -> InputField {
    cx.new(|cx| {
        InputState::new(cx)
            .default_value(value)
            .placeholder(hint)
            .masked(masked)
    })
}

struct Service {
    url: InputField,
    model: InputField,
    token: InputField,
    status: Option<Result<(), SharedString>>,
    test: Option<Task<()>>,
}

impl Service {
    fn new(endpoint: Option<Endpoint>, model_hint: &'static str, cx: &mut App) -> Self {
        let endpoint = endpoint.unwrap_or_default();
        Self {
            url: new_input(endpoint.url, "https://api.example.com", false, cx),
            token: new_input(endpoint.token, "Bearer token", true, cx),
            model: new_input(endpoint.model, model_hint, false, cx),
            status: None,
            test: None,
        }
    }
    fn endpoint(&self, cx: &App) -> Option<Endpoint> {
        let get = |f: &InputField| f.read(cx).value().trim().to_string();
        let ep = Endpoint {
            url: get(&self.url),
            token: get(&self.token),
            model: get(&self.model),
        };
        (!ep.url.is_empty() || !ep.token.is_empty() || !ep.model.is_empty()).then_some(ep)
    }
}

enum Draft {
    General {
        hotkey: InputField,
        folder: InputField,
    },
    Ai([Service; 2]),
}

struct Settings {
    draft: Draft,
    focus: FocusHandle,
    error: Option<SharedString>,
}

const SERVICES: [(&str, &str); 2] = [
    ("Language model", "OpenAI Responses API · /v1/responses"),
    ("Decision model", "Decisions API · /v1/decisions"),
];

impl Settings {
    fn new(page: Page, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = |mode, pref, _: &App| {
            match mode {
                ThemeMode::Light => Theme::light(),
                ThemeMode::Dark => Theme::dark(),
            }
            .with_accent(gpui::rgb(0x1677ff).into())
            .with_preference(pref)
        };
        Theme::set_global(
            theme(window.appearance().into(), ThemePreference::System, cx),
            cx,
        );
        Theme::observe_system_appearance(window, theme).detach();
        let draft = match page {
            Page::General => {
                let config = &cx.global::<AppState>().config;
                let hotkey = config.hotkey.clone();
                let folder = config
                    .save_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                Draft::General {
                    hotkey: new_input(hotkey, Config::default().hotkey, false, cx),
                    folder: new_input(folder, "Pictures folder", false, cx),
                }
            }
            Page::Ai => {
                let ai = cx.global::<AppState>().ai.clone();
                Draft::Ai([
                    Service::new(ai.llm.clone(), "e.g. gpt-5-mini", cx),
                    Service::new(ai.decisions.clone(), "e.g. decisions/d3-ultra", cx),
                ])
            }
        };
        let fields = match &draft {
            Draft::General { hotkey, folder } => vec![(None, hotkey), (None, folder)],
            Draft::Ai(services) => services
                .iter()
                .enumerate()
                .flat_map(|(i, s)| [&s.url, &s.model, &s.token].map(|f| (Some(i), f)))
                .collect(),
        };
        window.focus(&fields[0].1.read(cx).focus_handle(cx), cx);
        for (index, field) in fields {
            cx.subscribe_in(
                field,
                window,
                move |this, _, event, window, cx| match event {
                    InputEvent::Submit(_) => this.save(window, cx),
                    InputEvent::Change(_) => {
                        this.error = None;
                        if let (Draft::Ai(services), Some(i)) = (&mut this.draft, index) {
                            services[i].test = None;
                            services[i].status = None;
                        }
                        cx.notify();
                    }
                    _ => {}
                },
            )
            .detach();
        }
        cx.on_release(move |_, cx| *page.handle(cx.global_mut::<AppState>()) = None)
            .detach();
        Self {
            draft,
            focus: cx.focus_handle(),
            error: None,
        }
    }

    fn persist(&self, cx: &mut App) -> Result<()> {
        match &self.draft {
            Draft::General { hotkey, folder } => {
                let hotkey = hotkey.read(cx).value().trim();
                ensure!(
                    hotkey.parse::<HotKey>().is_ok(),
                    "Enter a valid shortcut, such as {}.",
                    Config::default().hotkey
                );
                let dir = folder.read(cx).value().trim();
                let config = Config {
                    hotkey: hotkey.to_owned(),
                    save_dir: (!dir.is_empty()).then(|| dir.into()),
                };
                config.save()?;
                cx.global_mut::<AppState>().config = config;
            }
            Draft::Ai(services) => {
                let [llm, decisions] = services.each_ref().map(|s| s.endpoint(cx));
                for ((name, _), endpoint) in SERVICES.iter().zip([&llm, &decisions]) {
                    ensure!(
                        endpoint.as_ref().is_none_or(Endpoint::is_complete),
                        "{name} needs a URL and model. Clear all three fields to turn it off."
                    );
                }
                let ai = AiSettings { llm, decisions };
                ai.save(&ai_path()?)?;
                cx.global_mut::<AppState>().ai = Arc::new(ai);
                cx.refresh_windows();
            }
        }
        Ok(())
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.persist(cx) {
            Ok(()) => Self::close(window, cx),
            Err(e) => {
                self.error = Some(truncate_chars(&format!("{e:#}"), 220).into());
                cx.notify();
            }
        }
    }
    fn close(window: &mut Window, cx: &mut Context<Self>) {
        cx.defer_in(window, |_, window, _| window.remove_window());
    }

    fn browse(&mut self, cx: &mut Context<Self>) {
        let Draft::General { folder, .. } = &self.draft else {
            return;
        };
        let folder = folder.clone();
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose save folder".into()),
        });
        cx.spawn(async move |this, cx| match picker.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.first() {
                    folder.update(cx, |f, cx| f.set_value(path.display().to_string(), cx));
                }
            }
            Ok(Err(e)) => {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("{e:#}").into());
                    cx.notify();
                })
                .ok();
            }
            _ => {}
        })
        .detach();
    }

    fn test(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Draft::Ai(services) = &mut self.draft else {
            return;
        };
        let s = &mut services[i];
        let Some(ep) = s.endpoint(cx).filter(Endpoint::is_complete) else {
            s.status = Some(Err("Enter a URL and model to test this service.".into()));
            return cx.notify();
        };
        s.status = None;
        let test = cx.background_spawn(async move {
            if i == 0 {
                bluesnap_ai::test_llm(&ep)
            } else {
                bluesnap_ai::test_decisions(&ep)
            }
        });
        s.test = Some(cx.spawn_in(window, async move |this, cx| {
            let result = test
                .await
                .map_err(|e| truncate_chars(&format!("{e:#}"), 220).into());
            this.update(cx, |this, cx| {
                if let Draft::Ai(services) = &mut this.draft {
                    services[i].status = Some(result);
                    services[i].test = None;
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn service_card(
        &self,
        i: usize,
        s: &Service,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        card()
            .child(
                card_header()
                    .pt(px(14.))
                    .pb(px(4.))
                    .child(card_title(SERVICES[i].0))
                    .child(card_description(SERVICES[i].1)),
            )
            .child(
                card_content()
                    .pt(px(8.))
                    .pb(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(field().label("Server URL").child(input(&s.url)))
                    .child(
                        div()
                            .flex()
                            .gap(px(12.))
                            .child(
                                field()
                                    .flex_1()
                                    .min_w_0()
                                    .label("Model")
                                    .child(input(&s.model)),
                            )
                            .child(
                                field()
                                    .flex_1()
                                    .min_w_0()
                                    .label("Token")
                                    .hint("Optional")
                                    .child(input(&s.token)),
                            ),
                    ),
            )
            .child(
                card_footer()
                    .justify_start()
                    .py(px(8.))
                    .gap(px(12.))
                    .child(
                        button(("test", i))
                            .outline()
                            .label("Test connection")
                            .loading(s.test.is_some())
                            .on_click(cx.listener(move |this, _, w, cx| this.test(i, w, cx))),
                    )
                    .children(s.status.as_ref().map(|r| {
                        let (msg, color) = match r {
                            Ok(()) => ("Connected".into(), theme.success),
                            Err(e) => (e.clone(), theme.destructive),
                        };
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(color)
                            .child(msg)
                    })),
            )
    }
}

impl Render for Settings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let body = div()
            .id("settings-scroll")
            .overflow_y_scroll()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(22.));
        let body = match &self.draft {
            Draft::General { hotkey, folder } => {
                let shortcut = field().label("Global shortcut")
                    .description("Restart BlueSnap to activate a new shortcut.")
                    .child(input(hotkey));
                let browse = button("browse").outline().label("Browse…")
                    .on_click(cx.listener(|this, _, _, cx| this.browse(cx)));
                let directory = field().label("Default save folder")
                    .description("Leave empty to use your Pictures folder.")
                    .child(div().flex().gap(px(8.))
                        .child(div().flex_1().min_w_0().child(input(folder))).child(browse));
                body.child(section("Capture").child(card().p(px(16.)).child(shortcut)))
                    .child(section("Saving").child(card().p(px(16.)).child(directory)))
            }
            Draft::Ai(services) => body
                .child(ui_text("Auto redaction recognizes text locally and sends it to the services you configure. The language model also receives a screen preview. Leave a service empty to turn it off.").muted())
                .child(self.service_card(0, &services[0], cx))
                .child(self.service_card(1, &services[1], cx)),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .track_focus(&self.focus)
            .key_context("BlueSnapSettings")
            .on_action(cx.listener(|_, _: &NextField, w, cx| w.focus_next(cx)))
            .on_action(cx.listener(|_, _: &PreviousField, w, cx| w.focus_prev(cx)))
            .on_action(cx.listener(|this, _: &Save, w, cx| this.save(w, cx)))
            .on_action(cx.listener(|_, _: &Close, w, cx| Self::close(w, cx)))
            .child(body)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(20.))
                    .py(px(12.))
                    .bg(theme.surface)
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(theme.destructive)
                            .children(self.error.clone()),
                    )
                    .child(
                        button("cancel")
                            .outline()
                            .label("Cancel")
                            .on_click(cx.listener(|_, _, w, cx| Self::close(w, cx))),
                    )
                    .child(
                        button("save")
                            .primary()
                            .label("Save")
                            .on_click(cx.listener(|this, _, w, cx| this.save(w, cx))),
                    ),
            )
    }
}
