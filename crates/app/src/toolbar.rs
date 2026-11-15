//! Toolbars: the buttons every toolbar shares (one table of icons, labels and
//! colors), the editor's toolbar and style bar, and building blocks other
//! panels (the scrolling-capture bar) reuse.

use bluesnap_core::{Color, PALETTE, SizeLevel, THEME_BLUE, Tool};
use gpui::{
    AnyElement, AnyView, App, AppContext, Context, Div, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px, rgb, rgba, svg, white,
};

use crate::editor_view::EditorView;
use crate::output::Finish;

pub const PAD: f32 = 4.0;
const GAP: f32 = 2.0;
const SEP: f32 = 1.0 + 2.0 * 4.0; // 1px line with 4px margins
const BUTTON: f32 = 32.0;
const ICON: f32 = 20.0;
const STYLE_BUTTON: f32 = 24.0;

/// A toolbar button's meaning. Each toolbar lists the actions it offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Tool(Tool),
    Undo,
    Scrolling,
    ToText,
    Edit,
    Save,
    Cancel,
    Done,
}

impl Action {
    fn icon(self) -> &'static str {
        match self {
            Action::Tool(Tool::Rect) => "icons/rect.svg",
            Action::Tool(Tool::Ellipse) => "icons/ellipse.svg",
            Action::Tool(Tool::Arrow) => "icons/arrow.svg",
            Action::Tool(Tool::Pen) => "icons/pen.svg",
            Action::Tool(Tool::Mosaic) => "icons/mosaic.svg",
            Action::Tool(Tool::Text) => "icons/text.svg",
            Action::Undo => "icons/undo.svg",
            Action::Scrolling => "icons/scroll.svg",
            Action::ToText => "icons/ocr.svg",
            Action::Edit => "icons/edit.svg",
            Action::Save => "icons/save.svg",
            Action::Cancel => "icons/cancel.svg",
            Action::Done => "icons/done.svg",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Action::Tool(Tool::Rect) => "Rectangle",
            Action::Tool(Tool::Ellipse) => "Ellipse",
            Action::Tool(Tool::Arrow) => "Arrow",
            Action::Tool(Tool::Pen) => "Pen",
            Action::Tool(Tool::Mosaic) => "Mosaic",
            Action::Tool(Tool::Text) => "Text",
            Action::Undo => "Undo",
            Action::Scrolling => "Scrolling screenshot: scroll the window to capture more",
            Action::ToText => "To text: copy the text in the selection",
            Action::Edit => "Edit: annotate in a window",
            Action::Save => "Save",
            Action::Cancel => "Cancel (Esc)",
            Action::Done => "Copy (Enter)",
        }
    }

    fn color(self) -> Hsla {
        match self {
            Action::Cancel => hsla(Color::rgb(0xF5222D)),
            Action::Done => hsla(THEME_BLUE),
            _ => hsla(Color::rgb(0x333333)),
        }
    }

    /// The session-ending actions, as an output.
    pub fn finish(self) -> Option<Finish> {
        match self {
            Action::Save => Some(Finish::Save),
            Action::Cancel => Some(Finish::Cancel),
            Action::Done => Some(Finish::Copy),
            _ => None,
        }
    }
}

const TOOLS: [Action; 6] = [
    Action::Tool(Tool::Rect),
    Action::Tool(Tool::Ellipse),
    Action::Tool(Tool::Arrow),
    Action::Tool(Tool::Pen),
    Action::Tool(Tool::Mosaic),
    Action::Tool(Tool::Text),
];
/// How every toolbar ends.
pub const FINISH: [Action; 3] = [Action::Save, Action::Cancel, Action::Done];

/// The capture overlay's toolbar, in groups separated by lines.
pub const CAPTURE_TOOLBAR: &[&[Action]] =
    &[&TOOLS, &[Action::Undo], &[Action::Scrolling, Action::ToText, FINISH[0], FINISH[1], FINISH[2]]];
/// The document editor's toolbar: no scrolling capture of an image.
pub const DOCUMENT_TOOLBAR: &[&[Action]] = &[&TOOLS, &[Action::Undo], &[Action::ToText, FINISH[0], FINISH[1], FINISH[2]]];
/// The scrolling capture's bar, after its status text.
pub const SCROLLING_BAR: &[&[Action]] = &[&[Action::Edit, FINISH[0], FINISH[1], FINISH[2]]];

/// Logical size of a panel of button `groups`. `extra` is the width of other
/// content placed first, as a group of its own (separated by a line).
pub fn panel_size(groups: &[&[Action]], extra: f32) -> (f32, f32) {
    let buttons: usize = groups.iter().map(|g| g.len()).sum();
    let has_extra = extra > 0.0;
    let separators = (groups.len() + usize::from(has_extra)).saturating_sub(1);
    let children = buttons + separators + usize::from(has_extra);
    let width = 2.0 * PAD
        + buttons as f32 * BUTTON
        + separators as f32 * SEP
        + extra
        + children.saturating_sub(1) as f32 * GAP;
    (width, 2.0 * PAD + BUTTON)
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

/// A white rounded panel laying out its children in a row.
pub fn panel() -> Div {
    div()
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
        // Clicks on the panel never reach what's underneath.
        .occlude()
}

/// A panel floating at (`x`, `y`) with an explicit size.
fn floating_panel((w, h): (f32, f32), x: f32, y: f32) -> Div {
    panel().absolute().left(px(x)).top(px(y)).w(px(w)).h(px(h))
}

pub fn separator() -> Div {
    div().w(px(1.0)).h(px(20.0)).mx(px(4.0)).bg(rgba(0x0000_0022))
}

/// The button for `action`; the caller attaches what it does. `active` shows
/// it highlighted (the current tool), `enabled: false` grays it out.
pub fn action_button(action: Action, active: bool, enabled: bool) -> Stateful<Div> {
    let blue = hsla(THEME_BLUE);
    let color = match (active, enabled) {
        (true, _) => blue,
        (_, false) => hsla(Color::rgb(0xBBBBBB)),
        _ => action.color(),
    };
    div()
        .id(action.label())
        .tooltip(tooltip(action.label()))
        .size(px(BUTTON))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .when(active, |d| d.bg(rgba(0x1677_FF1F)))
        .when(!active, |d| d.hover(|s| s.bg(rgba(0x0000_000F))))
        .child(svg().path(action.icon()).size(px(ICON)).text_color(color))
}

fn on_press(
    cx: &mut Context<EditorView>,
    f: impl Fn(&mut EditorView, &mut Window, &mut Context<EditorView>) + 'static,
) -> impl Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static {
    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
        cx.stop_propagation();
        f(this, window, cx);
        cx.notify();
    })
}

/// The editor's toolbar with `groups` of actions at (`x`, `y`).
pub fn toolbar(view: &EditorView, groups: &[&[Action]], x: f32, y: f32, cx: &mut Context<EditorView>) -> AnyElement {
    let editor = view.editor();
    let mut bar = floating_panel(panel_size(groups, 0.0), x, y);
    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            bar = bar.child(separator());
        }
        for &action in *group {
            let (active, enabled) = match action {
                Action::Tool(tool) => (editor.tool() == Some(tool), true),
                Action::Undo => (false, editor.can_undo()),
                Action::ToText => (view.is_recognizing(), true),
                _ => (false, true),
            };
            bar = bar.child(
                action_button(action, active, enabled)
                    .on_mouse_down(MouseButton::Left, on_press(cx, move |this, w, cx| this.perform(action, w, cx))),
            );
        }
    }
    bar.into_any_element()
}

pub fn style_bar(tool: Tool, size: SizeLevel, color: Color, x: f32, y: f32, cx: &mut Context<EditorView>) -> AnyElement {
    let mut bar = floating_panel(style_bar_size(tool), x, y);
    for (level, dot) in SizeLevel::ALL.into_iter().zip([4.0, 8.0, 12.0]) {
        let active = level == size;
        bar = bar.child(
            div()
                .size(px(STYLE_BUTTON))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .cursor_pointer()
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
                    .cursor_pointer()
                    .when(active, |d| d.border_2().border_color(hsla(THEME_BLUE)))
                    .child(div().size(px(16.0)).rounded(px(3.0)).bg(hsla(c)).border_1().border_color(rgba(0x0000_0033)))
                    .on_mouse_down(MouseButton::Left, on_press(cx, move |this, _, _| this.editor_mut().set_color(c))),
            );
        }
    }
    bar.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_size_counts_buttons_separators_and_gaps() {
        // 12 buttons, 2 separators, 13 gaps between the 14 children.
        assert_eq!(panel_size(CAPTURE_TOOLBAR, 0.0), (2.0 * PAD + 12.0 * BUTTON + 2.0 * SEP + 13.0 * GAP, 40.0));
        // One button less without scrolling capture.
        assert_eq!(panel_size(CAPTURE_TOOLBAR, 0.0).0 - panel_size(DOCUMENT_TOOLBAR, 0.0).0, BUTTON + GAP);
        // Extra content (the scrolling bar's status) is a group of its own:
        // status | 4 buttons = 6 children.
        assert_eq!(panel_size(SCROLLING_BAR, 200.0).0, 2.0 * PAD + 4.0 * BUTTON + SEP + 200.0 + 5.0 * GAP);
    }
}
