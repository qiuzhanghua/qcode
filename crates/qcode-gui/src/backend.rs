//! The Backend port: everything the window needs from a kernel, local or remote.
//!
//! [`LocalBackend`] wraps the engine thread; a remote client implements the
//! same trait without changing the views.

use crate::Engine;
use crate::runtime;
use qcode_core::event::{Event, EventKind};
use qcode_core::question::AnswerItem;
use qcode_core::schedule::ScheduledTask;
use qcode_core::store::{SessionStore, SessionSummary};
use qcode_core::task::{Root, Task};
use qcode_ui_bridge::{QueueOp, RaiseLimit, ScheduleOp, SessionEvent, UiCommand, channels};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// A Task view for the sidebar. Task:Session is 1:1, but the row keeps the
/// loaded log summary alongside the Task aggregate.
#[derive(Debug, Clone)]
pub struct TaskRow {
    pub task: Task,
    pub summary: SessionSummary,
    /// True when any message in the session log is a scheduled-task delivery
    /// (ADR-0059): the sidebar shows a clock mark on such tasks.
    pub has_scheduled: bool,
}

/// Whether the session log contains a scheduled-task delivery message.
fn has_scheduled_message(store: &SessionStore, id: &str) -> bool {
    store
        .load(id)
        .map(|events| {
            events.iter().any(|event| {
                matches!(&event.kind, EventKind::UserMessage(message) if message.scheduled.is_some())
            })
        })
        .unwrap_or(false)
}

/// One `[schedules.<name>]` entry as the sidebar needs it (ADR-0059 §1, §5).
///
/// The declaration plus its stored run state: the window shows "missed", not the
/// cron, and it never writes either half back to the config file.
#[derive(Debug, Clone)]
pub struct ScheduleRow {
    pub name: String,
    /// Verbatim cron string. Shown as written: the user edits this text, so
    /// translating it to prose would hide the thing they came to check
    /// (ADR-0063 §5).
    pub cron: String,
    /// The task it runs on; `None` when every fire creates its own.
    pub task: Option<String>,
    /// `new = true`: every fire makes a fresh task.
    pub new_task: bool,
    /// `queue` or `skip`, as configured.
    pub overlap: Option<String>,
    /// How long a scheduled turn's approval may sit unanswered.
    pub approval_timeout: Option<String>,
    /// `enabled = false` entries exist but never fire; they still get a row so
    /// the board shows them instead of hiding them (ADR-0063 §5).
    pub enabled: bool,
    pub next_fire_at: Option<qcode_core::schedule::PrimitiveDateTime>,
    pub status: Option<qcode_core::schedule::ScheduleStatus>,
    pub missed_windows: u32,
    /// The entry could not be resolved at all (bad cron, no target, …).
    pub broken: Option<String>,
}

impl ScheduleRow {
    /// What the task row says about this schedule, or `None` when there is
    /// nothing worth a reader's attention.
    pub fn hint(&self) -> Option<String> {
        use qcode_core::schedule::ScheduleStatus;
        if let Some(reason) = &self.broken {
            return Some(format!("定时配置有误: {reason}"));
        }
        match &self.status {
            Some(ScheduleStatus::Orphaned) => Some("定时已失效（任务已删除）".to_string()),
            Some(ScheduleStatus::Missed(windows)) => Some(format!("定时错过 {windows} 次")),
            Some(ScheduleStatus::Failed(reason)) => Some(format!("定时失败: {reason}")),
            _ => None,
        }
    }
}

/// Everything the window needs from a kernel, local or remote.
///
/// P0 has exactly one implementation. The methods are synchronous because the
/// local backend only sends commands; the event channel carries the output.
pub trait Backend {
    /// Where new sessions record their cwd; not an ownership boundary.
    fn cwd(&self) -> &Path;
    fn tasks(&self) -> Result<Vec<TaskRow>, String>;
    /// Declared schedules and their stored run state; empty when the feature is
    /// unused, so the sidebar needs no special case.
    fn schedules(&self) -> Vec<ScheduleRow> {
        Vec::new()
    }
    /// 运行时定时任务（含 config 迁移导入的条目）；空 = 未使用。
    fn schedule_tasks(&self) -> Vec<ScheduledTask> {
        Vec::new()
    }
    /// 对定时任务的操作（创建 / 编辑 / 暂停 / 恢复 / 立即执行 / 删除）。
    /// 结果经 `ScheduleChanged` / `ScheduleNotice` 事件回传，不在本方法里返回。
    fn schedule_op(&mut self, _op: ScheduleOp) {}
    /// First user message in a session, for the sidebar.
    fn title(&self, _id: &str) -> Option<String> {
        None
    }
    /// Replace a Task's roots; persists a `roots_changed` event.
    fn set_roots(&mut self, id: &str, roots: Vec<Root>) -> Result<(), String>;
    /// Persist an explicit task title.
    fn rename_task(&mut self, id: &str, title: String) -> Result<(), String>;
    /// Delete a task and its local session log/side outputs.
    fn delete_task(&mut self, id: &str) -> Result<(), String>;
    /// Start a draft. The first prompt creates the session log.
    fn new_session(&mut self);
    /// Load the transcript, then point the engine at the session.
    fn resume(&mut self, id: &str) -> Result<Vec<Event>, String>;
    /// Send a message to `session` (`None` = wherever the window is, i.e. a
    /// draft). The session is named explicitly because more than one turn can
    /// be live: a stale "current" must not post to the wrong task.
    fn prompt(
        &mut self,
        session: Option<&str>,
        text: String,
        attachments: Vec<qcode_core::engine::AttachmentSource>,
    );
    /// Continue the paused turn in a fresh budget window (ADR-0060). A no-op
    /// when nothing is paused.
    fn continue_turn(&mut self, _raise: Option<RaiseLimit>) {}
    /// Steer a message into the running turn's next step. A no-op when idle.
    fn steer(&mut self, _text: String, _attachments: Vec<qcode_core::engine::AttachmentSource>) {}
    /// One operation on a queued (next-turn) message: steer it into the
    /// running turn, move it, or drop it.
    fn queue_op(&mut self, _id: String, _op: QueueOp) {}
    fn answer(&mut self, call_id: String, allow: bool, session: bool);
    /// Answer an `ask_user` question. `session` names the task (`None` = the
    /// window's current one); `text` is the model-visible reply (the original
    /// questions plus the answers).
    fn answer_question(
        &mut self,
        _session: Option<String>,
        _call_id: String,
        _text: String,
        _answers: Vec<AnswerItem>,
    ) {
    }
    /// Stop `session`'s turn (`None` = the draft the engine is on). A session
    /// that is not running is a no-op (ADR-0058 §6).
    fn cancel(&mut self, session: Option<&str>);
    /// Stop every live session; the window calls this before quitting and then
    /// waits for each `TurnFinished`.
    fn cancel_all(&mut self);
    /// Change the provider+model for subsequent turns (grouped picker, V2.1).
    fn set_model(&mut self, provider: String, model: String);
    /// Model/provider currently active in the backend, for syncing after a
    /// settings rebuild.
    fn model(&self) -> Option<String> {
        None
    }
    fn provider(&self) -> Option<String> {
        None
    }
    fn try_recv(&mut self) -> Option<SessionEvent>;
    /// Build a fresh backend from the current configuration. The shell calls
    /// this when the config file changes, so saving Settings takes effect
    /// without restarting the process or flashing a new window.
    fn rebuild(&self) -> Result<Box<dyn Backend>, String> {
        Err("this backend cannot reload".to_string())
    }

    /// The async path: the shell runs this on a background executor.
    fn rebuild_hook(&self) -> Option<RebuildFn> {
        None
    }

    /// True when a kernel is assembled; a false backend explains why prompts do
    /// nothing instead of dropping them.
    fn is_configured(&self) -> bool {
        true
    }
}

/// The window runs without a kernel: no config yet, so there is no provider to
/// assemble. Tasks stay readable because they live in the session store, which
/// does not need one.
pub struct UnconfiguredBackend {
    cwd: PathBuf,
    store: SessionStore,
    rebuild: Option<RebuildFn>,
}

impl UnconfiguredBackend {
    pub fn new(cwd: PathBuf, store: SessionStore) -> Self {
        Self {
            cwd,
            store,
            rebuild: None,
        }
    }

    pub fn with_rebuild(mut self, rebuild: RebuildFn) -> Self {
        self.rebuild = Some(rebuild);
        self
    }
}

impl Backend for UnconfiguredBackend {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn tasks(&self) -> Result<Vec<TaskRow>, String> {
        let summaries = self.store.list().map_err(|error| error.to_string())?;
        Ok(summaries
            .into_iter()
            .filter_map(|summary| {
                let task = self
                    .store
                    .task(&summary.id)
                    .or_else(|_| summary.legacy_task())
                    .ok()?;
                Some(TaskRow {
                    task,
                    has_scheduled: has_scheduled_message(&self.store, &summary.id),
                    summary,
                })
            })
            .collect())
    }

    fn title(&self, id: &str) -> Option<String> {
        let events = self.store.load(id).ok()?;
        events.iter().find_map(|event| match &event.kind {
            EventKind::UserMessage(message) => {
                let text = message.text.trim().replace('\n', " ");
                (!text.is_empty()).then(|| text.chars().take(48).collect())
            }
            _ => None,
        })
    }

    fn set_roots(&mut self, _id: &str, _roots: Vec<Root>) -> Result<(), String> {
        Err("not configured yet: open Settings first".to_string())
    }

    fn rename_task(&mut self, id: &str, title: String) -> Result<(), String> {
        self.store
            .rename_task(id, title)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn delete_task(&mut self, id: &str) -> Result<(), String> {
        self.store
            .delete_session(id)
            .map_err(|error| error.to_string())
    }

    fn new_session(&mut self) {}

    fn resume(&mut self, id: &str) -> Result<Vec<Event>, String> {
        self.store.load(id).map_err(|error| error.to_string())
    }

    fn schedule_tasks(&self) -> Vec<ScheduledTask> {
        self.store.load_schedule_tasks()
    }

    fn schedule_op(&mut self, _op: ScheduleOp) {}

    fn prompt(
        &mut self,
        _session: Option<&str>,
        _text: String,
        _attachments: Vec<qcode_core::engine::AttachmentSource>,
    ) {
    }
    fn steer(&mut self, _text: String, _attachments: Vec<qcode_core::engine::AttachmentSource>) {}

    fn answer(&mut self, _call_id: String, _allow: bool, _session: bool) {}

    fn cancel(&mut self, _session: Option<&str>) {}

    fn cancel_all(&mut self) {}

    fn set_model(&mut self, _provider: String, _model: String) {}

    fn try_recv(&mut self) -> Option<SessionEvent> {
        None
    }

    fn is_configured(&self) -> bool {
        false
    }

    fn rebuild(&self) -> Result<Box<dyn Backend>, String> {
        Err("rebuild is asynchronous; use rebuild_hook".to_string())
    }

    fn rebuild_hook(&self) -> Option<RebuildFn> {
        self.rebuild.clone()
    }
}

/// Rebuilds a backend from the current configuration.
///
/// The future is boxed because the result (`Box<dyn Backend>`) is not `Send`:
/// it carries the engine and its store, so the rebuild has to be awaited on the
/// UI thread's executor rather than a background one.
pub type RebuildFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Box<dyn Backend>, String>>>>;
pub type RebuildFn = std::sync::Arc<dyn Fn() -> RebuildFuture + Send + Sync>;

/// The in-process backend: engine thread + bridge + session store.
pub struct LocalBackend {
    cwd: PathBuf,
    model: String,
    provider: String,
    store: SessionStore,
    /// `[schedules]` as the config declared it (ADR-0059 §1). The run state is
    /// read from the store on demand, so the sidebar sees the truth after a
    /// restart without the window holding a copy.
    schedules: Vec<qcode_core::schedule::Schedule>,
    /// Entries that could not be resolved, so a typo is visible rather than
    /// looking like "no schedules configured".
    schedule_errors: Vec<(String, String)>,
    commands: UnboundedSender<UiCommand>,
    events: UnboundedReceiver<SessionEvent>,
    rebuild: Option<RebuildFn>,
}

impl LocalBackend {
    pub fn new(engine: Engine) -> Self {
        let cwd = engine.workspace.clone();
        let model = engine.model.clone();
        let provider = engine.provider_name.clone();
        let store = engine.store.clone();
        let schedules = engine.schedules.clone();
        let schedule_errors = engine.schedule_errors.clone();
        let (ui, port) = channels();
        let commands = port.take_commands().expect("command receiver");
        runtime::spawn(engine, port, commands);
        Self {
            cwd,
            model,
            provider,
            store,
            schedules,
            schedule_errors,
            commands: ui.commands,
            events: ui.events,
            rebuild: None,
        }
    }

    /// Attach the rebuild hook so a config change can replace this backend.
    pub fn with_rebuild(mut self, rebuild: RebuildFn) -> Self {
        self.rebuild = Some(rebuild);
        self
    }

    fn send(&self, command: UiCommand) {
        let _ = self.commands.send(command);
    }
}

impl Backend for LocalBackend {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn schedules(&self) -> Vec<ScheduleRow> {
        let states = self.store.schedule_states().unwrap_or_default();
        let mut rows: Vec<ScheduleRow> = self
            .schedules
            .iter()
            .map(|schedule| {
                let state = states.get(&schedule.name);
                ScheduleRow {
                    name: schedule.name.clone(),
                    // Verbatim, not the parsed spec: see `Schedule::cron_text`.
                    cron: schedule.cron_text.clone(),
                    task: schedule.task.clone(),
                    new_task: schedule.creates_task(),
                    overlap: Some(match schedule.overlap {
                        qcode_core::Overlap::Queue => "queue".to_string(),
                        qcode_core::Overlap::Skip => "skip".to_string(),
                    }),
                    approval_timeout: schedule.approval_timeout.map(|timeout| {
                        let seconds = timeout.as_secs();
                        if seconds == 0 {
                            "0".to_string()
                        } else if seconds % 3600 == 0 {
                            format!("{}h", seconds / 3600)
                        } else if seconds % 60 == 0 {
                            format!("{}m", seconds / 60)
                        } else {
                            format!("{seconds}s")
                        }
                    }),
                    enabled: schedule.enabled,
                    next_fire_at: state.and_then(|state| state.next_fire_at),
                    status: state.and_then(|state| state.last_status.clone()),
                    missed_windows: state.map(|state| state.missed_windows).unwrap_or(0),
                    broken: None,
                }
            })
            .collect();
        for (name, reason) in &self.schedule_errors {
            rows.push(ScheduleRow {
                name: name.clone(),
                // A broken entry has no parsed config to show; the reason is the
                // only thing worth a reader's attention.
                cron: String::new(),
                task: None,
                new_task: false,
                overlap: None,
                approval_timeout: None,
                enabled: false,
                next_fire_at: None,
                status: None,
                missed_windows: 0,
                broken: Some(reason.clone()),
            });
        }
        rows
    }

    fn rebuild(&self) -> Result<Box<dyn Backend>, String> {
        Err("rebuild is asynchronous; use rebuild_hook".to_string())
    }

    fn rebuild_hook(&self) -> Option<RebuildFn> {
        self.rebuild.clone()
    }

    fn tasks(&self) -> Result<Vec<TaskRow>, String> {
        let summaries = self.store.list().map_err(|error| error.to_string())?;
        let tasks = summaries
            .into_iter()
            .filter_map(|summary| {
                let task = self
                    .store
                    .task(&summary.id)
                    .or_else(|_| summary.legacy_task())
                    .ok()?;
                Some(TaskRow {
                    task,
                    has_scheduled: has_scheduled_message(&self.store, &summary.id),
                    summary,
                })
            })
            .collect();
        Ok(tasks)
    }

    fn title(&self, id: &str) -> Option<String> {
        let events = self.store.load(id).ok()?;
        events.iter().find_map(|event| match &event.kind {
            EventKind::UserMessage(message) => {
                let text = message.text.trim().replace('\n', " ");
                (!text.is_empty()).then(|| text.chars().take(48).collect())
            }
            _ => None,
        })
    }

    fn set_roots(&mut self, id: &str, roots: Vec<Root>) -> Result<(), String> {
        self.store
            .replace_roots(id, roots)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn rename_task(&mut self, id: &str, title: String) -> Result<(), String> {
        self.store
            .rename_task(id, title)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn delete_task(&mut self, id: &str) -> Result<(), String> {
        self.store
            .delete_session(id)
            .map_err(|error| error.to_string())
    }

    fn new_session(&mut self) {
        self.send(UiCommand::NewSession);
    }

    fn schedule_tasks(&self) -> Vec<ScheduledTask> {
        self.store.load_schedule_tasks()
    }

    fn schedule_op(&mut self, op: ScheduleOp) {
        self.send(UiCommand::ScheduleOp(op));
    }

    fn resume(&mut self, id: &str) -> Result<Vec<Event>, String> {
        let events = self.store.load(id).map_err(|error| error.to_string())?;
        self.send(UiCommand::Resume { id: id.to_string() });
        Ok(events)
    }

    fn prompt(
        &mut self,
        session: Option<&str>,
        text: String,
        attachments: Vec<qcode_core::engine::AttachmentSource>,
    ) {
        self.send(UiCommand::Prompt {
            session: session.map(str::to_string),
            text,
            attachments,
        });
    }

    fn continue_turn(&mut self, raise: Option<RaiseLimit>) {
        self.send(UiCommand::Continue { raise });
    }

    fn steer(&mut self, text: String, attachments: Vec<qcode_core::engine::AttachmentSource>) {
        self.send(UiCommand::Steer { text, attachments });
    }

    fn queue_op(&mut self, id: String, op: QueueOp) {
        self.send(UiCommand::QueueOp { id, op });
    }

    fn answer(&mut self, call_id: String, allow: bool, session: bool) {
        self.send(UiCommand::Answer {
            call_id,
            allow,
            session,
        });
    }

    fn answer_question(
        &mut self,
        session: Option<String>,
        call_id: String,
        text: String,
        answers: Vec<AnswerItem>,
    ) {
        self.send(UiCommand::AnswerQuestion {
            session,
            call_id,
            text,
            answers,
        });
    }

    fn cancel(&mut self, session: Option<&str>) {
        self.send(UiCommand::Cancel {
            session: session.map(str::to_string),
        });
    }

    fn cancel_all(&mut self) {
        self.send(UiCommand::CancelAll);
    }

    fn set_model(&mut self, provider: String, model: String) {
        self.provider = provider.clone();
        self.model = model.clone();
        self.send(UiCommand::SetModel { provider, model });
    }

    fn model(&self) -> Option<String> {
        Some(self.model.clone())
    }

    fn provider(&self) -> Option<String> {
        Some(self.provider.clone())
    }

    fn try_recv(&mut self) -> Option<SessionEvent> {
        self.events.try_recv().ok()
    }
}
