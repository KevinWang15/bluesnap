//! Calls to the two APIs: an OpenAI-compatible Responses API for the LLM and a
//! Decisions API for the decision model. Blocking; callers fan out on threads.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};

use crate::settings::Endpoint;

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(45)))
            .timeout_connect(Some(Duration::from_secs(10)))
            // Error bodies carry the useful message; read them like any other.
            .http_status_as_error(false)
            .tls_config(ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build())
            .build()
            .into()
    })
}

/// A failed call, with the server's status (0 when the request never got one).
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            0 => write!(f, "{}", self.message),
            s => write!(f, "HTTP {s}: {}", self.message),
        }
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// Overload worth retrying shortly (the decisions queue fills under bursts).
    fn is_busy(&self) -> bool {
        matches!(self.status, 429 | 502 | 503)
    }
}

fn post(endpoint: &Endpoint, path: &str, body: &Value) -> Result<Value, ApiError> {
    let url = endpoint.endpoint_url(path);
    let mut request = agent().post(&url).header("Content-Type", "application/json");
    if !endpoint.token.trim().is_empty() {
        request = request.header("Authorization", format!("Bearer {}", endpoint.token.trim()));
    }
    let mut response =
        request.send(body.to_string()).map_err(|e| ApiError { status: 0, message: format!("{url}: {e}") })?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| ApiError { status, message: format!("reading the response: {e}") })?;
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let message = error_message(&value).unwrap_or_else(|| text.chars().take(200).collect());
        return Err(ApiError { status, message });
    }
    Ok(value)
}

/// `{"error": {"message": …}}`, `{"error": "…"}` or `{"message": …}`.
fn error_message(v: &Value) -> Option<String> {
    let e = v.get("error").unwrap_or(v);
    e.get("message").or(Some(e)).and_then(Value::as_str).map(str::to_string)
}

/// Retries `f` while the server is busy, backing off 0.1, 0.2, 0.4 … s.
fn with_retries<T>(mut f: impl FnMut() -> Result<T, ApiError>) -> Result<T, ApiError> {
    let mut delay = Duration::from_millis(100);
    for _ in 0..5 {
        match f() {
            Err(e) if e.is_busy() => std::thread::sleep(delay),
            other => return other,
        }
        delay *= 2;
    }
    f()
}

/// The LLM's text reply to `prompt`. Asks for no reasoning, which the
/// experiments showed is ~10x faster for this task at the same quality; servers
/// that reject the option get the request again without it.
pub fn llm_reply(endpoint: &Endpoint, prompt: &str) -> anyhow::Result<String> {
    let mut body = json!({
        "model": endpoint.model.trim(),
        "input": [{"role": "user", "content": [{"type": "input_text", "text": prompt}]}],
        "reasoning": {"effort": "none"},
    });
    let reply = match with_retries(|| post(endpoint, "responses", &body)) {
        Err(e) if e.status == 400 && e.message.to_lowercase().contains("reasoning") => {
            body.as_object_mut().expect("object").remove("reasoning");
            with_retries(|| post(endpoint, "responses", &body))
        }
        other => other,
    }?;
    output_text(&reply).ok_or_else(|| anyhow!("the response has no text output"))
}

/// The text of a Responses API reply: `output_text`, else the message parts.
fn output_text(reply: &Value) -> Option<String> {
    if let Some(text) = reply.get("output_text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    let text: String = reply
        .get("output")?
        .as_array()?
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .flat_map(|item| item.get("content").and_then(Value::as_array).into_iter().flatten())
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect();
    Some(text)
}

/// Asks the decision model which of `criteria` (name → description) best
/// describes `state`; returns each name's probability.
pub fn decide(endpoint: &Endpoint, state: &str, criteria: &[(&str, &str)]) -> anyhow::Result<HashMap<String, f64>> {
    let criteria: serde_json::Map<String, Value> =
        criteria.iter().map(|(name, description)| (name.to_string(), json!(description))).collect();
    let body = json!({
        "model": endpoint.model.trim(),
        "state": state,
        "questions": {"kind": {"kind": "choice", "criteria": criteria}},
    });
    let reply = with_retries(|| post(endpoint, "decisions", &body))?;
    let probabilities = reply
        .pointer("/answers/kind/probabilities")
        .and_then(Value::as_object)
        .context("the decision has no probabilities")?;
    Ok(probabilities.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))).collect())
}

/// A minimal request to check an endpoint's settings.
pub fn test_llm(endpoint: &Endpoint) -> anyhow::Result<()> {
    let reply = llm_reply(endpoint, "Reply with the word OK.")?;
    if reply.trim().is_empty() {
        bail!("the model returned an empty reply");
    }
    Ok(())
}

pub fn test_decisions(endpoint: &Endpoint) -> anyhow::Result<()> {
    decide(endpoint, "hello", &[("greeting", "a greeting"), ("other", "anything else")]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_text_from_responses_api_replies() {
        let reply = json!({"output": [
            {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "hmm"}]},
            {"type": "message", "content": [{"type": "output_text", "text": "[1,"}, {"type": "output_text", "text": "2]"}]},
        ]});
        assert_eq!(output_text(&reply).as_deref(), Some("[1,2]"));
        assert_eq!(output_text(&json!({"output_text": "hi"})).as_deref(), Some("hi"));
    }

    #[test]
    fn error_messages_from_common_shapes() {
        assert_eq!(error_message(&json!({"error": {"message": "bad key"}})).as_deref(), Some("bad key"));
        assert_eq!(error_message(&json!({"error": "queue is full"})).as_deref(), Some("queue is full"));
        assert_eq!(error_message(&json!({"message": "nope"})).as_deref(), Some("nope"));
    }
}
