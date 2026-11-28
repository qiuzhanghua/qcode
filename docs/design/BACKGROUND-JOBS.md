# 后台任务：超时不杀，转后台

> 设计依据：[ADR-0106](./DECISIONS.md#adr-0106超时不再杀进程转后台并给一个可读的任务句柄2026-11-22)。
> 本文只写**怎么落地**与**边界在哪**，为什么这么选在 ADR 里。
> 相关：[ADR-0073](./DECISIONS.md#adr-0073退出时杀掉-run_shell-的进程组detached-线程让-kill_on_drop-永不执行2026-11-12)
> （退出时杀进程组）、[ADR-0003](./DECISIONS.md#adr-0003并行只读工具2026-10-19)（并行工具）、
> [SANDBOX-MODE.md](./SANDBOX-MODE.md)（写作用域梯子）。

## 1. 改动前：到点就杀，且杀得看不出来

`run_shell` 曾把等待与终止压成同一条路径：

| 项 | 改动前 | 现在 |
|---|---|---|
| 参数 | `timeout_secs`，默认 `120`，`clamp(1, 600)` | 同名同址默认收到 `60`（`shell.rs` `DEFAULT_WAIT_SECS`），语义变「前台等待预算」 |
| 到点 | `SIGTERM` 整组 → `TERM_GRACE`(2s) → `SIGKILL` 整组 | 转后台；信号梯子只留给取消与 `job_kill` |
| 返回 | `ToolStatus::Error` + `command timed out after Ns` | `Ok` + 句柄 + 已有输出；那句文案已删 |
| 模型侧 | 只有「猜一个 `timeout_secs`」，`spec` 里一个字都没提长命令 | `spec` 写明「超预算不杀、转后台、用 `job_read` 跟进」 |
| 用户侧 | 工具卡 `status: None` 表示「还在跑」，`output` 只在结果到达时填一次 | 不变 |

后果是两个都很糟的选择：猜小了白跑一遍再重跑，猜大了整轮卡住、用户连话都插不进去
（`ask_user` 之外没有交互点）。

## 2. 要的行为

三条，缺一不可：

1. **跑久了不丢**：超过等待预算就转后台，命令继续跑，输出继续攒。
2. **能读增量**：模型按需读「上次之后的新输出」，而不是每次重读全文。
3. **能停**：模型（或用户）可以明确杀掉某个后台任务。

有这三条，模型在长任务期间就能和用户继续对话——这才是「后台」的全部价值。
**「跑完自动叫醒模型」不在这一版**（§8 说明为什么现在做不到）。

## 3. 决定

### 3.1 超时是「前台等待预算」，不是「死刑判决」

`timeout_secs` 的含义改为：**我最多愿意当着你的面等这么久**。到点不杀，转后台，
返回一个任务句柄。名字和取值范围都不变，所以旧调用不需要改一个字。

```
run_shell {command: "cargo test"}            → 60s 内跑完：和今天完全一样
                                             → 到 60s：返回 job 句柄 + 已有输出
run_shell {command: "npm run dev", background: true}  → 立刻返回 job 句柄
```

新增 `background: bool`（默认 `false`）。它的价值是把「本来就知道不会结束」的命令
（dev server、watch、tail -f）从「白等 60 秒」变成「立刻返回」。

**取消路径故意不变**：用户在工具还在**前台等待**时按 Esc，仍然 SIGTERM→SIGKILL 整组。
理由是这两件事的语义不同——

| 事件 | 谁按的 | 语义 | 行为 |
|---|---|---|---|
| 等待预算用完 | 时钟 | 「我不等了，你继续」 | 转后台 |
| 用户取消回合 | 人 | 「停下」 | 杀整组（与今天一致） |

取消检查发生在转后台之前，所以不存在「用户按了 Esc、命令却转后台活下来」的竞态。
已转后台的任务与取消无关——工具调用早就返回了，没有可取消的东西。

### 3.2 任务身份与注册表

- 句柄形如 `job-7`：注册表内自增、不复用。生产里一个进程只装配一次工具表，所以
  实际等同进程内唯一；测试各自一份表，编号各自从头。
- 注册表挂在 **工具注册表**（`builtin_registry_with_env` 装配的那一份）上，不是
  进程级 `static`：任务表不该比用它的人活得久——会话拆掉时任务交给寿命上限收尸，
  而不是交给「最后一个 `Arc` 什么时候掉」。**退出钩子不走这张表**：它看的是
  `child_groups`（§3.6），两张表各自回答不同的问题。
- 每个任务记：句柄、命令行（`job_list` 只显示首行）、cwd、起始时刻、状态
  （`Running` / `Exited(码)` / `Killed` / `Expired`）、每流的已产出字节数、缓冲。

**上限三个**：同时存活 16 个（超过就拒绝并让模型先 `job_kill`）；单任务寿命
**240 分钟**（到点整组 `SIGTERM`→`SIGKILL`，状态记 `Expired`，`job_read` 里说明是
寿命到点）；已结束的任务只留最近 8 个供查询，更早的按「没有这个任务」回答。
没有上限的话，「转后台」等于「泄漏」。

寿命是 `MAX_WAIT`（`job_read` 一次最长等 120 分钟）的**两倍**，代码里就是这么定义的
（`MAX_LIFETIME = MAX_WAIT * 2`）。这条不等式有含义：长轮询必须在任务死之前有机会超时，
否则它唯一能产出的状态「还没输出、还活着、再问一次」就不可达，每次都只能等到 `Expired`。

### 3.3 输出：每流一条环形缓冲，各一个单调游标

- `stdout` 与 `stderr` **各一条**缓冲，每条 512 KiB（合计 1 MiB），只保留**最近**的
  字节。**没有**合并成一条，也没有归属标记：合并要在每次 append 前对齐两条流的
  到达顺序，而记录顺序只在「谁先说的」有意义时才值钱，对构建/测试日志反而把 diff
  搅乱。代价是每次 `job_read` 先给 `stdout` 再给 `stderr`（与前台「stdout 全文再
  stderr 全文」同一个顺序），后台读里看不到时间交错。
- 每流一个**只增不减**的游标（已读偏移）。`job_read` 从游标读到缓冲末尾，
  然后把游标推到底。
- 游标被覆盖掉的情形（一次产出超过容量才来读）必须**说出来**：
  返回里带「有 N 字节已经被冲掉」。沉默地少给数据比少给更坏。
- 每次 `job_read` **每流**最多返回 32 KiB（与前台 `OUTPUT_LIMIT` 一致），两条流合计
  最多 64 KiB，剩下的下次读。

### 3.4 长轮询：`job_read` 可以等

模型最笨的用法是「读一次 → 睡 → 再读」。让 `job_read` 自己等：

| 参数 | 含义 |
|---|---|
| `job` | 句柄 |
| `wait_secs` | 可选，最长等多久（`0`–`7200`，默认 `0` = 立刻返回） |

在有新输出、或任务退出、或超过 `wait_secs` 时返回。于是「等这个构建结束」是**一次**
工具调用。`wait_secs` 的值会被 `ToolContext::cancel` 打断（用户取消 → 立刻返回，不杀任务）。

### 3.5 三个工具

| 工具 | 审批分类 | 干什么 |
|---|---|---|
| `job_list` | `Read` | 列出存活与最近结束的任务（句柄、命令行首行、状态、年龄、字节数） |
| `job_read` | `Read` | 读新输出，可带 `wait_secs` |
| `job_kill` | `Execute` | 整组 SIGTERM→SIGKILL，与前台取消同一套信号路径 |

**相位 → 结果状态**（`Phase::tool_status`）：

| 相位 | `ToolStatus` | 理由 |
|---|---|---|
| `Running` / `Exited(0)` | `Ok` | 工具做的事成功了 |
| `Exited(非零)` / `Expired` | `Error` | 任务本身失败；读成功了，但结果要让模型看见「失败」 |
| `Killed` | `Ok` | **不能**是 `Cancelled`：引擎把 `Cancelled` 读作「本回合被取消」，会当场结束回合、不再叫模型（`qcode-core/src/engine/tools.rs`）。杀任务是模型要求的成功动作 |

`job_kill` 的时延跟着进程的实际反应走：SIGTERM 之后等最多 `TERM_GRACE`(2s)，进程死了
就立刻返回；只有忽略 TERM 的才吃满宽限期再 SIGKILL（再等最多 2s）。「固定睡 4 秒」
是实现初期的一个 bug——`tests/jobs.rs` 同时钉住「合作进程 <1s」与「忽略 TERM 仍升级」。
只读两个工具给 `Read`：它们读的是**用户已经批准过的那条命令**的输出，不引入新的
文件或网络访问。`job_kill` 给 `Execute`：它确实是「对机器下手」，虽然方向是停。

### 3.6 生命周期：退出即全杀，沿用 ADR-0073 的机制

这是本文最省事的一条。`run_shell` 今天在 spawn 之后建一个
`child_groups::LiveGroup::register(pid)`，命令被 reap 时守卫 `Drop` 注销；`qcode`
退出时 `kill_all()` 把还登记的组整组 SIGKILL。

后台任务**不另建登记表**：把那个 `LiveGroup` 守卫交给任务的 supervisor 持有即可。
守卫的生命周期跟着任务走，于是——

- `qcode` 退出 → `kill_all()` → 后台任务一起死（ADR-0073 的保证一条没破）；
- 任务被 reap → 结构体掉落 → 守卫注销 → 不会因为 pid 复用而误杀无关进程。

换句话说，「后台」只相对于**回合**成立，不相对于**进程**。用户关掉 qc，任务就没了。
这是有意的保守选择：跨 qc 重启存活要另建持久化与孤儿收尸，见 §8。

### 3.7 沙箱不动

后台任务与前台命令走**同一个** `Sandbox` 实例、同一套 `with_roots` 重建逻辑、
同一份环境白名单。分工界线：`run_shell` 决定怎么起（沙箱、进程组、`PDEATHSIG`、
Windows job），`jobs` 只负责「起了之后怎么记账和读输出」。所以没有任何一条
沙箱约束需要因为「转后台」而放宽。

## 4. 数据流

```
run_shell::execute
  ├─ 建沙箱 Command（不变）
  ├─ spawn + process_group(0)
  ├─ 两条管道各起一个 pump，边读边追加进各自的 Stream（前台不设上限）
  ├─ 前台等待 min(预算, 退出)
  │     ├─ 退出 → 老路径：等 pump 收尾、truncate、返回（逐字节不变）
  │     └─ 到点 → 把游标推进到这一刻、给两条 Stream 设上限，
  │               连同 LiveGroup 守卫与退出通知交给 jobs::detach → 立刻返回句柄
  └─ background: true → 跳过等待，走同一条交接路径
```

进注册表之后：

```
pump(stdout) ──▶ Stream(out) ┐
pump(stderr) ──▶ Stream(err) ┤◀── read(每流游标) ── job_read
supervise ── wait ──▶ 相位 + done ┴── wait_done ── job_kill / 寿命到点
```

「转后台」交给注册表的就是前台等待期间**已经在填的那两条 `Stream`**，所以已经产出的
字节不会在交接那一刻消失——否则 `cargo test` 前 60 秒的输出会凭空不见。`supervise`
持有 `LiveGroup` 与退出通知：进程被 reap 之后才注销守卫、翻 `done`，等它的
`kill` / 寿命到点因此不必靠睡眠猜。

## 5. 边界与失败模式

| 情形 | 处理 |
|---|---|
| 孙进程持有管道（`sh -c 'sleep 600 &'`） | 沿用 `DRAIN_GRACE` 的思路：进程退出后最多再等 5s 收尾，之后照常报退出码，不因为管道没关而「永远在跑」 |
| 输出洪泛（每秒几十 MB） | 每流只留 512 KiB（合计 1 MiB）；`dropped_bytes` 如实上报 |
| 16 个任务打满 | 拒绝新命令，错误里附 `job_list` 的提示，不改任何既有任务 |
| 转后台后 `qcode` 崩溃（非正常退出） | 与今天同级：Linux 靠 `PDEATHSIG`（只管直接子进程），macOS 无兜底，Windows 靠 job object。**没有变差**：今天崩溃时会残留的，转后台后一样会残留 |
| 会话被删除 / 任务被关掉 | 任务不随之结束，靠寿命上限收尸。跨会话管理不在本版 |
| 同一批里两个 `job_read` 抢同一个游标 | 每流一个游标、只有模型一个读者，但 `job_read` 是 `Read` 类，引擎会把同一批只读调用**并行**发出去；两个读各自推进自己的游标，输出被切成前后两段落到两条结果里，`dropped_bytes` 让「谁丢了数据」可见 |

## 6. 与既有决定的关系

- **ADR-0073 不变**，而且被复用：退出即全杀靠的还是 `child_groups`，只是守卫的
  宿主从栈帧换成了任务结构体。本文没有引入任何「例外于 procguard 的进程」。
- **ADR-0003（并行只读工具）不冲突**：`job_read` 是只读，可以和别的只读工具并行。
- **ADR-0105（i18n）**：新增用户可见文案全部进 `locales/app.yml`，与工具结果同语言。
  工具名（`job_read`）与参数名不翻。
- **Clean room**：这里的交互形状（长命令转后台 + 按游标读增量输出）是通用做法，
  没有参照任何禁止清单里的实现；命令名、参数名、文案全部自创。

## 7. 验证（已落地）

原计划的九条 + 落地时补的两条，都落在三个测试文件里：

| 断言 | 测试 |
|---|---|
| 预算内跑完逐字节不变；到点转后台、句柄可用、进程确实还活着、`job_kill` 收掉整组 | `crates/qcode-tools/tests/jobs.rs::a_spent_budget_detaches_instead_of_killing` |
| `background: true` 立刻返回，且不冒充退出码 | `…::background_returns_without_waiting` |
| 两次 `job_read` 拿 `a` 与 `b`，不重复 | `…::reads_are_incremental` |
| `wait_secs` 在退出那一刻返回，不等满预算 | `…::wait_secs_returns_when_the_job_exits` |
| 等待期取消只停等待、不停任务 | `…::cancelling_the_wait_does_not_stop_the_job` |
| `job_kill` 对合作进程 <1s，对忽略 TERM 的升级到 SIGKILL | `…::job_kill_returns_as_soon_as_the_process_is_gone`、`…::job_kill_escalates_when_the_process_ignores_sigterm` |
| 第 17 个任务被拒，前 16 个不受影响 | `…::the_seventeenth_live_job_is_refused` |
| 冲掉的字节如实上报 | `…::eviction_is_reported_rather_than_hidden` |
| 两个预算就是 60s / 240 分钟 | `…::the_two_budgets_are_the_agreed_numbers` |
| 长轮询能在任务死之前超时 | `…::a_wait_can_time_out_while_the_job_is_still_running` |
| 退出即全杀 | `crates/qcode-tools/tests/jobs_exit.rs::quitting_kills_background_jobs` |
| 杀任务/读被杀的任务都不结束回合；`Cancelled` 专属「回合取消」 | `crates/qcode-providers/tests/job_turn.rs`（三条） |

前台取消仍然杀整组：`crates/qcode-tools/tests/tools.rs::cancellation_kills_the_whole_process_group`。

一条边界说明：`child_groups::kill_all()` 走的是信号，不是 `job_kill`，所以任务的相位
变成 `Exited(未知码)` 而不是 `Killed`——进程死没死是可观察的，**谁**杀的它不可观察，
而 qcode 退出之后也没有读者了。测试只钉「进程真的没了」。

## 8. 本版不做，以及为什么

| 不做 | 理由 |
|---|---|
| **跑完自动通知模型** | 需要引擎有「外部往会话注入一条消息」的通道。今天只有回合内的 steering 与 `qcode-driver` 的定时投递，两者都不是这条缝。`wait_secs` 覆盖了 90% 的用例，剩下 10% 值得单独一个 ADR |
| **跨 qc 重启存活** | 需要持久化任务表 + 重启后的孤儿认领 + 输出落盘。与「退出即全杀」直接冲突，得先改 ADR-0073 |
| **GUI/TUI 的任务面板** | 现在是纯工具输出（模型在 transcript 里看到什么，用户就看得见）。做成可点杀的面板要先决定它归哪个视图：工具卡、还是内建终端（ADR-0062）的兄弟 |
| **任务的输出落盘** | 两流合计 1 MiB 内存够用；落盘要处理清理、配额、隐私（输出里可能有密钥） |
| **优先级 / 限流** | 16 个并发上限是粗糙但够用的闸 |

## 9. 落地顺序（已完成）

1. `jobs` 注册表 + 环形缓冲 + 游标（无 UI、无引擎改动，可单测）。✅
2. `run_shell` 转后台（含「已产出的字节不丢」）。✅
3. `job_list` / `job_read` / `job_kill` 三个工具接进 `builtin_registry_with_env`。✅
4. i18n 文案 + 上面 §7 的测试。✅
5. `spec` 里给模型的描述：明确「长命令会自动转后台，用 `job_read` 跟进」——
   否则模型不知道有这个能力，等于没做。✅
