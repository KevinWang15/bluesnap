//! A small notice over the editor: what's happening (with a spinner), what
//! happened, or what went wrong, optionally with one action button.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, App, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, Styled, Transformation, Window, div, percentage,
    prelude::FluentBuilder, px, rgb, rgba, svg, white,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Work in progress: a spinner, stays until replaced.
    Busy,
    Done,
    Info,
    Error,
}

/// What a toast's button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    OpenAiSettings,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Toast {
    pub kind: Kind,
    pub title: SharedString,
    pub detail: Option<SharedString>,
    pub action: Option<(SharedString, Action)>,
}

impl Toast {
    pub fn new(kind: Kind, title: impl Into<SharedString>) -> Self {
        Self { kind, title: title.into(), detail: None, action: None }
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn action(mut self, label: impl Into<SharedString>, action: Action) -> Self {
        self.action = Some((label.into(), action));
        self
    }

    /// How long it stays up; busy toasts stay until replaced, problems longer
    /// so there's time to read them.
    pub fn lifetime(&self) -> Option<Duration> {
        match self.kind {
            Kind::Busy => None,
            Kind::Done => Some(Duration::from_millis(2500)),
            Kind::Info => Some(Duration::from_secs(6)),
            Kind::Error => Some(Duration::from_secs(12)),
        }
    }

    /// Whether a click elsewhere dismisses it (busy toasts track work).
    pub fn dismissable(&self) -> bool {
        self.kind != Kind::Busy
    }
}

/// The toast, centered horizontally at `(center_x, top)`, for a window
/// `window_width` wide. `on_action` runs the button; `on_close` dismisses.
pub fn render(
    toast: &Toast,
    center_x: f32,
    top: f32,
    window_width: f32,
    on_action: impl Fn(Action, &mut Window, &mut App) + 'static,
    on_close: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    const WIDTH: f32 = 420.0;
    let width = (window_width - 16.0).clamp(0.0, WIDTH);
    let left = (center_x - width / 2.0).clamp(8.0, (window_width - width - 8.0).max(8.0));
    let icon = match toast.kind {
        Kind::Busy => svg()
            .path("icons/spinner.svg")
            .size(px(16.))
            .text_color(rgb(0x69B1FF))
            .with_animation("spin", Animation::new(Duration::from_millis(900)).repeat(), |s, t| {
                s.with_transformation(Transformation::rotate(percentage(t)))
            })
            .into_any_element(),
        kind => {
            let (path, color) = match kind {
                Kind::Done => ("icons/done.svg", 0x73D13D),
                Kind::Error => ("icons/warning.svg", 0xFF7875),
                _ => ("icons/info.svg", 0x69B1FF),
            };
            svg().path(path).size(px(16.)).text_color(rgb(color)).into_any_element()
        }
    };
    let action = toast.action.clone();
    // A full-width row, so the toast is centered over (and clamped to) the window.
    div()
        .absolute()
        .top(px(top))
        .left(px(left))
        .w(px(width))
        .flex()
        .justify_center()
        .child(
            div()
                .id("toast")
                .occlude()
                .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| cx.stop_propagation())
                .max_w(px(width))
                .flex()
                .items_start()
                .gap(px(10.))
                .px(px(14.))
                .py(px(10.))
                .rounded(px(10.))
                .bg(rgba(0x1F1F_1FEB))
                .border_1()
                .border_color(rgba(0xFFFF_FF1A))
                .shadow_lg()
                .text_color(white())
                .child(div().pt(px(1.)).flex_none().child(icon))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .min_w(px(0.))
                        .flex_shrink()
                        .child(div().text_size(px(13.)).font_weight(gpui::FontWeight::SEMIBOLD).child(toast.title.clone()))
                        .when_some(toast.detail.clone(), |d, detail| {
                            d.child(div().text_size(px(12.)).line_height(px(17.)).text_color(rgba(0xFFFF_FFB3)).child(detail))
                        }),
                )
                .when_some(action, |d, (label, action)| {
                    d.child(
                        div()
                            .id("toast-action")
                            .flex_none()
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(6.))
                            .bg(rgb(0x1677FF))
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0x4096FF)))
                            .child(label)
                            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                cx.stop_propagation();
                                on_action(action, window, cx);
                            }),
                    )
                })
                .when(toast.dismissable(), |d| {
                    d.child(
                        div()
                            .id("toast-close")
                            .flex_none()
                            .px(px(4.))
                            .text_size(px(14.))
                            .text_color(rgba(0xFFFF_FF80))
                            .cursor_pointer()
                            .hover(|s| s.text_color(white()))
                            .child("✕")
                            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                cx.stop_propagation();
                                on_close(window, cx);
                            }),
                    )
                }),
        )
        .into_any_element()
}
