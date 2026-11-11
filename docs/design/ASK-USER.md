# ask_user 工具设计文档

> 本文设计一个让 LLM 在工具调用中暂停、向用户请求确认 / 选择 / 缺失信息的能力：`ask_user` 工具。
> 只描述设计，不含实现代码。文中术语一律沿用本仓库既有词汇（`Tool`、`ToolContext`、`Approver`、`UiEvent` / `UiCommand`、session 事件投影等）。

## 1. 概述

`ask_user` 是注册在 `ToolRegistry` 里的一个普通工具（`qcode-tools`），执行时向当前运行时的用户提问，等待回答后把答案作为工具结果喂回模型。它由三层构成：

- **工具层**（`qcode-tools` 的 `ask_user`）：声明 `ToolSpec`，把模型参数翻译成提问请求，把回答 / 超时渲染成工具结果。
- **引擎服务**（`qcode-core` 的 `Questioner` 服务）：类似 `Approver` 的一个 trait，校验请求、等待回答、支持计时；回答方实现（GUI / TUI / ACP / stdin）各自注入。
- **回答方 UI**：问题面板（GUI 弹窗、TUI 状态区、ACP 通知），经 `UiEvent::Question` / `UiCommand::AnswerQuestion` 与引擎通信。

设计目标：

1. **模型可见即可重建**——任何送达模型的内容都能从 session JSONL 事件日志重建；问题状态不单独落库，全部由 `ToolCall` / `ToolResult` / `UserMessage` 事件投影推导。
2. **超时不等于回答**——计时等待到期返回 pending，问题保持可回答，绝不把超时解释为批准、拒绝或跳过。
3. **呈现与协议解耦**——`intent` 只改变 UI 呈现，不改变答案编码；不认识 intent 的 UI 回退为通用选项列表。
4. **交互权归运行时根**——子 agent（`subagent` 工具的嵌套 turn）不能提问；嵌套注册表剔除 `ask_user`，未决问题由子代理写进最终结果交给上层模型。

## 2. 术语与角色

| 概念 | 含义 |
|---|---|
| Question Item | 一个问题：稳定 `id`、问题正文、可选 `header`（眉题）、`detail`（辅助 Markdown）、`options`、`multi_select`、可选 `intent` |
| Option | `label`（既是用户可见文字，也是回给模型的选中值）+ 可选 `description` |
| Answer Item | 按 `id` 回执：`selected`（选中标签数组）+ 可选 `custom`（自由文本）。`selected` 为空且无 `custom` 表示用户跳过该题 |
| Intent | 调用方声明的呈现意图：`confirm`（默认）或 `approval`（权限审批），仅影响呈现 |
| Questioner | `ctx.questioner` 上的回答方抽象，返回答案即完成一次提问；引擎注入，`None` = 该环境无人可答 |
| Open / Continued | 问题的两种可回答状态：工具调用仍可能带着答案返回为 open；超时后答案只能作为带 `question_reply` 标记的新用户消息到达，即 continued |
| Settled | 已结算：问题带着最终答案批次进入只读历史 |

角色划分沿用审批体系的三段式思路：`Questioner` 服务定义（qcode-core）、注入方（GUI / TUI / ACP / headless stdin）、调用方（`ask_user` 工具、未来的 `user-approval` 等）。

## 3. 后台设计

### 3.1 总体架构

```
模型 ⇄ ask_user 工具（qcode-tools）
          │ 翻译参数 / 渲染结果
          ▼
   ctx.questioner 服务（qcode-core）
     ├─ ask()        阻塞提问，等待回答
     ├─ ask_timed()  计时提问（QuestionWait + 倒计时）
     └─ answer()     迟到回答通道（continued 问题）
          │
   BridgeQuestioner → UiEvent::Question → GUI/TUI/ACP 问题面板
          ▲
   Question 投影（从 JSONL 事件折叠问题状态：open / continued / settled）
```

`ask_user` 与审批体系是同一条"人机交互总线"上的两个消费者，但通道独立：审批由策略引擎驱动（`Policy::evaluate` → `ApprovalRequested` → `ctx.approver.approve()`），提问由模型驱动（工具自己调用 `ctx.questioner`）。两者互不抢占对方的答案通道。

### 3.2 工具层（ask_user）

**参数**（`serde_json::Value`，工具内 `#[derive(Deserialize)]` 解析）：

```json
{
  "questions": [
    {
      "id": "q1",
      "header": "可选眉题",
      "question": "问题正文",
      "detail": "可选辅助 Markdown",
      "options": [
        { "label": "选项一", "description": "选项说明，可空" }
      ],
      "multi_select": false
    }
  ],
  "timeout": 120,
  "intent": "confirm"
}
```

**双模式**：无 `timeout` → 阻塞式，一直等到回答或取消；有 `timeout` → 计时式，正整数为前台等待窗口（秒），`-1` 表示无限期（语义退化为阻塞）。工具描述明确告诉模型：空 `selected` 且无 `custom` 的回答项是用户跳过——继续工作、不要再问、不能把跳过当作许可。

**参数约束**：单次调用内问题 `id` 必须唯一；推荐选项放首位并在 `label` 末尾追加 `(Recommended)`（呈现层剥离后缀、改显示"推荐"徽标，不改变回传值）。

**结果渲染**：结构化结果一律 `JSON.stringify` 成单个文本块（`ToolOutput::ok`），紧凑 JSON 而非富内容块。计时超时的结果是：

```json
{ "pending": true, "call_id": "call_...", "message": "等待超时。这不是跳过：问题仍可回答，之后的回答会以带 question_reply 标记的用户消息送达。" }
```

`message` 是固定指令文案，放在结果值内部（而非旁边），因为投影与 UI 都要从记录的结果文本反解出"这是超时而非答案"。模型侧用 `output` 里的 `pending` 字段区分"没答"与"答了"。

**审批分类**：新增 `ApprovalClass::Question`，默认策略对其直接 `Allow`——向用户提问本身就是交互，不应再被审批策略二次拦截。

**准入**：`ctx.questioner` 为 `None`（如 headless 未注入回答方）时，阻塞模式立即以 `NO_QUESTIONER` 失败；计时模式等到期后返回 pending（与无 Client 认领的语义一致）。

### 3.3 引擎服务（Questioner）

#### 3.3.1 服务定义与注入

`Questioner` trait 与 `Approver` 同构：

```rust
#[async_trait]
pub trait Questioner: Send + Sync {
    async fn ask(&self, request: &QuestionRequest, cancel: CancellationToken)
        -> Vec<AnswerItem>;   // 空 selected 且无 custom 的条目 = 用户跳过
}
```

通过 `ToolContext.questioner: Option<Arc<dyn Questioner>>` 注入（仿照 `scheduler` 字段，见 `tool.rs`）。回答方实现：

- `BridgeQuestioner`（GUI / TUI）：`UiEvent::Question` 投递面板，等 `UiCommand::AnswerQuestion`，oneshot 通道返回（同 `BridgeApprover`）。
- `InteractiveQuestioner`（headless `qc run`）：stderr 打印问题、stdin 读行，仿 `InteractiveApprover` 的 `spawn_blocking` + `tokio::select!` 取消模式。
- ACP：经 JSON-RPC notification / request 与编辑器客户端交互。
- 测试：程序化实现，直接返回固定答案。

`ask_user::execute` 内部流程：解析参数 → 校验 → 构造 `QuestionRequest` → 调 `ctx.questioner.ask(request, ctx.cancel)`；返回后按窗口语义组装 `ToolOutput`。取消由 `ctx.cancel` 观察（工具调用期间 engine 不 drop `execute` future，与 `run_shell` 一致），返回 `ToolOutput::cancelled`。

#### 3.3.2 校验

`ask()` 分发前校验：

1. **请求校验**：至少一个问题（否则 `EMPTY_QUESTIONS`）；`intent` 合法性：`approval` 要求恰好一个问题、单选、选项标签必须覆盖 `deny` / `allow` / `always`（命名而非位置决定裁决）、必须阻塞请求（否则 `BAD_INTENT`）；`timeout` 非法值（0 或非整数）以 `BAD_TIMEOUT` 拒绝。
2. **调用方准入**：`ask_user` 只出现在根 agent 的注册表里（见 3.5）；工具本身不需要再查"我是不是子代理"。

#### 3.3.3 计时等待（QuestionWait）

计时等待是引擎侧纯内存对象，不落盘：

- 记录 Host 时钟的截止时间；倒计时归属工具调用，不归属任何 UI 组件——用户收起面板再从工具行重开，计时不丢失。
- UI 在用户开始编辑 / 点击"慢慢回答"时发 `UiCommand::HoldQuestion { call_id }`，引擎把该 wait 的 deadline 暂停；用户提交或取消编辑后发 `UiCommand::ReleaseQuestion` 恢复原 deadline。单 Client（GUI/TUI 各一个回答方）下无需多端同步协议，断线场景退化为"到期转 continued"，回答仍可走迟到通道。
- **超时语义**：到期只中止本次提问的等待并返回 pending，**不取消 Turn**；`ask_timed` 把到期映射为 `{ pending: true, call_id }`。
- **取消传播**：`ctx.cancel` 中止等待 → 工具返回 `cancelled`；引擎销毁时统一关闭存活 wait。

#### 3.3.4 Session 投影（question 投影）

问题状态**不单独存储**，从已有 session 事件折叠（fold）出来，供所有前端与 transcript 重建：

- **识别调用**：`ToolCall` 事件已原样记录 `arguments`（见 `engine/tools.rs` 的 `EventKind::ToolCall`），其中出现 `timeout` 字段即计时调用，进入投影；纯阻塞调用不进入（其答案已在 `ToolResult` 里）。事件 schema 无需为此新增事件类型。
- **状态机**：
  - `ToolCall(name == "ask_user")` → open；
  - `ToolResult` 载荷含 `pending` → continued；载荷含答案批次 → 带着该批次 settled；其他结果（`error` / `cancelled`）→ 丢弃；
  - 中断修复：Session 恢复时 `store.repair_interrupted` 补写的未知结果合成 → 转为 continued，让迟到回答仍可挂靠；
  - 迟到回答：带 `question_reply` 标记的 `UserMessage` 被 agent 准入 → 带着消息里的批次 settled（正文无合法批次则以零答案结算）。
- **视图**：对所有前端暴露 `active`（可回答，按提问序）与 `settled`（已结算，按结算序）两半。transcript 行从 settled 里读"用户最终答了什么"——超时调用自己的结果记录的是超时而非答案。

#### 3.3.5 迟到回答路径（answer）

continued 问题的唯一回答通道是引擎的 `answer` 方法（对应 `UiCommand::AnswerQuestion`）：

1. 校验问题仍在 continued（否则返回 false）；
2. **去重**：同一 `call_id` 已有排队回复时以 `REPLY_QUEUED` 拒绝；批次必须恰好覆盖该调用每个问题 `id` 一次，否则 `BAD_ANSWER`；
3. 构造一条 `question_reply` 用户消息（携带 `call_id` + 原问题 + 答案批次），经 inbox steering 注入运行中的 turn（复用 `UiCommand::Steer` 的通道；turn 未在运行则先恢复根 agent 再回答）；
4. 回复排队后的清理：inbox 被认领、被丢弃（问题重新可回答）、对应轮次结束。

**关键不变式**：没有任何 UI 命令会"放弃"一个问题（收起面板什么都不发）；两条回答路径都不会为已结束的工具调用伪造结果。

#### 3.3.6 错误分类

`QuestionError`（仿 `ToolError`，跨进程传输可还原）：`EMPTY_QUESTIONS`、`BAD_INTENT`、`BAD_ANSWER`、`BAD_TIMEOUT`、`NO_QUESTIONER`、`ASK_ABORTED`、`ASK_TIMED_OUT`、`REPLY_QUEUED`。错误经 `ToolOutput::error` 保留为模型可见的工具失败文案。

### 3.4 后台关键设计决策

| 决策 | 理由 |
|---|---|
| 问题状态走投影而非独立存储 | 满足"模型可见 ⟺ 日志可重建"；天然支持跨重启恢复与 session 导出 |
| 用 `ToolCall.arguments` 里的 `timeout` 字段区分计时/阻塞 | `ToolCall` 事件已原样记录参数，无需新增事件类型 |
| 超时装载 pending 文案进结果 JSON | 投影与 UI 都从结果文本反解"超时而非答案"，单一事实来源 |
| 迟到回答复用用户消息 + inbox steering | 不新增 agent 唤醒机制；回复遵循普通 inbox 准入/丢弃语义 |
| 子 agent 注册表剔除 `ask_user` | 交互权归运行时根；与 `subagent` 剔除自身的既有做法一致 |
| `intent` 只改呈现不改协议 | 旧 UI 不认识 intent 仍显示通用选项列表，答案编码完全一致 |
| 独立于审批体系的新通道 | 审批是策略驱动、模型被动；提问是模型驱动、工具主动；共享答案通道会造成语义混乱 |

## 4. 前端设计

### 4.1 提问入口与路由

`UiEvent::Question`（携带 `call_id`、问题列表、`timed` 标志、剩余时间、`intent`，经 `SessionEvent.target` 定位会话）到达后，前端按 `intent` 路由：`approval` 走固定审批卡片，其余走通用问题流程。路由集中在入口处，保证两种呈现不争抢同一请求。

### 4.2 通用问题卡片

一次提问渲染为一张可最小化的卡片，一次一个问题、分页器切换（"n / 总数"）：

- **头部**：眉题（`header`）、倒计时状态区、最小化 / 关闭按钮。
- **问题区**：标题可展开 / 收起长文；`detail` 以 Markdown 渲染；长选项描述同样可展开。
- **选项区**：单选渲染为编号列表（radio 语义），多选渲染为复选框；推荐选项剥离 `(Recommended)` 后缀、改显示"推荐"徽标（不改变回传值），且推荐项预填为隐式草稿；单选选中后自动跳下一题。
- **自由文本**：单选下 `custom` 覆盖选择（选中即清空选项），多选下作为补充；输入框自动增高，Enter 继续 / 提交、Shift+Enter 换行，处理 IME 组合输入（避免中文输入法选词时误提交）。
- **底部**：分页器、反馈区（校验错误用字典 key 存储、渲染时翻译；运行期错误原文显示）、跳过按钮、下一题 / 提交按钮。
- **跳过**：显式跳过产生空 `selected` 无 `custom` 的回答项；提交前校验整批完成度，缺题跳回第一题未完成的并提示。

### 4.3 倒计时与等待状态

计时问题的卡片头部有等待状态区：counting / waiting（慢慢回答）/ editing / continued：

- 正常倒计时显示剩余秒数与"慢慢回答"按钮；点击或开始编辑即 `HoldQuestion`，引擎暂停 deadline，卡片显示"已暂停"；提交或取消编辑后 `ReleaseQuestion` 恢复。
- 窗口 blur / 页面隐藏释放持有，回到前台且处于编辑态时重新持有（GUI 侧）。
- 问题转 continued 后显示"已继续"状态，回答改走迟到通道（`AnswerQuestion` RPC 路径）。

### 4.4 提交与通道语义

- **open 问题**（窗口内）：提交只算"已发送"；草稿保留到投影关闭卡片为止；若提交途中卡片翻为 continued，控件重新武装并提示重试，同一草稿改走迟到通道。
- **continued 问题**：`AnswerQuestion` 接受后隐藏面板；失败（如 `REPLY_QUEUED`）恢复按钮并显示错误。

提交中冻结整个回答面；关闭按钮对可隐藏请求只是离席（hide，什么都不发），否则是取消。

### 4.5 审批卡片（intent=approval）

`approval` 意图渲染固定审批卡片（拒绝 / 允许 / 始终允许，可带主体名称与本地化文案），不支持多选与计时；按钮按选项标签区分——标签命名而非位置决定裁决，UI 不能从选项顺序推断。提交后决策按普通答案批次返回模型。

### 4.6 对话流中的呈现

- **工具行**：`ask_user` 调用在对话流注册专属 toolview，区分取消 / 中止 / pending / 已答 / 已关闭等状态；通过投影判断问题是否仍可回答或已结算，提供从工具行重开回答面板的入口；已结算调用渲染只读"回顾卡片"——同一面板铺在记录的答案之上，无提交入口，跳过的问题有专门提示。
- **问答配对**：按稳定 `id` 把问题与答案配对，在对话流展示"问题 → 回答"结果视图。
- **迟到回答节点**：带 `question_reply` 标记的用户消息渲染为专门节点，从标记提取 `call_id`、原问题与答案，与 pending 问题卡片配对展示。

### 4.7 持久化与一致性

- **问题状态跨重启恢复**：来自投影（Host 侧），重启后重新打开 session 即可看到可回答 / 已结算问题。
- **草稿不跨端**：未提交的输入草稿只存在于单个前端进程的内存中。
- **单 Client 语义**：一个会话同一时刻只有一个回答方持有面板；最后一个回答方离开后，计时在原 deadline 放行模型。

### 4.8 可访问性与本地化

- 选项区按单 / 多选使用 `radiogroup` / `group` 与 `radio` / `checkbox` 角色；展开按钮带 `aria-expanded` / `aria-controls`；卡片以标题关联可访问名。
- 固定文案走类型化字典 + `t()`，支持运行时切换语言；模型提供的选项标签保持原文作为答案值，不做翻译。

## 5. 端到端流程

**阻塞**：模型调用 → 工具校验 → `ctx.questioner.ask()` → 回答方面板 → 用户提交 → 答案作为工具结果 JSON 返回模型。

**计时窗口内回答**：模型调用（含 `timeout`）→ `ask_timed` 建 wait → 回答方认领面板并接管倒计时 → 窗口内提交 → 答案作为工具结果返回。

**计时超时后回答**：窗口到期 → 工具返回 `{pending:true,...}` → 模型继续独立工作 → 投影转 continued → 用户稍后提交 → `answer` 校验去重 → 构造 `question_reply` 用户消息 steer 进 agent → agent 准入后投影 settled，对话流显示问答配对。

## 6. 与既有审批体系的关系

| 维度 | 审批（Approver） | 提问（Questioner） |
|---|---|---|
| 驱动方 | 策略引擎（`Policy::evaluate` → `Action::Ask`） | 模型（工具调用） |
| 触发点 | 任何工具调用前 | `ask_user` 工具执行时 |
| 答案词汇 | `Allow / AllowSession / Deny` | 选项 + 自由文本 |
| 通道 | `UiEvent::Approval` / `UiCommand::Answer` | `UiEvent::Question` / `UiCommand::AnswerQuestion` |
| 超时 | 无（等待即阻塞） | 计时窗口 + pending 语义 |

两者共享 `CancellationToken` 取消、事件投影与前端呈现基础设施，但答案通道完全独立，避免把"允许执行"误当"回答问题"。

## 7. 已知限制

- headless（`qc run` 未注入回答方）下阻塞调用立即失败；计时调用只能等到期转 pending。
- 中断的阻塞调用不能接受迟到回答（结果已 cancelled，问题丢弃）。
- 子 agent 不能提问；其未决问题必须写进子代理最终结果交给父模型。
- 词汇只有"选项 + 自由文本"的问题表单；文件选择器、diff 预览确认等更富交互尚无协议词汇。
- 模型侧结果一律为紧凑 JSON 文本，非富内容块。
- `question_reply` 迟到回答要求 turn 存活或先恢复根 agent；已关闭 session 需先 `resume`。

## 8. 实现状态（v1）

已按本设计落地的部分：

- 核心链路完整：`ask_user` 工具（阻塞 / 计时 / 无回答方兜底）、`Questioner` 服务注入、`BridgeQuestioner`（窗口内 oneshot 直答 + 超时静默摘除注册表条目）、`InteractiveQuestioner`（headless stdin）、迟到回答走 `question_reply` 用户消息 + inbox steering，事件 schema v9。
- 前端：GUI 提问面板（分页器、单/多选、跳过、倒计时、超时转"已继续"后回答仍可送达）；TUI 键盘回答（数字选选项 / s 跳过 / Enter 提交 / Esc 取消）；headless `qc run` stdin 回答。
- 子代理注册表剔除 `ask_user`（与剔除 `subagent`、schedule 工具同法）。

v1 暂缓、设计仍成立的部分：

- ACP 回答方（`questioner` 为 `None`，ACP 会话里 `ask_user` 报"无回答方"）；后续可仿 `AcpApprover` 走 JSON-RPC。
- `HoldQuestion` / `ReleaseQuestion`（倒计时暂停）：当前倒计时只在客户端展示，Host 侧窗口固定；"慢慢回答"暂不暂停 deadline。
- GUI / TUI 的自由文本（`custom`）输入：v1 只有选项与跳过，`custom` 由协议支持但前端暂不渲染输入框。
- 迟到回答去重：由 UI 提交后乐观关闭面板规避重复提交；`REPLY_QUEUED` 语义保留在协议层。
- 独立的 active / settled 投影视图：当前由事件直接驱动 UI（`UiEvent::Question` / `QuestionClosed`），跨重启恢复靠 replay + `adopt_dormant` 收留后台问题。
