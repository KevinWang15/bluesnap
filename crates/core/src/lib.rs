//! Platform-independent core of bluesnap.
//!
//! Nothing in this crate talks to the OS or a UI toolkit: it holds the unified
//! editing and canvas engine ([`Editor`]), multi-image [`Composition`]s, the
//! `.bs` BSON file format ([`format`]), annotation shapes, layout helpers, and
//! the CPU rasterizer.

pub mod bson;
pub mod composition;
pub mod editor;
pub mod format;
pub mod geom;
pub mod ink;
pub mod layout;
pub mod outline;
pub mod pixels;
pub mod reading_order;
pub mod render;
pub mod shape;
pub mod stitch;
pub mod style;
pub mod text;

pub use composition::{Composition, Element, ImageAsset, ImageId, LayerOrder, PlacedImage};
pub use editor::{BrushRing, CANVAS_HANDLE_OFFSET, Editor, RightClick, SelectionMode, TextKey, VisualBand};
pub use geom::{CursorKind, Point, Rect};
pub use outline::WindowOutline;
pub use render::Document;
pub use style::{ArrowStyle, BACKGROUND_PALETTE, Color, CounterFormat, PALETTE, REDACT_GRAY, RedactArea, RedactEffect, Redaction, SizeLevel, Style, THEME_BLUE, TextStyle, Tool};
pub use text::{FontFamily, FontSet, FontSource, Fonts};
pub use tiny_skia::Pixmap;
