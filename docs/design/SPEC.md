# qcode v1 — 实现规格

> 依赖决策见 [DECISIONS.md](./DECISIONS.md)。本文件描述“怎么做”，不包含代码。
> 所有路径/前缀/命令名以常量封装，改名时只改一处。

## 1. 范围

**v1 包含**

- macOS / Linux 单机 CLI + TUI。
- 单 agent、多轮工具调用、人工审批、JSONL 持久化、崩溃恢复、对话 fork。
- 内置 10 个工具、MCP client（仅 tools）、AGENTS.md、上下文压缩。
- headless 模式（JSONL 事件流），可进 CI。

**v1 不包含**

- Windows、子 agent、skills、`web_search`、图片输入、自动更新、服务端/多租户。

## 2. 术语

| 术语 | 含义 |
|---|---|
| Session | 一次连续工作上下文，对应一个 JSONL 文件 |
| Turn | 一次用户输入到 agent 停止（无工具调用）的完整周期 |
| Item | 事件日志中的一条记录 |
| ToolCall | 模型发起的工具调用，带唯一 `call_id` |
| Approval | 针对某个 ToolCall 的决策（allow / ask / deny） |
| Workspace | `--cwd` 指定的目录；写权限的默认边界 |
| Compaction | 把较老历史折叠成摘要的过程 |

## 3. 架构

```
┌─────────────┐   events    ┌──────────────┐
│ qcode-tui   │◀────────────│              │
└─────────────┘             │  qcode-core  │──▶ qcode-providers ──▶ LLM API
┌─────────────┐   events    │  (turn loop, │
│ qcode   │◀────────────│   policy,    │──▶ qcode-tools ──▶ shell / fs
│  --json     │             │   store)     │
└─────────────┘             │              │──▶ qcode-mcp ──▶ MCP servers
                            └──────────────┘
```

- **单进程**：engine + 前端同进程；shell 与 MCP server 是子进程。
- **依赖方向**：`core` 定义 trait，`providers` / `tools` / `mcp` 实现，`cli` / `tui` 消费。
  `core` 不依赖任何具体 provider 或工具实现。
- **并发**：每 session 同一时刻只有一个 turn；turn 内只读工具可并行（上限 4），
  写操作与 shell 串行。
- **事件流**：engine 只产出事件，TUI 与 `--json` 是两种 sink；审批通过反向 channel 会合。

## 4. 事件模型

### 4.1 信封

```json
{"v":1,"seq":42,"ts":"2026-10-18T11:00:00Z","session":"01J...","turn":7,
 "type":"tool_call","data":{"call_id":"call_1","name":"read_file","arguments":{"path":"src/main.rs"}}}
```

- `seq`：从 1 开始单调递增，缺号视为损坏。
- `v`：schema 版本；加载时跑迁移函数。
- 事件不可修改；纠正只能追加新事件。

### 4.2 事件类型

| type | data | 说明 |
|---|---|---|
| `session_started` | `model, provider, cwd, version` | 文件第一条 |
| `turn_started` | `input` | |
| `user_message` | `text, attachments` | 含排队输入 |
| `assistant_message` | `blocks[], provider_raw, usage` | blocks: text / reasoning / tool_call |
| `approval_requested` | `call_id, rule_id?, reason` | |
| `approval_decided` | `call_id, decision, rule_id, scope` | |
| `tool_call` | `call_id, name, arguments` | |
| `tool_result` | `call_id, status, output, metadata` | status: ok / error / cancelled / interrupted |
| `compaction` | `trigger, summary, replaced_from, replaced_to, tokens_before, tokens_after` | |
| `turn_ended` | `status, usage, cost_usd` | status: completed / cancelled / failed |
| `error` | `kind, message, retryable` | |

### 4.3 不变量

1. 每个 `tool_call` 恰好对应一个 `tool_result`；崩溃恢复时补 `interrupted`。
2. `assistant_message` 必须带 `provider_raw`，否则视为损坏。
3. 同一 turn 内所有事件 `turn` 字段一致。
4. `tool_result` 的 `output` 超过 32KB 时，正文落盘，事件里只留截断预览与路径。

## 5. 状态机

| from | 触发 | to | 动作 |
|---|---|---|---|
| Idle | submit | Streaming | append `turn_started` |
| Streaming | 收到 tool_calls（有 ask） | AwaitingApproval | emit `approval_requested` |
| Streaming | 收到 tool_calls（全 allow） | RunningTool | 执行 |
| AwaitingApproval | allow | RunningTool | append `approval_decided` |
| AwaitingApproval | deny | Streaming | 写入 denied 结果，交回模型 |
| RunningTool | 全部完成 | Streaming | append `tool_result` |
| Streaming | 无 tool_calls | Idle | append `turn_ended` |
| Streaming | 上下文 ≥ 78% | Compacting | 摘要 |
| Compacting | 完成 | Streaming | append `compaction` |
| 任意 | Esc / 取消 | Cancelling | abort 流、杀进程组 |
| Cancelling | 清理完成 | Idle | append `turn_ended(cancelled)` |

取消语义：协作式。流式请求 drop；工具进程组先 SIGTERM，2 秒后 SIGKILL；
已产生的部分输出保留并标 `cancelled`。崩溃恢复：replay 到最后一个完整事件，
悬挂 `tool_call` 补 `interrupted`，下一轮上下文显式写明“上次被中断”。

## 6. Turn loop

1. append `turn_started`。
2. 组装上下文（§9），调用 provider 流式接口。
3. 流式过程中只向前端推 delta，不逐 token 落盘。
4. 流结束：append `assistant_message`（含 `provider_raw` 与 usage）。
5. 无 tool_calls → append `turn_ended`，回到 Idle。
6. 有 tool_calls：按只读/写分组 → 策略求值 → 必要审批 → 执行 → append `tool_result`。
7. 回到 2；每轮检查 `--max-turns` 与 `--max-cost`，超限则终止并标记失败原因。

## 7. 工具契约

统一返回：`{status, output, metadata{truncated, bytes, exit_code?, path?}}`。

| 工具 | 参数 | 副作用 | 默认审批 | 备注 |
|---|---|---|---|---|
| `read_file` | `path, offset?, limit?` | 无 | allow（workspace 内） | 带行号；二进制检测；上限 2000 行 / 32KB |
| `write_file` | `path, content` | 写 | allow（workspace 内） | 临时文件 + rename 原子写；路径校验 |
| `edit_file` | `path, old_string, new_string, replace_all?` | 写 | allow（workspace 内） | `old_string` 必须唯一，否则报错并给出候选位置 |
| `apply_patch` | `diff` | 写 | ask | 标准 unified diff；全有或全无；拒绝绝对路径与 `..` |
| `list_dir` | `path` | 无 | allow | 默认忽略 `.git`、`target`、`node_modules` |
| `glob` | `pattern, path?` | 无 | allow | 结果排序；上限 1000 |
| `grep` | `pattern, path?, glob?, context?` | 无 | allow | 内部 ripgrep；遵守 .gitignore |
| `run_shell` | `command, cwd?, timeout?` | 执行 | ask | 独立进程组；清洗环境变量；默认超时 120s |
| `todo_write` | `items[]` | session 状态 | allow | 渲染在 TUI，不进文件系统 |
| `web_fetch` | `url` | 网络 | ask | 仅 http/https；上限 100KB；域名策略 |

工具描述（进 prompt 的文本）自写，不复用任何现有产品的描述。

## 8. 策略与审批

规则是数据，判定是纯函数：

```toml
[[rules]]
id = "allow-cargo-test"
tool = "run_shell"
command_prefix = "cargo test"
action = "allow"
scope = "user"          # user | project | session
```

求值顺序：

1. 收集所有匹配规则；任一 `deny` → deny。
2. 否则取最具体的一条（约束数量多者优先；同分取更长的 `command_prefix` / `path_glob`）。
3. 无匹配：交互模式 ask，非交互模式 deny。
4. 项目配置只能产生 `deny` / `ask`，不能新增 `allow`。
5. 每次判定 append `approval_decided`，含 `rule_id`。

默认策略：workspace 内读/写 allow；workspace 外一律 ask；`run_shell`、`web_fetch`、
MCP 工具默认 ask。`--yes` 只把 ask 降为 allow，不覆盖 deny。

## 9. 上下文装配

顺序固定，保证前缀稳定以命中 prompt cache：

1. 基础 system prompt（静态、带版本号，不含时间戳）。
2. 工具定义（静态、按名字排序）。
3. `AGENTS.md`：user-level → repo root → … → cwd，越近优先级越高，带来源标注。
4. `todo_write` 的当前状态。
5. compaction 摘要（若有）。
6. 最近的历史 turn（默认保留最近 2 轮原文）。

预算：上下文窗口的 78% 触发压缩；为输出预留 20%。
压缩第一步驱逐大工具输出（替换为“已落盘，路径 …”），第二步用**同一个模型**做结构化摘要
（目标 / 已完成 / 改动文件 / 未决 / 下一步），温度 0。压缩不在工具执行中进行。
cache 断点：静态前缀末尾、压缩摘要末尾。

## 10. Provider 适配

```text
trait Provider {
    capabilities() -> Capabilities
    stream(request, cancel) -> Stream<Event>
    count_tokens(request) -> Option<usize>
}
```

`Capabilities`：`tools`、`parallel_tools`、`streaming_tool_args`、`reasoning`、
`prompt_cache`、`images`、`max_context`、`max_output`。

请求前按能力裁剪参数并 append 一条 `error/notice` 事件说明裁剪内容。
错误分类：`Auth`、`RateLimit{retry_after}`、`ContextOverflow`、`InvalidRequest`、
`Server`、`Network`、`Cancelled`、`Protocol`。

重试：仅首 token 之前；指数退避 + jitter，最多 3 次，尊重 `Retry-After`；
首 token 之后失败则终止本 turn，已生成部分标未完成，不静默重放。

## 11. MCP

```toml
[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
enabled = true
```

- 懒连接：首次调用前启动，`list_tools` 结果缓存 5 分钟。
- 工具名 `mcp__<server>__<tool>`；权限按工具粒度，默认 ask。
- server stderr 写入独立日志；server 崩溃只降级该 server，不终止 session。
- 输出同样走 32KB 截断与外置；进上下文时加来源标记。
- v1 只支持 tools；resources / prompts / sampling / elicitation 不进 v1。

## 12. 配置与凭据

路径统一 XDG（macOS 同样使用 `~/.config`、`~/.local/share`），常量封装：

| 用途 | 路径 |
|---|---|
| 用户配置 | `~/.config/qcode/config.toml` |
| 项目配置 | `<workspace>/.qcode/config.toml` |
| 数据 | `~/.local/share/qcode/` |
| 缓存 | `~/.cache/qcode/` |

优先级：CLI flags > 环境变量 > 项目配置 > 用户配置。
凭据：环境变量（`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `DEEPSEEK_API_KEY`）优先，
可选 OS keychain；永不写入项目配置；日志统一脱敏。
shell 环境清洗：只传 `PATH HOME USER SHELL LANG LC_* TERM TMPDIR` 与用户显式声明的变量；
剔除 `*_KEY`、`*_TOKEN`、`*_SECRET`、`AWS_*`、`GITHUB_TOKEN` 等。

## 13. CLI

| 命令 | 说明 |
|---|---|
| `qcode` | 启动 TUI（新会话） |
| `qc run "prompt"` | headless；prompt 也可从 stdin 读 |
| `qcode resume <id>` | 恢复会话（TUI） |
| `qc sessions list/show` | 会话管理 |
| `qc config edit/show` | 配置 |
| `qc mcp list/add/remove` | MCP 管理 |
| `qc doctor` | 自检（toolchain、路径、provider 连通性） |
| `qc sandbox exec` | 内部使用，不对外承诺 |

`run` 的关键 flag：`--json`、`--model`、`--provider`、`--approval-policy`、`--yes`、
`--max-turns`、`--max-cost`、`--timeout`、`--cwd`、`--config`。
stdout 只输出 JSONL 事件；日志与进度走 stderr。
`--json` 默认不输出 `provider_raw`，`--include-raw` 才输出。

## 14. TUI

- 布局：顶部状态栏（模型 / 上下文占用 / 本 turn 成本 / 沙箱模式），中部 transcript，
  底部输入框，审批时浮层。
- 键位：Enter 发送，Shift+Enter 换行，Esc 取消 turn，Ctrl+C 两次退出，Ctrl+O 展开工具输出。
- 工具调用默认折叠为一行（名称 + 参数摘要 + 状态 + 耗时），可展开看完整输出。
- diff 用红绿行；审批面板展示 argv、cwd、触发规则、沙箱模式。
- 渲染 60Hz 合并流式 delta，避免逐 token 重绘。
- 长 transcript 只渲染可见行（虚拟化），不能全量布局。
- 默认不捕获鼠标，保留终端原生选择/复制。
- 输入用 `tui-textarea`；CJK 宽度、IME、粘贴、多行光标纳入测试。
- 框架选型与约束见 [DECISIONS.md](./DECISIONS.md) 的 ADR-0001。

## 15. 存储

```text
~/.local/share/qcode/
  sessions/<ulid>.jsonl
  sessions/<ulid>/outputs/<call_id>.txt
  index.sqlite
  locks/<ulid>.lock
```

- 单写者：会话文件加 flock，第二个进程只能只读或拒绝。
- 写入：append + 在 turn 边界 fsync；末尾半行在加载时丢弃。
- 索引：session id、起止时间、模型、cwd、turn 数、成本；可全量重建。
- 清理：`outputs/` 随会话删除；提供 `qc sessions gc`。

## 16. 测试与评测

- 单测：策略求值、压缩裁剪、事件投影、路径校验（symlink / `..` / 大小写）。
- 集成：`MockProvider` 脚本化流式分片、工具调用、错误注入、取消，覆盖全部状态转换。
- 黄金：事件 schema 快照；TUI 关键帧 insta 快照。
- eval：见 [EVAL.md](./EVAL.md)，`eval/fixtures/workspace` + 5 条任务。

## 17. 里程碑

| 阶段 | 周 | 交付 |
|---|---|---|
| M0 | 1–2 | workspace 骨架、事件日志、Provider trait + OpenAI-compatible 流式、headless 单轮 |
| M1 | 3–5 | 工具循环（read/write/edit/shell）、策略与审批、JSONL 持久化、resume |
| M2 | 6–8 | TUI、取消、压缩、AGENTS.md、MCP client |
| M3 | 9–10 | `apply_patch`、`web_fetch`、sandbox runner（stretch）、打包 |
| M4 | 11–12 | eval fixture + 5 任务、文档、notarize |

裁剪顺序（时间不够时）：sandbox runner → `web_fetch` → 对话 fork → MCP。

## 18. 风险

- `rig-core` 升级破坏性变更：pin + 防腐层 + 适配层单测。
- 自写 loop 低估：超过 2 周未跑通则退回 `AgentRun`。
- eval fixture 工作量：M4 的关键路径，M2 起就要并行准备。
- 命名：发布前必须完成商标与占用检查。
