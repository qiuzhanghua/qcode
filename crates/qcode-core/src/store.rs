//! Append-only JSONL session store.
//!
//! One file per session under `<root>/sessions/<id>.jsonl`. Writes are appends;
//! a torn final line is discarded on load, which is the crash-recovery contract.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use time::{Duration, OffsetDateTime};

use crate::event::{
    Event, EventKind, RootsChanged, SessionStarted, TaskConfigured, TaskRenamed, ToolResult,
    ToolStatus,
};
use crate::schedule::{ScheduleState, ScheduleStatus, ScheduledTask};
use crate::task::{Root, Task, TaskError};

/// Wall clock in the store, at minute resolution. The scheduler only needs
/// "which window is this", so seconds would be noise in the file.
pub(crate) const WALL_CLOCK_FORMAT: &[time::format_description::FormatItem<'static>] =
    time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");

fn format_wall_clock(at: time::PrimitiveDateTime) -> String {
    at.format(WALL_CLOCK_FORMAT)
        .unwrap_or_else(|_| String::from("1970-01-01T00:00:00"))
}

fn parse_wall_clock(text: &str) -> Option<time::PrimitiveDateTime> {
    time::PrimitiveDateTime::parse(text, WALL_CLOCK_FORMAT).ok()
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid session id: {0}")]
    InvalidId(String),
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("non-monotonic seq: expected {expected}, got {got}")]
    Seq { expected: u64, got: u64 },
    #[error("invalid fork boundary {boundary} (session has 1..={last})")]
    InvalidFork { boundary: u64, last: u64 },
    #[error("session is already open for writing: {0}")]
    Locked(String),
    #[error("index error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Task(#[from] TaskError),
}

/// Exclusive writer lock for one session, held for the duration of a turn.
#[derive(Debug)]
pub struct SessionLock {
    #[cfg(unix)]
    _file: fs::File,
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    pub started: SessionStarted,
    /// RFC 3339 timestamp of the first event.
    pub started_at: String,
    pub turns: u32,
    pub last_seq: u64,
    /// Sum of `turn_ended.cost_usd` in this session.
    pub cost_usd: f64,
}

/// A newly created task/session pair. `task_id == started.session`.
#[derive(Debug, Clone)]
pub struct CreatedTask {
    pub id: String,
    pub started: Event,
    /// The initial `task_configured` event, to forward to the UI.
    pub configured: Event,
}

impl SessionSummary {
    /// Legacy migration view for a session that has no loaded log.
    ///
    /// This ignores `task_configured` / `roots_changed`; use
    /// [`SessionStore::task`] when the event log is available.
    pub fn legacy_task(&self) -> Result<Task, TaskError> {
        Task::from_legacy_session(self.id.clone(), self.started.cwd.clone())
    }
}

impl SessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn session_path(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{id}.jsonl"))
    }

    pub fn outputs_dir(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(id).join("outputs")
    }

    /// Persist an untruncated tool output and return its path.
    pub fn write_output(&self, id: &str, call_id: &str, text: &str) -> Result<PathBuf, StoreError> {
        self.validate_id(id)?;
        let dir = self.outputs_dir(id);
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.txt", sanitize_output_name(call_id)));
        fs::write(&path, text)?;
        Ok(path)
    }

    /// Create a new session file and append its `session_started` event.
    /// Returns the generated id together with the event that opened the log.
    pub fn create(&self, meta: &SessionStarted) -> Result<(String, Event), StoreError> {
        let id = ulid::Ulid::new().to_string();
        fs::create_dir_all(self.sessions_dir())?;
        let event = Event::new(1, &id, 0, EventKind::SessionStarted(meta.clone()));
        self.append(&event)?;
        Ok((id, event))
    }

    /// Create a new task/session and persist its initial root configuration.
    ///
    /// The legacy single-cwd session is migrated to one read-write primary
    /// `workspace` root. Callers that need a different root set can use
    /// [`Self::replace_roots`] or construct the events explicitly later.
    pub fn create_task(&self, meta: &SessionStarted) -> Result<CreatedTask, StoreError> {
        let (id, started) = self.create(meta)?;
        let task = Task::from_legacy_session(id.clone(), meta.cwd.clone())?;
        let configured = Event::new(
            2,
            &id,
            0,
            EventKind::TaskConfigured(TaskConfigured { roots: task.roots }),
        );
        if let Err(error) = self.append(&configured) {
            // 创建失败不能留下只写了 `session_started` 的半成品：否则下次
            // 刷新列表时它会以“凭空出现的空任务”混进任务列表。
            let _ = self.delete_session(&id);
            return Err(error);
        }
        Ok(CreatedTask {
            id,
            started,
            configured,
        })
    }

    /// Create a task with an explicit root set, including the empty set.
    pub fn create_task_with_roots(
        &self,
        meta: &SessionStarted,
        roots: Vec<Root>,
    ) -> Result<CreatedTask, StoreError> {
        let id = ulid::Ulid::new().to_string();
        let task = Task::new(id.clone(), roots)?;
        fs::create_dir_all(self.sessions_dir())?;
        let started = Event::new(1, &id, 0, EventKind::SessionStarted(meta.clone()));
        self.append(&started)?;
        let configured = Event::new(
            2,
            &id,
            0,
            EventKind::TaskConfigured(TaskConfigured { roots: task.roots }),
        );
        if let Err(error) = self.append(&configured) {
            // 同上：失败时把半成品日志和索引行一起清掉，避免空任务幽灵。
            let _ = self.delete_session(&id);
            return Err(error);
        }
        Ok(CreatedTask {
            id,
            started,
            configured,
        })
    }

    /// Replace a task's root list and append a `roots_changed` event.
    ///
    /// The caller is responsible for applying this at a turn boundary.
    pub fn replace_roots(&self, id: &str, roots: Vec<Root>) -> Result<Event, StoreError> {
        let task = Task::new(id.to_string(), roots)?;
        let _lock = self.lock(id)?;
        let events = self.load(id)?;
        let seq = events.last().map(|event| event.seq).unwrap_or(0) + 1;
        let turn = events.last().map(|event| event.turn).unwrap_or(0);
        let event = Event::new(
            seq,
            id,
            turn,
            EventKind::RootsChanged(RootsChanged { roots: task.roots }),
        );
        self.append(&event)?;
        Ok(event)
    }

    /// Rename a task. The title lives in the event log like every other
    /// durable task fact.
    pub fn rename_task(&self, id: &str, title: impl Into<String>) -> Result<Event, StoreError> {
        let mut task = self.task(id)?;
        task.rename(title)?;
        let title = task.title.expect("rename validates and sets a title");
        let _lock = self.lock(id)?;
        let events = self.load(id)?;
        let seq = events.last().map(|event| event.seq).unwrap_or(0) + 1;
        let turn = events.last().map(|event| event.turn).unwrap_or(0);
        let event = Event::new(seq, id, turn, EventKind::TaskRenamed(TaskRenamed { title }));
        self.append(&event)?;
        Ok(event)
    }

    /// Rebuild the Task view from the session log, falling back to a legacy
    /// single-root task when no task event exists.
    pub fn task(&self, id: &str) -> Result<Task, StoreError> {
        let events = self.load(id)?;
        self.task_from_events(id, &events)
    }

    /// Rebuild a Task from an already-loaded event slice.
    pub fn task_from_events(&self, id: &str, events: &[Event]) -> Result<Task, StoreError> {
        let Some(started) = events.first().and_then(|event| match &event.kind {
            EventKind::SessionStarted(started) => Some(started),
            _ => None,
        }) else {
            return Err(StoreError::NotFound(id.to_string()));
        };
        let mut task = Task::from_legacy_session(id.to_string(), started.cwd.clone())?;
        for event in events {
            let title = task.title.clone();
            match &event.kind {
                EventKind::TaskConfigured(configured) => {
                    task = Task::new(id.to_string(), configured.roots.clone())?;
                }
                EventKind::RootsChanged(changed) => {
                    task = Task::new(id.to_string(), changed.roots.clone())?;
                }
                EventKind::TaskRenamed(renamed) => {
                    task.rename(renamed.title.clone())?;
                    continue;
                }
                _ => {}
            }
            task.title = title;
        }
        Ok(task)
    }

    pub fn append(&self, event: &Event) -> Result<(), StoreError> {
        self.validate_id(&event.session)?;
        let path = self.session_path(&event.session);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        serde_json::to_writer(&mut file, event)?;
        file.write_all(b"\n")?;
        file.flush()?;
        // Durability point: a finished turn must survive a crash. Mid-turn
        // events are replayed up to the last complete line.
        if matches!(event.kind, EventKind::TurnEnded(_)) {
            file.sync_all()?;
        }
        // The index is a cache: a failure must never fail a session write.
        if matches!(
            &event.kind,
            EventKind::SessionStarted(_) | EventKind::TurnEnded(_)
        ) && let Err(err) = self.index_event(event)
        {
            tracing::warn!(%err, session = %event.session, "failed to update session index");
        }
        Ok(())
    }

    /// Take the single-writer lock for a session (`locks/<id>.lock`).
    pub fn lock(&self, id: &str) -> Result<SessionLock, StoreError> {
        self.validate_id(id)?;
        let dir = self.root.join("locks");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{id}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;

        #[cfg(unix)]
        let lock = {
            use std::os::fd::AsRawFd;

            // SAFETY: `flock` only touches this file description.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    return Err(StoreError::Locked(id.to_string()));
                }
                return Err(StoreError::Io(err));
            }
            SessionLock { _file: file }
        };

        #[cfg(not(unix))]
        let lock = {
            let _ = file;
            SessionLock {}
        };

        Ok(lock)
    }

    /// Append `interrupted` results for tool calls whose result never landed
    /// (a crash or kill between `tool_call` and `tool_result`). Idempotent.
    pub fn repair_interrupted(&self, id: &str) -> Result<Vec<Event>, StoreError> {
        let events = self.load(id)?;
        let completed: HashSet<&str> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::ToolResult(result) => Some(result.call_id.as_str()),
                _ => None,
            })
            .collect();

        let mut seq = events.last().map(|event| event.seq).unwrap_or(0);
        let mut repaired = Vec::new();
        for event in &events {
            let EventKind::ToolCall(call) = &event.kind else {
                continue;
            };
            if completed.contains(call.call_id.as_str()) {
                continue;
            }
            seq += 1;
            repaired.push(Event::new(
                seq,
                id,
                event.turn,
                EventKind::ToolResult(ToolResult {
                    call_id: call.call_id.clone(),
                    status: ToolStatus::Interrupted,
                    output: "interrupted: the previous run stopped before this tool finished"
                        .to_string(),
                    metadata: Default::default(),
                    diff: None,
                }),
            ));
        }

        for event in &repaired {
            self.append(event)?;
        }
        if repaired.is_empty() {
            Ok(events)
        } else {
            self.load(id)
        }
    }

    pub fn load(&self, id: &str) -> Result<Vec<Event>, StoreError> {
        self.validate_id(id)?;
        let path = self.session_path(id);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(id.to_string()));
            }
            Err(err) => return Err(StoreError::Io(err)),
        };

        let lines: Vec<String> = BufReader::new(file).lines().collect::<Result<_, _>>()?;
        let total = lines.len();
        let mut events: Vec<Event> = Vec::new();

        for (index, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(line) {
                Ok(event) => {
                    if let Some(last) = events.last() {
                        let expected = last.seq + 1;
                        if event.seq != expected {
                            return Err(StoreError::Seq {
                                expected,
                                got: event.seq,
                            });
                        }
                    }
                    events.push(event);
                }
                // A crash can leave a partial final line. Drop only that one.
                Err(_) if index + 1 == total => break,
                Err(err) => return Err(StoreError::Json(err)),
            }
        }

        Ok(events)
    }

    pub fn list(&self) -> Result<Vec<SessionSummary>, StoreError> {
        match self.list_indexed() {
            Ok(Some(summaries)) => return Ok(summaries),
            Ok(None) => {}
            Err(err) => tracing::warn!(%err, "session index unavailable; scanning"),
        }

        let summaries = self.scan_sessions()?;
        if let Err(err) = self.rebuild_index(&summaries) {
            tracing::warn!(%err, "failed to rebuild the session index");
        }
        Ok(summaries)
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.sqlite")
    }

    fn open_index(&self) -> Result<Connection, StoreError> {
        fs::create_dir_all(&self.root)?;
        let conn = Connection::open(self.index_path())?;
        // 引擎线程在写、UI 线程在列任务（list_indexed）是并发访问同一个
        // sqlite 文件；不加 busy handler 时锁一重叠就直接 SQLITE_BUSY，
        // 索引更新失败被静默吞掉，出现 last_seq/turns 不一致的脏行。
        conn.busy_timeout(std::time::Duration::from_millis(500))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                started_at TEXT NOT NULL,
                model TEXT NOT NULL,
                provider TEXT NOT NULL,
                cwd TEXT NOT NULL,
                version TEXT NOT NULL,
                turns INTEGER NOT NULL,
                last_seq INTEGER NOT NULL,
                cost_usd REAL NOT NULL DEFAULT 0
            );",
        )?;
        // Add the cost column to indexes created before it existed.
        let _ = conn.execute(
            "ALTER TABLE sessions ADD COLUMN cost_usd REAL NOT NULL DEFAULT 0",
            [],
        );
        // Scheduler run state (ADR-0059 §1). It lives here, never in the config
        // file: the config is watched by mtime, so writing it on every fire
        // would rebuild the engine each time and race with the Settings window.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schedules (
                name TEXT PRIMARY KEY,
                last_fired_at TEXT,
                next_fire_at TEXT,
                last_status TEXT,
                missed_windows INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        Ok(conn)
    }

    fn index_event(&self, event: &Event) -> Result<(), StoreError> {
        match &event.kind {
            EventKind::SessionStarted(meta) => {
                let conn = self.open_index()?;
                conn.execute(
                    "INSERT OR REPLACE INTO sessions
                     (id, started_at, model, provider, cwd, version, turns, last_seq, cost_usd)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
                    params![
                        event.session,
                        event.ts.to_string(),
                        meta.model,
                        meta.provider,
                        meta.cwd,
                        meta.version,
                        event.turn,
                        event.seq,
                    ],
                )?;
            }
            EventKind::TurnEnded(ended) => {
                let conn = self.open_index()?;
                conn.execute(
                    "UPDATE sessions SET turns = ?1, last_seq = ?2,
                     cost_usd = cost_usd + ?3 WHERE id = ?4",
                    params![event.turn, event.seq, ended.cost_usd, event.session],
                )?;
            }
            EventKind::TaskConfigured(_) | EventKind::RootsChanged(_) => {
                let conn = self.open_index()?;
                conn.execute(
                    "UPDATE sessions SET last_seq = ?1 WHERE id = ?2",
                    params![event.seq, event.session],
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    fn count_session_files(&self) -> Result<usize, StoreError> {
        let dir = self.sessions_dir();
        if !dir.exists() {
            return Ok(0);
        }
        let mut count = 0;
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                count += 1;
            }
        }
        Ok(count)
    }

    fn list_indexed(&self) -> Result<Option<Vec<SessionSummary>>, StoreError> {
        if !self.index_path().exists() {
            return Ok(None);
        }
        let conn = Connection::open(self.index_path())?;
        conn.busy_timeout(std::time::Duration::from_millis(500))?;
        let mut stmt = conn.prepare(
            "SELECT id, started_at, model, provider, cwd, version, turns, last_seq, cost_usd
             FROM sessions ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                started: SessionStarted {
                    model: row.get(2)?,
                    provider: row.get(3)?,
                    cwd: row.get(4)?,
                    version: row.get(5)?,
                },
                started_at: row.get(1)?,
                turns: row.get(6)?,
                last_seq: row.get(7)?,
                cost_usd: row.get(8)?,
            })
        })?;
        let mut summaries = Vec::new();
        for row in rows {
            summaries.push(row?);
        }
        // A missing row means the index is stale; rebuild from the logs.
        if summaries.len() == self.count_session_files()? {
            Ok(Some(summaries))
        } else {
            Ok(None)
        }
    }

    fn scan_sessions(&self) -> Result<Vec<SessionSummary>, StoreError> {
        let dir = self.sessions_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut summaries = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(events) = self.load(id) else {
                continue;
            };
            let Some(first) = events.first() else {
                continue;
            };
            let EventKind::SessionStarted(started) = &first.kind else {
                continue;
            };
            let cost_usd = events
                .iter()
                .filter_map(|event| match &event.kind {
                    EventKind::TurnEnded(ended) => Some(ended.cost_usd),
                    _ => None,
                })
                .sum();
            summaries.push(SessionSummary {
                id: id.to_string(),
                started: started.clone(),
                started_at: first.ts.to_string(),
                turns: events.last().map(|event| event.turn).unwrap_or(0),
                last_seq: events.last().map(|event| event.seq).unwrap_or(0),
                cost_usd,
            });
        }

        summaries.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(summaries)
    }

    fn rebuild_index(&self, summaries: &[SessionSummary]) -> Result<(), StoreError> {
        let mut conn = self.open_index()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM sessions", [])?;
        for summary in summaries {
            tx.execute(
                "INSERT OR REPLACE INTO sessions
                 (id, started_at, model, provider, cwd, version, turns, last_seq, cost_usd)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    summary.id,
                    summary.started_at,
                    summary.started.model,
                    summary.started.provider,
                    summary.started.cwd,
                    summary.started.version,
                    summary.turns,
                    summary.last_seq,
                    summary.cost_usd,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Conversation fork: copy events up to and including `at_seq` into a new
    /// session, renumbering `seq` and `session`. Files on disk are untouched.
    ///
    /// The new session can be resumed like any other; `None` forks the whole
    /// conversation.
    pub fn fork(&self, source: &str, at_seq: Option<u64>) -> Result<String, StoreError> {
        let events = self.load(source)?;
        let last = events.last().map(|event| event.seq).unwrap_or(0);
        let boundary = at_seq.unwrap_or(last);
        if boundary == 0 || boundary > last || !events.iter().any(|event| event.seq == boundary) {
            return Err(StoreError::InvalidFork { boundary, last });
        }

        let id = ulid::Ulid::new().to_string();
        fs::create_dir_all(self.sessions_dir())?;
        for (index, event) in events
            .iter()
            .filter(|event| event.seq <= boundary)
            .enumerate()
        {
            let mut copy = event.clone();
            copy.session = id.clone();
            copy.seq = index as u64 + 1;
            if let Err(err) = self.append(&copy) {
                // Do not leave a half-written fork behind.
                let _ = fs::remove_file(self.session_path(&id));
                return Err(err);
            }
        }
        Ok(id)
    }

    /// Sessions whose last event is older than `cutoff`.
    fn expired_sessions(&self, cutoff: OffsetDateTime) -> Result<Vec<String>, StoreError> {
        let mut ids = Vec::new();
        let dir = self.sessions_dir();
        if !dir.exists() {
            return Ok(ids);
        }
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(events) = self.load(id) else {
                continue;
            };
            let Some(last) = events.last() else {
                continue;
            };
            if last.ts < cutoff {
                ids.push(id.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Delete one session: log, per-session outputs directory and lock file.
    pub fn delete_session(&self, id: &str) -> Result<(), StoreError> {
        self.validate_id(id)?;
        let path = self.session_path(id);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        let dir = self.sessions_dir().join(id);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        let lock = self.root.join("locks").join(format!("{id}.lock"));
        if lock.exists() {
            fs::remove_file(lock)?;
        }
        if let Ok(conn) = self.open_index() {
            let _ = conn.execute("DELETE FROM sessions WHERE id = ?1", params![id]);
        }
        // 联动清理投递到该会话的运行时定时任务（ADR-0063）：目标会话没了，任务
        // 失去投递目标。放在 store 层保证所有删除入口（GUI、gc 过期清理）一致，
        // 不留孤儿任务；保存失败只记日志，不阻断会话删除。
        let mut tasks = self.load_schedule_tasks();
        let before = tasks.len();
        tasks.retain(|task| task.target.session_id() != Some(id));
        if tasks.len() != before
            && let Err(error) = self.save_schedule_tasks(&tasks)
        {
            eprintln!("qcode-core: cannot prune schedule tasks for deleted session {id}: {error}");
        }
        Ok(())
    }

    /// Remove sessions idle for `older_than_days`; `delete = false` reports only.
    pub fn gc(&self, older_than_days: u64, delete: bool) -> Result<Vec<String>, StoreError> {
        let days = i64::try_from(older_than_days).unwrap_or(i64::MAX);
        let cutoff = OffsetDateTime::now_utc() - Duration::days(days);
        let ids = self.expired_sessions(cutoff)?;
        if delete {
            for id in &ids {
                self.delete_session(id)?;
            }
        }
        Ok(ids)
    }

    /// Every schedule's stored run state, keyed by schedule name.
    ///
    /// An entry that is no longer declared in the config is simply not looked
    /// up; [`SessionStore::prune_schedules`] drops those rows.
    pub fn schedule_states(&self) -> Result<BTreeMap<String, ScheduleState>, StoreError> {
        let conn = self.open_index()?;
        let mut stmt = conn.prepare(
            "SELECT name, last_fired_at, next_fire_at, last_status, missed_windows
             FROM schedules ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut states = BTreeMap::new();
        for row in rows {
            let (name, last_fired_at, next_fire_at, last_status, missed_windows) = row?;
            states.insert(
                name,
                ScheduleState {
                    last_fired_at: last_fired_at.as_deref().and_then(parse_wall_clock),
                    next_fire_at: next_fire_at.as_deref().and_then(parse_wall_clock),
                    last_status: last_status.as_deref().and_then(ScheduleStatus::parse),
                    missed_windows: missed_windows.clamp(0, u32::MAX as i64) as u32,
                },
            );
        }
        Ok(states)
    }

    /// Write one schedule's run state, replacing whatever the row held.
    pub fn save_schedule_state(&self, name: &str, state: &ScheduleState) -> Result<(), StoreError> {
        let conn = self.open_index()?;
        conn.execute(
            "INSERT INTO schedules (name, last_fired_at, next_fire_at, last_status, missed_windows)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(name) DO UPDATE SET
                last_fired_at = excluded.last_fired_at,
                next_fire_at = excluded.next_fire_at,
                last_status = excluded.last_status,
                missed_windows = excluded.missed_windows",
            params![
                name,
                state.last_fired_at.map(format_wall_clock),
                state.next_fire_at.map(format_wall_clock),
                state.last_status.as_ref().map(ScheduleStatus::as_str),
                i64::from(state.missed_windows),
            ],
        )?;
        Ok(())
    }

    /// Forget the rows of schedules that are no longer in the config. Returns
    /// how many went away.
    pub fn prune_schedules(&self, declared: &[String]) -> Result<usize, StoreError> {
        let conn = self.open_index()?;
        let mut stmt = conn.prepare("SELECT name FROM schedules")?;
        let stored: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        let mut removed = 0;
        for name in stored {
            if !declared.contains(&name) {
                conn.execute("DELETE FROM schedules WHERE name = ?1", params![name])?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    // ==== 运行时定时任务（Scheduled Tasks）存储 ====
    //
    // 任务定义存 `<root>/schedule/tasks.json`（JSON 数组，原子写）。与会话日志
    // （JSONL）不同，任务是 host 级资源，不属于任何会话，独立文件即可。

    pub fn schedule_tasks_path(&self) -> PathBuf {
        self.root.join("schedule").join("tasks.json")
    }

    /// 容错读取任务定义：文件缺失 / 损坏都视为空（下次保存会覆盖坏数据）。
    pub fn load_schedule_tasks(&self) -> Vec<ScheduledTask> {
        let Ok(text) = fs::read_to_string(self.schedule_tasks_path()) else {
            return Vec::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// 原子写任务定义：先写临时文件再 rename。
    pub fn save_schedule_tasks(&self, tasks: &[ScheduledTask]) -> Result<(), StoreError> {
        let path = self.schedule_tasks_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(tasks)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// 目标会话日志里是否已经出现某条投递文案对应的 `user/message` 事件。
    ///
    /// 崩溃恢复 / 投递确认用它区分"投过没投过"。主消息（follow-up）落盘时
    /// `queued_id` 为 `None`（只有 steering 消息才带 id），所以按文案匹配：
    /// 定时投递的 framing 文案（`[SCHEDULED TASK REMINDER] (id) …`）是任务
    /// 独有的，与用户输入撞车的概率可以忽略。
    pub fn schedule_delivery_landed(
        &self,
        session_id: &str,
        prompt: &str,
    ) -> Result<bool, StoreError> {
        let events = self.load(session_id)?;
        Ok(events.iter().any(|event| {
            matches!(
                &event.kind,
                EventKind::UserMessage(message) if message.text == prompt
            )
        }))
    }

    fn validate_id(&self, id: &str) -> Result<(), StoreError> {
        let ok = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric());
        if ok {
            Ok(())
        } else {
            Err(StoreError::InvalidId(id.to_string()))
        }
    }
}

/// Tool call ids are model-controlled; keep them from escaping `outputs/`.
fn sanitize_output_name(call_id: &str) -> String {
    let name: String = call_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let name = name.trim_matches('.').to_string();
    if name.is_empty() {
        "output".to_string()
    } else {
        name.chars().take(120).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ToolCall, TurnEnded, TurnStarted, TurnStatus, Usage, UserMessage};
    use crate::task::{Root, RootAccess};

    fn meta() -> SessionStarted {
        SessionStarted {
            model: "mock-1".to_string(),
            provider: "mock".to_string(),
            cwd: "/tmp".to_string(),
            version: "0.1.0".to_string(),
        }
    }

    fn seed(store: &SessionStore) -> String {
        let (id, _) = store.create(&meta()).expect("create");
        store
            .append(&Event::new(
                2,
                &id,
                1,
                EventKind::TurnStarted(TurnStarted {
                    input: "hi".to_string(),
                }),
            ))
            .expect("turn started");
        store
            .append(&Event::new(
                3,
                &id,
                1,
                EventKind::UserMessage(UserMessage {
                    starts_segment: false,
                    queued_id: None,
                    text: "hi".to_string(),
                    attachments: Vec::new(),

                    continuation: None,
                    question_reply: None,
                    scheduled: None,
                }),
            ))
            .expect("user message");
        store
            .append(&Event::new(
                4,
                &id,
                1,
                EventKind::UserMessage(UserMessage {
                    starts_segment: false,
                    queued_id: None,
                    text: "again".to_string(),
                    attachments: Vec::new(),

                    continuation: None,
                    question_reply: None,
                    scheduled: None,
                }),
            ))
            .expect("user message");
        id
    }

    #[test]
    fn fork_copies_a_prefix_under_a_new_id() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let source = seed(&store);

        let fork = store.fork(&source, Some(3)).expect("fork");

        assert_ne!(fork, source);
        assert_eq!(store.load(&source).expect("source").len(), 4);
        let copied = store.load(&fork).expect("fork session");
        assert_eq!(copied.len(), 3);
        assert!(copied.iter().all(|event| event.session == fork));
        assert_eq!(
            copied.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(matches!(&copied[1].kind, EventKind::TurnStarted(_)));
        assert_eq!(copied[1].turn, 1);
    }

    #[test]
    fn fork_without_a_boundary_copies_everything() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let source = seed(&store);

        let fork = store.fork(&source, None).expect("fork");
        assert_eq!(store.load(&fork).expect("fork session").len(), 4);
    }

    #[test]
    fn fork_rejects_a_boundary_outside_the_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let source = seed(&store);

        assert!(matches!(
            store.fork(&source, Some(0)),
            Err(StoreError::InvalidFork { .. })
        ));
        assert!(matches!(
            store.fork(&source, Some(99)),
            Err(StoreError::InvalidFork { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn second_writer_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let source = seed(&store);

        let lock = store.lock(&source).expect("first lock");
        assert!(matches!(store.lock(&source), Err(StoreError::Locked(_))));
        drop(lock);
        assert!(store.lock(&source).is_ok());
    }

    #[test]
    fn repair_marks_dangling_tool_calls_interrupted_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let (id, _) = store.create(&meta()).expect("create");
        store
            .append(&Event::new(
                2,
                &id,
                1,
                EventKind::ToolCall(ToolCall {
                    call_id: "c1".to_string(),
                    name: "run_shell".to_string(),
                    arguments: serde_json::json!({"command": "true"}),
                }),
            ))
            .expect("tool call");

        let repaired = store.repair_interrupted(&id).expect("repair");
        assert_eq!(repaired.len(), 3);
        match &repaired[2].kind {
            EventKind::ToolResult(result) => {
                assert_eq!(result.call_id, "c1");
                assert_eq!(result.status, ToolStatus::Interrupted);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(store.repair_interrupted(&id).expect("idempotent").len(), 3);
    }

    #[test]
    fn gc_reports_then_deletes_idle_sessions() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let source = seed(&store);

        let candidates = store.gc(0, false).expect("dry run");
        assert!(candidates.contains(&source));
        assert!(store.session_path(&source).exists());

        let deleted = store.gc(0, true).expect("delete");
        assert!(deleted.contains(&source));
        assert!(!store.session_path(&source).exists());
    }

    fn root(label: &str, path: &str, access: RootAccess, primary: bool) -> Root {
        Root {
            id: format!("{label}-root"),
            label: label.to_string(),
            path: std::path::PathBuf::from(path),
            access,
            primary,
        }
    }

    #[test]
    fn create_task_writes_initial_config_and_reconstructs() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let created = store.create_task(&meta()).expect("create task");
        assert_eq!(created.id, created.started.session);
        let events = store.load(&created.id).expect("load");
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].kind, EventKind::SessionStarted(_)));
        assert!(matches!(events[1].kind, EventKind::TaskConfigured(_)));
        let task = store.task(&created.id).expect("task");
        assert_eq!(task.id, created.id);
        assert_eq!(task.roots.len(), 1);
        assert_eq!(task.roots[0].label, "workspace");
        assert!(task.roots[0].primary);
    }

    #[test]
    fn empty_task_is_created_with_no_roots() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let created = store
            .create_task_with_roots(&meta(), Vec::new())
            .expect("empty task");
        let task = store.task(&created.id).expect("task");
        assert!(task.roots.is_empty());
        assert!(task.primary_root().is_none());
        let events = store.load(&created.id).expect("load");
        assert_eq!(events.len(), 2);
        assert!(matches!(events[1].kind, EventKind::TaskConfigured(_)));
    }

    #[test]
    fn rename_task_survives_root_changes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let created = store.create_task(&meta()).expect("create task");
        store
            .rename_task(&created.id, "  My Task  ")
            .expect("rename task");

        let task = store.task(&created.id).expect("task");
        assert_eq!(task.title(), Some("My Task"));

        store
            .replace_roots(
                &created.id,
                vec![root("code", "/src", RootAccess::ReadWrite, true)],
            )
            .expect("replace roots");
        let task = store.task(&created.id).expect("task");
        assert_eq!(task.title(), Some("My Task"));
        assert_eq!(task.roots[0].label, "code");
    }

    #[test]
    fn replace_roots_appends_history_and_updates_task() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let created = store.create_task(&meta()).expect("create task");
        let event = store
            .replace_roots(
                &created.id,
                vec![
                    root("code", "/src", RootAccess::ReadWrite, true),
                    root("docs", "/docs", RootAccess::ReadOnly, false),
                ],
            )
            .expect("replace roots");
        assert!(matches!(event.kind, EventKind::RootsChanged(_)));
        let task = store.task(&created.id).expect("task");
        assert_eq!(task.roots.len(), 2);
        assert_eq!(task.roots[0].label, "code");
        assert_eq!(task.roots[1].access, RootAccess::ReadOnly);
        assert!(task.primary_root().is_some());
    }

    #[test]
    fn legacy_session_migrates_to_a_single_root_task() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let id = seed(&store);
        let listed = store.list().expect("list");
        let summary = listed.iter().find(|item| item.id == id).expect("summary");
        let task = summary.legacy_task().expect("task");
        assert_eq!(task.id, id);
        assert_eq!(task.session_id(), id);
        assert_eq!(task.roots.len(), 1);
        assert_eq!(task.roots[0].label, "workspace");
        assert_eq!(task.roots[0].path, std::path::PathBuf::from("/tmp"));
        assert!(task.roots[0].primary);
    }

    #[test]
    fn index_lists_sessions_and_rebuilds_from_logs() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let source = seed(&store);
        store
            .append(&Event::new(
                5,
                &source,
                1,
                EventKind::TurnEnded(TurnEnded {
                    status: TurnStatus::Completed,
                    usage: Usage::default(),
                    cost_usd: 0.25,

                    pause: None,
                }),
            ))
            .expect("turn ended");

        let listed = store.list().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].turns, 1);
        assert!((listed[0].cost_usd - 0.25).abs() < 1e-9);
        assert!(!listed[0].started_at.is_empty());
        assert!(store.root().join("index.sqlite").exists());

        // Losing the index is harmless: the next list rebuilds it.
        std::fs::remove_file(store.root().join("index.sqlite")).expect("remove index");
        let rebuilt = store.list().expect("rebuild");
        assert_eq!(rebuilt.len(), 1);
        assert!(store.root().join("index.sqlite").exists());

        store.delete_session(&source).expect("delete");
        assert!(store.list().expect("after delete").is_empty());
    }

    #[test]
    fn a_schedule_state_round_trips_through_the_index() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = SessionStore::new(dir.path());
        let at = crate::schedule::at(2026, time::Month::June, 2, 3, 0);
        let state = ScheduleState {
            last_fired_at: Some(at),
            next_fire_at: Some(at + Duration::days(1)),
            last_status: Some(ScheduleStatus::Fired),
            missed_windows: 2,
        };
        store
            .save_schedule_state("nightly", &state)
            .expect("save state");

        let loaded = store.schedule_states().expect("states");
        assert_eq!(loaded["nightly"], state);
        assert_eq!(loaded["nightly"].missed_windows, 2);
    }

    #[test]
    fn saving_twice_replaces_the_row_instead_of_duplicating_it() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = SessionStore::new(dir.path());
        let at = crate::schedule::at(2026, time::Month::June, 2, 3, 0);
        store
            .save_schedule_state(
                "nightly",
                &ScheduleState {
                    next_fire_at: Some(at),
                    ..ScheduleState::default()
                },
            )
            .expect("first");
        store
            .save_schedule_state(
                "nightly",
                &ScheduleState {
                    next_fire_at: Some(at),
                    last_status: Some(ScheduleStatus::Skipped),
                    ..ScheduleState::default()
                },
            )
            .expect("second");

        let loaded = store.schedule_states().expect("states");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["nightly"].last_status, Some(ScheduleStatus::Skipped));
    }

    #[test]
    fn a_status_with_a_payload_survives_the_text_column() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = SessionStore::new(dir.path());
        for status in [
            ScheduleStatus::Missed(7),
            ScheduleStatus::Failed("no session".to_string()),
            ScheduleStatus::Orphaned,
        ] {
            store
                .save_schedule_state(
                    "nightly",
                    &ScheduleState {
                        last_status: Some(status.clone()),
                        ..ScheduleState::default()
                    },
                )
                .expect("save");
            assert_eq!(
                store.schedule_states().expect("states")["nightly"].last_status,
                Some(status)
            );
        }
    }

    #[test]
    fn pruning_drops_only_schedules_that_left_the_config() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = SessionStore::new(dir.path());
        for name in ["kept", "gone"] {
            store
                .save_schedule_state(name, &ScheduleState::default())
                .expect("save");
        }
        let removed = store.prune_schedules(&["kept".to_string()]).expect("prune");
        assert_eq!(removed, 1);
        let names: Vec<String> = store
            .schedule_states()
            .expect("states")
            .into_keys()
            .collect();
        assert_eq!(names, vec!["kept".to_string()]);
    }

    #[test]
    fn an_unreadable_stored_time_is_dropped_rather_than_fatal() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = SessionStore::new(dir.path());
        let conn = store.open_index().expect("index");
        conn.execute(
            "INSERT INTO schedules (name, next_fire_at, missed_windows) VALUES (?1, ?2, 0)",
            params!["hand-edited", "not a timestamp"],
        )
        .expect("insert");
        let states = store.schedule_states().expect("states");
        assert_eq!(states["hand-edited"].next_fire_at, None);
    }

    fn scheduled_task(id: &str, target: crate::schedule::Target) -> crate::schedule::ScheduledTask {
        let stakeholders = target.session_id().map(str::to_owned).into_iter().collect();
        crate::schedule::ScheduledTask {
            id: id.to_string(),
            title: id.to_string(),
            prompt: "run the review".to_string(),
            rule: crate::schedule::Rule::Daily { hour: 9, minute: 0 },
            target,
            overlap: crate::schedule::Overlap::Queue,
            approval_timeout: None,
            status: crate::schedule::TaskStatus::Active,
            stakeholders,
            last_receipt: None,
            delivery_log: Vec::new(),
            earlier_records_pruned: false,
            reserved_session_id: None,
            next_fire_at: None,
            rearm_after: std::time::Duration::ZERO,
        }
    }

    #[test]
    fn delete_session_prunes_tasks_that_deliver_to_it() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = SessionStore::new(dir.path());
        let doomed = seed(&store);
        let kept = seed(&store);
        let tasks = vec![
            scheduled_task(
                "t1",
                crate::schedule::Target::Session { id: doomed.clone() },
            ),
            scheduled_task("t2", crate::schedule::Target::Session { id: kept.clone() }),
            scheduled_task("t3", crate::schedule::Target::NewOnce),
        ];
        store.save_schedule_tasks(&tasks).expect("save tasks");

        store.delete_session(&doomed).expect("delete session");

        let remaining = store.load_schedule_tasks();
        assert_eq!(remaining.len(), 2);
        // 指向被删会话的任务被联动清理。
        assert!(remaining.iter().all(|task| task.id != "t1"));
        // 指向其他会话 / 新建目标的任务不受影响。
        assert!(remaining.iter().any(|task| task.id == "t2"));
        assert!(remaining.iter().any(|task| task.id == "t3"));
    }
}
