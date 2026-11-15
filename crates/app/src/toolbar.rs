//! Toolbars: the buttons every toolbar shares (one table of icons, labels and
//! colors), the editor's toolbar and style bar, and building blocks other
//! panels (the scrolling-capture bar) reuse.

use bluesnap_core::{Color, CounterFormat, PALETTE, RedactArea, RedactEffect, Redaction, SizeLevel, THEME_BLUE, Tool};
use gpui::{
    AnyElement, AnyView, App, AppContext, Context, Div, FontWeight, Hsla, InteractiveElement, IntoElement, MouseButton,
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
const STYLE_ICON: f32 = 16.0;
/// The panel's border, included in its size.
const BORDER: f32 = 1.0;
/// The redact strength slider: its width, the inset of the track's ends (room
/// for the thumb), and the thumb's diameter.
const SLIDER: f32 = 120.0;
const SLIDER_INSET: f32 = 9.0;
const THUMB: f32 = 14.0;
/// Width of the redact bar's Auto button (AI redaction).
const AUTO: f32 = 64.0;
/// Redact style bar buttons before the slider: area and effect segments.
const REDACT_SEGMENTS: usize = RedactArea::ALL.len() + RedactEffect::ALL.len();

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
            Action::Tool(Tool::Highlighter) => "icons/highlighter.svg",
            Action::Tool(Tool::Counter) => "icons/counter.svg",
            Action::Tool(Tool::Redact) => "icons/mosaic.svg",
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
            Action::Tool(Tool::Highlighter) => "Highlighter (Shift: straight line)",
            Action::Tool(Tool::Counter) => "Counter: click to place 1, 2, 3 …",
            Action::Tool(Tool::Redact) => "Redact",
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
    let width = 2.0 * (BORDER + PAD)
        + buttons as f32 * BUTTON
        + separators as f32 * SEP
        + extra
        + children.saturating_sub(1) as f32 * GAP;
    (width, 2.0 * (BORDER + PAD) + BUTTON)
}

/// Logical size of the style bar: 3 sizes, then (if the tool has a color) | 6
/// colors, then for counters | 4 numberings. The redact tool's bar is area |
/// effect | strength slider | 3 sizes, then `| Auto` when AI is set up.
pub fn style_bar_size(tool: Tool, auto: bool) -> (f32, f32) {
    let (buttons, seps, extra) = match tool {
        Tool::Redact if auto => (REDACT_SEGMENTS + 3, 4, SLIDER + AUTO),
        Tool::Redact => (REDACT_SEGMENTS + 3, 3, SLIDER),
        Tool::Counter => (3 + PALETTE.len() + CounterFormat::ALL.len(), 2, 0.0),
        _ if tool.uses_color() => (3 + PALETTE.len(), 1, 0.0),
        _ => (3, 0, 0.0),
    };
    let children = buttons + seps + usize::from(extra > 0.0) + usize::from(auto && tool == Tool::Redact);
    let width =
        2.0 * (BORDER + PAD) + buttons as f32 * STYLE_BUTTON + seps as f32 * SEP + extra + (children - 1) as f32 * GAP;
    (width, 2.0 * (BORDER + PAD) + STYLE_BUTTON)
}

/// Window x of the strength slider's 0 and 1 for a redact style bar at `bar_x`.
fn slider_ends(bar_x: f32) -> (f32, f32) {
    let before = REDACT_SEGMENTS as f32 * STYLE_BUTTON + 2.0 * SEP + (REDACT_SEGMENTS + 2) as f32 * GAP;
    let start = bar_x + BORDER + PAD + before + SLIDER_INSET;
    (start, start + SLIDER - 2.0 * SLIDER_INSET)
}

/// The strength for the pointer at window x `x` on a redact style bar at `bar_x`.
pub fn slider_value(bar_x: f32, x: f32) -> f32 {
    let (lo, hi) = slider_ends(bar_x);
    ((x - lo) / (hi - lo)).clamp(0.0, 1.0)
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
    div().flex_none().w(px(1.0)).h(px(20.0)).mx(px(4.0)).bg(rgba(0x0000_0022))
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

/// A small style bar button: the caller adds its content and what it does.
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

/// One choice of a segmented control: an icon, highlighted when chosen.
fn segment(label: &'static str, icon: &'static str, active: bool) -> Stateful<Div> {
    let color = if active { hsla(THEME_BLUE) } else { rgb(0x444444).into() };
    style_button(active)
        .id(label)
        .tooltip(tooltip(label))
        .cursor_pointer()
        .when(!active, |d| d.hover(|s| s.bg(rgba(0x0000_000F))))
        .child(svg().path(icon).size(px(STYLE_ICON)).text_color(color))
}

/// Brush size dots; grayed out and inert when `enabled` is false.
fn size_dots(mut bar: Div, size: SizeLevel, enabled: bool, cx: &mut Context<EditorView>) -> Div {
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

/// The style bar of a tool with a color. `numbering` is the counters' format,
/// shown for the counter tool.
pub fn style_bar(
    tool: Tool,
    size: SizeLevel,
    color: Color,
    numbering: Option<CounterFormat>,
    x: f32,
    y: f32,
    cx: &mut Context<EditorView>,
) -> AnyElement {
    let mut bar = size_dots(floating_panel(style_bar_size(tool, false), x, y), size, true, cx);
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
    if let Some(current) = numbering {
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
    bar.into_any_element()
}

/// The redact bar's Auto button: ready, or running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auto {
    Ready,
    Running,
}

/// The redact tool's style bar: brush or area | mosaic, blur or gray |
/// strength slider | brush size | Auto (AI redaction; it explains how to set it
/// up when it isn't). Controls that don't apply are grayed out rather than
/// hidden, so the bar keeps its layout.
pub fn redact_bar(
    redaction: Redaction,
    size: SizeLevel,
    auto: Auto,
    x: f32,
    y: f32,
    cx: &mut Context<EditorView>,
) -> AnyElement {
    let mut bar = floating_panel(style_bar_size(Tool::Redact, true), x, y);
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
    bar.into_any_element()
}

/// The strength slider. A press anywhere on it jumps the thumb there and
/// starts a drag, which the view follows window-wide.
fn slider(redaction: Redaction, cx: &mut Context<EditorView>) -> Stateful<Div> {
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
        // 14 buttons, 2 separators, 15 gaps between the 16 children.
        let frame = 2.0 * (BORDER + PAD);
        assert_eq!(panel_size(CAPTURE_TOOLBAR, 0.0), (frame + 14.0 * BUTTON + 2.0 * SEP + 15.0 * GAP, 42.0));
        // One button less without scrolling capture.
        assert_eq!(panel_size(CAPTURE_TOOLBAR, 0.0).0 - panel_size(DOCUMENT_TOOLBAR, 0.0).0, BUTTON + GAP);
        // Extra content (the scrolling bar's status) is a group of its own:
        // status | 4 buttons = 6 children.
        assert_eq!(panel_size(SCROLLING_BAR, 200.0).0, frame + 4.0 * BUTTON + SEP + 200.0 + 5.0 * GAP);
    }

    #[test]
    fn style_bar_sizes_follow_the_tool() {
        let frame = 2.0 * (BORDER + PAD);
        // 3 sizes | 6 colors: 9 buttons, a separator, 9 gaps.
        assert_eq!(style_bar_size(Tool::Rect, false).0, frame + 9.0 * STYLE_BUTTON + SEP + 9.0 * GAP);
        // Counters add | 4 numberings: 13 buttons, 2 separators, 14 gaps.
        assert_eq!(style_bar_size(Tool::Counter, false).0, frame + 13.0 * STYLE_BUTTON + 2.0 * SEP + 14.0 * GAP);
        // 2 areas | 3 effects | slider | 3 sizes: 8 buttons, 3 separators and
        // the slider, so 11 gaps.
        let redact = frame + 8.0 * STYLE_BUTTON + 3.0 * SEP + SLIDER + 11.0 * GAP;
        assert_eq!(style_bar_size(Tool::Redact, false), (redact, frame + STYLE_BUTTON));
        // With AI set up: | Auto, one more separator and button, two more gaps.
        assert_eq!(style_bar_size(Tool::Redact, true).0, redact + SEP + AUTO + 2.0 * GAP);
    }

    #[test]
    fn slider_maps_its_track_to_strength() {
        let (lo, hi) = slider_ends(100.0);
        // The slider sits after the segments and two separators, inside the bar.
        assert!(lo > 100.0 + 5.0 * STYLE_BUTTON && hi < 100.0 + style_bar_size(Tool::Redact, false).0);
        assert_eq!(hi - lo, SLIDER - 2.0 * SLIDER_INSET);
        assert_eq!(slider_value(100.0, lo), 0.0);
        assert_eq!(slider_value(100.0, (lo + hi) / 2.0), 0.5);
        // Past either end (a drag off the track) clamps.
        assert_eq!(slider_value(100.0, lo - 50.0), 0.0);
        assert_eq!(slider_value(100.0, hi + 50.0), 1.0);
    }
}
