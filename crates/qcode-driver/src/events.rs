//! 可跟随的事件出口（ADR-0092 §8）——**只留缝，不做界面**。
//!
//! # 为什么现在就要留
//!
//! §8 说「GUI 要能看到 daemon 正在跑的 turn」，而本期明确不做那个界面。但接口必须
//! 现在就定，否则以后要做就得改驱动核心——那时 GUI 和 daemon 都已经依赖它了。
//!
//! # 缝的形状
//!
//! 一行一条 JSON，追加写到一个文件（`schedule/events.jsonl`）。选文件而不是 Unix
//! socket / 命名管道，理由有三条，每条都是踩过的：
//!
//! 1. **跨平台一个实现**。socket 要三平台各一段，而这个文件谁都能读。
//! 2. **跟随就是 `tail -f`**。没有连接状态、没有断线重连、没有「谁先关」。
//! 3. **挂了不影响驱动**。写事件失败只是少一条记录，不该让一次投递失败——
//!    这与 `session/*.jsonl` 是同一种取舍：日志是诊断，不是正确性。
//!
//! # 它不是第二个事实源
//!
//! 会话日志仍然是唯一的事实源。这个文件是**给「我能不能看它现在跑到哪了」用的**，
//! 可以随时删、可以从任意位置开始读。所以它的格式可以变，而会话日志不行。

use std::io::Write as _;
use std::path::{Path, PathBuf};

use time::PrimitiveDateTime;

/// 一条事件。刻意做小：它服务的是「看着它跑」，不是审计。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// 一拍。`away` 与计数都在里面，于是「刚才发生了什么」不必再算。
    Beat {
        at: PrimitiveDateTime,
        away: bool,
        fired: usize,
        skipped: usize,
        orphaned: usize,
    },
    /// 一次回合开始。`session` 是那条会话的 id——GUI 将来靠它去找日志。
    TurnStarted {
        at: PrimitiveDateTime,
        session: String,
        task_id: String,
    },
    /// 一次回合结束。
    TurnFinished {
        at: PrimitiveDateTime,
        session: String,
        ok: bool,
    },
    /// 让路 / 接手。驾驶权换手时写一条，于是「谁在开车」有时间线。
    Driving {
        at: PrimitiveDateTime,
        /// 谁在开：`gui:<时刻>` 或 `daemon:<时刻>`。
        identity: String,
        /// `true` = 开始驱动；`false` = 让出（等别人）。
        driving: bool,
    },
}

impl Event {
    fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Beat {
                at,
                away,
                fired,
                skipped,
                orphaned,
            } => serde_json::json!({
                "kind": "beat", "at": at.to_string(), "away": away,
                "fired": fired, "skipped": skipped, "orphaned": orphaned,
            }),
            Self::TurnStarted {
                at,
                session,
                task_id,
            } => serde_json::json!({
                "kind": "turn_started", "at": at.to_string(),
                "session": session, "task_id": task_id,
            }),
            Self::TurnFinished { at, session, ok } => serde_json::json!({
                "kind": "turn_finished", "at": at.to_string(),
                "session": session, "ok": ok,
            }),
            Self::Driving {
                at,
                identity,
                driving,
            } => serde_json::json!({
                "kind": "driving", "at": at.to_string(),
                "identity": identity, "driving": driving,
            }),
        }
    }

    /// 一行 JSON（带换行）。单独拿出来是为了能测格式而不碰文件。
    pub fn to_line(&self) -> String {
        let mut line = self.to_json().to_string();
        line.push('\n');
        line
    }
}

impl Event {
    /// 一拍结束时那一条。计数直接抄 `Report`：它是驱动算出来的权威。
    pub fn from_report(report: &crate::runtime::Report, at: PrimitiveDateTime) -> Self {
        Self::Beat {
            at,
            away: report.away,
            fired: report.fired,
            skipped: report.skipped,
            orphaned: report.orphaned,
        }
    }
}

/// 事件出口。
///
/// 实现只有一个（[`FileSink`]），但驱动只认这个 trait：将来要换成 socket 或者
/// 直接推给窗口时，驱动的代码不动。
pub trait Sink: Send {
    fn emit(&mut self, event: Event);
}

/// 什么都不做。GUI 用它——窗口里的事件走它自己那条路（`SessionEvent` 端口），
/// 不需要第二个出口。
pub struct Noop;

impl Sink for Noop {
    fn emit(&mut self, _event: Event) {}
}

/// 追加写 `schedule/events.jsonl`。
///
/// **每一次都重开文件**而不是长期持有句柄：daemon 可能跑几天，而 macOS 的手册页
/// 明确说 `vnode` 可能失效（日志轮转、文件被删）。重开的代价是一次 `open`——一拍
/// 一次，可以忽略；换来的是「删掉文件之后它自己会重建」。
pub struct FileSink {
    path: PathBuf,
}

impl FileSink {
    /// `data_dir/schedule/events.jsonl`。父目录不在这里建：写的时候顺带建，
    /// 于是「从没写过事件」不会留下一个空目录。
    pub fn under(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("schedule").join("events.jsonl"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Sink for FileSink {
    fn emit(&mut self, event: Event) {
        // 失败只记日志：这个文件是诊断，不该让一次投递失败。
        //
        // 与 `session/*.jsonl` 同一个取舍——那一边写失败也是记日志，因为会话
        // 日志的写者有 `SessionWriter` 的重试，而这里的读者只是一个 `tail`。
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            file.write_all(event.to_line().as_bytes())
        })();
        if let Err(error) = result {
            tracing::debug!(
                path = %self.path.display(),
                %error,
                "cannot write a schedule event; the driver keeps going"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Report;

    #[test]
    fn every_event_is_one_json_line() {
        let at = crate::tests_support::at_2026();
        for event in [
            Event::Beat {
                at,
                away: false,
                fired: 1,
                skipped: 0,
                orphaned: 0,
            },
            Event::TurnStarted {
                at,
                session: "01S".to_string(),
                task_id: "01T".to_string(),
            },
            Event::TurnFinished {
                at,
                session: "01S".to_string(),
                ok: true,
            },
            Event::Driving {
                at,
                identity: "daemon:x".to_string(),
                driving: true,
            },
        ] {
            let line = event.to_line();
            assert!(line.ends_with('\n'), "每条都要以换行结尾：{line:?}");
            assert_eq!(
                line.matches('\n').count(),
                1,
                "一行一条，不能有内嵌换行：{line:?}"
            );
            let value: serde_json::Value =
                serde_json::from_str(line.trim()).expect("must be valid JSON");
            assert!(value["kind"].is_string(), "要有 kind：{value}");
            assert!(value["at"].is_string(), "要有 at：{value}");
        }
    }

    #[test]
    fn a_file_sink_appends_one_line_per_event() {
        let dir = tempfile::tempdir().expect("dir");
        let mut sink = FileSink::under(dir.path());
        let at = crate::tests_support::at_2026();

        sink.emit(Event::Driving {
            at,
            identity: "daemon:1".to_string(),
            driving: true,
        });
        sink.emit(Event::Beat {
            at,
            away: false,
            fired: 2,
            skipped: 0,
            orphaned: 0,
        });

        let text = std::fs::read_to_string(sink.path()).expect("read");
        assert_eq!(text.lines().count(), 2, "两次 emit 两行：{text}");
        assert!(text.contains("\"driving\":true"), "{text}");
        assert!(text.contains("\"fired\":2"), "{text}");
    }

    /// 写不进去时不能 panic、不能让驱动失败。
    #[test]
    fn a_broken_sink_is_silent() {
        let dir = tempfile::tempdir().expect("dir");
        // 用一个**目录**当文件路径：`open` 必然失败（EISDIR）。
        let blocked = dir.path().join("schedule");
        std::fs::create_dir_all(blocked.join("events.jsonl")).expect("dir");
        let mut sink = FileSink::under(dir.path());
        sink.emit(Event::Beat {
            at: crate::tests_support::at_2026(),
            away: false,
            fired: 0,
            skipped: 0,
            orphaned: 0,
        });
        // 走到这里就说明它没 panic。
    }

    /// `Report` 到事件的翻译在驱动里，这里只钉住「字段对得上」这一层契约。
    #[test]
    fn a_report_maps_onto_a_beat_event() {
        let report = Report {
            away: true,
            fired: 3,
            skipped: 1,
            orphaned: 2,
            ..Report::default()
        };
        let event = Event::from_report(&report, crate::tests_support::at_2026());
        match event {
            Event::Beat {
                away,
                fired,
                skipped,
                orphaned,
                ..
            } => {
                assert!(away);
                assert_eq!((fired, skipped, orphaned), (3, 1, 2));
            }
            other => panic!("expected a beat, got {other:?}"),
        }
    }
}
