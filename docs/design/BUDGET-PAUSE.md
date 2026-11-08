# 预算暂停（Paused）：把「超限报错」改成「交接后让用户决定」

> 状态：已实现（ADR-0060）。§2 与实现有两处有意偏离，见本文末「实现注记」。
> 关联：ADR-0031（预算与无进展）、ADR-0057（schema v5）、ADR-0060（本文档的决定）
> 起因：GUI 里 `max_turns` 命中时弹出红色「请求报错 · 完整信息」，正文是
> `exceeded max turns (64): raise [agent] max_turns or pass --max-turns, or send another
> message to keep going`，只有一个「关闭」。

## 1. 问题

### 1.1 语义错位：预算用完不是错误

三处预算止停走的是同一条「失败」通道（`crates/qcode-core/src/engine/turn.rs`）：

| 触发 | 位置 | 写事件 | turn 状态 | 返回 |
|---|---|---|---|---|
| 超轮数 | `turn.rs:142` | `error(max_turns)` | `TurnEnded{Failed}` | `Err(MaxTurns)` |
| 超成本 | `turn.rs:366` | `error(max_cost)` | `TurnEnded{Failed}` | `Err(MaxCost)` |
| 无进展 | `turn.rs:427` | `error(no_progress)` | `TurnEnded{Failed}` | `Err(NoProgress)` |

三者的性质不同，却共用一顶帽子：

- **预算命中**（轮数/成本）：任务可能完全正常，只是钱或步数到了。同样的输入重试
  **必然**原地不动，唯一出路是用户抬预算或接受当前进度继续。
- **无进展**（`no_progress`）：方法是错的，重试只会重复。这个确实该拦，但文案要说清
  「改方法」，不是「调大 max_turns」。
- **真错误**（provider/store/流中断）：重试可能成功。

现在一律 `Failed` + 红色报错卡，用户第一反应是「产品坏了」，而实际是产品按约停住了。

### 1.2 停的时候没有交接

用户真正想知道的是「做到哪了、还差什么」。这些数据**全都在手边**：`TurnEnded` 的
usage/cost、本轮工具调用序列、v5 起每个工具结果的 `diff`、快照、最后一段助手文本。
现在一个都没用上，只回一句「超过 64 了」。

### 1.3 GUI 的轮数提示口径是错的

`crates/qcode-gui/src/render/main.rs:529`：

```rust
let round_hint_visible = self.app.turns.len() >= self.round_hint_threshold
```

`app.turns.len()` 是**用户消息条数**，而引擎的 `max_turns` 是**单条消息内的模型调用数**。
后果：一个任务里连跑 64 轮时引擎停了、卡片不出现；反过来发满 64 条消息、任务其实很轻
时，卡片天天弹。同名的两张卡（黄色的「Turns 超限」和红色的「请求报错」）语义还是相反
的。

## 2. 决定

### 2.1 四态：`Completed / Cancelled / Paused / Failed`

`TurnStatus` 增加 `Paused`。预算命中落 `Paused`，无进展与真错误仍落 `Failed`。
落盘 schema `EVENT_SCHEMA_VERSION` 5 → 6（v5 日志无新字段，serde 缺省即可回放，
不回填）。

### 2.2 `max_cost_usd` 提前刹车

现在是 `total_cost > limit`（`turn.rs:367`），也就是**已经花超了才停**——对钱来讲太晚。
改为 `total_cost >= limit * 0.95` 即停（`Paused{reason: max_cost}`），余下的 5% 留给
2.6 的收尾调用。`max_turns` 仍在「恰好用完、下一轮将超」时停，语义不变。

### 2.3 临近时先说（不阻断）

新增 `EventKind::BudgetHint`，在**首次**越过阈值的那一轮发一次（`emitted` 位，跨过即
不再发）：
- 轮数：`used >= ceil(max_turns * 0.75)` 或 `remaining <= 8`
- 成本：`used_cost >= limit * 0.75`

GUI 挂在 turn 头（`已用 51/64 轮 · $0.12`），TUI 挂状态行。不弹卡、不夺焦点。
顺手把 1.3 的口径 bug 修掉：两处都改读这个事件的真实轮数，
`app.turns.len() >= threshold` 删除。

### 2.4 停时交接：可推导的不落盘，不可推导的落盘

沿用 ADR-0057 的原则（transcript 完全由 JSONL 重建）。分两块：

**A. `summarize(events) -> ProgressReport`** —— 共享函数，放 `qcode-core`，**不落盘**。
零成本、可复现，GUI / TUI / ACP / `qc run` 都调它：

```text
ProgressReport {
  tool_calls: u32,
  files: Vec<DiffFileStat>,        // 聚合 v5 的 ToolDiff，按 path 去重
  commands: Vec<CommandLine>,      // cmd + exit code + 是否被截断
  failed_tools: Vec<FailureNote>,  // tool + status + 一行输出摘要
  last_text: String,               // 最后一段助手文本，截断到 600 字
  started_at, ended_at: DateTime,
}
```

**B. `PauseSummary`** —— 模型写的那一段，**不可推导，必须落盘**：

停之前给模型**一次无工具收尾调用**，让它回答「已完成 / 未完成 / 建议下一步」。
要求它用固定小标题输出；UI 按标题轻解析，解析失败就整段原文显示（不赌模型出 JSON）。
上限 600 字。这次调用：

- **不计入 `max_turns`**（否则「为了总结又把预算花光」），但成本**照实记账**进
  `TurnEnded.usage` / `cost_usd`；
- 在 `max_cost` 已到 95% 的情况下若估算放不下，就**跳过**，退化为只剩 A ；
- 失败（provider 错、超时）不改变 turn 结局，`summary = None`。

`Paused` 至少要给出 A。A 缺（本轮没调工具）就退化成一句「本轮没有产生工具调用」。

### 2.5 事件形状（schema v6）

```text
TurnEnded {
  status: completed | cancelled | paused | failed,
  usage, cost_usd,
  pause: Option<PauseInfo>,          // 仅 paused 时有
}

PauseInfo {
  reason: max_turns | max_cost,
  limit: { turns: 64 } | { usd: 0.50 },
  used: { turns: 64, cost_usd: 0.118 },
  window: u32,                       // budget window，从 1 起
  summary: Option<String>,           // 收尾调用原文；None = 跳过或失败
}
```

`ProgressReport` 不进日志（可推导）。`window` 必须进日志：配置会被改、价格表会换，
事后无法从事件流反推「这是第几个预算窗口」。

### 2.6 「继续」不是伪造用户消息

继续 = 新 turn（`turns` 归零，预算天然重置），但不能伪装成用户打的一句话——否则
transcript 里会出现用户没说过的话。给 `UserMessage` 加标记：

```text
UserMessage { text, queued_id, continuation: Option<Continuation> }

Continuation {
  turn: u32,                         // 被接续的 turn
  window: u32,                       // 本次是第几个预算窗口
  mode: continue | raise_limit { turns: Option<u32>, usd: Option<f64>, persist: bool },
}
```

- `window` 由**引擎**数（读会话里已有 paused 次数 + 1），不由前端传，保证 `qc run`、
  子 agent、ACP 口径一致。
- UI 把它渲染成灰底条「继续（第 2 个预算窗口）」，不是用户气泡。
- 注入的 `text` 是简短指令（`按你上面的计划继续；已完成的部分不要重做`），
  与用户的真实消息在日志里可区分。

### 2.7 三键卡片，焦点默认在「继续」

替掉现有的黄色「Turns 超限」卡（语义重复），一张卡同时承担「临近」和「已停」：

| 按钮 | 行为 |
|---|---|
| **继续**（primary，默认焦点，回车） | §2.6，预算重置，`window + 1` |
| 放宽上限并继续 | 弹「只这一次 / 存进配置」；存配置走 `config_edit` 写 `agent.max_turns` / `agent.max_cost_usd` |
| 结束（Esc） | 收口为 `Paused`，会话留在侧栏，可随时回来继续 |

卡片内容：`PauseSummary`（已完成 / 未完成 / 建议下一步）+ 可折叠的 `ProgressReport`
（改了哪些文件 `+N −D`、跑过的命令、失败的工具行）。标题琥珀色，写
`已用 64/64 轮 · $0.12` 或 `已达成本预算 $0.50 的 95%`，**不写「报错」**。

保持 ADR-0031 §6 的立场：**不自动续**，但把「继续」做成一键、默认焦点，体验差距被抹平
而原则还在。

### 2.8 `no_progress` 不给「继续」

它是「方法错了」，继续只会重复同样的调用。保留 `Failed`，但：

- 文案改成可操作的中文：`第 3 次以相同参数调用 run_shell：继续只会重复。换个方法，或看看上面的改动再决定。`
- 卡片标题 `原地打转 · 已停止`（不是「请求报错 · 完整信息」），按钮 `关闭` / `查看改动`。
- 真错误（provider/store/流中断）保留红卡与「请求报错」标题。

### 2.9 无人值守链路默认停，不等

| 链路 | 命中预算 |
|---|---|
| `qc run` | 停，退出码 **75**（区别于通用失败 1、取消 130、超时 124）；打印摘要 + `session: <id>` + `qc run --resume <id> "继续"` |
| `--json` | 流里 `turn_ended.status = "paused"`，`pause` 原样输出 |
| ACP | `stop_reason = "max_turn_requests"` **保留**（协议语义就是「requests 用尽」），但**不再**发 error chunk 当失败；可选 `_meta` 带 `{reason, window}`。客户端已知这是可继续的 |
| 子 agent（`crates/qcode/src/subagent.rs`） | 不抛错。`SubagentOutcome` 加 `paused: Option<PauseInfo>`，工具结果返回结构化文本「子任务在 20 轮处暂停：已完成 A、B；未完成 C」。父 agent 自己决定再派一次还是收工 |

自动续（`--continue-on-budget`）**本轮不做**，留待有真实需求。

### 2.10 回放里的卡片只读

重放不产生副作用。回放出的历史 `Paused` 卡：按钮置灰，提示「历史记录；要继续此任务，
直接发一条消息」。只有**当前活跃会话、当前 turn** 的暂停卡可点。

### 2.11 预算窗口要能被看见

`window > 1` 的会话在成本报告（`qc cost` / `/cost` / 会话摘要）里显示
`2 个预算窗口 · 128 轮 · $0.23`，这样「一条任务其实跑了 5×64 轮」不会被误读成
「64 轮」。（决策 3）

## 3. 不做

- 自动续窗口（维持 ADR-0031 §6；`--continue-on-budget` 留待需求）。
- 让 `no_progress` 可继续。
- 为「进度汇总」单独存一份结构化快照进日志——A 部分永远现场推导。
- 时间预算（`--timeout` 之外）。

## 4. 影响面

| 层 | 改动 |
|---|---|
| `qcode-core` | `TurnStatus::Paused`、`PauseInfo`、`EventKind::BudgetHint`、`Continuation` 字段、`summarize()`、schema v6、`EngineError` 拆分出 `Paused` 语义（`MaxTurns`/`MaxCost` 不再是 `Err`，或变成 `Ok(outcome{paused})`） |
| `qcode-ui-state` | 投影四态；`Paused` 不置 `status.running`；卡片数据 |
| `qcode-gui` | 卡片三键 + 只读回放；turn 头 `paused`；删掉 `round_hint` 的错误口径；`apply_rebuild` 后的 re-attach 与暂停态并存 |
| `qcode-tui` | 状态行 `paused · 64/64 · $0.12`；`Enter` 继续 / `e` 结束；`/budget` 看汇总 |
| `qcode-acp` | `stop_reason` 保持，去掉 error chunk |
| `qc`（`run` / `subagent`） | 退出码 75；子 agent 结构化暂停结果 |

## 5. 文档要跟着改

- `docs/design/SPEC.md:117`「超限则终止并标记失败原因」→ 四态。
- `README.md:402`「超限写 `error(max_cost)` 并以失败结束」→ 暂停 + 继续。
- `docs/design/ACP.md:19`「命中 `[agent] max_turns`」→ 说清是暂停且不带 error。
- ADR-0031 §6 加一行指向 ADR-0060（结论不变，只是「继续」变一键）。
- 新增 ADR-0060。

## 6. 验证

- 引擎：`max_turns` 停在 `Paused{window:1}`；继续后 `window:2` 且 `turns` 归零；
  成本 95% 刹车（固定价表）；`no_progress` 仍 `Failed` 且无 `pause`；
  收尾调用不计入 `turns` 但计入 `cost_usd`；收尾调用失败时 `summary=None`、事件仍完整。
- 兼容：v5 日志（无 `pause` 字段）能回放；v6 往返一致。
- `ui-state`：`Paused` 不把 `status.running` 留成 `true`；`ProgressReport` 从事件流投影正确。
- GUI：三键；继续后**同一 session、同一任务**、`window` 递增；回放卡只读；
  删掉 `turns.len()` 口径后，长任务仍能出提示、短任务不再误报。
- CLI：`qc run` 退出码 75；`--json` 里 `status=paused`。
- ACP：`stop_reason=max_turn_requests` 且无 error chunk。
- 回放回归：重放历史 `Paused` 不触发任何注入或网络调用。

## 7. 实现注记（与 §2 的偏离）

1. **UI 入口按前端能力落地**。GUI 的三键是「继续 / 收尾 / 先这样」：§2.7 设想的是
   「继续 / 放宽上限并继续 / 结束」，但「放宽」需要一条输入行内嵌表单，本轮先不做，
   改由 `qc run --raise-turns / --raise-cost / --raise-persist` 覆盖无人值守场景；
   TUI 是 `/continue` 与 `/wrap`。三个前端的「收尾」共用同一个
   `qcode_core::engine::WRAP_UP_PROMPT`。
2. **提示卡的位置**。GUI 的提示条在**对话流里**（随 turn 滚动），不是 §2.3 设想的 turn 头
   chip；50 轮以上的黄色「Turns 超限」卡已被它取代（同一张卡同时承担「临近」和
   「已停」，语义不再重复），阈值改由 `budget_hint` 驱动。
3. **`ProgressReport` 的字段**比 §2.4 少：`started_at / ended_at` 没做（turn 的
   `started_at` 已在事件流里，另存一份只会漂移），`commands` 只保留命令文本 + 成败。
4. **`window` 的口径**：`budget_window` 统计的是「会话里已有多少次 pause」，+1 即本次
   窗口；因此同一会话里连续暂停三轮得到 `window = 3`，与「续了几次」等价。
5. **§2.1 的 `no_progress` 文案**没改（仍是英文原句 + `EngineError::NoProgress`）：
   它已经在红卡里，文案统一留到 `no_progress` 卡片单独做。
