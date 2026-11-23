# 会话标题的模型自动生成

> 状态：设计稿（待评审）
> 关联：现有标题推导（`crates/qcode/src/commands/sessions.rs` `task_title_from_events`、
> `crates/qcode-gui/src/backend.rs` `title`）、`TaskRenamed`（`crates/qcode-core/src/store.rs`）、
> schema v11（`crates/qcode-core/src/event.rs`）、定时任务命名（`crates/qcode-core/src/scheduler.rs`）
> 起因：侧栏与 `qc sessions list --titles` 的标题只有「用户重命名」和「首条消息截前 48 字符」
> 两个来源。截断体验差：中文按码点硬切可能断在词中间，且首条消息往往包含寒暄/上下文，
> 截出来不像标题。

## 1. 现状

标题推导链（[sessions.rs](../../crates/qcode/src/commands/sessions.rs)）：

1. **用户重命名**：`TaskRenamed` 事件，后写者胜出，永远优先。
2. **首条用户消息兜底**：剥定时任务预填 → 换行折叠空格 → trim → 前 48 码点。
3. **定时任务名**：`set_session_title_if_missing` 只在会话无标题时写入 `TaskRenamed`。

## 2. 目标与非目标

**目标**

- 会话建立后，用模型从首条用户消息生成一个简洁标题，替代「截前 48 字符」。
- 生成失败 / 未启用 / 无合格消息时，行为与现状完全一致（确定性兜底）。
- 用户重命名永远最高优先，且「钉住」——重命名后不再触发自动生成。
- 标题调用与主 turn 完全解耦：不占用 turn 的预算与轮数，不阻塞主循环。

**非目标**

- 不做「每条新消息都重生成」的变体（v1 只做首条消息触发；接口留 cadence 扩展点）。
- 不为标题调用单独计费或进入成本统计（独立小额调用，随 turn 失败一并丢弃）。
- 不生成多语言/风格化标题；语言跟随消息语言即可。

## 3. 标题来源优先级（权威链）

同一会话内，展示层按以下顺序取标题，**后写者胜出**（按事件 `seq` 比较）：

| 来源 | 事件 | 说明 |
|---|---|---|
| 用户重命名 | `TaskRenamed` | 显式、钉住；后写覆盖一切自动结果 |
| 模型生成 | 新 `TitleGenerated` | 见 §4；写后即为最新标题 |
| 确定性兜底 | 无事件，展示时推导 | 首条合格消息截断（现状逻辑） |
| 定时任务名 | `TaskRenamed` | 会话无标题时写入，之后视为一次重命名 |

**钉住规则**：一旦日志里出现用户发起的 `TaskRenamed`（含定时任务命名），该会话不再调度
自动生成；`TitleGenerated` 也不会覆盖它。

## 4. 事件模型（schema v12，additive）

新增事件 kind（`crates/qcode-core/src/event.rs`）：

```rust
/// 模型（或确定性兜底）生成会话标题。展示层取最新一条。
pub struct TitleGenerated {
    pub title: String,               // 规范化后的标题
    /// 使用到的用户消息 seq（首条触发时恒为 [first_seq]）。
    pub message_seqs: Vec<u64>,
    /// 来源：auto（模型生成，含 provider/model）或 fallback。
    pub source: TitleSource,
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TitleSource {
    Auto { provider: String, model: String },
    Fallback,
}
```

要点：

- 新增是 **additive**：`EVENT_SCHEMA_VERSION` 升到 12，旧日志（无新事件）读取不变。
- **兼容性实现注记**：[store.rs](../../crates/qcode-core/src/store.rs) 目前对未知
  `type` 直接 `StoreError::Json`——「旧版本读新日志」会失败。为落实 event.rs 文档承诺的
  「旧 reader 忽略未知 kind」，给 `EventKind` 加一个 catch-all `Unknown` 变体
  （`#[serde(other)]` 或按 `type` 预判），`store::load` 遇未知 kind 跳过而非报错；
  这同时让未来所有 additive 事件免费获得兼容性。
- `TaskRenamed` 保留不动，避免迁移与历史日志重放问题。

## 5. 触发与并发控制

**触发点**：driver 层（CLI 的 `prompt` 入口、GUI 的 `backend.prompt`）拿到本次输入、
在 `run_turn` 之前检查并发起，因为那里持有 `Box<dyn Provider>`（`qcode-kernel::build_provider`），
而 `run_turn` 里的 provider 是借用、无法 `tokio::spawn`。

**触发条件**（全部满足才调度）：

1. 配置 `[session_title] enabled = true`。
2. 会话还没有任何 `TaskRenamed` / `TitleGenerated`（无标题）。
3. 本次输入是**合格消息**：非 `continuation`、非 `scheduled`、非 `question_reply`、
   非 steering 注入（与 [event.rs](../../crates/qcode-core/src/event.rs)
   的 `UserMessage` 可选字段对应），清洗后非空。

**并发模型**：

- 生成任务 `tokio::spawn` 独立运行，自带 `CancellationToken`；**不阻塞主 turn**。
- 单会话单飞：写入前再查一次「仍无标题」，防止与并发 rename 竞态覆盖。
- 取消时机：服务关闭 / 进程退出 / 生成中用户重命名（写 `TitleGenerated` 前 abort 并放弃）。
- 失败不重试；一次失败后本会话不再调度（避免反复小额烧钱）。

**路由**：优先 `[session_title] provider/model` 显式覆盖；缺省时复用该会话的
`SessionStarted` 记录的 provider/model（[event.rs](../../crates/qcode-core/src/event.rs)），
不等待 request header。

## 6. 模型调用执行参数

复用现有 provider 端口：`CompletionRequest { model, system, messages, tools: vec![], max_output_tokens }` +
`collect_stream`（与 `turn.rs` 的 hand-off 调用同机制）。

| 配置项 | 默认 | 说明 |
|---|---|---|
| `enabled` | `false` | 显式开启，避免默认给所有会话增加成本 |
| `max_title_bytes` | `80` | **任何被接受标题**的 UTF-8 字节硬上限（含重命名、生成、兜底），不拆码点截断 |
| `fallback_max_words` | `5` | 确定性兜底的词数上限（空白分隔） |
| `fallback_max_bytes` | `40` | 确定性兜底字节上限；校验 `≤ max_title_bytes` |
| `max_input_bytes` | `2000` | 输入（含 JSON 框架）UTF-8 字节上限，超限放弃生成 |
| `max_output_tokens` | `64` | 独立于对话调用的输出 token 上限 |
| `timeout_ms` | `15_000` | 端到端超时，超时按失败处理 |
| `provider` / `model` | 空 | 必须成对出现；空 = 跟随会话主路由 |
| `target_words` | `5` | 非 CJK 目标词数（软目标，见下） |
| `target_cjk_chars` | `12` | CJK 目标字符数（软目标，见下） |

校验规则：正整数、`provider`/`model` 成对、`fallback_max_bytes ≤ max_title_bytes`、
`max_input_bytes ≤` 合理的上限。

**长度约束体系**（对标题字数的全部约束）：

- **硬上限**（唯一真正被强制执行的）：`max_title_bytes`——用户重命名、模型结果、兜底
  一律先规范化、再按 UTF-8 字节截断且不拆码点。按字节而非字符计数，保证展示侧宽度可控。
- **模型输出：只有软目标，无硬性字数校验**：`target_words` / `target_cjk_chars` 只写入
  提示词作引导（"约 N 个词/字"），生成结果不对字数做强制校验，仅受 `max_title_bytes`
  与 `max_output_tokens` 间接约束。因此模型标题可能明显长于兜底
  （80 字节 ≈ 40 个英文字符 vs 兜底 40 字节），两者风格允许不一致。
- **兜底：词数 + 字节双上限**：先取前 `fallback_max_words` 个词、再按
  `fallback_max_bytes` 字节截断，保证兜底始终短而完整。
- **输入上限**：`max_input_bytes` 防超大输入，超限直接放弃生成。

## 7. 提示词与输入构造

**系统提示**（自创文案，不照搬任何现成提示词）：要求

- 为编码助手会话生成一个简洁标题，**只输出一行纯文本**；
- 不使用引号、前缀、解释、Markdown、XML 或终端控制码，不写代码；
- 语言跟随消息语言；非 CJK 约 `target_words` 个词，CJK 约 `target_cjk_chars` 个字；
- 消息信息量很少时仍返回一个短的最佳努力标题（如「问候」），不要解释。

**输入构造**：把首条合格消息的文本做 JSON 编码后，作为唯一一条用户消息发给模型
（结构化的 JSON 框架使消息内容无法破坏请求边界）。

**输入范围**：只取首条消息，不携带对话历史与工具结果（标题成本与噪声都最小化）。

## 8. 输出解析与校验

- 只接受流终止原因为「正常结束」（`StopReason::EndTurn`）；`MaxTokens` / `ToolUse` 视为失败。
- 取模型输出的**第一个非空行**作为候选标题（模型可能违反单行要求把标题写在前面、
  注释在后面）；剥掉整行星号强调包裹（如 `**标题**`）。
- 规范化（复用/扩展现有清洗）：剥 OSC/CSI/ESC 转义与控制字符、不可见方向控制符
  （零宽、双向文本），空白折叠为单空格，压成单行，再按 `max_title_bytes` 字节上限
  截断且不拆码点。
- 空标题 = 失败，回退确定性兜底。
- `message_seqs` 必须取自本次输入、有序且唯一。

## 9. 失败语义

- 任何失败（超时、超 token、网络、空输出、规范化后为空）→ **静默**回退到确定性兜底
  （§10），只写 `tracing::debug!`，不打扰用户。
- 不写 `Error` 事件、不影响 turn 状态。
- 标题调用不进入 `TurnEnded.usage` / `cost_usd`（独立小调用，与对话预算解耦）。

> **实现注记**：§5 要求「一次失败后本会话不再调度」，但展示时推导的兜底不落日志、
> 无法记录"已尝试过"。实现把失败时的确定性兜底写成一条 `TitleGenerated{source: fallback}`
> 事件——既给出标题，也天然标记该会话已有标题，后续 turn 不再触发（与 §10 的
> 展示时推导等价，只是多了一条日志事实）。

## 10. 确定性兜底

保持不变，但提升为「无模型结果时」的最终兜底，与现状一致：

- 展示层推导（不写事件）：首条合格消息，剥预填 → 折叠空白 → trim。
- 上限从「前 48 码点」改为「前 `fallback_max_words` 个词、按 `fallback_max_bytes`
  字节截断」（与 §8 共用同一套清洗），让兜底与生成结果的风格一致。

## 11. 展示层改动

- 侧栏（`crates/qcode-gui/src/backend.rs` `title`）与 CLI（`sessions.rs` `task_title_from_events`）
  统一改为：**最新一条** `TitleGenerated` 或 `TaskRenamed` → 都没有再走首条消息兜底。
- `set_session_title_if_missing` 语义不变（仍是「无标题才写」）。

## 12. 实现拆分（落点）

| 文件 | 内容 |
|---|---|
| `crates/qcode-core/src/title.rs`（新） | 纯函数：清洗/规范化、词数 + 字节截断、兜底推导、输出解析（首行/剥强调）、JSON 框架 |
| `crates/qcode-core/src/event.rs` | `TitleGenerated` + `TitleSource` + catch-all `Unknown` 变体，schema v12 |
| `crates/qcode-core/src/store.rs` | 宽容读取未知 kind；`append` 复用 |
| `crates/qcode-core/src/scheduler.rs` / `sessions.rs` / `backend.rs` | 展示层读取最新标题 |
| `crates/qcode/src/cli.rs` / `qcode-gui/src/backend.rs` | 触发点：调度生成任务（持有 owned provider） |

## 13. 测试

- 规范化：控制码剥离、字节截断不拆码点、星号剥离、空输入。
- 事件：`TitleGenerated` 序列化 roundtrip；旧日志（v11）读取不变；新 kind 被旧读取器跳过。
- 调度：首条触发一次、单飞、重命名后不再触发、生成中重命名不覆盖、失败不重试、失败回退兜底。
- 权威链：`TitleGenerated` 覆盖兜底、`TaskRenamed` 覆盖 `TitleGenerated`。

## 14. 开放问题

1. 默认 `enabled` 是 `false`（省成本）还是 `true`（体验优先）？建议 `false`，UI 提供开关。
2. 「每条新消息重生成」（all-prompts）是否作为后续独立设计；v1 只留 cadence 字段占位。
