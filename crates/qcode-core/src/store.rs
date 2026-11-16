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
    Event, EventKind, RootsChanged, SessionStarted, TaskConfigured, TaskRenamed, TitleGenerated,
    TitleSource, ToolResult, ToolStatus,
};
use crate::schedule::ScheduledTask;
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
    #[cfg(any(unix, windows))]
    _file: fs::File,
}

/// Held while rewriting a shared JSON file read-modify-write style.
///
/// Unlike [`SessionLock`] this one **waits** instead of failing: the critical
/// section is "read a small JSON array, change it, write it back", which is
/// milliseconds. Refusing to wait there would turn two frontends touching the
/// same data directory into a `Locked` error the user has to retry by hand,
/// and the whole point of ADR-0088 §5 is that they must both succeed.
#[derive(Debug)]
pub struct FileLock {
    #[cfg(any(unix, windows))]
    _file: fs::File,
}

/// Take an exclusive lock on `file`, waiting for whoever holds it.
#[cfg(unix)]
fn lock_exclusive_blocking(file: &fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    // SAFETY: `file` is alive for the whole call, so the fd is valid.
    // No `LOCK_NB`: we *want* to wait (see [`FileLock`]).
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

/// Same, through `LockFileEx` — and again **without**
/// `LOCKFILE_FAIL_IMMEDIATELY`, which is the whole difference from the GUI's
/// single-instance lock (ADR-0083).
#[cfg(windows)]
fn lock_exclusive_blocking(file: &fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LockFileEx};

    // SAFETY: `overlapped` is a valid stack OVERLAPPED; in synchronous use the
    // kernel only reads its offset (all zero = from byte 0). One byte is enough
    // to express "exclusive".
    let mut overlapped = unsafe { std::mem::zeroed() };
    let handle: HANDLE = file.as_raw_handle() as HANDLE;
    let rc = unsafe { LockFileEx(handle, LOCKFILE_EXCLUSIVE_LOCK, 0, 1, 0, &mut overlapped) };
    if rc != 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

#[cfg(not(any(unix, windows)))]
fn lock_exclusive_blocking(_file: &fs::File) -> std::io::Result<()> {
    // No advisory-lock primitive we know of: the honest answer is to say so
    // rather than silently pretend the file is protected.
    Err(std::io::Error::other(
        "no cross-process file lock on this platform",
    ))
}

/// Try to take the same lock **without waiting**. `Ok(false)` means someone else
/// holds it — an answer, not an error (ADR-0091).
#[cfg(unix)]
fn lock_exclusive_immediate(file: &fs::File) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;

    // SAFETY: `file` is alive for the whole call, so the fd is valid.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }
    Err(error)
}

#[cfg(windows)]
fn lock_exclusive_immediate(file: &fs::File) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, GetLastError, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };

    // SAFETY: as in `lock_exclusive_blocking`.
    let mut overlapped = unsafe { std::mem::zeroed() };
    let handle: HANDLE = file.as_raw_handle() as HANDLE;
    let rc = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if rc != 0 {
        return Ok(true);
    }
    // `Error::other`, not `Error::new(ErrorKind::Other, ..)`: clippy's
    // `io_other_error` lint flags the long form, and it only fires on the
    // Windows build — which is exactly the shape that shipped a broken v0.11.0.
    let code = unsafe { GetLastError() };
    if code == ERROR_LOCK_VIOLATION {
        return Ok(false);
    }
    Err(std::io::Error::other(format!(
        "LockFileEx failed with {code}"
    )))
}

#[cfg(not(any(unix, windows)))]
fn lock_exclusive_immediate(_file: &fs::File) -> std::io::Result<bool> {
    Err(std::io::Error::other(
        "no cross-process file lock on this platform",
    ))
}

/// Non-blocking flavour: the session single-writer lock, where "someone else
/// already has it" is an answer rather than a reason to wait.
#[cfg(windows)]
fn take_immediate(file: fs::File) -> std::io::Result<SessionLock> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, GetLastError, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };

    // SAFETY: as in `lock_exclusive_blocking`.
    let mut overlapped = unsafe { std::mem::zeroed() };
    let handle: HANDLE = file.as_raw_handle() as HANDLE;
    let rc = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if rc != 0 {
        return Ok(SessionLock { _file: file });
    }
    let code = unsafe { GetLastError() };
    if code == ERROR_LOCK_VIOLATION {
        // `Error::new`, not `From<ErrorKind>`: the latter takes no message, and
        // passing one is a type error that only the Windows build ever sees.
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "locked",
        ));
    }
    Err(std::io::Error::from_raw_os_error(code as i32))
}

#[cfg(not(any(unix, windows)))]
fn take_immediate(_file: fs::File) -> std::io::Result<SessionLock> {
    Ok(SessionLock {})
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
    ///
    /// Root ids are re-keyed to `{this id}-{label}`, which is the convention
    /// every other writer follows. Callers that build a root set by *borrowing*
    /// another session's (a sub-agent inheriting its parent's roots, ADR-0087)
    /// would otherwise leave ids that name a session the root no longer belongs
    /// to — harmless today (ids are only unique-within-a-task) but exactly the
    /// kind of shared identity that turns into a cross-task collision the first
    /// time someone caches by root id.
    pub fn create_task_with_roots(
        &self,
        meta: &SessionStarted,
        roots: Vec<Root>,
    ) -> Result<CreatedTask, StoreError> {
        let id = ulid::Ulid::new().to_string();
        let roots = roots
            .into_iter()
            .map(|root| Root {
                id: format!("{id}-{}", root.label),
                ..root
            })
            .collect();
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

    /// Append a model-or-fallback session title (会话标题自动生成设计). Unlike
    /// [`Self::rename_task`] it does not touch the Task aggregate — a generated
    /// title is a display fact, not a user rename.
    pub fn append_title_generated(
        &self,
        id: &str,
        title: String,
        message_seqs: Vec<u64>,
        source: TitleSource,
    ) -> Result<Event, StoreError> {
        let _lock = self.lock(id)?;
        let events = self.load(id)?;
        let seq = events.last().map(|event| event.seq).unwrap_or(0) + 1;
        let turn = events.last().map(|event| event.turn).unwrap_or(0);
        let event = Event::new(
            seq,
            id,
            turn,
            EventKind::TitleGenerated(TitleGenerated {
                title,
                message_seqs,
                source,
            }),
        );
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
            let _ = id;
            // 与 Unix 分支一致：锁冲突（WouldBlock）映射为 `StoreError::Locked`，
            // 而不是 `Io`——调用方（如标题生成的等锁重试）靠这个变体区分
            // 「被占用」与「真正读写失败」。
            match take_immediate(file) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return Err(StoreError::Locked(id.to_string()));
                }
                other => other?,
            }
        };

        Ok(lock)
    }

    /// Take an exclusive cross-process lock on a shared file, **waiting** for it.
    ///
    /// Used by the schedule task store, whose writes are read-modify-write of a
    /// JSON array: two frontends (GUI driving, TUI registering) may touch it at
    /// once, and a lost update there is silent data loss (ADR-0088 §5).
    pub fn lock_file_blocking(&self, name: &str) -> Result<FileLock, StoreError> {
        let dir = self.root.join("locks");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{name}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        lock_exclusive_blocking(&file)?;
        Ok(FileLock { _file: file })
    }

    /// Try to take `locks/<name>.lock` **without waiting**.
    ///
    /// `Ok(None)` means another process holds it. That is an answer rather than
    /// a failure, which is why this returns an `Option` instead of an error —
    /// the caller asks "am I the one?" and both replies are normal (ADR-0091).
    ///
    /// The lock is released by the kernel when the holder dies, so holding it is
    /// proof of life; no cleanup code and no pid file.
    pub fn try_lock_file(&self, name: &str) -> Result<Option<FileLock>, StoreError> {
        let dir = self.root.join("locks");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{name}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        if lock_exclusive_immediate(&file)? {
            return Ok(Some(FileLock { _file: file }));
        }
        Ok(None)
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
                    images: Vec::new(),
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
        // A newer build may append kinds this reader does not know. Skipping
        // them keeps the log replayable (schema-additive contract); the seq
        // continuity check below then has to tolerate the gap.
        let mut skipped_unknown = false;

        for (index, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(line) {
                Ok(event) => {
                    if let Some(last) = events.last() {
                        let expected = last.seq + 1;
                        if event.seq != expected {
                            if !skipped_unknown {
                                return Err(StoreError::Seq {
                                    expected,
                                    got: event.seq,
                                });
                            }
                            // A skipped future-kind event explains the gap; the
                            // log itself is fine.
                            tracing::warn!(
                                expected,
                                got = event.seq,
                                "seq gap after skipping an unknown event kind"
                            );
                        }
                    }
                    events.push(event);
                }
                // A crash can leave a partial final line. Drop only that one.
                Err(_) if index + 1 == total => break,
                Err(err) => {
                    // A deserialization failure on a non-final line is either
                    // corruption or a kind this build predates. The `type` tag
                    // tells them apart: an unknown kind is skipped, corruption
                    // still fails the load.
                    let unknown = serde_json::from_str::<serde_json::Value>(line)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("type")
                                .and_then(|tag| tag.as_str())
                                .map(|tag| !crate::event::KNOWN_EVENT_TYPES.contains(&tag))
                        })
                        .unwrap_or(false);
                    if unknown {
                        skipped_unknown = true;
                        tracing::debug!(
                            session = %id,
                            "skipping an event kind written by a newer build"
                        );
                        continue;
                    }
                    return Err(StoreError::Json(err));
                }
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
                cost_usd REAL NOT NULL DEFAULT 0,
                parent TEXT
            );",
        )?;
        // Add the cost column to indexes created before it existed.
        let _ = conn.execute(
            "ALTER TABLE sessions ADD COLUMN cost_usd REAL NOT NULL DEFAULT 0",
            [],
        );
        // Same for the parent link (ADR-0087). An index written before it
        // existed has no parent for any row, which reads back as `None` — the
        // same thing those logs say, so the two agree until the next rebuild.
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN parent TEXT", []);
        // 旧 schema 里的 `schedules` 表（ADR-0059 的配置路径运行状态）已经不再
        // 使用：`[schedules]` 条目现在靠 `reconcile_config` 变成任务，状态记在
        // `tasks.json` 的任务上（ADR-0093）。老库里那张表留着不动，**不必也不
        // 该 DROP**：删掉用户的旧数据没有任何收益，而万一要回退，它还在。
        // Cross-process facts about the *process*, not about one task
        // (ADR-0088 §3). The scheduler's clock lives in the GUI today, so
        // "who is driving" has to be visible to a frontend that has no clock
        // of its own — otherwise "registered" is a promise nobody can check.
        // 它是**进程**的事实、不是某个任务的事实，所以独立一张表：老 `schedules`
        // 表以任务名为主键，心跳塞进去得用一个保留名。
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS runtime_status (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL
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
                     (id, started_at, model, provider, cwd, version, turns, last_seq, cost_usd, parent)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9)",
                    params![
                        event.session,
                        event.ts.to_string(),
                        meta.model,
                        meta.provider,
                        meta.cwd,
                        meta.version,
                        event.turn,
                        event.seq,
                        meta.parent,
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
            "SELECT id, started_at, model, provider, cwd, version, turns, last_seq, cost_usd, parent
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
                    parent: row.get(9)?,
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
                 (id, started_at, model, provider, cwd, version, turns, last_seq, cost_usd, parent)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
                    summary.started.parent,
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
            tracing::warn!(%error, session = %id, "cannot prune schedule tasks for deleted session");
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

    /// Remember a cross-process fact about the running process (ADR-0088 §3).
    ///
    /// Used for the scheduler's heartbeat: the clock lives in the GUI, and a
    /// frontend without one has to be able to tell "nobody is driving" from
    /// "driving, just quiet". Overwrites: one row per key.
    pub fn set_runtime_status(
        &self,
        key: &str,
        value: &str,
        at: time::PrimitiveDateTime,
    ) -> Result<(), StoreError> {
        let conn = self.open_index()?;
        conn.execute(
            "INSERT INTO runtime_status (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at",
            params![key, value, format_wall_clock(at)],
        )?;
        Ok(())
    }

    /// Read a [`SessionStore::set_runtime_status`] row back.
    pub fn runtime_status(
        &self,
        key: &str,
    ) -> Result<Option<(String, time::PrimitiveDateTime)>, StoreError> {
        let conn = self.open_index()?;
        let mut stmt =
            conn.prepare("SELECT value, updated_at FROM runtime_status WHERE key = ?1")?;
        let mut rows = stmt.query_map(params![key], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let Some(row) = rows.next() else {
            return Ok(None);
        };
        let (value, at) = row?;
        // An unparseable timestamp is the same as never written: the caller's
        // only question is "was a driver here recently", and a value we cannot
        // place in time cannot answer it.
        Ok(parse_wall_clock(&at).map(|at| (value, at)))
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
        // 临时文件名必须**每个写者不同**。原来固定用 `tasks.json.tmp`，两个进程
        // 同时写时（ADR-0088 之后 TUI 与 GUI 都可能写）：A 写完、B 覆盖它、A
        // rename 走、B 的 rename 撞 ENOENT——B 那次登记直接失败。`Scheduler` 的锁
        // 让这条路径今天不该发生，但把一个「并发下必然报错」的写法换成「并发下
        // 最后写者胜」，成本是一个 pid，收益是失败模式从报错变成可对账的覆盖。
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// 目标会话日志里是否已经出现某次定时投递对应的 `user/message` 事件。
    ///
    /// 崩溃恢复 / 投递确认用它区分"投过没投过"。投递消息落盘时带
    /// `scheduled` 标记（[`crate::schedule::ScheduledMessage`]，含 `task_id`
    /// 与发生点 `at`），因此按任务 + 发生点匹配：`at` 精确到触发时刻，
    /// 能唯一标识"本次投递"，比文案匹配更可靠（用户输入不会撞车）。
    pub fn schedule_delivery_landed(
        &self,
        session_id: &str,
        task_id: &str,
        at: &str,
    ) -> Result<bool, StoreError> {
        let events = self.load(session_id)?;
        Ok(events.iter().any(|event| {
            matches!(
                &event.kind,
                EventKind::UserMessage(message)
                    if message.scheduled.as_ref().is_some_and(|s| s.task_id == task_id && s.at == at)
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
            parent: None,
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
    fn append_title_generated_is_read_back_as_the_latest_title() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let created = store.create_task(&meta()).expect("create task");

        store
            .append_title_generated(
                &created.id,
                "模型标题".to_string(),
                vec![3],
                TitleSource::Auto {
                    provider: "mock".to_string(),
                    model: "mock-1".to_string(),
                },
            )
            .expect("append generated title");
        assert_eq!(
            crate::title::latest_title(&store.load(&created.id).expect("load")).as_deref(),
            Some("模型标题")
        );

        // A fallback written later wins, exactly like a later rename.
        store
            .append_title_generated(
                &created.id,
                "兜底标题".to_string(),
                vec![3],
                TitleSource::Fallback,
            )
            .expect("append fallback title");
        assert_eq!(
            crate::title::latest_title(&store.load(&created.id).expect("load")).as_deref(),
            Some("兜底标题")
        );
        // 生成标题不动 Task 聚合：`task()` 仍无标题，用户重命名语义不受污染。
        assert_eq!(store.task(&created.id).expect("task").title(), None);
    }

    #[test]
    fn load_skips_an_unknown_event_kind_written_by_a_newer_build() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let created = store.create_task(&meta()).expect("create task");
        let path = store.session_path(&created.id);

        // A user message at seq 3, then an unknown-kind line at seq 4 (a newer
        // build), then a valid assistant line at seq 5 — the gap must not fail
        // the load, and the unknown line must be skipped.
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open");
        writeln!(
            file,
            r#"{{"v":11,"seq":3,"ts":"2026-11-16T00:00:00Z","session":"{}","turn":1,"type":"user_message","data":{{"text":"hi"}}}}"#,
            created.id
        )
        .expect("user line");
        writeln!(
            file,
            r#"{{"v":99,"seq":4,"ts":"2026-11-16T00:00:00Z","session":"{}","turn":1,"type":"future_kind","data":{{}}}}"#,
            created.id
        )
        .expect("unknown line");
        writeln!(
            file,
            r#"{{"v":11,"seq":5,"ts":"2026-11-16T00:00:00Z","session":"{}","turn":1,"type":"assistant_message","data":{{"blocks":[],"provider_raw":null}}}}"#,
            created.id
        )
        .expect("assistant line");
        file.flush().expect("flush");

        let events = store
            .load(&created.id)
            .expect("load skips the unknown kind");
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| match &event.kind {
                EventKind::UserMessage(_) => "user",
                EventKind::AssistantMessage(_) => "assistant",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["other", "other", "user", "assistant"]);
        assert_eq!(events[2].seq, 3);
        assert_eq!(events[3].seq, 5);
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
            origin: crate::schedule::Origin::User,
            stakeholders,
            last_receipt: None,
            delivery_log: Vec::new(),
            earlier_records_pruned: false,
            missed_windows: 0,
            reserved_session_id: None,
            next_fire_at: None,
            cwd: None,
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

    #[test]
    fn schedule_delivery_landed_matches_task_id_and_occurrence() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let id = seed(&store);
        let at = "2026-11-13T09:00:00";
        let scheduled = crate::schedule::ScheduledMessage {
            task_id: "task-1".to_string(),
            title: "morning".to_string(),
            prompt: "run the review".to_string(),
            at: at.to_string(),
        };
        store
            .append(&Event::new(
                5,
                &id,
                1,
                EventKind::UserMessage(UserMessage {
                    starts_segment: false,
                    queued_id: None,
                    text: scheduled.prompt.clone(),
                    attachments: Vec::new(),

                    continuation: None,
                    question_reply: None,
                    scheduled: Some(scheduled),
                }),
            ))
            .expect("scheduled delivery");

        // 任务 + 发生点都命中才算"投过"。
        assert!(
            store
                .schedule_delivery_landed(&id, "task-1", at)
                .expect("landed")
        );
        // 同任务、不同发生点（下一次触发）不算。
        assert!(
            !store
                .schedule_delivery_landed(&id, "task-1", "2026-11-14T09:00:00")
                .expect("next occurrence not landed")
        );
        // 其他任务不算（批量投递的同会话任务互不干扰）。
        assert!(
            !store
                .schedule_delivery_landed(&id, "task-2", at)
                .expect("other task not landed")
        );
    }
}

/// ADR-0088 §5 的锁必须是**跨进程**的，而同进程内 `flock` 对同一个 fd
/// 的重复获取是成功的（锁属于进程，不是 fd），所以单进程测试根本测不到
/// 互斥。这里开两个真线程 + 两次独立 `open`：POSIX 下同一进程的不同 fd
/// 之间 `flock` 仍会互斥，足以证明「谁先拿到、另一个等」。
///
/// 更贴切的验证是 `tests/` 下的双进程用例，但那要 spawn 二进制；这一条
/// 覆盖的是「锁确实阻塞、而不是静默放行」。
#[test]
fn file_lock_blocks_a_second_holder_until_released() {
    let dir = tempfile::tempdir().expect("data dir");
    let store = SessionStore::new(dir.path());
    let first = store.lock_file_blocking("schedule-tasks").expect("first");
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let writer = {
        let store = store.clone();
        let order = order.clone();
        std::thread::spawn(move || {
            let _second = store
                .lock_file_blocking("schedule-tasks")
                .expect("second acquires once released");
            order.lock().expect("order").push("writer");
        })
    };

    // 给 writer 一点时间去撞锁；它必须还没进去。
    std::thread::sleep(std::time::Duration::from_millis(120));
    assert!(
        order.lock().expect("order").is_empty(),
        "the second holder got in while the first still held the lock"
    );
    order.lock().expect("order").push("main-drops");
    drop(first);
    writer.join().expect("writer");
    assert_eq!(
        *order.lock().expect("order"),
        ["main-drops", "writer"],
        "the writer must only proceed after the guard is dropped"
    );
}

#[test]
fn runtime_status_round_trips_and_overwrites() {
    let dir = tempfile::tempdir().expect("data dir");
    let store = SessionStore::new(dir.path());
    assert_eq!(
        store.runtime_status("driver.heartbeat").expect("read"),
        None
    );

    let first = crate::schedule::at(2026, time::Month::June, 1, 9, 0);
    store
        .set_runtime_status("driver.heartbeat", "gui:1", first)
        .expect("write");
    let (value, at) = store
        .runtime_status("driver.heartbeat")
        .expect("read")
        .expect("row");
    assert_eq!(value, "gui:1");
    assert_eq!(at, first);

    // 心跳是覆盖写，不是追加：一个键只有一行。
    let second = crate::schedule::at(2026, time::Month::June, 1, 9, 1);
    store
        .set_runtime_status("driver.heartbeat", "gui:1", second)
        .expect("overwrite");
    let (_, at) = store
        .runtime_status("driver.heartbeat")
        .expect("read")
        .expect("row");
    assert_eq!(at, second);
}

#[test]
fn driver_status_needs_the_lock_not_just_a_heartbeat() {
    use crate::scheduler::{
        DRIVER_HEARTBEAT_BUDGET, DRIVER_HEARTBEAT_KEY, DRIVER_LOCK, DriverStatus,
    };

    let dir = tempfile::tempdir().expect("data dir");
    let store = SessionStore::new(dir.path());
    let now = crate::schedule::at(2026, time::Month::June, 1, 9, 0);

    // 都没有：Unknown，而不是「不在跑」——这两件事对用户是不同的
    // （一个还没配好，一个曾经会跑但停了）。
    assert_eq!(DriverStatus::read(&store, now), DriverStatus::Unknown);

    // **只写心跳、不持锁：Gone。** 这是 ADR-0091 的核心断言——心跳不再是权威，
    // 它是「活着的细节」。旧实现会在这里报 Running，于是「进程死了但心跳还新」
    // 会被说成「在跑」。
    store
        .set_runtime_status(DRIVER_HEARTBEAT_KEY, "gui:1", now)
        .expect("write");
    assert_eq!(
        DriverStatus::read(&store, now),
        DriverStatus::Gone { at: now }
    );
    assert!(!DriverStatus::read(&store, now).is_running());

    // 锁在手 + 心跳新鲜：在跑。
    let lock = store
        .try_lock_file(DRIVER_LOCK)
        .expect("lock")
        .expect("free");
    assert_eq!(
        DriverStatus::read(&store, now),
        DriverStatus::Running { at: now }
    );

    // 锁在手 + 心跳过期：Held。**仍然算在跑**——进程还在，醒来会继续。
    // 把它说成「不在跑」会劝人去重建一条本来没事的任务。
    let later = now + DRIVER_HEARTBEAT_BUDGET + time::Duration::seconds(1);
    assert_eq!(
        DriverStatus::read(&store, later),
        DriverStatus::Held { at: Some(now) }
    );
    assert!(DriverStatus::read(&store, later).is_running());

    // 边界：正好等于预算仍算在跑（避免 90s 整那一刻闪一下）。
    let boundary = now + DRIVER_HEARTBEAT_BUDGET;
    assert_eq!(
        DriverStatus::read(&store, boundary),
        DriverStatus::Running { at: now }
    );

    // 驱动自己不去探锁（同进程重入也会互斥），它用 held_by_me。
    assert!(DriverStatus::held_by_me(&store, now).is_running());

    // 放开之后又回到 Gone：锁是唯一的身份，进程一走就没了。
    drop(lock);
    assert_eq!(
        DriverStatus::read(&store, now),
        DriverStatus::Gone { at: now }
    );
}
