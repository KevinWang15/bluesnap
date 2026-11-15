//! Finding sensitive text in a screenshot's recognized lines.
//!
//! Chosen by experiment (12 synthetic screenshots with known sensitive spans;
//! see the README): sending the screenshot image and asking for boxes placed
//! them far off (about 10% found), while OCR line boxes plus a text-only
//! question found them all. So the caller runs OCR, and here:
//!
//! - the LLM gets the lines, numbered, in chunks of [`CHUNK`] consecutive lines
//!   (enough context to tell a card's last digits from a price), all chunks in
//!   parallel, and names the sensitive substrings;
//! - the decision model classifies each line, in parallel. Alone it decides;
//!   with the LLM it only adds lines it is very sure about that the LLM left
//!   alone, which caught the few spans the LLM missed.

use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::bail;
use serde_json::Value;

use bluesnap_core::text::FontSet;
use bluesnap_core::{Pixmap, Rect};

use crate::client;
use crate::settings::{AiSettings, Endpoint};

/// Lines per LLM request.
const CHUNK: usize = 8;
/// Requests in flight at once, per model.
const MAX_PARALLEL: usize = 16;
/// The decision model alone flags a line below this probability of "ordinary".
const DECIDE_ALONE: f64 = 0.5;
/// Alongside the LLM it only adds lines it is this sure about.
const DECIDE_EXTRA: f64 = 0.05;

const PROMPT: &str = "Below are the text lines recognized in a screenshot someone wants to share, numbered. \
Find every piece of text that should be redacted first: passwords, API keys, tokens, recovery codes and other \
credentials; email addresses; phone numbers; postal addresses; government ID, bank account, IBAN and card numbers \
(and their CVV); dates of birth; people's names. Return only the sensitive part, copied exactly as it appears in \
the line, not its label (for 'Email: a@b.com' return 'a@b.com'); for a URL or connection string with credentials \
return the 'user:password' part. Ordinary UI text, code, dates of events, prices and reference numbers are not \
sensitive. Reply with only a JSON array of {\"line\": number, \"text\": string}; [] if nothing is sensitive.\n\n";

/// Categories for the decision model; anything but `ordinary` is sensitive.
const CRITERIA: [(&str, &str); 5] = [
    ("secret", "a password, API key, access token, recovery code or other credential"),
    ("contact", "an email address, phone number or postal address of a person or organization"),
    ("identity", "a person's name, date of birth, or government ID number"),
    ("financial", "a bank account, IBAN, payment card number or card security code"),
    ("ordinary", "ordinary interface, document or code text with no personal or secret information"),
];

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

/// Finds sensitive text in `lines` (in reading order, so chunks keep context)
/// with whichever models `settings` configures. Fails only when no request
/// succeeded; otherwise [`Report::failed`] says how many were lost.
pub fn find_sensitive(settings: &AiSettings, lines: &[String]) -> anyhow::Result<Report> {
    let (llm, decisions) = (settings.llm(), settings.decisions());
    if llm.is_none() && decisions.is_none() {
        bail!("no AI model is configured");
    }
    let errors = Mutex::new(Vec::new());
    let (from_llm, kinds) = std::thread::scope(|s| {
        let from_llm = llm.map(|e| s.spawn(|| ask_llm(e, lines, &errors)));
        let kinds = decisions.map(|e| s.spawn(|| classify(e, lines, &errors)));
        (from_llm.map(|h| h.join().expect("LLM worker panicked")), kinds.map(|h| h.join().expect("decision worker panicked")))
    });

    let mut findings = from_llm.clone().unwrap_or_default();
    if let Some(kinds) = &kinds {
        let threshold = if from_llm.is_some() { DECIDE_EXTRA } else { DECIDE_ALONE };
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
/// `image` pixels, any order): [`find_sensitive`] on the lines in reading
/// order, each finding placed with [`bluesnap_core::ink::place_span`].
pub fn locate_sensitive(
    settings: &AiSettings,
    image: &Pixmap,
    lines: &[(Rect, String)],
    fonts: &FontSet,
) -> anyhow::Result<(Vec<Rect>, Report)> {
    let boxes: Vec<Rect> = lines.iter().map(|(r, _)| *r).collect();
    // Reading order, so each request sees neighboring lines as context.
    let order: Vec<usize> = bluesnap_core::reading_order::rows(&boxes).into_iter().flatten().collect();
    let texts: Vec<String> = order.iter().map(|&i| lines[i].1.clone()).collect();
    let report = find_sensitive(settings, &texts)?;
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

fn ask_llm(endpoint: &Endpoint, lines: &[String], errors: &Mutex<Vec<String>>) -> Vec<Finding> {
    let chunks: Vec<Range<usize>> = (0..lines.len()).step_by(CHUNK).map(|i| i..(i + CHUNK).min(lines.len())).collect();
    parallel_map(&chunks, |chunk| {
        let numbered: Vec<String> = chunk.clone().map(|i| format!("{i}: {}", lines[i])).collect();
        match client::llm_reply(endpoint, &format!("{PROMPT}{}", numbered.join("\n"))) {
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
fn classify(endpoint: &Endpoint, lines: &[String], errors: &Mutex<Vec<String>>) -> Vec<Option<f64>> {
    // Framed as on-screen text: alone, table headers like "Email" read as
    // sensitive (9 false positives instead of 13 in the experiments).
    parallel_map(lines, |line| match client::decide(endpoint, &format!("Text found on screen: {line}"), &CRITERIA) {
        Ok(p) => Some(p.get("ordinary").copied().unwrap_or(1.0)),
        Err(e) => {
            errors.lock().expect("poisoned").push(format!("{e:#}"));
            None
        }
    })
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
            if let Some((other, range)) = chunk.clone().find_map(|i| Some((i, find_loose(&lines[i], text)?))) {
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

/// Byte range of `needle` in `hay`, ignoring whitespace and ASCII case (models
/// sometimes normalize spaces in long numbers).
fn find_loose(hay: &str, needle: &str) -> Option<Range<usize>> {
    let hay: Vec<(usize, char)> = hay.char_indices().filter(|(_, c)| !c.is_whitespace()).collect();
    let needle: Vec<char> = needle.chars().filter(|c| !c.is_whitespace()).collect();
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
    let hay: Vec<(usize, char)> = hay.char_indices().filter(|(_, c)| !c.is_whitespace()).collect();
    let needle: Vec<char> = needle.chars().filter(|c| !c.is_whitespace()).collect();
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
