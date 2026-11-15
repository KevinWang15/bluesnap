//! Optional AI features, off unless the user configures an endpoint: finding
//! sensitive text in a screenshot so the editor can redact it automatically.
//!
//! Only recognized text is sent, never the image.

mod client;
pub mod redact;
pub mod settings;

pub use client::{test_decisions, test_llm};
pub use redact::{Finding, Report, find_sensitive, locate_sensitive};
pub use settings::{AiSettings, Endpoint};
