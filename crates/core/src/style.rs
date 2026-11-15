//! Tools, colors and stroke sizes shared by the editor, the rasterizer and the UI.

/// Straight (non-premultiplied) sRGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(hex: u32) -> Self {
        Self { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8, a: 255 }
    }

    pub const fn to_hex(self) -> u32 {
        (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    Rect,
    Ellipse,
    Arrow,
    Pen,
    Redact,
    Text,
}

impl Tool {
    pub const ALL: [Tool; 6] = [Tool::Rect, Tool::Ellipse, Tool::Arrow, Tool::Pen, Tool::Redact, Tool::Text];

    pub fn index(self) -> usize {
        self as usize
    }

    /// Whether the tool's color can be changed (redaction has effects instead).
    pub fn uses_color(self) -> bool {
        self != Tool::Redact
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
