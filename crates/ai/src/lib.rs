//! Optional AI services, off unless the user configures them:
//!
//! - [`settings`]: the endpoints (an LLM and a decision model), saved by the
//!   app's AI Settings window;
//! - [`client`]: calls to them, for any feature;
//! - features built on them: [`redact`] finds sensitive text to cover.
//!
//! Features send recognized text, never the screenshot itself.

pub mod client;
pub mod redact;
pub mod settings;

pub use client::{test_decisions, test_llm};
pub use redact::{Finding, Report, find_sensitive, locate_sensitive};
pub use settings::{AiSettings, Endpoint};
