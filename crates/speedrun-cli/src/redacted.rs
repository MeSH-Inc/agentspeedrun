//! Values that have passed through the shared redactor, so downstream code
//! cannot accidentally handle raw secret-bearing prompt text.

/// Prompt text that is safe to send or spool. The only constructor runs
/// `speedrun_core::redact`, so a request body or spool entry built from this
/// value is redacted by construction.
#[derive(Debug, Clone)]
pub struct RedactedText(String);

impl RedactedText {
    pub fn new(raw: &str) -> Self {
        let (text, _changed) = speedrun_core::redact::redact(raw);
        Self(text)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
