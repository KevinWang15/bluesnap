//! Platform-independent core of bluesnap.
//!
//! Nothing in this crate talks to the OS or a UI toolkit: it holds the capture
//! session state machine ([`Editor`]), annotation shapes, layout of the floating
//! panels and a CPU rasterizer used for both the live preview and the export.

pub mod editor;
pub mod geom;
pub mod ink;
pub mod layout;
pub mod pixels;
pub mod reading_order;
pub mod render;
pub mod shape;
pub mod stitch;
pub mod style;
pub mod text;

pub use editor::{BrushRing, Editor, RightClick, SelectionMode, TextKey};
pub use geom::{CursorKind, Point, Rect};
pub use render::Document;
pub use style::{Color, CounterFormat, PALETTE, TextStyle, REDACT_GRAY, RedactArea, RedactEffect, Redaction, SizeLevel, Style, THEME_BLUE, Tool};
pub use text::{FontFamily, FontSet, FontSource, Fonts};
pub use tiny_skia::Pixmap;
