//! Engine runtime: turns `UiCommand`s into `run_turn` calls, owns the session
//! and the follow-up inbox, and streams `UiEvent`s back.
//!
//! Deliberately free of GPUI: it only needs qcode-core, qcode-ui-bridge and
//! tokio, so the queue/turn policy can be tested without opening a window.

use qcode_core::engine::{DeltaSink, RunOptions, TurnContext, run_turn};
use qcode_core::event::SessionStarted;
use qcode_core::inbox::{QueuedMessage, SessionQueue, Submission};
use qcode_core::schedule::{
    Advance, DeliveryReceipt, Schedule, ScheduleState, ScheduleStatus, ScheduledTask, Target,
    TaskStatus, TaskTick, Tick, advance_rule, append_delivery, task_tick,
};
use qcode_core::scheduler::{EditTask, NewTask, Scheduler};
use qcode_core::store::SessionStore;
use qcode_ui_bridge::{KernelPort, QueueOp, ScheduleOp, SessionTarget, TaggedPort, UiCommand};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::host::SessionHost;

/// The pieces a running window needs to execute turns.
///
/// Everything here comes from `qcode_kernel::assemble`; the window never wires
/// its own provider or tools. Held once behind [`crate::host::Deps`] so every
/// session shares it instead of cloning a provider per session (ADR-0058 §1).
pub struct Engine {
    pub store: SessionStore,
    pub provider: std::sync::Arc<dyn qcode_core::provider::Provider>,
    pub provider_name: String,
    /// Egress proxy URL of the assembled kernel; a cross-provider model
    /// switch rebuilds the provider with the same proxy.
    pub proxy_url: Option<String>,
    pub tools: qcode_core::tool::ToolRegistry,
    pub policy: qcode_core::policy::Policy,
    pub workspace: PathBuf,
    /// Config file the engine was assembled from; "切即默认" persists the
    /// switched model into its `[default-model]` section.
    pub config_path: PathBuf,
    pub model: String,
    pub pricing: Option<qcode_core::cost::Pricing>,
    pub pricing_table: std::collections::BTreeMap<String, qcode_core::cost::Pricing>,
    pub max_turns: u32,
    pub max_cost_usd: Option<f64>,
    pub context_window: Option<u64>,
    pub network: Option<qcode_core::network::SharedNetwork>,
    pub snapshots: Option<qcode_core::snapshot::SnapshotStore>,
    /// The environment `run_shell` injects (ADR-0064); the turn samples it
    /// first and records what the agent sees.
    pub shell_env: std::sync::Arc<qcode_tools::shell_env::ShellEnvCache>,
    /// Scheduled turns from `[schedules]` (ADR-0059). Empty = no scheduler.
    /// Their run state lives in the store, not here.
    pub schedules: Vec<qcode_core::schedule::Schedule>,
    /// `[schedules]` entries that did not resolve, with the reason. Shown as a
    /// broken row so a typo is visible instead of looking like "not configured".
    pub schedule_errors: Vec<(String, String)>,
    /// How often the scheduler's clock beats. `None` = 30 s, which is finer
    /// than a cron expression can express. A test (or a hand check) sets a
    /// short beat instead of waiting half a minute.
    pub schedule_tick: Option<Duration>,
}

/// Create a fresh empty task (no roots) and tell the UI to switch to it.
/// `provider_name` is the engine's current provider (may change after a
/// cross-provider model switch).
pub(crate) fn create_empty_task(
    engine: &Engine,
    provider_name: &str,
    port: &KernelPort,
    model: &str,
) -> Option<String> {
    create_task(engine, provider_name, port, model, true)
}

/// Create a task without taking the view with it.
///
/// A scheduled fire that creates its own task must land in the sidebar, not on
/// the screen: the user may be reading something else (ADR-0059 §6). It is the
/// same call with the `SessionChanged` left out.
pub(crate) fn create_task(
    engine: &Engine,
    provider_name: &str,
    port: &KernelPort,
    model: &str,
    announce: bool,
) -> Option<String> {
    let meta = SessionStarted {
        model: model.to_string(),
        provider: provider_name.to_string(),
        cwd: engine.workspace.display().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    match engine.store.create_task_with_roots(&meta, Vec::new()) {
        Ok(created) => {
            let target = SessionTarget::Session(created.id.clone());
            // A new task is a new session: its own "Always" list, not the one
            // the previous session accumulated.
            port.clear_allow(&target);
            let port = port.tagged(target);
            port.send_event(&created.started);
            port.send_event(&created.configured);
            if announce {
                port.send_session_changed(Some(created.id.clone()));
            }
            Some(created.id)
        }
        Err(error) => {
            eprintln!("qcode-gui: cannot create task: {error}");
            port.tagged(SessionTarget::Draft)
                .send_turn_failed(error.to_string());
            None
        }
    }
}

/// What the UI was last told about the queues, so the ticker only publishes
/// real changes (the engine claims steering from its own thread).
#[derive(Default, PartialEq, Eq)]
struct Published {
    queue: Vec<QueuedMessage>,
    steering: Vec<QueuedMessage>,
}

fn publish(port: &TaggedPort, inbox: &SessionQueue, last: &mut Published) {
    let queue = inbox.queued();
    if queue != last.queue {
        port.send_queue_changed(queue.clone());
        last.queue = queue;
    }
    let steering = inbox.pending_step();
    if steering != last.steering {
        port.send_steering_changed(steering.clone());
        last.steering = steering;
    }
}

/// Remember a schedule's decision, in memory and in the store.
///
/// A store failure is logged, not fatal: the window keeps running with the
/// in-memory state and simply loses the record across a restart.
fn save_schedule_state(
    engine: &Engine,
    name: &str,
    state: &ScheduleState,
    cache: &mut std::collections::BTreeMap<String, ScheduleState>,
) {
    cache.insert(name.to_string(), state.clone());
    if let Err(error) = engine.store.save_schedule_state(name, state) {
        eprintln!("qcode-gui: cannot save schedule state for {name}: {error}");
    }
}

/// Reports a panicking turn to the app loop.
///
/// `run_turn` runs inside `tokio::spawn`, so a panic — a bug in a tool, in a
/// provider decoder, or an `unwrap` on model output — unwinds the spawned task
/// and the `done` message is never sent. Without this the loop keeps `active`
/// set forever: the UI stays on "running", Stop targets a task that is gone,
/// and new sessions keep being refused. Dropping the guard on unwind turns
/// that into the same `TurnFailed` a returned `Err` produces (ADR-0058 §9 P0).
struct TurnGuard {
    done: tokio::sync::mpsc::UnboundedSender<(SessionTarget, Result<(), String>)>,
    target: SessionTarget,
    armed: bool,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.done.send((
                self.target.clone(),
                Err("the turn task panicked; the engine is idle again, see the logs".to_string()),
            ));
        }
    }
}

// 恢复会话不改 model/provider：SessionStarted 里只是历史快照，拿它发请求
// 会把旧会话的模型名（如 mock-1）发给当前 provider。界面和引擎都以当前
// 配置/用户选择的模型为准。

/// Run the engine on its own thread: `run_turn` is async, the window is not.
pub(crate) fn spawn(engine: Engine, port: KernelPort, mut commands: UnboundedReceiver<UiCommand>) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("qcode-gui: cannot start the runtime: {error}");
                return;
            }
        };
        runtime.block_on(async move {
            // One host per window: the authority for "which sessions are live".
            // P1 wires a single slot through it (see `host.rs`).
            let host = Arc::new(SessionHost::new(engine));
            let deps = host.deps().clone();
            let engine = &deps.engine;

            // Where the window is looking. Commands that do not name a session
            // act on it; it is *not* the engine's idea of the live turn, because
            // there can be several (ADR-0058 §7, P5).
            let mut current = SessionTarget::Draft;
            // One entry per live turn, keyed by the session it belongs to. This
            // is the loop's authority for "is this session running"; the host
            // mirrors it so the approval path can see the same thing.
            let mut live: HashMap<SessionTarget, CancellationToken> = HashMap::new();
            let mut model = engine.model.clone();
            // Mutable provider + provider_name: a cross-provider model switch
            // (grouped header picker, V2.1) rebuilds the provider in place.
            let mut provider = engine.provider.clone();
            let mut provider_name = engine.provider_name.clone();
            let mut pricing = engine.pricing;
            // Finishing turns report the session they belong to: with more than
            // one live, "the turn finished" is not enough to route the clean-up.
            let (done_tx, mut done_rx) =
                tokio::sync::mpsc::unbounded_channel::<(SessionTarget, Result<(), String>)>();
            let mut ticker = tokio::time::interval(Duration::from_millis(250));
            // One clock for the whole window, not one per session (ADR-0059):
            // a minute is the resolution of a cron expression, and 30s leaves
            // room for the tick to drift without skipping a window.
            let beat = engine
                .schedule_tick
                .unwrap_or(Duration::from_secs(30))
                .max(Duration::from_millis(50));
            let mut clock = tokio::time::interval(beat);
            // Three beats without one is "the process was away", not "the tick
            // came round": see below.
            let gap_limit = beat.saturating_mul(3);
            // Wall-clock windows are compared against the *monotonic* beat to
            // tell "the tick came round" from "the process was asleep"
            // (ADR-0059 §2). Without this a suspended laptop would look like a
            // scheduler that missed nothing, and a clock corrected backwards
            // would look like one that owes a turn.
            let mut last_beat: Option<std::time::Instant> = None;
            let mut published: HashMap<SessionTarget, Published> = HashMap::new();

            // An approval prompt from a scheduled turn expires (ADR-0059 §4):
            // nobody is watching, so it must not hold the session lock all
            // night. Set right before `start_turn!` and taken by it.
            let mut approval_deadline: Option<Duration> = None;

            // The scheduler's run state, mirrored in the store between runs.
            let schedules: Vec<Schedule> = engine.schedules.clone();
            let mut schedule_states: std::collections::BTreeMap<String, ScheduleState> =
                engine.store.schedule_states().unwrap_or_default();
            let declared: Vec<String> = schedules
                .iter()
                .map(|schedule| schedule.name.clone())
                .collect();
            if let Err(error) = engine.store.prune_schedules(&declared) {
                eprintln!("qcode-gui: cannot prune schedule state: {error}");
            }

            // 运行时定时任务（Scheduled Tasks）：任务定义由 `Scheduler` 管理，
            // 投递在本时钟 beat 扫描（与 config 条目并存；迁移后这里接管）。
            // 单一时钟 beat 驱动全量扫描——cron 分钟级、interval ≥60s，30s
            // beat 的精度足够，也省去独立定时器与 kick 信号。
            let scheduler = Arc::new(Scheduler::new(engine.store.clone()));
            // in-flight 占位：已投递、等待确认的任务不再被扫描。
            let mut inflight: HashSet<String> = HashSet::new();
            // message_id -> [(task_id, 目标会话, 投递文案)]：对应 user/message
            // 事件落盘后确认（主消息落盘不带 queued_id，按文案匹配）。
            let mut pending_ack: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
            // ADR-0059 迁移：config 声明条目导入为运行时任务（幂等）。
            if let Ok(added) = scheduler.import_config(
                &schedules,
                &schedule_states,
                qcode_core::schedule::now_local(),
            ) {
                if added > 0 {
                    eprintln!("qcode-gui: imported {added} [schedules] entries as tasks");
                }
            }

            // Publish a session's queue rows, remembering what the UI was told so
            // an unchanged queue costs nothing.
            macro_rules! publish_to {
                ($target:expr) => {{
                    let target: SessionTarget = $target;
                    if let Some(queue) = host.queue(&target) {
                        let last = published.entry(target.clone()).or_default();
                        publish(&port.tagged(target.clone()), &queue, last);
                    }
                }};
            }

            // Run one queued message as its own turn on the engine task. The
            // engine drains the `next-step` inbox before every model request,
            // so steering sent while this turn is live lands in it.
            //
            // `$target` is the session the message belongs to. A draft becomes a
            // logged session on its first turn; every other target already names
            // one, so a background task's message can never land on the task the
            // window happens to be looking at.
            macro_rules! start_turn {
                ($target:expr, $text:expr, $attachments:expr) => {{
                    start_turn!(
                        $target,
                        $text,
                        $attachments,
                        None,
                        None,
                        engine.max_turns,
                        engine.max_cost_usd
                    )
                }};
                ($target:expr, $text:expr, $attachments:expr, $continuation:expr, $scheduled:expr, $max_turns:expr, $max_cost:expr) => {{
                    let target: SessionTarget = $target;
                    let target = match target.id() {
                        Some(_) => target,
                        None => {
                            let created = match engine.store.create_task(&SessionStarted {
                                model: model.clone(),
                                provider: provider_name.clone(),
                                cwd: engine.workspace.display().to_string(),
                                version: env!("CARGO_PKG_VERSION").to_string(),
                            }) {
                                Ok(created) => created,
                                Err(error) => {
                                    eprintln!("qcode-gui: cannot create a task: {error}");
                                    port.tagged(target).send_turn_failed(error.to_string());
                                    continue;
                                }
                            };
                            let new_target = SessionTarget::Session(created.id.clone());
                            // The draft becomes a session: drop whatever the
                            // draft slot held so nothing leaks by target.
                            host.close_if_idle(&target);
                            current = new_target.clone();
                            let port = port.tagged(new_target.clone());
                            port.send_event(&created.started);
                            port.send_event(&created.configured);
                            port.send_session_changed(Some(created.id.clone()));
                            new_target
                        }
                    };
                    let session_id = target.id().expect("a turn names a session").to_string();
                    // Every turn of a session runs on that session's slot: its
                    // cancel token and its follow-up inbox.
                    let (token, task_queue) = host.open(&target);
                    host.begin_turn(&target);
                    let sink = port.tagged(target.clone());
                    live.insert(target.clone(), token.clone());
                    let store = engine.store.clone();
                    let provider = provider.clone();
                    let tools = engine.tools.clone();
                    let policy = engine.policy.clone();
                    let snapshots = engine.snapshots.clone();
                    let network = engine.network.clone();
                    let workspace = engine.workspace.clone();
                    // 定时任务服务随回合注入：工具经 `ToolContext.scheduler`
                    // 拿到运行时 `Scheduler`（模型可创建 / 管理定时任务）。
                    let task_scheduler = scheduler.clone();
                    let max_turns = $max_turns;
                    let max_cost_usd = $max_cost;
                    let context_window = engine.context_window;
                    let task_model = model.clone();
                    let task_session = session_id;
                    let task_approver = match approval_deadline.take() {
                        Some(deadline) => sink.approver_with_deadline(deadline),
                        None => sink.approver(),
                    };
                    let task_questioner = sink.questioner();
                    let task_target = target;
                    let done = done_tx.clone();
                    let guard = TurnGuard {
                        done: done.clone(),
                        target: task_target.clone(),
                        armed: true,
                    };
                    let cancel = token.clone();
                    let task_inbox = task_queue.inbox();
                    let task_shell_env = engine.shell_env.clone();
                    tokio::spawn(async move {
                        // A panic anywhere below unwinds this task without
                        // reaching `done.send`; `guard` covers that path.
                        let mut guard = guard;
                        // 把引擎的流式 Delta（含上下文 token 估算）转发给 UI，
                        // 驱动打字机效果与 ctx% 进度。
                        let delta_port = sink.clone();
                        // ADR-0064: sample once (or when an rc file moved),
                        // then write what the agent will actually see. This
                        // runs on the turn's task, not the UI thread.
                        task_shell_env.current();
                        if let Some(report) = task_shell_env.take_audit() {
                            let _ = qcode_core::engine::SessionWriter::record_shell_audit(
                                &store,
                                &task_session,
                                (&report).into(),
                            );
                        }
                        let options = RunOptions {
                            session_id: &task_session,
                            model: &task_model,
                            input: $text,
                            cwd: workspace,
                            max_turns,
                            cancel,
                            pricing,
                            max_cost_usd,
                            on_delta: Some(DeltaSink::new(move |delta| {
                                delta_port.send_delta(delta)
                            })),
                            context_window,
                            agents_md: None,
                            skills: None,
                            network,
                            attachments: $attachments,
                            inbox: Some(task_inbox),
                            continuation: $continuation,
                            scheduled: $scheduled,
                        };
                        let result = run_turn(
                            TurnContext {
                                store: &store,
                                provider: provider.as_ref(),
                                tools: &tools,
                                policy: &policy,
                                approver: &task_approver,
                                snapshots: snapshots.as_ref(),
                                scheduler: Some(task_scheduler),
                                questioner: Some(std::sync::Arc::new(task_questioner)),
                            },
                            options,
                            move |event| sink.send_event(event),
                        )
                        .await;
                        guard.armed = false;
                        let _ = done.send((
                            task_target,
                            result.map(|_| ()).map_err(|error| error.to_string()),
                        ));
                    });
                }};
            }

            // 投递一个到期的定时任务组：同一固定会话的多个任务各自独立投递
            // （干净 prompt + 结构化标记），UI 据此渲染成「定时执行」卡片；落盘
            // 确认在 beat 扫描里按 messageId 查日志完成。投递只是把消息放进
            // 收件箱（瞬态内存）。
            macro_rules! deliver_schedule_group {
                ($session_id:expr, $tasks:expr) => {{
                    let session_id: String = $session_id;
                    let tasks: Vec<ScheduledTask> = $tasks;
                    let target = SessionTarget::Session(session_id.clone());
                    let busy = live.contains_key(&target);
                    let queue = host.queue(&target).unwrap_or_else(|| host.open(&target).1);
                    for task in &tasks {
                        let scheduled = qcode_core::schedule::ScheduledMessage {
                            task_id: task.id.clone(),
                            title: task.title.clone(),
                            prompt: task.prompt.clone(),
                            at: task
                                .next_fire_at
                                .map(|at| at.to_string())
                                .unwrap_or_default(),
                        };
                        let message = queue
                            .inbox()
                            .followup_scheduled(task.prompt.clone(), scheduled);
                        inflight.insert(task.id.clone());
                        pending_ack
                            .entry(message.id.clone())
                            .or_default()
                            .push((task.id.clone(), session_id.clone(), task.prompt.clone()));
                        if !busy {
                            approval_deadline = task.approval_timeout.or(approval_deadline);
                        }
                    }
                    // 空闲会话立刻开回合（一次一个，其余留在队列等回合结束）；
                    // 忙碌会话消息留在队列，回合结束后由 `done_rx` 分支接管。
                    if !busy
                        && let Some(claimed) = queue.finish()
                    {
                        start_turn!(
                            target.clone(),
                            claimed.text,
                            claimed.attachments,
                            None,
                            claimed.scheduled,
                            engine.max_turns,
                            engine.max_cost_usd
                        );
                    }
                }};
            }

            loop {
                tokio::select! {
                    // The engine claims steering from another thread, so the
                    // pending rows are refreshed on a slow tick rather than
                    // tied to a UI event.
                    _ = ticker.tick() => {
                        // The engine claims steering from its own thread, so the
                        // pending rows are refreshed on a slow tick. Every live
                        // session is republished, not just the visible one: a
                        // background turn's queue must not go stale.
                        let mut targets: Vec<SessionTarget> = live.keys().cloned().collect();
                        if !targets.contains(&current) {
                            targets.push(current.clone());
                        }
                        for target in targets {
                            publish_to!(target);
                        }
                    }
                    // The scheduler's beat (ADR-0059 §1–§3). Firing is just a
                    // prompt into a session's queue, so a scheduled turn runs
                    // through exactly the same path as a typed one.
                    _ = clock.tick() => {
                        let beat = std::time::Instant::now();
                        // A gap means the process was away: count the windows
                        // that passed and schedule the next one, without
                        // replaying them (a night's sleep must not release
                        // eleven turns at once).
                        let away = match last_beat {
                            None => true,
                            Some(last) => {
                                beat.duration_since(last) > gap_limit
                            }
                        };
                        last_beat = Some(beat);
                        let now = qcode_core::schedule::now_local();
                        for schedule in &schedules {
                            let previous = schedule_states
                                .get(&schedule.name)
                                .cloned()
                                .unwrap_or_default();
                            if away {
                                let recovery =
                                    qcode_core::schedule::recover(schedule, &previous, now);
                                let mut next = previous.clone();
                                next.next_fire_at = recovery.next_fire_at;
                                next.missed_windows = recovery.missed;
                                if recovery.missed > 0 {
                                    next.last_status =
                                        Some(ScheduleStatus::Missed(recovery.missed));
                                }
                                save_schedule_state(
                                    engine,
                                    &schedule.name,
                                    &next,
                                    &mut schedule_states,
                                );
                                continue;
                            }
                            // `new = true` creates its task at fire time, so
                            // there is nothing to be orphaned and nothing to
                            // look up.
                            let target = match &schedule.task {
                                Some(id) => SessionTarget::Session(id.clone()),
                                None => SessionTarget::Draft,
                            };
                            let exists = target
                                .id()
                                .is_none_or(|id| engine.store.task(id).is_ok());
                            let busy = live.contains_key(&target);
                            let decision =
                                qcode_core::schedule::tick(schedule, &previous, now, busy, exists);
                            let status = match decision {
                                Tick::NotYet => None,
                                Tick::SkipBusy => Some(ScheduleStatus::Skipped),
                                Tick::Orphaned => Some(ScheduleStatus::Orphaned),
                                Tick::Fire => {
                                    // A fresh task per fire: logs a session but
                                    // does not steal the view.
                                    let target = match target.id() {
                                        Some(_) => target.clone(),
                                        None => match create_task(
                                            engine,
                                            &provider_name,
                                            &port,
                                            &model,
                                            false,
                                        ) {
                                            Some(id) => SessionTarget::Session(id),
                                            None => {
                                                let next = qcode_core::schedule::advance(
                                                    schedule,
                                                    now,
                                                    ScheduleStatus::Failed(
                                                        "cannot create a task".to_string(),
                                                    ),
                                                );
                                                save_schedule_state(
                                                    engine,
                                                    &schedule.name,
                                                    &next,
                                                    &mut schedule_states,
                                                );
                                                continue;
                                            }
                                        },
                                    };
                                    let busy = live.contains_key(&target);
                                    let queue = host
                                        .queue(&target)
                                        .unwrap_or_else(|| host.open(&target).1);
                                    approval_deadline = schedule.approval_timeout;
                                    match queue.submit(
                                        schedule.prompt.clone(),
                                        Vec::new(),
                                        busy,
                                    ) {
                                        Submission::Start(message) => {
                                            start_turn!(
                                                target.clone(),
                                                message.text,
                                                message.attachments
                                            );
                                            Some(ScheduleStatus::Fired)
                                        }
                                        // `overlap = "queue"` on a busy session:
                                        // the message waits for the current turn.
                                        Submission::Queued => Some(ScheduleStatus::Fired),
                                    }
                                }
                            };
                            if let Some(status) = status {
                                let next = qcode_core::schedule::advance(schedule, now, status);
                                save_schedule_state(
                                    engine,
                                    &schedule.name,
                                    &next,
                                    &mut schedule_states,
                                );
                            }
                        }

                        // ==== 运行时定时任务（Scheduled Tasks）====
                        // 迁移导入后的任务由这里接管投递。与 config 条目共用同一
                        // 时钟 beat：cron 分钟级、interval ≥60s，30s beat 精度足够。
                        let now = qcode_core::schedule::now_local();

                        // ① 确认已投递：user/message 事件落盘 = 投递已确认
                        //（回执 ≠ 执行完成；以日志为事实源）。
                        let mut confirmed: Vec<String> = Vec::new();
                        for (message_id, pending) in pending_ack.iter() {
                            let mut all_landed = true;
                            for (task_id, session, prompt) in pending {
                                let landed = engine
                                    .store
                                    .schedule_delivery_landed(session, prompt)
                                    .unwrap_or(false);
                                if !landed {
                                    all_landed = false;
                                    continue;
                                }
                                if scheduler
                                    .update(task_id, |task| {
                                        let receipt = DeliveryReceipt {
                                            occurrence_at: task.next_fire_at.unwrap_or(now),
                                            delivered_at: now,
                                            message_id: message_id.clone(),
                                            target_session_id: Some(session.clone()),
                                        };
                                        if append_delivery(
                                            &mut task.delivery_log,
                                            receipt.clone(),
                                            now,
                                        ) {
                                            task.earlier_records_pruned = true;
                                        }
                                        task.last_receipt = Some(receipt);
                                        // 只有当下次触发点仍是「已触发」的时刻才推进
                                        // 到下一个未来时刻。RunNow 已把下一次拨到未来，
                                        // 这里再推会跳过一个周期。
                                        if task.next_fire_at.is_none_or(|next| next <= now) {
                                            match advance_rule(&task.rule, now) {
                                                Advance::Next(next) => {
                                                    task.next_fire_at = Some(next);
                                                }
                                                Advance::Ended => {
                                                    task.status = TaskStatus::Ended;
                                                    task.next_fire_at = None;
                                                }
                                            }
                                        }
                                    })
                                    .is_ok()
                                {
                                    inflight.remove(task_id);
                                }
                            }
                            if all_landed {
                                confirmed.push(message_id.clone());
                            }
                        }
                        // 投递落盘确认后通知 UI 刷新：立即执行的按钮脱离等待态。
                        let landed = !confirmed.is_empty();
                        for message_id in confirmed {
                            pending_ack.remove(&message_id);
                        }
                        if landed {
                            port.tagged(SessionTarget::Draft).send_schedule_changed();
                        }

                        // ② 扫描到期任务：跳过 in-flight 与 rearm 间隔内的。
                        let mut due: Vec<ScheduledTask> = Vec::new();
                        for task in scheduler.list() {
                            if task.status != TaskStatus::Active || inflight.contains(&task.id) {
                                continue;
                            }
                            if let Some(last) = &task.last_receipt
                                && now - last.delivered_at
                                    < time::Duration::seconds(
                                        task.rearm_after.as_secs() as i64,
                                    )
                            {
                                continue;
                            }
                            let target = task
                                .target
                                .session_id()
                                .map(|id| SessionTarget::Session(id.to_string()));
                            let busy = target
                                .as_ref()
                                .is_some_and(|target| live.contains_key(target));
                            let exists = target
                                .as_ref()
                                .and_then(|target| target.id())
                                .is_none_or(|id| engine.store.task(id).is_ok());
                            match task_tick(&task, now, busy, exists) {
                                TaskTick::Fire => due.push(task),
                                TaskTick::SkipBusy => {
                                    // 与 config 的 skip 一致：跳过本次并推进。
                                    let _ = scheduler.update(&task.id, |task| {
                                        match advance_rule(&task.rule, now) {
                                            Advance::Next(next) => {
                                                task.next_fire_at = Some(next);
                                            }
                                            Advance::Ended => {
                                                task.status = TaskStatus::Ended;
                                                task.next_fire_at = None;
                                            }
                                        }
                                    });
                                }
                                _ => {}
                            }
                        }

                        // ③ 分组投递：同一固定会话合并为一条消息；New/NewOnce
                        // 各自建会话再投。
                        let mut groups: BTreeMap<String, Vec<ScheduledTask>> = BTreeMap::new();
                        let mut fresh: Vec<ScheduledTask> = Vec::new();
                        for task in due {
                            match task.target.session_id() {
                                Some(id) => {
                                    groups.entry(id.to_string()).or_default().push(task);
                                }
                                None => fresh.push(task),
                            }
                        }
                        for task in fresh {
                            let Some(id) =
                                create_task(engine, &provider_name, &port, &model, false)
                            else {
                                continue; // 建会话失败：下一个 beat 再试。
                            };
                            let _ = scheduler.update(&task.id, |task| {
                                task.reserved_session_id = Some(id.clone());
                                task.target = Target::Session { id: id.clone() };
                            });
                            deliver_schedule_group!(id, vec![task]);
                        }
                        for (session_id, tasks) in groups {
                            deliver_schedule_group!(session_id, tasks);
                        }
                    }
                    command = commands.recv() => {
                        let Some(command) = command else {
                            break;
                        };
                        match command {
                            UiCommand::Answer {
                                call_id,
                                allow,
                                session: remember,
                            } => port.dispatch(UiCommand::Answer {
                                call_id,
                                allow,
                                session: remember,
                            }),
                            UiCommand::AnswerQuestion {
                                session,
                                call_id,
                                text,
                                answers,
                            } => {
                                // Fast path: the question is still open in the
                                // bridge registry (the tool is waiting). A
                                // timed-out question has no registry entry, so
                                // the answer is steered in as a user message —
                                // the running turn claims it at its next step
                                // boundary, and an idle leftover becomes its own
                                // follow-up turn.
                                let target = session
                                    .map(SessionTarget::Session)
                                    .unwrap_or_else(|| current.clone());
                                if !port.answer_question(&call_id, answers.clone())
                                    && let Some(queue) = host.queue(&target)
                                {
                                    queue.steer_question_reply(call_id.clone(), answers, text);
                                }
                            }
                            UiCommand::Cancel { session } => {
                                // Per-session stop (ADR-0058 §6): only that
                                // session's token is tripped, so a stop raced from
                                // an idle task cannot kill a background turn.
                                let target = session
                                    .map(SessionTarget::Session)
                                    .unwrap_or_else(|| current.clone());
                                host.cancel(&target);
                                if let Some(token) = live.get(&target) {
                                    token.cancel();
                                }
                            }
                            UiCommand::CancelAll => {
                                host.cancel_all();
                                for token in live.values() {
                                    token.cancel();
                                }
                            }
                            UiCommand::NewSession => {
                                // No stop: the outgoing task keeps its own turn
                                // (P5). Only its *backend* is recycled, and only
                                // if it is idle — an idle session replays from the
                                // log when it is focused again.
                                host.close_if_idle(&current);
                                current = create_empty_task(engine, &provider_name, &port, &model)
                                    .map(SessionTarget::Session)
                                    .unwrap_or(SessionTarget::Draft);
                            }
                            UiCommand::Resume { id } => {
                                // Switching is just re-pointing: the outgoing
                                // session's slot is dropped if it is idle, and a
                                // background turn keeps running untouched (P5).
                                let target = SessionTarget::Session(id.clone());
                                if target != current {
                                    host.close_if_idle(&current);
                                }
                                // A replayed session starts with a clean "Always"
                                // list, like a fresh window — but a session with a
                                // turn in flight keeps its own: switching away and
                                // back must not ask the same question again.
                                if !live.contains_key(&target) {
                                    port.clear_allow(&target);
                                }
                                current = target.clone();
                                port.tagged(target).send_session_changed(Some(id));
                            }
                            UiCommand::SetModel {
                                provider: next_provider,
                                model: next,
                            } => {
                                // Cross-provider switch: rebuild the provider
                                // for the target route, keeping the session,
                                // tools and policy intact. Failure keeps the
                                // previous provider and reports the reason.
                                if next_provider != provider_name {
                                    match qcode_kernel::build_provider_for(
                                        Some(&engine.config_path),
                                        &engine.workspace,
                                        &next_provider,
                                        &next,
                                        engine.proxy_url.as_deref(),
                                    ) {
                                        Ok(new_provider) => {
                                            provider = new_provider.into();
                                            provider_name = next_provider;
                                        }
                                        Err(error) => {
                                            eprintln!(
                                                "qcode-gui: cannot switch provider: {error}"
                                            );
                                            port.tagged(current.clone()).send_turn_failed(format!(
                                                "无法切换到 {next_provider}：{error}"
                                            ));
                                        }
                                    }
                                }
                                model = next;
                                pricing = engine.pricing_table.get(&model).copied();
                                // “切即默认”：切换模型同时写进 [default-model]，
                                // 新会话默认用它；写失败只记日志，不阻断切换。
                                if let Err(error) = qcode_kernel::config_edit::set_default_model(
                                    &engine.config_path,
                                    Some(provider_name.as_str()),
                                    Some(model.as_str()),
                                ) {
                                    eprintln!("qcode-gui: cannot persist default model: {error}");
                                }
                            }
                            UiCommand::Prompt {
                                session,
                                text,
                                attachments,
                            } => {
                                // The message carries its session, so a stale
                                // `current` can never post it to another task.
                                let target = session
                                    .map(SessionTarget::Session)
                                    .unwrap_or_else(|| current.clone());
                                // Running: the message waits in this session's
                                // follow-up queue; idle: it starts its own turn.
                                let running = live.contains_key(&target);
                                let queue = host
                                    .queue(&target)
                                    .unwrap_or_else(|| host.open(&target).1);
                                match queue.submit(text, attachments, running) {
                                    Submission::Start(message) => start_turn!(
                                        target.clone(),
                                        message.text,
                                        message.attachments
                                    ),
                                    Submission::Queued => {}
                                }
                                publish_to!(target);
                            }
                            UiCommand::Continue { raise } => {
                                // Only an idle engine continues: a live turn
                                // already has a window, and a queue submit
                                // while paused is itself a continue.
                                if !live.contains_key(&current) {
                                    let raise = raise.unwrap_or_default();
                                    let renewal = Some(qcode_core::event::Continuation {
                                        // The engine fills these in from the log.
                                        turn: 0,
                                        window: 1,
                                        raise: (raise.turns.is_some() || raise.usd.is_some())
                                            .then(|| raise.into()),
                                    });
                                    // The new limits apply to this window only;
                                    // the next turn falls back to the config.
                                    let turns = raise.turns.unwrap_or(engine.max_turns);
                                    let cost = raise.usd.or(engine.max_cost_usd);
                                    start_turn!(
                                        current.clone(),
                                        qcode_core::engine::CONTINUE_PROMPT.to_string(),
                                        Vec::new(),
                                        renewal,
                                        None,
                                        turns,
                                        cost
                                    );
                                }
                            }
                            UiCommand::Steer { text, attachments } => {
                                // Only a live turn can take steering; the dock
                                // does not offer it while idle.
                                let target = current.clone();
                                let running = live.contains_key(&target);
                                let queue = host
                                    .queue(&target)
                                    .unwrap_or_else(|| host.open(&target).1);
                                queue.steer(text, attachments, running);
                                publish_to!(target);
                            }
                            UiCommand::QueueOp { id, op } => {
                                let target = current.clone();
                                let running = live.contains_key(&target);
                                let queue = host
                                    .queue(&target)
                                    .unwrap_or_else(|| host.open(&target).1);
                                match op {
                                    // Steering only reaches the running turn;
                                    // an idle engine leaves the queue untouched.
                                    QueueOp::Steer => {
                                        queue.steer_queued(&id, running);
                                    }
                                    QueueOp::MoveUp => {
                                        queue.move_queued(&id, -1);
                                    }
                                    QueueOp::MoveDown => {
                                        queue.move_queued(&id, 1);
                                    }
                                    QueueOp::Remove => {
                                        queue.remove_queued(&id);
                                    }
                                    QueueOp::Unsteer => {
                                        queue.unsteer(&id);
                                    }
                                }
                                publish_to!(target);
                            }
                            UiCommand::ScheduleOp(op) => {
                                let changed = !matches!(
                                    op,
                                    ScheduleOp::List | ScheduleOp::Get { .. }
                                );
                                let now = qcode_core::schedule::now_local();
                                let result: Result<Option<String>, String> = match op {
                                    ScheduleOp::Create {
                                        title,
                                        prompt,
                                        rule,
                                        target,
                                        overlap,
                                        approval_timeout,
                                        rearm_after,
                                    } => scheduler
                                        .create(
                                            NewTask {
                                                title,
                                                prompt,
                                                rule,
                                                target,
                                                overlap,
                                                approval_timeout,
                                                rearm_after,
                                            },
                                            now,
                                        )
                                        .map(|task| {
                                            Some(format!(
                                                "已创建定时任务 `{}`",
                                                task.title
                                            ))
                                        })
                                        .map_err(|error| error.to_string()),
                                    ScheduleOp::List => Ok(Some(format!(
                                        "共 {} 个定时任务",
                                        scheduler.list().len()
                                    ))),
                                    ScheduleOp::Get { id } => match scheduler.find(&id) {
                                        Some(task) => Ok(Some(format!("{task:?}"))),
                                        None => Err("定时任务不存在".to_string()),
                                    },
                                    ScheduleOp::Edit {
                                        id,
                                        title,
                                        prompt,
                                        rule,
                                        target,
                                        overlap,
                                        approval_timeout,
                                        rearm_after,
                                    } => scheduler
                                        .edit(
                                            &id,
                                            EditTask {
                                                title,
                                                prompt,
                                                rule,
                                                target,
                                                overlap,
                                                approval_timeout,
                                                rearm_after,
                                            },
                                            now,
                                        )
                                        .map(|_| Some("已保存".to_string()))
                                        .map_err(|error| error.to_string()),
                                    ScheduleOp::Pause { id } => scheduler
                                        .pause(&id)
                                        .map(|_| Some("已暂停".to_string()))
                                        .map_err(|error| error.to_string()),
                                    ScheduleOp::Resume { id } => scheduler
                                        .resume(&id, now)
                                        .map(|_| Some("已恢复".to_string()))
                                        .map_err(|error| error.to_string()),
                                    ScheduleOp::RunNow { id } => {
                                        match scheduler.run_now(&id, now) {
                                            Ok(()) => {
                                                // 立即投递，不等下一个 beat：点击
                                                // 即有反应（ADR-0059 §4）。投递放
                                                // 在闭包之外：`start_turn!` 的失败
                                                // 分支用 `continue`，只能在循环上下
                                                // 文展开。
                                                if let Some(task) = scheduler.find(&id) {
                                                    match task.target.session_id() {
                                                        Some(session) => {
                                                            deliver_schedule_group!(
                                                                session.to_string(),
                                                                vec![task]
                                                            );
                                                        }
                                                        None => {
                                                            if let Some(new_id) = create_task(
                                                                engine,
                                                                &provider_name,
                                                                &port,
                                                                &model,
                                                                false,
                                                            ) {
                                                                deliver_schedule_group!(
                                                                    new_id,
                                                                    vec![task]
                                                                );
                                                            }
                                                        }
                                                    }
                                                }
                                                // 立即把下次触发拨到最近的未来时刻，详情
                                                // 页不用等落盘确认就显示正确的下次执行时间。
                                                let _ = scheduler.update(&id, |task| {
                                                    match advance_rule(&task.rule, now) {
                                                        Advance::Next(next) => {
                                                            task.next_fire_at = Some(next);
                                                        }
                                                        Advance::Ended => {
                                                            task.status = TaskStatus::Ended;
                                                            task.next_fire_at = None;
                                                        }
                                                    }
                                                });
                                                Ok(Some("已触发".to_string()))
                                            }
                                            Err(error) => Err(error.to_string()),
                                        }
                                    }
                                    ScheduleOp::Delete { id } => scheduler
                                        .delete(&id)
                                        .map(|_| Some("已删除".to_string()))
                                        .map_err(|error| error.to_string()),
                                };
                                match result {
                                    Ok(_) if changed => port
                                        .tagged(SessionTarget::Draft)
                                        .send_schedule_changed(),
                                    Err(message) => port
                                        .tagged(SessionTarget::Draft)
                                        .send_schedule_notice(message),
                                    _ => {}
                                }
                            }
                        }
                    }
                    Some((target, result)) = done_rx.recv() => {
                        // The finished session is the *one that finished*: with
                        // several live, cleaning up `current` would clear the
                        // wrong task's badge (P5).
                        live.remove(&target);
                        if let Err(error) = result {
                            eprintln!("qcode-gui: turn failed: {error}");
                            port.tagged(target.clone()).send_turn_failed(error);
                        }
                        host.end_turn(&target);
                        port.tagged(target.clone())
                            .send_turn_finished(target.id().map(str::to_string));
                        // Drain that session's follow-up queue: the next queued
                        // message starts its own turn now that it is idle. Another
                        // session's queue is untouched.
                        if let Some(queue) = host.queue(&target)
                            && let Some(message) = queue.finish()
                        {
                            start_turn!(target.clone(), message.text, message.attachments);
                        }
                        publish_to!(target);
                    }
                }
            }
        });
    });
}
