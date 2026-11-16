//! 定时任务管理服务（运行时任务，区别于 config 只读声明）。
//!
//! 只负责"任务定义"的管理与校验：创建 / 编辑 / 启停 / 立即执行 / 删除 / 列表 /
//! 查看 / 迁移。投递本身由引擎侧的 `ScheduleDriver` 负责（见 `qcode-gui::runtime`），
//! 这里不读时钟——`now` 由调用方传入，方便用假时钟测试。
//!
//! 任务定义落在 `SessionStore` 的 `schedule/tasks.json`（见 `store.rs`）。

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use time::PrimitiveDateTime;

use crate::schedule::{
    Rule, Schedule, ScheduleState, ScheduledTask, Target, TaskStatus, UNATTENDED_APPROVAL_TIMEOUT,
    import_config_schedule,
};
use crate::store::{SessionStore, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("schedule task: {0}")]
    Invalid(String),
    #[error("schedule task `{0}` not found")]
    NotFound(String),
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
        let before = tasks.len();
        tasks.retain(|task| task.id != id);
        if tasks.len() == before {
            return Err(ScheduleError::NotFound(id.to_string()));
        }
        self.store.save_schedule_tasks(&tasks)?;
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

    /// 把 config 声明的 `[schedules.<name>]` 条目一次性导入为运行时任务（幂等）：
    /// 同名的任务已存在则跳过。
    pub fn import_config(
        &self,
        schedules: &[Schedule],
        states: &BTreeMap<String, ScheduleState>,
        now: PrimitiveDateTime,
    ) -> Result<usize, ScheduleError> {
        // 读-改-写全程持锁（ADR-0088 §5）：两个前端同时登记时，后来者若拿
        // 自己那份旧快照写回，会把前者整表覆盖。
        let _tasks_guard = self.store.lock_file_blocking(SCHEDULE_TASKS_LOCK)?;
        let mut tasks = self.store.load_schedule_tasks();
        let mut existing: HashSet<String> = tasks.iter().map(|task| task.id.clone()).collect();
        let mut added = 0;
        for schedule in schedules {
            if !existing.insert(schedule.name.clone()) {
                continue;
            }
            tasks.push(import_config_schedule(
                schedule,
                states.get(&schedule.name),
                now,
            ));
            added += 1;
        }
        if added > 0 {
            self.store.save_schedule_tasks(&tasks)?;
        }
        Ok(added)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{Overlap, Rule, TaskStatus};
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

    #[test]
    fn import_config_is_idempotent_and_preserves_state() {
        let (_dir, scheduler) = store();
        let schedule = Schedule {
            name: "nightly".to_string(),
            cron: crate::schedule::CronSpec::parse("0 3 * * *").expect("cron"),
            cron_text: "0 3 * * *".to_string(),
            task: Some("t1".to_string()),
            prompt: "review".to_string(),
            overlap: Overlap::Queue,
            approval_timeout: None,
            enabled: true,
        };
        let states = BTreeMap::new();
        assert_eq!(
            scheduler
                .import_config(std::slice::from_ref(&schedule), &states, now())
                .expect("import"),
            1
        );
        assert_eq!(
            scheduler
                .import_config(std::slice::from_ref(&schedule), &states, now())
                .expect("import"),
            0
        );
        let imported = scheduler.find("nightly").expect("found");
        assert_eq!(
            imported.rule,
            Rule::Cron {
                expr: "0 3 * * *".to_string()
            }
        );
        assert_eq!(
            imported.target,
            Target::Session {
                id: "t1".to_string()
            }
        );
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
