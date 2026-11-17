//! Optional AI services, off unless the user configures them:
//!
//! - [`settings`]: the endpoints (an LLM and a decision model), saved by the
//!   app's AI Settings window;
//! - [`client`]: calls to them, for any feature;
//! - features built on them: [`redact`] analyzes screen context in parallel
//!   with local OCR and finds sensitive text to cover.

pub mod client;
pub mod redact;
pub mod settings;

pub use client::{test_decisions, test_llm};
pub use redact::{
    ContextOutcome, Finding, Report, describe_image_context, describe_text_context, find_sensitive,
    find_sensitive_with_context, locate_sensitive, locate_sensitive_with_context,
};
pub use settings::{AiSettings, Endpoint};
