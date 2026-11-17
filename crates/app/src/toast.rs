//! Small notices:
//! - in-editor [`Toast`]s (what's happening, what happened, or what went wrong,
//!   optionally with one action button), and
//! - a floating 3-second corner notification shown when BlueSnap starts so
//!   users know it is running in the system tray / menu bar.

use std::time::Duration;

use bluesnap_platform::{Native, PixelRect, Platform};
use gpui::{
    Animation, AnimationExt, AnyElement, AnyWindowHandle, App, AppContext, Bounds, Context, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, SharedString, Styled,
    Task, Transformation, Window, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, percentage,
    point, prelude::FluentBuilder, px, rgb, rgba, size, svg, white,
};

use crate::AppState;

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
                            .size(px(22.))
                            .rounded(px(5.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgba(0xFFFF_FF80))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgba(0xFFFF_FF1A)).text_color(white()))
                            .child(svg().path("icons/cancel.svg").size(px(13.)))
                            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                cx.stop_propagation();
                                on_close(window, cx);
                            }),
                    )
                }),
        )
        .into_any_element()
}

/// How long the launch corner notification stays visible before auto-closing.
const STARTUP_LIFETIME: Duration = Duration::from_secs(3);
const STARTUP_CARD_WIDTH: f32 = 340.0;
const STARTUP_SHADOW_PAD: f32 = 8.0;
const STARTUP_MARGIN: f32 = 16.0;

fn startup_detail(tray_location: &str, hotkey: Option<&str>) -> String {
    match hotkey.map(str::trim).filter(|hk| !hk.is_empty()) {
        Some(hk) => format!("Find it in the {tray_location}, or press {hk} to capture."),
        None => format!("Find it in the {tray_location}."),
    }
}

fn startup_card_height(detail: &str) -> f32 {
    // ~36 characters per 12px line inside the 242px text column.
    let lines = detail.chars().count().div_ceil(36).max(1) as f32;
    12.0 + 18.0 + 2.0 + 17.0 * lines + 12.0
}

fn corner_window_rect(work: PixelRect, size: (i32, i32), margin: i32, top_right: bool) -> PixelRect {
    let (w, h) = (size.0.min(work.width).max(1), size.1.min(work.height).max(1));
    let x = (work.x + work.width - w - margin).max(work.x);
    let y = if top_right {
        (work.y + margin).min((work.y + work.height - h).max(work.y))
    } else {
        (work.y + work.height - h - margin).max(work.y)
    };
    PixelRect { x, y, width: w, height: h }
}

struct StartupToast {
    detail: SharedString,
    window: Option<AnyWindowHandle>,
    dismissed: bool,
    _task: Option<Task<()>>,
}

impl StartupToast {
    fn new(detail: SharedString, cx: &mut Context<Self>) -> Self {
        cx.on_release(|this, cx| {
            this._task = None;
            this.dismissed = true;
            if let Some(w) = this.window
                && cx.try_global::<AppState>().is_some_and(|s| s.startup_toast == Some(w))
            {
                cx.global_mut::<AppState>().startup_toast = None;
            }
        })
        .detach();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(STARTUP_LIFETIME).await;
            this.update(cx, |this, cx| this.dismiss(cx)).ok();
        });
        Self { detail, window: None, dismissed: false, _task: Some(task) }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.dismissed, true) {
            return;
        }
        self._task = None;
        let window = self.window;
        if let Some(w) = window
            && cx.try_global::<AppState>().is_some_and(|s| s.startup_toast == Some(w))
        {
            cx.global_mut::<AppState>().startup_toast = None;
        }
        if let Some(w) = window {
            cx.defer(move |cx| {
                w.update(cx, |_, window, _| window.remove_window()).ok();
            });
        }
    }
}

impl Render for StartupToast {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().p(px(STARTUP_SHADOW_PAD)).child(
            div()
                .id("startup-toast")
                .occlude()
                .size_full()
                .px(px(14.))
                .py(px(12.))
                .rounded(px(10.))
                .bg(rgba(0x1F1F_1FF2))
                .border_1()
                .border_color(rgba(0xFFFF_FF1F))
                .shadow_md()
                .text_color(white())
                .flex()
                .items_start()
                .gap(px(10.))
                .child(
                    div()
                        .flex_none()
                        .mt(px(1.))
                        .size(px(28.))
                        .rounded(px(7.))
                        .bg(rgb(0x1677FF))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(svg().path("icons/app.svg").size(px(18.)).text_color(white())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(13.))
                                .line_height(px(18.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("BlueSnap is running"),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .line_height(px(17.))
                                .text_color(rgba(0xFFFF_FFB8))
                                .child(self.detail.clone()),
                        ),
                )
                .child(
                    div()
                        .id("startup-toast-close")
                        .flex_none()
                        .size(px(24.))
                        .rounded(px(6.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgba(0xFFFF_FF8C))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgba(0xFFFF_FF1A)).text_color(white()))
                        .child(svg().path("icons/cancel.svg").size(px(14.)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.dismiss(cx);
                            }),
                        ),
                ),
        )
    }
}

/// Shows a 3-second non-activating corner toast telling the user BlueSnap is
/// running and where to find its tray / menu bar icon.
pub fn show_startup(hotkey: Option<&str>, cx: &mut App) {
    dismiss_startup(cx);
    let platform = bluesnap_platform::native();
    let (display, work) = match platform.notification_area() {
        Ok(area) => area,
        Err(e) => return log::debug!("could not determine notification area: {e:#}"),
    };
    let Some(gpui_display) = cx.displays().into_iter().find(|d| u32::from(d.id()) == display.id) else {
        return;
    };
    let detail = startup_detail(Native::TRAY_LOCATION, hotkey);
    let scale = display.scale_factor;
    let logical_w = STARTUP_CARD_WIDTH + 2.0 * STARTUP_SHADOW_PAD;
    let logical_h = startup_card_height(&detail) + 2.0 * STARTUP_SHADOW_PAD;
    let size_px = ((logical_w * scale).round() as i32, (logical_h * scale).round() as i32);
    let margin_px = ((STARTUP_MARGIN - STARTUP_SHADOW_PAD).max(0.0) * scale).round() as i32;
    let win_rect = corner_window_rect(work, size_px, margin_px, Native::NOTIFY_TOP_RIGHT);

    let origin = gpui_display.bounds().origin;
    let bounds = Bounds::new(
        point(origin.x + px(win_rect.x as f32 / scale), origin.y + px(win_rect.y as f32 / scale)),
        size(px(win_rect.width as f32 / scale), px(win_rect.height as f32 / scale)),
    );
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        focus: false,
        show: !cfg!(any(target_os = "windows", target_os = "macos")),
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        display_id: Some(gpui_display.id()),
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };
    let detail: SharedString = detail.into();
    let Ok(handle) = cx.open_window(options, |_, cx| cx.new(|cx| StartupToast::new(detail, cx))) else {
        return;
    };
    let win_ref = handle.update(cx, |this, window, _| {
        this.window = Some(window.window_handle());
        bluesnap_platform::window_ref(window)
    });
    let Ok(Ok(win_ref)) = win_ref else {
        handle.update(cx, |_, window, _| window.remove_window()).ok();
        return;
    };
    cx.global_mut::<AppState>().startup_toast = Some(handle.into());

    cx.spawn(async move |cx| {
        if !handle.read_with(cx, |this, _| !this.dismissed).unwrap_or(false) {
            return;
        }
        if let Err(e) = platform.place_floating_window(win_ref, &display, win_rect, None, None) {
            log::warn!("startup toast window setup: {e:#}");
        }
    })
    .detach();
}

/// Immediately closes the startup corner toast if it is still open; returns
/// `true` if a toast was dismissed.
pub fn dismiss_startup(cx: &mut App) -> bool {
    let Some(handle) = cx.global_mut::<AppState>().startup_toast.take() else {
        return false;
    };
    handle.update(cx, |_, window, _| window.remove_window()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_detail_includes_location_and_optional_hotkey() {
        assert_eq!(
            startup_detail("system tray", Some("Ctrl+Alt+A")),
            "Find it in the system tray, or press Ctrl+Alt+A to capture."
        );
        assert_eq!(
            startup_detail("menu bar", Some("Ctrl+Cmd+A")),
            "Find it in the menu bar, or press Ctrl+Cmd+A to capture."
        );
        assert_eq!(startup_detail("system tray", None), "Find it in the system tray.");
        assert_eq!(startup_detail("menu bar", Some("   ")), "Find it in the menu bar.");
    }

    #[test]
    fn corner_rect_respects_work_area_on_windows_and_macos() {
        // Windows: 2560x1440 with a 72px taskbar at the bottom -> bottom-right above taskbar.
        let win_work = PixelRect { x: 0, y: 0, width: 2560, height: 1368 };
        let r = corner_window_rect(win_work, (534, 141), 12, false);
        assert_eq!(r, PixelRect { x: 2014, y: 1215, width: 534, height: 141 });

        // macOS: 3024x1964 Retina with a 74px menu bar at the top and 140px Dock at the bottom -> top-right below menu bar.
        let mac_work = PixelRect { x: 0, y: 74, width: 3024, height: 1750 };
        let r = corner_window_rect(mac_work, (712, 188), 16, true);
        assert_eq!(r, PixelRect { x: 2296, y: 90, width: 712, height: 188 });

        // Tiny work area clamps cleanly without going out of bounds.
        let tiny = PixelRect { x: 10, y: 20, width: 200, height: 80 };
        let r = corner_window_rect(tiny, (500, 200), 16, false);
        assert_eq!(r, PixelRect { x: 10, y: 20, width: 200, height: 80 });
    }
}
