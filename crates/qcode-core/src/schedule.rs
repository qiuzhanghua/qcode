//! Cron schedules: parsing, matching and the "is it due yet" decision
//! (ADR-0059).
//!
//! Deliberately free of clocks, files and the store: every function here takes
//! the current **wall clock** as an argument, so the rules can be tested with a
//! fake time. The host keeps a monotonic instant for "has this window come yet"
//! and uses the wall clock only for "when is the next one" — a laptop that
//! sleeps for a night must not turn one schedule into eleven turns (ADR-0059
//! §3), and a machine whose clock is corrected backwards must not replay them
//! either.
//!
//! Local time is the caller's business: `next_after` works on
//! [`PrimitiveDateTime`] wall clock and never constructs an instant. Whoever
//! owns the calendar (the GUI process, later `qc daemon`) decides which offset
//! "local" means.

use std::fmt;
use std::time::Duration;

use time::Duration as TimeSpan;
use time::{Date, Month, PrimitiveDateTime, Time, Weekday};

/// Five fields, like `crontab(5)`: minute, hour, day of month, month,
/// day of week.
const FIELDS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CronError {
    #[error("a schedule needs 5 fields (minute hour day month weekday), got {0}")]
    FieldCount(usize),
    #[error("cannot parse `{field}`: {reason}")]
    Field { field: String, reason: String },
    #[error("value {value} is out of range {min}..={max} in `{field}`")]
    Range {
        field: String,
        value: u32,
        min: u32,
        max: u32,
    },
}

/// One field of a cron expression: a set of allowed values plus whether it was
/// written as `*`.
///
/// The distinction matters for day of month / day of week: `crontab(5)` treats
/// `0 3 1 * *` as "the 1st" and `0 3 * * 1` as "every Monday", but
/// `0 3 1 * 1` as "the 1st **or** any Monday" — the star is what makes a field
/// not count.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    name: &'static str,
    min: u32,
    max: u32,
    allowed: Vec<bool>,
    starred: bool,
}

impl Field {
    fn parse(
        name: &'static str,
        spec: &str,
        min: u32,
        max: u32,
        names: &[&str],
    ) -> Result<Self, CronError> {
        let span = (max - min + 1) as usize;
        let mut allowed = vec![false; span];
        let field = spec.trim();
        if field.is_empty() {
            return Err(CronError::Field {
                field: spec.to_string(),
                reason: "empty".to_string(),
            });
        }
        let starred = field == "*" || field.starts_with("*/");
        for part in field.split(',') {
            let (range, step) = match part.split_once('/') {
                Some((range, step)) => {
                    let step: u32 = step.parse().map_err(|_| CronError::Field {
                        field: spec.to_string(),
                        reason: format!("`{step}` is not a step"),
                    })?;
                    if step == 0 {
                        return Err(CronError::Field {
                            field: spec.to_string(),
                            reason: "step must be > 0".to_string(),
                        });
                    }
                    (range, step)
                }
                None => (part, 1),
            };
            let (low, high) = match range {
                "*" => (min, max),
                _ => match range.split_once('-') {
                    Some((low, high)) => (
                        value(low, min, max, names, spec)?,
                        value(high, min, max, names, spec)?,
                    ),
                    None => {
                        let at = value(range, min, max, names, spec)?;
                        // `5/15` in crontab means "from 5 to the end, every 15".
                        (at, if step > 1 { max } else { at })
                    }
                },
            };
            if low > high {
                return Err(CronError::Field {
                    field: spec.to_string(),
                    reason: format!("{low}-{high} is backwards"),
                });
            }
            let mut at = low;
            while at <= high {
                allowed[(at - min) as usize] = true;
                at += step;
            }
        }
        Ok(Self {
            name,
            min,
            max,
            allowed,
            starred,
        })
    }

    fn matches(&self, value: u32) -> bool {
        let index = value.wrapping_sub(self.min) as usize;
        self.allowed.get(index).copied().unwrap_or(false)
    }
}

/// `12`, `JAN`, `mon`, … → number.
fn value(text: &str, min: u32, max: u32, names: &[&str], field: &str) -> Result<u32, CronError> {
    let parsed = if let Ok(number) = text.trim().parse::<u32>() {
        number
    } else {
        let lower = text.trim().to_ascii_lowercase();
        names
            .iter()
            .position(|name| *name == lower)
            .map(|index| index as u32 + min)
            .ok_or_else(|| CronError::Field {
                field: field.to_string(),
                reason: format!("`{text}` is not a number or a known name"),
            })?
    };
    if parsed < min || parsed > max {
        return Err(CronError::Range {
            field: field.to_string(),
            value: parsed,
            min,
            max,
        });
    }
    Ok(parsed)
}

const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// A parsed five-field cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSpec {
    minute: Field,
    hour: Field,
    day: Field,
    month: Field,
    /// 0 = Sunday, as in `crontab(5)`; 7 is accepted and folded to 0.
    weekday: Field,
}

impl CronSpec {
    /// Parse `"0 3 * * *"`. Names and `7` for Sunday are accepted.
    pub fn parse(expr: &str) -> Result<Self, CronError> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != FIELDS {
            return Err(CronError::FieldCount(parts.len()));
        }
        let mut weekday = Field::parse("weekday", parts[4], 0, 7, &DAY_NAMES)?;
        // `crontab(5)` accepts 7 as Sunday; fold it onto 0 so matching only has
        // to know one spelling.
        if weekday.allowed[7] {
            weekday.allowed[0] = true;
        }
        Ok(Self {
            minute: Field::parse("minute", parts[0], 0, 59, &[])?,
            hour: Field::parse("hour", parts[1], 0, 23, &[])?,
            day: Field::parse("day", parts[2], 1, 31, &[])?,
            month: Field::parse("month", parts[3], 1, 12, &MONTH_NAMES)?,
            weekday,
        })
    }

    /// Whether `at` (to the minute) is one of this expression's times.
    ///
    /// Day of month and day of week follow `crontab(5)`: if **both** are
    /// restricted, either may match.
    pub fn matches(&self, at: PrimitiveDateTime) -> bool {
        if !self.minute.matches(at.minute() as u32)
            || !self.hour.matches(at.hour() as u32)
            || !self.month.matches(at.month() as u32)
        {
            return false;
        }
        let day_of_month = self.day.matches(at.day() as u32);
        let day_of_week = self.weekday.matches(weekday_of(at.weekday()));
        let day_of_month_starred = self.day.starred;
        let day_of_week_starred = self.weekday.starred;
        match (day_of_month_starred, day_of_week_starred) {
            _ if day_of_month_starred && day_of_week_starred => true,
            _ if day_of_month_starred => day_of_week,
            _ if day_of_week_starred => day_of_month,
            _ => day_of_month || day_of_week,
        }
    }

    /// The first time strictly after `after`, at minute resolution.
    ///
    /// Scans minute by minute, which is at most `4 years` of steps for a
    /// pathological expression like `0 0 30 2 *` (never fires) — `None` there.
    pub fn next_after(&self, after: PrimitiveDateTime) -> Option<PrimitiveDateTime> {
        // Start at the next minute; cron fires at :00 seconds.
        let mut at = (after + TimeSpan::minutes(1)).replace_second(0).ok()?;
        // Four years covers every real expression: the worst case is a date
        // that only exists on a leap day, `0 0 29 2 *`.
        for _ in 0..(4 * 366 * 24 * 60) {
            if self.matches(at) {
                return Some(at);
            }
            at = (at + TimeSpan::minutes(1)).replace_second(0).ok()?;
        }
        None
    }
}

fn weekday_of(day: Weekday) -> u32 {
    match day {
        Weekday::Sunday => 0,
        Weekday::Monday => 1,
        Weekday::Tuesday => 2,
        Weekday::Wednesday => 3,
        Weekday::Thursday => 4,
        Weekday::Friday => 5,
        Weekday::Saturday => 6,
    }
}

/// What a scheduled turn does when the session is already busy (ADR-0059 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overlap {
    /// Wait for the current turn, then run (the default).
    #[default]
    Queue,
    /// Drop this occurrence and record `schedule_skipped`.
    Skip,
}

impl Overlap {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "queue" => Some(Self::Queue),
            "skip" => Some(Self::Skip),
            _ => None,
        }
    }
}

/// One `[schedules.<name>]` entry, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub name: String,
    pub cron: CronSpec,
    /// Reuse an existing task, or create a fresh one on every fire.
    pub task: Option<String>,
    pub prompt: String,
    pub overlap: Overlap,
    /// How long a scheduled turn's approval may sit unanswered before it is
    /// denied (ADR-0059 §4). `None` = wait forever (a human is around).
    pub approval_timeout: Option<Duration>,
    pub enabled: bool,
}

impl Schedule {
    /// A fresh task per fire instead of reusing `task`.
    pub fn creates_task(&self) -> bool {
        self.task.is_none()
    }
}

impl fmt::Display for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

/// What the store remembers between process runs (ADR-0059 §1).
///
/// Never written back to the config file: the config is watched by mtime, so a
/// scheduler that wrote there would rebuild the engine on every fire and fight
/// with the Settings window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScheduleState {
    /// Wall clock of the last window that produced a decision.
    pub last_fired_at: Option<PrimitiveDateTime>,
    /// Wall clock of the next window. `< now` means the process was away.
    pub next_fire_at: Option<PrimitiveDateTime>,
    pub last_status: Option<ScheduleStatus>,
    /// Windows that passed while the process was not running. Recorded, never
    /// replayed (ADR-0059 §3).
    pub missed_windows: u32,
}

/// Why the last window did or did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleStatus {
    /// The turn was submitted.
    Fired,
    /// The session was busy and `overlap = "skip"`.
    Skipped,
    /// The task the entry points at no longer exists.
    Orphaned,
    /// Windows that passed while nothing was running.
    Missed(u32),
    /// The prompt could not be submitted (no session, store error, …).
    Failed(String),
}

impl ScheduleStatus {
    pub fn as_str(&self) -> String {
        match self {
            Self::Fired => "fired".to_string(),
            Self::Skipped => "skipped".to_string(),
            Self::Orphaned => "orphaned".to_string(),
            Self::Missed(windows) => format!("missed:{windows}"),
            Self::Failed(reason) => format!("failed:{reason}"),
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "fired" => Some(Self::Fired),
            "skipped" => Some(Self::Skipped),
            "orphaned" => Some(Self::Orphaned),
            other => match other.split_once(':') {
                Some(("missed", count)) => count.parse().ok().map(Self::Missed),
                Some(("failed", reason)) => Some(Self::Failed(reason.to_string())),
                _ => None,
            },
        }
    }
}

/// What the host should do with an entry right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    /// Its window has not come yet.
    NotYet,
    /// Submit `prompt` to `task` (or to a new task).
    Fire,
    /// The window came and the session was already busy: `overlap = "skip"`.
    SkipBusy,
    /// `task` names a session that is gone; disable the entry (ADR-0059 §2).
    Orphaned,
}

/// Decide one entry's next action.
///
/// `now` is wall clock; `busy` is the session's live phase from the host. An
/// entry with no `next_fire_at` is being seen for the first time and only gets
/// one scheduled (it must not fire immediately just because the process
/// started).
pub fn tick(
    schedule: &Schedule,
    state: &ScheduleState,
    now: PrimitiveDateTime,
    busy: bool,
    task_exists: bool,
) -> Tick {
    if !schedule.enabled {
        return Tick::NotYet;
    }
    let Some(next) = state.next_fire_at else {
        return Tick::NotYet;
    };
    if now < next {
        return Tick::NotYet;
    }
    if schedule.task.is_some() && !task_exists {
        return Tick::Orphaned;
    }
    if busy && schedule.overlap == Overlap::Skip {
        return Tick::SkipBusy;
    }
    Tick::Fire
}

/// What [`recover`] found when an entry was loaded (or after a long tick gap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    /// Windows that came and went while nothing was running.
    pub missed: u32,
    /// The next window to wait for.
    pub next_fire_at: Option<PrimitiveDateTime>,
}

/// Catch up with the calendar **without** running anything (ADR-0059 §3).
///
/// A laptop that was asleep for a night has one entry with eleven windows
/// behind it; firing eleven turns at 9am would burn tokens and fight over the
/// session lock. So the windows are counted, recorded as `missed`, and the next
/// one is scheduled from `now`.
pub fn recover(schedule: &Schedule, state: &ScheduleState, now: PrimitiveDateTime) -> Recovery {
    let Some(next) = state.next_fire_at else {
        return Recovery {
            missed: 0,
            next_fire_at: schedule.cron.next_after(now),
        };
    };
    if next > now {
        return Recovery {
            missed: 0,
            next_fire_at: Some(next),
        };
    }
    // Count the windows that passed. `MAX_MISSED` keeps a clock that jumped a
    // decade back from turning into a million-step loop.
    const MAX_MISSED: u32 = 1_000;
    let mut missed = 0;
    let mut at = next;
    while at <= now && missed < MAX_MISSED {
        missed += 1;
        match schedule.cron.next_after(at) {
            Some(following) => at = following,
            None => break,
        }
    }
    Recovery {
        missed: state.missed_windows.saturating_add(missed),
        next_fire_at: schedule.cron.next_after(now),
    }
}

/// The window to record after a decision: the one we just handled is behind us.
pub fn advance(
    schedule: &Schedule,
    now: PrimitiveDateTime,
    status: ScheduleStatus,
) -> ScheduleState {
    ScheduleState {
        last_fired_at: Some(now.replace_second(0).unwrap_or(now)),
        next_fire_at: schedule.cron.next_after(now),
        last_status: Some(status),
        missed_windows: 0,
    }
}

/// A wall clock at minute resolution, for the tests and the host alike.
pub fn at(year: i32, month: Month, day: u8, hour: u8, minute: u8) -> PrimitiveDateTime {
    let date = Date::from_calendar_date(year, month, day).expect("valid date");
    let time = Time::from_hms(hour, minute, 0).expect("valid time");
    PrimitiveDateTime::new(date, time)
}

#[cfg(test)]
mod tests;
