//! The toolbar under the selection and the style bar (sizes and colors) next to it.

use bluesnap_core::{Color, PALETTE, SizeLevel, THEME_BLUE, Tool};
use gpui::{
    AnyElement, AnyView, App, AppContext, Context, Div, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px, rgb, rgba, svg, white,
};

use crate::overlay::{Finish, Overlay};

const PAD: f32 = 4.0;
const GAP: f32 = 2.0;
const SEP: f32 = 1.0 + 2.0 * 4.0; // 1px line with 4px margins
const BUTTON: f32 = 32.0;
const ICON: f32 = 20.0;
const STYLE_BUTTON: f32 = 24.0;

const TOOLS: [(Tool, &str, &str); 6] = [
    (Tool::Rect, "icons/rect.svg", "Rectangle"),
    (Tool::Ellipse, "icons/ellipse.svg", "Ellipse"),
    (Tool::Arrow, "icons/arrow.svg", "Arrow"),
    (Tool::Pen, "icons/pen.svg", "Pen"),
    (Tool::Mosaic, "icons/mosaic.svg", "Mosaic"),
    (Tool::Text, "icons/text.svg", "Text"),
];

/// Hover label for a toolbar button.
struct Tooltip(SharedString);

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .bg(rgba(0x0000_00E0))
            .text_color(white())
            .text_size(px(12.0))
            .child(self.0.clone())
    }
}

fn tooltip(text: &'static str) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_, cx| cx.new(|_| Tooltip(text.into())).into()
}

/// Logical size of the toolbar: 6 tools | undo | extract text, save, cancel, done.
pub fn toolbar_size() -> (f32, f32) {
    let buttons = 11.0;
    let children = buttons + 2.0;
    (2.0 * PAD + buttons * BUTTON + 2.0 * SEP + (children - 1.0) * GAP, 2.0 * PAD + BUTTON)
}

/// Logical size of the style bar: 3 sizes, then (if the tool has a color) | 6 colors.
pub fn style_bar_size(tool: Tool) -> (f32, f32) {
    let (buttons, seps) = if tool.uses_color() { (3.0 + PALETTE.len() as f32, 1.0) } else { (3.0, 0.0) };
    let children = buttons + seps;
    (2.0 * PAD + buttons * STYLE_BUTTON + seps * SEP + (children - 1.0) * GAP, 2.0 * PAD + STYLE_BUTTON)
}

fn hsla(c: Color) -> Hsla {
    rgb(c.to_hex()).into()
}

fn panel(width: f32, height: f32) -> Div {
    div()
        .absolute()
        .w(px(width))
        .h(px(height))
        .p(px(PAD))
        .gap(px(GAP))
        .flex()
        .flex_row()
        .items_center()
        .bg(rgb(0xFFFFFF))
        .border_1()
        .border_color(rgba(0x0000_0022))
        .rounded(px(8.0))
        .shadow_md()
        .cursor_pointer()
        // Clicks on the panel never reach the canvas underneath.
        .occlude()
}

fn separator() -> Div {
    div().w(px(1.0)).h(px(20.0)).mx(px(4.0)).bg(rgba(0x0000_0022))
}

fn icon_button(icon: &'static str, label: &'static str, color: Hsla, active: bool) -> Stateful<Div> {
    div()
        .id(label)
        .tooltip(tooltip(label))
        .size(px(BUTTON))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .when(active, |d| d.bg(rgba(0x1677_FF1F)))
        .when(!active, |d| d.hover(|s| s.bg(rgba(0x0000_000F))))
        .child(svg().path(icon).size(px(ICON)).text_color(color))
}

fn on_press(
    cx: &mut Context<Overlay>,
    f: impl Fn(&mut Overlay, &mut Window, &mut Context<Overlay>) + 'static,
) -> impl Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static {
    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
        cx.stop_propagation();
        f(this, window, cx);
        cx.notify();
    })
}

pub fn toolbar(this: &Overlay, x: f32, y: f32, cx: &mut Context<Overlay>) -> AnyElement {
    let (w, h) = toolbar_size();
    let editor = this.editor();
    let normal = hsla(Color::rgb(0x333333));
    let blue = hsla(THEME_BLUE);
    let disabled = hsla(Color::rgb(0xBBBBBB));

    let mut bar = panel(w, h).left(px(x)).top(px(y));
    for (tool, icon, label) in TOOLS {
        let active = editor.tool() == Some(tool);
        bar = bar.child(
            icon_button(icon, label, if active { blue } else { normal }, active)
                .on_mouse_down(MouseButton::Left, on_press(cx, move |this, _, _| this.editor_mut().toggle_tool(tool))),
        );
    }
    bar.child(separator())
        .child(
            icon_button("icons/undo.svg", "Undo", if editor.can_undo() { normal } else { disabled }, false).on_mouse_down(
                MouseButton::Left,
                on_press(cx, |this, _, _| {
                    this.editor_mut().undo();
                }),
            ),
        )
        .child(separator())
        .child(
            icon_button(
                "icons/ocr.svg",
                "To text: copy the text in the selection",
                if this.is_recognizing() { blue } else { normal },
                this.is_recognizing(),
            )
                .on_mouse_down(MouseButton::Left, on_press(cx, |this, w, cx| this.extract_text(w, cx))),
        )
        .child(
            icon_button("icons/save.svg", "Save", normal, false)
                .on_mouse_down(MouseButton::Left, on_press(cx, |this, w, cx| this.finish(Finish::Save, w, cx))),
        )
        .child(
            icon_button("icons/cancel.svg", "Cancel (Esc)", hsla(Color::rgb(0xF5222D)), false)
                .on_mouse_down(MouseButton::Left, on_press(cx, |this, w, cx| this.finish(Finish::Cancel, w, cx))),
        )
        .child(
            icon_button("icons/done.svg", "Copy (Enter)", blue, false)
                .on_mouse_down(MouseButton::Left, on_press(cx, |this, w, cx| this.finish(Finish::Copy, w, cx))),
        )
        .into_any_element()
}

pub fn style_bar(tool: Tool, size: SizeLevel, color: Color, x: f32, y: f32, cx: &mut Context<Overlay>) -> AnyElement {
    let (w, h) = style_bar_size(tool);
    let mut bar = panel(w, h).left(px(x)).top(px(y));

    for (level, dot) in SizeLevel::ALL.into_iter().zip([4.0, 8.0, 12.0]) {
        let active = level == size;
        bar = bar.child(
            div()
                .size(px(STYLE_BUTTON))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .when(active, |d| d.bg(rgba(0x1677_FF1F)))
                .when(!active, |d| d.hover(|s| s.bg(rgba(0x0000_000F))))
                .child(div().size(px(dot)).rounded_full().bg(if active { hsla(THEME_BLUE) } else { rgb(0x555555).into() }))
                .on_mouse_down(MouseButton::Left, on_press(cx, move |this, _, _| this.editor_mut().set_size(level))),
        );
    }

    if tool.uses_color() {
        bar = bar.child(separator());
        for c in PALETTE {
            let active = c == color;
            bar = bar.child(
                div()
                    .size(px(STYLE_BUTTON))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .when(active, |d| d.border_2().border_color(hsla(THEME_BLUE)))
                    .child(div().size(px(16.0)).rounded(px(3.0)).bg(hsla(c)).border_1().border_color(rgba(0x0000_0033)))
                    .on_mouse_down(MouseButton::Left, on_press(cx, move |this, _, _| this.editor_mut().set_color(c))),
            );
        }
    }
    bar.into_any_element()
}
