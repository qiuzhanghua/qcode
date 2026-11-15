# 定时任务：登记与驱动分离（ADR-0088 实现规格）

> 状态：已实现（2026-11-15）。日期：2026-11-15。上游决策见 [DECISIONS.md](./DECISIONS.md) ADR-0088。
> 本文只写「怎么落」，不重复「为什么」；理由去 ADR 查。落点若有偏离，记录在各步的
> 「实际做法」里。

## 实现结果速览

| 步骤 | 落点 | 状态 |
|---|---|---|
| 1 过期语义 | `schedule::recover_task` + `ScheduledTask::missed_windows` + runtime 的 `away` 分支 | ✅ |
| 2 heartbeat | `runtime_status` 表 + `scheduler::DriverStatus` + GUI clock beat 写入 | ✅ |
| 3a 跨进程锁 | `SessionStore::lock_file_blocking`（阻塞，含 Windows `LockFileEx`）+ 8 处读改写 | ✅ |
| 4 拆分判据 | `ToolContext` / `TurnContext` 的 `driver` 字段 + 三端注入 + 文案 | ✅ |
| 5 剔除复核 | ACP 改为保留；子代理保留剔除但改写理由 | ✅ |
| 6 收尾 | README 对照表与四类差异、ADR 状态 Accepted | ✅ |

## 0. 两个已拍定的取舍

### 0.1 §5 走 3a（加锁），不做 sqlite 迁移

ADR-0088 §5 原本倾向「把运行时任务搬进 `index.sqlite`」，理由是「锁要自己写、跨三平台」。
**规划时核实后改判，走 3a（给 `tasks.json` 加锁）**，理由如下：

- **锁的骨架已经在仓库里**：`SessionStore::lock`（`crates/qcode-core/src/store.rs:309`）就是
  按会话的 `flock` 文件锁，`single_instance` 也已有三平台的锁先例。
- **sqlite 迁移的账比看上去大**：`scheduler.rs` 有 8 处 `load_schedule_tasks` /
  `save_schedule_tasks`，加上迁移、回退路径、现有测试全是 JSON 形状的。
- **3a 的风险面小**：只包住「读全表 → 改 → 写全表」这一小段，语义不变。

**但 3a 有两个不能照抄的地方**（照抄就是错的）：

1. **`SessionStore::lock` 的 Windows 分支是空的**（`store.rs:337-341`，`#[cfg(not(unix))]`
   返回 `SessionLock {}`，不锁）。ADR-0083 已经在 `crates/qcode-gui/src/single_instance/windows.rs`
   用 `LockFileEx` 做过一遍，3a 要照那里补上，否则 Windows 上等于没做。
2. **不能沿用 `LOCK_NB`**。现有 `lock()` 是非阻塞的，抢不到就返回 `StoreError::Locked`；
   那是给「同一会话不许两个 turn」用的，**抢不到是正确行为**。任务表写是毫秒级的
   临界区，抢不到应该**等**，不是报错。`task_ops.rs:42` 已经踩过这个坑并写了注释。

### 0.2 CLI 的 `new_once` 照旧绑定当前会话

`schedule_create` 在会话内创建时，`Target::NewOnce` 会自动转成
`Target::Session { id: ctx.session_id }`（`crates/qcode-tools/src/schedule.rs:242-247`）。
**这个行为在 `qc run` / TUI 里保持不变，不做特殊处理。**

接受它的代价：`qc run` 的会话是短命的，任务会绑到一次历史运行上，到点由 GUI 往那个会话
投递。这在语义上成立（会话在 store 里，`exists` 检查走 `engine.store.task(id)`），
记录在案即可，不额外加限制。

## 1. 步骤 1：运行时任务路径补 `away` 判定（修 bug，不改行为承诺）

**问题**：ADR-0059 §3 定了「错过不补跑」。配置路径实现了，**运行时任务路径没有**：
`runtime.rs` 里 `// ==== 运行时定时任务 ====` 那段（`:818` 起）在 `away` 分支
（`:704-737`）之外无条件执行，`task_tick` 只判 `now >= next_fire_at`，没有「太晚了」
的门槛。后果：宿主关三天再开，所有过期任务在开机后一拍内一起触发一轮。

**改法**：

- 给 `ScheduledTask` 加 `missed_windows: u32`（`#[serde(default)]`，旧 `tasks.json` 能读）。
  位置：`crates/qcode-core/src/schedule.rs:998` 的 `ScheduledTask`。
- 新增纯函数 `qcode_core::schedule::recover_task(task, now) -> TaskRecovery { missed, next_fire_at }`，
  与配置路径的 `recover()`（`:521`）同形但按 `Rule` 而非 `CronSpec` 计窗。用 `MAX_MISSED`
  （1_000）防时钟倒退成死循环。
- `runtime.rs` 运行时任务块开头：`if away { 对每条 Active 任务跑 recover_task → 落盘 → continue }`。
- `schedule_view` 的详情（`crates/qcode-tools/src/schedule.rs:74` `detail()`）在
  `missed_windows > 0` 时加一行「错过 k 次（宿主不在运行时到点不算）」。

**不做的**：`task_tick` 保持纯函数、只回答「到点了吗」。「太晚了怎么办」由调用方决定——
这是 ADR-0059 §2「判断到期用单调时间」的分工，别把墙钟策略塞进纯函数。

**验收**：停机跨周期再开，日志里是**一条错过**，不是一次投递。

## 2. 步骤 2：驱动 heartbeat

**问题**：ADR-0088 §3 要求「驱动是否在跑」可观测。不做这个，步骤 4 的提醒就是静态承诺。

**改法**：

- 新 kv 表（`store.rs` 的 `open_index`，`CREATE TABLE IF NOT EXISTS` 自动迁移）：
  ```sql
  CREATE TABLE IF NOT EXISTS runtime_status (
      key TEXT PRIMARY KEY,
      value TEXT NOT NULL,
      updated_at TEXT NOT NULL
  );
  ```
  **不塞进 `schedules` 表**：它的主键是任务名，heartbeat 是全局单例，塞进去要用保留名，
  是污染。
- `SessionStore::set_runtime_status(key, value, at)` / `runtime_status(key) -> Option<(value, at)>`。
  key = `"driver.heartbeat"`，value = 实例标识（数据目录 + 进程启动时刻足够区分）。
- GUI 的 clock beat 里写一次（`:700` 那个 `_ = clock.tick()` 分支内），**频率跟随已有 beat，
  不新增定时器**。
- 读面 `DriverStatus`（放 `qcode-core`，三端共用）：
  ```rust
  pub enum DriverStatus {
      Running { at: PrimitiveDateTime },
      Stale { at: PrimitiveDateTime },   // 超过阈值没更新
      Unknown,                            // 从没写过
  }
  ```
  阈值**复用 `gap_limit` 同一个常量来源**（3 拍），不要出现第二套魔数——否则会出现
  「驱动认为自己在跑、任务认为已经错过」的错位。

**验收**：GUI 在跑 → 说「驱动在跑」；GUI 进程退出 → 变「不在跑」；从没跑过 GUI 的数据目录
→ 「未知」。

## 3. 步骤 3a：`tasks.json` 的读-改-写加跨进程锁

**问题**：`save_schedule_tasks` 是原子 rename，但 `Scheduler::create/update/pause/...`
是「读全表 → 改 → 写全表」（`scheduler.rs:143-145`、`:274-284` 等 8 处）。允许 TUI 写之后，
两个进程同时读改写 → **后写者整表覆盖前者**，窗口毫秒级，但是静默丢数据。

**改法**：

- `store.rs` 新增阻塞版文件锁（**不改** `lock()` 的语义，它服务的是另一件事）：
  `SessionStore::lock_blocking(path) -> StoreGuard`，Unix 用 `flock(LOCK_EX)`（不带 `LOCK_NB`），
  Windows 用 `LockFileEx` 且不带 `LOCKFILE_FAIL_IMMEDIATELY`。
- Windows 分支照 `crates/qcode-gui/src/single_instance/windows.rs` 的先例抄，**不要**
  照 `store.rs` 现有的 `#[cfg(not(unix))] return SessionLock{}` 那个空实现。
- `Scheduler` 的每个读改写方法用 `_guard = self.store.lock_schedule_tasks()?` 包住，
  guard 在函数末尾 drop 时释放。**只包住「读 → 改 → 写」这一段**，别把整个方法体
  （含网络、含 `questioner.ask`）都塞进去。

**命名**：锁文件 `<root>/locks/schedule-tasks.lock`，与 `locks/<session>.lock` 同目录。

**验收**：两个进程同时 `schedule_create`，两条任务都在，没有一条被整表覆盖。

## 4. 步骤 4：拆开登记/驱动判据 + 三端注入

**问题**：`ToolContext.scheduler: Option<Arc<dyn SchedulerHandle>>` 一个字段回答两个问题，
而 `SchedulerHandle` 只有 CRUD（`scheduler.rs:322-341`）——它保证的是「能管理定义」，
工具却拿它判定「能不能注册」。

**改法**：

- `ToolContext`（`crates/qcode-core/src/tool.rs:38`）新增 `driver: DriverStatus`，
  与 `scheduler` 并列。两个字段各答一问：
  - `scheduler: Some(_)` = 能登记
  - `driver` = 能不能触发
- `crates/qcode-tools/src/schedule.rs`：`handle()` 拿不到仍返回 `UNAVAILABLE`（那是连 store
  都没有，近乎不可能），拿到就**成功**，并在结果尾部按 §2 措辞附驱动状态。
- 三端注入：

  | 文件 | 现在 | 改成 |
  |---|---|---|
  | `crates/qcode/src/commands/run.rs:285` | `scheduler: None` | `Some(Scheduler::new(store))` + 读 heartbeat |
  | `crates/qcode/src/tui.rs:451` | `scheduler: None` | 同上 |
  | `crates/qcode-gui/src/runtime.rs:597` | `Some(task_scheduler)` | 不变，但补 `driver` |
  | `crates/qcode-kernel/src/subagent.rs:156` | `None` | **保持 `None`**（见步骤 5） |

- **文案**（照 ADR-0088 §2，三处不能各写一套）：
  > 「已登记；该任务需要有一个持有调度时钟的进程在运行时才会触发（当前只有 GUI 提供）。
  > GUI 关窗只是隐藏到托盘、进程继续跑，**退出进程才停**。它没在跑的时候，到点不会发生，
  > 并记为错过——不补跑。」

- `schedule_list` 每条任务带驱动状态（`Running` / `不在跑` / `未知`）。

**测试改动**：`schedule.rs` 现有 `unavailable_without_handle` 用例要改语义——handle 在、
驱动不在 → **成功** + 正文含驱动说明。

**验收**：见 ADR-0088 的 5 条验证。

## 5. 步骤 5：`without(schedule)` 的三处复核

开放登记后，三处显式剔除都要重新表态：

| 调用点 | 现状 | 结论 |
|---|---|---|
| 子代理：`run.rs:243`、`tui.rs:359`、`runtime.rs:527` | 剔除 | **保持剔除**。理由与驱动无关：子代理不该有会话外的持久副作用 |
| `crates/qcode-kernel/src/lib.rs:126`（子代理注册表） | 剔除 | 同上 |
| ACP：`crates/qcode/src/commands/misc.rs:70` | 剔除，注释写「依赖 GUI 运行时驱动」 | **改为保留**。按 §1 的逻辑，ACP 有 store 就能登记；注释换成「能登记、驱动看 heartbeat」 |

## 6. 步骤 6：收尾

- README 三端对照表 `schedule_*` 两行改「✅ 登记 / 驱动仅 GUI」。
- README「三类差异」那节里 `schedule_*` 从「刻意不给」挪走——它现在是第四类。
- ADR-0088 状态 `Proposed` → `Accepted`，§5 按 0.1 改写。

## 7. 不做（范围边界）

- 不做 daemon（ADR-0088 §6 已否决）。
- 不修跨前端 `busy` 判不出来（ADR-0088 后果已记录，靠按会话 flock 兜底）。
- 不动 `approval_timeout` / 无人值守审批的语义。
- 不做 sqlite 迁移（见 0.1，作为独立技术债）。
