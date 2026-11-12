//! Masking credentials in text that is about to be logged or shown.
//!
//! Defense in depth. Nothing in qcode logs a key on purpose, but a provider error
//! body, a `Debug` impl or a URL with the key in its query string can carry one,
//! and since ADR-0077 those lines land in a file that survives for a week.
//!
//! The set lives here, in `qcode-kernel`, for one reason: [`assemble`] is the only
//! place every frontend passes through, and it is where the key becomes known.
//! Installing there means `qc acp` cannot be forgotten the way it was when the two
//! call sites each installed it themselves.
//!
//! [`assemble`]: crate::assemble

use std::sync::{Arc, OnceLock, RwLock};

/// Masks known secret values in any string that is about to be logged or shown.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut secrets: Vec<String> = secrets
            .into_iter()
            // Shorter than this and the replacement is indistinguishable from
            // vandalism: a two-character "secret" would eat every second word.
            .filter(|secret| secret.len() >= 4)
            .collect();
        // Longest first, because a shorter secret can be a substring of a longer
        // one: masking `abcdef` before `sk-abcdef` would leave `sk-<redacted>`.
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

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    pub fn secrets(&self) -> &[String] {
        &self.secrets
    }
}

static REDACTOR: OnceLock<RwLock<Arc<Redactor>>> = OnceLock::new();

fn cell() -> &'static RwLock<Arc<Redactor>> {
    REDACTOR.get_or_init(|| RwLock::new(Arc::new(Redactor::default())))
}

/// The current set, cloned so the lock is not held while formatting.
///
/// A poisoned lock is recovered rather than obeyed: the poison means some thread
/// panicked, and treating that as "no secrets" would quietly turn redaction off
/// for the rest of the process.
pub fn current() -> Arc<Redactor> {
    let cell = cell();
    let guard = cell.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(&guard)
}

/// Register secrets for redaction everywhere.
///
/// The set only ever grows. A `qc gui` window that re-resolves after a settings
/// change may hold a *new* key, but a string captured moments earlier may still
/// contain the **old** one, so forgetting it would re-open exactly the leak this
/// exists to close.
pub fn install(secrets: impl IntoIterator<Item = String>) {
    let cell = cell();
    let mut guard = cell
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut all = guard.secrets().to_vec();
    all.extend(secrets);
    *guard = Arc::new(Redactor::new(all));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Longest first, or the shorter secret eats the longer one's prefix and the
    /// mask becomes recognisable: `sk-<redacted>` still hands over the shape.
    ///
    /// Registered shortest-first on purpose — the order the caller happens to pass
    /// them in must not decide how well the mask works.
    #[test]
    fn overlapping_secrets_are_masked_longest_first() {
        let redactor = Redactor::new(["abcdefgh".to_string(), "sk-abcdefgh".to_string()]);
        assert_eq!(redactor.redact("sk-abcdefgh"), "<redacted>");
        assert_eq!(redactor.redact("abcdefgh"), "<redacted>");
    }

    /// Every occurrence, not just the first: a key repeated in a message and in a
    /// field would otherwise survive in the second place.
    #[test]
    fn every_occurrence_is_masked() {
        let redactor = Redactor::new(["sk-abcdefgh".to_string()]);
        assert_eq!(
            redactor.redact("sk-abcdefgh and sk-abcdefgh"),
            "<redacted> and <redacted>"
        );
    }

    /// A one- or two-character "secret" would replace fragments of ordinary words;
    /// refusing them keeps the log readable when a key is misconfigured as empty.
    #[test]
    fn secrets_too_short_to_be_real_are_ignored() {
        let redactor = Redactor::new(["ab".to_string(), String::new()]);
        assert_eq!(redactor.redact("about a table"), "about a table");
        assert!(redactor.is_empty());
    }

    /// [`install`] runs again after a settings change, and the old key may still be
    /// in a string captured before it. Forgetting it would re-open the leak.
    #[test]
    fn installing_again_keeps_the_earlier_secrets() {
        const OLD: &str = "sk-old-REVOKED-BUT-STILL-IN-MEMORY";
        const NEW: &str = "sk-new-CURRENT";
        install([OLD.to_string()]);
        install([NEW.to_string()]);

        let redactor = current();
        assert_eq!(
            redactor.redact(OLD),
            "<redacted>",
            "the earlier key was forgotten"
        );
        assert_eq!(redactor.redact(NEW), "<redacted>");
    }
}
