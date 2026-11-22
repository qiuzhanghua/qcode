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

/// 星期名（`weekly` 的展示形式里用）。索引与 [`weekday_of`] 同一套：周日 = 0。
fn weekday_name(day: u32) -> String {
    // 每个 arm 直接调 `tr`，而不是先算出 key 再调一次：key 字面量要落在调用点上，
    // 否则「引用了哪个 key」这件事就只剩运行期才知道（门禁 4 与 6 都看不见）。
    use qcode_i18n::tr;
    match day {
        0 => tr("core.schedule.weekday.sun"),
        1 => tr("core.schedule.weekday.mon"),
        2 => tr("core.schedule.weekday.tue"),
        3 => tr("core.schedule.weekday.wed"),
        4 => tr("core.schedule.weekday.thu"),
        5 => tr("core.schedule.weekday.fri"),
        _ => tr("core.schedule.weekday.sat"),
    }
    .to_string()
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

// `[schedules]` 的旧运行状态（`ScheduleState` / `ScheduleStatus` / `tick` /
// `recover` / `advance`）已删除：ADR-0093 把配置条目并进任务表，进度记在
// `ScheduledTask` 上（`next_fire_at` / `missed_windows` / `last_receipt`）。
// 留着那套纯函数只会让第二个执行者以为还有第二条投递路径。

// ===== 定时任务 =====
//
// 唯一的一套定时任务模型。来源有两个：用户在 GUI / CLI / 模型工具里创建
// （`Origin::User`），或由 `[schedules.<name>]` 声明、经 `reconcile_config`
// 投影出来（`Origin::Config`）。两者在投递上**没有任何区别**——这正是 ADR-0093
// 的目的：曾经的配置路径自己投递，与运行时任务各投一次。
//
// 投递语义为"投递器"：回执只证明投递成功，不等于执行完成。全部是纯数据 +
// 纯函数，wall clock 作参数，延续本模块的可测风格。

/// 定时规则：6 种。`Cron` 保存表达式文本（解析复用 [`CronSpec`]），其余为纯数据。
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
    /// 每周若干天 HH:MM（`weekdays`：0=周日，同 crontab；7 折算为 0）。
    ///
    /// `weekday` 是旧单值形态的兼容字段：旧数据只有 `weekday` 时兜底读取，
    /// 新数据只写 `weekdays`。消费方统一走 [`Rule::effective_weekdays`]。
    Weekly {
        #[serde(default)]
        weekdays: Vec<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        weekday: Option<u8>,
        hour: u8,
        minute: u8,
    },
    /// 每月某日 HH:MM（`day`：1-31；所在月份没有该天则跳过当月）。
    Monthly { day: u8, hour: u8, minute: u8 },
}

/// 规则描述解析失败的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    #[error("rule `{text}`: unknown kind; use cron / once / every / daily / weekly / monthly")]
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
/// - `weekly <weekday-list> <HH:MM>`（weekday：`sun`/`mon`/… 或 0..7，0=周日；
///   可多选，逗号或空格分隔）
/// - `monthly <day> <HH:MM>`（day 1-31）
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
            // `weekly <weekday-list> HH:MM`：weekday-list 支持逗号分隔（`1,3,5`）
            // 或空格分隔多个（`mon wed fri`），至少一个；最后一个 token 是时刻。
            let parts = &parts[1..];
            if parts.len() < 2 {
                return Err(RuleError::Invalid {
                    text: text.to_string(),
                    reason: "expected `weekly <weekday> HH:MM`".to_string(),
                });
            }
            let (rest, hhmm) = parts.split_at(parts.len() - 1);
            let mut weekdays: Vec<u8> = Vec::new();
            for token in rest {
                for piece in token.split(',') {
                    weekdays.push(
                        parse_weekday(piece).ok_or_else(|| RuleError::Invalid {
                            text: text.to_string(),
                            reason: "expected `weekly <weekday> HH:MM`".to_string(),
                        })? % 7,
                    );
                }
            }
            if weekdays.is_empty() {
                return Err(RuleError::Invalid {
                    text: text.to_string(),
                    reason: "expected `weekly <weekday> HH:MM`".to_string(),
                });
            }
            let (hour, minute) = parse_hhmm(hhmm[0]).ok_or_else(|| RuleError::Invalid {
                text: text.to_string(),
                reason: "expected `weekly <weekday> HH:MM`".to_string(),
            })?;
            Ok(Rule::Weekly {
                weekdays,
                weekday: None,
                hour,
                minute,
            })
        }
        Some("monthly") => {
            let day: u8 = parts
                .get(1)
                .and_then(|part| part.parse().ok())
                .filter(|day| (1..=31).contains(day))
                .ok_or_else(|| RuleError::Invalid {
                    text: text.to_string(),
                    reason: "expected `monthly <day> HH:MM` (day 1-31)".to_string(),
                })?;
            let (hour, minute) =
                parse_hhmm(parts.get(2).copied().unwrap_or("")).ok_or_else(|| {
                    RuleError::Invalid {
                        text: text.to_string(),
                        reason: "expected `monthly <day> HH:MM` (day 1-31)".to_string(),
                    }
                })?;
            Ok(Rule::Monthly { day, hour, minute })
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
        return (number <= 7).then_some(number % 7);
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
                weekdays,
                weekday,
                hour,
                minute,
            } => {
                let time = Time::from_hms(*hour, *minute, 0).ok()?;
                let today = now.date();
                let today_ix = weekday_of(today.weekday()) as u8; // 0=周日
                // 归一化后的多选星期；空列表（旧数据两个字段都缺）按没有规则处理。
                let targets = Self::effective_weekdays(weekdays, *weekday);
                if targets.is_empty() {
                    return None;
                }
                let mut best: Option<PrimitiveDateTime> = None;
                for target in targets {
                    let delta = (target + 7 - today_ix) % 7;
                    let days = if delta == 0 {
                        // 今天：时刻还没到 → 今天；已过 → 下周。
                        if time > now.time() { 0 } else { 7 }
                    } else {
                        i64::from(delta)
                    };
                    let candidate = PrimitiveDateTime::new(today + TimeSpan::days(days), time);
                    best = Some(best.map_or(candidate, |b| b.min(candidate)));
                }
                best
            }
            Rule::Monthly { day, hour, minute } => {
                let time = Time::from_hms(*hour, *minute, 0).ok()?;
                let today = now.date();
                // 从当月起扫描最多 12 个月：当月没有该日（如 31 号落在 2 月）则
                // 跳过；`day` 当天时刻已过也会顺延到下月。
                let first = today.year() * 12 + today.month() as i32 - 1;
                for month_index in (first..).take(12) {
                    let year = month_index.div_euclid(12);
                    let month = month_index.rem_euclid(12) + 1;
                    let month_enum = Month::try_from(month as u8).ok()?;
                    if *day <= month_enum.length(year) {
                        let date = Date::from_calendar_date(year, month_enum, *day).ok()?;
                        let candidate = PrimitiveDateTime::new(date, time);
                        if candidate > now {
                            return Some(candidate);
                        }
                    }
                }
                None
            }
        }
    }

    /// 归一化的星期集合（0=周日，去重、升序）。兼容旧单值 `weekday` 字段：
    /// 新数据用 `weekdays` 列表，旧数据只有 `weekday` 时兜底。
    pub fn effective_weekdays(weekdays: &[u8], legacy: Option<u8>) -> Vec<u8> {
        let mut days: Vec<u8> = if weekdays.is_empty() {
            legacy.map(|day| day % 7).into_iter().collect()
        } else {
            weekdays.iter().map(|day| day % 7).collect()
        };
        days.sort_unstable();
        days.dedup();
        days
    }

    /// 规则的紧凑描述（列表 / 详情展示用）。
    pub fn describe(&self) -> String {
        match self {
            Rule::Cron { expr } => format!("cron `{expr}`"),
            Rule::Once { at } => format!("once {at}"),
            Rule::Interval { seconds } => format!("every {seconds}s"),
            Rule::Daily { hour, minute } => format!("daily {:02}:{:02}", hour, minute),
            Rule::Weekly {
                weekdays,
                weekday,
                hour,
                minute,
            } => {
                let names: Vec<String> = Self::effective_weekdays(weekdays, *weekday)
                    .into_iter()
                    .map(|day| weekday_name(u32::from(day)))
                    .collect();
                // 分隔符也是译文的一部分（中文顿号 / 英文逗号加空格），不能写死在
                // `join` 里。
                let separator = qcode_i18n::tr("core.schedule.weekday.separator");
                format!(
                    "weekly {} {:02}:{:02}",
                    names.join(separator.as_ref()),
                    hour,
                    minute
                )
            }
            Rule::Monthly { day, hour, minute } => qcode_i18n::trp(
                "core.schedule.rule.monthly",
                &[
                    day.to_string(),
                    format!("{hour:02}"),
                    format!("{minute:02}"),
                ],
            ),
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

/// 宿主不在之后，一条运行时任务的恢复结果（与 [`Recovery`] 同形，按 `Rule` 计窗）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskRecovery {
    /// 累计错过的窗口数（含任务上已记的）。
    pub missed: u32,
    /// 恢复后的下一次发生点；`None` = 一次性规则已经过去，该收尾了。
    pub next_fire_at: Option<PrimitiveDateTime>,
}

/// 宿主「不在」超过一个拍间隔后，把运行时任务推进到未来，**不补投**。
///
/// 与配置路径的 [`recover`] 是同一件事、同一套语义（ADR-0059 §3「错过不补跑」），
/// 差别只在计窗的依据是 [`Rule`] 而不是 `CronSpec`。存在的理由是这个语义最初只落
/// 在配置路径上，运行时任务路径漏了——于是宿主关三天再开，所有过期任务会在开机后
/// 一拍内一起触发一轮（ADR-0088 §4）。
pub fn recover_task(task: &ScheduledTask, now: PrimitiveDateTime) -> TaskRecovery {
    let Some(next) = task.next_fire_at else {
        return TaskRecovery {
            missed: 0,
            next_fire_at: None,
        };
    };
    if next > now {
        return TaskRecovery {
            missed: 0,
            next_fire_at: Some(next),
        };
    }
    // 同 `recover`：时钟倒退十年不该变成一个百万步的循环。
    const MAX_MISSED: u32 = 1_000;
    let mut missed = 0;
    let mut at = next;
    while at <= now && missed < MAX_MISSED {
        missed += 1;
        match task.rule.next_after(at) {
            // `following > at` 是死循环护栏：某条规则若把「下一次」算成同一时刻，
            // 循环必须停下而不是转满 MAX_MISSED 圈。
            Some(following) if following > at => at = following,
            _ => break,
        }
    }
    TaskRecovery {
        missed: task.missed_windows.saturating_add(missed),
        next_fire_at: task.rule.next_after(now),
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
            Target::Session { id } => {
                qcode_i18n::trp("core.schedule.target.session", &[id.as_str()])
            }
            Target::NewOnce => qcode_i18n::tr("core.schedule.target.new_once").to_string(),
            Target::New => qcode_i18n::tr("core.schedule.target.new").to_string(),
        }
    }

    /// 解析 `target` 参数的文本形式（ADR-0090 §0.2）。
    ///
    /// 空串按 `new_once` 处理：那是工具与 CLI 的默认值，两处都不该各自写一遍。
    /// `new-once` 这个连字符别名是对外的既有输入，别趁搬家删掉。
    ///
    /// 与 `Overlap::parse`、`TaskStatus::parse` 同族，都住在这里——解析散在两个 crate
    /// 里时，`Target` 是唯一一个不一致的。
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() || text == "new_once" || text == "new-once" {
            return Some(Target::NewOnce);
        }
        if text == "new" {
            return Some(Target::New);
        }
        if let Some(id) = text.strip_prefix("session:") {
            let id = id.trim();
            return (!id.is_empty()).then(|| Target::Session { id: id.to_string() });
        }
        None
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
    /// 这次投递执行的任务内容（prompt）。旧记录没有该字段，显示时空缺。
    #[serde(default)]
    pub prompt: String,
}

/// 这条任务是谁声明的（ADR-0092 §11 / ADR-0093）。
///
/// 引入它是因为 `[schedules]` 配置条目与运行时任务必须合并成一套，而合并之后
/// 「谁说了算」不能靠猜：配置声明的条目，改它的地方是 `config.toml`；用户自己
/// 建的任务，改它的地方是 GUI / CLI。混起来就会出现「在 GUI 里删掉、重启又回来」
/// 这种没人能解释的行为。
///
/// `#[serde(default)]`：老的 `tasks.json` 没有这个键，读成 [`Origin::User`] ——
/// 那是安全的默认值（不会被 reconcile 改动，也不会被 prune 删掉）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// GUI / CLI / 模型工具建的。用户是唯一的所有者。
    #[default]
    User,
    /// `[schedules.<name>]` 声明、启动时 reconcile 出来的。`config.toml` 是
    /// 唯一的所有者，任务表只是它在运行时的投影。
    Config,
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
    /// 谁声明的（见 [`Origin`]）。
    #[serde(default)]
    pub origin: Origin,
    /// 牵涉会话（只增不减），用于变更通知路由。
    pub stakeholders: Vec<String>,
    pub last_receipt: Option<DeliveryReceipt>,
    /// 投递历史，按保留策略裁剪（见 [`append_delivery`]）。
    pub delivery_log: Vec<DeliveryReceipt>,
    pub earlier_records_pruned: bool,
    /// 宿主不在运行期间错过的触发次数（ADR-0059 §3「错过不补跑」的记账）。
    ///
    /// 只增不减：`recover` 的语义是「错过了几个窗口」，而窗口一旦错过就不会
    /// 变少。旧实现里配置路径每次 fire 都把它归零（`advance` 重建整个
    /// `ScheduleState`），于是「错过 3 次」这个事实会在下一次触发时消失——那是
    /// 两套账本合并前的遗留，合并后统一按只增处理。
    /// `#[serde(default)]`：老的 `tasks.json` 没有这个键也能读。
    #[serde(default)]
    pub missed_windows: u32,
    /// `new` 目标：投递前先落盘预定会话 id，崩溃后复用同一会话。
    pub reserved_session_id: Option<String>,
    #[serde(with = "serde_wall_clock::option")]
    pub next_fire_at: Option<PrimitiveDateTime>,
    /// 投递回合的工作目录（绝对路径）。`None` = 用会话 / 引擎默认工作目录。
    #[serde(default)]
    pub cwd: Option<String>,
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

/// 最小投递间隔：上次投递后至少间隔这么久才允许再次投递，防止确认延迟 /
/// 高频规则把目标会话刷成轰炸。全局固定 60 秒，不随任务走（对齐 dsh
/// `MIN_DELIVERY_INTERVAL_MS`）。
pub const MIN_DELIVERY_INTERVAL: TimeSpan = TimeSpan::seconds(60);

/// 无人值守执行时，审批没设截止时间会补上的缺省值（ADR-0092 §7）。
///
/// 为什么必须有：`approval_timeout = None` 的语义是「一直等人工」（ADR-0059
/// §4）。有窗口/终端时这是对的——人去泡杯咖啡就回来了。但 daemon 没有人工，
/// 一条没设截止时间的任务会让那次会话的写锁**永远**握着，那个会话从此不可写，
/// 而且没有任何东西会报错。
///
/// 为什么是 5 分钟：定时任务都在后台跑，没人会盯着屏幕等它；5 分钟足够盖住
/// 「用户正好在机器旁边、看到提示就点一下」，又不至于让一个真正的无人值守
/// 回合锁住会话一整晚。真正需要更长时间的，应当显式写 `--approval-timeout-secs`。
///
/// **不能静默补**：建任务时就要把「无人值守会这样处理」说出来，理由同 ADR-0090
/// §5——用户必须知道他的任务在没人看着的时候会怎么被处理。
pub const UNATTENDED_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// 本次投递最早可再次发生的时刻（对齐 dsh `nextAdmissionAt`）。
///
/// 锚定规则与 dsh 一致：任务自身频率 ≥ 最小间隔时锚定到发生点（确认延迟
/// 不累积、不往后拖）；否则锚定到投递确认时刻（保证两次投递至少隔 60s）。
/// 没有回执（从未投递过）返回 `None`，表示不受护栏限制。
pub fn delivery_floor_end(task: &ScheduledTask) -> Option<PrimitiveDateTime> {
    let receipt = task.last_receipt.as_ref()?;
    let occurrence = receipt.occurrence_at;
    let anchor = if task
        .next_fire_at
        .is_some_and(|next| next - occurrence >= MIN_DELIVERY_INTERVAL)
    {
        occurrence
    } else {
        receipt.delivered_at
    };
    Some(anchor + MIN_DELIVERY_INTERVAL)
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

#[cfg(windows)]
pub fn local_offset() -> time::UtcOffset {
    // The CRT (`localtime_r`) only exists on unix; on Windows the offset is
    // measured directly: local wall clock minus UTC wall clock. The two
    // `SystemTimeToFileTime` calls interpret both as "UTC" representations,
    // so the difference is exactly the current UTC offset — DST included,
    // no transition tables to keep in sync.
    #[repr(C)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }
    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    unsafe extern "system" {
        fn GetSystemTime(lpSystemTime: *mut SystemTime);
        fn GetLocalTime(lpSystemTime: *mut SystemTime);
        fn SystemTimeToFileTime(lpSystemTime: *const SystemTime, lpFileTime: *mut FileTime) -> i32;
    }
    let as_100ns = |time: &SystemTime| -> u64 {
        let mut file: FileTime = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are valid for one write; the function either
        // fails (returns 0, file stays zero) or fills `file`.
        let ok = unsafe { SystemTimeToFileTime(time, &mut file) };
        if ok == 0 {
            return 0;
        }
        (u64::from(file.high) << 32) | u64::from(file.low)
    };
    let mut utc: SystemTime = unsafe { std::mem::zeroed() };
    let mut local: SystemTime = unsafe { std::mem::zeroed() };
    // SAFETY: pointers are valid for one write; these never fail.
    unsafe {
        GetSystemTime(&mut utc);
        GetLocalTime(&mut local);
    }
    let diff_seconds = (as_100ns(&local).wrapping_sub(as_100ns(&utc)) as i64) / 10_000_000;
    time::UtcOffset::from_whole_seconds(diff_seconds.clamp(-86_400, 86_400) as i32)
        .unwrap_or(time::UtcOffset::UTC)
}

/// A wall clock at minute resolution, for the tests and the host alike.
pub fn at(year: i32, month: Month, day: u8, hour: u8, minute: u8) -> PrimitiveDateTime {
    let date = Date::from_calendar_date(year, month, day).expect("valid date");
    let time = Time::from_hms(hour, minute, 0).expect("valid time");
    PrimitiveDateTime::new(date, time)
}

#[cfg(test)]
mod tests;
