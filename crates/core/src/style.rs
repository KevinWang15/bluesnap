//! Tools, colors and stroke sizes shared by the editor, the rasterizer and the UI.

use crate::text::FontFamily;

/// Straight (non-premultiplied) sRGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Self = Self { r: 0, g: 0, b: 0, a: 0 };

    pub const fn rgb(hex: u32) -> Self {
        Self { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8, a: 255 }
    }

    pub const fn is_transparent(self) -> bool {
        self.a == 0
    }

    pub const fn to_hex(self) -> u32 {
        (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
    }

    /// Whether dark text reads better on this color than white text.
    pub fn is_light(self) -> bool {
        // Relative luminance with sRGB weights, on the gamma-encoded values.
        0.299 * self.r as f32 + 0.587 * self.g as f32 + 0.114 * self.b as f32 > 160.0
    }

    /// Text on this color: dark on light colors, white otherwise.
    pub fn contrast(self) -> Color {
        if self.is_light() { COUNTER_DARK_TEXT } else { Color::rgb(0xFFFFFF) }
    }
}

/// Theme color of the whole tool: selection borders, handles, active tool.
pub const THEME_BLUE: Color = Color::rgb(0x1677FF);

/// Colors offered in the style bar. The first one is the default.
pub const PALETTE: [Color; 6] = [
    Color::rgb(0xF5222D), // red
    Color::rgb(0xFAAD14), // yellow
    Color::rgb(0x52C41A), // green
    Color::rgb(0x1677FF), // blue
    Color::rgb(0x000000), // black
    Color::rgb(0xFFFFFF), // white
];

/// Preset canvas background colors offered in the Editor's right-click context menu.
pub const BACKGROUND_PALETTE: [(Color, &str); 12] = [
    (Color::rgb(0xFFFFFF), "White (#FFFFFF)"),
    (Color::rgb(0xF1F5F9), "Light gray (#F1F5F9)"),
    (Color::rgb(0xCBD5E1), "Gray (#CBD5E1)"),
    (Color::rgb(0x334155), "Slate (#334155)"),
    (Color::rgb(0x191919), "Dark (#191919)"),
    (Color::rgb(0x000000), "Black (#000000)"),
    (Color::TRANSPARENT, "Transparent"),
    (Color::rgb(0xDBEAFE), "Soft blue (#DBEAFE)"),
    (Color::rgb(0xDCFCE7), "Soft green (#DCFCE7)"),
    (Color::rgb(0xFEF3C7), "Soft amber (#FEF3C7)"),
    (Color::rgb(0xFFE4E6), "Soft rose (#FFE4E6)"),
    (Color::rgb(0x1677FF), "Blue (#1677FF)"),
];

/// Opacity of highlighter strokes.
pub const HIGHLIGHT_ALPHA: u8 = 115; // 45%

/// Text color of counters on light colors (white text elsewhere).
pub const COUNTER_DARK_TEXT: Color = Color::rgb(0x1F1F1F);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    Rect,
    Ellipse,
    Arrow,
    Pen,
    /// Translucent freehand strokes, like a marker over paper.
    Highlighter,
    /// Numbered markers placed with a click: 1, 2, 3, …
    Counter,
    Redact,
    Text,
}

impl Tool {
    pub const ALL: [Tool; 8] = [
        Tool::Rect,
        Tool::Ellipse,
        Tool::Arrow,
        Tool::Pen,
        Tool::Highlighter,
        Tool::Counter,
        Tool::Redact,
        Tool::Text,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    /// Whether the tool's color can be changed (redaction has effects instead).
    pub fn uses_color(self) -> bool {
        self != Tool::Redact
    }

    /// Style of the tool's first shapes in a session.
    pub fn default_style(self) -> Style {
        match self {
            Tool::Highlighter => Style { color: PALETTE[1], ..Style::default() }, // yellow
            _ => Style::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SizeLevel {
    Small,
    #[default]
    Medium,
    Large,
}

impl SizeLevel {
    pub const ALL: [SizeLevel; 3] = [SizeLevel::Small, SizeLevel::Medium, SizeLevel::Large];

    /// Stroke width, brush width or font size in logical pixels for `tool`.
    /// Multiply by the display scale factor to get screenshot pixels.
    pub fn logical_px(self, tool: Tool) -> f32 {
        let table: [f32; 3] = match tool {
            Tool::Rect | Tool::Ellipse | Tool::Arrow | Tool::Pen => [2.0, 4.0, 6.0],
            Tool::Highlighter => [10.0, 18.0, 30.0],
            // Marker diameter.
            Tool::Counter => [20.0, 28.0, 40.0],
            Tool::Redact => [12.0, 24.0, 40.0],
            Tool::Text => [16.0, 24.0, 36.0],
        };
        table[self as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub color: Color,
    pub size: SizeLevel,
}

impl Default for Style {
    fn default() -> Self {
        Self { color: PALETTE[0], size: SizeLevel::Medium }
    }
}

/// How a text annotation looks: a typeface family and what's behind the
/// glyphs. The seven styles CleanShot X offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextStyle {
    #[default]
    Standard,
    Rounded,
    Monospaced,
    /// Outlined in a contrasting color, readable on any background.
    Outlined,
    /// On a box of the text color, in a contrasting color.
    Boxed,
    /// On a pill of the text color, in the rounded face.
    RoundedBoxed,
    /// Monospaced on a light card with a border, like a code chip.
    MonospacedBoxed,
}

/// What a [`TextStyle`] draws behind its glyphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextBackdrop {
    None,
    Outline,
    Box,
    Pill,
    Card,
}

impl TextStyle {
    pub const ALL: [TextStyle; 7] = [
        TextStyle::Standard,
        TextStyle::Rounded,
        TextStyle::Monospaced,
        TextStyle::Outlined,
        TextStyle::Boxed,
        TextStyle::RoundedBoxed,
        TextStyle::MonospacedBoxed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TextStyle::Standard => "Standard",
            TextStyle::Rounded => "Rounded",
            TextStyle::Monospaced => "Monospaced",
            TextStyle::Outlined => "Outlined",
            TextStyle::Boxed => "Boxed",
            TextStyle::RoundedBoxed => "Rounded Boxed",
            TextStyle::MonospacedBoxed => "Monospaced Boxed",
        }
    }

    pub fn family(self) -> FontFamily {
        match self {
            TextStyle::Rounded | TextStyle::RoundedBoxed => FontFamily::Rounded,
            TextStyle::Monospaced | TextStyle::MonospacedBoxed => FontFamily::Mono,
            _ => FontFamily::Sans,
        }
    }

    pub fn backdrop(self) -> TextBackdrop {
        match self {
            TextStyle::Outlined => TextBackdrop::Outline,
            TextStyle::Boxed => TextBackdrop::Box,
            TextStyle::RoundedBoxed => TextBackdrop::Pill,
            TextStyle::MonospacedBoxed => TextBackdrop::Card,
            _ => TextBackdrop::None,
        }
    }

    /// The glyphs' color for text in `color`: on a box or pill of that
    /// color, the contrasting one.
    pub fn ink(self, color: Color) -> Color {
        match self.backdrop() {
            TextBackdrop::Box | TextBackdrop::Pill => color.contrast(),
            _ => color,
        }
    }
}

/// The line an arrow takes from its start to its tip, named like the
/// connectors of diagramming tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum ArrowStyle {
    #[default]
    Straight,
    /// Horizontal and vertical segments with rounded corners (an elbow or
    /// orthogonal connector): usually three, like right, down, right.
    Elbow,
    /// An S curve, leaving the start and reaching the tip along the same axis.
    Curved,
}

impl ArrowStyle {
    pub const ALL: [ArrowStyle; 3] = [ArrowStyle::Straight, ArrowStyle::Elbow, ArrowStyle::Curved];
}

/// How counters are numbered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CounterFormat {
    /// 1, 2, 3, 4 …
    #[default]
    Decimal,
    /// A, B, C, D … Z, AA, AB …
    UpperAlpha,
    /// I, II, III, IV …
    Roman,
    /// a, b, c, d …
    LowerAlpha,
}

impl CounterFormat {
    pub const ALL: [CounterFormat; 4] =
        [CounterFormat::Decimal, CounterFormat::UpperAlpha, CounterFormat::Roman, CounterFormat::LowerAlpha];

    /// The label of counter `n` (1-based).
    pub fn label(self, n: u32) -> String {
        let n = n.max(1);
        match self {
            CounterFormat::Decimal => n.to_string(),
            CounterFormat::UpperAlpha => alpha(n, b'A'),
            CounterFormat::LowerAlpha => alpha(n, b'a'),
            // Beyond 3999 Roman numerals need overlines; fall back to digits.
            CounterFormat::Roman if n >= 4000 => n.to_string(),
            CounterFormat::Roman => roman(n),
        }
    }
}

/// Spreadsheet-style column letters: 1 → A, 26 → Z, 27 → AA.
fn alpha(mut n: u32, a: u8) -> String {
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push(a + (n % 26) as u8);
        n /= 26;
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII letters")
}

fn roman(mut n: u32) -> String {
    const TABLE: [(u32, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for (value, digits) in TABLE {
        while n >= value {
            out.push_str(digits);
            n -= value;
        }
    }
    out
}

/// How a redaction hides what's under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum RedactEffect {
    #[default]
    Mosaic,
    Blur,
    /// An opaque gray block.
    Gray,
}

impl RedactEffect {
    pub const ALL: [RedactEffect; 3] = [RedactEffect::Mosaic, RedactEffect::Blur, RedactEffect::Gray];

    /// Whether the strength setting changes anything.
    pub fn has_strength(self) -> bool {
        self != RedactEffect::Gray
    }
}

/// The fill of [`RedactEffect::Gray`].
pub const REDACT_GRAY: Color = Color::rgb(0x8C8C8C);

/// What the redact tool draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RedactArea {
    /// Freehand strokes, as wide as the brush size.
    #[default]
    Brush,
    /// A rectangle dragged out like the rectangle tool.
    Rect,
}

impl RedactArea {
    pub const ALL: [RedactArea; 2] = [RedactArea::Brush, RedactArea::Rect];
}

/// The redact tool's settings, and a redaction shape's look.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Redaction {
    pub effect: RedactEffect,
    pub area: RedactArea,
    /// Slider position, 0..=1: mosaic block size or blur radius.
    pub strength: f32,
}

impl Default for Redaction {
    fn default() -> Self {
        Self { effect: RedactEffect::default(), area: RedactArea::default(), strength: 0.35 }
    }
}

/// Mosaic block size and blur radius (sigma) for strength 0 and 1, in logical px.
const MOSAIC_RANGE: (f32, f32) = (4.0, 40.0);
const BLUR_RANGE: (f32, f32) = (2.0, 24.0);

impl RedactEffect {
    /// Block size (mosaic) or blur sigma for `strength`, in screenshot pixels,
    /// quantized to whole pixels (block) or half pixels (sigma, as 2 × sigma)
    /// so nearby slider positions share cached results.
    pub fn amount(self, strength: f32, scale: f32) -> u32 {
        let s = strength.clamp(0.0, 1.0);
        let lerp = |(lo, hi): (f32, f32)| (lo + (hi - lo) * s) * scale;
        match self {
            RedactEffect::Mosaic => lerp(MOSAIC_RANGE).round().max(2.0) as u32,
            RedactEffect::Blur => (lerp(BLUR_RANGE) * 2.0).round().max(1.0) as u32,
            RedactEffect::Gray => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_labels_in_each_format() {
        let labels = |f: CounterFormat, ns: &[u32]| ns.iter().map(|n| f.label(*n)).collect::<Vec<_>>();
        assert_eq!(labels(CounterFormat::Decimal, &[1, 2, 10]), ["1", "2", "10"]);
        assert_eq!(labels(CounterFormat::UpperAlpha, &[1, 26, 27, 52, 703]), ["A", "Z", "AA", "AZ", "AAA"]);
        assert_eq!(labels(CounterFormat::LowerAlpha, &[3, 28]), ["c", "ab"]);
        assert_eq!(labels(CounterFormat::Roman, &[1, 4, 9, 14, 40, 1994]), ["I", "IV", "IX", "XIV", "XL", "MCMXCIV"]);
    }

    #[test]
    fn light_colors_get_dark_text() {
        assert!(PALETTE[5].is_light()); // white
        assert!(PALETTE[1].is_light()); // yellow
        assert!(!PALETTE[0].is_light()); // red
        assert!(!PALETTE[3].is_light()); // blue
    }

    #[test]
    fn strength_maps_to_effect_amounts() {
        assert_eq!(RedactEffect::Mosaic.amount(0.0, 1.0), 4);
        assert_eq!(RedactEffect::Mosaic.amount(1.0, 2.0), 80);
        // Blur amounts are 2 × sigma.
        assert_eq!(RedactEffect::Blur.amount(0.0, 1.0), 4);
        assert_eq!(RedactEffect::Blur.amount(1.0, 1.0), 48);
        assert_eq!(RedactEffect::Gray.amount(0.5, 1.0), 0);
        // Out-of-range strengths clamp.
        assert_eq!(RedactEffect::Mosaic.amount(3.0, 1.0), 40);
    }
}
