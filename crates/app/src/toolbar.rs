//! First-class reusable toolbars and style bars shared across the Capture
//! overlay, the Composition Editor window, and the Scrolling Capture bar.

use std::sync::Arc;

use bluesnap_core::{
    ArrowStyle, BACKGROUND_PALETTE, Color, CounterFormat, LayerOrder, PALETTE, RedactArea, RedactEffect, Redaction,
    SizeLevel, THEME_BLUE, TextStyle, Tool,
};
use gpui::{
    AnyElement, AnyView, App, AppContext, Context, Div, FontWeight, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px, rgb, rgba, svg, white,
};

use crate::canvas_view::CanvasView;
use crate::output::Finish;

pub const PAD: f32 = 4.0;
const GAP: f32 = 2.0;
const SEP: f32 = 1.0 + 2.0 * 4.0; // 1px line with 4px margins
const BUTTON: f32 = 32.0;
const ICON: f32 = 20.0;
const STYLE_BUTTON: f32 = 24.0;
const STYLE_ICON: f32 = 16.0;
/// The panel's border, included in its size.
const BORDER: f32 = 1.0;
/// The redact strength slider: its width, the inset of the track's ends (room
/// for the thumb), and the thumb's diameter.
const SLIDER: f32 = 120.0;
const SLIDER_INSET: f32 = 9.0;
const THUMB: f32 = 14.0;
/// Width of the text style bar's style picker button.
const TEXT_STYLE: f32 = 132.0;
/// Width of the redact bar's Auto button (AI redaction).
const AUTO: f32 = 64.0;

/// Where a toolbar is shown, which adjusts labels and shortcut hints for `Cancel` and `Done`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolbarMode {
    /// Standalone capture overlay or scrolling capture bar.
    Capture,
    /// Capture overlay or scrolling bar triggered by Editor's `"Add image"`.
    CaptureForEditor,
    /// Composition Editor window (`Esc` clears tool/selection, bare `Enter` does not close).
    Editor,
}

/// A toolbar button's meaning. Each surface composes the actions it offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Leftmost button in the Editor: hides the Editor and captures a new image into it.
    AddImage,
    /// Opens an existing `.bs` composition or raster image file in the Editor.
    OpenFile,
    Tool(Tool),
    Undo,
    Scrolling,
    ToText,
    /// Sends the capture (raw image + unflattened annotations) into the Editor.
    AddToEditor,
    Save,
    Cancel,
    Done,
}

impl Action {
    fn icon(self) -> &'static str {
        match self {
            Action::AddImage => "icons/add-image.svg",
            Action::OpenFile => "icons/open.svg",
            Action::Tool(Tool::Rect) => "icons/rect.svg",
            Action::Tool(Tool::Ellipse) => "icons/ellipse.svg",
            Action::Tool(Tool::Arrow) => "icons/arrow.svg",
            Action::Tool(Tool::Pen) => "icons/pen.svg",
            Action::Tool(Tool::Highlighter) => "icons/highlighter.svg",
            Action::Tool(Tool::Counter) => "icons/counter.svg",
            Action::Tool(Tool::Redact) => "icons/mosaic.svg",
            Action::Tool(Tool::Text) => "icons/text.svg",
            Action::Undo => "icons/undo.svg",
            Action::Scrolling => "icons/scroll.svg",
            Action::ToText => "icons/ocr.svg",
            Action::AddToEditor => "icons/edit.svg",
            Action::Save => "icons/save.svg",
            Action::Cancel => "icons/cancel.svg",
            Action::Done => "icons/done.svg",
        }
    }

    pub fn label_for(self, mode: ToolbarMode) -> &'static str {
        match self {
            Action::AddImage => "Add image: capture another screenshot",
            Action::OpenFile => {
                if cfg!(target_os = "macos") {
                    "Open .bs or image file (⌘O)"
                } else {
                    "Open .bs or image file (Ctrl+O)"
                }
            }
            Action::Tool(Tool::Rect) => "Rectangle",
            Action::Tool(Tool::Ellipse) => "Ellipse",
            Action::Tool(Tool::Arrow) => "Arrow",
            Action::Tool(Tool::Pen) => "Pen",
            Action::Tool(Tool::Highlighter) => "Highlighter (Shift: straight line)",
            Action::Tool(Tool::Counter) => "Counter: click to place 1, 2, 3 …",
            Action::Tool(Tool::Redact) => "Redact",
            Action::Tool(Tool::Text) => "Text",
            Action::Undo => {
                if cfg!(target_os = "macos") {
                    "Undo (⌘Z)"
                } else {
                    "Undo (Ctrl+Z)"
                }
            }
            Action::Scrolling => "Scrolling screenshot: scroll the window to capture more",
            Action::ToText => "To text: copy the text in the selection",
            Action::AddToEditor => "Add to editor",
            Action::Save => {
                if cfg!(target_os = "macos") {
                    "Save (.png or .bs) (⌘S)"
                } else {
                    "Save (.png or .bs) (Ctrl+S)"
                }
            }
            Action::Cancel => match mode {
                ToolbarMode::Editor => "Close",
                ToolbarMode::Capture | ToolbarMode::CaptureForEditor => "Cancel (Esc)",
            },
            Action::Done => match mode {
                ToolbarMode::CaptureForEditor => "Add to editor (Enter)",
                ToolbarMode::Editor => "Copy and close",
                ToolbarMode::Capture => "Copy (Enter)",
            },
        }
    }

    fn color(self) -> Hsla {
        match self {
            Action::AddImage | Action::Done => hsla(THEME_BLUE),
            Action::Cancel => hsla(Color::rgb(0xF5222D)),
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

const TOOLS: [Action; Tool::ALL.len()] = [
    Action::Tool(Tool::Rect),
    Action::Tool(Tool::Ellipse),
    Action::Tool(Tool::Arrow),
    Action::Tool(Tool::Pen),
    Action::Tool(Tool::Highlighter),
    Action::Tool(Tool::Counter),
    Action::Tool(Tool::Redact),
    Action::Tool(Tool::Text),
];
/// How every toolbar ends.
pub const FINISH: [Action; 3] = [Action::Save, Action::Cancel, Action::Done];

/// Standalone capture overlay toolbar: includes `AddToEditor`.
pub const CAPTURE_TOOLBAR: &[&[Action]] = &[
    &TOOLS,
    &[Action::Undo],
    &[Action::Scrolling, Action::ToText, Action::AddToEditor, FINISH[0], FINISH[1], FINISH[2]],
];

/// Capture overlay toolbar when capturing a new image from inside the Editor:
/// `AddToEditor` is hidden and `Done` adds the image to the open Editor.
pub const CAPTURE_FOR_EDITOR_TOOLBAR: &[&[Action]] =
    &[&TOOLS, &[Action::Undo], &[Action::Scrolling, Action::ToText, FINISH[0], FINISH[1], FINISH[2]]];

/// Composition Editor toolbar: `AddImage` at the far left, followed by `OpenFile`,
/// then all annotation tools, Undo, ToText, Save, Cancel, and Done (Copy).
pub const EDITOR_TOOLBAR: &[&[Action]] = &[
    &[Action::AddImage, Action::OpenFile],
    &TOOLS,
    &[Action::Undo],
    &[Action::ToText, FINISH[0], FINISH[1], FINISH[2]],
];

/// Scrolling capture bar (standalone): includes `AddToEditor`.
pub const SCROLLING_BAR: &[&[Action]] = &[&[Action::AddToEditor, FINISH[0], FINISH[1], FINISH[2]]];

/// Scrolling capture bar when triggered from the Editor: `AddToEditor` is hidden.
pub const SCROLLING_FOR_EDITOR_BAR: &[&[Action]] = &[&[FINISH[0], FINISH[1], FINISH[2]]];

/// One child of a panel row, in order: a named control of some width, or a
/// separator line. Panel sizes and control positions are both computed from
/// these lists, so they always agree with each other.
#[derive(Clone, Debug, PartialEq)]
pub enum Slot {
    Item(String, f32),
    Sep,
}

/// Logical size of a panel holding `slots` (`height` is its controls' height).
fn row_size(slots: &[Slot], height: f32) -> (f32, f32) {
    let widths: f32 = slots
        .iter()
        .map(|s| match s {
            Slot::Item(_, w) => *w,
            Slot::Sep => SEP,
        })
        .sum();
    let gaps = slots.len().saturating_sub(1) as f32 * GAP;
    (2.0 * (BORDER + PAD) + widths + gaps, 2.0 * (BORDER + PAD) + height)
}

/// Each named control of a panel at `origin` holding `slots`, with its bounds.
pub fn row_items(slots: &[Slot], height: f32, origin: (f32, f32)) -> Vec<(String, (f32, f32, f32, f32))> {
    let (mut x, y) = (origin.0 + BORDER + PAD, origin.1 + BORDER + PAD);
    let mut out = Vec::new();
    for slot in slots {
        let w = match slot {
            Slot::Item(name, w) => {
                out.push((name.clone(), (x, y, *w, height)));
                *w
            }
            Slot::Sep => SEP,
        };
        x += w + GAP;
    }
    out
}

/// A toolbar's slots: `extra` content first (the scrolling bar's status) as a
/// group of its own, then the button groups, separated by lines.
pub fn toolbar_slots(groups: &[&[Action]], extra: f32) -> Vec<Slot> {
    let mut slots = Vec::new();
    if extra > 0.0 {
        slots.extend([Slot::Item("status".into(), extra), Slot::Sep]);
    }
    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            slots.push(Slot::Sep);
        }
        slots.extend(group.iter().map(|a| Slot::Item(format!("{a:?}"), BUTTON)));
    }
    slots
}

/// Logical size of a panel of button `groups`.
pub fn panel_size(groups: &[&[Action]], extra: f32) -> (f32, f32) {
    row_size(&toolbar_slots(groups, extra), BUTTON)
}

/// Slots for `tool`'s style bar. When `with_layer_ops` is `true` (an element is
/// selected in the Composition Editor), appends `| BringForward, SendBackward, Delete`.
pub fn style_slots_ex(tool: Tool, with_layer_ops: bool) -> Vec<Slot> {
    let item = |name: String, w| Slot::Item(name, w);
    let sizes = || SizeLevel::ALL.map(|l| item(format!("size:{l:?}"), STYLE_BUTTON));
    let mut slots = Vec::new();
    if tool == Tool::Redact {
        slots.extend(RedactArea::ALL.map(|a| item(format!("area:{a:?}"), STYLE_BUTTON)));
        slots.push(Slot::Sep);
        slots.extend(RedactEffect::ALL.map(|e| item(format!("effect:{e:?}"), STYLE_BUTTON)));
        slots.extend([Slot::Sep, item("slider".into(), SLIDER), Slot::Sep]);
        slots.extend(sizes());
        slots.extend([Slot::Sep, item("auto".into(), AUTO)]);
    } else {
        slots.extend(sizes());
        if tool.uses_color() {
            slots.push(Slot::Sep);
            slots.extend((0..PALETTE.len()).map(|i| item(format!("color:{i}"), STYLE_BUTTON)));
        }
        match tool {
            Tool::Counter => {
                slots.push(Slot::Sep);
                slots.extend(CounterFormat::ALL.map(|f| item(format!("numbering:{f:?}"), STYLE_BUTTON)));
            }
            Tool::Text => slots.extend([Slot::Sep, item("text-style".into(), TEXT_STYLE)]),
            Tool::Arrow => {
                slots.push(Slot::Sep);
                slots.extend(ArrowStyle::ALL.map(|s| item(format!("arrow-style:{s:?}"), STYLE_BUTTON)));
            }
            _ => {}
        }
    }
    if with_layer_ops {
        slots.extend([
            Slot::Sep,
            item("layer:forward".into(), STYLE_BUTTON),
            item("layer:backward".into(), STYLE_BUTTON),
            item("layer:delete".into(), STYLE_BUTTON),
        ]);
    }
    slots
}

pub fn style_slots(tool: Tool) -> Vec<Slot> {
    style_slots_ex(tool, false)
}

pub fn style_bar_size_ex(tool: Tool, with_layer_ops: bool) -> (f32, f32) {
    row_size(&style_slots_ex(tool, with_layer_ops), STYLE_BUTTON)
}

pub fn style_bar_size(tool: Tool) -> (f32, f32) {
    style_bar_size_ex(tool, false)
}

/// Height of the tallest style bar any capture tool shows: the room the
/// capture overlay keeps beside its toolbar so picking a tool never moves it.
pub fn tallest_style_bar() -> f32 {
    Tool::ALL.iter().map(|&tool| style_bar_size(tool).1).fold(0.0, f32::max)
}

/// Slots & size of the standalone layer-actions bar shown when an `Element::Image`
/// is selected in the Composition Editor (`Bring Forward | Send Backward | Delete`).
pub fn image_bar_slots() -> Vec<Slot> {
    vec![
        Slot::Item("layer:forward".into(), STYLE_BUTTON),
        Slot::Item("layer:backward".into(), STYLE_BUTTON),
        Slot::Sep,
        Slot::Item("layer:delete".into(), STYLE_BUTTON),
    ]
}

pub fn image_bar_size() -> (f32, f32) {
    row_size(&image_bar_slots(), STYLE_BUTTON)
}

fn style_item(tool: Tool, name: &str, origin: (f32, f32)) -> (f32, f32, f32, f32) {
    row_items(&style_slots(tool), STYLE_BUTTON, origin)
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, r)| r)
        .expect("the style bar has this control")
}

pub fn text_style_button(bar_x: f32) -> (f32, f32) {
    let (x, _, w, _) = style_item(Tool::Text, "text-style", (bar_x, 0.0));
    (x, w)
}

fn slider_ends(bar_x: f32) -> (f32, f32) {
    let (x, _, w, _) = style_item(Tool::Redact, "slider", (bar_x, 0.0));
    (x + SLIDER_INSET, x + w - SLIDER_INSET)
}

pub fn slider_value(bar_x: f32, x: f32) -> f32 {
    let (lo, hi) = slider_ends(bar_x);
    ((x - lo) / (hi - lo)).clamp(0.0, 1.0)
}

pub fn hsla(c: Color) -> Hsla {
    rgb(c.to_hex()).into()
}

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
        .occlude()
}

fn floating_panel((w, h): (f32, f32), x: f32, y: f32) -> Div {
    panel().absolute().left(px(x)).top(px(y)).w(px(w)).h(px(h))
}

pub fn separator() -> Div {
    div().flex_none().w(px(1.0)).h(px(20.0)).mx(px(4.0)).bg(rgba(0x0000_0022))
}

/// The button for `action`; `mode` adjusts tooltips/shortcuts for Capture vs Editor.
pub fn action_button(action: Action, active: bool, enabled: bool, mode: ToolbarMode) -> Stateful<Div> {
    let blue = hsla(THEME_BLUE);
    let color = match (active, enabled) {
        (true, _) => blue,
        (_, false) => hsla(Color::rgb(0xBBBBBB)),
        _ => action.color(),
    };
    let label = action.label_for(mode);
    let tip = if action == Action::ToText && active { "Recognizing text…" } else { label };
    div()
        .id(label)
        .tooltip(tooltip(tip))
        .size(px(BUTTON))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .when(enabled, |d| d.cursor_pointer())
        .when(active, |d| d.bg(rgba(0x1677_FF1F)))
        .when(enabled && !active, |d| d.hover(|s| s.bg(rgba(0x0000_000F))))
        .child(svg().path(action.icon()).size(px(ICON)).text_color(color))
}

fn on_press(
    cx: &mut Context<CanvasView>,
    f: impl Fn(&mut CanvasView, &mut Window, &mut Context<CanvasView>) + 'static,
) -> impl Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static {
    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
        cx.stop_propagation();
        f(this, window, cx);
        cx.notify();
    })
}

/// Renders the main toolbar with `groups` of actions at (`x`, `y`).
pub fn toolbar(
    view: &CanvasView,
    groups: &[&[Action]],
    mode: ToolbarMode,
    x: f32,
    y: f32,
    cx: &mut Context<CanvasView>,
) -> AnyElement {
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
                Action::ToText => (view.is_recognizing(), !view.is_recognizing()),
                _ => (false, true),
            };
            bar = bar.child(action_button(action, active, enabled, mode).when(enabled, |b| {
                b.on_mouse_down(MouseButton::Left, on_press(cx, move |this, w, cx| this.perform(action, w, cx)))
            }));
        }
    }
    bar.into_any_element()
}

fn style_button(active: bool) -> Div {
    div()
        .size(px(STYLE_BUTTON))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .when(active, |d| d.bg(rgba(0x1677_FF1F)))
}

fn segment(label: &'static str, icon: &'static str, active: bool) -> Stateful<Div> {
    let color = if active { hsla(THEME_BLUE) } else { rgb(0x444444).into() };
    style_button(active)
        .id(label)
        .tooltip(tooltip(label))
        .cursor_pointer()
        .when(!active, |d| d.hover(|s| s.bg(rgba(0x0000_000F))))
        .child(svg().path(icon).size(px(STYLE_ICON)).text_color(color))
}

fn icon_op_button(
    id: &'static str,
    label: &'static str,
    icon: &'static str,
    enabled: bool,
    danger: bool,
) -> Stateful<Div> {
    let color: Hsla = if !enabled {
        rgb(0xC8C8C8).into()
    } else if danger {
        hsla(Color::rgb(0xF5222D))
    } else {
        rgb(0x444444).into()
    };
    style_button(false)
        .id(id)
        .tooltip(tooltip(label))
        .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(rgba(0x0000_000F))))
        .child(svg().path(icon).size(px(STYLE_ICON)).text_color(color))
}

fn append_layer_ops(mut bar: Div, (can_back, can_fwd): (bool, bool), with_leading_sep: bool, cx: &mut Context<CanvasView>) -> Div {
    if with_leading_sep {
        bar = bar.child(separator());
    }
    bar = bar.child(
        icon_op_button("layer:forward", "Bring forward (])", "icons/bring-forward.svg", can_fwd, false).when(
            can_fwd,
            |b| {
                b.on_mouse_down(
                    MouseButton::Left,
                    on_press(cx, |this, _, _| {
                        this.editor_mut().reorder_selected(LayerOrder::Forward);
                    }),
                )
            },
        ),
    );
    bar = bar.child(
        icon_op_button("layer:backward", "Send backward ([)", "icons/send-backward.svg", can_back, false).when(
            can_back,
            |b| {
                b.on_mouse_down(
                    MouseButton::Left,
                    on_press(cx, |this, _, _| {
                        this.editor_mut().reorder_selected(LayerOrder::Backward);
                    }),
                )
            },
        ),
    );
    if !with_leading_sep {
        bar = bar.child(separator());
    }
    bar.child(
        icon_op_button("layer:delete", "Delete (Del)", "icons/trash.svg", true, true).on_mouse_down(
            MouseButton::Left,
            on_press(cx, |this, _, _| {
                this.editor_mut().delete_selected();
            }),
        ),
    )
}

/// Standalone bar shown when a placed `Image` is selected in the Composition Editor.
pub fn image_bar((can_back, can_fwd): (bool, bool), x: f32, y: f32, cx: &mut Context<CanvasView>) -> AnyElement {
    let bar = floating_panel(image_bar_size(), x, y);
    append_layer_ops(bar, (can_back, can_fwd), false, cx).into_any_element()
}

fn size_dots(mut bar: Div, size: SizeLevel, enabled: bool, cx: &mut Context<CanvasView>) -> Div {
    for (level, dot) in SizeLevel::ALL.into_iter().zip([4.0, 8.0, 12.0]) {
        let active = enabled && level == size;
        let color: Hsla = match (active, enabled) {
            (true, _) => hsla(THEME_BLUE),
            (_, false) => rgb(0xC8C8C8).into(),
            _ => rgb(0x555555).into(),
        };
        let button = style_button(active).child(div().size(px(dot)).rounded_full().bg(color));
        bar = bar.child(if enabled {
            button
                .cursor_pointer()
                .when(!active, |d| d.hover(|s| s.bg(rgba(0x0000_000F))))
                .on_mouse_down(MouseButton::Left, on_press(cx, move |this, _, _| this.editor_mut().set_size(level)))
        } else {
            button
        });
    }
    bar
}

/// What a style bar shows beyond sizes and colors.
pub enum Extra {
    None,
    Numbering(CounterFormat),
    TextStyle(TextStyle, bool),
    ArrowStyle(ArrowStyle),
}

pub fn style_bar(
    tool: Tool,
    style: bluesnap_core::Style,
    extra: Extra,
    layer_ops: Option<(bool, bool)>,
    x: f32,
    y: f32,
    cx: &mut Context<CanvasView>,
) -> AnyElement {
    let (size, color) = (style.size, style.color);
    let mut bar = size_dots(floating_panel(style_bar_size_ex(tool, layer_ops.is_some()), x, y), size, true, cx);
    if tool.uses_color() {
        bar = bar.child(separator());
        for c in PALETTE {
            let active = c == color;
            bar = bar.child(
                style_button(false)
                    .cursor_pointer()
                    .when(active, |d| d.border_2().border_color(hsla(THEME_BLUE)))
                    .child(div().size(px(16.0)).rounded(px(3.0)).bg(hsla(c)).border_1().border_color(rgba(0x0000_0033)))
                    .on_mouse_down(MouseButton::Left, on_press(cx, move |this, _, _| this.editor_mut().set_color(c))),
            );
        }
    }
    if let Extra::TextStyle(style, open) = extra {
        bar = bar.child(separator()).child(
            div()
                .id("text-style")
                .tooltip(tooltip("Text style"))
                .flex_none()
                .w(px(TEXT_STYLE))
                .h(px(STYLE_BUTTON))
                .px(px(8.))
                .flex()
                .items_center()
                .justify_between()
                .rounded(px(4.))
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(0x333333))
                .cursor_pointer()
                .when(open, |d| d.bg(rgba(0x1677_FF1F)).text_color(hsla(THEME_BLUE)))
                .when(!open, |d| d.hover(|s| s.bg(rgba(0x0000_000F))))
                .child(style.label())
                .child(div().text_size(px(9.)).child(if open { "▲" } else { "▼" }))
                .on_mouse_down(MouseButton::Left, on_press(cx, |this, _, _| this.toggle_text_styles())),
        );
    }
    if let Extra::ArrowStyle(current) = extra {
        bar = bar.child(separator());
        for style in ArrowStyle::ALL {
            let (label, icon) = match style {
                ArrowStyle::Straight => ("Straight arrow", "icons/arrow.svg"),
                ArrowStyle::Elbow => ("Elbow arrow: right-angle turns", "icons/arrow-elbow.svg"),
                ArrowStyle::Curved => ("Curved arrow", "icons/arrow-curved.svg"),
            };
            bar = bar.child(
                segment(label, icon, style == current)
                    .on_mouse_down(MouseButton::Left, on_press(cx, move |this, _, _| this.editor_mut().set_arrow_style(style))),
            );
        }
    }
    if let Extra::Numbering(current) = extra {
        bar = bar.child(separator());
        for format in CounterFormat::ALL {
            let (glyph, label) = match format {
                CounterFormat::Decimal => ("1", "Numbers: 1, 2, 3 …"),
                CounterFormat::UpperAlpha => ("A", "Letters: A, B, C …"),
                CounterFormat::Roman => ("I", "Roman numerals: I, II, III …"),
                CounterFormat::LowerAlpha => ("a", "Lowercase letters: a, b, c …"),
            };
            let active = format == current;
            bar = bar.child(
                style_button(active)
                    .id(label)
                    .tooltip(tooltip(label))
                    .cursor_pointer()
                    .when(!active, |d| d.hover(|s| s.bg(rgba(0x0000_000F))))
                    .text_size(px(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(if active { hsla(THEME_BLUE) } else { rgb(0x444444).into() })
                    .child(glyph)
                    .on_mouse_down(
                        MouseButton::Left,
                        on_press(cx, move |this, _, _| this.editor_mut().set_counter_format(format)),
                    ),
            );
        }
    }
    if let Some(ops) = layer_ops {
        bar = append_layer_ops(bar, ops, true, cx);
    }
    bar.into_any_element()
}

pub type TextPreview = (TextStyle, Arc<gpui::RenderImage>, (f32, f32));

const MENU_ROW: f32 = 36.0;

pub fn text_style_menu_row(i: usize) -> (f32, f32, f32, f32) {
    (BORDER + PAD, BORDER + PAD + i as f32 * MENU_ROW, 0.0, MENU_ROW)
}

pub fn text_style_menu_size(count: usize) -> (f32, f32) {
    (190.0, 2.0 * (BORDER + PAD) + count as f32 * MENU_ROW)
}

pub fn text_style_menu(
    previews: &[TextPreview],
    current: TextStyle,
    dark: bool,
    x: f32,
    y: f32,
    cx: &mut Context<CanvasView>,
) -> AnyElement {
    let (w, h) = text_style_menu_size(previews.len());
    let mut menu = div()
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(w))
        .h(px(h))
        .p(px(PAD))
        .flex()
        .flex_col()
        .rounded(px(8.))
        .border_1()
        .border_color(rgba(0x0000_0022))
        .shadow_lg()
        .occlude()
        .bg(if dark { rgb(0x2A2A2E) } else { rgb(0xFFFFFF) });
    for (style, image, (iw, ih)) in previews.iter().cloned() {
        let active = style == current;
        menu = menu.child(
            div()
                .id(style.label())
                .h(px(MENU_ROW))
                .flex_none()
                .px(px(8.))
                .flex()
                .items_center()
                .justify_between()
                .rounded(px(5.))
                .cursor_pointer()
                .when(active, |d| d.bg(rgba(0x1677_FF26)))
                .when(!active, |d| d.hover(|s| s.bg(if dark { rgba(0xFFFF_FF14) } else { rgba(0x0000_000D) })))
                .child(gpui::img(image).w(px(iw)).h(px(ih)))
                .when(active, |d| d.child(svg().path("icons/done.svg").size(px(14.)).text_color(hsla(THEME_BLUE))))
                .on_mouse_down(MouseButton::Left, on_press(cx, move |this, _, _| this.choose_text_style(style))),
        );
    }
    menu.into_any_element()
}

const BG_COLS: usize = 6;
const BG_SWATCH_BTN: f32 = 26.0;
const BG_SWATCH_GAP: f32 = 4.0;
const BG_PAD: f32 = 8.0;

pub fn background_menu_size() -> (f32, f32) {
    let rows = BACKGROUND_PALETTE.len().div_ceil(BG_COLS);
    let inner_w = BG_COLS as f32 * BG_SWATCH_BTN + (BG_COLS - 1) as f32 * BG_SWATCH_GAP;
    let swatches_h = rows as f32 * BG_SWATCH_BTN + rows.saturating_sub(1) as f32 * BG_SWATCH_GAP;
    let w = 2.0 * (BORDER + BG_PAD) + inner_w;
    // Title (18) + gap (6) + swatches + divider (1 + 2*6) + footer (24).
    let h = 2.0 * (BORDER + BG_PAD) + 18.0 + 6.0 + swatches_h + 13.0 + 24.0;
    (w, h)
}

pub fn background_menu_swatch(i: usize) -> (f32, f32, f32, f32) {
    let row = i / BG_COLS;
    let col = i % BG_COLS;
    let x = BORDER + BG_PAD + col as f32 * (BG_SWATCH_BTN + BG_SWATCH_GAP);
    let y = BORDER + BG_PAD + 18.0 + 6.0 + row as f32 * (BG_SWATCH_BTN + BG_SWATCH_GAP);
    (x, y, BG_SWATCH_BTN, BG_SWATCH_BTN)
}

fn color_chip(color: Color, chip_px: f32) -> Div {
    if color.is_transparent() {
        let half = chip_px / 2.0;
        div()
            .size(px(chip_px))
            .rounded(px(3.5))
            .overflow_hidden()
            .bg(white())
            .border_1()
            .border_color(rgba(0x0000_0033))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .child(div().size(px(half)).bg(rgb(0xCBD5E1)))
                    .child(div().size(px(half)).bg(white())),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .child(div().size(px(half)).bg(white()))
                    .child(div().size(px(half)).bg(rgb(0xCBD5E1))),
            )
    } else {
        div().size(px(chip_px)).rounded(px(3.5)).bg(hsla(color)).border_1().border_color(rgba(0x0000_0033))
    }
}

/// Context-menu-style floating toolbar anchored under the cursor when the user
/// right-clicks exposed background space in the Composition Editor.
pub fn background_menu(
    current: Color,
    has_custom: bool,
    x: f32,
    y: f32,
    cx: &mut Context<CanvasView>,
) -> AnyElement {
    let (w, h) = background_menu_size();
    let mut card = div()
        .id("background-color-menu")
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(w))
        .h(px(h))
        .p(px(BG_PAD))
        .flex()
        .flex_col()
        .rounded(px(8.0))
        .bg(white())
        .border_1()
        .border_color(rgba(0x0000_0022))
        .shadow_lg()
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .h(px(18.0))
                .mb(px(6.0))
                .flex()
                .items_center()
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(0x333333))
                .child("Background color"),
        );

    for (row_idx, chunk) in BACKGROUND_PALETTE.chunks(BG_COLS).enumerate() {
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(BG_SWATCH_GAP))
            .when(row_idx > 0, |d| d.mt(px(BG_SWATCH_GAP)));
        for &(color, label) in chunk {
            let active = color == current;
            row = row.child(
                div()
                    .id(label)
                    .tooltip(tooltip(label))
                    .size(px(BG_SWATCH_BTN))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .when(active, |d| d.border_2().border_color(hsla(THEME_BLUE)))
                    .when(!active, |d| d.hover(|s| s.bg(rgba(0x0000_000F))))
                    .child(color_chip(color, 18.0))
                    .on_mouse_down(
                        MouseButton::Left,
                        on_press(cx, move |this, _, _| this.choose_background_color(color)),
                    ),
            );
        }
        card = card.child(row);
    }

    let hex_label = if current.is_transparent() {
        "Transparent".to_string()
    } else {
        format!("#{:06X}", current.to_hex())
    };
    let mut footer = div()
        .h(px(24.0))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(color_chip(current, 14.0))
                .child(div().text_size(px(11.5)).text_color(rgb(0x555555)).child(hex_label)),
        );
    if has_custom {
        footer = footer.child(
            div()
                .id("bg-custom-color")
                .tooltip(tooltip("Choose a custom background color"))
                .px(px(7.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .rounded(px(4.0))
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(hsla(THEME_BLUE))
                .cursor_pointer()
                .hover(|s| s.bg(rgba(0x1677_FF1A)))
                .child("Custom…")
                .on_mouse_down(
                    MouseButton::Left,
                    on_press(cx, |this, window, cx| this.pick_custom_background_color(window, cx)),
                ),
        );
    }

    card.child(div().w_full().h(px(1.0)).my(px(6.0)).bg(rgba(0x0000_0014)))
        .child(footer)
        .into_any_element()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auto {
    Ready,
    Running,
}

pub fn redact_bar(
    redaction: Redaction,
    size: SizeLevel,
    auto: Auto,
    layer_ops: Option<(bool, bool)>,
    x: f32,
    y: f32,
    cx: &mut Context<CanvasView>,
) -> AnyElement {
    let mut bar = floating_panel(style_bar_size_ex(Tool::Redact, layer_ops.is_some()), x, y);
    for area in RedactArea::ALL {
        let (label, icon) = match area {
            RedactArea::Brush => ("Brush: paint over what to hide", "icons/brush.svg"),
            RedactArea::Rect => ("Area: drag a rectangle to hide", "icons/area.svg"),
        };
        bar = bar.child(
            segment(label, icon, redaction.area == area)
                .on_mouse_down(MouseButton::Left, on_press(cx, move |this, _, _| this.editor_mut().set_redaction(|r| r.area = area))),
        );
    }
    bar = bar.child(separator());
    for effect in RedactEffect::ALL {
        let (label, icon) = match effect {
            RedactEffect::Mosaic => ("Mosaic", "icons/mosaic.svg"),
            RedactEffect::Blur => ("Blur", "icons/blur.svg"),
            RedactEffect::Gray => ("Gray block", "icons/gray.svg"),
        };
        bar = bar.child(
            segment(label, icon, redaction.effect == effect).on_mouse_down(
                MouseButton::Left,
                on_press(cx, move |this, _, _| this.editor_mut().set_redaction(|r| r.effect = effect)),
            ),
        );
    }
    bar = bar.child(separator()).child(slider(redaction, cx)).child(separator());
    bar = size_dots(bar, size, redaction.area == RedactArea::Brush, cx);
    {
        let running = auto == Auto::Running;
        let color: Hsla = if running { rgb(0x999999).into() } else { hsla(THEME_BLUE) };
        bar = bar.child(separator()).child(
            div()
                .id("auto")
                .tooltip(tooltip(if running {
                    "Finding sensitive text…"
                } else {
                    "Auto: find passwords, keys, contact details, IDs and card numbers with AI, and cover them"
                }))
                .flex_none()
                .w(px(AUTO))
                .h(px(STYLE_BUTTON))
                .flex()
                .items_center()
                .justify_center()
                .gap(px(4.))
                .rounded(px(4.))
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .when(running, |d| d.bg(rgba(0x0000_000A)))
                .when(!running, |d| {
                    d.cursor_pointer()
                        .hover(|s| s.bg(rgba(0x1677_FF1F)))
                        .on_mouse_down(MouseButton::Left, on_press(cx, |this, window, cx| this.auto_redact(window, cx)))
                })
                .child(svg().path("icons/auto.svg").size(px(14.)).text_color(color))
                .child(if running { "Finding…" } else { "Auto" }),
        );
    }
    if let Some(ops) = layer_ops {
        bar = append_layer_ops(bar, ops, true, cx);
    }
    bar.into_any_element()
}

fn slider(redaction: Redaction, cx: &mut Context<CanvasView>) -> Stateful<Div> {
    let enabled = redaction.effect.has_strength();
    let label = match redaction.effect {
        RedactEffect::Mosaic => "Mosaic block size",
        RedactEffect::Blur => "Blur strength",
        RedactEffect::Gray => "Gray covers fully",
    };
    let (fill, thumb): (Hsla, Hsla) =
        if enabled { (hsla(THEME_BLUE), hsla(THEME_BLUE)) } else { (rgb(0xD0D0D0).into(), rgb(0xC0C0C0).into()) };
    let track = SLIDER - 2.0 * SLIDER_INSET;
    let at = SLIDER_INSET + redaction.strength.clamp(0.0, 1.0) * track;
    let line = |left: f32, width: f32, color: Hsla| {
        div().absolute().left(px(left)).top(px((STYLE_BUTTON - 4.0) / 2.0)).w(px(width)).h(px(4.0)).rounded(px(2.0)).bg(color)
    };
    div()
        .id(label)
        .tooltip(tooltip(label))
        .relative()
        .flex_none()
        .w(px(SLIDER))
        .h(px(STYLE_BUTTON))
        .child(line(SLIDER_INSET, track, rgb(0xE3E3E3).into()))
        .child(line(SLIDER_INSET, at - SLIDER_INSET, fill))
        .child(
            div()
                .absolute()
                .left(px(at - THUMB / 2.0))
                .top(px((STYLE_BUTTON - THUMB) / 2.0))
                .size(px(THUMB))
                .rounded_full()
                .bg(white())
                .border_2()
                .border_color(thumb)
                .when(enabled, |d| d.shadow_sm()),
        )
        .when(enabled, |d| {
            d.cursor_pointer().on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.press_slider(ev.position.x, window);
                    cx.notify();
                }),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_size_counts_buttons_separators_and_gaps() {
        let frame = 2.0 * (BORDER + PAD);
        // CAPTURE_TOOLBAR: 8 tools | Undo | Scrolling, ToText, AddToEditor, Save, Cancel, Done = 15 buttons, 2 separators, 16 gaps.
        assert_eq!(panel_size(CAPTURE_TOOLBAR, 0.0), (frame + 15.0 * BUTTON + 2.0 * SEP + 16.0 * GAP, 42.0));
        // CAPTURE_FOR_EDITOR_TOOLBAR hides AddToEditor: one button + gap narrower.
        assert_eq!(panel_size(CAPTURE_TOOLBAR, 0.0).0 - panel_size(CAPTURE_FOR_EDITOR_TOOLBAR, 0.0).0, BUTTON + GAP);
        // EDITOR_TOOLBAR starts with AddImage on the far left.
        assert_eq!(EDITOR_TOOLBAR[0][0], Action::AddImage);
        assert_eq!(panel_size(SCROLLING_BAR, 200.0).0, frame + 4.0 * BUTTON + SEP + 200.0 + 5.0 * GAP);
        assert_eq!(panel_size(SCROLLING_FOR_EDITOR_BAR, 200.0).0, frame + 3.0 * BUTTON + SEP + 200.0 + 4.0 * GAP);
    }

    #[test]
    fn control_positions_fill_their_panels() {
        let check = |slots: Vec<Slot>, height: f32| {
            let (w, h) = row_size(&slots, height);
            let items = row_items(&slots, height, (10.0, 20.0));
            let last = items.last().unwrap().1;
            assert_eq!(last.0 + last.2 + BORDER + PAD, 10.0 + w);
            assert_eq!(last.1 + last.3 + BORDER + PAD, 20.0 + h);
        };
        for tool in Tool::ALL {
            check(style_slots(tool), STYLE_BUTTON);
            check(style_slots_ex(tool, true), STYLE_BUTTON);
        }
        check(image_bar_slots(), STYLE_BUTTON);
        check(toolbar_slots(CAPTURE_TOOLBAR, 0.0), BUTTON);
        check(toolbar_slots(EDITOR_TOOLBAR, 0.0), BUTTON);
        check(toolbar_slots(SCROLLING_BAR, 200.0), BUTTON);
    }

    #[test]
    fn style_bar_sizes_follow_the_tool() {
        let frame = 2.0 * (BORDER + PAD);
        assert_eq!(style_bar_size(Tool::Rect).0, frame + 9.0 * STYLE_BUTTON + SEP + 9.0 * GAP);
        assert_eq!(style_bar_size(Tool::Counter).0, frame + 13.0 * STYLE_BUTTON + 2.0 * SEP + 14.0 * GAP);
        let redact = frame + 8.0 * STYLE_BUTTON + 3.0 * SEP + SLIDER + 11.0 * GAP;
        assert_eq!(style_bar_size(Tool::Redact), (redact + SEP + AUTO + 2.0 * GAP, frame + STYLE_BUTTON));
    }

    #[test]
    fn slider_maps_its_track_to_strength() {
        let (lo, hi) = slider_ends(100.0);
        assert!(lo > 100.0 + 5.0 * STYLE_BUTTON && hi < 100.0 + style_bar_size(Tool::Redact).0);
        assert_eq!(hi - lo, SLIDER - 2.0 * SLIDER_INSET);
        assert_eq!(slider_value(100.0, lo), 0.0);
        assert_eq!(slider_value(100.0, (lo + hi) / 2.0), 0.5);
        assert_eq!(slider_value(100.0, lo - 50.0), 0.0);
        assert_eq!(slider_value(100.0, hi + 50.0), 1.0);
    }

    #[test]
    fn background_menu_dimensions_fit_palette_grid_and_controls() {
        let (w, h) = background_menu_size();
        assert_eq!(w, 194.0);
        assert_eq!(h, 135.0);
    }

    #[test]
    fn action_labels_match_toolbar_mode_and_platform() {
        assert_eq!(Action::Cancel.label_for(ToolbarMode::Capture), "Cancel (Esc)");
        assert_eq!(Action::Cancel.label_for(ToolbarMode::Editor), "Close");
        assert_eq!(Action::Done.label_for(ToolbarMode::Capture), "Copy (Enter)");
        assert_eq!(Action::Done.label_for(ToolbarMode::CaptureForEditor), "Add to editor (Enter)");
        assert_eq!(Action::Done.label_for(ToolbarMode::Editor), "Copy and close");
        let expected_mod = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+" };
        assert!(Action::OpenFile.label_for(ToolbarMode::Editor).contains(expected_mod));
        assert!(Action::Undo.label_for(ToolbarMode::Editor).contains(expected_mod));
        assert!(Action::Save.label_for(ToolbarMode::Editor).contains(expected_mod));
    }
}
