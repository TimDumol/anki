// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use std::fmt;

use serde::Serialize;
use serde_json::Value;

/// Public errors contain only deliberate messages, never upstream response
/// bodies.
#[derive(Clone, Debug, Serialize)]
pub struct Failure {
    pub code: &'static str,
    pub message: &'static str,
    pub retryable: bool,
    pub details: Value,
}

impl Failure {
    pub fn new(code: &'static str, message: &'static str, retryable: bool) -> Self {
        Self {
            code,
            message,
            retryable,
            details: Value::Null,
        }
    }

    pub fn details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    pub fn from_error(error: &anyhow::Error) -> Self {
        error.downcast_ref::<Self>().cloned().unwrap_or_else(|| {
            Self::new(
                "operation_failed",
                "Operation failed; inspect collection availability and input.",
                false,
            )
        })
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for Failure {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_errors_preserve_codes_without_exposing_upstream_bodies() {
        let upstream = anyhow::anyhow!("remote response with credentials and private content");
        let public = Failure::from_error(&upstream);
        assert_eq!(public.code, "operation_failed");
        let encoded = serde_json::to_string(&public).unwrap();
        assert!(!encoded.contains("credentials"));
        assert!(!encoded.contains("private content"));
        let typed = anyhow::Error::from(Failure::new(
            "stale_revision",
            "Fetch the note again.",
            false,
        ))
        .context("internal context that should stay private");
        assert_eq!(Failure::from_error(&typed).code, "stale_revision");
    }
}
