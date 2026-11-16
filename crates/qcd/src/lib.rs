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
        let workspace =
            std::env::current_dir().unwrap_or_else(|_| assembled.store.root().to_path_buf());
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
        }
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
                tracing::info!(
                    session = %start.session,
                    turns = outcome.turns,
                    "scheduled turn finished"
                );
            }
            // 回合失败不是 daemon 的失败：会话日志里有记录，下一拍照常。
            // 打日志是为了 `qc logs` 能看到，不是为了让进程退出。
            Err(error) => {
                tracing::warn!(session = %start.session, %error, "scheduled turn failed");
            }
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
