//! `.bs` file format: BSON-encoded editable BlueSnap compositions containing
//! canvas dimensions, raw lossless image binary data, and all scene elements.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use crate::bson::{self, Doc, Value};
use crate::composition::{Composition, Element, ImageAsset, ImageId, PlacedImage};
use crate::geom::{Point, Rect};
use crate::pixels::{par_map, pixmap_from_rgba, pixmap_from_straight_rgba, to_rgba_straight};
use crate::shape::{Geometry, RedactShape, Shape};
use crate::style::{ArrowStyle, Color, CounterFormat, RedactEffect, SizeLevel, Style, TextStyle};

const FORMAT_MAGIC: &str = "bluesnap";
const FORMAT_VERSION: i32 = 1;
const MAX_CANVAS_SIDE: u32 = 65_535;
const MAX_IMAGE_PIXELS: u64 = 256 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = (MAX_IMAGE_PIXELS as usize) * 4;
const MAX_ABS_COORD: f32 = 1_000_000.0;

/// Dimensions that an editable document can store and reopen.
pub fn valid_canvas_size(width: u32, height: u32) -> bool {
    (1..=MAX_CANVAS_SIDE).contains(&width) && (1..=MAX_CANVAS_SIDE).contains(&height)
        && u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS
}

#[derive(Debug)]
pub enum FormatError {
    Bson(bson::Error),
    Invalid(String),
    Io(std::io::Error),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Bson(e) => write!(f, "invalid .bs BSON structure: {e}"),
            FormatError::Invalid(s) => write!(f, "invalid .bs file: {s}"),
            FormatError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FormatError {}

impl From<bson::Error> for FormatError {
    fn from(e: bson::Error) -> Self {
        FormatError::Bson(e)
    }
}

impl From<std::io::Error> for FormatError {
    fn from(e: std::io::Error) -> Self {
        FormatError::Io(e)
    }
}

fn bad(msg: impl Into<String>) -> FormatError {
    FormatError::Invalid(msg.into())
}

/// Encodes `comp` into `.bs` BSON bytes.
pub fn encode_bs(comp: &Composition) -> Result<Vec<u8>, FormatError> {
    let mut seen: HashMap<ImageId, ()> = HashMap::new();
    let mut unique_assets: Vec<&ImageAsset> = Vec::new();
    for el in &comp.elements {
        if let Element::Image(placed) = el
            && seen.insert(placed.asset.id, ()).is_none()
        {
            unique_assets.push(&placed.asset);
        }
    }
    let images: Vec<Value> = par_map(&unique_assets, |asset| encode_image_asset(asset))
        .into_iter()
        .map(|res| res.map(Value::Document))
        .collect::<Result<_, _>>()?;

    let elements: Vec<Value> = comp.elements.iter().map(|el| Value::Document(encode_element(el))).collect();

    let mut root = Doc::with_capacity(8);
    root.push("format", Value::String(FORMAT_MAGIC.into()));
    root.push("version", Value::Int32(FORMAT_VERSION));
    root.push("width", Value::Int32(comp.width as i32));
    root.push("height", Value::Int32(comp.height as i32));
    root.push("scale", Value::Double(comp.scale as f64));
    root.push("background", Value::Document(encode_color(comp.background)));
    root.push("images", Value::Array(images));
    root.push("elements", Value::Array(elements));
    Ok(bson::encode(&root)?)
}

/// Decodes a [`Composition`] from `.bs` BSON bytes.
pub fn decode_bs(bytes: &[u8]) -> Result<Composition, FormatError> {
    let root = bson::decode(bytes)?;
    let magic = root.req_str("format")?;
    if magic != FORMAT_MAGIC {
        return Err(bad(format!("expected format `{FORMAT_MAGIC}`, got `{magic}`")));
    }
    let version = root.req_i32("version")?;
    if version != FORMAT_VERSION {
        return Err(bad(format!("unsupported .bs format version {version}")));
    }
    let width = u32::try_from(root.req_i32("width")?)
        .ok()
        .filter(|&w| (1..=MAX_CANVAS_SIDE).contains(&w))
        .ok_or_else(|| bad("invalid canvas width"))?;
    let height = u32::try_from(root.req_i32("height")?)
        .ok()
        .filter(|&h| (1..=MAX_CANVAS_SIDE).contains(&h))
        .ok_or_else(|| bad("invalid canvas height"))?;
    if !valid_canvas_size(width, height) {
        return Err(bad("canvas dimensions exceed maximum pixel budget"));
    }
    let scale = root.req_f32("scale")?;
    if !(0.1..=16.0).contains(&scale) {
        return Err(bad("invalid canvas scale"));
    }
    let background = root.get("background").and_then(Value::as_doc).map(decode_color).transpose()?.unwrap_or(Color::rgb(0xFFFFFF));

    let img_vals = root.req_array("images")?;
    let img_docs: Vec<&Doc> = img_vals
        .iter()
        .map(|val| val.as_doc().ok_or_else(|| bad("image entry is not a BSON document")))
        .collect::<Result<_, _>>()?;
    let decoded_assets: Vec<ImageAsset> =
        par_map(&img_docs, |doc| decode_image_asset(doc)).into_iter().collect::<Result<_, _>>()?;

    let mut assets: HashMap<ImageId, ImageAsset> = HashMap::with_capacity(decoded_assets.len());
    for asset in decoded_assets {
        assets.insert(asset.id, asset);
    }

    let elem_vals = root.req_array("elements")?;
    let mut elements = Vec::with_capacity(elem_vals.len());
    for val in elem_vals {
        let doc = val.as_doc().ok_or_else(|| bad("element entry is not a BSON document"))?;
        elements.push(decode_element(doc, &assets)?);
    }

    Ok(Composition { width, height, scale, background, elements })
}

/// Atomically writes `comp` to `path` in `.bs` BSON format.
pub fn save_bs(comp: &Composition, path: &Path) -> Result<(), FormatError> {
    let bytes = encode_bs(comp)?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    write_private_or_normal(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Reads a [`Composition`] from a `.bs` file on disk.
pub fn load_bs(path: &Path) -> Result<Composition, FormatError> {
    let bytes = std::fs::read(path)?;
    decode_bs(&bytes)
}

fn write_private_or_normal(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(bytes)?;
        f.sync_all()
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

fn encode_image_asset(asset: &ImageAsset) -> Result<Doc, FormatError> {
    let png_bytes = if let Some(cached) = asset.cached_png() {
        cached.to_vec()
    } else {
        let rgba = to_rgba_straight(asset.pixmap());
        let mut out = Vec::with_capacity(rgba.len() / 4);
        {
            let mut encoder = png::Encoder::new(&mut out, asset.width(), asset.height());
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Fast);
            let mut writer = encoder.write_header().map_err(|e| bad(format!("PNG encode header: {e}")))?;
            writer.write_image_data(&rgba).map_err(|e| bad(format!("PNG encode data: {e}")))?;
        }
        asset.cache_png(out.clone());
        out
    };
    let mut d = Doc::with_capacity(5);
    d.push("id", Value::Int64(asset.id.0 as i64));
    d.push("width", Value::Int32(asset.width() as i32));
    d.push("height", Value::Int32(asset.height() as i32));
    d.push("encoding", Value::String("png".into()));
    d.push("data", Value::Binary(png_bytes));
    Ok(d)
}

fn decode_image_asset(doc: &Doc) -> Result<ImageAsset, FormatError> {
    let id = u64::try_from(doc.req_i64("id")?).map_err(|_| bad("negative image id"))?;
    decode_image_pixels(doc, ImageId(id))
}

fn decode_image_pixels(doc: &Doc, id: ImageId) -> Result<ImageAsset, FormatError> {
    let width = u32::try_from(doc.req_i32("width")?)
        .ok()
        .filter(|&w| (1..=MAX_CANVAS_SIDE).contains(&w))
        .ok_or_else(|| bad("invalid image width"))?;
    let height = u32::try_from(doc.req_i32("height")?)
        .ok()
        .filter(|&h| (1..=MAX_CANVAS_SIDE).contains(&h))
        .ok_or_else(|| bad("invalid image height"))?;
    if (width as u64) * (height as u64) > MAX_IMAGE_PIXELS {
        return Err(bad("image dimensions exceed maximum pixel budget"));
    }
    let encoding = doc.req_str("encoding")?;
    let raw = doc.req_bytes("data")?;

    match encoding {
        "png" => {
            let rgba = decode_png_rgba(raw, width, height)?;
            let pixmap = pixmap_from_straight_rgba(width, height, rgba).ok_or_else(|| bad("failed to build image pixmap"))?;
            Ok(ImageAsset::with_id_and_png(id, pixmap, raw.to_vec()))
        }
        "rgba" => {
            let expected = width as usize * height as usize * 4;
            if raw.len() != expected {
                return Err(bad("raw RGBA byte length mismatch"));
            }
            let pixmap = pixmap_from_rgba(width, height, raw.to_vec()).ok_or_else(|| bad("failed to build image pixmap"))?;
            Ok(ImageAsset::with_id(id, pixmap))
        }
        other => Err(bad(format!("unsupported image encoding `{other}`"))),
    }
}

fn decode_png_rgba(bytes: &[u8], expected_w: u32, expected_h: u32) -> Result<Vec<u8>, FormatError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().map_err(|e| bad(format!("PNG decode header: {e}")))?;
    if reader.info().width != expected_w || reader.info().height != expected_h {
        return Err(bad("PNG dimensions do not match asset header"));
    }
    let buf_size = reader
        .output_buffer_size()
        .filter(|&n| n <= MAX_IMAGE_BYTES)
        .ok_or_else(|| bad("PNG buffer too large"))?;
    let mut buf = vec![0u8; buf_size];
    let info = reader.next_frame(&mut buf).map_err(|e| bad(format!("PNG decode pixels: {e}")))?;
    if info.width != expected_w || info.height != expected_h {
        return Err(bad("PNG frame dimensions do not match asset header"));
    }
    buf.truncate(info.buffer_size());
    match info.color_type {
        png::ColorType::Rgba => Ok(buf),
        png::ColorType::Rgb => Ok(buf.as_chunks::<3>().0.iter().flat_map(|&[r, g, b]| [r, g, b, 255]).collect()),
        png::ColorType::GrayscaleAlpha => Ok(buf.as_chunks::<2>().0.iter().flat_map(|&[g, a]| [g, g, g, a]).collect()),
        png::ColorType::Grayscale => Ok(buf.into_iter().flat_map(|g| [g, g, g, 255]).collect()),
        other => Err(bad(format!("unsupported PNG color type {other:?}"))),
    }
}

fn encode_color(c: Color) -> Doc {
    let mut d = Doc::with_capacity(4);
    d.push("r", Value::Int32(c.r as i32));
    d.push("g", Value::Int32(c.g as i32));
    d.push("b", Value::Int32(c.b as i32));
    d.push("a", Value::Int32(c.a as i32));
    d
}

fn decode_color(d: &Doc) -> Result<Color, FormatError> {
    let byte = |k| -> Result<u8, FormatError> {
        u8::try_from(d.req_i32(k)?).map_err(|_| bad(format!("color channel `{k}` out of range")))
    };
    Ok(Color { r: byte("r")?, g: byte("g")?, b: byte("b")?, a: byte("a")? })
}

fn check_coord(v: f32, label: &str) -> Result<f32, FormatError> {
    if (-MAX_ABS_COORD..=MAX_ABS_COORD).contains(&v) {
        Ok(v)
    } else {
        Err(bad(format!("coordinate `{label}` out of valid range")))
    }
}

fn encode_point(p: Point) -> Doc {
    let mut d = Doc::with_capacity(2);
    d.push("x", Value::Double(p.x as f64));
    d.push("y", Value::Double(p.y as f64));
    d
}

fn decode_point(d: &Doc) -> Result<Point, FormatError> {
    Ok(Point::new(check_coord(d.req_f32("x")?, "x")?, check_coord(d.req_f32("y")?, "y")?))
}

fn encode_rect(r: Rect) -> Doc {
    let mut d = Doc::with_capacity(4);
    d.push("x", Value::Double(r.x as f64));
    d.push("y", Value::Double(r.y as f64));
    d.push("w", Value::Double(r.w as f64));
    d.push("h", Value::Double(r.h as f64));
    d
}

fn decode_rect(d: &Doc) -> Result<Rect, FormatError> {
    let x = check_coord(d.req_f32("x")?, "x")?;
    let y = check_coord(d.req_f32("y")?, "y")?;
    let w = check_coord(d.req_f32("w")?, "w")?;
    let h = check_coord(d.req_f32("h")?, "h")?;
    if w < 0.0 || h < 0.0 {
        return Err(bad("rectangle width and height must be non-negative"));
    }
    Ok(Rect::new(x, y, w, h))
}

fn encode_points_bin(pts: &[Point]) -> Value {
    let mut bytes = Vec::with_capacity(pts.len() * 8);
    for p in pts {
        bytes.extend_from_slice(&p.x.to_le_bytes());
        bytes.extend_from_slice(&p.y.to_le_bytes());
    }
    Value::Binary(bytes)
}

fn decode_shape_points(d: &Doc) -> Result<Vec<Point>, FormatError> {
    let pts = if let Some(bin_val) = d.get("points_bin") {
        let bytes = bin_val.as_bytes().ok_or_else(|| bad("`points_bin` is not binary"))?;
        if !bytes.len().is_multiple_of(8) {
            return Err(bad("`points_bin` byte length must be a multiple of 8"));
        }
        let mut pts = Vec::with_capacity(bytes.len() / 8);
        for chunk in bytes.as_chunks::<8>().0 {
            let x = check_coord(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]), "x")?;
            let y = check_coord(f32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]), "y")?;
            pts.push(Point::new(x, y));
        }
        pts
    } else {
        decode_points(d.req_array("points")?)?
    };
    if pts.is_empty() {
        return Err(bad("stroke must contain at least one point"));
    }
    Ok(pts)
}

fn decode_points(vals: &[Value]) -> Result<Vec<Point>, FormatError> {
    vals.iter()
        .map(|v| decode_point(v.as_doc().ok_or_else(|| bad("point entry is not a document"))?))
        .collect()
}

fn encode_size(s: SizeLevel) -> &'static str {
    match s {
        SizeLevel::Small => "small",
        SizeLevel::Medium => "medium",
        SizeLevel::Large => "large",
    }
}

fn decode_size(s: &str) -> Result<SizeLevel, FormatError> {
    match s {
        "small" => Ok(SizeLevel::Small),
        "medium" => Ok(SizeLevel::Medium),
        "large" => Ok(SizeLevel::Large),
        other => Err(bad(format!("unknown size `{other}`"))),
    }
}

fn encode_element(el: &Element) -> Doc {
    let mut d = Doc::with_capacity(6);
    match el {
        Element::Image(placed) => {
            d.push("kind", Value::String("image".into()));
            d.push("image_id", Value::Int64(placed.asset.id.0 as i64));
            d.push("bounds", Value::Document(encode_rect(placed.bounds)));
        }
        Element::Shape(shape) => {
            encode_shape(&mut d, shape);
        }
    }
    d
}

fn encode_shape(d: &mut Doc, shape: &Shape) {
    match &shape.geometry {
        Geometry::Rect(r) => {
            d.push("kind", Value::String("rect".into()));
            d.push("bounds", Value::Document(encode_rect(*r)));
        }
        Geometry::Ellipse(r) => {
            d.push("kind", Value::String("ellipse".into()));
            d.push("bounds", Value::Document(encode_rect(*r)));
        }
        Geometry::Arrow { from, to, style } => {
            d.push("kind", Value::String("arrow".into()));
            d.push("from", Value::Document(encode_point(*from)));
            d.push("to", Value::Document(encode_point(*to)));
            let s = match style {
                ArrowStyle::Straight => "straight",
                ArrowStyle::Elbow => "elbow",
                ArrowStyle::Curved => "curved",
            };
            d.push("arrow_style", Value::String(s.into()));
        }
        Geometry::Pen(pts) => {
            d.push("kind", Value::String("pen".into()));
            d.push("points_bin", encode_points_bin(pts));
        }
        Geometry::Highlight(pts) => {
            d.push("kind", Value::String("highlight".into()));
            d.push("points_bin", encode_points_bin(pts));
        }
        Geometry::Counter { center, number, format } => {
            d.push("kind", Value::String("counter".into()));
            d.push("center", Value::Document(encode_point(*center)));
            d.push("number", Value::Int32(*number as i32));
            let fmt = match format {
                CounterFormat::Decimal => "decimal",
                CounterFormat::UpperAlpha => "upper_alpha",
                CounterFormat::Roman => "roman",
                CounterFormat::LowerAlpha => "lower_alpha",
            };
            d.push("format", Value::String(fmt.into()));
        }
        Geometry::Redact { area, effect, strength } => {
            d.push("kind", Value::String("redact".into()));
            let eff = match effect {
                RedactEffect::Mosaic => "mosaic",
                RedactEffect::Blur => "blur",
                RedactEffect::Gray => "gray",
            };
            d.push("effect", Value::String(eff.into()));
            d.push("strength", Value::Double(*strength as f64));
            match area {
                RedactShape::Brush(pts) => {
                    d.push("area_kind", Value::String("brush".into()));
                    d.push("points_bin", encode_points_bin(pts));
                }
                RedactShape::Rect(r) => {
                    d.push("area_kind", Value::String("rect".into()));
                    d.push("bounds", Value::Document(encode_rect(*r)));
                }
            }
        }
        Geometry::Text { origin, content, style } => {
            d.push("kind", Value::String("text".into()));
            d.push("origin", Value::Document(encode_point(*origin)));
            d.push("content", Value::String(content.clone()));
            let ts = match style {
                TextStyle::Standard => "standard",
                TextStyle::Rounded => "rounded",
                TextStyle::Monospaced => "monospaced",
                TextStyle::Outlined => "outlined",
                TextStyle::Boxed => "boxed",
                TextStyle::RoundedBoxed => "rounded_boxed",
                TextStyle::MonospacedBoxed => "monospaced_boxed",
            };
            d.push("text_style", Value::String(ts.into()));
        }
    }
    d.push("color", Value::Document(encode_color(shape.style.color)));
    d.push("size", Value::String(encode_size(shape.style.size).into()));
}

/// Encodes one editable item independently of its source document.
pub fn encode_clipboard_element(element: &Element, scale: f32) -> Result<Vec<u8>, FormatError> {
    let mut doc = encode_element(element);
    if let Element::Image(image) = element {
        doc.push("asset", Value::Document(encode_image_asset(&image.asset)?));
    }
    doc.push("scale", Value::Double(scale as f64));
    Ok(bson::encode(&doc)?)
}

pub fn decode_clipboard_element(bytes: &[u8]) -> Result<(Element, f32), FormatError> {
    let doc = bson::decode(bytes)?;
    let scale = doc.req_f32("scale")?;
    if !(0.1..=16.0).contains(&scale) {
        return Err(bad("invalid clipboard scale"));
    }
    let kind = doc.req_str("kind")?;
    let element = if kind == "image" {
        // Independent documents may have assigned the same asset ID.
        let asset = decode_image_pixels(doc.req_doc("asset")?, ImageId::next())?;
        decode_placed_image(&doc, asset)?
    } else {
        Element::Shape(decode_shape(&doc, kind)?)
    };
    Ok((element, scale))
}

fn decode_element(d: &Doc, assets: &HashMap<ImageId, ImageAsset>) -> Result<Element, FormatError> {
    let kind = d.req_str("kind")?;
    if kind == "image" {
        let id = ImageId(u64::try_from(d.req_i64("image_id")?).map_err(|_| bad("invalid image_id"))?);
        let asset = assets.get(&id).ok_or_else(|| bad(format!("missing image asset {}", id.0)))?.clone();
        return decode_placed_image(d, asset);
    }

    Ok(Element::Shape(decode_shape(d, kind)?))
}

fn decode_placed_image(doc: &Doc, asset: ImageAsset) -> Result<Element, FormatError> {
    let bounds = decode_rect(doc.req_doc("bounds")?)?;
    if bounds.w < 1.0 || bounds.h < 1.0 {
        return Err(bad("placed image bounds must be at least 1x1"));
    }
    Ok(Element::Image(PlacedImage::new(asset, bounds)))
}

fn decode_shape(d: &Doc, kind: &str) -> Result<Shape, FormatError> {
    let style = Style {
        color: decode_color(d.req_doc("color")?)?,
        size: decode_size(d.req_str("size")?)?,
    };
    let geometry = match kind {
        "rect" => Geometry::Rect(decode_rect(d.req_doc("bounds")?)?),
        "ellipse" => Geometry::Ellipse(decode_rect(d.req_doc("bounds")?)?),
        "arrow" => {
            let from = decode_point(d.req_doc("from")?)?;
            let to = decode_point(d.req_doc("to")?)?;
            let arrow_style = match d.req_str("arrow_style")? {
                "straight" => ArrowStyle::Straight,
                "elbow" => ArrowStyle::Elbow,
                "curved" => ArrowStyle::Curved,
                other => return Err(bad(format!("unknown arrow_style `{other}`"))),
            };
            Geometry::Arrow { from, to, style: arrow_style }
        }
        "pen" => Geometry::Pen(decode_shape_points(d)?),
        "highlight" => Geometry::Highlight(decode_shape_points(d)?),
        "counter" => {
            let center = decode_point(d.req_doc("center")?)?;
            let number = u32::try_from(d.req_i32("number")?).unwrap_or(1).max(1);
            let format = match d.req_str("format")? {
                "decimal" => CounterFormat::Decimal,
                "upper_alpha" => CounterFormat::UpperAlpha,
                "roman" => CounterFormat::Roman,
                "lower_alpha" => CounterFormat::LowerAlpha,
                other => return Err(bad(format!("unknown counter format `{other}`"))),
            };
            Geometry::Counter { center, number, format }
        }
        "redact" => {
            let effect = match d.req_str("effect")? {
                "mosaic" => RedactEffect::Mosaic,
                "blur" => RedactEffect::Blur,
                "gray" => RedactEffect::Gray,
                other => return Err(bad(format!("unknown redact effect `{other}`"))),
            };
            let strength = d.req_f32("strength")?.clamp(0.0, 1.0);
            let area = match d.req_str("area_kind")? {
                "brush" => RedactShape::Brush(decode_shape_points(d)?),
                "rect" => RedactShape::Rect(decode_rect(d.req_doc("bounds")?)?),
                other => return Err(bad(format!("unknown redact area `{other}`"))),
            };
            Geometry::Redact { area, effect, strength }
        }
        "text" => {
            let origin = decode_point(d.req_doc("origin")?)?;
            let content = d.req_str("content")?.to_owned();
            let text_style = match d.req_str("text_style")? {
                "standard" => TextStyle::Standard,
                "rounded" => TextStyle::Rounded,
                "monospaced" => TextStyle::Monospaced,
                "outlined" => TextStyle::Outlined,
                "boxed" => TextStyle::Boxed,
                "rounded_boxed" => TextStyle::RoundedBoxed,
                "monospaced_boxed" => TextStyle::MonospacedBoxed,
                other => return Err(bad(format!("unknown text_style `{other}`"))),
            };
            Geometry::Text { origin, content, style: text_style }
        }
        other => return Err(bad(format!("unknown element kind `{other}`"))),
    };
    Ok(Shape { geometry, style })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_image_alpha_survives_save_and_reopen() {
        let raw = vec![255, 0, 0, 128, 30, 60, 90, 0];
        let image = pixmap_from_straight_rgba(2, 1, raw).unwrap();
        let comp = Composition::from_slice(image, 1.0, vec![]);
        let restored = decode_bs(&encode_bs(&comp).unwrap()).unwrap();
        let image = restored.elements[0].as_image().unwrap().asset.pixmap();
        assert_eq!(image.data(), &[128, 0, 0, 128, 0, 0, 0, 0]);
    }
    use crate::style::PALETTE;

    #[test]
    fn bs_format_round_trips_images_and_every_shape_variant_bit_exactly() {
        let mut rgba = vec![0u8; 16 * 12 * 4];
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *px = [(i * 7) as u8, (i * 13) as u8, (i * 29) as u8, 255];
        }
        let img = pixmap_from_rgba(16, 12, rgba.clone()).unwrap();
        let shapes = vec![
            Shape { geometry: Geometry::Rect(Rect::new(1.0, 2.0, 10.0, 8.0)), style: Style { color: PALETTE[0], size: SizeLevel::Small } },
            Shape { geometry: Geometry::Ellipse(Rect::new(2.0, 3.0, 9.0, 7.0)), style: Style { color: PALETTE[1], size: SizeLevel::Large } },
            Shape {
                geometry: Geometry::Arrow { from: Point::new(0.0, 0.0), to: Point::new(15.0, 10.0), style: ArrowStyle::Curved },
                style: Style { color: PALETTE[2], size: SizeLevel::Medium },
            },
            Shape { geometry: Geometry::Pen(vec![Point::new(1.0, 1.0), Point::new(5.0, 8.0)]), style: Style::default() },
            Shape { geometry: Geometry::Highlight(vec![Point::new(2.0, 2.0), Point::new(12.0, 2.0)]), style: Style::default() },
            Shape {
                geometry: Geometry::Counter { center: Point::new(6.0, 6.0), number: 2, format: CounterFormat::Roman },
                style: Style { color: PALETTE[3], size: SizeLevel::Medium },
            },
            Shape {
                geometry: Geometry::Redact { area: RedactShape::Rect(Rect::new(0.0, 0.0, 8.0, 6.0)), effect: RedactEffect::Blur, strength: 0.6 },
                style: Style::default(),
            },
            Shape {
                geometry: Geometry::Redact {
                    area: RedactShape::Brush(vec![Point::new(0.0, 0.0), Point::new(10.0, 10.0)]),
                    effect: RedactEffect::Mosaic,
                    strength: 0.25,
                },
                style: Style::default(),
            },
            Shape {
                geometry: Geometry::Text { origin: Point::new(3.0, 4.0), content: "Hello 你好".into(), style: TextStyle::RoundedBoxed },
                style: Style { color: PALETTE[4], size: SizeLevel::Large },
            },
        ];
        let mut comp = Composition::from_slice(img, 2.0, shapes);
        comp.width = 640;
        comp.height = 480;

        let encoded = encode_bs(&comp).unwrap();
        let decoded = decode_bs(&encoded).unwrap();
        assert_eq!(decoded, comp);
        let decoded_rgba = to_rgba_straight(decoded.elements[0].as_image().unwrap().asset.pixmap());
        assert_eq!(decoded_rgba, rgba);
    }

    #[test]
    fn multi_capture_editor_session_saves_reopens_and_renders_identically() {
        use std::sync::Arc;
        use crate::{Document, Editor, Fonts, Tool};

        let fonts = Arc::new(Fonts::empty());
        let mut cap_rgba = vec![0u8; 200 * 150 * 4];
        for (i, px) in cap_rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *px = [(i * 5) as u8, (i * 11) as u8, 180, 255];
        }
        let doc = Document::from_rgba(200, 150, cap_rgba).unwrap();
        let mut capture = Editor::new(doc, fonts.clone(), vec![Rect::new(20.0, 10.0, 120.0, 90.0)], 1.0);
        // Select window and draw a rectangle in Capture #1.
        capture.pointer_move(Point::new(50.0, 40.0), false);
        capture.pointer_down(Point::new(50.0, 40.0));
        capture.pointer_up(Point::new(50.0, 40.0));
        capture.toggle_tool(Tool::Rect);
        capture.pointer_down(Point::new(30.0, 20.0));
        capture.pointer_move(Point::new(90.0, 60.0), false);
        capture.pointer_up(Point::new(90.0, 60.0));

        let (img1, scale1, shapes1) = capture.extract_slice().unwrap();
        assert_eq!((img1.width(), img1.height()), (120, 90));
        assert_eq!(shapes1.len(), 1);

        // Open Editor from Capture #1: canvas defaults to first image size (120x90).
        let comp = Composition::from_slice(img1, scale1, shapes1);
        let mut editor = Editor::from_composition(comp, fonts.clone());
        assert_eq!(editor.screen(), Rect::new(0.0, 0.0, 120.0, 90.0));

        // Append a second captured image with its own annotation.
        let img2 = pixmap_from_rgba(80, 60, [40, 200, 90, 255].repeat(80 * 60)).unwrap();
        let shape2 = Shape { geometry: Geometry::Ellipse(Rect::new(10.0, 10.0, 50.0, 35.0)), style: Style::default() };
        editor.append_slice(img2, 1.0, vec![shape2]);

        // Expand the canvas from the left edge (-30 px).
        editor.pointer_down(Point::new(0.0, 45.0));
        editor.pointer_move(Point::new(-30.0, 45.0), false);
        editor.pointer_up(Point::new(-30.0, 45.0));
        assert_eq!(editor.take_canvas_origin_shift(), Point::new(-30.0, 0.0));

        let before = editor.export().unwrap();
        let bs_bytes = encode_bs(&editor.to_composition().unwrap()).unwrap();

        // Reopen `.bs` in a fresh Editor: rendered output must match pixel-for-pixel,
        // and pre-existing elements must still be selectable and deletable.
        let reopened_comp = decode_bs(&bs_bytes).unwrap();
        let mut reopened = Editor::from_composition(reopened_comp, fonts);
        let after = reopened.export().unwrap();
        assert_eq!((before.width(), before.height()), (after.width(), after.height()));
        assert_eq!(before.data(), after.data());

        let count_before_delete = reopened.elements().len();
        reopened.pointer_down(Point::new(40.0, 10.0)); // hit Capture #1's Rect (shifted +30 in x)
        reopened.pointer_up(Point::new(40.0, 10.0));
        assert!(reopened.delete_selected());
        assert_eq!(reopened.elements().len(), count_before_delete - 1);
    }
}
