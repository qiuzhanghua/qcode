//! Engine runtime: turns `UiCommand`s into `run_turn` calls, owns the session
//! and the follow-up inbox, and streams `UiEvent`s back.
//!
//! Deliberately free of GPUI: it only needs qcode-core, qcode-ui-bridge and
//! tokio, so the queue/turn policy can be tested without opening a window.

use qcode_core::engine::{DeltaSink, RunOptions, TurnContext, run_turn};
use qcode_core::event::SessionStarted;
use qcode_core::inbox::{QueuedMessage, SessionQueue, Submission};
use qcode_core::schedule::{Schedule, ScheduledMessage, ScheduledTask};
use qcode_core::scheduler::{EditTask, NewTask, Scheduler};
use qcode_core::store::SessionStore;
use qcode_ui_bridge::{KernelPort, QueueOp, ScheduleOp, SessionTarget, TaggedPort, UiCommand};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::host::SessionHost;
use qcode_driver::runtime::Host as _;

/// The pieces a running window needs to execute turns.
///
/// Everything here comes from `qcode_kernel::assemble`; the window never wires
/// its own provider or tools. Held once behind [`crate::host::Deps`] so every
/// session shares it instead of cloning a provider per session (ADR-0058 §1).
pub struct Engine {
    pub store: SessionStore,
    pub provider: std::sync::Arc<dyn qcode_core::provider::Provider>,
    pub provider_name: String,
    /// The sandbox backend name (`none`, `bwrap`, `landlock`, …) this engine
    /// was assembled with, for the status bar. Carried on the engine so a
    /// settings rebuild can push the new value onto the UI instead of leaving
    /// the bar on the value the window started with.
    pub sandbox: String,
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
    /// Rendered `# Skills` index, injected into every turn's system prompt.
    /// Same string `qc run` and the TUI pass (ADR-0087): without it the model
    /// cannot see the skills at all, so `/skills` would list what it may not
    /// use.
    pub skills_prompt: Option<String>,
    /// The workspace `AGENTS.md`, injected into the system prompt. The GUI used
    /// to hardcode `None` here while the other two frontends loaded it.
    pub agents_md: Option<String>,
    /// The environment `run_shell` injects (ADR-0064); the turn samples it
    /// first and records what the agent sees.
    pub shell_env: std::sync::Arc<qcode_tools::shell_env::ShellEnvCache>,
    /// Scheduled turns from `[schedules]` (ADR-0059), already resolved.
    ///
    /// This is a **declaration**, not a mechanism: at startup it is reconciled
    /// into the task table (`Origin::Config`), and from then on the driver fires
    /// it like any other task (ADR-0093). Empty = nothing declared.
    pub schedules: Vec<qcode_core::schedule::Schedule>,
    /// Every `[schedules.<name>]` key in the config, including the ones that
    /// failed to resolve.
    ///
    /// Reconciliation needs "is it still declared", which is a question about
    /// the file, not about the successfully parsed subset: a cron typo must not
    /// look like "the user deleted this entry" and take the task's delivery
    /// history with it.
    pub schedule_declared: Vec<String>,
    /// `[schedules]` entries that did not resolve, with the reason. Shown as a
    /// broken row so a typo is visible instead of looking like "not configured".
    pub schedule_errors: Vec<(String, String)>,
    /// 会话标题自动生成策略；`None` = 未开启（会话标题自动生成设计）。
    pub session_title: Option<qcode_core::title::TitleConfig>,
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
        parent: None,
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
            // The user sees this in the UI too (`send_turn_failed`); the log is
            // for reading afterwards, which is the point of ADR-0077.
            tracing::warn!(%error, "cannot create task");
            port.tagged(SessionTarget::Draft)
                .send_turn_failed(error.to_string());
            None
        }
    }
}

/// 窗口对 [`qcode_driver::runtime::Host`] 的回答。
///
/// 驱动问的每一件事，只有窗口知道答案：谁的会话正在跑回合（`live` 是本进程的
/// 权威）、谁的会话还在（store 的权威）、消息往哪个收件箱投（`SessionHost`）。
/// `qcode-driver` 自己一样都读不到，所以由这里喂。
///
/// 每拍现造一个、拍完就丢：它借的东西（`live`、`model`、`provider_name`）会在
/// 别的 select 分支里被改，而借用只活在这一拍之内。
struct GuiHost<'a> {
    engine: &'a Engine,
    port: &'a KernelPort,
    host: &'a SessionHost,
    live: &'a HashMap<SessionTarget, CancellationToken>,
    scheduler: &'a Scheduler,
    /// 写进心跳的实例标识（ADR-0088 §3）。
    driver_instance: &'a str,
    model: &'a str,
    provider_name: &'a str,
    /// 本拍已经开过回合的会话。投递是逐任务的，而 `live` 要等 `start_turn!`
    /// 跑完才更新——所以这一拍里第二个任务问「忙不忙」会得到「不忙」，于是又去
    /// 开一个回合，同一个会话就并发跑两个回合了。
    starting: HashSet<String>,
}

/// 目标会话还在吗。
///
/// `New` / `NewOnce` 没有绑定会话 → **恒真**：它每次投递自己建会话，判成「不在」
/// 会让一条「每天新建会话」的任务被永久标记为孤儿。
fn target_exists(store: &SessionStore, task: &ScheduledTask) -> bool {
    task.target
        .session_id()
        .is_none_or(|id| store.task(id).is_ok())
}

/// 目标会话此刻正在跑回合吗。
///
/// `starting` 是「本拍刚为它开过回合」：`live` 要等 `start_turn!` 跑完才更新，
/// 而一拍里可能连续投递到同一个会话。
fn target_busy(
    live: &HashMap<SessionTarget, CancellationToken>,
    starting: &HashSet<String>,
    task: &ScheduledTask,
) -> bool {
    let Some(id) = task.target.session_id() else {
        return false;
    };
    starting.contains(id) || live.contains_key(&SessionTarget::Session(id.to_string()))
}

/// 「立即执行」准入闸。`Some(原因)` = 拒绝，`None` = 放行。
///
/// 两道闸都是跨进程 / 全局信号：
/// - 目标会话锁（ADR-0060）：任一进程（GUI 或 qcd）里只要这个会话有活动回合，
///   `SessionWriter` 就持有 `locks/<id>.lock`，这里非阻塞探测即知「忙」；探测
///   成功（拿到锁）立刻 drop 释放，无副作用。`new` 目标没有固定会话，天然不忙，
///   直接放行（对齐 dsh：每轮投递独立新会话，互不干扰）。
/// - 最小投递间隔（60s）：上次投递还没到护栏时刻也拒绝，防连续狂点。
///
/// 命中闸门时调用方**不得** run_now / 投递 / 登记 / advance——任务状态零变更，
/// 并回 `ScheduleRunNowRejected` 让 shell 解除「执行中…」等待态（否则这个乐观
/// 状态没有回执可依、按钮永远卡住）。
/// 返回拒收理由（`None` = 放行）。
///
/// 返回 `Option<std::borrow::Cow<'static, str>>` 而不是 `&'static str`：理由是界面
/// 文案，跟着 locale 走。
fn run_now_admission(
    store: &SessionStore,
    task: &ScheduledTask,
) -> Option<std::borrow::Cow<'static, str>> {
    let locked = task
        .target
        .session_id()
        .or(task.reserved_session_id.as_deref())
        .is_some_and(|session| {
            matches!(
                store.lock(session),
                Err(qcode_core::store::StoreError::Locked(_))
            )
        });
    if locked {
        return Some(qcode_i18n::tr("gui.schedule.reject.busy"));
    }
    // 护栏时刻**还没到**才拒绝：`delivery_floor_end` 只回答「最早可再投时刻」，
    // 不判断它是否已过——拿 `.is_some()` 判断会把「投过一次」当成永远拒绝。
    let now = qcode_core::schedule::now_local();
    if qcode_core::schedule::delivery_floor_end(task).is_some_and(|end| end > now) {
        return Some(qcode_i18n::tr("gui.schedule.reject.too_frequent"));
    }
    None
}

impl GuiHost<'_> {
    /// 「立即执行」：投递一条任务并返回（目标会话，投递结果）。`None` = 建会话失败。
    ///
    /// 与 beat 那一条走**同一段代码**（`create_session` → `bind_session` →
    /// `deliver`），差别只在触发源：这里是用户点了按钮，beat 是时钟到点。
    /// ADR-0092 §6 要的就是这个——两份各自演化的投递路径迟早会不一致，而
    /// 「立即执行」与「到点执行」表现不同是用户最难理解的一类错。
    ///
    /// 返回整个 `Delivery`（而不是只取 `.start`）：调用方登记确认时需要
    /// `message_id`（驱动按它查「这条落盘了吗」），而忙碌会话的投递没有 `start`，
    /// 目标会话仍要从这里带出去。
    fn deliver_now(
        &mut self,
        task: &ScheduledTask,
        approval: Option<Duration>,
    ) -> Option<(String, qcode_driver::runtime::Delivery)> {
        let session = match task.target.session_id() {
            Some(id) => id.to_string(),
            None => {
                let id = self.create_session()?;
                qcode_driver::runtime::bind_session(self, &task.id, &id);
                id
            }
        };
        let delivery = self.deliver(&session, task, approval)?;
        Some((session, delivery))
    }
}

impl qcode_driver::runtime::Host for GuiHost<'_> {
    fn now(&self) -> time::PrimitiveDateTime {
        qcode_core::schedule::now_local()
    }

    fn heartbeat(&mut self, at: time::PrimitiveDateTime) {
        // 先留心跳，再判 away（ADR-0088 §3）：别的进程要能看出「这个数据目录有人
        // 在推动时间」。挂起期间心跳本来就该停在旧值上，那正是「不在跑」的证据。
        if let Err(error) = self.engine.store.set_runtime_status(
            qcode_core::scheduler::DRIVER_HEARTBEAT_KEY,
            self.driver_instance,
            at,
        ) {
            tracing::warn!(%error, "cannot write driver heartbeat");
        }
    }

    fn executor(&self) -> qcode_driver::Executor {
        // 窗口开着 = 人在旁边。无人值守的缺省审批截止时间从这里开始生效
        // （ADR-0092 §7）：`None` 就是「一直等人工」，不许改。
        qcode_driver::Executor::Attended
    }

    fn tasks(&self) -> Vec<ScheduledTask> {
        self.scheduler.list()
    }

    fn update_task(&mut self, id: &str, patch: &mut dyn FnMut(&mut ScheduledTask)) -> bool {
        self.scheduler.update(id, patch).is_ok()
    }

    fn is_busy(&self, task: &ScheduledTask) -> bool {
        target_busy(self.live, &self.starting, task)
    }

    fn exists(&self, task: &ScheduledTask) -> bool {
        target_exists(&self.engine.store, task)
    }

    fn create_session(&mut self) -> Option<String> {
        create_task(
            self.engine,
            self.provider_name,
            self.port,
            self.model,
            false,
        )
    }

    fn deliver(
        &mut self,
        session: &str,
        task: &ScheduledTask,
        approval: Option<Duration>,
    ) -> Option<qcode_driver::runtime::Delivery> {
        let target = SessionTarget::Session(session.to_string());
        let busy = self.live.contains_key(&target) || self.starting.contains(session);
        let queue = self
            .host
            .queue(&target)
            .unwrap_or_else(|| self.host.open(&target).1);
        let scheduled = ScheduledMessage {
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
            .followup_scheduled(task.prompt.clone(), scheduled.clone());
        // 空闲会话立刻开回合（一次一个，其余留在收件箱等回合结束）。
        let start = if busy {
            None
        } else {
            queue.finish().map(|claimed| {
                self.starting.insert(session.to_string());
                qcode_driver::runtime::StartTurn {
                    session: session.to_string(),
                    text: claimed.text,
                    // 用消息上带着的那份标记，而不是这里临时构造的：`finish()`
                    // 认领的就是它，两边不一致会让回执与卡片对不上。
                    scheduled: claimed.scheduled.unwrap_or(scheduled),
                    approval,
                }
            })
        };
        Some(qcode_driver::runtime::Delivery {
            message_id: message.id,
            start,
        })
    }

    fn landed(&self, session: &str, task_id: &str, at: &str) -> qcode_driver::runtime::Landed {
        use qcode_driver::runtime::Landed;
        match self
            .engine
            .store
            .schedule_delivery_landed(session, task_id, at)
        {
            Ok(true) => Landed::Yes,
            Ok(false) => Landed::NotYet,
            // 读不出来（会话被删、日志损坏）不能当成「还没落盘」：那会让这个槽位
            // 永久卡在 in-flight，而任务从此不再被投递。
            Err(_) => Landed::Gone,
        }
    }

    fn changed(&mut self) {
        self.port
            .tagged(SessionTarget::Draft)
            .send_schedule_changed();
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

/// Reports a panicking turn to the app loop.
///
/// `run_turn` runs inside `tokio::spawn`, so a panic — a bug in a tool, in a
/// provider decoder, or an `unwrap` on model output — unwinds the spawned task
/// and the `done` message is never sent. Without this the loop keeps `active`
/// set forever: the UI stays on "running", Stop targets a task that is gone,
/// and new sessions keep being refused. Dropping the guard on unwind turns
/// that into the same `TurnFailed` a returned `Err` produces (ADR-0058 §9 P0).
struct TurnGuard {
    done: tokio::sync::mpsc::UnboundedSender<(
        SessionTarget,
        Option<ScheduledMessage>,
        Result<(), String>,
    )>,
    target: SessionTarget,
    /// The scheduled-task delivery this turn belongs to, if any. Carried to the
    /// finish branch so its result can be logged against the task that fired it
    /// (a panicking turn still reports via [`Drop`], which is why this lives
    /// here rather than in the send inside `start_turn!`).
    scheduled: Option<ScheduledMessage>,
    armed: bool,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.done.send((
                self.target.clone(),
                self.scheduled.clone(),
                Err(qcode_i18n::tr("gui.err.turn_panicked").to_string()),
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
                tracing::warn!(%error, "cannot start the runtime");
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
            // 谁在开车：先抢驱动锁（ADR-0091）。抢到才是这个数据目录的时钟；
            // 抢不到（另一个前端已经在推）就**不驱动**——窗口照常，只是不推动时间。
            //
            // 在此之前这里没有任何互斥，靠的是「GUI 单实例」这个隐含前提（ADR-0083），
            // 而那条规则的作用域是显示会话，不是数据目录。ADR-0092 的 daemon 一旦落地，
            // 两个驱动会同时在扫同一份任务表。
            let mut driver_lock = match engine.store.try_lock_file(qcode_core::scheduler::DRIVER_LOCK) {
                Ok(Some(lock)) => Some(lock),
                Ok(None) => {
                    tracing::info!(
                        "another process holds the driver lock; this window will not schedule"
                    );
                    // 告诉用户，而不是安静地少做一件事：换个「没有驱动」的窗口来
                    // 建任务，用户会以为任务会跑。走 notice 通道是因为它已经是
                    // 「说一句给用户看」的现成机制，不必为此新加一套界面状态。
                    port.tagged(SessionTarget::Draft).send_schedule_notice(
                        qcode_i18n::tr("gui.schedule.notice_other_driver").into_owned(),
                    );
                    None
                }
                Err(error) => {
                    // 拿不到锁就退化成「不驱动」而不是「假装在驱动」：后者会让两个
                    // 进程都认为自己在推时间，而重复投递是没有提示的那种错。
                    tracing::warn!(%error, "cannot take the driver lock; not scheduling");
                    port.tagged(SessionTarget::Draft).send_schedule_notice(
                        qcode_i18n::tr("gui.schedule.no_driver_lock").to_string(),
                    );
                    None
                }
            };

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
            let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<(
                SessionTarget,
                Option<ScheduledMessage>,
                Result<(), String>,
            )>();
            let mut ticker = tokio::time::interval(Duration::from_millis(250));
            // One clock for the whole window, not one per session (ADR-0059):
            // a minute is the resolution of a cron expression, and 30s leaves
            // room for the tick to drift without skipping a window.
            let beat = engine
                .schedule_tick
                .unwrap_or(Duration::from_secs(30))
                .max(Duration::from_millis(50));
            let mut clock = tokio::time::interval(beat);
            // 一拍之内的全部决策与记账在驱动里（ADR-0092 §6）：away 判定、
            // 崩溃恢复、投递、确认。窗口只提供 Host 并消费「该开的回合」。
            //
            // 循环仍留在窗口：`select!` 里除了时钟还有 UI 命令、回合结束、队列
            // 发布、会话关闭，那些是窗口的事。把循环也搬进驱动只会造出第三个
            // 循环，见 `qcode_driver::runtime` 的模块注释。
            let mut schedule_driver =
                qcode_driver::runtime::ScheduleDriver::new(qcode_driver::runtime::Clock::new(beat));
            // 谁在开车：写进心跳，供没有时钟的前端（TUI / CLI）判定「驱动在跑」。
            // 用进程启动时刻而不是 pid：pid 会被复用，读到一个 pid 无法证明它就是
            // 当初写下的那个进程（ADR-0083 对 pid 文件的否定同理）。
            let driver_instance = format!(
                "gui:{}",
                qcode_core::schedule::now_local()
                    .to_string()
                    .replace(' ', "T")
            );
            let mut published: HashMap<SessionTarget, Published> = HashMap::new();
            // 「立即执行」投递后的提前确认信号（见 `ScheduleOp::RunNow` 分支）：
            // 回执确认不必等下一个 beat（默认 30s），消息一落盘就该确认。
            let (confirm_tx, mut confirm_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

            // An approval prompt from a scheduled turn expires (ADR-0059 §4):
            // nobody is watching, so it must not hold the session lock all
            // night. Set right before `start_turn!` and taken by it.
            let mut approval_deadline: Option<Duration> = None;

            // 定时任务：**一套模型**（ADR-0093）。
            //
            // `[schedules.<name>]` 条目在这里被对齐进任务表，此后与用户建的任务
            // 走同一条投递路径（下面的 `schedule_driver`）。曾经配置条目由 GUI
            // 自己再投一次，于是同一个条目一分钟投两遍——真机实测过。
            let scheduler = Arc::new(Scheduler::new(engine.store.clone()));
            //
            // `declared` 取的是 `config.toml` 里的**键**，不是 resolved 之后的
            // 列表：cron 写坏时条目会被挑进 `schedule_errors`，按 resolved 判存在
            // 性就会把任务连投递历史一起删掉。
            let declared: Vec<String> = engine.schedule_declared.clone();
            let schedules: Vec<Schedule> = engine.schedules.clone();
            match scheduler.reconcile_config(&declared, &schedules, qcode_core::schedule::now_local())
            {
                Ok(report) if report.touched() => {
                    tracing::info!(
                        created = report.created,
                        updated = report.updated,
                        removed = report.removed,
                        "[schedules] reconciled into tasks"
                    );
                    // **必须广播**：侧栏的定时任务列表读的是任务表，而它只在收到
                    // 事件时刷新（`shell/mod.rs` 的 `ScheduleChanged` 分支）。
                    // 不广播的话，配置声明的条目在启动后一直是「还没有定时任务」
                    // ——要等到用户碰巧做点别的事才出现。这是 ADR-0096 把列表的
                    // 来源从 config 换成任务表时漏掉的一环。
                    //
                    // 走 `Draft` 是这条事件与具体会话无关（host 级）的惯例：
                    // `Host::changed` 也这么发。
                    port.tagged(SessionTarget::Draft).send_schedule_changed();
                }
                Ok(_) => {}
                // 不致命：任务表这一次没对齐，投递照旧按上次的样子跑。
                Err(error) => tracing::warn!(%error, "cannot reconcile [schedules]"),
            }

            // 挂任务表文件监听：任何进程（GUI 自己或脱离 GUI 常驻的 `qcd`）写
            // `schedule/tasks.json` 时，UI 都能立刻刷新详情页与运行记录。通知来自
            // OS 而不是驱动进程——qcd 开车时它不推事件（阶段 2 只写日志），而双方
            // 唯一的交集就是这张表。见 `fs_watch` 模块。
            crate::fs_watch::spawn_schedule_watcher(&port, engine.store.root());

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
                    // 求值一次，供 options / guard / done 三处复用；`$scheduled`
                    // 在调用点通常是会移动的字段，直接写多次会 use-after-move。
                    let task_scheduled = $scheduled;
                    let target: SessionTarget = $target;
                    let target = match target.id() {
                        Some(_) => target,
                        None => {
                            let created = match engine.store.create_task(&SessionStarted {
                                model: model.clone(),
                                provider: provider_name.clone(),
                                cwd: engine.workspace.display().to_string(),
                                version: env!("CARGO_PKG_VERSION").to_string(),
                                parent: None,
                            }) {
                                Ok(created) => created,
                                Err(error) => {
                                    tracing::warn!(%error, "cannot create a task");
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
                    let title_config = engine.session_title.clone();
                    // ADR-0087: the GUI used to leave both of these `None`, so
                    // the model never saw `AGENTS.md` or the skills index.
                    let turn_skills = engine.skills_prompt.clone();
                    let turn_agents_md = engine.agents_md.clone();
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
                        scheduled: task_scheduled.clone(),
                        armed: true,
                    };
                    let cancel = token.clone();
                    let task_inbox = task_queue.inbox();
                    let task_shell_env = engine.shell_env.clone();
                    // 会话标题生成完成后通知 UI 刷新任务列表。端口必须在 spawn
                    // 前 clone：在闭包内再 `port.tagged(..)` 会把外层循环的
                    // `port` 整个 move 进闭包，后续迭代就没法用了。
                    let title_port = port.tagged(SessionTarget::Session(task_session.clone()));
                    // 会话标题自动生成（会话标题自动生成设计）：回合开始前就并行
                    // 启动——模型调用与回着重叠，落盘等待会话锁（回合结束必然
                    // 释放），回合一结束标题立即就位并刷新侧栏。首条消息尚未写入
                    // 日志，把本次输入作为 pending 来源传给生成器。
                    if let Some(title_config) = title_config {
                        let gen_store = store.clone();
                        let gen_provider = provider.clone();
                        let gen_session = task_session.clone();
                        let gen_prompt = $text.clone();
                        let gen_port = title_port.clone();
                        tokio::spawn(async move {
                            let gen_cancel = tokio_util::sync::CancellationToken::new();
                            match qcode_core::title::generate_title(
                                gen_provider.as_ref(),
                                &gen_store,
                                &gen_session,
                                &title_config,
                                &gen_cancel,
                                Some(&gen_prompt),
                            )
                            .await
                            {
                                Ok(Some(_)) => gen_port.send_tasks_changed(),
                                Ok(None) | Err(_) => {}
                            }
                        });
                    }
                    tokio::spawn(async move {
                        // A panic anywhere below unwinds this task without
                        // reaching `done.send`; `guard` covers that path.
                        let mut guard = guard;
                        // 回合结果要能关联回触发它的定时任务：在 `task_scheduled`
                        // 被 options 消费前留一份给 `done.send`。
                        let scheduled_for_done = task_scheduled.clone();
                        // 定时任务可指定工作目录：投递回合用任务的 cwd，缺省回落
                        // 到引擎工作目录。
                        let cwd = task_scheduled
                            .as_ref()
                            .and_then(|scheduled| task_scheduler.find(&scheduled.task_id))
                            .and_then(|task| task.cwd)
                            .map(PathBuf::from)
                            .unwrap_or_else(|| workspace.clone());
                        // 把引擎的流式 Delta（含上下文 token 估算）转发给 UI，
                        // 驱动打字机效果与 ctx% 进度。
                        let delta_port = sink.clone();
                        // ADR-0087: the GUI registers `subagent` too. The child
                        // registry is this turn's registry minus the tools a
                        // child must not have — nesting stops at depth one, and
                        // `ask_user` belongs to the runtime root. Scheduling is
                        // dropped for a different reason since ADR-0088: it is
                        // no longer "GUI only" (every frontend can register),
                        // but a child's cron entry outlives the parent turn it
                        // was created in — a delegatable job must not leave
                        // persistent side effects behind, exactly as in
                        // `qc run` and the TUI.
                        let mut tools = tools;
                        {
                            let child_port = sink.clone();
                            let child_tools = tools.without(
                                &[
                                    qcode_tools::schedule::SCHEDULE_TOOL_NAMES.as_slice(),
                                    qcode_tools::ask::ASK_USER_TOOL_NAMES.as_slice(),
                                ]
                                .concat(),
                            );
                            let on_child: qcode_kernel::subagent::ChildEventSink =
                                std::sync::Arc::new(move |event: &qcode_core::event::Event| {
                                    child_port.send_child(event);
                                });
                            let runner = qcode_kernel::subagent::build(
                                &store,
                                provider.clone(),
                                child_tools,
                                &policy,
                                std::sync::Arc::new(task_approver.clone()),
                                &workspace,
                                &task_model,
                                context_window,
                                pricing,
                                network.clone(),
                                snapshots.clone(),
                                Some(on_child),
                            );
                            tools.extend([std::sync::Arc::new(
                                qcode_tools::subagent::SubagentTool::new(runner),
                            )
                                as std::sync::Arc<dyn qcode_core::tool::Tool>]);
                        }
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
                            cwd,
                            max_turns,
                            cancel,
                            pricing,
                            max_cost_usd,
                            on_delta: Some(DeltaSink::new(move |delta| {
                                delta_port.send_delta(delta)
                            })),
                            context_window,
                            agents_md: turn_agents_md.clone(),
                            skills: turn_skills.clone(),
                            network,
                            attachments: $attachments,
                            inbox: Some(task_inbox),
                            continuation: $continuation,
                            scheduled: task_scheduled,
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
                                // 时钟就在这个进程里（下面那个 `clock` 分支），
                                // 所以"谁在触发"的答案是「我自己」——不需要去读
                                // 心跳，也不必让一个刚起来、还没到第一拍的用户
                                // 看到"驱动未知"（ADR-0088 §3）。
                                driver: qcode_core::scheduler::DriverStatus::Running {
                                    at: qcode_core::schedule::now_local(),
                                },
                                questioner: Some(std::sync::Arc::new(task_questioner)),
                            },
                            options,
                            move |event| sink.send_event(event),
                        )
                        .await;
                        guard.armed = false;
                        let _ = done.send((
                            task_target,
                            scheduled_for_done,
                            result.map(|_| ()).map_err(|error| error.to_string()),
                        ));
                    });
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
                        // 不是驱动就不写心跳、不扫描（ADR-0091 §4）。跳过整段而不是
                        // 只跳过投递：away 判定与 config 窗口推进也属于「推动时间」，
                        // 让两个进程各推一次就是把「错过」算两遍。
                        if driver_lock.is_none() {
                            // 持锁的那个进程可能已经走了——最常见的是用户在窗口里
                            // 点「停止常驻驱动」，把 qcd 停掉。此时窗口本可以接管，
                            // 但锁只在启动时抢过一次，于是「GUI 开着」却没有任何
                            // 时钟在走：任务到点不触发，界面显示「驱动不在跑」，而
                            // 提示语还写着「GUI 开着」就行。两者矛盾。
                            //
                            // 每拍重试（30s 一次）而不是只在启动时试一次，与 `qcd`
                            // 的等待循环对称：它也是盯着锁、对方一退就接手。抢不到
                            // 就照旧不驱动——`try_lock_file` 立即返回，不会阻塞。
                            if let Ok(Some(lock)) =
                                engine.store.try_lock_file(qcode_core::scheduler::DRIVER_LOCK)
                            {
                                driver_lock = Some(lock);
                                tracing::info!(
                                    "the other driver is gone; this window takes over"
                                );
                                port.tagged(SessionTarget::Draft).send_schedule_notice(
                                    qcode_i18n::tr("gui.schedule.notice_took_over").into_owned(),
                                );
                                port.tagged(SessionTarget::Draft).send_schedule_changed();
                            }
                        }
                        if driver_lock.is_none() {
                            // 但「立即执行」是本进程自己投的（GUI 直接投递那条路，
                            // 见 `ScheduleOp::RunNow`），确认也必须由本进程完成——
                            // 否则详情页的「执行中」等待态与运行记录永远落不了地。
                            // 只确认、不扫描：这里只认本进程的登记，不会替持锁的
                            // daemon 确认它的投递。
                            let (confirmed, abandoned) = {
                                let mut host = GuiHost {
                                    engine,
                                    port: &port,
                                    host: &host,
                                    live: &live,
                                    scheduler: &scheduler,
                                    driver_instance: &driver_instance,
                                    model: &model,
                                    provider_name: &provider_name,
                                    starting: HashSet::new(),
                                };
                                schedule_driver.confirm_landed(
                                    &mut host,
                                    qcode_core::schedule::now_local(),
                                )
                            };
                            if confirmed > 0 || abandoned > 0 {
                                port.tagged(SessionTarget::Draft).send_schedule_changed();
                            }
                            continue;
                        }
                        // ==== 一拍（ADR-0092 §6）====
                        //
                        // 心跳、away 判定、扫描、投递、记账、确认、崩溃恢复全在
                        // `qcode_driver::runtime::ScheduleDriver` 里——「到期会不会投递 /
                        // 投了会不会重投 / 重启会不会重投」在那里用假时钟与真 store 验
                        // （`qcode-driver/tests/drive.rs`）。
                        //
                        // 窗口只做两件事：回答驱动问的事实（`GuiHost`），以及把它算出的
                        // 「该开的回合」真的开起来（`start_turn!` 留在这里，因为它要用
                        // 引擎的装配）。墙钟由 `GuiHost::now` 现取——驱动可能在
                        // 一拍里跨过分钟边界，那一拍用同一个 `now` 更一致。
                        let report = {
                            let mut host = GuiHost {
                                engine,
                                port: &port,
                                host: &host,
                                live: &live,
                                scheduler: &scheduler,
                                driver_instance: &driver_instance,
                                model: &model,
                                provider_name: &provider_name,
                                // 同一拍里只给一个会话开回合：投递是逐任务的，而
                                // `live` 要等 `start_turn!` 跑完才更新。
                                starting: HashSet::new(),
                            };
                            schedule_driver.on_beat(&mut host, std::time::Instant::now())
                        };
                        // 一拍里该开的回合。投递、记账、确认、崩溃恢复都已经在
                        // 驱动里做完（上面那一拍，ADR-0092 §6）；窗口只剩「把它算出的
                        // 回合真的开起来」——因为 `start_turn!` 要用引擎的装配，而那
                        // 一套不该塞进 `qcode-driver` 的 trait。
                        for start in report.starts {
                            approval_deadline = start.approval;
                            start_turn!(
                                SessionTarget::Session(start.session),
                                start.text,
                                Vec::new(),
                                None,
                                Some(start.scheduled),
                                engine.max_turns,
                                engine.max_cost_usd
                            );
                        }
                    }
                    // 「立即执行」的提前确认（`ScheduleOp::RunNow` 分支投递后发起的
                    // 500ms 延迟信号）：刚投的消息一落盘就写回执，不必等下一个 beat。
                    // 与 beat 的确认幂等——已确认的登记会被移出 `pending`。
                    _ = confirm_rx.recv() => {
                        let (confirmed, _abandoned) = {
                            let mut host = GuiHost {
                                engine,
                                port: &port,
                                host: &host,
                                live: &live,
                                scheduler: &scheduler,
                                driver_instance: &driver_instance,
                                model: &model,
                                provider_name: &provider_name,
                                starting: HashSet::new(),
                            };
                            schedule_driver.confirm_landed(
                                &mut host,
                                qcode_core::schedule::now_local(),
                            )
                        };
                        if confirmed > 0 {
                            port.tagged(SessionTarget::Draft).send_schedule_changed();
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
                                            tracing::warn!(%error, "cannot switch provider");
                                            // The turn keeps running on the old
                                            // route: this is a notice, not a
                                            // failed turn. Sending `TurnFailed`
                                            // here aborted the live turn card
                                            // and dropped its pending approvals.
                                            port.tagged(current.clone()).send_notice(
                                                qcode_i18n::trp(
                                                    "gui.runtime.switch_provider_failed",
                                                    &[
                                                        next_provider.as_str(),
                                                        error.to_string().as_str(),
                                                    ],
                                                ),
                                            );
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
                                    tracing::warn!(%error, "cannot persist default model");
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
                                let target = current.clone();
                                let running = live.contains_key(&target);
                                let queue = host
                                    .queue(&target)
                                    .unwrap_or_else(|| host.open(&target).1);
                                // 「是不是在跑」只有引擎知道；界面那边是乐观
                                // 猜测，可能是陈旧的。以前无论真假都调 `steer`，
                                // 而它在空闲时返回 None 并把文本丢掉——用户按的是
                                // 发送、输入框已经清空，那句话就无影无踪了。空闲时
                                // 退回成一条普通消息：它开自己的 turn，和 `Prompt`
                                // 的空闲分支同一条路。
                                if running {
                                    queue.steer(text, attachments, true);
                                } else {
                                    match queue.submit(text, attachments, false) {
                                        Submission::Start(message) => start_turn!(
                                            target.clone(),
                                            message.text,
                                            message.attachments
                                        ),
                                        Submission::Queued => {}
                                    }
                                }
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
                                // 手动创建的 ID 单独带走：成功后 UI 要自动跳到
                                // 新任务的详情页（见 `ScheduleCreated` 事件）。
                                let mut created_id: Option<String> = None;
                                let result: Result<Option<String>, String> = match op {
                                    ScheduleOp::Create {
                                        title,
                                        prompt,
                                        rule,
                                        target,
                                        overlap,
                                        approval_timeout,
                                        cwd,
                                    } => scheduler
                                        .create(
                                            NewTask {
                                                title,
                                                prompt,
                                                rule,
                                                target,
                                                overlap,
                                                approval_timeout,
                                                // 表单留空时把 workspace 写实（ADR-0090 §11）。
                                                // 到点回落本来就落到 `engine.workspace`
                                                // （见下面投递那段），所以这里不是改变
                                                // 行为，而是让任务上存的地址与将来实际跑的
                                                // 那一个**是同一个**：将来从别的目录启动
                                                // GUI 也不会把这条任务的落点挪走。
                                                cwd: Some(cwd.unwrap_or_else(|| {
                                                    engine.workspace.display().to_string()
                                                })),
                                            },
                                            now,
                                        )
                                        .map(|task| {
                                            created_id = Some(task.id.clone());
                                            Some(qcode_i18n::trp(
                                                "gui.schedule.op.created",
                                                &[task.title.as_str()],
                                            ))
                                        })
                                        .map_err(|error| error.to_string()),
                                    ScheduleOp::List => Ok(Some(qcode_i18n::trp(
                                        "gui.schedule.op.listed",
                                        &[scheduler.list().len().to_string()],
                                    ))),
                                    ScheduleOp::Get { id } => match scheduler.find(&id) {
                                        Some(task) => Ok(Some(format!("{task:?}"))),
                                        None => Err(qcode_i18n::tr("gui.schedule.op.not_found").to_string()),
                                    },
                                    ScheduleOp::Edit {
                                        id,
                                        title,
                                        prompt,
                                        rule,
                                        target,
                                        overlap,
                                        approval_timeout,
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
                                            },
                                            now,
                                        )
                                        .map(|_| Some(qcode_i18n::tr("ui.action.saved").to_string()))
                                        .map_err(|error| error.to_string()),
                                    ScheduleOp::Pause { id } => scheduler
                                        .pause(&id)
                                        .map(|_| Some(qcode_i18n::tr("gui.schedule.op.paused").to_string()))
                                        .map_err(|error| error.to_string()),
                                    ScheduleOp::Resume { id } => scheduler
                                        .resume(&id, now)
                                        .map(|_| Some(qcode_i18n::tr("gui.schedule.op.resumed").to_string()))
                                        .map_err(|error| error.to_string()),
                                    ScheduleOp::RunNow { id } => {
                                        // 准入闸：目标会话锁 + 最小投递间隔
                                        // （见 `run_now_admission`）。命中即拒——
                                        // 不 run_now / 不投递 / 不登记 / 不 advance，
                                        // 任务状态零变更，并回 `ScheduleRunNowRejected`
                                        // 让 shell 解除「执行中…」等待态。
                                        let rejected = scheduler
                                            .find(&id)
                                            .and_then(|task| {
                                                run_now_admission(&engine.store, &task)
                                            });
                                        if let Some(message) = rejected {
                                            port.tagged(SessionTarget::Draft)
                                                .send_schedule_run_now_rejected(id.clone());
                                            Err(message.to_string())
                                        } else {
                                        match scheduler.run_now(&id, now) {
                                            Ok(()) => {
                                                // 立即投递，不等下一个 beat：点击
                                                // 即有反应（ADR-0059 §4）。投递放
                                                // 在闭包之外：`start_turn!` 的失败
                                                // 分支用 `continue`，只能在循环上下
                                                // 文展开。
                                                if let Some(task) = scheduler.find(&id) {
                                                    // 「立即执行」是人在点，与 beat 的
                                                    // `Attended` 同一条规则：从同一个方法
                                                    // 拿生效值，别在这里另写一份。
                                                    let approval = qcode_driver::Executor::Attended
                                                        .effective_approval(task.approval_timeout);
                                                    // 与 beat 走**同一条**投递路径
                                                    // （ADR-0092 §6）：`New`/`NewOnce`
                                                    // 先建会话、`NewOnce` 建完就绑，否则
                                                    // 每次「立即执行」都会新建一个会话，
                                                    // 详情页也没有「打开会话」按钮。
                                                    let session_and_delivery = {
                                                        let mut gui_host = GuiHost {
                                                            engine,
                                                            port: &port,
                                                            host: &host,
                                                            live: &live,
                                                            scheduler: &scheduler,
                                                            driver_instance: &driver_instance,
                                                            model: &model,
                                                            provider_name: &provider_name,
                                                            starting: HashSet::new(),
                                                        };
                                                        gui_host.deliver_now(&task, approval)
                                                    };
                                                    if let Some((session, delivery)) =
                                                        session_and_delivery
                                                    {
                                                        // 登记这次直接投递：它不走扫描，但
                                                        // 确认必须与扫描投递走同一条路径
                                                        // （驱动按 `pending` 记账、以会话日志
                                                        // 为事实源）。不登记的话这条投递永远
                                                        // 不会被确认——详情页的「执行中」与
                                                        // 运行记录就落不了地。忙碌会话的消息
                                                        // 还在队列里等回合结束，同样登记：
                                                        // 落盘后由确认路径按日志认领。
                                                        schedule_driver.register_delivery(
                                                            &delivery.message_id,
                                                            &task.id,
                                                            &session,
                                                            task.next_fire_at.unwrap_or(now),
                                                        );
                                                        if let Some(start) = delivery.start {
                                                            approval_deadline = start.approval;
                                                            start_turn!(
                                                                SessionTarget::Session(
                                                                    start.session,
                                                                ),
                                                                start.text,
                                                                Vec::new(),
                                                                None,
                                                                Some(start.scheduled),
                                                                engine.max_turns,
                                                                engine.max_cost_usd
                                                            );
                                                        }
                                                    }
                                                }
                                                // 立即把下次触发拨到最近的未来时刻，详情
                                                // 页不用等落盘确认就显示正确的下次执行时间。
                                                let _ = scheduler
                                                    .update(&id, |task| qcode_driver::advance_past(task, now));
                                                // 投递后主动确认：默认 beat 30s，而刚投的
                                                // 消息在回合开始（`begin_turn`）就落盘，回执
                                                // 不该等下一个 30s 窗口。回合启动可能慢于
                                                // 500ms（provider 初始化等），所以按 500ms
                                                // 间隔多试几次、共 3s——消息一落盘就被确认。
                                                // 忙碌会话的消息仍在排队时这些确认都返回
                                                // NotYet，由 beat 兜底。
                                                let tx = confirm_tx.clone();
                                                tokio::spawn(async move {
                                                    for _ in 0..6 {
                                                        tokio::time::sleep(
                                                            Duration::from_millis(500),
                                                        )
                                                        .await;
                                                        if tx.send(()).is_err() {
                                                            break;
                                                        }
                                                    }
                                                });
                                                Ok(Some(qcode_i18n::tr("gui.schedule.op.ran").to_string()))
                                            }
                                            Err(error) => Err(error.to_string()),
                                        }
                                        }
                                    }
                                    ScheduleOp::Delete { id } => scheduler
                                        .delete(&id)
                                        .map(|_| Some(qcode_i18n::tr("gui.schedule.op.deleted").to_string()))
                                        .map_err(|error| error.to_string()),
                                };
                                match result {
                                    Ok(_) if changed => {
                                        let port = port.tagged(SessionTarget::Draft);
                                        match created_id {
                                            Some(id) => port.send_schedule_created(id),
                                            None => port.send_schedule_changed(),
                                        }
                                    }
                                    Err(message) => port
                                        .tagged(SessionTarget::Draft)
                                        .send_schedule_notice(message),
                                    _ => {}
                                }
                            }
                        }
                    }
                    Some((target, scheduled, result)) = done_rx.recv() => {
                        // The finished session is the *one that finished*: with
                        // several live, cleaning up `current` would clear the
                        // wrong task's badge (P5).
                        live.remove(&target);
                        // 定时任务的执行结果：投递时已记开始时间与执行内容，这里
                        // 补上成败。成功记 info（落盘），失败记 warn（两端可见）。
                        if let Some(scheduled) = &scheduled {
                            match &result {
                                Ok(()) => tracing::info!(
                                    task_id = %scheduled.task_id,
                                    title = %scheduled.title,
                                    session_id = ?target.id(),
                                    "scheduled task finished",
                                ),
                                Err(error) => tracing::warn!(
                                    task_id = %scheduled.task_id,
                                    title = %scheduled.title,
                                    session_id = ?target.id(),
                                    %error,
                                    "scheduled task failed",
                                ),
                            }
                        }
                        if let Err(error) = result {
                            // Log before the move: `send_turn_failed` takes ownership.
                            tracing::warn!(%error, "turn failed");
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
                            // 队列续跑的定时任务消息同样带 `scheduled` 标记，否则
                            // 其执行结果会与触发它的任务失去关联。
                            start_turn!(
                                target.clone(),
                                message.text,
                                message.attachments,
                                None,
                                message.scheduled,
                                engine.max_turns,
                                engine.max_cost_usd
                            );
                            // 刚才续跑的回合由 `start_turn!` 异步启动，落盘发生在
                            // 它的 `begin_turn`。立即安排确认：**忙碌会话上点「立即
                            // 执行」投出的消息**（`deliver` 时 `busy` → 只入队不开回合）
                            // 就在这一刻落盘，回执不该等下一个 30s beat——否则按钮
                            // 在回合结束后还要再等半分钟才变回。
                            let tx = confirm_tx.clone();
                            tokio::spawn(async move {
                                for _ in 0..6 {
                                    tokio::time::sleep(Duration::from_millis(500)).await;
                                    if tx.send(()).is_err() {
                                        break;
                                    }
                                }
                            });
                        }
                        publish_to!(target);
                    }
                }
            }
        });
    });
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use qcode_core::schedule::{Overlap, Rule, ScheduledTask, Target};

    /// GUI 对「谁在跑 / 谁还在」的两问是 `plan_tick` 唯一的外部输入。
    ///
    /// 这条测试守的是**翻译层**：决策本身在 `qcode-driver` 里用假时钟测了，
    /// 但如果 GUI 把「谁忙」译错（比如把 `New` 目标当成绑定会话），那 7 条测试全绿
    /// 而 GUI 仍然会误判。所以这一层也要有人看着。
    fn task_with(target: Target) -> ScheduledTask {
        ScheduledTask {
            id: "01T".to_string(),
            title: "t".to_string(),
            prompt: "p".to_string(),
            rule: Rule::Daily { hour: 9, minute: 0 },
            target,
            overlap: Overlap::Queue,
            approval_timeout: None,
            status: qcode_core::schedule::TaskStatus::Active,
            origin: qcode_core::schedule::Origin::User,
            stakeholders: Vec::new(),
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
    fn the_probe_reads_live_and_store_correctly() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = qcode_core::store::SessionStore::new(dir.path());

        let bound = task_with(Target::Session {
            id: "01MISSING".to_string(),
        });
        // 一个**不存在**的会话：store 说不存在，live 里当然也没有。
        let live: HashMap<SessionTarget, CancellationToken> = HashMap::new();
        let starting: HashSet<String> = HashSet::new();
        assert!(
            !target_exists(&store, &bound),
            "a session that was never created must not read as existing"
        );
        assert!(!target_busy(&live, &starting, &bound));
    }

    /// `New` / `NewOnce` 目标没有绑定会话：`exists` 必须为真（它每次自己建会话），
    /// 否则一条「每天新建会话」的任务会被永久判成 `Orphaned`。
    #[test]
    fn unbound_targets_always_exist() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = qcode_core::store::SessionStore::new(dir.path());
        let live: HashMap<SessionTarget, CancellationToken> = HashMap::new();
        let starting: HashSet<String> = HashSet::new();

        for target in [Target::New, Target::NewOnce] {
            let task = task_with(target);
            assert!(
                target_exists(&store, &task),
                "a fresh target creates its own session and must count as existing"
            );
            assert!(
                !target_busy(&live, &starting, &task),
                "a fresh target has no session to be busy"
            );
        }
    }

    /// 立即执行准入闸：目标会话被任一进程持有（`SessionWriter` 的锁在）→ 拒绝；
    /// 释放后放行；`new` 目标没有固定会话 → 恒放行（每轮投递独立新会话）。
    #[test]
    fn run_now_admission_reads_the_session_lock() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = qcode_core::store::SessionStore::new(dir.path());
        let session = store
            .create_task(&qcode_core::event::SessionStarted {
                model: "mock-1".to_string(),
                provider: "mock".to_string(),
                cwd: "/tmp".to_string(),
                version: "0".to_string(),
                parent: None,
            })
            .expect("session")
            .id;
        let task = task_with(Target::Session {
            id: session.clone(),
        });

        // 会话里正有一个回合（锁被持有，可能是 qcd 开的）：拒绝。
        let guard = store.lock(&session).expect("take the session lock");
        // 断言比对中文文案，先钉语言（I18N.md 不变量 5）。
        qcode_i18n::pin_for_tests();
        assert_eq!(
            run_now_admission(&store, &task).as_deref(),
            Some("该任务正在执行中，请等待其回合结束再执行")
        );
        drop(guard);

        // 锁释放后放行。
        assert_eq!(run_now_admission(&store, &task), None);

        // `new` 目标没有固定会话：恒放行。
        let fresh = task_with(Target::New);
        assert_eq!(run_now_admission(&store, &fresh), None);
    }

    /// 最小投递间隔护栏：刚投递过（有回执）且在 60s 护栏内 → 拒绝；护栏已过
    /// （旧回执）→ 放行。后一半守的是真实的回归：`.is_some()` 判护栏会把
    /// 「投过一次」当成永远拒绝，按钮从此点不动。
    #[test]
    fn run_now_admission_obeys_the_delivery_floor() {
        let dir = tempfile::tempdir().expect("data dir");
        let store = qcode_core::store::SessionStore::new(dir.path());
        let mut task = task_with(Target::New);
        let now = qcode_core::schedule::now_local();
        task.last_receipt = Some(qcode_core::schedule::DeliveryReceipt {
            occurrence_at: now,
            delivered_at: now,
            message_id: "m".to_string(),
            target_session_id: None,
            prompt: String::new(),
        });
        qcode_i18n::pin_for_tests();
        assert_eq!(
            run_now_admission(&store, &task).as_deref(),
            Some("投递过于频繁：两次投递至少间隔 60 秒")
        );

        // 回执是很久以前的（护栏已过）：放行。
        let stale = now - Duration::from_secs(120);
        task.last_receipt = Some(qcode_core::schedule::DeliveryReceipt {
            occurrence_at: stale,
            delivered_at: stale,
            message_id: "m".to_string(),
            target_session_id: None,
            prompt: String::new(),
        });
        assert_eq!(run_now_admission(&store, &task), None);
    }
}

/// GUI 的 [`qcode_driver::runtime::Host`] 实现：投递真的进了收件箱吗。
///
/// 单独一个模块，因为它要造一个**真的** `Engine` + `SessionHost`（不跑回合、
/// 不开窗口）：驱动那边用假 Host 验的是决策与记账，这一层验的是「窗口把驱动的话
/// 译成了对收件箱的动作」。两边分开测，合起来才是「到点的任务真的会被跑」。
#[cfg(test)]
mod host_tests {
    use super::*;
    use qcode_core::schedule::{Overlap, Rule, Target};
    use qcode_core::scheduler::NewTask;
    use std::sync::Arc;

    /// 一个能答话的引擎：真的 store、真的 policy，provider 是 mock（不跑）。
    ///
    /// 与 `tests/host.rs` 里的那份同一个形状：`Engine` 的每个字段都是「装配好的
    /// 一次性输入」，所以窗口之外想拿到一个 Engine 只能这么造。
    fn engine(dir: &std::path::Path) -> Engine {
        Engine {
            store: SessionStore::new(dir),
            provider: Arc::new(qcode_providers::MockProvider::text("hello")),
            provider_name: "mock".to_string(),
            sandbox: "none".to_string(),
            proxy_url: None,
            tools: qcode_core::tool::ToolRegistry::new(Vec::new()),
            policy: qcode_core::policy::Policy::new(Vec::new()),
            workspace: dir.to_path_buf(),
            config_path: dir.join("config.toml"),
            model: "mock-1".to_string(),
            pricing: None,
            pricing_table: std::collections::BTreeMap::new(),
            max_turns: 4,
            max_cost_usd: None,
            context_window: Some(128_000),
            network: None,
            snapshots: None,
            skills_prompt: None,
            agents_md: None,
            shell_env: std::sync::Arc::new(qcode_tools::shell_env::ShellEnvCache::disabled()),
            schedules: Vec::new(),
            schedule_declared: Vec::new(),
            schedule_errors: Vec::new(),
            // 未开启：这些测试关心的是投递路径，不是标题生成。
            session_title: None,
            schedule_tick: None,
        }
    }

    fn due_task(
        scheduler: &Scheduler,
        target: Target,
        at: time::PrimitiveDateTime,
    ) -> ScheduledTask {
        let task = scheduler
            .create(
                NewTask {
                    title: "morning".to_string(),
                    prompt: "review the commits".to_string(),
                    rule: Rule::Daily { hour: 9, minute: 0 },
                    target,
                    overlap: Overlap::Queue,
                    approval_timeout: None,
                    cwd: None,
                },
                at - Duration::from_secs(3600),
            )
            .expect("create task");
        scheduler
            .update(&task.id, |task| task.next_fire_at = Some(at))
            .expect("aim at the due moment");
        scheduler.find(&task.id).expect("task")
    }

    /// 到点的任务必须真的进收件箱，并给出「该开的回合」。
    ///
    /// 这是 GUI 侧最容易悄悄坏掉的一环：`deliver` 少写一行 `followup_scheduled`，
    /// 驱动那边 9 条测试全绿（它只验「我调了 deliver」），而用户什么也收不到。
    #[test]
    fn a_due_task_lands_in_the_session_inbox() {
        let data = tempfile::tempdir().expect("data dir");
        // `channels()` 返回 (UI 端, 内核端)；窗口持有内核端，UI 端要给一个
        // 活得够久的变量，否则发送会失败。
        let (_ui, port) = qcode_ui_bridge::channels();
        let host = SessionHost::new(engine(data.path()));
        let deps = host.deps().clone();
        let engine = &deps.engine;
        let scheduler = Scheduler::new(engine.store.clone());
        let session = engine
            .store
            .create_task(&qcode_core::event::SessionStarted {
                model: "mock-1".to_string(),
                provider: "mock".to_string(),
                cwd: "/tmp".to_string(),
                version: "0".to_string(),
                parent: None,
            })
            .expect("session")
            .id;
        let now = qcode_core::schedule::now_local();
        let task = due_task(
            &scheduler,
            Target::Session {
                id: session.clone(),
            },
            now - Duration::from_secs(1),
        );

        let live: HashMap<SessionTarget, CancellationToken> = HashMap::new();
        let mut gui_host = GuiHost {
            engine,
            port: &port,
            host: &host,
            live: &live,
            scheduler: &scheduler,
            driver_instance: "test",
            model: "mock-1",
            provider_name: "mock",
            starting: HashSet::new(),
        };
        let delivery = gui_host.deliver(&session, &task, None).expect("deliver");
        assert!(!delivery.message_id.is_empty(), "驱动要拿这个 id 去查落盘");

        // 会话空闲 ⇒ 消息被立刻认领、并给出「该开的回合」：这就是
        // `followup_scheduled` + `finish()` 的效果——它把消息从队列里取走，
        // 交给回合去落盘。少写这一行 `followup_scheduled`，驱动那边 9 条测试
        // 仍然全绿（它只验「我调了 deliver」），而用户什么也收不到。
        let start = delivery.start.expect("空闲会话必须开回合");
        assert_eq!(start.session, session);
        assert_eq!(
            start.scheduled.task_id, task.id,
            "回合要能关联回触发它的任务，否则执行结果无法记回任务"
        );
        assert_eq!(start.scheduled.prompt, "review the commits");
        assert_eq!(start.approval, None, "有人值守 ⇒ 一直等人工");

        let target = SessionTarget::Session(session.clone());
        let queue = host.queue(&target).expect("session slot");
        assert!(
            queue.inbox().queued().is_empty(),
            "已被回合认领的消息不该还留在队列里"
        );

        // 落盘之前：`landed` 必须说「还没」。若这一层把错误读成「还没落盘」，
        // 驱动会一直等；若把「还没」读成「已落盘」，驱动会提前推进而丢一次投递。
        assert_eq!(
            gui_host.landed(&session, &task.id, &start.scheduled.at),
            qcode_driver::runtime::Landed::NotYet
        );

        // 回合真的把那一条写进日志之后，`landed` 才说「已落盘」。
        let seq = engine.store.load(&session).expect("session log").len() as u64 + 1;
        engine
            .store
            .append(&qcode_core::event::Event::new(
                seq,
                &session,
                1,
                qcode_core::event::EventKind::UserMessage(qcode_core::event::UserMessage {
                    starts_segment: false,
                    queued_id: None,
                    text: start.text.clone(),
                    attachments: Vec::new(),
                    continuation: None,
                    question_reply: None,
                    scheduled: Some(start.scheduled.clone()),
                }),
            ))
            .expect("the turn writes the message");
        assert_eq!(
            gui_host.landed(&session, &task.id, &start.scheduled.at),
            qcode_driver::runtime::Landed::Yes,
            "日志里有了那条 user/message 才算确认"
        );
    }

    /// 忙碌会话的投递只排队、不开回合。
    #[test]
    fn a_busy_session_queues_without_starting_a_turn() {
        let data = tempfile::tempdir().expect("data dir");
        // `channels()` 返回 (UI 端, 内核端)；窗口持有内核端，UI 端要给一个
        // 活得够久的变量，否则发送会失败。
        let (_ui, port) = qcode_ui_bridge::channels();
        let host = SessionHost::new(engine(data.path()));
        let deps = host.deps().clone();
        let engine = &deps.engine;
        let scheduler = Scheduler::new(engine.store.clone());
        let session = engine
            .store
            .create_task(&qcode_core::event::SessionStarted {
                model: "mock-1".to_string(),
                provider: "mock".to_string(),
                cwd: "/tmp".to_string(),
                version: "0".to_string(),
                parent: None,
            })
            .expect("session")
            .id;
        let now = qcode_core::schedule::now_local();
        let task = due_task(
            &scheduler,
            Target::Session {
                id: session.clone(),
            },
            now - Duration::from_secs(1),
        );

        let mut live: HashMap<SessionTarget, CancellationToken> = HashMap::new();
        let target = SessionTarget::Session(session.clone());
        live.insert(target.clone(), CancellationToken::new());
        let mut gui_host = GuiHost {
            engine,
            port: &port,
            host: &host,
            live: &live,
            scheduler: &scheduler,
            driver_instance: "test",
            model: "mock-1",
            provider_name: "mock",
            starting: HashSet::new(),
        };
        let delivery = gui_host.deliver(&session, &task, None).expect("deliver");
        assert!(delivery.start.is_none(), "忙的话只排队，不许并发开回合");

        let queue = host.queue(&target).expect("session slot");
        assert_eq!(
            queue.inbox().queued().len(),
            1,
            "消息仍在队列里等这一回合结束"
        );
    }
}
