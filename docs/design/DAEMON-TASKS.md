# 常驻 daemon 任务清单（ADR-0092）

> 设计依据：[ADR-0092](./DECISIONS.md#adr-0092常驻-daemondriving-不再绑在窗口上2026-11-15)。
> 状态截至 2026-11-16。**阶段 0、阶段 1 已完成**；阶段 2–5 未开始。
> 阶段 1 的收尾（配置条目并入任务表）单独成篇：[ADR-0096](./DECISIONS.md#adr-0096schedules-与任务表合一一个条目只投一次2026-11-16)。

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

## 阶段 1：让「一次投递」成为可测单元 ✅（2026-11-16）

把 `qcode-gui/src/runtime.rs` 里这几样从窗口生命周期里挪出来：

| 原位置 | 内容 |
|---|---|
| `runtime.rs` `deliver_schedule_group!` | 投递：写收件箱、记账、空闲就开回合 |
| `runtime.rs` `inflight` / `pending_ack` | 「已投递、未确认」的记账 |
| `runtime.rs` `last_beat` / `gap_limit` | 时钟 beat、「进程睡过去了」判定 |
| `runtime.rs` 确认 / 崩溃恢复两段 | 落盘确认、补确认、回执、推进规则 |
| `runtime.rs` `bind_fresh_session` | `NewOnce` 首次投递后绑定会话 |

### 做法：方案 A，但**只搬一拍，不搬循环**

`qcode-driver/src/runtime.rs`，两层：

- **决策层**（`lib.rs`，已有）：`plan_tick` / `plan_recovery` / `advance_past` /
  `delivery_session`。纯函数、假时钟可测。
- **IO 层**（`runtime.rs`，新增 569 行）：`Clock` + `ScheduleDriver::on_beat` + `Host`。

```rust
pub trait Host {
    fn now(&self) -> PrimitiveDateTime;
    fn heartbeat(&mut self, at: PrimitiveDateTime);
    fn executor(&self) -> Executor;              // Attended / Unattended
    fn tasks(&self) -> Vec<ScheduledTask>;
    fn update_task(&mut self, id: &str, patch: &mut dyn FnMut(&mut ScheduledTask)) -> bool;
    fn is_busy(&self, task: &ScheduledTask) -> bool;
    fn exists(&self, task: &ScheduledTask) -> bool;
    fn create_session(&mut self) -> Option<String>;
    fn deliver(&mut self, session: &str, task: &ScheduledTask, approval: Option<Duration>)
        -> Option<Delivery>;                     // 含「该不该立刻开回合」
    fn landed(&self, session: &str, task_id: &str, at: &str) -> Landed;  // Yes/NotYet/Gone
    fn changed(&mut self);
}

impl ScheduleDriver {
    pub fn on_beat<H: Host>(&mut self, host: &mut H, at: Instant) -> Report;
}
```

**与原计划不同的一处，必须说清**：原计划写的是
`pub async fn drive<H: Host>(host, clock, stop)`——让驱动拥有整个 `loop { select! }`。
实现时发现那是错的：

- GUI 的 `select!` 除了时钟还有 UI 命令、回合结束、队列发布（250ms ticker）、会话关闭。
  那些是窗口的事，塞进 `Host` 会造出一种假抽象——trait 上挂十几个只有窗口能实现的
  方法，daemon 一个都用不上。
- 真按 `drive()` 写，GUI 要么留着原来那段再把 `drive()` 的循环包一层（第三个循环），
  要么先重写整个 `select!`。前者正是 §6 要治的病。

所以分界线画在**一拍**上：前端拥有循环，驱动拥有「一拍之内的全部决策与记账」。
`Plan` 那种「只读计划、由调用方落地」的形状保持不变，而 `Delivery` / `StartTurn`
是新的：`deliver` 只把消息放进收件箱，**要不要开回合**由 `Delivery.start` 告诉调用方，
调用方再用自己的装配去跑（GUI 用 `start_turn!`，daemon 直接 await `run_turn`）。

「打开一个回合」不进 trait 是有意的：`run_turn` 要 provider、工具注册表、策略、
sandbox、子代理、审批器……那是前端装配的，不是调度的事。

### 出口判据

1. ✅ 用**假 Host** 在单进程里跑通「建会话 → 到期 → 投递 → 确认落盘 → 再扫」，
   不启窗口、不联网。`qcode-driver/tests/drive.rs` 12 条，真 `SessionStore` + 假时钟：
   - `a_due_task_is_delivered_confirmed_and_advanced`（主链，五步全验）
   - `a_pending_delivery_is_not_delivered_twice`（没落盘就不重投）
   - `a_landed_delivery_is_recovered_instead_of_delivered_again`（重启不重投）
   - `a_restart_after_a_landed_delivery_still_records_the_receipt`（重启补记回执，真机抓获）
   - `a_restart_without_a_landed_delivery_records_no_receipt`（没投过就不造回执）
   - `a_restart_with_an_overdue_task_does_not_replay_it`（错过不补跑）
   - `a_long_gap_records_misses_without_firing`（睡三天只记错过）
   - `a_deleted_target_is_orphaned_and_kept_active`
   - `the_first_beat_only_advances`
   - `unattended_delivery_carries_the_default_deadline` /
     `attended_delivery_keeps_waiting_for_a_human`
   - `a_beat_reads_the_task_table_a_constant_number_of_times`（性能护栏，见下）
2. ✅ 调度段从 211 行非空降到 **约 50 行**（`runtime.rs` 本身 1643 → 1563）：`deliver_schedule_group!`、
   `bind_fresh_session`、`inflight` / `pending_ack`、`last_beat` / `gap_limit`、`GuiProbe`、
   确认与恢复两段、以及 `[schedules]` 配置驱动循环（ADR-0096 连同它的 sqlite 状态一起删）
   全部消失。**新增**的是 `GuiHost`（约 130 行）——那一层不是重复逻辑，是「窗口把驱动的话
   译成对收件箱的动作」。`deliver_schedule_group!`、`bind_fresh_session`、
   `inflight` / `pending_ack`、`last_beat` / `gap_limit`、`GuiProbe`、确认与恢复两段
   全部消失或搬走。**新增**的是 `GuiHost`（约 130 行）——那一层不是重复逻辑，是
   「窗口把驱动的话译成对收件箱的动作」。
3. ✅ GUI 侧覆盖没减：`target_exists` / `target_busy` 两条翻译层测试保留（改成测自由
   函数），另加两条真的 `Engine` + `SessionHost` 单元测试
   （`runtime::host_tests`）——「到点真的进收件箱吗」「忙的会话只排队吗」。这两条守的
   是最容易悄悄坏的一环：`deliver` 少写一行 `followup_scheduled`，驱动那 10 条测试
   仍然全绿（它们只验「我调了 deliver」），而用户什么也收不到。

### 实现中踩到的三个坑（值得记）

- **`Host::tasks` 是读盘**（`Scheduler::list()` → 读 `schedule/tasks.json` + 解析）。
  第一版在 `Action::Fire` 的循环里对每个到期任务各调一次，于是一百条任务同时到期时
  是 100 次读盘 + 100 次解析。测试里三五个任务完全看不出来，所以补了
  `a_beat_reads_the_task_table_a_constant_number_of_times`（二十条任务，断言每拍
  读盘次数 ≤ 3）。这类回归只有**用性能断言**才拦得住。
- **`start_turn!` 里的 `continue`**：那个宏的错误分支靠 `continue` 跳过这一拍。搬进
  任何方法之后 `continue` 都不再合法，所以「建会话失败」改成 `deliver` 返回 `None`、
  驱动跳过这个任务、下一拍再试。顺带修掉一个副作用：原实现会让同一拍里**后续**的
  到期任务也不投递，现在只有失败的那条被跳过。
- **重启后第一拍是 `away`，而 away 分支不补确认——回执会永久丢失。** 这条是**在真机
  上抓到的**，不是设计时想到的：单元测试 12 条全绿、clippy 干净、跨端通过，然后按
  DAEMON-TASKS 自己的出口判据去真机跑了一遍才发现。

  复现：GUI 投递了一个 `once` 任务、回合跑完、进程在下一拍（30s）确认之前被 `kill -9`。
  重启后任务表是 `status: ended` / `last_receipt: null` / `delivery_log: 0`，而会话日志
  里躺着完整的 `user_message` + `assistant_message` + `turn_ended`。用户查「这个任务跑过
  吗」，答案是「没记录」。

  原因：`Clock::tick` 第一拍**总是** `away`（内存记账空，无从判断上一次是什么时候），而
  `on_beat` 的 away 分支在确认与恢复之前就 `return` 了。所以「投递已落盘、规则没推进」
  这个状态，在**新进程**里没有任何一条路径能走到。

  修法：`record_missed` 在推进前先问一句「这一次发生点其实已经投过吗」，投过就补记回执
  （`message_id` 前缀 `recovered:`，审计时能看出是补记的）。回归测试
  `a_restart_after_a_landed_delivery_still_records_the_receipt`，另有
  `a_restart_without_a_landed_delivery_records_no_receipt` 守住反面（没投过就不许凭空
  造回执）。

  **为什么这个坑值得单独记**：它在原代码里同样存在——只是原来这段逻辑长在窗口生命周期
  里，谁也复现不了「杀掉进程再起来」这个状态。阶段 1 的第一个真实收益不是省代码，是让
  这一类「只有真机和重启才暴露」的错变得可测、可复现。

### 没做的（诚实清单）

- ~~**`[schedules]` 配置路径没搬**~~ —— **已解决**（ADR-0096）：配置条目经
  `reconcile_config` 变成任务，配置路径与它的 sqlite 状态表一起删掉。现在只有一条投递
  路径，daemon 不需要再实现第二份。真机实测同一个配置 150 秒内从 4 个会话降到 2 个。
- 事件出口（ADR-0092 §8）只留了 `changed()`，没有做成可跟随的流；§8 说的缝还在。

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

## 阶段 3：分离进程 + `qc daemon` 客户端 ✅（2026-11-16）

- 分离（`setsid` 等价物 / macOS、Linux、Windows 各一段）。Windows 只做当前用户会话里
  的分离进程，**不做服务**（ADR-0092 §12 非目标）。
- `qc daemon start|stop|status`：只做**拉起 + 问话**，不自己驱动。
- **`qcd` 没装时要明确失败**：PATH 里没有 `qcd` 时退出码非 0、stderr 说清楚，不假装成功。

**出口判据**：造一个 `qcd` 不在 PATH 的场景，集成测试断言退出码与 stderr 文本。这条
是 ADR-0092 §11 单独强调的诚实细节，目前**完全无从验证**——连 `qc daemon start` 都还
没有，用户敲下去拿到的是 clap 的 unknown subcommand。

---

## 阶段 4：GUI 改消费方 + daemon 让路 ✅（2026-11-16）

- ~~GUI 删掉自己那份驱动循环~~ —— **阶段 1 已经做了**（`GuiHost` 实现 `Host`，
  时钟分支调 `on_beat`）。本阶段确认它没被破坏。
- **daemon 让路**（§10）：`wait_for_driver_lock` 每秒试一次，拿到就接手。不是退出——
  「先开窗口、后启 daemon」是很自然的顺序，要求人关窗后再手敲一次不合算。等待期间
  **什么都不做**：不写心跳、不扫任务表。
- **事件出口留缝**（§8）：`qcode-driver/src/events.rs`，四个事件（beat /
  turn_started / turn_finished / driving），一行一条 JSON，落
  `schedule/events.jsonl`。选文件而不是 socket：跨平台一个实现、跟随就是 `tail -f`、
  写失败不影响驱动。**只留缝，GUI 的跟随界面不做。**
- **身份已经能分辨**：GUI 写 `gui:<时刻>`、daemon 写 `daemon:<时刻>`，两者都进心跳与
  `Driving` 事件。

**出口判据**：ADR-0092「验证 2」——GUI 开着、daemon 已在跑，GUI **不重复驱动**。

实机验过让路：用一个真进程持锁 5 秒，`qcd` 打印「waiting for it to exit」，锁释放后
打印「taking over」，事件流里留下 `driving:false` → `driving:true` 的时间线。

**未做（诚实清单）**：设置页那一行还没显示「驱动在 daemon 那里」。GUI 的定时任务界面
本来就没有驱动状态位，加它要动 `Backend` 缓存与渲染层。目前用户从 `qc schedule list`、
`qc daemon status` 和工具结果里看得到三处。

---

## 阶段 5：收尾 ✅（2026-11-16）

- ADR-0092 状态行改「已实现」，§11 的进度表收敛成「实现记录」（留结论，删逐行对照）。
- `driver_footer` 的话术跟着改：不再说「当前只有 GUI 提供」，两条路都提
  （GUI 开着 / `qc daemon start`）。**有测试钉住它**——`the_not_running_footer_names_both_drivers`
  检查它提到两条路、名字准确、且「不补跑」没被删。这条话术曾经是说谎的，而说错它的
  后果是用户不去试那条存在的路。
- `CHANGELOG`。
- **「`qc` 侧只做客户端」从承诺变成测试**：`the_cli_does_not_depend_on_the_driver` 读
  `crates/qcode/Cargo.toml`，断言没有 `qcode-driver` 依赖行。行为测试只能抽查几个命令，
  依赖不会说谎——它一旦被加进去，整条边界就没了。验过会红。

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
