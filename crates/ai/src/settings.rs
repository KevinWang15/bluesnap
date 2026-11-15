//! The user's AI endpoints, stored as JSON next to the main config.

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Where one model is served and how to call it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Base URL such as `https://api.example.com/v1`, or the full endpoint URL.
    pub url: String,
    /// Sent as a bearer token.
    pub token: String,
    pub model: String,
}

impl Endpoint {
    /// Usable when URL and model are set (some local servers need no token).
    pub fn is_complete(&self) -> bool {
        !self.url.trim().is_empty() && !self.model.trim().is_empty()
    }

    /// The full URL of `path` (like `responses`) under the base URL, unless the
    /// URL already ends with it.
    pub(crate) fn endpoint_url(&self, path: &str) -> String {
        let base = self.url.trim().trim_end_matches('/');
        if base.ends_with(&format!("/{path}")) {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/{path}")
        } else {
            format!("{base}/v1/{path}")
        }
    }
}

/// Both endpoints are optional; AI features are offered when either is set.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    /// An LLM behind an OpenAI-compatible Responses API (`/v1/responses`).
    pub llm: Option<Endpoint>,
    /// A decision model behind a Decisions API (`/v1/decisions`).
    pub decisions: Option<Endpoint>,
}

impl AiSettings {
    pub fn is_configured(&self) -> bool {
        self.llm().is_some() || self.decisions().is_some()
    }

    pub fn llm(&self) -> Option<&Endpoint> {
        self.llm.as_ref().filter(|e| e.is_complete())
    }

    pub fn decisions(&self) -> Option<&Endpoint> {
        self.decisions.as_ref().filter(|e| e.is_complete())
    }

    /// Reads `path`; a missing file means nothing is configured.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("invalid {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Writes `path`, readable only by the user since it holds API tokens.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        write_private(&tmp, json.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(data)
}

/// On Windows the per-user config directory is already private to the user.
#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_urls_accept_base_or_full_urls() {
        let e = |url: &str| Endpoint { url: url.into(), token: String::new(), model: "m".into() };
        assert_eq!(e("https://h.example").endpoint_url("responses"), "https://h.example/v1/responses");
        assert_eq!(e("https://h.example/v1/").endpoint_url("responses"), "https://h.example/v1/responses");
        assert_eq!(e("https://h.example/v1/decisions").endpoint_url("decisions"), "https://h.example/v1/decisions");
    }

    #[test]
    fn settings_round_trip_and_incomplete_endpoints_are_off() {
        let dir = std::env::temp_dir().join(format!("bluesnap-ai-test-{}", std::process::id()));
        let path = dir.join("ai.json");
        assert_eq!(AiSettings::load(&path).unwrap(), AiSettings::default());
        let settings = AiSettings {
            llm: Some(Endpoint { url: "https://h.example/v1".into(), token: "t".into(), model: "m".into() }),
            decisions: Some(Endpoint { url: String::new(), token: String::new(), model: "d".into() }),
        };
        settings.save(&path).unwrap();
        let loaded = AiSettings::load(&path).unwrap();
        assert_eq!(loaded, settings);
        assert!(loaded.llm().is_some() && loaded.decisions().is_none() && loaded.is_configured());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
