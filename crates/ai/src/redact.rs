//! Finding sensitive text in a screenshot's recognized lines.
//!
//! Chosen by experiment (12 synthetic screenshots with known sensitive spans;
//! see the README): sending the screenshot image and asking for boxes placed
//! them far off (about 10% found), while OCR line boxes plus text classification
//! found them accurately. However, isolated numbers like `"$471,932.37"` or
//! `"973.37B"` look benign on their own unless the classifier knows the full
//! screenshot is an internal FinOps / billing / analytics dashboard. So:
//!
//! 1. **In parallel with local OCR ("find text")**, [`describe_image_context`]
//!    sends a compact thumbnail of the screenshot to the LLM to summarize what
//!    the image is (background info) and what kinds of values on it should be
//!    treated as sensitive data (falling back to [`describe_text_context`] if
//!    the configured LLM is text-only);
//! 2. **Span & line classification** then passes that full-picture context into
//!    both the LLM (in chunks of [`CHUNK`] consecutive lines) and the decision
//!    model (per line, in parallel), placing exact boxes with [`bluesnap_core::ink::place_span`].

use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::bail;
use serde_json::Value;

use bluesnap_core::pixels::{crop, pixmap_from_rgba};
use bluesnap_core::text::FontSet;
use bluesnap_core::{Pixmap, Rect};

use crate::client::{self, VisionError};
use crate::settings::{AiSettings, Endpoint};

/// Lines per LLM request.
const CHUNK: usize = 8;
/// Requests in flight at once, per model.
const MAX_PARALLEL: usize = 16;
/// Maximum side length (in pixels) of the thumbnail sent for background context.
const CONTEXT_THUMB_MAX: u32 = 1024;
/// Maximum OCR lines sampled when falling back to text-only background context.
const CONTEXT_TEXT_MAX_LINES: usize = 80;
/// The decision model alone flags a line below this probability of "ordinary".
const DECIDE_ALONE: f64 = 0.5;
/// Alongside the LLM (without image context) it only adds lines it is this sure about.
const DECIDE_EXTRA: f64 = 0.05;
/// Alongside the LLM when full-screen context is available.
const DECIDE_EXTRA_WITH_CONTEXT: f64 = 0.35;

const CONTEXT_IMAGE_PROMPT: &str = "\
Someone wants to share this screenshot externally after redacting sensitive information.\n\
Answer directly right now without calling any tools; do not write a preamble.\n\
Output a concise briefing (under 120 words) with two sections:\n\
1. Background: What kind of page, app, dashboard, or document this screenshot shows.\n\
2. Sensitive data: Which specific kinds of values visible in this image should be treated as \
confidential or sensitive in context — including not only credentials and personal data \
(passwords, API keys, tokens, emails, phones, addresses, names, usernames, IDs, bank/card numbers), \
but also context-dependent confidential data such as financial, billing, cost, spend or revenue \
amounts, business/FinOps/usage metrics (token counts, request counts, record counts, percentages), \
internal model/project/organization/account identifiers, or private records when this is an \
internal dashboard, billing page, analytics view, admin console, or private document. \
Explicitly distinguish confidential data values from generic static UI chrome, product titles, \
navigation items, and column/card headings (e.g. 'Dashboards', 'Total tokens', 'Requests', \
'Billed cost', 'Export', 'Share' are UI labels, while the numbers, dollar figures, and internal \
identifiers under them are the sensitive values).";

const CONTEXT_TEXT_PROMPT: &str = "\
Below is text recognized from a screenshot someone wants to share externally after redacting \
sensitive information.\n\
Answer directly right now without calling any tools; do not write a preamble.\n\
Output a concise briefing (under 120 words) with two sections:\n\
1. Background: What kind of page, app, dashboard, or document this screenshot shows.\n\
2. Sensitive data: Which specific kinds of values on this screen should be treated as \
confidential or sensitive in context — including credentials, personal data, and any \
context-dependent confidential financial/billing/cost/revenue amounts, business/usage/token/request \
metrics, or internal model/project/organization/account identifiers if this is an internal \
dashboard, billing view, analytics tool, admin panel, or private record, while distinguishing \
confidential values from generic static UI labels/headings.\n\n";

const PROMPT: &str = "Answer directly with JSON only; do not call any tools.\n\
Below are the text lines recognized in a screenshot someone wants to share, numbered. \
Using the screenshot background above (when provided), find every piece of text that should be redacted first: \
passwords, API keys, tokens, recovery codes and other credentials; email addresses; phone numbers; postal addresses; \
government ID, bank account, IBAN and card numbers (and their CVV); dates of birth; people's names and usernames; \
and any context-sensitive confidential values on this screen (such as internal billing, cost, spend or revenue \
figures, business/FinOps/usage/token/request/record counts and percentages, or internal model, project and \
organization identifiers on private dashboards, billing pages, or internal tools). \
Return only the sensitive part, copied exactly as it appears in the line, not generic static UI labels \
(for 'Email: a@b.com' return 'a@b.com'; for 'External $471,932.37 +44.6%' return '$471,932.37' and '+44.6%'); \
for a URL or connection string with credentials return the 'user:password' part. Generic static UI chrome, \
app/page titles, column/card headings, menus, public documentation, and non-confidential text are not sensitive. \
Reply with only a JSON array of {\"line\": number, \"text\": string}; [] if nothing is sensitive.\n\n";

/// Baseline categories for the decision model when no LLM background context is available.
const CRITERIA_BASE: [(&str, &str); 5] = [
    ("secret", "a password, API key, access token, recovery code or other credential"),
    ("contact", "an email address, phone number or postal address of a person or organization"),
    ("identity", "a person's name, date of birth, or government ID number"),
    ("financial", "a bank account, IBAN, payment card number or card security code"),
    ("ordinary", "ordinary interface, document or code text with no personal or secret information"),
];

/// Context-aware categories for the decision model when the LLM provided screenshot background info.
const CRITERIA_WITH_CONTEXT: [(&str, &str); 6] = [
    ("secret", "a password, API key, access token, recovery code or other credential"),
    ("contact", "an email address, phone number or postal address of a person or organization"),
    ("identity", "a person's name, username, account handle, date of birth, or government ID number"),
    ("financial", "a bank account, IBAN, card number, security code, or confidential billing, cost, spend, revenue or financial figure"),
    ("contextual_sensitive", "a confidential business, FinOps or usage metric (count, volume, percentage), internal model/project/organization identifier, or other value identified as sensitive in the screenshot background"),
    ("ordinary", "a generic static UI label, navigation item, card/column title, date range, or non-sensitive text with no confidential values"),
];

/// Outcome of the parallel image-context LLM step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextOutcome {
    /// Background & sensitivity summary ready to pass to the decision model and span extractor.
    Ready(String),
    /// The configured LLM rejected image input (text-only model); caller can fall back to [`describe_text_context`].
    UnsupportedImage,
    /// Network, auth, or empty-output failure.
    Failed(String),
}

/// Calls `endpoint` with a compact thumbnail of `image` to summarize what the
/// screenshot is and what could be treated as sensitive data on it. Designed to
/// run concurrently with local OCR (`recognize_text`).
pub fn describe_image_context(endpoint: &Endpoint, image: &Pixmap) -> ContextOutcome {
    let Some(data_url) = thumbnail_png_data_url(image) else {
        return ContextOutcome::Failed("could not encode screenshot thumbnail".into());
    };
    match client::llm_vision_reply(endpoint, CONTEXT_IMAGE_PROMPT, &data_url) {
        Ok(reply) => {
            let trimmed = reply.trim();
            if trimmed.is_empty() {
                ContextOutcome::Failed("empty context reply".into())
            } else {
                ContextOutcome::Ready(trimmed.to_string())
            }
        }
        Err(VisionError::UnsupportedImage(msg)) => {
            log::debug!("LLM rejected image input for context step ({msg}); will fall back to OCR text");
            ContextOutcome::UnsupportedImage
        }
        Err(VisionError::Request(e)) => ContextOutcome::Failed(format!("{e:#}")),
    }
}

/// Text-only fallback for [`describe_image_context`] when the configured LLM
/// does not accept image inputs.
pub fn describe_text_context(endpoint: &Endpoint, lines: &[String]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let sample: Vec<&str> = lines.iter().take(CONTEXT_TEXT_MAX_LINES).map(String::as_str).collect();
    let prompt = format!("{CONTEXT_TEXT_PROMPT}{}", sample.join("\n"));
    match client::llm_reply(endpoint, &prompt) {
        Ok(reply) => {
            let trimmed = reply.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Err(e) => {
            log::warn!("text context fallback failed: {e:#}");
            None
        }
    }
}

/// Sensitive text: a byte range of one of the given lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub line: usize,
    pub range: Range<usize>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
    /// Requests that failed even after retries; their lines went unchecked.
    pub failed: usize,
    pub requests: usize,
    /// Why the first failed request failed, to show the user.
    pub first_error: Option<String>,
}

/// Finds sensitive text in `lines` (in reading order), deriving screen context
/// from the text first when an LLM is configured.
pub fn find_sensitive(settings: &AiSettings, lines: &[String]) -> anyhow::Result<Report> {
    if lines.is_empty() {
        if settings.llm().is_none() && settings.decisions().is_none() {
            bail!("no AI model is configured");
        }
        return Ok(Report::default());
    }
    let context = settings.llm().and_then(|ep| describe_text_context(ep, lines));
    find_sensitive_with_context(settings, lines, context.as_deref())
}

/// Finds sensitive text in `lines` (in reading order) using pre-computed
/// `context` (from [`describe_image_context`] run in parallel with OCR).
pub fn find_sensitive_with_context(
    settings: &AiSettings,
    lines: &[String],
    context: Option<&str>,
) -> anyhow::Result<Report> {
    let (llm, decisions) = (settings.llm(), settings.decisions());
    if llm.is_none() && decisions.is_none() {
        bail!("no AI model is configured");
    }
    if lines.is_empty() {
        return Ok(Report::default());
    }
    let context = context.map(str::trim).filter(|s| !s.is_empty());
    let errors = Mutex::new(Vec::new());
    let (from_llm, kinds) = std::thread::scope(|s| {
        let from_llm = llm.map(|e| s.spawn(|| ask_llm(e, lines, context, &errors)));
        let kinds = decisions.map(|e| s.spawn(|| classify(e, lines, context, &errors)));
        (
            from_llm.map(|h| h.join().expect("LLM worker panicked")),
            kinds.map(|h| h.join().expect("decision worker panicked")),
        )
    });

    let mut findings = from_llm.clone().unwrap_or_default();
    if let Some(kinds) = &kinds {
        let threshold = match (from_llm.is_some(), context.is_some()) {
            (true, true) => DECIDE_EXTRA_WITH_CONTEXT,
            (true, false) => DECIDE_EXTRA,
            (false, _) => DECIDE_ALONE,
        };
        for (line, p) in kinds.iter().enumerate() {
            let flagged = p.is_some_and(|p| p < threshold);
            if flagged && !findings.iter().any(|f| f.line == line) {
                findings.push(Finding { line, range: value_range(&lines[line]) });
            }
        }
    }
    let requests = from_llm.as_ref().map_or(0, |_| lines.len().div_ceil(CHUNK)) + kinds.as_ref().map_or(0, |_| lines.len());
    let errors = errors.into_inner().expect("poisoned");
    if !errors.is_empty() && errors.len() == requests {
        bail!("{}", errors[0]);
    }
    for e in errors.iter().take(3) {
        log::warn!("AI request failed: {e}");
    }
    Ok(Report { findings: merge(findings), failed: errors.len(), requests, first_error: errors.first().cloned() })
}

/// Where sensitive text is drawn in `image`, given its OCR `lines` (boxes in
/// `image` pixels, any order).
pub fn locate_sensitive(
    settings: &AiSettings,
    image: &Pixmap,
    lines: &[(Rect, String)],
    fonts: &FontSet,
) -> anyhow::Result<(Vec<Rect>, Report)> {
    let boxes: Vec<Rect> = lines.iter().map(|(r, _)| *r).collect();
    let order: Vec<usize> = bluesnap_core::reading_order::rows(&boxes).into_iter().flatten().collect();
    let texts: Vec<String> = order.iter().map(|&i| lines[i].1.clone()).collect();
    let context = match settings.llm() {
        Some(ep) => match describe_image_context(ep, image) {
            ContextOutcome::Ready(ctx) => Some(ctx),
            ContextOutcome::UnsupportedImage => describe_text_context(ep, &texts),
            ContextOutcome::Failed(err) => {
                log::warn!("image context step failed: {err}");
                None
            }
        },
        None => None,
    };
    locate_ordered(settings, image, &boxes, &order, &texts, fonts, context.as_deref())
}

/// Like [`locate_sensitive`], using a `context` summary already computed in
/// parallel while local OCR was running.
pub fn locate_sensitive_with_context(
    settings: &AiSettings,
    image: &Pixmap,
    lines: &[(Rect, String)],
    fonts: &FontSet,
    context: Option<&str>,
) -> anyhow::Result<(Vec<Rect>, Report)> {
    let boxes: Vec<Rect> = lines.iter().map(|(r, _)| *r).collect();
    let order: Vec<usize> = bluesnap_core::reading_order::rows(&boxes).into_iter().flatten().collect();
    let texts: Vec<String> = order.iter().map(|&i| lines[i].1.clone()).collect();
    locate_ordered(settings, image, &boxes, &order, &texts, fonts, context)
}

fn locate_ordered(
    settings: &AiSettings,
    image: &Pixmap,
    boxes: &[Rect],
    order: &[usize],
    texts: &[String],
    fonts: &FontSet,
    context: Option<&str>,
) -> anyhow::Result<(Vec<Rect>, Report)> {
    let report = find_sensitive_with_context(settings, texts, context)?;
    let rects = report
        .findings
        .iter()
        .map(|f| bluesnap_core::ink::place_span(image, fonts, boxes[order[f.line]], &texts[f.line], f.range.clone()))
        .collect();
    Ok((rects, report))
}

/// Runs `f` over `items` on up to [`MAX_PARALLEL`] threads; results in order.
fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..items.len().min(MAX_PARALLEL) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    *results[i].lock().expect("poisoned") = Some(f(item));
                }
            });
        }
    });
    results.into_iter().map(|r| r.into_inner().expect("poisoned").expect("every item ran")).collect()
}

fn format_llm_chunk_prompt(context: Option<&str>, numbered_lines: &str) -> String {
    match context {
        Some(ctx) => format!(
            "Screenshot background and what should be treated as sensitive here:\n{ctx}\n\n{PROMPT}{numbered_lines}"
        ),
        None => format!("{PROMPT}{numbered_lines}"),
    }
}

fn format_decision_state(context: Option<&str>, line: &str) -> (String, &'static [(&'static str, &'static str)]) {
    match context {
        Some(ctx) => (
            format!("Screenshot background and what is sensitive here:\n{ctx}\n\nText line on screen: {line}"),
            &CRITERIA_WITH_CONTEXT,
        ),
        None => (format!("Text found on screen: {line}"), &CRITERIA_BASE),
    }
}

fn ask_llm(endpoint: &Endpoint, lines: &[String], context: Option<&str>, errors: &Mutex<Vec<String>>) -> Vec<Finding> {
    let chunks: Vec<Range<usize>> = (0..lines.len()).step_by(CHUNK).map(|i| i..(i + CHUNK).min(lines.len())).collect();
    parallel_map(&chunks, |chunk| {
        let numbered: Vec<String> = chunk.clone().map(|i| format!("{i}: {}", lines[i])).collect();
        let prompt = format_llm_chunk_prompt(context, &numbered.join("\n"));
        match client::llm_reply(endpoint, &prompt) {
            Ok(reply) => parse_reply(&reply, lines, chunk.clone()),
            Err(e) => {
                errors.lock().expect("poisoned").push(format!("{e:#}"));
                Vec::new()
            }
        }
    })
    .into_iter()
    .flatten()
    .collect()
}

/// Each line's probability of being ordinary text, `None` where the request failed.
fn classify(
    endpoint: &Endpoint,
    lines: &[String],
    context: Option<&str>,
    errors: &Mutex<Vec<String>>,
) -> Vec<Option<f64>> {
    parallel_map(lines, |line| {
        let (state, criteria) = format_decision_state(context, line);
        match client::decide(endpoint, &state, criteria) {
            Ok(p) => Some(p.get("ordinary").copied().unwrap_or(1.0)),
            Err(e) => {
                errors.lock().expect("poisoned").push(format!("{e:#}"));
                None
            }
        }
    })
}

/// Downscales `image` to at most [`CONTEXT_THUMB_MAX`] px on its longest side
/// (cropping extreme scrolling heights first so top headers/cards stay legible)
/// and encodes it as a `data:image/png;base64,…` URL without cloning `image`.
fn thumbnail_png_data_url(image: &Pixmap) -> Option<String> {
    let (w, h) = (image.width(), image.height());
    if w == 0 || h == 0 {
        return None;
    }
    let cropped;
    let src = if h > w.saturating_mul(3) {
        cropped = crop(image, 0, 0, w, w.saturating_mul(3))?;
        &cropped
    } else {
        image
    };
    let (sw, sh) = (src.width(), src.height());
    let png = if sw <= CONTEXT_THUMB_MAX && sh <= CONTEXT_THUMB_MAX {
        src.encode_png().ok()?
    } else {
        let scale = (CONTEXT_THUMB_MAX as f32) / (sw.max(sh) as f32);
        let tw = ((sw as f32 * scale).round() as u32).max(1);
        let th = ((sh as f32 * scale).round() as u32).max(1);
        let thumb = downscale_box(src, tw, th)?;
        thumb.encode_png().ok()?
    };
    Some(format!("data:image/png;base64,{}", base64_encode(&png)))
}

fn downscale_box(src: &Pixmap, tw: u32, th: u32) -> Option<Pixmap> {
    let (sw, sh) = (src.width() as usize, src.height() as usize);
    let (tw_u, th_u) = (tw as usize, th as usize);
    let src_data = src.data();
    let src_stride = sw * 4;
    let mut out = vec![0u8; tw_u * th_u * 4];
    for oy in 0..th_u {
        let y0 = (oy * sh) / th_u;
        let y1 = ((oy + 1) * sh).div_ceil(th_u).min(sh).max(y0 + 1);
        for ox in 0..tw_u {
            let x0 = (ox * sw) / tw_u;
            let x1 = ((ox + 1) * sw).div_ceil(tw_u).min(sw).max(x0 + 1);
            let mut sum = [0u32; 4];
            for y in y0..y1 {
                let row = &src_data[y * src_stride + x0 * 4..y * src_stride + x1 * 4];
                for px in row.as_chunks::<4>().0 {
                    sum[0] += px[0] as u32;
                    sum[1] += px[1] as u32;
                    sum[2] += px[2] as u32;
                    sum[3] += px[3] as u32;
                }
            }
            let count = ((x1 - x0) * (y1 - y0)) as u32;
            let dst = &mut out[(oy * tw_u + ox) * 4..(oy * tw_u + ox + 1) * 4];
            dst[0] = ((sum[0] + count / 2) / count) as u8;
            dst[1] = ((sum[1] + count / 2) / count) as u8;
            dst[2] = ((sum[2] + count / 2) / count) as u8;
            dst[3] = ((sum[3] + count / 2) / count) as u8;
        }
    }
    pixmap_from_rgba(tw, th, out)
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let (chunks, rem) = bytes.as_chunks::<3>();
    for &[b0, b1, b2] in chunks {
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        out.push(TABLE[(n & 0x3F) as usize] as char);
    }
    match *rem {
        [b0] => {
            let n = (b0 as u32) << 16;
            out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
            out.push('=');
            out.push('=');
        }
        [b0, b1] => {
            let n = ((b0 as u32) << 16) | ((b1 as u32) << 8);
            out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
            out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

/// The LLM's `[{"line": n, "text": "…"}]`, as findings within `chunk`.
///
/// Models sometimes cite the wrong line number or retype a character, so text
/// not in its line is looked for in the chunk's other lines, then matched
/// approximately; only if all fail is the whole cited line redacted, to be safe.
fn parse_reply(reply: &str, lines: &[String], chunk: Range<usize>) -> Vec<Finding> {
    let json = match (reply.find('['), reply.rfind(']')) {
        (Some(a), Some(b)) if a < b => &reply[a..=b],
        _ => return Vec::new(),
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else {
        log::warn!("unreadable AI reply: {}", reply.chars().take(200).collect::<String>());
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let line = match item.get("line")? {
                Value::Number(n) => n.as_u64()? as usize,
                Value::String(s) => s.trim().parse().ok()?,
                _ => return None,
            };
            let text = item.get("text")?.as_str()?.trim();
            if !chunk.contains(&line) || text.is_empty() {
                return None;
            }
            if let Some(range) = find_loose(&lines[line], text) {
                return Some(Finding { line, range });
            }
            if let Some((other, range)) = chunk.clone().filter(|&i| i != line).find_map(|i| Some((i, find_loose(&lines[i], text)?))) {
                return Some(Finding { line: other, range });
            }
            let range = find_approx(&lines[line], text).unwrap_or_else(|| {
                log::debug!("AI named text not on line {line}; redacting the line");
                0..lines[line].len()
            });
            Some(Finding { line, range })
        })
        .collect()
}

fn non_ws_chars(hay: &str, needle: &str) -> (Vec<(usize, char)>, Vec<char>) {
    let hay = hay.char_indices().filter(|(_, c)| !c.is_whitespace()).collect();
    let needle = needle.chars().filter(|c| !c.is_whitespace()).collect();
    (hay, needle)
}

/// Byte range of `needle` in `hay`, ignoring whitespace and ASCII case (models
/// sometimes normalize spaces in long numbers).
fn find_loose(hay: &str, needle: &str) -> Option<Range<usize>> {
    let (hay, needle) = non_ws_chars(hay, needle);
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    let start = (0..=hay.len() - needle.len())
        .find(|&i| hay[i..i + needle.len()].iter().zip(&needle).all(|((_, a), b)| a.eq_ignore_ascii_case(b)))?;
    let (last, c) = hay[start + needle.len() - 1];
    Some(hay[start].0..last + c.len_utf8())
}

/// Like [`find_loose`], allowing one differing character in five (OCR and the
/// model disagree on look-alikes such as O and 0).
fn find_approx(hay: &str, needle: &str) -> Option<Range<usize>> {
    let (hay, needle) = non_ws_chars(hay, needle);
    if needle.len() < 4 || needle.len() > hay.len() {
        return None;
    }
    let allowed = needle.len() / 5;
    let (start, misses) = (0..=hay.len() - needle.len())
        .map(|i| (i, hay[i..i + needle.len()].iter().zip(&needle).filter(|((_, a), b)| !a.eq_ignore_ascii_case(b)).count()))
        .min_by_key(|(_, misses)| *misses)?;
    if misses > allowed {
        return None;
    }
    let (last, c) = hay[start + needle.len() - 1];
    Some(hay[start].0..last + c.len_utf8())
}

/// For a flagged line like "Email: a@b.com", the part after a short label.
fn value_range(line: &str) -> Range<usize> {
    let label_end = line.char_indices().take(32).find(|(_, c)| matches!(c, ':' | '=' | '：'));
    if let Some((i, c)) = label_end {
        let label = &line[..i];
        let value = &line[i + c.len_utf8()..];
        let plain_label = !label.trim().is_empty() && !label.chars().any(|c| c == '@' || c.is_ascii_digit());
        if plain_label && !value.trim().is_empty() {
            let start = i + c.len_utf8() + (value.len() - value.trim_start().len());
            return start..line.trim_end().len();
        }
    }
    0..line.trim_end().len()
}

/// Sorted, with overlapping ranges on the same line joined.
fn merge(mut findings: Vec<Finding>) -> Vec<Finding> {
    findings.sort_by_key(|f| (f.line, f.range.start));
    let mut out: Vec<Finding> = Vec::new();
    for f in findings {
        match out.last_mut() {
            Some(last) if last.line == f.line && f.range.start <= last.range.end => {
                last.range.end = last.range.end.max(f.range.end)
            }
            _ => out.push(f),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn replies_become_ranges_in_their_lines() {
        let lines = lines(&["Email: a@b.com", "Card 4539 1488 0343 6467 exp 09/28", "Theme: dark"]);
        let reply = "Sure:\n```json\n[{\"line\": 0, \"text\": \"a@b.com\"}, {\"line\": \"1\", \"text\": \"4539148803436467\"},\
                     {\"line\": 2, \"text\": \"not in the line\"}, {\"line\": 9, \"text\": \"x\"}]\n```";
        let found = parse_reply(reply, &lines, 0..3);
        assert_eq!(found[0], Finding { line: 0, range: 7..14 });
        // Spaces the model dropped don't matter.
        assert_eq!(&lines[1][found[1].range.clone()], "4539 1488 0343 6467");
        // Text that isn't in its line: the whole line. Lines outside the chunk: ignored.
        assert_eq!(found[2], Finding { line: 2, range: 0..11 });
        assert_eq!(found.len(), 3);
        // A wrong line number, and a retyped look-alike (O for 0).
        let found = parse_reply(r#"[{"line": 2, "text": "a@b.com"}, {"line": 1, "text": "4539 1488 O343 6467"}]"#, &lines, 0..3);
        assert_eq!(found, vec![Finding { line: 0, range: 7..14 }, Finding { line: 1, range: 5..24 }]);
        assert!(parse_reply("no JSON here", &lines, 0..3).is_empty());
    }

    #[test]
    fn loose_matching_handles_case_space_and_unicode() {
        assert_eq!(find_loose("住址：上海市 浦东", "上海市浦东"), Some(9..25));
        assert_eq!(find_loose("TOKEN ghp_AbC", "ghp_abc"), Some(6..13));
        assert_eq!(find_loose("short", "much longer"), None);
    }

    #[test]
    fn decided_lines_drop_short_labels() {
        assert_eq!(value_range("Email: a@b.com"), 7..14);
        assert_eq!(value_range("手机号：138 0013 8000"), 12..25);
        // Not a label: digits or an address before the colon, or nothing after it.
        assert_eq!(value_range("10:30 call Bob"), 0..14);
        assert_eq!(value_range("a@b.com: hi"), 0..11);
        assert_eq!(value_range("Password:"), 0..9);
    }

    #[test]
    fn overlapping_findings_merge() {
        let f = |line, r: Range<usize>| Finding { line, range: r };
        assert_eq!(merge(vec![f(1, 5..9), f(0, 0..3), f(1, 0..6), f(1, 12..14)]), vec![f(0, 0..3), f(1, 0..9), f(1, 12..14)]);
    }

    #[test]
    fn parallel_map_keeps_order() {
        let items: Vec<u32> = (0..100).collect();
        assert_eq!(parallel_map(&items, |x| x * 2), items.iter().map(|x| x * 2).collect::<Vec<_>>());
    }

    #[test]
    fn base64_and_thumbnail_data_url_encode_valid_compact_png() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");

        let large = pixmap_from_rgba(1600, 1200, vec![180; 1600 * 1200 * 4]).unwrap();
        let url = thumbnail_png_data_url(&large).unwrap();
        assert!(url.starts_with("data:image/png;base64,iVBORw0KGgo"));
        let down = downscale_box(&large, 1024, 768).unwrap();
        assert_eq!((down.width(), down.height()), (1024, 768));

        // Ultra-tall scrolling screenshot crops top 3x width before downscaling.
        let tall = pixmap_from_rgba(400, 3000, vec![90; 400 * 3000 * 4]).unwrap();
        assert!(thumbnail_png_data_url(&tall).is_some());
    }

    #[test]
    fn context_enriches_decision_state_and_llm_prompt_only_when_present() {
        let (s0, c0) = format_decision_state(None, "$471,932.37");
        assert_eq!(s0, "Text found on screen: $471,932.37");
        assert_eq!(c0.len(), 5);

        let ctx = "Internal AI FinOps billing dashboard; dollar spend and token/request volumes are confidential.";
        let (s1, c1) = format_decision_state(Some(ctx), "$471,932.37");
        assert!(s1.contains(ctx) && s1.contains("$471,932.37"));
        assert_eq!(c1.len(), 6);
        assert!(c1.iter().any(|(k, _)| *k == "contextual_sensitive"));

        let p = format_llm_chunk_prompt(Some(ctx), "0: Billed cost\n1: $471,932.37");
        assert!(p.starts_with("Screenshot background"));
        assert!(p.contains(ctx));
    }

    #[test]
    fn mock_server_image_context_unlocks_finops_decisions_and_text_fallback() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let reject_vision = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let reject_bg = reject_vision.clone();
        let stop_bg = stop.clone();

        let server = std::thread::spawn(move || {
            while !stop_bg.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else { break };
                if stop_bg.load(Ordering::Relaxed) {
                    break;
                }
                let mut buf = Vec::new();
                let mut header_end = None;
                let mut tmp = [0u8; 4096];
                loop {
                    let n = stream.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if header_end.is_none() {
                        header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
                    }
                    if let Some(hend) = header_end {
                        let headers = String::from_utf8_lossy(&buf[..hend]);
                        let clen = headers
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:")?.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= hend + clen {
                            break;
                        }
                    }
                }
                let Some(hend) = header_end else { continue };
                let head = String::from_utf8_lossy(&buf[..hend]).to_string();
                let req_body: Value = serde_json::from_slice(&buf[hend..]).unwrap_or(Value::Null);

                let (status, resp_json) = if head.starts_with("POST /v1/responses") {
                    assert_eq!(req_body.get("tool_choice").and_then(Value::as_str), Some("none"));
                    let body_str = req_body.to_string();
                    if body_str.contains("input_image") {
                        if reject_bg.load(Ordering::Relaxed) {
                            (400, serde_json::json!({"error": {"message": "image input not supported"}}))
                        } else {
                            (
                                200,
                                serde_json::json!({
                                    "output_text": "1. Background: Internal AI FinOps dashboard.\n2. Sensitive data: Dollar costs and token volumes; headings are ordinary."
                                }),
                            )
                        }
                    } else if body_str.contains("Below is text recognized from a screenshot") {
                        (
                            200,
                            serde_json::json!({
                                "output_text": "1. Background: Text-derived FinOps dashboard.\n2. Sensitive data: Dollar costs and token counts."
                            }),
                        )
                    } else {
                        (
                            200,
                            serde_json::json!({
                                "output_text": "[{\"line\": 1, \"text\": \"973.37B\"}, {\"line\": 3, \"text\": \"$471,932.37\"}]"
                            }),
                        )
                    }
                } else {
                    let state = req_body.get("state").and_then(Value::as_str).unwrap_or("");
                    let has_ctx = state.contains("FinOps dashboard");
                    let is_metric = state.ends_with("973.37B") || state.ends_with("$471,932.37") || state.ends_with("7,394,140");
                    let ord = if has_ctx && is_metric { 0.0 } else { 0.99 };
                    (
                        200,
                        serde_json::json!({
                            "answers": {"kind": {"probabilities": {"ordinary": ord, "contextual_sensitive": 1.0 - ord}}}
                        }),
                    )
                };
                let payload = resp_json.to_string();
                let reason = if status == 200 { "OK" } else { "Bad Request" };
                let reply = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });

        let base_url = format!("http://{addr}/v1");
        let llm_ep = Endpoint { url: base_url.clone(), token: "tok".into(), model: "mock-llm".into() };
        let dec_ep = Endpoint { url: base_url, token: "tok".into(), model: "mock-decisions".into() };
        let img = pixmap_from_rgba(64, 48, vec![200; 64 * 48 * 4]).unwrap();
        let ocr = lines(&["Total tokens", "973.37B", "Billed cost", "$471,932.37", "7,394,140"]);

        // Without context, decisions-only misses isolated numbers (reproducing original bug).
        let dec_only = AiSettings { llm: None, decisions: Some(dec_ep.clone()) };
        assert!(find_sensitive_with_context(&dec_only, &ocr, None).unwrap().findings.is_empty());

        // With image context computed in parallel, both LLM + decision model redact all 3 metrics and keep headings.
        let ContextOutcome::Ready(ctx) = describe_image_context(&llm_ep, &img) else {
            panic!("expected Ready context");
        };
        assert!(ctx.contains("FinOps dashboard"));
        let both = AiSettings { llm: Some(llm_ep.clone()), decisions: Some(dec_ep) };
        let rep = find_sensitive_with_context(&both, &ocr, Some(&ctx)).unwrap();
        let redacted_lines: Vec<usize> = rep.findings.iter().map(|f| f.line).collect();
        assert_eq!(redacted_lines, vec![1, 3, 4]);

        // Text-only LLM fallback when image input returns HTTP 400.
        reject_vision.store(true, Ordering::Relaxed);
        assert_eq!(describe_image_context(&llm_ep, &img), ContextOutcome::UnsupportedImage);
        let fallback_ctx = describe_text_context(&llm_ep, &ocr).expect("fallback text context");
        assert!(fallback_ctx.contains("Text-derived FinOps dashboard"));

        stop.store(true, Ordering::Relaxed);
        let _ = std::net::TcpStream::connect(addr);
        let _ = server.join();
    }

    /// Against a real endpoint: BLUESNAP_AI_URL, BLUESNAP_AI_TOKEN and
    /// BLUESNAP_AI_LLM and/or BLUESNAP_AI_DECISIONS (model names).
    #[test]
    #[ignore = "needs a live AI endpoint"]
    fn live_endpoint_finds_secrets() {
        let var = |k| std::env::var(k).ok();
        let endpoint = |model: Option<String>| {
            model.map(|model| Endpoint { url: var("BLUESNAP_AI_URL").unwrap(), token: var("BLUESNAP_AI_TOKEN").unwrap_or_default(), model })
        };
        let settings = AiSettings { llm: endpoint(var("BLUESNAP_AI_LLM")), decisions: endpoint(var("BLUESNAP_AI_DECISIONS")) };
        let lines = lines(&["Settings", "Email: margaret@example.co.uk", "Theme: dark", "API key: sk-proj-Qm3vT8xLw2Zr9KpY4nB7"]);
        let report = find_sensitive(&settings, &lines).unwrap();
        let found: Vec<&str> = report.findings.iter().map(|f| &lines[f.line][f.range.clone()]).collect();
        assert!(found.contains(&"margaret@example.co.uk") && found.iter().any(|t| t.contains("sk-proj")), "{found:?}");
        assert!(!report.findings.iter().any(|f| f.line == 0 || f.line == 2), "{found:?}");
    }
}
