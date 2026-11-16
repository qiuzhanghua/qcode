//! 定时任务管理服务：**一套模型**（任务表 `tasks.json`），config 只是来源之一。
//!
//! 只负责"任务定义"的管理与校验：创建 / 编辑 / 启停 / 立即执行 / 删除 / 列表 /
//! 查看 / 与 `[schedules]` 对齐。投递本身由 `qcode_driver` 负责，这里不读时钟——
//! `now` 由调用方传入，方便用假时钟测试。
//!
//! `[schedules.<name>]` 是**另一个入口，不是另一套机制**：它经
//! [`Scheduler::reconcile_config`] 变成任务，此后与用户建的任务走同一条投递路径。

use std::time::Duration;

use time::PrimitiveDateTime;

use crate::schedule::{
    Origin, Rule, Schedule, ScheduledTask, Target, TaskStatus, UNATTENDED_APPROVAL_TIMEOUT,
};
use crate::store::{SessionStore, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("schedule task: {0}")]
    Invalid(String),
    #[error("schedule task `{0}` not found")]
    NotFound(String),
    /// 这条任务由 `config.toml` 声明，任务表只是它的投影：改它要去改配置。
    #[error(
        "schedule task `{0}` is declared in config.toml ([schedules.{0}]); edit that file instead"
    )]
    ConfigOwned(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Rule(#[from] crate::schedule::RuleError),
}

/// 「谁在推动时间」的答案（ADR-0088 §3，四态见 ADR-0091）。
///
/// 任务定义是纯数据，任何前端都能登记；但**让任务真的到点触发**需要有一个持有
/// 调度时钟的进程（ADR-0059 §1：一个时钟，不是 N 个）。
///
/// 判据分两层，缺一层就会说谎（ADR-0091 §2）：
///
/// - **锁**回答「有没有」：`locks/driver.lock` 由内核在进程死亡时收走，所以
///   持锁就是活着的证明，不靠心跳、不靠 pid 文件。
/// - **心跳**回答「多久没动了」：它只说明醒没醒着，不说明在不在。
///
/// 早先只有心跳一个判据，于是「进程被挂起」和「进程不存在」被合并成同一句话，
/// 而这两件事的后果相反：前者醒来会继续，后者永远不会。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverStatus {
    /// 锁在手，心跳新鲜：在跑，而且醒着。
    Running { at: PrimitiveDateTime },
    /// 锁在手，但心跳老旧或者还没有：在跑，可能被挂起或者卡住。
    ///
    /// 合盖的笔记本正是这个形状——**它仍然是在跑**，醒来会继续，所以不能报
    /// 「不补跑」。
    Held { at: Option<PrimitiveDateTime> },
    /// 没有锁，但有心跳：来过又走了（进程死了，锁被内核收走）。
    Gone { at: PrimitiveDateTime },
    /// 没有锁，也没有心跳：这个数据目录没跑过会驱动的前端。
    Unknown,
}

/// 心跳多久不更新就算「没醒着」。
///
/// 与调度器判断「进程不在」用的 `gap_limit`（三个 beat）是**同一个口径**：
/// 两处若各自定阈值，会出现「驱动认为自己在跑、任务认为已经错过」的错位。
/// 默认 beat 是 30s，所以 90s 是默认值（`qcode-gui::runtime` 的 `beat`）。
pub const DRIVER_HEARTBEAT_BUDGET: time::Duration = time::Duration::seconds(90);

/// 一次探锁的结果。
enum Probe {
    /// 试拿成功并立刻放开：没有人在推时钟。
    Free,
    /// 有人拿着。
    Held,
    /// 这台机器/这个存储上没有可用的文件锁（或者锁文件建不出来）。
    Unavailable,
}

/// 试拿一次驱动锁，**立刻放开**（ADR-0091 §4）。
///
/// 观察者（`qc schedule list`、TUI）只能这样问「有人在吗」。必须立刻放开：
/// 一次只读的查询不该把真正的驱动挡在门外。
///
/// `locks/` 不存在就直接判 `Free`，不建目录——问一句「有没有人在跑」不该在磁盘上
/// 留下痕迹（ADR-0090 §8 的同一条理由）。
fn probe_driver_lock(store: &SessionStore) -> Probe {
    if !store.root().join("locks").exists() {
        return Probe::Free;
    }
    match store.try_lock_file(DRIVER_LOCK) {
        Ok(Some(lock)) => {
            // 立刻放开。
            drop(lock);
            Probe::Free
        }
        // 同一个进程再 open 一次同一个文件，flock 仍会互斥（按打开的文件描述算），
        // 所以驱动自己不该走这条路——它用 `held_by_me`。
        Ok(None) => Probe::Held,
        Err(_) => Probe::Unavailable,
    }
}

impl DriverStatus {
    /// 观察者的判定：探锁 + 读心跳。
    pub fn read(store: &SessionStore, now: PrimitiveDateTime) -> Self {
        let heartbeat = heartbeat_of(store);
        match probe_driver_lock(store) {
            Probe::Held => match heartbeat {
                Some(at) if now - at <= DRIVER_HEARTBEAT_BUDGET => Self::Running { at },
                other => Self::Held { at: other },
            },
            Probe::Free => match heartbeat {
                Some(at) => Self::Gone { at },
                None => Self::Unknown,
            },
            // 没有可用的锁就只剩心跳一个判据——那是这套机制之前的行为。宁可
            // 退回老答案，也不要假装「探测过」。
            Probe::Unavailable => match heartbeat {
                Some(at) if now - at <= DRIVER_HEARTBEAT_BUDGET => Self::Running { at },
                Some(at) => Self::Gone { at },
                None => Self::Unknown,
            },
        }
    }

    /// **驱动自己**的判定：它知道锁在手，因此不能去探（同进程重入也会互斥）。
    pub fn held_by_me(store: &SessionStore, now: PrimitiveDateTime) -> Self {
        match heartbeat_of(store) {
            Some(at) if now - at <= DRIVER_HEARTBEAT_BUDGET => Self::Running { at },
            other => Self::Held { at: other },
        }
    }

    /// 是否有一个进程在负责推动时间（在跑，哪怕它暂时睡着）。
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running { .. } | Self::Held { .. })
    }

    /// 一句话说清「能不能触发」，供工具结果与界面共用。
    pub fn describe(self) -> &'static str {
        match self {
            Self::Running { .. } => "驱动在跑（定时任务会正常触发）",
            Self::Held { .. } => "驱动在跑，但心跳已过期（可能被挂起或者卡住；醒来会继续）",
            Self::Gone { .. } => "驱动不在跑（上次退出了；到点不会触发，记为错过）",
            Self::Unknown => "未知（这个数据目录还没有会驱动定时任务的前端跑过）",
        }
    }
}

fn heartbeat_of(store: &SessionStore) -> Option<PrimitiveDateTime> {
    // 没有索引文件就是从来没跑过任何前端，也就从来没有心跳——**而且不去建它**：
    // 打开索引的副作用是 `create_dir_all` 加建全部表，问一句「有没有人在跑」不该
    // 在磁盘上留下痕迹（ADR-0090 §8 的同一条理由）。
    //
    // 这道门只管心跳，不管锁：锁文件在 `locks/` 里，跟索引无关。所以「有个进程
    // 持着锁但这个数据目录还没有索引」是能判定成 Held 的，不该被这条门吃掉。
    if !store.root().join("index.sqlite").exists() {
        return None;
    }
    // 读不出来与从没写过，对调用方是同一件事：没有人能证明有人醒着。
    store
        .runtime_status(DRIVER_HEARTBEAT_KEY)
        .ok()
        .flatten()
        .map(|(_, at)| at)
}

/// 心跳在 `runtime_status` 表里的键。
pub const DRIVER_HEARTBEAT_KEY: &str = "driver.heartbeat";

/// 驱动锁的名字（`locks/driver.lock`，ADR-0091）。
///
/// 与任务表的 `schedule-tasks` 是**两把锁，两个问题**：这把回答「谁是时钟」并且
/// 持有整个进程生命周期；那把回答「谁在改任务表」并且只持有毫秒级。
pub const DRIVER_LOCK: &str = "driver";

/// 驱动状态跟随结果的说明（ADR-0088 §2，四态见 ADR-0091 §2）。
///
/// 「不在跑」那两条分支（`Gone` / `Unknown`）的措辞刻意写死三件事，缺一件这条提示就会
/// 误导人：
/// 1. 谁在触发（持有调度时钟的进程，当前只有 GUI）；
/// 2. 关窗不等于停下（托盘常驻，**退出进程**才停）；
/// 3. 不在跑的时候不是「稍后补跑」，而是「记为错过」。
///
/// 住在这里而不是工具 crate 里（ADR-0090 §0.2）：CLI 也要说同一件事，而它是
/// `qcode-core` 的又一个消费者。三处各写一份的后果不是「措辞不一致」这么轻——这三条
/// 是用户判断「我现在到底会不会收到提醒」的全部依据，任缺一条都会把人引到错的行为上。
///
/// `Held` 单独一支，而且**不**照抄「不补跑」那句：锁在手意味着进程还在，挂起之后醒来
/// 会继续。把「被挂起」说成「不在跑」，就是劝人去重建一条本来没事的任务。
pub fn driver_footer(driver: DriverStatus) -> String {
    match driver {
        DriverStatus::Running { .. } => "驱动：在跑（到点会正常投递）。".to_string(),
        DriverStatus::Held { at } => {
            let when = at.map_or_else(|| "还没有心跳".to_string(), |at| format!("上次心跳 {at}"));
            format!(
                "**驱动在跑，但心跳已过期**（{when}）：持有调度时钟的进程还在（锁在它手上），\
                 任务没有丢，但它可能被挂起或者卡住了。唤醒后会继续按规则触发——\
                 这段里错过的窗口仍然记为错过，不补跑。"
            )
        }
        DriverStatus::Gone { at } => format!(
            "**驱动不在跑**（上次心跳 {at}，进程已退出）：该任务需要有一个持有调度时钟的\
             进程在运行时才会触发（当前只有 GUI 提供）。GUI 关窗只是隐藏到托盘、进程继续跑，\
             **退出进程才停**。它没在跑的时候，到点不会发生，并记为错过——不补跑。"
        ),
        DriverStatus::Unknown => {
            "**驱动未知**：这个数据目录还没有会驱动定时任务的前端跑过。该任务需要有一个持有调度时钟的\
             进程在运行时才会触发（当前只有 GUI 提供）。GUI 关窗只是隐藏到托盘、进程继续跑，\
             **退出进程才停**。它没在跑的时候，到点不会发生，并记为错过——不补跑。"
                .to_string()
        }
    }
}

/// 审批截止时间这一栏该怎么说（ADR-0092 §7）。
///
/// 存在两张皮的原因：`approval_timeout = None` 在**有人值守**时是「一直等人工」，
/// 在**无人值守**（daemon）时会被补成 [`UNATTENDED_APPROVAL_TIMEOUT`] 并自动
/// 拒绝。同一份任务表、两种执行者、两种结果——所以文案必须把它们一起说出来，
/// 而不是只在设了值的时候才提一句。
///
/// 与 [`driver_footer`] 放在一起、被 CLI 和 `schedule_*` 工具共用：ADR-0090 §5
/// 那条「建了 ≠ 会跑」的诚实，靠的就是「用户和模型看到的是同一句话」。
pub fn approval_footer(timeout: Option<Duration>) -> String {
    match timeout {
        Some(timeout) => format!("审批：{}(超时自动拒绝)。", describe_duration(timeout)),
        None => format!(
            "审批：**未设截止时间**——有人值守时一直等人工。\
             无人值守执行时（常驻 daemon）会补 {}，超时自动拒绝：\
             没有人在旁边，等下去会把那个会话的写锁一直握着。\
             要改就加 `--approval-timeout-secs`。",
            describe_duration(UNATTENDED_APPROVAL_TIMEOUT)
        ),
    }
}

/// `300s` → `5m`、`90s` → `1m30s`。只为人读，不参与任何判定。
fn describe_duration(timeout: Duration) -> String {
    let seconds = timeout.as_secs();
    if seconds == 0 {
        return "0s".to_string();
    }
    if seconds.is_multiple_of(3600) {
        return format!("{}h", seconds / 3600);
    }
    if seconds.is_multiple_of(60) {
        return format!("{}m", seconds / 60);
    }
    let (minutes, rest) = (seconds / 60, seconds % 60);
    if minutes == 0 {
        format!("{rest}s")
    } else {
        format!("{minutes}m{rest}s")
    }
}

/// 新建任务的输入（`rule` 已解析为冻结规则，见 `schedule::parse_rule`）。
#[derive(Debug, Clone)]
pub struct NewTask {
    pub title: String,
    pub prompt: String,
    pub rule: Rule,
    pub target: Target,
    pub overlap: crate::schedule::Overlap,
    pub approval_timeout: Option<Duration>,
    /// 投递回合的工作目录（绝对路径）；`None` = 用会话 / 引擎默认。
    pub cwd: Option<String>,
}

/// 编辑任务的增量补丁（`None` = 不修改该字段）。
#[derive(Debug, Clone, Default)]
pub struct EditTask {
    pub title: Option<String>,
    pub prompt: Option<String>,
    pub rule: Option<Rule>,
    pub target: Option<Target>,
    pub overlap: Option<crate::schedule::Overlap>,
    /// `Some(None)` = 清除审批超时。
    pub approval_timeout: Option<Option<Duration>>,
}

/// 定时任务管理服务。多个调用面（模型工具 / GUI）共用同一个实例。
#[derive(Debug, Clone)]
pub struct Scheduler {
    store: SessionStore,
}

/// 任务表写锁的名字（文件 `<root>/locks/schedule-tasks.lock`）。
///
/// 任务表是「读全表 → 改 → 写全表」，而 ADR-0088 之后 **TUI/CLI 也能写**：
/// 两个前端同时登记时，后来者若拿自己那份旧快照写回，就会把前者整表覆盖。
/// 锁只在读改写这一段持有（毫秒级），不包住任何会等人的操作。
const SCHEDULE_TASKS_LOCK: &str = "schedule-tasks";

impl Scheduler {
    pub fn new(store: SessionStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    pub fn list(&self) -> Vec<ScheduledTask> {
        self.store.load_schedule_tasks()
    }

    pub fn find(&self, id: &str) -> Option<ScheduledTask> {
        self.store
            .load_schedule_tasks()
            .into_iter()
            .find(|task| task.id == id)
    }

    /// 校验并创建任务：先落盘再返回，驱动侧只负责投递。
    pub fn create(
        &self,
        input: NewTask,
        now: PrimitiveDateTime,
    ) -> Result<ScheduledTask, ScheduleError> {
        let title = input.title.trim();
        if title.is_empty() {
            return Err(ScheduleError::Invalid(
                "title must not be empty".to_string(),
            ));
        }
        if title.chars().count() > 120 {
            return Err(ScheduleError::Invalid(
                "title must be at most 120 characters".to_string(),
            ));
        }
        if input.prompt.trim().is_empty() {
            return Err(ScheduleError::Invalid(
                "prompt must not be empty".to_string(),
            ));
        }
        if let Rule::Once { at } = &input.rule
            && *at <= now
        {
            return Err(ScheduleError::Invalid(
                "a one-shot task must be scheduled in the future".to_string(),
            ));
        }
        if let Target::Session { id } = &input.target
            && self.store.task(id).is_err()
        {
            return Err(ScheduleError::Invalid(format!(
                "target session `{id}` does not exist"
            )));
        }
        let id = ulid::Ulid::new().to_string();
        let next_fire_at = input.rule.next_after(now);
        let stakeholders = input
            .target
            .session_id()
            .map(str::to_owned)
            .into_iter()
            .collect();
        let task = ScheduledTask {
            id,
            title: input.title.clone(),
            prompt: input.prompt,
            rule: input.rule,
            target: input.target,
            overlap: input.overlap,
            approval_timeout: input.approval_timeout,
            status: TaskStatus::Active,
            // 用户（或模型工具、CLI）建的：config.toml 无权改动它。
            origin: Origin::User,
            stakeholders,
            last_receipt: None,
            delivery_log: Vec::new(),
            earlier_records_pruned: false,
            missed_windows: 0,
            reserved_session_id: None,
            next_fire_at,
            cwd: input.cwd,
        };
        // 读-改-写全程持锁（ADR-0088 §5）：两个前端同时登记时，后来者若拿
        // 自己那份旧快照写回，会把前者整表覆盖。
        let _tasks_guard = self.store.lock_file_blocking(SCHEDULE_TASKS_LOCK)?;
        let mut tasks = self.store.load_schedule_tasks();
        tasks.push(task.clone());
        self.store.save_schedule_tasks(&tasks)?;
        Ok(task)
    }
    /// 编辑任务。改规则时从 `now` 重算下一次触发点。
    pub fn edit(
        &self,
        id: &str,
        patch: EditTask,
        now: PrimitiveDateTime,
    ) -> Result<ScheduledTask, ScheduleError> {
        // 读-改-写全程持锁（ADR-0088 §5）：两个前端同时登记时，后来者若拿
        // 自己那份旧快照写回，会把前者整表覆盖。
        let _tasks_guard = self.store.lock_file_blocking(SCHEDULE_TASKS_LOCK)?;
        let mut tasks = self.store.load_schedule_tasks();
        let updated = {
            let task = tasks
                .iter_mut()
                .find(|task| task.id == id)
                .ok_or_else(|| ScheduleError::NotFound(id.to_string()))?;
            reject_config_owned(task)?;
            if let Some(title) = patch.title {
                let trimmed = title.trim();
                if trimmed.is_empty() {
                    return Err(ScheduleError::Invalid(
                        "title must not be empty".to_string(),
                    ));
                }
                if trimmed.chars().count() > 120 {
                    return Err(ScheduleError::Invalid(
                        "title must be at most 120 characters".to_string(),
                    ));
                }
                task.title = title;
            }
            if let Some(prompt) = patch.prompt {
                if prompt.trim().is_empty() {
                    return Err(ScheduleError::Invalid(
                        "prompt must not be empty".to_string(),
                    ));
                }
                task.prompt = prompt;
            }
            if let Some(rule) = patch.rule {
                if let Rule::Once { at } = &rule
                    && *at <= now
                {
                    return Err(ScheduleError::Invalid(
                        "a one-shot task must be scheduled in the future".to_string(),
                    ));
                }
                task.rule = rule;
                if task.status == TaskStatus::Active {
                    task.next_fire_at = task.rule.next_after(now);
                }
            }
            if let Some(target) = patch.target {
                if let Target::Session { id } = &target
                    && self.store.task(id).is_err()
                {
                    return Err(ScheduleError::Invalid(format!(
                        "target session `{id}` does not exist"
                    )));
                }
                task.target = target.clone();
                task.stakeholders = target.session_id().map(str::to_owned).into_iter().collect();
            }
            if let Some(overlap) = patch.overlap {
                task.overlap = overlap;
            }
            if let Some(approval_timeout) = patch.approval_timeout {
                task.approval_timeout = approval_timeout;
            }
            task.clone()
        };
        self.store.save_schedule_tasks(&tasks)?;
        Ok(updated)
    }

    pub fn pause(&self, id: &str) -> Result<(), ScheduleError> {
        // 读-改-写全程持锁（ADR-0088 §5）：两个前端同时登记时，后来者若拿
        // 自己那份旧快照写回，会把前者整表覆盖。
        let _tasks_guard = self.store.lock_file_blocking(SCHEDULE_TASKS_LOCK)?;
        let mut tasks = self.store.load_schedule_tasks();
        let task = tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| ScheduleError::NotFound(id.to_string()))?;
        // `enabled = false` 才是停掉配置条目的办法；在这里暂停会在下次启动时
        // 被 reconcile 恢复，用户看不到任何解释。
        reject_config_owned(task)?;
        task.status = TaskStatus::Paused;
        self.store.save_schedule_tasks(&tasks)?;
        Ok(())
    }

    pub fn resume(&self, id: &str, now: PrimitiveDateTime) -> Result<(), ScheduleError> {
        // 读-改-写全程持锁（ADR-0088 §5）：两个前端同时登记时，后来者若拿
        // 自己那份旧快照写回，会把前者整表覆盖。
        let _tasks_guard = self.store.lock_file_blocking(SCHEDULE_TASKS_LOCK)?;
        let mut tasks = self.store.load_schedule_tasks();
        let task = tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| ScheduleError::NotFound(id.to_string()))?;
        reject_config_owned(task)?;
        task.status = TaskStatus::Active;
        if task.next_fire_at.is_none() {
            task.next_fire_at = task.rule.next_after(now);
        }
        self.store.save_schedule_tasks(&tasks)?;
        Ok(())
    }

    /// 立即执行一次：把下一次触发点拨到 `now`，驱动扫描即视为到期。
    pub fn run_now(&self, id: &str, now: PrimitiveDateTime) -> Result<(), ScheduleError> {
        // 读-改-写全程持锁（ADR-0088 §5）：两个前端同时登记时，后来者若拿
        // 自己那份旧快照写回，会把前者整表覆盖。
        let _tasks_guard = self.store.lock_file_blocking(SCHEDULE_TASKS_LOCK)?;
        let mut tasks = self.store.load_schedule_tasks();
        let task = tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| ScheduleError::NotFound(id.to_string()))?;
        if task.status != TaskStatus::Active {
            return Err(ScheduleError::Invalid(
                "cannot run a task that is paused or ended".to_string(),
            ));
        }
        task.next_fire_at = Some(now);
        self.store.save_schedule_tasks(&tasks)?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<(), ScheduleError> {
        // 读-改-写全程持锁（ADR-0088 §5）：两个前端同时登记时，后来者若拿
        // 自己那份旧快照写回，会把前者整表覆盖。
        let _tasks_guard = self.store.lock_file_blocking(SCHEDULE_TASKS_LOCK)?;
        let mut tasks = self.store.load_schedule_tasks();
        let Some(task) = tasks.iter().find(|task| task.id == id) else {
            return Err(ScheduleError::NotFound(id.to_string()));
        };
        // 删掉也会被下次启动的 reconcile 造回来。要让一条配置条目消失，删的是
        // `config.toml` 里那一段。
        reject_config_owned(task)?;
        tasks.retain(|task| task.id != id);
        self.store.save_schedule_tasks(&tasks)?;
        Ok(())
    }

    /// 会话没有显式标题时用给定标题命名它（`SchedulerHandle` 的实现本体）。
    ///
    /// 「没有标题」= 事件日志里没有 `TaskRenamed`。写标题只是追加一个重命名
    /// 事件；会话若不存在则返回 `NotFound`，由调用方决定要不要吞掉这个错误。
    pub fn set_session_title_if_missing(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<(), ScheduleError> {
        let events = self.store.load(session_id).map_err(ScheduleError::Store)?;
        let task = self
            .store
            .task_from_events(session_id, &events)
            .map_err(ScheduleError::Store)?;
        if task.title().is_some() {
            return Ok(());
        }
        // rename 上限 80 字符，任务名上限 120：超了就截断，别让写标题失败。
        let title: String = title.trim().chars().take(80).collect();
        if title.is_empty() {
            return Ok(());
        }
        self.store
            .rename_task(session_id, title)
            .map_err(ScheduleError::Store)?;
        Ok(())
    }

    /// 内部读-改-写：驱动投递后写回执 / 推进发生点 / 换状态等引擎侧变更。
    pub fn update(
        &self,
        id: &str,
        f: impl FnOnce(&mut ScheduledTask),
    ) -> Result<(), ScheduleError> {
        // 读-改-写全程持锁（ADR-0088 §5）：两个前端同时登记时，后来者若拿
        // 自己那份旧快照写回，会把前者整表覆盖。
        let _tasks_guard = self.store.lock_file_blocking(SCHEDULE_TASKS_LOCK)?;
        let mut tasks = self.store.load_schedule_tasks();
        let Some(task) = tasks.iter_mut().find(|task| task.id == id) else {
            return Err(ScheduleError::NotFound(id.to_string()));
        };
        f(task);
        self.store.save_schedule_tasks(&tasks)?;
        Ok(())
    }

    /// 把 config 声明的 `[schedules.<name>]` 与任务表对齐（幂等、可重复调用）。
    ///
    /// 取代原来的 `import_config`（「同名就跳过」）。跳过是不够的：用户改了 cron、
    /// 改了 prompt、把 `enabled` 设成 `false`、或者整条删掉——这些都不会传播。
    /// 更糟的是旧实现让配置路径**继续自己投递**，于是同一个条目一分钟要投两次
    /// （真机实测：一分钟一次的任务，150 秒建了 4 个会话）。
    ///
    /// 三条规则：
    ///
    /// 1. **声明了就存在**：`schedules` 里有的名字，任务表里一定有，字段按声明写。
    /// 2. **声明变了就跟着变**：rule / prompt / target / overlap / approval_timeout /
    ///    启用状态都从声明来。**投递历史、回执、错过计数、预定会话不动**——那是
    ///    运行事实，不是声明。
    /// 3. **声明没了才删**：`declared` 里没有、而任务是 [`Origin::Config`] 的，
    ///    删掉。
    ///
    /// # 为什么 `declared` 与 `schedules` 是两个参数
    ///
    /// 坏的条目（cron 写错、prompt 空）会被 [`resolve_schedules`] 挑出去进
    /// `schedule_errors`，不出现resolved 的 `schedules` 里。如果按 `schedules`
    /// 判断「还在不在」，一个 cron 拼写错误就会让任务被删掉——用户改回来时任务
    /// 和它的投递历史已经没了。所以「还在不在」按 `config.toml` 里的**键**判断
    /// （`declared`），「该是什么样」才按 resolved 的声明。
    ///
    /// [`resolve_schedules`]: crate::schedule::Rule
    pub fn reconcile_config(
        &self,
        declared: &[String],
        schedules: &[Schedule],
        now: PrimitiveDateTime,
    ) -> Result<ReconcileReport, ScheduleError> {
        // 读-改-写全程持锁（ADR-0088 §5）：两个前端同时登记时，后来者若拿
        // 自己那份旧快照写回，会把前者整表覆盖。
        let _tasks_guard = self.store.lock_file_blocking(SCHEDULE_TASKS_LOCK)?;
        let mut tasks = self.store.load_schedule_tasks();
        let mut report = ReconcileReport::default();

        for schedule in schedules {
            let desired = config_task(schedule, now);
            match tasks.iter_mut().find(|task| task.id == schedule.name) {
                Some(existing) => {
                    if apply_declaration(existing, &desired) {
                        report.updated += 1;
                    } else {
                        report.unchanged += 1;
                    }
                }
                None => {
                    tasks.push(desired);
                    report.created += 1;
                }
            }
        }

        // 声明里已经没有的、且是配置投影出来的任务：删。
        //
        // `Origin::User` 的任务即使撞名也不删——用户自己建的东西不该被一份配置
        // 文件的缺失带走。
        let before = tasks.len();
        tasks.retain(|task| {
            !(task.origin == Origin::Config && !declared.iter().any(|name| name == &task.id))
        });
        report.removed = before - tasks.len();

        if report.created + report.updated + report.removed > 0 {
            self.store.save_schedule_tasks(&tasks)?;
        }
        Ok(report)
    }
}

/// [`Scheduler::reconcile_config`] 的结果，给日志与测试用。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    pub created: usize,
    /// 声明变了、已写回的任务数。
    pub updated: usize,
    /// 声明没了、已删除的任务数。
    pub removed: usize,
    /// 声明没变的任务数（绝大多数）。
    pub unchanged: usize,
}

impl ReconcileReport {
    /// 任务表是否被动过。
    pub fn touched(&self) -> bool {
        self.created + self.updated + self.removed > 0
    }
}

/// 按一条 config 声明造出对应的任务。
fn config_task(schedule: &Schedule, now: PrimitiveDateTime) -> ScheduledTask {
    let (target, stakeholders) = match &schedule.task {
        Some(id) => (Target::Session { id: id.clone() }, vec![id.clone()]),
        None => (Target::New, Vec::new()),
    };
    ScheduledTask {
        // id = 条目名：用户与 config 用同一个名字称呼它，`qc schedule view tick`
        // 与 `[schedules.tick]` 指的是同一条。
        id: schedule.name.clone(),
        title: schedule.name.clone(),
        prompt: schedule.prompt.clone(),
        rule: Rule::Cron {
            expr: schedule.cron_text.clone(),
        },
        target,
        overlap: schedule.overlap,
        approval_timeout: schedule.approval_timeout,
        status: if schedule.enabled {
            TaskStatus::Active
        } else {
            TaskStatus::Paused
        },
        origin: Origin::Config,
        stakeholders,
        last_receipt: None,
        delivery_log: Vec::new(),
        earlier_records_pruned: false,
        missed_windows: 0,
        reserved_session_id: None,
        next_fire_at: schedule.cron.next_after(now),
        cwd: None,
    }
}

/// 把声明写进一条已存在的任务，返回是否真的改了。
///
/// **只动声明性的字段**。投递历史、回执、错过计数、预定会话、`last_status` 之类
/// 的运行事实一律保留：那些是「发生过什么」，不是「应该是什么」。把它们一起覆盖
/// 等于每次启动都把审计记录清空。
fn apply_declaration(task: &mut ScheduledTask, desired: &ScheduledTask) -> bool {
    let rule_changed = task.rule != desired.rule;
    let mut changed = false;

    if task.title != desired.title {
        task.title = desired.title.clone();
        changed = true;
    }
    if task.prompt != desired.prompt {
        task.prompt = desired.prompt.clone();
        changed = true;
    }
    if rule_changed {
        // 规则换了，下一次触发点得重算——留着旧的会按旧 cron 的时刻投递。
        task.rule = desired.rule.clone();
        task.next_fire_at = desired.next_fire_at;
        changed = true;
    }
    if task.target != desired.target {
        // 换了目标，原先的 stakeholder 里那个会话不再相关；重算而不是追加，
        // 否则「牵涉会话（只增不减）」会无限长下去。
        task.target = desired.target.clone();
        task.stakeholders = desired.stakeholders.clone();
        changed = true;
    }
    if task.overlap != desired.overlap {
        task.overlap = desired.overlap;
        changed = true;
    }
    if task.approval_timeout != desired.approval_timeout {
        task.approval_timeout = desired.approval_timeout;
        changed = true;
    }
    // `enabled = false` ⇒ Paused；重新启用 ⇒ Active。只在两端之间切换，不去碰
    // 用户手动 `Ended` 的任务（一次性规则跑完之后的状态）。
    if task.status != desired.status && task.status != TaskStatus::Ended {
        task.status = desired.status;
        changed = true;
    }
    if task.origin != Origin::Config {
        // 名字撞上了用户自己建的任务：声明接管它。记一条，因为用户的编辑从此
        // 会被 config 覆盖，那是需要解释的行为。
        tracing::warn!(
            id = %task.id,
            "a user task shares its id with a [schedules] entry; the config now owns it"
        );
        task.origin = Origin::Config;
        changed = true;
    }
    changed
}

/// config 声明的任务不许在这里改。
///
/// 拒绝而不是「允许但下次 reconcile 覆盖」：后者会让用户在 GUI 里改一个字，
/// 重启后无声消失——那种「改了没用、也没人说」是 ADR-0090 §5 要消灭的失败形态。
/// 报错至少告诉他该去哪儿改。
fn reject_config_owned(task: &ScheduledTask) -> Result<(), ScheduleError> {
    match task.origin {
        Origin::Config => Err(ScheduleError::ConfigOwned(task.id.clone())),
        Origin::User => Ok(()),
    }
}

/// 模型工具访问定时任务管理的门面。
///
/// 引擎把运行时 `Scheduler` 以 trait object 注入 `ToolContext`（见 `tool.rs`），
/// qcode-tools 只依赖这个接口，不依赖具体实现与存储。方法都是同步文件操作，
/// 与 GUI 事件循环里对 `Scheduler` 的直接调用保持一致。
pub trait SchedulerHandle: Send + Sync + std::fmt::Debug {
    fn list(&self) -> Vec<ScheduledTask>;
    fn find(&self, id: &str) -> Option<ScheduledTask>;
    fn create(
        &self,
        input: NewTask,
        now: PrimitiveDateTime,
    ) -> Result<ScheduledTask, ScheduleError>;
    fn edit(
        &self,
        id: &str,
        patch: EditTask,
        now: PrimitiveDateTime,
    ) -> Result<ScheduledTask, ScheduleError>;
    fn pause(&self, id: &str) -> Result<(), ScheduleError>;
    fn resume(&self, id: &str, now: PrimitiveDateTime) -> Result<(), ScheduleError>;
    fn run_now(&self, id: &str, now: PrimitiveDateTime) -> Result<(), ScheduleError>;
    fn delete(&self, id: &str) -> Result<(), ScheduleError>;
    /// 会话没有显式标题时用给定标题命名它。`schedule_create` 创建任务后把任务名
    /// 写回绑定会话，让「对话没有 title 时任务名就是对话名」在**所有前端**生效。
    ///
    /// 「没有标题」= 事件日志里没有 `TaskRenamed`（用户从未重命名）；写标题只是
    /// 追加一个重命名事件，会话内容不动。
    fn set_session_title_if_missing(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<(), ScheduleError>;
}

impl SchedulerHandle for Scheduler {
    fn list(&self) -> Vec<ScheduledTask> {
        self.list()
    }

    fn find(&self, id: &str) -> Option<ScheduledTask> {
        self.find(id)
    }

    fn create(
        &self,
        input: NewTask,
        now: PrimitiveDateTime,
    ) -> Result<ScheduledTask, ScheduleError> {
        self.create(input, now)
    }

    fn edit(
        &self,
        id: &str,
        patch: EditTask,
        now: PrimitiveDateTime,
    ) -> Result<ScheduledTask, ScheduleError> {
        self.edit(id, patch, now)
    }

    fn pause(&self, id: &str) -> Result<(), ScheduleError> {
        self.pause(id)
    }

    fn resume(&self, id: &str, now: PrimitiveDateTime) -> Result<(), ScheduleError> {
        self.resume(id, now)
    }

    fn run_now(&self, id: &str, now: PrimitiveDateTime) -> Result<(), ScheduleError> {
        self.run_now(id, now)
    }

    fn delete(&self, id: &str) -> Result<(), ScheduleError> {
        self.delete(id)
    }

    fn set_session_title_if_missing(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<(), ScheduleError> {
        self.set_session_title_if_missing(session_id, title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{DeliveryReceipt, Overlap, Rule, TaskStatus};
    use time::Month::June;

    /// `approval_footer` 的两条分支必须把「两种执行者」说清楚。
    ///
    /// 这条测的不是措辞好看，是 ADR-0092 §7 的「不能静默补」：`None` 在有人
    /// 值守时是「一直等」，在无人值守时会被补成 5 分钟并自动拒绝。任何一种
    /// 说法漏掉另一半，用户（和模型）都会以为自己知道结果。
    #[test]
    fn approval_footer_names_both_executors() {
        let set = approval_footer(Some(Duration::from_secs(90)));
        assert!(set.contains("1m30s"), "要给出人读的时长：{set}");
        assert!(set.contains("自动拒绝"), "设了值就只剩一种结果：{set}");
        assert!(
            !set.contains("无人值守"),
            "设了值之后无人值守与有人值守一致，不必再区分：{set}"
        );

        let unset = approval_footer(None);
        assert!(unset.contains("未设截止时间"), "{unset}");
        assert!(unset.contains("一直等人工"), "有人值守那一半：{unset}");
        assert!(unset.contains("无人值守"), "无人值守那一半：{unset}");
        assert!(
            unset.contains(&describe_duration(UNATTENDED_APPROVAL_TIMEOUT)),
            "必须写出会补上的具体值，否则用户不知道该填多少：{unset}"
        );
        assert!(
            unset.contains("--approval-timeout-secs"),
            "要给出改的办法，否则这条只是坏消息：{unset}"
        );
    }

    #[test]
    fn durations_read_like_a_human_wrote_them() {
        assert_eq!(describe_duration(Duration::from_secs(0)), "0s");
        assert_eq!(describe_duration(Duration::from_secs(45)), "45s");
        assert_eq!(describe_duration(Duration::from_secs(90)), "1m30s");
        assert_eq!(describe_duration(Duration::from_secs(300)), "5m");
        assert_eq!(describe_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(describe_duration(Duration::from_secs(5400)), "90m");
    }

    fn store() -> (tempfile::TempDir, Scheduler) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        // 预置一个 id 为 `t1` 的会话：`Target::Session` 的创建/编辑校验要求
        // 目标会话存在（与 dsh 的 validateTarget 一致）。
        store
            .append(&crate::event::Event::new(
                1,
                "t1",
                0,
                crate::event::EventKind::SessionStarted(crate::event::SessionStarted {
                    model: "mock-1".to_string(),
                    provider: "mock".to_string(),
                    cwd: "/tmp".to_string(),
                    version: "0.1.0".to_string(),
                    parent: None,
                }),
            ))
            .expect("seed session t1");
        let scheduler = Scheduler::new(store);
        (dir, scheduler)
    }

    fn now() -> PrimitiveDateTime {
        crate::schedule::at(2026, June, 1, 10, 0)
    }

    fn new_task() -> NewTask {
        NewTask {
            title: "nightly".to_string(),
            prompt: "review the night's changes".to_string(),
            rule: Rule::Cron {
                expr: "0 3 * * *".to_string(),
            },
            target: Target::Session {
                id: "t1".to_string(),
            },
            overlap: Overlap::Queue,
            approval_timeout: None,
            cwd: None,
        }
    }

    /// ADR-0088 §5：两个「前端」（这里是两个线程、两个 Scheduler 实例，
    /// 与两个进程对同一 tasks.json 的关系等价）同时登记，两条任务都要在。
    ///
    /// 没有那把锁时，两边各自读到同一份快照、各自 push 一条再写回，后写者
    /// 会用自己那份旧快照把前者覆盖掉——静默丢数据，而且只在高并发时出现。
    #[test]
    fn concurrent_creates_do_not_overwrite_each_other() {
        let (dir, scheduler) = store();
        let mut handles = Vec::new();
        for index in 0..12 {
            let scheduler = scheduler.clone();
            handles.push(std::thread::spawn(move || {
                let mut input = new_task();
                input.title = format!("task-{index}");
                scheduler.create(input, now()).expect("create")
            }));
        }
        for handle in handles {
            handle.join().expect("thread");
        }
        // 从磁盘重读，而不是问内存：覆盖是写回时的错。
        let stored = SessionStore::new(dir.path()).load_schedule_tasks();
        assert_eq!(stored.len(), 12, "a write lost a peer's task");
        let mut titles: Vec<&str> = stored.iter().map(|task| task.title.as_str()).collect();
        titles.sort_unstable();
        let mut expected: Vec<String> = (0..12).map(|index| format!("task-{index}")).collect();
        expected.sort_unstable();
        assert_eq!(
            titles,
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn create_persists_and_lists() {
        let (_dir, scheduler) = store();
        let created = scheduler.create(new_task(), now()).expect("create");
        assert_eq!(created.status, TaskStatus::Active);
        assert_eq!(
            created.next_fire_at,
            Some(crate::schedule::at(2026, June, 2, 3, 0))
        );
        assert_eq!(created.stakeholders, vec!["t1".to_string()]);
        assert_eq!(scheduler.list().len(), 1);
        assert_eq!(scheduler.find(&created.id).expect("find").id, created.id);
    }

    #[test]
    fn create_validates_title_prompt_and_future_once() {
        let (_dir, scheduler) = store();
        let mut bad = new_task();
        bad.title = "  ".to_string();
        assert!(matches!(
            scheduler.create(bad, now()),
            Err(ScheduleError::Invalid(_))
        ));
        let mut bad = new_task();
        bad.prompt = String::new();
        assert!(matches!(
            scheduler.create(bad, now()),
            Err(ScheduleError::Invalid(_))
        ));
        let mut past = new_task();
        past.rule = Rule::Once {
            at: crate::schedule::at(2026, June, 1, 8, 0),
        };
        assert!(matches!(
            scheduler.create(past, now()),
            Err(ScheduleError::Invalid(_))
        ));
    }

    #[test]
    fn edit_pause_resume_run_now_and_delete() {
        let (_dir, scheduler) = store();
        let created = scheduler.create(new_task(), now()).expect("create");

        scheduler.pause(&created.id).expect("pause");
        assert_eq!(
            scheduler.find(&created.id).expect("find").status,
            TaskStatus::Paused
        );

        scheduler.resume(&created.id, now()).expect("resume");
        assert_eq!(
            scheduler.find(&created.id).expect("find").status,
            TaskStatus::Active
        );

        scheduler.run_now(&created.id, now()).expect("run now");
        assert_eq!(
            scheduler.find(&created.id).expect("find").next_fire_at,
            Some(now())
        );

        scheduler.delete(&created.id).expect("delete");
        assert!(scheduler.find(&created.id).is_none());
        assert!(matches!(
            scheduler.delete(&created.id),
            Err(ScheduleError::NotFound(_))
        ));
    }

    #[test]
    fn edit_recomputes_the_next_fire_when_the_rule_changes() {
        let (_dir, scheduler) = store();
        let created = scheduler.create(new_task(), now()).expect("create");
        let edited = scheduler
            .edit(
                &created.id,
                EditTask {
                    rule: Some(Rule::Daily {
                        hour: 9,
                        minute: 30,
                    }),
                    ..EditTask::default()
                },
                now(),
            )
            .expect("edit");
        assert_eq!(
            edited.rule,
            Rule::Daily {
                hour: 9,
                minute: 30
            }
        );
        assert_eq!(
            edited.next_fire_at,
            Some(crate::schedule::at(2026, June, 2, 9, 30))
        );
    }

    /// 一条 `[schedules.<name>]` 声明。`overlap` / `enabled` 由测试覆盖。
    fn config_entry(name: &str, cron: &str, prompt: &str) -> Schedule {
        Schedule {
            name: name.to_string(),
            cron: crate::schedule::CronSpec::parse(cron).expect("cron"),
            cron_text: cron.to_string(),
            task: Some("t1".to_string()),
            prompt: prompt.to_string(),
            overlap: Overlap::Queue,
            approval_timeout: None,
            enabled: true,
        }
    }

    /// 声明里有的名字，任务表里一定有；重复 reconcile 不再新增。
    #[test]
    fn reconcile_creates_what_the_config_declares() {
        let (_dir, scheduler) = store();
        let entry = config_entry("nightly", "0 3 * * *", "review");
        let declared = vec!["nightly".to_string()];

        let first = scheduler
            .reconcile_config(&declared, std::slice::from_ref(&entry), now())
            .expect("reconcile");
        assert_eq!(first.created, 1);
        assert_eq!(first.updated, 0);

        let task = scheduler.find("nightly").expect("created");
        assert_eq!(task.origin, Origin::Config, "配置来源要能分辨出来");
        assert_eq!(
            task.rule,
            Rule::Cron {
                expr: "0 3 * * *".to_string()
            }
        );
        assert_eq!(
            task.target,
            Target::Session {
                id: "t1".to_string()
            }
        );

        // 第二次：什么都不变。
        let second = scheduler
            .reconcile_config(&declared, std::slice::from_ref(&entry), now())
            .expect("reconcile");
        assert_eq!(second.created, 0);
        assert_eq!(second.unchanged, 1);
        assert!(!second.touched());
    }

    /// 改了 cron / prompt / overlap / 目标，任务跟着变；**投递历史不动**。
    ///
    /// 「不动」这一半同样重要：把运行事实一起覆盖等于每次启动都清空审计记录。
    #[test]
    fn reconcile_follows_edits_and_keeps_run_history() {
        let (_dir, scheduler) = store();
        let declared = vec!["nightly".to_string()];
        let mut entry = config_entry("nightly", "0 3 * * *", "review");
        scheduler
            .reconcile_config(&declared, std::slice::from_ref(&entry), now())
            .expect("reconcile");

        // 造一点运行事实：一次投递回执 + 错过计数。
        let receipt = DeliveryReceipt {
            occurrence_at: now(),
            delivered_at: now(),
            message_id: "m1".to_string(),
            target_session_id: Some("t1".to_string()),
            prompt: "review".to_string(),
        };
        scheduler
            .update("nightly", |task| {
                task.last_receipt = Some(receipt.clone());
                task.delivery_log.push(receipt.clone());
                task.missed_windows = 3;
                task.reserved_session_id = Some("t1".to_string());
            })
            .expect("record run facts");

        // 用户改配置。
        entry.cron = crate::schedule::CronSpec::parse("30 4 * * *").expect("cron");
        entry.cron_text = "30 4 * * *".to_string();
        entry.prompt = "review harder".to_string();
        entry.overlap = Overlap::Skip;
        let report = scheduler
            .reconcile_config(&declared, std::slice::from_ref(&entry), now())
            .expect("reconcile");
        assert_eq!(report.updated, 1);

        let task = scheduler.find("nightly").expect("task");
        assert_eq!(
            task.rule,
            Rule::Cron {
                expr: "30 4 * * *".to_string()
            },
            "改 cron 必须传播，否则按旧时刻投递"
        );
        assert_eq!(task.prompt, "review harder");
        assert_eq!(task.overlap, Overlap::Skip);
        assert_eq!(
            task.next_fire_at.map(|at| at.hour()),
            Some(4),
            "规则变了，下一次触发点要按新规则重算"
        );
        // 运行事实原样保留。
        assert_eq!(task.last_receipt, Some(receipt.clone()));
        assert_eq!(task.delivery_log.len(), 1);
        assert_eq!(task.missed_windows, 3);
        assert_eq!(task.reserved_session_id.as_deref(), Some("t1"));
    }

    /// 只改 prompt（不动 cron）时，**下一次触发点不该被推走**。
    ///
    /// 这条守的是一个会让任务「晚一拍」甚至「跳过一整轮」的错：每次都按 `now`
    /// 重算 `next_fire_at`，一个刚刚快要到点的任务会被推到下一个窗口。
    #[test]
    fn a_prompt_only_edit_keeps_the_pending_window() {
        let (_dir, scheduler) = store();
        let declared = vec!["nightly".to_string()];
        let mut entry = config_entry("nightly", "0 3 * * *", "review");
        scheduler
            .reconcile_config(&declared, std::slice::from_ref(&entry), now())
            .expect("reconcile");
        let pending = scheduler.find("nightly").expect("task").next_fire_at;

        entry.prompt = "a different prompt".to_string();
        scheduler
            .reconcile_config(&declared, std::slice::from_ref(&entry), now())
            .expect("reconcile");

        let task = scheduler.find("nightly").expect("task");
        assert_eq!(task.prompt, "a different prompt");
        assert_eq!(task.next_fire_at, pending, "规则没变，窗口不许动");
    }

    /// 声明没了 → 任务删掉；但用户自己建的任务即使撞名也不动。
    #[test]
    fn reconcile_prunes_only_what_it_created() {
        let (_dir, scheduler) = store();
        let declared = vec!["nightly".to_string()];
        let entry = config_entry("nightly", "0 3 * * *", "review");
        scheduler
            .reconcile_config(&declared, std::slice::from_ref(&entry), now())
            .expect("reconcile");

        let report = scheduler
            .reconcile_config(&[], std::slice::from_ref(&entry), now())
            .expect("reconcile with nothing declared");
        assert_eq!(report.removed, 1);
        assert!(scheduler.find("nightly").is_none(), "配置删了就没了");

        // 用户建的任务：id 与一条（假的）配置条目同名，也不许被 reconcile 带走。
        let user = scheduler.create(new_task(), now()).expect("user task");
        scheduler
            .reconcile_config(&[], std::slice::from_ref(&entry), now())
            .expect("reconcile");
        assert!(
            scheduler.find(&user.id).is_some(),
            "Origin::User 的任务不该被一份配置文件的缺失带走"
        );
    }

    /// cron 写错：条目进 `schedule_errors`、不出现在 resolved 列表里，但**键还在**，
    /// 所以任务必须留着。
    ///
    /// 这条是 reconcile 最容易搞错的一处：按「resolved 列表里有没有」判存在性，
    /// 一个拼写错误就会把任务和它的投递历史一起删掉，用户改回来时已经晚了。
    #[test]
    fn a_broken_entry_is_kept_not_pruned() {
        let (_dir, scheduler) = store();
        let entry = config_entry("nightly", "0 3 * * *", "review");
        scheduler
            .reconcile_config(
                &["nightly".to_string()],
                std::slice::from_ref(&entry),
                now(),
            )
            .expect("reconcile");

        // 用户把 cron 写坏了：`declared` 里还有这个名字，但 resolved 列表是空的。
        let report = scheduler
            .reconcile_config(&["nightly".to_string()], &[], now())
            .expect("reconcile");
        assert_eq!(report.removed, 0, "键还在，只是解析失败");
        let task = scheduler.find("nightly").expect("kept");
        assert_eq!(
            task.rule,
            Rule::Cron {
                expr: "0 3 * * *".to_string()
            },
            "保留旧规则，等用户改回来"
        );
    }

    /// `enabled = false` 映射成 `Paused`，不是删除；改回 `true` 恢复。
    #[test]
    fn enabled_false_pauses_instead_of_removing() {
        let (_dir, scheduler) = store();
        let declared = vec!["nightly".to_string()];
        let mut entry = config_entry("nightly", "0 3 * * *", "review");

        entry.enabled = false;
        scheduler
            .reconcile_config(&declared, std::slice::from_ref(&entry), now())
            .expect("reconcile");
        let task = scheduler.find("nightly").expect("exists");
        assert_eq!(task.status, TaskStatus::Paused, "停用 ≠ 删除");

        entry.enabled = true;
        scheduler
            .reconcile_config(&declared, std::slice::from_ref(&entry), now())
            .expect("reconcile");
        assert_eq!(
            scheduler.find("nightly").expect("exists").status,
            TaskStatus::Active
        );
    }

    /// 配置声明的任务不许在任务表这一侧改（改了下次 reconcile 就覆盖）。
    #[test]
    fn a_config_owned_task_refuses_local_edits() {
        let (_dir, scheduler) = store();
        let entry = config_entry("nightly", "0 3 * * *", "review");
        scheduler
            .reconcile_config(
                &["nightly".to_string()],
                std::slice::from_ref(&entry),
                now(),
            )
            .expect("reconcile");

        for result in [
            scheduler.pause("nightly").map(|_| ()),
            scheduler.resume("nightly", now()).map(|_| ()),
            scheduler.delete("nightly"),
            scheduler
                .edit(
                    "nightly",
                    EditTask {
                        title: Some("renamed".to_string()),
                        ..EditTask::default()
                    },
                    now(),
                )
                .map(|_| ()),
        ] {
            assert!(
                matches!(result, Err(ScheduleError::ConfigOwned(_))),
                "配置来源的任务必须拒绝本地改动，实际 {result:?}"
            );
        }
        // 报错要说清楚去哪儿改。
        let message = ScheduleError::ConfigOwned("nightly".to_string()).to_string();
        assert!(message.contains("config.toml"), "{message}");
        assert!(message.contains("[schedules.nightly]"), "{message}");

        // 「立即执行」是运行命令、不是改声明，必须照旧可用。
        assert!(scheduler.run_now("nightly", now()).is_ok());
    }

    #[test]
    fn create_rejects_a_missing_target_session() {
        let (_dir, scheduler) = store();
        let mut bad = new_task();
        bad.target = Target::Session {
            id: "does-not-exist".to_string(),
        };
        assert!(matches!(
            scheduler.create(bad, now()),
            Err(ScheduleError::Invalid(_))
        ));
    }

    /// 会话没有显式标题（事件日志里无 `TaskRenamed`）时，任务名成为会话标题；
    /// 已有标题时不覆盖。
    #[test]
    fn set_session_title_if_missing_names_an_untitled_session() {
        let (_dir, scheduler) = store();
        scheduler
            .set_session_title_if_missing("t1", "每日日报")
            .expect("set title");
        let events = scheduler.store().load("t1").expect("load");
        let renames: Vec<&str> = events
            .iter()
            .filter_map(|event| match &event.kind {
                crate::event::EventKind::TaskRenamed(event) => Some(event.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(renames, ["每日日报"]);

        // 已有标题：第二次调用不再追加重命名事件。
        scheduler
            .set_session_title_if_missing("t1", "别的名字")
            .expect("no-op");
        let events = scheduler.store().load("t1").expect("load");
        let renames: Vec<&str> = events
            .iter()
            .filter_map(|event| match &event.kind {
                crate::event::EventKind::TaskRenamed(event) => Some(event.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(renames, ["每日日报"]);
    }

    #[test]
    fn edit_rejects_a_missing_target_session() {
        let (_dir, scheduler) = store();
        let created = scheduler.create(new_task(), now()).expect("create");
        let result = scheduler.edit(
            &created.id,
            EditTask {
                target: Some(Target::Session {
                    id: "does-not-exist".to_string(),
                }),
                ..EditTask::default()
            },
            now(),
        );
        assert!(matches!(result, Err(ScheduleError::Invalid(_))));
        // 拒绝后原任务保持不变，仍能正常查到。
        assert!(scheduler.find(&created.id).is_some());
    }
}
