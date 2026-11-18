//! Composition Editor clipboard. A single worker serves requests in keyboard
//! order and keeps the clipboard owner alive on X11.

use std::sync::mpsc;

use anyhow::{Context as _, Result};
use bluesnap_core::format::{decode_clipboard_element, encode_clipboard_element};
use bluesnap_core::pixels::pixmap_from_straight_rgba;
use bluesnap_core::shape::{Geometry, Shape};
use bluesnap_core::{Element, Pixmap};
use clipboard_rs::{Clipboard, ClipboardContext, ContentFormat, common::RustImage};
use futures::channel::oneshot;

const ELEMENT_FORMAT: &str = "application/x-bluesnap-element-v1";

pub enum CopyContent {
    Item { pixels: Pixmap, element: Option<Element>, scale: f32 },
    Text(String),
}

pub enum Paste {
    Element(Element, f32),
    Image(Pixmap),
    Text(String),
}

type Job = Box<dyn FnOnce(Result<&ClipboardContext>) + Send>;

pub struct EditorClipboard(mpsc::Sender<Job>);

impl gpui::Global for EditorClipboard {}

impl Default for EditorClipboard {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::spawn(move || {
            let mut context = None;
            for job in rx {
                let clipboard = match &context {
                    Some(clipboard) => Ok(clipboard),
                    None => ClipboardContext::new().map(|c| &*context.insert(c)).map_err(anyhow::Error::from_boxed),
                };
                job(clipboard);
            }
        });
        Self(tx)
    }
}

impl EditorClipboard {
    fn run<T, F>(&self, f: F) -> impl Future<Output = Result<T>> + use<T, F>
    where
        T: Send + 'static,
        F: FnOnce(&ClipboardContext) -> Result<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.send(Box::new(move |clipboard| {
            let _ = tx.send(clipboard.and_then(f));
        }));
        async move { rx.await.context("Clipboard worker stopped")? }
    }

    pub fn copy(&self, content: CopyContent) -> impl Future<Output = Result<()>> + use<> {
        self.run(move |clipboard| match content {
            CopyContent::Text(text) => clipboard.set_text(text).map_err(anyhow::Error::from_boxed),
            CopyContent::Item { pixels, element, scale } => {
                let encoded = element.as_ref().map(|item| encode_clipboard_element(item, scale)).transpose()?;
                let text = match element {
                    Some(Element::Shape(Shape { geometry: Geometry::Text { content, .. }, .. })) => Some(content),
                    _ => None,
                };
                write_formats(clipboard, pixels, encoded, text)
            }
        })
    }

    pub fn paste(&self, editing_text: bool) -> impl Future<Output = Result<Option<Paste>>> + use<> {
        self.run(move |clipboard| {
            if editing_text && clipboard.has(ContentFormat::Text) {
                return Ok(Some(Paste::Text(clipboard.get_text().map_err(anyhow::Error::from_boxed)?)));
            }
            if clipboard.has(ContentFormat::Other(ELEMENT_FORMAT.into()))
                && let Ok(bytes) = clipboard.get_buffer(ELEMENT_FORMAT)
                && let Ok((element, scale)) = decode_clipboard_element(&bytes)
            {
                return Ok(Some(Paste::Element(element, scale)));
            }
            let rgba = if clipboard.has(ContentFormat::Image) {
                let image = clipboard.get_image().map_err(anyhow::Error::from_boxed)?;
                Some(image.to_rgba8().map_err(anyhow::Error::from_boxed)?)
            } else {
                // Browsers and image viewers can offer an encoded JPEG alone.
                let jpeg = if cfg!(target_os = "macos") { "public.jpeg" } else if cfg!(windows) { "JFIF" } else { "image/jpeg" };
                if clipboard.has(ContentFormat::Other(jpeg.into())) {
                    let bytes = clipboard.get_buffer(jpeg).map_err(anyhow::Error::from_boxed)?;
                    Some(image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)?.into_rgba8())
                } else {
                    None
                }
            };
            if let Some(rgba) = rgba {
                let (w, h) = rgba.dimensions();
                let pixels = pixmap_from_straight_rgba(w, h, rgba.into_raw()).context("Invalid clipboard image dimensions")?;
                return Ok(Some(Paste::Image(pixels)));
            }
            if clipboard.has(ContentFormat::Text) {
                return Ok(Some(Paste::Text(clipboard.get_text().map_err(anyhow::Error::from_boxed)?)));
            }
            Ok(None)
        })
    }
}

#[cfg(not(windows))]
fn write_formats(clipboard: &ClipboardContext, pixels: Pixmap, encoded: Option<Vec<u8>>, text: Option<String>) -> Result<()> {
    use clipboard_rs::ClipboardContent;
    let png_format = if cfg!(target_os = "macos") { "public.png" } else { "image/png" };
    let mut formats = vec![ClipboardContent::Other(png_format.into(), pixels.encode_png()?)];
    if let Some(bytes) = encoded {
        formats.push(ClipboardContent::Other(ELEMENT_FORMAT.into(), bytes));
    }
    if let Some(text) = text {
        formats.push(ClipboardContent::Text(text));
    }
    clipboard.set(formats).map_err(anyhow::Error::from_boxed)
}

#[cfg(windows)]
fn write_formats(_: &ClipboardContext, pixels: Pixmap, encoded: Option<Vec<u8>>, text: Option<String>) -> Result<()> {
    use clipboard_win::{formats::CF_DIB, options::NoClear, raw};
    let png = pixels.encode_png()?;
    // Some applications only accept a DIB. The encoder emits a 14-byte BMP
    // file header followed by the DIB header, channel masks and bitmap bits.
    let mut bmp = Vec::new();
    image::codecs::bmp::BmpEncoder::new(&mut bmp).encode(
        &bluesnap_core::render::to_rgba_straight(&pixels), pixels.width(), pixels.height(), image::ExtendedColorType::Rgba8,
    )?;
    let png_format = clipboard_win::register_format("PNG").context("Could not register PNG clipboard format")?;
    let element_format = clipboard_win::register_format(ELEMENT_FORMAT).context("Could not register element clipboard format")?;
    let _open = clipboard_win::Clipboard::new_attempts(10)?;
    raw::empty()?;
    raw::set_without_clear(png_format.get(), &png)?;
    raw::set_without_clear(CF_DIB, &bmp[14..])?;
    if let Some(bytes) = encoded {
        raw::set_without_clear(element_format.get(), &bytes)?;
    }
    if let Some(text) = text {
        raw::set_string_with(&text, NoClear)?;
    }
    Ok(())
}
