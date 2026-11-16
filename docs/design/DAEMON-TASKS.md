# 常驻 daemon 任务清单（ADR-0092）

> 设计依据：[ADR-0092](./DECISIONS.md#adr-0092常驻-daemondriving-不再绑在窗口上2026-11-15)。
> 状态截至 2026-11-16。阶段 0 已完成，其余未开始。

这份清单只写**怎么落地**，不重述为什么——理由在 ADR-0092。与 ADR 的关系是：ADR 的
§11 是目标形态，这里是到达它的顺序，每一步带一个**能被证伪的出口判据**。

## 为什么把它写成任务清单

ADR-0092 §11 那张表被读成过现状（表里写「`qcode-driver`：时钟 beat、扫描、投递、
心跳、事件出口」，实际只有扫描/判到期）。一张没有进度栏的拆分表，下一次读它的人
会重新对照一遍代码。所以这里的每条都带「怎么算做完」。

## 三个已知事实（排期依据，已验证）

| 事实 | 判据 | 影响 |
|---|---|---|
| 跑一回合的引擎已经在 GPU 之外 | `qcode-core::engine::run_turn`；`qcode_kernel::assemble` 是唯一装配点 | daemon 不需要重写引擎，只调它 |
| `qc run` 已经是无头前端 | `crates/qcode/src/commands/run.rs`，397 行，自己 `assemble` 并跑一回合 | 它是 daemon 的现成模板 |
| 单实例锁原先在 `qcode-gui` | 已搬到 `qcode-core::single_instance`（阶段 0b） | `qcd` 可以复用它而不链 GPU |

`qcode-kernel` 的依赖树里 `wgpu`/`winit`/`wry`/`tao`/`glow`/`blade`/`softbuffer` 计数为
**0**，而 providers/tools/mcp/net 全在——ADR-0092 §1 的「完整引擎」与 §11 的「不链
GPU 栈」可以同时满足，这条拆分方向没有技术障碍。

---

## 阶段 0：先决条件 ✅（2026-11-16）

两个独立的改动，都不改变任何用户可见行为，但都在为后面拆雷。

### 0a 审批超时的缺省值 ✅

ADR-0092 §7 的硬约束：daemon 没有人工，一条没设截止时间的任务会让会话写锁**永远**
握着，而没有任何东西会报错。

- `qcode_core::schedule::UNATTENDED_APPROVAL_TIMEOUT`（5 分钟）——缺省值只有一个定义处。
- `qcode_driver::Executor`（`Attended` / `Unattended`）+ `effective_approval()`：缺省值
  由**共用规则**给出，不由每个执行者自己记着。
- `Action::Fire` 现在带 `approval_timeout`——**生效值**，调用方照着它设审批，不再自己
  去读 `task.approval_timeout`。GUI 投递路径已经改成消费它（阶段 2 接上 daemon 时
  不用再动投递代码）。
- `qcode_core::scheduler::approval_footer()`：CLI 与 `schedule_*` 工具共用同一句话，
  两种执行者的结果都写出来。`--json` 里加 `approval.{timeout_secs,unattended_default_secs}`。

**出口判据（已验）**：`cargo test -p qcode-driver` 的
`unattended_fills_the_missing_approval_deadline` 用假时钟断言「Attended ⇒ None、
Unattended ⇒ 5 分钟」；`a_declared_deadline_beats_the_unattended_default` 断言显式值
永远优先；`qc schedule create` 的两种输出都实机跑过。

### 0b 单实例/激活通道搬到 `qcode-core` ✅

`qcd` 不能依赖 `qcode-gui`（它引入 gpui/wgpu，83 项 GPU 相关依赖），所以这套三平台
原生代码（flock / LockFileEx / UDS / 命名管道）必须搬家，否则只能复制一份。

- `qcode-gui/src/single_instance/` → `qcode-core/src/single_instance/`；平台测试一并搬。
- `serve` 从收 `Sender<TrayCommand>` 改成收闭包 `Fn() -> bool`：这段代码不再认识任何
  界面类型。GUI 传「往托盘队列塞 `ShowWindow`」（映射留在 `qcode-gui::run::activator`）。
- 新增 `Scope`（文件名前缀 + 逃生变量名）。GUI 的 `gui.lock` / `gui.sock` /
  `gui.pipe` / `QC_GUI_SINGLE_INSTANCE` **逐字节不变**——改名会让升级期间老实例持有的
  锁对新二进制不可见，那正是两个窗口同时出现的一刻。daemon 将来用另一个 prefix。

**出口判据（已验）**：`cargo test -p qcode-core --test single_instance` 10 个跨进程用例；
`cargo tree -p qcode-core` 的 GPU 相关依赖数 = 0。

---

## 阶段 1：让「一次投递」成为可测单元（未开始，最大的一块）

把 `qcode-gui/src/runtime.rs` 里这三样从窗口生命周期里挪出来：

| 现状位置 | 内容 |
|---|---|
| `runtime.rs:476` `start_turn!` | 开一个回合（含审批 deadline、cost/model 上限） |
| `runtime.rs:693` `deliver_schedule_group!` | 投递：写收件箱、记账、空闲就开回合 |
| `runtime.rs:439`、`:442` | `inflight` / `pending_ack` 记账 |
| `runtime.rs:395`、`:398`、`:404` | 时钟 beat、`gap_limit`、「进程睡过去了」判定（`last_beat`） |

（行号对应 `runtime.rs` 1643 行的状态。）

### 做法：方案 A —— IO 层放进 `qcode-driver`，对 `Host` trait 泛型

ADR-0092 §6 说 driver 是「纯逻辑」，但投递必然有 IO。分两层：

- **决策层**（已存在，`lib.rs`）：`plan_tick` / `plan_recovery` / `advance_past` /
  `delivery_session`。纯函数、假时钟可测。
- **IO 层**（新增 `qcode-driver/src/runtime.rs`）：一个 `loop { select! }`，但会话收发、
  跑回合、事件出口全部走 `Host` trait。GUI 与 `qcd` 各实现一份 `Host`。

```rust
pub trait Host {
    /// 会话忙不忙、还在不在——`plan_tick` 的 `Probe` 从这里来。
    fn probe(&self) -> &dyn Probe;
    /// 投递一条消息到目标会话，返回消息 id。
    fn deliver(&mut self, target: &Target, message: Delivery) -> Result<String>;
    /// 开一个回合（daemon 在这里 await，GUI 在这里 spawn）。
    fn start_turn(&mut self, session: &str, deadline: Option<Duration>);
    /// 事件出口（ADR-0092 §8 留的缝）。GUI 走进程内通道，daemon 写可跟随的流。
    fn emit(&mut self, event: SchedulerEvent);
    fn now(&self) -> PrimitiveDateTime;
}

pub async fn drive<H: Host>(host: &mut H, clock: Clock, stop: Stop) -> Result<(), DriveError>;
```

选方案 A（放在 `qcode-driver` 内部）而不是新 crate 的理由：决策与 IO 在同一处，
`cargo tree` 一眼看出边界；拆成 `qcode-driver-host` 之后两个 crate 互相依赖，边界没
省心反而多一层。代价是 crate 头注释里「不做投递」那段要跟着改——它本来是按目标形态
写的。

**这一步的收益不是省代码，是「谁在跑」只在一处定义**：今天 GUI 与 daemon 之间任何
分歧都只能靠人同步，而人手同步的东西迟早会不一致（`advance_past` 抽出来之前，
`SkipBusy` 与 `Orphaned` 两条分支在 GUI 里各写了一遍一模一样的 `advance_rule`）。

### 出口判据

1. 用**内存假 Host** 在单进程里跑通「建会话 → 到期 → 投递 → 确认落盘 → 再扫」，
   不启窗口、不联网。这是今天做不到、也是 §6 存在的唯一理由。
2. `runtime.rs` 的调度段（`:912`–`:1127`，211 行非空）降到一层适配器；`create_task` /
   `bind_fresh_session` / `deliver_schedule_group!` 三个 helper 一起搬走。
3. `cargo test -p qcode-gui` 不减少覆盖：现有那些「谁在跑」的断言改成对着 `Host`
   假实现跑。

### 不要在这一步做的事

- 不碰进程形态（分离、单实例、控制通道）。进程形态相对简单，真正的难点是循环能不能
  泛型化；两件事混在一起做，会分不清失败是逻辑问题还是平台问题。
- 不做 GUI 跟随界面（ADR-0092 §8 明确只留缝）。

---

## 阶段 2：`qcd` 前台跑通（未开始）

`qcd` = kernel + driver IO 层 + 单实例锁（`Scope` 用新 prefix），**不链 GPU**。
先只做 `qcd --foreground`，不做分离。

要点：

- 复用 `qcode_kernel::assemble`——不要另建一个装配点（ADR-0036 那条「唯一装配点」）。
- 复用 `qcode_core::single_instance`，`Scope { prefix: "qcd", env_var: "QC_QCD_SINGLE_INSTANCE" }`。
- 审批一律 `Executor::Unattended`：没有人工，缺省 5 分钟到了自动拒绝。
- 日志走 `lifecycle` 的起止包围（ADR-0082）：没有终端的进程，`qc.log` 是唯一现场。

**出口判据**：ADR-0092「验证 1」——**不开任何 GUI**，`qc schedule create` 一条 1 分钟后
触发的任务，`qcd` 前台跑到点自己跑完，`qc sessions` 里查得到完整事件（含
`turn_started` / `assistant_message` / `turn_ended`）。

---

## 阶段 3：分离进程 + `qc daemon` 客户端（未开始，三平台各一段）

- 分离（`setsid` 等价物 / macOS、Linux、Windows 各一段）。Windows 只做当前用户会话里
  的分离进程，**不做服务**（ADR-0092 §12 非目标）。
- `qc daemon start|stop|status`：只做**拉起 + 问话**，不自己驱动。
- **`qcd` 没装时要明确失败**：PATH 里没有 `qcd` 时退出码非 0、stderr 说清楚，不假装成功。

**出口判据**：造一个 `qcd` 不在 PATH 的场景，集成测试断言退出码与 stderr 文本。这条
是 ADR-0092 §11 单独强调的诚实细节，目前**完全无从验证**——连 `qc daemon start` 都还
没有，用户敲下去拿到的是 clap 的 unknown subcommand。

---

## 阶段 4：GUI 改消费方 + daemon 让路（未开始）

- GUI 删掉自己那份驱动循环，改用 `qcode-driver` 的 IO 层，实现自己的 `Host`。
- 单写者锁拿不到时 daemon **不抢**、记一次错过、继续（ADR-0092 §10：有人在场优先）。
- 事件出口留缝（§8），只留接口不做界面。
- `runtime.rs:407` 那个硬编码的 `format!("gui:{}")` 身份要能区分是谁在开车。

**出口判据**：ADR-0092「验证 2」——GUI 开着、daemon 已在跑，GUI **不重复驱动**
（依赖 ADR-0091 的锁），设置页正确显示驱动在 daemon 那里。

---

## 阶段 5：收尾（未开始）

- ADR-0092 状态行改「已实现」，删 §11 的进度表（它会变成历史）。
- ADR-0090 §5 的话术跟着改：「当前只有 GUI 提供」→「由 `qcd` 提供」。
- `CHANGELOG`；`qc daemon` 的 help 文案。
- §11「`qc` 侧只做客户端」从承诺变成测试。

---

## 未覆盖（明确不做或还没想清楚）

- **登录自启**（launchd / systemd user unit / 计划任务）——ADR-0092 §4 明确本期不做。
- **系统通知**（§9）：daemon 不推送，维护一份「需要人看」的记录，前端查询时读它。
- **GUI 跟随（tail）界面**（§8）：只留缝。
- **多数据目录的 daemon**（§5）：一台机器只有一个 daemon，禁止多数据目录。
- **远程 / 多机调度**。
- **Windows 服务（service）形态**。
- **daemon 的内存与长时间稳定性**：`PERF.md` 目前只有 100k 事件的内存峰值（147 MiB），
  没有小时级曲线；daemon 是常驻进程，这条迟早要补。
- **两个 `Scope` 的真实并发场景**：`single_instance` 现在只有一个用户（GUI），
  「GUI 与 `qcd` 同时拿各自锁」要等阶段 2 才有真实用例。
