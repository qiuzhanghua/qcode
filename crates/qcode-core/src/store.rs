//! Append-only JSONL session store.
//!
//! One file per session under `<root>/sessions/<id>.jsonl`. Writes are appends;
//! a torn final line is discarded on load, which is the crash-recovery contract.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use time::{Duration, OffsetDateTime};

use crate::event::{
    Event, EventKind, RootsChanged, SessionStarted, TaskConfigured, TaskRenamed, ToolResult,
    ToolStatus,
};
use crate::task::{Root, Task, TaskError};

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
        self.append(&configured)?;
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
        self.append(&configured)?;
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
}
