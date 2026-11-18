//! `qcd` — 定时任务的常驻驱动（ADR-0092）。
//!
//! # 它是什么
//!
//! 一个**没有界面**的前端：装配好引擎，然后每 30 秒推一次时钟；到点的任务自己跑完。
//! 它是 ADR-0092 §1 说的「执行者」而不是「触发器」——触发器只需要往队列写一条消息，
//! 然后等某个前端去跑；那等于回到「没人开窗口就没人跑」的老问题。
//!
//! # 为什么是独立 package
//!
//! 见 `Cargo.toml`：依赖按 package 算，塞进 `crates/qcode` 当第二个 `[[bin]]` 会把
//! gpui → wgpu 链接进一个没有窗口的常驻进程，而且**不会报错**。
//!
//! # 这个模块为什么可测
//!
//! [`Daemon::tick`] 的两个时间来源都是参数：
//!
//! - `at: Instant` —— 单调时钟，判「进程刚才是不是睡过去了」
//! - [`Host::now`] —— 墙钟，判「到点没有」
//!
//! 生产时它们是真实的 `Instant::now()` 与系统时间；测试里换成一个可快进的假时钟，
//! 于是「一条 1 分钟后触发的任务到点跑完了」能在几毫秒内验完，而不是真等一分钟
//! （ADR-0092 验证 1 要求它进 CI）。

pub mod detach;
pub mod host;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::schedule::now_local;
use qcode_driver::runtime::{Report, ScheduleDriver, StartTurn};
use qcode_kernel::Assembled;
use tokio_util::sync::CancellationToken;

use host::DaemonHost;

/// 一拍默认 30 秒。
///
/// 与 GUI 同一个值，理由也一样：cron 的精度是分钟，30 秒留出漂移余量。更密的拍只
/// 会让「同一分钟内被投两次」更难排查。
pub const DEFAULT_BEAT: Duration = Duration::from_secs(30);

/// 这次启动的工作区，`fallback` 只在读不到 cwd 时用。
///
/// # 为什么不是一个 `current_dir()`
///
/// GUI 从 Finder / Dock 启动时 cwd 是 `/`（ADR-0085），而 `qcd` 是那个 GUI
/// `spawn` 出来的子进程，于是原样继承了 `/`。这不是一个「勉强的默认值」而是一个
/// **让 daemon 起不来**的值：`qcode_kernel::assemble` 把 cwd 当作沙箱的
/// primary root，而 `profile_for_roots` 对文件系统根是硬拒绝——
///
/// ```text
/// ERROR qcd: qcd stopped with an error error=sandbox setup failed:
///            refusing to sandbox the filesystem root as a root
/// ```
///
/// 结果是 `[sandbox] mode = "sandbox-exec"` 时，从 GUI 启停 daemon 全部无效：
/// 进程 spawn 成功、立刻以 exit=1 退出，而 `daemon::start` 只报 spawn 的 pid，
/// 于是界面看起来像「点了没反应」。实测这台机器上连点七次，七次都在 2ms 内死掉。
///
/// 同一份规则 GUI 早就有了（[`qcode_core::choose_workspace`]，见
/// `qc::commands::gui::gui_workspace`），daemon 是漏掉的那一处。用同一个函数而不是
/// 再写一遍判据：两处对「什么算一个合法的工作区」必须只有一个答案。
///
/// # 已知取舍：落点就是家目录
///
/// 文件系统根回落到**家目录**，与 GUI 从 Finder 启动时一致，这是选定的行为，
/// 不是顺手抄来的默认值。代价说清楚：家目录作为沙箱主根时写范围是整个家目录
/// （实测 `~/.ssh` 可写），而 `qcode_core::paths::dangerous_write_root` 明确拒绝
/// 把家目录当**额外写根**——两处判断不一致，是有意为之：交互式的 GUI 有审批兜底，
/// 而 daemon 无人值守。
///
/// 这个代价之所以可接受，是因为它几乎不会被用到：建任务时会把 workspace 写实
/// （ADR-0090 §11，`runtime.rs` 与 `qc schedule add` 都写 `cwd`），所以「任务没有
/// cwd」只剩下手工改过的历史数据。要让 daemon 真正拥有一个窄的落点，得先决定
/// 「daemon 的工作区是什么」，那是另一个问题（`~/.local/share/qcode` 是候选）。
pub fn launch_workspace(fallback: &std::path::Path) -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| fallback.to_path_buf());
    let home = qcode_kernel::config::home_dir().map(|home| home.canonicalize().unwrap_or(home));
    let (workspace, fell_back) = qcode_core::choose_workspace(cwd, home);
    if fell_back {
        tracing::warn!(
            workspace = %workspace.display(),
            "qcd: started with a filesystem root as cwd; using the home directory as the workspace"
        );
    }
    workspace
}

/// 墙钟的来源。
///
/// 抽出来是为了让 ADR-0092 的验收能进 CI：真等一分钟不是测试。生产用的是
/// [`SystemClock`]，测试用它把「一分钟后」变成「下一拍」。
///
/// 只做一个方法而不是一整套时间服务：驱动真正需要的墙钟只有这一个问题
/// （「现在几点」），一拍有多长由 [`Daemon::tick`] 的 `Instant` 参数回答。
pub trait WallClock: Send + Sync {
    fn now(&self) -> time::PrimitiveDateTime;
}

/// 生产用的墙钟。
pub struct SystemClock;

impl WallClock for SystemClock {
    fn now(&self) -> time::PrimitiveDateTime {
        now_local()
    }
}

/// 一个跑着的 daemon：引擎 + 驱动 + 它自己的「谁在跑」记账。
pub struct Daemon {
    /// 装配好的引擎。daemon 拥有它，因为它是这个进程的全部能力。
    pub assembled: Assembled,
    driver: ScheduleDriver,
    /// 本进程正在跑回合的会话。
    ///
    /// 与 GUI 的 `live` 同一个作用：一拍里如果有两个任务指向同一会话，第二个必须
    /// 看到「忙」。daemon 串行跑回合，所以这个集合只在**一拍之内**非空——但那就
    /// 足够了，因为 `skip` 语义要的正是「这一拍别插队」。
    live: HashSet<String>,
    /// 本拍已经被投递、还没开跑的会话（`deliver` 到 `start` 之间的窗口）。
    ///
    /// 没有它的话，同一拍里两条指向同一会话的任务都会看到「空闲」，于是都会拿到
    /// `start`——两条回合同时想写一个会话，单写者锁会让其中一条失败。
    starting: HashSet<String>,
    /// 谁在跑，写进心跳与审批策略（`Executor::Unattended`）。
    identity: String,
    /// 项目规矩（`AGENTS.md`）与 skills 提示，装配时算一次。
    agents_md: Option<String>,
    skills: Option<String>,
    /// 墙钟。生产是 [`SystemClock`]，测试可换。
    clock: Box<dyn WallClock>,
    /// 任务没指定 cwd 时的回落目录：进程启动时所在目录。
    workspace: std::path::PathBuf,
    /// 可跟随的事件出口（ADR-0092 §8）。daemon 用文件，GUI 用 `Noop`——
    /// 窗口里的事件走它自己那条 `SessionEvent` 端口，不需要第二个出口。
    sink: Box<dyn qcode_driver::events::Sink>,
}

impl Daemon {
    /// `beat` 是扫描间隔；生产用 [`DEFAULT_BEAT`]，测试给小值。
    pub fn new(assembled: Assembled, beat: Duration) -> Self {
        Self::with_clock(assembled, beat, Box::new(SystemClock))
    }

    /// 换一个墙钟。测试用它把「一分钟后」变成「下一拍」（ADR-0092 验证 1）。
    pub fn with_clock(assembled: Assembled, beat: Duration, clock: Box<dyn WallClock>) -> Self {
        let identity = format!("daemon:{}", now_local().to_string().replace(' ', "T"));
        // 启动目录：任务没指定 cwd 时回落到这里（ADR-0090 §2 那条规则的 daemon 侧）。
        // 取自 `assembled` 而不是再读一次 `current_dir()`：装配时的那个值已经过
        // 「文件系统根换成家目录」的规则，再读一次会得到没经过它的原值（见
        // [`launch_workspace`]）。
        let workspace = assembled.workspace.clone();
        let agents_md = qcode_core::agents::load(&workspace).ok().flatten();
        let skills = assembled.skills.prompt_section();
        Self {
            assembled,
            driver: ScheduleDriver::new(qcode_driver::runtime::Clock::new(beat)),
            live: HashSet::new(),
            starting: HashSet::new(),
            identity,
            agents_md,
            skills,
            clock,
            workspace,
            sink: Box::new(qcode_driver::events::Noop),
        }
    }

    /// 换一个事件出口。
    pub fn with_sink(mut self, sink: Box<dyn qcode_driver::events::Sink>) -> Self {
        self.sink = sink;
        self
    }

    /// 换上重新装配好的引擎（ADR-0103 §2，`SIGHUP`）。
    ///
    /// 只换 `assembled` 与由它派生的两段提示：`identity` 标识这个**进程**而不是
    /// 它的配置，`clock`/`sink` 是进程级设施，都不该因为一次配置变更而重置。
    ///
    /// 调用方必须先确认「没有回合在跑」——`tick` 之外没有别的入口会跑回合，而
    /// `live`/`starting` 只在一拍之内非空，所以主循环在拍与拍之间调用这里就是
    /// 安全的。这一点由调用点保证，不在这里猜。
    pub fn reload(&mut self, assembled: Assembled) {
        self.workspace = assembled.workspace.clone();
        self.agents_md = qcode_core::agents::load(&self.workspace).ok().flatten();
        self.skills = assembled.skills.prompt_section();
        self.assembled = assembled;
    }

    /// 有回合正在跑吗？`SIGHUP` 要等到没有才换入。
    pub fn has_live_turns(&self) -> bool {
        !self.live.is_empty() || !self.starting.is_empty()
    }

    /// 换成一个**可共用**的收集器，返回能读的那一半。
    ///
    /// 测试需要「驱动照常跑，同时我能看到它写了什么」。`Sink` 只有 `emit`（它不需要
    /// 读——跟随者是另一个方向的事），所以这里用共享句柄：驱动持有写端，测试持有读端。
    pub fn with_collected_events(
        mut self,
    ) -> (
        Self,
        std::sync::Arc<std::sync::Mutex<Vec<qcode_driver::events::Event>>>,
    ) {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        self.sink = Box::new(SharedCollector(shared.clone()));
        (self, shared)
    }

    /// 驱动身份，写进心跳。GUI 写的是 `gui:<时刻>`，两者要能分辨。
    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn beat(&self) -> Duration {
        self.driver.clock().beat()
    }

    /// 推一拍：先决策（同步、纯逻辑在 `qcode-driver`），再跑该跑的回合。
    ///
    /// 两段分开是必须的——`on_beat` 要 `&mut host`，跑回合要 `&assembled`，两者都
    /// 借 `self`。把 host 的借用限制在一个块里，后面才能用引擎。
    pub async fn tick(&mut self, at: Instant) -> Report {
        let report = {
            let mut host = DaemonHost {
                assembled: &self.assembled,
                live: &self.live,
                starting: &mut self.starting,
                identity: &self.identity,
                clock: self.clock.as_ref(),
            };
            let report = self.driver.on_beat(&mut host, at);
            // `deliver` 往 `starting` 里塞的会话，这一拍就要用上；不清的话下一拍
            // 那些会话会被永久算成「忙」，任务从此不再触发。
            std::mem::swap(&mut self.live, &mut self.starting);
            self.starting.clear();
            report
        };

        // 回调里跑，但**不**从 report 里取走：`Report.starts` 是给调用方看的
        // 「这一拍发生了什么」，清空它会让日志与测试都读不到「该开的回合有几条」。
        // `StartTurn` 是 `Clone`，这里取一份副本。
        for start in report.starts.clone() {
            self.run(start).await;
        }
        self.live.clear();
        // 一拍一条事件，**最后**写：到这里 `report` 的计数才是完整的。
        self.sink.emit(qcode_driver::events::Event::from_report(
            &report,
            self.clock.now(),
        ));
        report
    }

    /// 跑一个投递好的回合。
    ///
    /// 无人值守（`Executor::Unattended`）意味着两件事，都在这里落地：
    ///
    /// 1. **审批截止时间一定存在**——`approval` 由驱动按执行者补齐，`None` 在这里
    ///    是不该出现的；真出现了也按缺省补，因为「一直等人工」在没有人工的地方就是
    ///    死锁（ADR-0092 §7）。
    /// 2. **没有 questioner**：agent 想问问题就得到「没人能回答」。
    async fn run(&mut self, start: StartTurn) {
        let deadline = start
            .approval
            .unwrap_or(qcode_driver::UNATTENDED_APPROVAL_TIMEOUT);
        let approver = UnattendedApprover { deadline };
        let cancel = CancellationToken::new();
        let scheduled = start.scheduled.clone();
        // 任务自己记着工作目录（ADR-0090 §2：建任务时存的是绝对路径），缺省回落到
        // daemon 的工作目录。与 GUI 同一套答案——`StartTurn` 不带 cwd，因为它描述
        // 的是「该开哪个回合」，而 cwd 是任务定义的一部分。
        let cwd = qcode_core::scheduler::Scheduler::new(self.assembled.store.clone())
            .find(&scheduled.task_id)
            .and_then(|task| task.cwd)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.workspace.clone());

        // 事件必须在 `run_turn` **之前**写：`scheduled` 会被 `RunOptions` 消费掉，
        // 而「开始了」正是跟随者最需要立刻看到的一条——等回合跑完才写，
        // 「看着它跑」就没有意义了。
        let task_id = scheduled.task_id.clone();
        self.sink.emit(qcode_driver::events::Event::TurnStarted {
            at: self.clock.now(),
            session: start.session.clone(),
            task_id: task_id.clone(),
        });

        let outcome = run_turn(
            TurnContext {
                store: &self.assembled.store,
                provider: self.assembled.provider.as_ref(),
                tools: &self.assembled.tools,
                policy: &self.assembled.policy,
                approver: &approver,
                snapshots: None,
                scheduler: None,
                driver: qcode_core::scheduler::DriverStatus::held_by_me(
                    &self.assembled.store,
                    now_local(),
                ),
                questioner: None,
            },
            RunOptions {
                session_id: &start.session,
                model: &self.assembled.resolved.model,
                input: start.text,
                cwd,
                // 来自 `[agent] max_turns`，与 `qc run` / GUI 同一个来源。
                max_turns: self.assembled.config.agent.max_turns,
                cancel,
                pricing: None,
                max_cost_usd: None,
                on_delta: None,
                context_window: None,
                // `AGENTS.md` 与 skills 让无人值守的回合也守同一套项目规矩——
                // 少了它们，定时任务会做出与前台回合不一样的事。
                agents_md: self.agents_md.clone(),
                skills: self.skills.clone(),
                network: self.assembled.network.clone(),
                attachments: Vec::new(),
                inbox: None,
                continuation: None,
                scheduled: Some(scheduled),
            },
            |_event| {},
        )
        .await;

        match outcome {
            Ok(outcome) => {
                self.sink.emit(qcode_driver::events::Event::TurnFinished {
                    at: self.clock.now(),
                    session: start.session.clone(),
                    ok: true,
                });
                tracing::info!(
                    session = %start.session,
                    turns = outcome.turns,
                    "scheduled turn finished"
                );
            }
            // 回合失败不是 daemon 的失败：会话日志里有记录，下一拍照常。
            // 打日志是为了 `qc logs` 能看到，不是为了让进程退出。
            Err(error) => {
                self.sink.emit(qcode_driver::events::Event::TurnFinished {
                    at: self.clock.now(),
                    session: start.session.clone(),
                    ok: false,
                });
                tracing::warn!(session = %start.session, %error, "scheduled turn failed");
            }
        }
    }
}

/// 把事件写进共享的 `Vec`，供测试从外面读。
struct SharedCollector(std::sync::Arc<std::sync::Mutex<Vec<qcode_driver::events::Event>>>);

impl qcode_driver::events::Sink for SharedCollector {
    fn emit(&mut self, event: qcode_driver::events::Event) {
        if let Ok(mut events) = self.0.lock() {
            events.push(event);
        }
    }
}

/// 没有人工的审批者：等到截止时间，然后拒绝。
///
/// 为什么不是「立刻拒绝」：立即拒绝会让模型在一次工具调用上撞墙而不知道为什么，
/// 而等满截止时间至少让它与有人值守时的语义一致（工具没被批准过就不执行）。取消
/// （Ctrl-C / daemon 要退出）优先于等待。
struct UnattendedApprover {
    deadline: Duration,
}

#[async_trait::async_trait]
impl qcode_core::approval::Approver for UnattendedApprover {
    async fn approve(
        &self,
        request: &qcode_core::approval::ApprovalRequest,
        cancel: CancellationToken,
    ) -> qcode_core::approval::Answer {
        tracing::info!(
            tool = %request.tool,
            deadline_secs = self.deadline.as_secs(),
            "no human is watching; the approval will be denied when the deadline passes"
        );
        tokio::select! {
            biased;
            _ = cancel.cancelled() => qcode_core::approval::Answer::Deny,
            _ = tokio::time::sleep(self.deadline) => qcode_core::approval::Answer::Deny,
        }
    }
}
