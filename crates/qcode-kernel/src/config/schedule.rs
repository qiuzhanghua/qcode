//! `[schedules.<name>]`: scheduled turns declared in the config file
//! (ADR-0059 §1).
//!
//! The declaration lives here; the *runtime* state (`last_fired_at`,
//! `next_fire_at`, `last_status`) lives in the store, never in this file. The
//! config is watched by mtime, so a scheduler that wrote back here would rebuild
//! the engine on every fire and race with the Settings window.
//!
//! Keys are all optional so that a file written for a newer qcode still loads,
//! and a malformed entry is reported by [`resolve_schedules`] instead of being
//! fatal: one bad cron must not stop the window from starting.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use qcode_core::schedule::{CronSpec, Overlap, Schedule};

/// One `[schedules.<name>]` table, exactly as written.
#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleConfig {
    /// Five-field cron in the user's local time zone.
    #[serde(default)]
    pub cron: String,
    /// Reuse this existing task.
    #[serde(default)]
    pub task: Option<String>,
    /// Create a fresh task on every fire (instead of `task`).
    #[serde(default)]
    pub new: bool,
    #[serde(default)]
    pub prompt: String,
    /// `queue` (default) or `skip`.
    #[serde(default)]
    pub overlap: Option<String>,
    /// How long a scheduled turn's approval may sit unanswered, e.g. `30m`.
    /// `0` (the default) waits for a human.
    #[serde(default)]
    pub approval_timeout: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            cron: String::new(),
            task: None,
            new: false,
            prompt: String::new(),
            overlap: None,
            approval_timeout: None,
            enabled: true,
        }
    }
}

/// Why one entry could not be turned into a running [`Schedule`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleConfigError {
    #[error("schedule `{name}`: {source}")]
    Cron {
        name: String,
        #[source]
        source: qcode_core::schedule::CronError,
    },
    #[error("schedule `{name}`: prompt must not be empty")]
    EmptyPrompt { name: String },
    #[error("schedule `{name}`: overlap must be `queue` or `skip`, got `{value}`")]
    Overlap { name: String, value: String },
    #[error("schedule `{name}`: approval_timeout `{value}` is not a duration like `30m`")]
    ApprovalTimeout { name: String, value: String },
    #[error("schedule `{name}`: set either `task` or `new = true`, not neither")]
    NoTarget { name: String },
    #[error("schedule `{name}`: `task` and `new = true` are mutually exclusive")]
    BothTargets { name: String },
}

impl ScheduleConfigError {
    /// The entry this error is about; the sidebar groups by it.
    pub fn name(&self) -> &str {
        match self {
            Self::Cron { name, .. }
            | Self::EmptyPrompt { name }
            | Self::Overlap { name, .. }
            | Self::ApprovalTimeout { name, .. }
            | Self::NoTarget { name }
            | Self::BothTargets { name } => name,
        }
    }
}

/// Turn the declaration into what the host runs, and collect the entries that
/// cannot be run.
///
/// Errors are per entry on purpose: a typo in one cron leaves the other
/// schedules working, and the host can show the broken one in the sidebar
/// instead of silently dropping it.
pub fn resolve_schedules(
    entries: &BTreeMap<String, ScheduleConfig>,
) -> (Vec<Schedule>, Vec<ScheduleConfigError>) {
    let mut schedules = Vec::with_capacity(entries.len());
    let mut errors = Vec::new();
    for (name, entry) in entries {
        match resolve_one(name, entry) {
            Ok(schedule) => schedules.push(schedule),
            Err(error) => errors.push(error),
        }
    }
    (schedules, errors)
}

fn resolve_one(name: &str, entry: &ScheduleConfig) -> Result<Schedule, ScheduleConfigError> {
    let cron = CronSpec::parse(&entry.cron).map_err(|source| ScheduleConfigError::Cron {
        name: name.to_string(),
        source,
    })?;
    if entry.prompt.trim().is_empty() {
        return Err(ScheduleConfigError::EmptyPrompt {
            name: name.to_string(),
        });
    }
    let overlap = match entry.overlap.as_deref() {
        None => Overlap::default(),
        Some(text) => Overlap::parse(text).ok_or_else(|| ScheduleConfigError::Overlap {
            name: name.to_string(),
            value: text.to_string(),
        })?,
    };
    let approval_timeout = match entry.approval_timeout.as_deref() {
        None => None,
        Some(text) => parse_timeout(text).ok_or_else(|| ScheduleConfigError::ApprovalTimeout {
            name: name.to_string(),
            value: text.to_string(),
        })?,
    };
    let task = match (entry.task.as_deref(), entry.new) {
        (Some(_), true) => {
            return Err(ScheduleConfigError::BothTargets {
                name: name.to_string(),
            });
        }
        (Some(id), false) => Some(id.to_string()),
        (None, true) => None,
        (None, false) => {
            return Err(ScheduleConfigError::NoTarget {
                name: name.to_string(),
            });
        }
    };
    Ok(Schedule {
        name: name.to_string(),
        cron,
        cron_text: entry.cron.clone(),
        task,
        prompt: entry.prompt.clone(),
        overlap,
        approval_timeout,
        enabled: entry.enabled,
    })
}

/// `30m`, `90s`, `2h`, `1d`, or `0` for "wait forever".
///
/// A zero timeout is `None`, not `Duration::ZERO`: the engine treats a zero
/// deadline as "already expired", which would deny every scheduled approval
/// instantly.
fn parse_timeout(text: &str) -> Option<Option<Duration>> {
    let text = text.trim();
    if let Ok(seconds) = text.parse::<u64>() {
        return Some((seconds > 0).then(|| Duration::from_secs(seconds)));
    }
    let (number, unit) = text.split_at(text.len().checked_sub(1)?);
    let amount: u64 = number.trim().parse().ok()?;
    let unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return None,
    };
    let seconds = amount.checked_mul(unit)?;
    Some((seconds > 0).then(|| Duration::from_secs(seconds)))
}

#[cfg(test)]
mod tests;
