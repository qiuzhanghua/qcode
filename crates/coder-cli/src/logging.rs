//! stderr logging and secret redaction.
//!
//! The JSONL event stream on stdout is a machine interface and must stay clean;
//! all diagnostics go to stderr. `RUST_LOG` controls verbosity (default `warn`).

use tracing_subscriber::EnvFilter;

pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .try_init();
}

/// Masks known secret values in any string that is about to be logged or shown.
///
/// This is defense in depth: the code never logs credentials on purpose, but
/// provider errors and future debug output can carry arbitrary response bodies.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut secrets: Vec<String> = secrets
            .into_iter()
            .filter(|secret| secret.len() >= 4)
            .collect();
        // Longest first so overlapping secrets are fully masked.
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        Self { secrets }
    }

    pub fn redact(&self, input: &str) -> String {
        let mut out = input.to_string();
        for secret in &self.secrets {
            out = out.replace(secret.as_str(), "<redacted>");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_known_secret() {
        let redactor = Redactor::new(["sk-secret-1234".to_string()]);
        assert_eq!(
            redactor.redact("auth failed with sk-secret-1234"),
            "auth failed with <redacted>"
        );
    }

    #[test]
    fn ignores_tiny_values() {
        let redactor = Redactor::new(["ab".to_string()]);
        assert_eq!(redactor.redact("ab"), "ab");
    }

    #[test]
    fn masks_longest_first() {
        let redactor = Redactor::new(["abc".to_string(), "abcdef".to_string()]);
        assert_eq!(redactor.redact("abcdef"), "<redacted>");
    }
}
