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

use serde::{Deserialize, Serialize};
use time::Duration as TimeSpan;
use time::{Date, Month, Time, Weekday};

/// A scheduled-task delivery, riding on the user message it turned into
/// (ADR-0059). Lets the UI render it as a distinct "scheduled execution" card
/// and keeps it from masquerading as a typed message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledMessage {
    pub task_id: String,
    pub title: String,
    pub prompt: String,
    /// The wall-clock occurrence this delivery represents (`YYYY-MM-DDTHH:MM:SS`).
    pub at: String,
}

/// `time::serde::rfc3339` / `iso8601` 只支持带时区的 `OffsetDateTime`；墙钟时刻
/// (`PrimitiveDateTime`) 用自定义字符串编解码：`YYYY-MM-DDTHH:MM:SS`。
mod serde_wall_clock {
    use serde::Deserialize;
    use time::{Date, Month, PrimitiveDateTime, Time};

    pub fn serialize<S>(value: &PrimitiveDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&format(value))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<PrimitiveDateTime, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        parse(&text).ok_or_else(|| serde::de::Error::custom("invalid wall clock"))
    }

    /// `Option<PrimitiveDateTime>`：`None` → `null`。
    pub mod option {
        use super::*;

        pub fn serialize<S>(
            value: &Option<PrimitiveDateTime>,
            serializer: S,
        ) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            match value {
                Some(value) => serializer.serialize_some(&format(value)),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<PrimitiveDateTime>, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let text = Option::<String>::deserialize(deserializer)?;
            text.map(|text| {
                parse(&text).ok_or_else(|| serde::de::Error::custom("invalid wall clock"))
            })
            .transpose()
        }
    }

    fn format(value: &PrimitiveDateTime) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            value.year(),
            u8::from(value.month()),
            value.day(),
            value.hour(),
            value.minute(),
            value.second()
        )
    }

    fn parse(text: &str) -> Option<PrimitiveDateTime> {
        let (date, time) = text.split_once('T')?;
        let mut date_parts = date.split('-');
        let year: i32 = date_parts.next()?.parse().ok()?;
        let month: u8 = date_parts.next()?.parse().ok()?;
        let day: u8 = date_parts.next()?.parse().ok()?;
        let mut time_parts = time.split(':');
        let hour: u8 = time_parts.next()?.parse().ok()?;
        let minute: u8 = time_parts.next()?.parse().ok()?;
        let second: u8 = time_parts.next().unwrap_or("0").parse().ok()?;
        let month = Month::try_from(month).ok()?;
        let date = Date::from_calendar_date(year, month, day).ok()?;
        Some(PrimitiveDateTime::new(
            date,
            Time::from_hms(hour, minute, second).ok()?,
        ))
    }
}

/// Wall clock without a time zone: what a cron expression matches against.
pub use time::PrimitiveDateTime;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// The cron expression exactly as the user wrote it.
    ///
    /// Kept next to the parsed spec because the board shows it back to its
    /// author (ADR-0063 §5): `*/15` and `0,15,30,45` parse to the same spec, and
    /// showing the second to someone who typed the first reads as "the app
    /// rewrote my config".
    pub cron_text: String,
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

// ===== 运行时定时任务（Scheduled Tasks）====
//
// `[schedules]` 之上（只读 config 声明，ADR-0063）的运行时任务：由模型工具 / GUI
// 创建，支持 5 种规则，投递语义为"投递器"——回执只证明投递成功，不等于执行完成。
// 全部是纯数据 + 纯函数，wall clock 作参数，延续本模块的可测风格。

/// 定时规则：5 种。`Cron` 保存表达式文本（解析复用 [`CronSpec`]），其余为纯数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rule {
    /// 5 字段 cron（本地时间，crontab(5)）。
    Cron { expr: String },
    /// 一次性：指定墙钟时刻（本地时间）。
    Once {
        #[serde(with = "serde_wall_clock")]
        at: PrimitiveDateTime,
    },
    /// 每 N 秒（≥ 60）。
    Interval { seconds: u64 },
    /// 每天 HH:MM。
    Daily { hour: u8, minute: u8 },
    /// 每周某天 HH:MM（`weekday`：0=周日，同 crontab；7 折算为 0）。
    Weekly { weekday: u8, hour: u8, minute: u8 },
}

/// 规则描述解析失败的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    #[error("rule `{text}`: unknown kind; use cron / once / every / daily / weekly")]
    UnknownKind { text: String },
    #[error("rule `{text}`: {source}")]
    Cron {
        text: String,
        #[source]
        source: CronError,
    },
    #[error("rule `{text}`: {reason}")]
    Invalid { text: String, reason: String },
}

/// 把一条规则描述解析为冻结的 [`Rule`]。
///
/// 识别形式：
/// - `cron <expr>` 或裸 5 字段 cron 表达式
/// - `once <YYYY-MM-DD HH:MM>`（本地墙钟）
/// - `every <秒>`（≥ 60）
/// - `daily <HH:MM>`
/// - `weekly <weekday> <HH:MM>`（weekday：`sun`/`mon`/… 或 0..7，0=周日）
pub fn parse_rule(text: &str) -> Result<Rule, RuleError> {
    let text = text.trim();
    let parts: Vec<&str> = text.split_whitespace().collect();
    match parts.first().copied() {
        Some("cron") => {
            let expr = parts[1..].join(" ");
            if expr.is_empty() {
                return Err(RuleError::Invalid {
                    text: text.to_string(),
                    reason: "missing cron expression".to_string(),
                });
            }
            let _ = CronSpec::parse(&expr).map_err(|source| RuleError::Cron {
                text: text.to_string(),
                source,
            })?;
            Ok(Rule::Cron { expr })
        }
        Some("once") => {
            let at = parse_once(&parts[1..].join(" ")).ok_or_else(|| RuleError::Invalid {
                text: text.to_string(),
                reason: "expected `once YYYY-MM-DD HH:MM`".to_string(),
            })?;
            Ok(Rule::Once { at })
        }
        Some("every") => {
            let seconds: u64 =
                parts
                    .get(1)
                    .and_then(|part| part.parse().ok())
                    .ok_or_else(|| RuleError::Invalid {
                        text: text.to_string(),
                        reason: "expected `every <seconds>`".to_string(),
                    })?;
            if seconds < 60 {
                return Err(RuleError::Invalid {
                    text: text.to_string(),
                    reason: "interval must be at least 60 seconds".to_string(),
                });
            }
            Ok(Rule::Interval { seconds })
        }
        Some("daily") => {
            let (hour, minute) =
                parse_hhmm(parts.get(1).copied().unwrap_or("")).ok_or_else(|| {
                    RuleError::Invalid {
                        text: text.to_string(),
                        reason: "expected `daily HH:MM`".to_string(),
                    }
                })?;
            Ok(Rule::Daily { hour, minute })
        }
        Some("weekly") => {
            let weekday = parts
                .get(1)
                .and_then(|part| parse_weekday(part))
                .ok_or_else(|| RuleError::Invalid {
                    text: text.to_string(),
                    reason: "expected `weekly <weekday> HH:MM`".to_string(),
                })?;
            let (hour, minute) =
                parse_hhmm(parts.get(2).copied().unwrap_or("")).ok_or_else(|| {
                    RuleError::Invalid {
                        text: text.to_string(),
                        reason: "expected `weekly <weekday> HH:MM`".to_string(),
                    }
                })?;
            Ok(Rule::Weekly {
                weekday,
                hour,
                minute,
            })
        }
        // 裸表达式：当 cron 解析。
        Some(_) if parts.len() == 5 => {
            let _ = CronSpec::parse(text).map_err(|source| RuleError::Cron {
                text: text.to_string(),
                source,
            })?;
            Ok(Rule::Cron {
                expr: text.to_string(),
            })
        }
        _ => Err(RuleError::UnknownKind {
            text: text.to_string(),
        }),
    }
}

/// `YYYY-MM-DD HH:MM` → 墙钟时刻。
fn parse_once(text: &str) -> Option<PrimitiveDateTime> {
    let (date, time) = text.split_once(' ')?;
    let mut date_parts = date.split('-');
    let year: i32 = date_parts.next()?.parse().ok()?;
    let month: u8 = date_parts.next()?.parse().ok()?;
    let day: u8 = date_parts.next()?.parse().ok()?;
    let month = Month::try_from(month).ok()?;
    let date = Date::from_calendar_date(year, month, day).ok()?;
    let (hour, minute) = parse_hhmm(time)?;
    Some(PrimitiveDateTime::new(
        date,
        Time::from_hms(hour, minute, 0).ok()?,
    ))
}

/// `HH:MM` → (时, 分)。
fn parse_hhmm(text: &str) -> Option<(u8, u8)> {
    let (hour, minute) = text.split_once(':')?;
    let hour: u8 = hour.parse().ok()?;
    let minute: u8 = minute.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some((hour, minute))
}

/// `sun`/`mon`/… 或 0..7 → 0=周日。
fn parse_weekday(text: &str) -> Option<u8> {
    let lower = text.trim().to_ascii_lowercase();
    if let Ok(number) = lower.parse::<u8>() {
        return (number <= 7).then(|| number % 7);
    }
    DAY_NAMES
        .iter()
        .position(|name| *name == lower.as_str())
        .map(|index| index as u8)
}

impl Rule {
    /// 该规则在 `now` 之后的最近一次发生点；一次性规则已过即 `None`。
    pub fn next_after(&self, now: PrimitiveDateTime) -> Option<PrimitiveDateTime> {
        match self {
            Rule::Cron { expr } => CronSpec::parse(expr).ok()?.next_after(now),
            Rule::Once { at } => (*at > now).then_some(*at),
            Rule::Interval { seconds } => {
                Some(now + TimeSpan::seconds(i64::try_from(*seconds).ok()?))
            }
            Rule::Daily { hour, minute } => {
                let time = Time::from_hms(*hour, *minute, 0).ok()?;
                let today = now.date();
                let candidate = PrimitiveDateTime::new(today, time);
                if candidate > now {
                    Some(candidate)
                } else {
                    Some(PrimitiveDateTime::new(today.next_day()?, time))
                }
            }
            Rule::Weekly {
                weekday,
                hour,
                minute,
            } => {
                let time = Time::from_hms(*hour, *minute, 0).ok()?;
                let today = now.date();
                let today_ix = weekday_of(today.weekday()) as u8; // 0=周日
                let target = weekday % 7;
                let delta = (target + 7 - today_ix) % 7;
                let days = if delta == 0 {
                    // 今天：时刻还没到 → 今天；已过 → 下周。
                    if time > now.time() { 0 } else { 7 }
                } else {
                    i64::from(delta)
                };
                Some(PrimitiveDateTime::new(today + TimeSpan::days(days), time))
            }
        }
    }

    /// 规则的紧凑描述（列表 / 详情展示用）。
    pub fn describe(&self) -> String {
        match self {
            Rule::Cron { expr } => format!("cron `{expr}`"),
            Rule::Once { at } => format!("once {at}"),
            Rule::Interval { seconds } => format!("every {seconds}s"),
            Rule::Daily { hour, minute } => format!("daily {:02}:{:02}", hour, minute),
            Rule::Weekly {
                weekday,
                hour,
                minute,
            } => {
                let day = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"]
                    [(*weekday % 7) as usize];
                format!("weekly {day} {:02}:{:02}", hour, minute)
            }
        }
    }
}

/// 一次投递后的推进：循环规则算出下一次，一次性规则收尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    Next(PrimitiveDateTime),
    Ended,
}

/// 推进一次发生点（latest-only：只取 `now` 之后的最近一次，从不补发积压）。
pub fn advance_rule(rule: &Rule, now: PrimitiveDateTime) -> Advance {
    match rule.next_after(now) {
        Some(next) => Advance::Next(next),
        None => Advance::Ended,
    }
}

/// 投递目标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// 既有会话（任务 id）。
    Session { id: String },
    /// 首次投递创建保留会话，之后复用。
    NewOnce,
    /// 每次投递都新建会话（先落盘 `reserved_session_id`）。
    New,
}

impl Target {
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Target::Session { id } => Some(id),
            _ => None,
        }
    }

    /// 投递目标的紧凑描述（列表 / 详情展示用）。
    pub fn describe(&self) -> String {
        match self {
            Target::Session { id } => format!("会话 `{id}`"),
            Target::NewOnce => "新会话（首次投递创建并复用）".to_string(),
            Target::New => "新会话（每次投递都新建）".to_string(),
        }
    }
}

/// 任务生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Active,
    Paused,
    Ended,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Active => "active",
            TaskStatus::Paused => "paused",
            TaskStatus::Ended => "ended",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "active" => Some(TaskStatus::Active),
            "paused" => Some(TaskStatus::Paused),
            "ended" => Some(TaskStatus::Ended),
            _ => None,
        }
    }
}

/// 一次投递的回执。**回执 ≠ 执行完成**：只证明消息已作为 `user/message` 事件写入
/// 目标会话日志。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryReceipt {
    /// 对应哪个发生点。
    #[serde(with = "serde_wall_clock")]
    pub occurrence_at: PrimitiveDateTime,
    /// 投递确认时刻。
    #[serde(with = "serde_wall_clock")]
    pub delivered_at: PrimitiveDateTime,
    /// 共享同一条投递消息（批量合并时多个任务共用一个 id）。
    pub message_id: String,
    /// 实际投往的会话（`new` 目标在投递后才确定）。
    pub target_session_id: Option<String>,
}

/// 运行时定时任务。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub rule: Rule,
    pub target: Target,
    pub overlap: Overlap,
    /// 定时回合的审批超时：超时自动拒绝（`None` 一直等人工）。
    pub approval_timeout: Option<Duration>,
    pub status: TaskStatus,
    /// 牵涉会话（只增不减），用于变更通知路由。
    pub stakeholders: Vec<String>,
    pub last_receipt: Option<DeliveryReceipt>,
    /// 投递历史，按保留策略裁剪（见 [`append_delivery`]）。
    pub delivery_log: Vec<DeliveryReceipt>,
    pub earlier_records_pruned: bool,
    /// `new` 目标：投递前先落盘预定会话 id，崩溃后复用同一会话。
    pub reserved_session_id: Option<String>,
    #[serde(with = "serde_wall_clock::option")]
    pub next_fire_at: Option<PrimitiveDateTime>,
    /// 最小投递间隔：空闲后也要等这么久才能再投。
    pub rearm_after: Duration,
}

/// 投递历史保留策略：最多保留最近 30 天 / 200 条。
pub const DELIVERY_LOG_MAX_DAYS: i64 = 30;
pub const DELIVERY_LOG_MAX_RECORDS: usize = 200;

/// 追加一条回执并按保留策略裁剪。返回是否发生了裁剪。
pub fn append_delivery(
    log: &mut Vec<DeliveryReceipt>,
    receipt: DeliveryReceipt,
    now: PrimitiveDateTime,
) -> bool {
    log.push(receipt);
    let cutoff = now - TimeSpan::days(DELIVERY_LOG_MAX_DAYS);
    log.retain(|receipt| receipt.delivered_at >= cutoff);
    if log.len() > DELIVERY_LOG_MAX_RECORDS {
        let keep = log.len() - DELIVERY_LOG_MAX_RECORDS;
        log.drain(..keep);
        true
    } else {
        false
    }
}

/// 一个运行时任务的到期判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskTick {
    NotYet,
    Fire,
    SkipBusy,
    Orphaned,
}

pub fn task_tick(
    task: &ScheduledTask,
    now: PrimitiveDateTime,
    busy: bool,
    task_exists: bool,
) -> TaskTick {
    if task.status != TaskStatus::Active {
        return TaskTick::NotYet;
    }
    let Some(next) = task.next_fire_at else {
        return TaskTick::NotYet;
    };
    if now < next {
        return TaskTick::NotYet;
    }
    if task.target.session_id().is_some() && !task_exists {
        return TaskTick::Orphaned;
    }
    if busy && task.overlap == Overlap::Skip {
        return TaskTick::SkipBusy;
    }
    TaskTick::Fire
}

/// 把 config 声明的 `[schedules.<name>]` 条目导入为运行时任务（ADR-0059 迁移）。
///
/// `state` 保留 store 里已有的触发进度（`next_fire_at`）；没有则按 `now` 计算。
/// 迁移是幂等的：同一名字的任务已存在时不重复导入（由调用方判重）。
pub fn import_config_schedule(
    schedule: &Schedule,
    state: Option<&ScheduleState>,
    now: PrimitiveDateTime,
) -> ScheduledTask {
    let next_fire_at = state
        .and_then(|s| s.next_fire_at)
        .or_else(|| schedule.cron.next_after(now));
    let (target, stakeholders) = match &schedule.task {
        Some(id) => (Target::Session { id: id.clone() }, vec![id.clone()]),
        None => (Target::New, Vec::new()),
    };
    ScheduledTask {
        id: schedule.name.clone(),
        title: schedule.name.clone(),
        prompt: schedule.prompt.clone(),
        rule: Rule::Cron {
            expr: schedule.cron_text.clone(),
        },
        target,
        overlap: schedule.overlap,
        approval_timeout: schedule.approval_timeout,
        status: TaskStatus::Active,
        stakeholders,
        last_receipt: None,
        delivery_log: Vec::new(),
        earlier_records_pruned: false,
        reserved_session_id: None,
        next_fire_at,
        rearm_after: Duration::ZERO,
    }
}

/// The wall clock the user writes `0 3 * * *` against: local time.
///
/// Read per call rather than cached once, so a DST change during a long-lived
/// window moves the next window with it. `time`'s own `now_local` is behind a
/// feature that is documented as unsound once threads exist, and the GUI has
/// plenty of them, so the offset comes from `localtime_r` instead (`qcode-core`
/// already links `libc` for the session lock). Non-unix builds use UTC and say
/// so in the docs.
pub fn now_local() -> PrimitiveDateTime {
    let local = time::OffsetDateTime::now_utc().to_offset(local_offset());
    PrimitiveDateTime::new(local.date(), local.time())
}

/// Offset of local time from UTC right now.
#[cfg(unix)]
pub fn local_offset() -> time::UtcOffset {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_secs() as libc::time_t,
        Err(_) => 0,
    };
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `localtime_r` writes into `local` and reads the process time zone;
    // `local` is owned by this frame.
    let converted = unsafe { libc::localtime_r(&seconds, &mut local) };
    if converted.is_null() {
        return time::UtcOffset::UTC;
    }
    // `tm_gmtoff` is seconds east of UTC (BSD/GNU; present on macOS and Linux).
    let east = local.tm_gmtoff;
    time::UtcOffset::from_whole_seconds(east.clamp(-86_400, 86_400) as i32)
        .unwrap_or(time::UtcOffset::UTC)
}

#[cfg(not(unix))]
pub fn local_offset() -> time::UtcOffset {
    time::UtcOffset::UTC
}

/// A wall clock at minute resolution, for the tests and the host alike.
pub fn at(year: i32, month: Month, day: u8, hour: u8, minute: u8) -> PrimitiveDateTime {
    let date = Date::from_calendar_date(year, month, day).expect("valid date");
    let time = Time::from_hms(hour, minute, 0).expect("valid time");
    PrimitiveDateTime::new(date, time)
}

#[cfg(test)]
mod tests;
