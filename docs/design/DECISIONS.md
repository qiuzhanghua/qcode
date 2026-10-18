# qcode — 设计决策

> 状态：定稿（v1 范围）。L0–L6 已定。日期：2026-10-18。
> 本文件是实现时的唯一决策依据；改动需追加决策记录。
> 实现规格见 [SPEC.md](./SPEC.md)，评测方案见 [EVAL.md](./EVAL.md)。

## 0. 目标

- 定位：自用为主、可开源的第二代 coding CLI。
- MVP 判据：在真实仓库完成一次多文件修改并使测试通过；作者愿意每天使用。
- 非目标：功能对齐 Codex / Claude Code；v1 不做 Windows、不做 GUI、不做多租户服务。

## 1. Clean room

采用资料白名单，细则见 [SOURCES.md](./SOURCES.md)。

- 禁止：Codex 源码；任何形态的 Claude Code 源码（含 2026 年泄漏的 npm bundle）；第三方复刻项目。
- dsh（MIT）仅作为工具使用，不作为核心 loop 的架构参照。
- prompt、错误文案、命令名、配置字段名、UI 字符串全部自创。
- 依赖必须过 `cargo-deny`；禁止 GPL/AGPL 传染（`birdcage` 因此排除）。

## 2. 形态与命名

engine-first，TUI 只订阅事件流。

| crate | 职责 |
|---|---|
| `qcode-core` | domain、turn/loop、policy、事件、存储抽象 |
| `qcode-providers` | provider 适配与流式解析 |
| `qcode-tools` | 内置工具、进程执行、sandbox runner |
| `qcode-mcp` | rmcp client 封装 |
| `qcode` | 参数、配置加载、headless 输出 |
| `qcode-tui` | ratatui 前端 |

命名：`qcode` 仅为内部代号（与 Coder Technologies 有撞名风险），发布前定名。
spec 中所有路径、环境变量前缀、crate 名抽成常量，便于整体替换；配置目录预留迁移读取。

## 3. 平台、沙箱与进程模型

- v1：macOS + Linux。Windows：v2。
- sandbox 抽象为 trait；v1 落地“无沙箱 + 审批”兜底，OS 沙箱为 stretch。
- 事实约束：macOS `sandbox-exec` 已标 deprecated，且嵌套调用返回 EPERM(71)
  → **sandbox 只在一个进程边界上套一次**。
- 进程模型：只读工具与 `write_file` 在引擎进程内执行；只有 `run_shell` 走
  `qc sandbox exec --policy … -- argv` 独立进程组。
- `write_file` 的路径校验（canonicalize、workspace 前缀、symlink、`..`）
  是安全关键代码，必须有单测。
- 主机名级出网控制在三平台都做不到 → 网络策略的边界是本地代理，v2 再引入。
- Linux 倾向 bubblewrap（Landlock 管不住 unix socket）；Ubuntu 24.04 需 AppArmor profile。

## 4. 领域模型

### 4.1 事件与存储

- append-only JSONL 是唯一真相，turn 边界 fsync。
- SQLite 只做可重建索引/投影。
- 事件结构：`{v, seq, ts, session, turn, type, payload}`。
- assistant/tool 事件保存 provider wire JSON（thinking / reasoning_content 必须可回传）。
- schema 带 version + 迁移函数。

### 4.2 状态机与中断

- 状态：`Idle / Streaming / AwaitingApproval / RunningTool / Compacting`。
- Esc 一次 = 取消当前 turn：abort 流、kill 工具进程组（TERM → 2s → KILL）、
  保留部分输出并标 `cancelled`。取消由外层 `select!` + drop stream 实现。
- 崩溃恢复：replay；悬挂 `tool_call` 标 `interrupted`，下一轮显式告知模型。
- 不支持 mid-tool resume；用户输入排队到 turn 边界；v1 无 steering。

### 4.3 回滚

- v1 只做对话 fork（从任意事件分叉），文件不动。
- 代码回滚交给用户自己的 git；事件保留 turn checkpoint marker 作为 v2 接口。

### 4.4 审批

- 三值：allow / ask / deny；deny 最高，同值下最具体规则优先。
- 规则是 TOML 数据；作用域 session / project / user。
- 每次决策写事件，含命中的 rule id（审计基础）。
- 非交互模式默认 deny，除非 `--yes` 或策略文件放行。
- 审批 UI 展示 argv、cwd、沙箱模式、触发原因。
- `always allow` 只能精确到命令前缀，不允许任意通配。

### 4.5 工具

- 内置：`read_file`、`write_file`、`edit_file`（唯一字符串替换 + 唯一性校验）、
  `list_dir`、`glob`、`grep`（ripgrep）、`run_shell`、`apply_patch`（标准 unified diff）、
  `todo_write`、`web_fetch`（只读、截断、默认需审批或域名白名单）。
- 工具总数 ≤ 10。
- shell 只用于 build/test/git/包管理。
- 输出 head+tail 截断（约 32KB），全文落盘并回传路径；结果带 exit code / truncated / bytes。
- 只读工具可并行；写操作与 shell 串行。

### 4.6 上下文压缩

- 混合：先驱逐大工具输出（占位符 + 落盘路径），再结构化摘要较老 turn
  （决策 / 改过的文件 / 待办 / 未决问题）。
- 阈值 75–80%；最近 2 轮保留原文。
- 压缩是事件；不在工具执行中压缩。
- 稳定前缀（system + tools + AGENTS.md），历史只追加，以命中 prompt cache。

### 4.7 子 agent

- v1 只定义契约（tool 接口 + 权限继承规则），不做实现。

### 4.8 评测

- 合成 fixture：`eval/fixtures/workspace`，纯 std、三 crate（core / store / cli）、
  离线、`cargo test` < 5s；每个任务在固定 commit 的快照上跑。
- 5 条任务：修 bug / 加功能 / 重构 / 跨文件 API 改动 / 修 CI，判据均为命令退出码。
- 二阶段用真实仓库（`qrlib` 或自有中等仓库），三阶段 self-hosting（qcode 改 qcode）。
- 与 MVP 判据共用。详见 [EVAL.md](./EVAL.md)。

### 4.9 Provider 能力与降级

- 每个模型一份 capability 描述符（rig catalog + 本地 override）。
- 请求前裁剪不支持的参数，并记事件说明裁剪内容。
- tool calling 不支持 → 拒绝该模型，不做文本解析兜底。
- 并行工具仅当 provider 支持且本批工具全只读。
- 不支持 cache → 退化并在 UI 显示“无缓存”。
- 图片输入 v1 不做。
- 原则：不静默降级，每次裁剪可审计。

### 4.10 MCP

- server = 显式信任的插件；装即信代码，但每次调用仍走审批。
- server 进程默认不套 FS 沙箱；记录 argv 与来源；用户可配置关闭。
- 工具名 `mcp__<server>__<tool>` 防冲突。
- 权限按工具粒度配置，默认 ask。
- 输出加来源标记；输出里的“指令”不改变任何权限。
- v1 只支持 tools；resources/prompts v2；sampling 不用；elicitation 不用于审批。

### 4.11 AGENTS.md 与 skills

- 从 cwd 向上到 repo root（`.git` 边界），越近优先级越高；user-level 最低。
- 合并时带来源标注；总预算 32KB，超出截断并提示。
- v1 不做 imports；skills 放 v2。
- UI 显示“已加载项目指令”；项目指令不能放宽权限。

### 4.12 TUI（v1）

- 必须：流式文本、工具调用行内折叠、审批面板（argv / cwd / 原因 / 沙箱模式）、
  Esc 中断、Shift+Enter 多行、滚动与复制、状态栏（模型 / 上下文占用 / 本 turn 成本）。
- diff 只做红绿行，不做语法高亮；主题 light/dark；鼠标可选。
- 不做：语法高亮、图片、分屏、插件系统。
- 流式渲染按 60Hz 合并。

### 4.13 headless

- `qc run "prompt" --json` 输出 JSONL，与 TUI 同源。
- 退出码：`0` 成功 / `1` 任务失败 / `2` 用法错误 / `3` 审批拒绝 / `4` provider 错误。
- 审批：`--approval-policy`（allow / deny / 规则文件）或 stdin 的 JSONL 协议。
- 必须支持 `--max-turns`、`--max-cost`、`--timeout`、`--cwd`。
- 非 TTY 默认按非交互处理，未知操作 deny。

### 4.14 分发

- v1 只做 GitHub Releases 静态二进制：macOS aarch64/x86_64、Linux x86_64 gnu/musl、aarch64；
  另支持 `cargo install --path`。
- macOS 必须 codesign + notarize，否则 Gatekeeper 拦截。
- 不做自动更新；brew tap、npm wrapper 放 v2。

### 4.15 测试与 CI

- 三层：① 纯逻辑单测（policy / compaction / 事件投影）；
  ② `MockProvider` 集成测试（流式分片 / 工具调用 / 错误注入 / 取消），覆盖每个状态转换；
  ③ 黄金测试（事件 JSON schema 快照 + TUI 关键帧 insta 快照）。
- CI：macOS + Linux × stable + MSRV，跑 fmt / clippy `-D warnings` / test / cargo-deny。
- 真实 provider 的 smoke test 只在夜间或手动跑，不阻塞 PR。

## 5. 技术选型

- core loop 自写，不用 `rig-agent` 的 `AgentRunner`。
- rig：只用 `rig-core`（provider、原生消息、流式事件、usage/cost），
  依赖 pin 死 `=0.44.0`，外面包一层自有 `Provider` trait 做防腐层。
- 事件日志存 provider wire JSON，不存 rig 类型；禁用 `Agent::resume` / `RUN_FORMAT`。
- rig 自带的 terminal coding agent 只允许看 API 用法示例，不作为架构与 prompt 参考。
- rmcp 3.x：stdio + streamable HTTP；v1 不支持 2024-11-05 legacy SSE；
  `call_tool` 外层自包 timeout（库没有 per-request deadline）。
- provider v1：OpenAI-compatible（DeepSeek / Ollama / vLLM）+ Anthropic 原生。
- DeepSeek 默认模型：`deepseek-flash`（2026-10-18 用户选择；可用 `--model` 覆盖）。
- 已知成本：rig 强制 `serde_json` 的 `preserve_order` + `float_roundtrip`。
- 回退条件：自写 loop 超过 2 周未跑通“多轮工具 + 审批 + 取消”，改用 `AgentRun`。

## 6. 资源与许可

- 1–2 人兼职，3 个月 MVP。
- MIT；先私有后开源。

## 7. 风险与未决

- 命名未定：发布前必须完成商标与 crates.io/npm/GitHub 占用检查。
- rig 处于 pre-1.0，每月破坏性变更；靠 pin + 防腐层控制爆炸半径。
- macOS `sandbox-exec` 已 deprecated；v1 无 OS 沙箱，安全依赖审批。
- prompt injection 无根治手段，只能靠审批、来源标记和权限最小化。
- eval fixture 的编写工作量可能被低估；它是 M4 的关键路径。
- v1 范围仍偏大；M3/M4 可按实际情况裁剪。

## 8. 决策记录

- 2026-10-18：L0–L2 锁定（本文档）。
- 2026-10-18：L3 锁定（Q17–Q21 按推荐；Q22 取 `rig-core` + 自写 loop）。
- 2026-10-18：L4 锁定（Q23–Q26 按推荐）。
- 2026-10-18：L5/L6 锁定（Q27–Q31 按推荐；评测采用合成 fixture）。

## 9. ADR

### ADR-0001：TUI 框架选 ratatui

- 状态：接受（2026-10-18）
- 背景：需要一个能承载流式 transcript、可折叠工具调用、diff、审批浮层和多行输入的终端前端。
- 候选：ratatui、cursive、iocraft、tui-realm。
- 决定：ratatui，M2 引入时 pin 精确版本。

**理由**

1. transcript 本质是事件日志的投影，immediate mode 的“每帧从状态重画”与事件驱动模型一致；
   retained mode 反而要维护一棵会跟事件流打架的视图树。
2. `TestBackend` + insta 支持关键帧快照，直接满足 SPEC §4.15 的测试要求；
   cursive / iocraft 没有同等成熟的快照方案。
3. 生态最大（`tui-textarea` 等），MIT 许可证。

**约束**

1. ratatui 只做渲染；事件循环、焦点、键位映射由 `qcode-tui` 自己实现。
2. 长 transcript 必须虚拟化，只渲染可见行。
3. 默认不捕获鼠标，保留终端原生的选择/复制。
4. 输入用 `tui-textarea`；CJK 宽度、IME、粘贴、多行光标必须有测试。
5. ratatui 的用法限制在 `qcode-tui` 内，版本 pin 死；升级当独立任务。

**后果**

- M2 增加“transcript 虚拟化”和“CJK / IME 测试”两项任务。
- M2 开始前不引入 ratatui 依赖，`qcode-tui` 保持空壳。
