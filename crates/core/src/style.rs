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
    Mosaic,
    Text,
}

impl Tool {
    pub const ALL: [Tool; 6] = [Tool::Rect, Tool::Ellipse, Tool::Arrow, Tool::Pen, Tool::Mosaic, Tool::Text];

    pub fn index(self) -> usize {
        self as usize
    }

    /// Whether the tool's color can be changed (mosaic always shows the image).
    pub fn uses_color(self) -> bool {
        self != Tool::Mosaic
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
            Tool::Mosaic => [12.0, 24.0, 40.0],
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
