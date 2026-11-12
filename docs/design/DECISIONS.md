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

- v1 只定义契约（tool 接口 + 权限继承规则）；ADR-0013 实现深度 1 的 `subagent`
  （独立 session、同 provider/policy/approver、子工具集去掉 `subagent`）。

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
- server 进程默认套 FS 沙箱：运行时目录只读+执行，写入只给 workspace 与每个 server 的私有
  HOME；凭据目录需显式声明。记录 argv 与来源；用户可显式关闭（见 ADR-0066）。
- 工具名 `mcp__<server>__<tool>` 防冲突。
- 权限按工具粒度配置，默认 ask。
- 输出加来源标记；输出里的“指令”不改变任何权限。
- v1 只支持 tools；resources/prompts v2；sampling 不用；elicitation 不用于审批。

### 4.11 AGENTS.md 与 skills

- 从 cwd 向上到 repo root（`.git` 边界），越近优先级越高；user-level 最低。
- 合并时带来源标注；总预算 32KB，超出截断并提示。
- v1 不做 imports；skills 提前实现，格式与边界见 [SKILLS.md](./SKILLS.md) 与 ADR-0004。
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
- rmcp `=3.9.0`（ADR-0067 修正了原先的 `=3.2.0`）：stdio + Streamable HTTP，2024-11-05 legacy
  SSE 仍不做。`call_tool` 外层自包 timeout（库没有 per-request deadline）。
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
- 2026-10-18：M2 开工，pin `ratatui = "=0.29.0"`（与 `tui-textarea 0.7` 的 `^0.29`
  配套，避免同进程两个 ratatui）；ratatui 0.29 仍依赖的 `paste` 已被标记 unmaintained，
  在 `deny.toml` 记录豁免，升级到 0.30 + 新版 tui-textarea 时一并撤销。

## ADR-0002：MCP 客户端（2026-10-19）

**背景**：M2-7 要接 stdio MCP server。SPEC 原先写“懒连接”，但 `tools/list` 的结果决定了
发给模型的工具定义，连接无法推迟到第一次 `tools/call`。

**决定**

1. 用 `rmcp = "=3.2.0"`，只开 `client` + `transport-child-process`。
   **修正（2026-11-11）**：版本已是 `=3.9.0`（见 ADR-0067），features 增加
   `transport-streamable-http-client-reqwest`；「只 stdio」的前提见 ADR-0067 §1。
2. 在构建 registry 时串行连接所有 enabled server，`tools/list` 后立即注册；握手上限 20s。
   失败只 `warn` 并跳过该 server（spawn / 握手 / list 都是）。
3. 每 server `timeout_secs`（默认 120）同时约束 `tools/list` 与 `tools/call`；取消走
   `CancellationToken`，两者都映射成 `tool_result`（error / cancelled）。
4. MCP 工具用 `ApprovalClass::External`，默认 ask；规则按完整工具名精确匹配。
5. stderr 每 server 一个日志文件；输出加 `[mcp:<server>]` 并做 32KB 截断。

**后果**

- 启动延迟随 enabled server 数量线性增长。后续若要并行连接，只改 `connect_all`，
  工具契约不变。
- ~~`tools/list` 不缓存 TTL：进程生命周期内只列一次；长会话里 server 动态增删工具需重启 qc。~~
  **2026-11-11 作废**：2026-07-28 用 `ttlMs` / `cacheScope` 规定缓存寿命，`list_changed` 通知
  要求立即失效，「只列一次」不再是可选实现而是协议违规。见 ADR-0067 §5。
- `rmcp` pre-1.0，升级必须连同 `call_tool` 返回类型与工具生命周期一起评估。

## ADR-0003：并行只读工具（2026-10-19）

**决定**

1. 只并发 provider 同一批中**相邻**的只读调用（`ApprovalClass::Read`），上限 4；写、shell、
   MCP（`External`）在原位置单独执行。相邻而不是全批并发，保证读不会跨过同批写入。
2. 整批先规划（策略与审批）再执行；`tool_result` 按调用顺序落盘，完成顺序不影响事件流。
3. 取消共享 turn token；并行段保留已产生的输出，未执行的调用补 `cancelled`。

**后果**：混合批的墙钟收益只来自连续读段；换来的是不引入跨写读的竞态。

## ADR-0004：Agent Skills（2026-10-19）

**背景**：原计划把 skills 放到 v2；用户要求先于 M3 实现。格式采用公开的
[Agent Skills specification](https://agentskills.io/specification)，不参考任何竞品实现。

**决定**

1. 发现根：`~/.config/qcode/skills`（user）与 `<workspace>/.qcode/skills`（project）；
   同名 project 覆盖 user。非法 / 超限 / symlink 的 skill 跳过并 `warn`。
2. system prompt 只放 `name` + `description` 的索引（≤8KiB）；正文由 `skill` 工具按需加载，
   附件通过 `skill(name, file=...)` 读取。这就是渐进披露，避免把全部 skill 内容塞进上下文。
3. frontmatter 只解释 `name` / `description`，支持单行、引号、`|`、`>`；`allowed-tools`
   被刻意忽略，skill 不能放宽审批。`SKILL.md` 上限 32KiB，附件读取上限 256KiB / 输出 32KiB。
4. `skill` 工具是 `ApprovalClass::Read`；附件路径 canonicalize 后必须仍在 skill 目录内，
   目录与文件都拒绝 symlink。
5. 配置：`[skills] enabled = true`（默认）；关闭时完全不发现、不注册工具。

**后果**

- 不使用 skill 的 turn 只付索引 token；使用时要多一次 `skill` 工具往返。
- 用户级 skill 在 workspace 外，只能由 `skill` 工具读取，`read_file` 的 workspace 边界不变。
- 暂不支持 imports、远程安装、slash command 与自动激活；`tools/list` 式的动态刷新也不需要。

## ADR-0006：可配置 skill roots（2026-10-19）

**背景**：用户已有一份 DSH 风格的 skills 目录（`/Users/q/space/skills/skills`），qcode 只扫
`~/.config/qcode/skills` 与 `<workspace>/.qcode/skills`，所以 `dev-env-inventory` 在 TUI 里
不可见；把真实 skill 复制两份也无法长期维护。

**决定**

1. `[skills] roots = ["..."]` 增加额外发现根，优先级 user → config roots（按列出顺序）→
   project；支持 `~/`，相对路径按 workspace 解析。
2. 新增 `qc skills list`：打印 name、来源、目录、附件数、description，以及没有发现时检查过
   的根；`enabled = false` 时直接说明。
3. `SKILL.md` 上限从 32KiB 提到 64KiB：真实多语言 skill（如 40KB 的 `send-to-filehelper`）
   不应因为字节数被静默跳过；正文仍由 `skill` 工具按需加载。
4. 仍拒绝单个 symlink skill 目录；要复用外部目录，用 `roots` 或 symlink 整个根目录。

**后果**

- 多份 skill 来源可以共存，project 仍可覆盖任意来源。
- 64KiB 的正文最多约 1.6 万 token，是否加载由模型按 description 决定。

## ADR-0005：apply_patch、web_fetch 与 sandbox（2026-10-19）

**决定**

1. `apply_patch` 接受标准 unified diff（create / delete / modify），整包先解析并计算；
   任一 hunk 失败就不落盘。写入用 staging + 备份 + rename，尽量把 I/O 失败也收敛成回滚。
   路径走 `path::resolve`，拒绝绝对路径、`..`、rename 与 symlink 逃逸；默认 ask
   （`ApprovalClass::BulkWrite`）。
2. `web_fetch` 只支持 http(s)，15s 超时、5 次 redirect、100KiB 流式截断；只返回文本类
   内容，拒绝 URL 内凭据。默认 ask（`ApprovalClass::Network`），不注入凭据。
   **不做 SSRF 白名单**：引擎没有可靠的出网边界，审批提示里展示完整 URL 是当前的控制手段；
   v2 的网络代理边界再补域名策略。
3. sandbox runner 继续作为 stretch，不在本版实现。macOS `sandbox-exec` 已 deprecated 且
   嵌套 EPERM；Linux bubblewrap/Landlock 各有取舍。v1 的执行边界仍是审批 + workspace
   路径封闭 + `run_shell` 的进程组/环境清洗/超时/取消。
4. 打包沿用现有 `build-for-release` 矩阵；M3 只验证门禁与 `--list`。

**后果**

- `apply_patch` 不支持 rename / mode 变更 / 二进制 patch；需要时用 `write_file` 或 shell。
- `web_fetch` 可能访问内网地址，安全上依赖用户审批；模型能看到最终 URL 与状态。
- `todo_write` 与 `web_search` 不在 M3 范围。

## ADR-0007：TUI slash commands（2026-10-19）

**背景**：想在不离开 TUI、也不消耗模型调用的前提下检查 skill 发现与正文；CLI 的
`qc skills list` 每次都要退出 TUI。

**决定**

1. 以 `/` 开头的输入由 `App::handle_key` 本地处理，不进入 engine，也不写事件日志：
   - `/skills`：列出 `name [source] files=N` 与 description；空集合时提示跑 `qc skills list`。
   - `/skills <name>`：显示来源目录、附件清单与正文，正文用独立的 `Block::Skill` 渲染。
   - 其他命令只提示 `unknown command`。
2. 命令在 turn 运行期间也可用；普通文本提交仍受 running 状态限制。
3. TUI 拿到的是 `SkillView` 快照（main 从 `SkillSet` 投影），不把 `qcode-tools` 依赖塞进
   `qcode-tui`；修改 skill 后仍需重启 qc。

**后果**

- 以 `/` 开头的正常提问会被当成命令；目前没有转义，必要时改写开头。
- 命令集保持最小，不做 `/model`、`/clear` 等；需要时按同样机制扩展。

## ADR-0008：兼容 `npx skills` 的全局目录（2026-10-19）

**背景**：`npx skills list -g` 在 `~/.agents/skills` 维护 45 个全局 skill，qcode 只扫
`~/.config/qcode/skills` 与 workspace，所以 `qc skills list` 看不到它们。

**决定**

1. 默认发现 `~/.agents/skills` 与 `~/.config/agents/skills`（来源 `global`）；项目级增加
   `<workspace>/.agents/skills`；qcode 专属 `<workspace>/.qcode/skills` 仍最高。
2. 优先级 global → user → config → project(`.agents`) → project(`.qcode`)，同名后加载者胜。
   仍拒绝单个 symlink skill 目录；`npx skills` 安装的是实体目录，不受影响。
3. system prompt 的 skills 索引上限从 8KiB 提到 16KiB：45 个全局 skill 的
   name + description 合计约 8.7KiB，8KiB 会截掉最后几个。
4. 不扫描 `~/.claude/skills`：它只是指向 `~/.agents/skills` 的 symlink，且属于特定产品目录。

**后果**

- 默认启动会带 45 个全局 skill 的索引（约 4k token）；不需要时可在 config 中禁用 skills，
  或把 `roots` 收窄（未来可加 `exclude`）。
- `qc skills list` 与 TUI `/skills` 直接显示 global 来源，和 `npx skills list -g` 对齐。

## ADR-0009：`/<skill-name>` 直接激活（2026-10-19）

**背景**：模型是否调用 `skill` 工具不可控；`/skills` 只能查看，不能把 skill 指令交给模型。

**决定**

1. TUI 中任何 `/<name>` 若匹配已发现的 skill，构造一条 user message：
   `[skill:<name>]\n<正文>\n\n<task 或默认指令>`，交给正常的 `run_turn`；不经过 `skill`
   工具，也不改 system prompt。`/skills`、`/skills <name>` 仍只读。
2. TUI 的 `UserMessage` 投影识别 `[skill:` 前缀，只显示 `activated skill <name>` 一行，
   避免正文刷屏；事件日志保存完整 user_message，可审计、可 replay。
3. `qc run --skill <name>` 是 headless 等价物；未知名字在创建 session 之前失败并列出候选。
4. 激活是一次 user message，不是一次性 system 注入：正文留在历史里，后续 turn 仍可见，
   直到 compaction；这符合“激活后继续按它工作”的预期。
5. 审批不变：skill 里的 shell 命令仍按正常规则 ask/deny；skill 不能放宽权限。

**后果**

- 正文会占上下文（最大 64KiB）；长 skill 可能加速触发 78% 压缩。
- 以 `/` 开头的普通提问仍会被当成命令；要发字面 `/` 需改写。
- 若将来需要“只对一轮生效”，再增加显式 `/use --once` 或 system 注入模式。

## ADR-0010：sandbox 抽象与实验性 sandbox-exec（2026-10-19）

**背景**：M3-3 把 OS 沙箱列为 stretch，v1 只有“审批 + workspace 路径封闭”。后续要求把
stretch 往前推，但 macOS `sandbox-exec` 已 deprecated、嵌套调用 EPERM，Linux bwrap/Landlock
也无法在 macOS 宿主验证。

**决定**

1. `qcode-tools::sandbox` 定义 `Sandbox` trait 与两种模式：`none`（默认）与
   `sandbox-exec`（仅 macOS）。`run_shell` 经 trait 构造命令，构造失败即工具失败，绝不
   回退到未沙箱命令。
2. `sandbox-exec` 由 `[sandbox] mode = "sandbox-exec"` 显式开启（config 或 TUI 共用），
   启动时用同一 profile 跑 preflight；失败则 `qc` 拒绝启动，`qc doctor` 报 fail。
3. Profile：`deny default`；只读 `/`、系统目录与 workspace，只写 workspace、临时目录、
   `CARGO_HOME`/`RUSTUP_HOME`/`CARGO_TARGET_DIR`；`network = true` 才 allow 网络。
   `run_shell` 仍是独立进程组，取消/超时路径不变。
4. Linux bubblewrap/Landlock 不在本次实现：无法在当前平台验证，且 Ubuntu 24.04 需要
   AppArmor profile；`qc sandbox exec` 仍不对外提供。

**后果**

- 默认行为不变：`mode = "none"`，审批仍是主边界。
- 在已有沙箱内（CI 容器、DSH runner）启用会因嵌套 EPERM 而启动失败；这是有意的 fail
  closed，不会静默降级。
- Profile 仍是实验性：mach 服务、特殊工具链可能需要追加 allow；不把它当作完整安全边界。
- 非嵌套 macOS 上已验证 workspace 内可写、workspace 外被拒、preflight 通过。

## ADR-0011：Linux bwrap 后端（2026-10-19）

**背景**：ADR-0010 只落地 macOS `sandbox-exec`。Linux 侧倾向 bubblewrap（Landlock 管不住
unix socket），但当前开发机是 macOS，无法本机运行 bwrap。

**决定**

1. `SandboxMode::Bwrap`；参数构造做成与平台无关的纯函数 `bwrap_args`，可在 macOS 单测，
   Linux 后端只负责拼 `Command` 与 preflight。
2. 最小只读根：`--ro-bind` 系统目录（`/usr /bin /sbin /lib /lib64 /etc /opt /sys /run`，
   不存在的自动跳过）+ `--proc` + `--dev`；workspace 读写 bind；`CARGO_HOME`/`RUSTUP_HOME`/
   `CARGO_TARGET_DIR` 读写 bind；`--tmpfs /tmp` 在 workspace 位于 `/tmp` 下时跳过，避免
   tmpfs 覆盖 bind 源；`TMPDIR=/tmp`。
3. 隔离：`--unshare-user/pid/ipc/uts/cgroup-try`，`network = false` 时额外 `--unshare-net`；
   `--die-with-parent --new-session` 保持取消/超时能回收整棵树。
4. preflight 用同一套参数运行 `/bin/true`；`bwrap` 缺失、userns 被 AppArmor 限制或 bind
   失败都拒绝启动。Linux 运行时测试在 preflight 失败时跳过；Ubuntu CI 安装 bubblewrap 后
   执行。

**后果**

- Ubuntu 24.04 默认限制 unprivileged user namespaces，需 `sysctl
  kernel.apparmor_restrict_unprivileged_userns=0` 或为 bwrap 提供 AppArmor profile；
  `qc doctor` 会直接报出 preflight 的错误。
- 只读根不含 `/` 与用户 home：读取范围比 macOS profile 更窄；需要额外路径时再扩参数。
- 仍未实现 Landlock；`qc sandbox exec` 不对外提供。

## ADR-0012：Linux Landlock 后端（2026-10-19）

**背景**：bwrap 依赖外部二进制与 unprivileged user namespaces（Ubuntu 24.04 需 AppArmor
放行）。Landlock 自 Linux 5.13 起是非特权进程可用的内核 LSM，适合做无依赖的文件系统收敛。

**决定**

1. `SandboxMode::Landlock`；直接使用 syscall 444/445/446（`landlock_create_ruleset` /
   `add_rule` / `restrict_self`），不引入新 crate。
2. ruleset 在父进程构建；`run_shell` 的 `pre_exec` 中只做 `prctl(PR_SET_NO_NEW_PRIVS)` +
   `landlock_restrict_self` 两个原始系统调用（fork 后不分配内存），因此只限制子进程，
   qc 本身与其它工具不受影响。
3. 规则：`/usr /bin /sbin /lib /lib64 /etc /opt /sys /proc /run /dev` 只读，常用设备
   （`/dev/null`、`/dev/urandom` 等）读写，workspace、`/tmp`、`/var/tmp` 与 Cargo/Rustup
   目录读写；`handled_access_fs` 取当前 ABI 支持的全部文件系统位（ABI 1/2/3/5 递增）。
4. preflight 用 `/bin/true` 走同一 `pre_exec`；内核不支持则拒绝启动。CI 在 Ubuntu 上直接
   跑 Landlock 运行时测试，失败（内核过旧/被禁用）则跳过。
5. 网络不限制：Landlock ABI 4 仅覆盖 TCP bind/connect，且需要额外请求网络权限；需要网络
   隔离时用 `bwrap`。

**后果**

- 优点：无外部依赖、无需 userns，workspace 外写入在 fork 后即被内核拒绝。
- 限制：只约束文件系统；不能像 bwrap 那样隔离 `/tmp`、PID、网络与挂载。`qc doctor`
  会显示 `Landlock filesystem confinement (network not restricted)`。
- `handled_access_fs` 包含当前内核支持的全部位，旧内核会少管一些新访问类型；preflight
  只保证按当前 ABI 成功应用，不保证未来 ABI 的语义。

## ADR-0013：subagent（深度 1）（2026-10-19）

**背景**：DECISIONS §4.7 只定义了 tool 接口与权限继承契约。主 agent 的上下文会被子任务的
中间输出占满；需要一个隔离的子会话，只把最终答案带回父对话。

**决定**

1. `qcode-tools::subagent` 定义 `SubagentRunner` trait 与 `subagent` 工具；`qcode::subagent`
   实现 runner，拥有子 `SessionStore`、`Arc<dyn Provider>`、子 `ToolRegistry`、`Policy` 与
   `Arc<dyn Approver>`。
2. 子工具集 = 父工具集去掉 `subagent`，因此深度固定为 1，不能递归；子 agent 与父共用
   workspace、provider、policy 和 approver，但**看不到父对话**（无 AGENTS.md / skills 索引）。
3. 每次调用创建独立 session（独立 JSONL）。TUI 通过 `TuiMsg::Child` 把子事件渲染成嵌套
   进度块（`↳` 文本 / 工具行，不改父状态机）；块按子 session id 分组，并绑定到父
   `subagent` 工具行（显示 `N nested`）。Ctrl-B 切换最近一组，`/subagents [n]`
   列出/切换任意组。headless 不转发，父只收到工具结果：最终文本 + 子 session id +
   turn 数。空结果用占位文本。
4. 参数：`task` 必填；`max_turns` 默认 20、上限 40。`ApprovalClass::External`（默认 ask、
   串行）；子工具调用仍逐个走策略与审批，权限不能放大。
5. 子会话的 usage/cost 通过 `SubagentOutcome` → `ToolOutput` → engine 汇总进父 turn；
   `turn_ended.usage/cost_usd` 与 TUI 成本包含子 agent。子 session 仍独立记录自己的 usage。
   `--max-cost` 在子 agent 返回后的下一次 provider 响应检查时生效。
6. TUI 的审批提示改为由 `ApprovalRequest`（tool/summary/reason）驱动；否则子 agent 的
   审批没有父 transcript 行可查，用户看不到提示内容。

**后果**

- 子 agent 适合“调研 / 独立改动”，任务描述必须自包含；子 agent 不能请求父对话补充。
- 父 turn 的 usage/cost 已包含子 agent；需要拆分时看子 session 的 `turn_ended`。
- TUI 的嵌套进度只读：子 `turn_ended` 不会结束父 turn，子 `session_started` 不会覆盖父
  provider/model；完整事件仍以子 session 为准。
- 深度 1 是刻意的：避免递归成本与审批套娃；需要更深时再加显式 `depth` 预算。

## ADR-0014：resume 与对话 fork（2026-10-19）

**背景**：SPEC §1 把“对话 fork”列入 v1，§4.3 规定 v1 只做对话 fork、文件不动。此前引擎已有
`SessionWriter::resume`，但没有用户入口。

**决定**

1. `qc run --resume <id>` 在已有 session 上追加一个 turn；不重复写 `session_started`。
   `qc resume <id>` 在 TUI 里恢复：先 replay 已有事件，再按正常 `run_turn` 追加。
2. `qc sessions fork <id> [--at SEQ]` 把 seq ≤ 边界的 events 复制到新 session：
   新 ULID、`session` 改写、`seq` 从 1 重新编号，`ts`/`turn`/内容保持；默认复制全部。
   边界必须落在已有 seq 上，否则报错。
3. fork 只是会话分叉：不改 workspace 文件、不复制 outputs；文件回滚仍交给 git。
4. 新 session 可继续 `--resume` 或 TUI resume；原 session 不受影响。

**后果**

- 分叉后的 seq 与 session 与原日志不同，便于独立继续；`sessions list` 会看到两条记录。
- 当前没有交互式“从某条消息分叉”的 TUI 命令，边界用 `--at` 指定；需要时再加 UI。
- 尚未处理悬挂 `tool_call` 的 `interrupted` 标记（归入存储/崩溃恢复项）。

## ADR-0015：存储可靠性与 gc（2026-10-19）

**背景**：SPEC §15 要求单写者、turn 边界 fsync、清理与崩溃恢复；此前只有 append +
torn-line 丢弃，`interrupted` 状态从未写入。

**决定**

1. `SessionStore::lock` 用 `locks/<id>.lock` + `flock(LOCK_EX|LOCK_NB)`；`SessionWriter`
   持有锁直到 turn 结束。第二个写者得到 `StoreError::Locked`，只读命令（`sessions
   list/show`）不受影响。
2. `append` 在写入 `turn_ended` 后 `sync_all`；其余事件只 `flush`，与“turn 边界持久化”
   一致。
3. `SessionWriter::resume` 的顺序是 lock → `repair_interrupted` → load；repair 为每个
   没有对应 `tool_result` 的 `tool_call` 追加 `interrupted` 结果（保留原 turn），幂等。
   TUI resume 复用同一 repair，因此 replay 能看到中断的工具。
4. `qc sessions gc --older-than-days N [--yes]`：按最后事件 `ts` 选过期 session；
   默认 dry-run，`--yes` 删除 JSONL、`sessions/<id>/` 与对应 lock。`delete_session`
   也清理 per-session 目录。

**后果**

- 仍未实现 `index.sqlite`，`sessions list` 依旧全量扫描；大数量下会慢。
- 仍未生成 `outputs/<call_id>.txt`，`ToolMetadata.full_output_path` 保持为空。
- 锁是进程级 advisory lock；NFS 等文件系统上的行为取决于底层 flock 支持。
- repair 假设 `tool_call` 事件自身是执行记录；如果崩溃发生在 assistant_message 与
  tool_call 之间，还没有可补的结果。

## ADR-0016：Provider 重试与能力门控（2026-10-19）

**背景**：SPEC §10 要求首 token 前重试（最多 3 次、指数退避 + jitter、尊重 `Retry-After`），
且并发只读工具只在 provider 支持时启用。此前 `stream()` 只调用一次，`parallel_tools`
能力被忽略。

**决定**

1. `collect_stream` 拆成重试外壳 + `collect_once`：仅 `RateLimit` / `Server` / `Network`
   在**首 token 之前**重试，最多 3 次；`Retry-After` 优先，否则 250ms 起指数退避加
   subsecond jitter。退避期间收到取消立即按 cancelled 结束。
2. 一旦收到 text / reasoning / tool-call 增量或完整工具调用，就不再重试；失败直接
   返回错误并终止本 turn。当前实现不会把已生成的部分写成 `assistant_message`（后续可补
   “partial + incomplete”事件）。
3. `execute_calls` 读取 `provider.capabilities().parallel_tools`；为 false 时只读工具也按
   调用顺序串行执行。`MockProvider` 默认声明完整能力，测试可用 `with_capabilities`
   降级。
4. 请求参数裁剪与 token 计数仍未实现：当前请求没有需要按能力删除的可选参数。

**后果**

- 网络抖动/限流在首 token 前可自愈；流开始后不会静默重放，避免重复工具调用。
- 非并行 provider 的批量只读调用会变慢，但语义正确。
- 重试只记 `tracing::warn`，不写事件日志；需要审计时再补 `error(retry)` 事件。

## ADR-0017：path_glob、scope 与项目配置（2026-10-19）

**背景**：SPEC §8 的规则要求 `path_glob` 与 scope；§12 要求 CLI > env > project > user。
此前规则只有 tool/command_prefix，且只有一份配置文件。

**决定**

1. `Rule` 增加 `path_glob`（匹配 `path` 参数）与 `scope`（`user|project|session`，默认
   user）。求值仍是 deny 优先；否则按（约束数、scope 窄度、command_prefix 长度、
   path_glob 长度）取最大。
2. `ApprovalDecided.scope` 现在来自命中的规则；无规则命中/默认策略仍记 `session`。
3. 新增 `<workspace>/.qcode/config.toml`：叠加到用户/显式配置上，合并 providers、
   pricing、MCP servers、skills roots 与 approval rules。项目规则强制 project scope，
   `allow` 降级为 `ask`。
4. `--approval-policy` 的规则标记为 session scope，仍替换（而非追加）配置规则。

**后果**

- `path_glob` 只作用于带 `path` 参数的工具；`apply_patch` 不接受该参数，因此不会命中。
- 项目配置的 `context` / `sandbox` 段本轮不合并（避免无法区分“未设置”与默认值）。
- 项目可覆盖 provider/pricing 仍可能改变行为；如需更严格，后续再限制可覆盖字段。
- `Scope` 只写进审批事件，不改变规则存储位置。

## ADR-0018：config / mcp 管理命令（2026-10-19）

**背景**：SPEC §13 列出 `qc config edit/show` 与 `qc mcp list/add/remove`；此前只能手改
TOML。

**决定**

1. `qc config show` 打印合并后的有效配置：provider（只显示 `api_key_env` 名称）、context、
   sandbox、skills、审批规则与 MCP servers。密钥从不落盘，因此无需额外脱敏。
2. `qc config edit` 解析 `--config` / `QCODE_CONFIG` / `~/.config/qcode/config.toml`，
   创建父目录与空文件后交给 `$EDITOR`（退回 `$VISUAL`、`vi`/`notepad`），退出后再解析
   一次并报告错误。
3. `qc mcp add NAME --command CMD [--arg ...] [--disabled] [--timeout-secs N]` 与
   `qc mcp remove NAME` 操作**用户配置**文件：用 `toml::Table` 读改写，写前解析成
   `Config` 并跑 `McpConfig::validate`；未知/重复 server 报错。
4. `qc mcp list` 读取合并配置（含项目配置），只读展示。

**后果**

- add/remove 会重写整个 TOML 文件，注释与键顺序丢失；复杂编辑仍用 `config edit`。
- 暂不支持把 server 写到项目配置（没有 `--project`）；避免仓库自动获得外部工具。
- 暂不支持 `env` 字段（MCP 配置 schema 目前没有该字段）。
- `config edit` 依赖交互式 `$EDITOR`，CI 里不应调用。

## ADR-0019：TUI diff 着色、审批细节与快照（2026-10-19）

**背景**：SPEC §14 要求 diff 用红绿行、审批面板展示 argv/cwd/触发规则；§16 要求 TUI
关键帧快照。此前审批只有 tool/summary/reason/sandbox，工具输出全部灰色，渲染测试只有
字符串断言。

**决定**

1. `ApprovalRequest` 增加 `argv`（shell command 或 `path …`）与 `cwd`（shell 的有效
   cwd），TUI 审批浮层展示 rule / argv / cwd；`InteractiveApprover` 的 stderr 提示也带上。
2. `apply_patch` / `edit_file` 的展开输出按行着色：`+++`/`+` 绿、`---`/`-` 红、`@@` 青、
   `diff`/`index` 粗体；其它工具保持灰色。
3. 审批浮层高度从 40% 提到 60%，避免新增行把 `[y]/[n]` 挤出屏幕。
4. `qcode-tui` 引入 `insta` dev-dependency，`render::transcript_snapshot` 固化一帧
   80×24 的 transcript；更新快照用
   `INSTA_UPDATE=always cargo test -p qcode-tui --test render transcript_snapshot`。

**后果**

- 快照对布局/文案变化敏感，是有意的回归信号；UI 改动需要一起更新 `.snap`。
- argv 对超长命令可能换行；浮层使用 `Wrap`，极端长度仍可能挤占按钮空间。
- diff 着色是行首启发式，不是语法分析；非 diff 工具的 `+`/`-` 行不受影响。

## ADR-0020：大输出落盘与 SQLite 索引（2026-10-19）

**背景**：SPEC §15 要求 `sessions/<ulid>/outputs/<call_id>.txt` 与 `index.sqlite`；此前
`full_output_path` 永远为空，`sessions list` 全量解析每个 JSONL。

**决定**

1. `ToolOutput` 增加 `full_output: Option<String>`；各工具在截断时把原文附上
   （shell、read/list、glob/grep、MCP、skills、subagent）。只有
   `metadata.truncated && full_output.is_some()` 时引擎才写文件，避免无谓 IO。
2. 引擎在写 `tool_result` 前调用 `SessionStore::write_output`，路径为
   `sessions/<id>/outputs/<call_id>.txt`；call_id 里的 `/`、`..` 等被替换/裁剪，最长
   120 字符，防止模型控制的 id 逃出 outputs 目录。写入失败只 warn，不影响工具结果。
3. `index.sqlite` 在 `<data>/index.sqlite`，表 `sessions(id, started_at, model, provider,
   cwd, version, turns, last_seq, cost_usd)`。`append` 在 `session_started` 时 upsert，
   在 `turn_ended` 时累加 cost/turns/last_seq；索引错误绝不失败 session 写入。
4. `list` 优先读索引；索引缺失、表损坏或行数与 JSONL 数量不一致时，扫描全部日志并重建。
   `delete_session` / `gc` 同步删索引行。
5. 旧索引没有 `cost_usd` 列时，`open_index` 尝试 `ALTER TABLE ... ADD COLUMN`，失败
   （已存在）被忽略。

**后果**

- 大输出仍会在工具返回时短暂驻留内存；截断上限由各工具控制，最大是 shell 的 1MiB 量级。
- `gc` 删除 `sessions/<id>/`，因此 outputs 一并清理。
- 索引是缓存，不是真相来源；删除后自动重建。当前顺序按 id（ULID 时间序），未单独建
  started_at 索引。
- 未做 schema 版本表；后续列变更继续用 `ALTER TABLE` 幂等补列。

## ADR-0021：macOS notarize 占位（2026-10-19）

**背景**：M4 要求 notarize，但当前环境既没有 Developer ID Application 证书，也没有
Apple ID / team / app-specific password。

**决定**

- 增加 `notarize-macos.sh`：对每个产物做 `codesign --options runtime --timestamp`、
  打 zip、`xcrun notarytool submit --wait`。所有凭据从环境变量读取，缺失即报错退出。
- 脚本不接入 `build-for-release`；打包与签名/公证保持分离，避免无凭据时阻塞普通构建。
- 裸 Mach-O 不能 `stapler`，脚本不假装离线票据可用；需要时把 CLI 包进 `.app`/`.pkg`/
  `.dmg` 再扩展。

**后果**

- M4 的 notarize 仍是“流程占位”，不算完成；需要真实证书后跑一次并记录 ticket。
- Windows 代码签名（Authenticode）同样未做，v1 不包含。

## ADR-0022：v1 收尾四项（2026-10-19）

**背景**：v1 剩余边角：`provider_raw` 默认输出、doctor 无连通性探测、压缩未驱逐大输出、
项目配置不合并 context/sandbox。

**决定**

1. `--json` 打印事件前删除 `data.provider_raw`；`--include-raw` 保留。磁盘日志始终完整，
   因此 replay/调试不受影响。
2. `qc doctor` 对非 mock provider 发一条极小的 `Reply with the single word ok.` 请求，
   10s 超时；Auth/InvalidRequest 记 fail，其它 provider/网络错误记 warn，超时记 warn。
   这会消耗一次极小请求并可能产生费用。
3. 压缩的摘要输入先把 >4k 字符的旧 `tool_result` 截成前 1k + 省略标记；保留的最近 2 轮
   不受影响。事件日志本身仍保留完整输出（大输出另有 `outputs/` 全文）。
4. 项目配置只有显式出现 `[context]` / `[sandbox]` 时才覆盖用户值；通过解析 TOML 文档
   判断 section 是否存在，避免把“未设置”当成默认值覆盖。

**后果**

- `provider_raw` 默认不出现在 CI 的 JSONL 里，需要时显式加 flag。
- doctor 不再是纯离线命令；离线环境会显示 connectivity warn，但不会因此失败。
- 压缩摘要对旧工具输出是有损的；模型仍可通过 `outputs/` 路径或重新读取文件获得细节。
- 项目配置仍不能放宽审批；sandbox 项目覆盖仍需人工审计。

## ADR-0023：统一网络出口与本地代理（2026-10-19）

**背景**：v2 决定所有网络统一走本地代理，并要求域名策略与审计。应用级代理无法覆盖
shell/MCP 的任意 socket，需要明确边界。

**决定**

1. qcode 启动进程内 HTTP CONNECT 代理，只监听 `127.0.0.1:0`；每个 qcode 进程一个
   session token，放在代理 URL userinfo。
2. provider、`web_fetch`、`web_search` 的 HTTP client 必须经代理，失败关闭，不回落直连；
   代码只允许通过 `net::client()` 构造 client。
3. shell/MCP 注入 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`，属于
   **尽力而为**，不是硬边界。v2.0 的 MCP 使用 session 共享 token 与 allowlist，简化实现；
   每 server 隔离留到后续版本。
4. allowlist 以 host:port 为单位；未知域名交互式 ask、非交互 deny；批准只对当前 session
   生效；项目配置只能收紧。HTTPS 只检查 CONNECT host，不做 TLS 中间人。
5. 解析到 private/loopback/link-local 的地址默认拒绝（`allow_private` 才放行），防 SSRF。
6. 审计写 `logs/network.jsonl`；交互式网络决策写入 session 事件，保证 replay 可审计。
7. OS 级 hard mode 放 v2.2：macOS Seatbelt 先做；Linux netns + relay 调研；Windows
   WFP/AppContainer 放 v3。

**后果**

- 能阻止模型/工具误连未知域名，但不能阻止 `--noproxy`、raw socket、DoH 等本地绕过；
  威胁模型是“防误操作”，不是防恶意本机代码。
- shell/MCP 不遵守代理时策略失效；文档与 `qc doctor` 必须提示。
- 代理不解决 DNS 泄漏；DNS 由代理进程解析，但仍可能有系统级解析。
- 服务端/多租户不在本期；未来若需要集中审计，再把代理拆成 `qcode-proxyd`。

**被否方案**

- 系统代理/全局 DNS 拦截：影响整机，权限与可移植性差。
- 仅工具内 allowlist、不起代理：无法统一审计与多工具复用。
- v2.0 直接上 OS 级强制：Windows 成本过高，会拖垮 v2.0。


### ADR-0023 实现状态（2026-10-19）

- 新增 `qcode-net`：loopback HTTP CONNECT 代理、session token、host allowlist、
  wildcard/端口规则、SSRF 私网拦截、JSONL 审计；provider / `web_fetch` /
  `web_search`（后续）统一用 `Network::client()`。
- shell 与 MCP 子进程注入 `HTTP_PROXY/HTTPS_PROXY/ALL_PROXY/NO_PROXY`；
  MCP 仍然共用 session token/allowlist（简化实现）。
- `unknown = "ask"` 目前由 `web_fetch` 的工具审批兑现：审批通过后把 host 加入
  session allowlist；shell/MCP 没有交互式 ask，未知 host 直接拒绝。
- 未做：per-session token（ACP 多会话）、shell/MCP 的异步 ask、`network.mode = strict`
  的 OS 级强制。它们分别留到 ACP 与 v2.2。

### ADR-0023 修正（2026-10-20）

- `Egress::client()` 原先在 `Proxy::all` 之后调用 `ClientBuilder::no_proxy()`；reqwest
  里这个方法会 **清空代理配置**（不是「忽略 NO_PROXY」），所有请求其实都直连，
  allowlist/审计/SSRF 拦截全部失效。改为 `Proxy::no_proxy(None)`，并给
  `plain_http_requests_are_proxied` 增加审计断言作为回归测试。

## ADR-0024：ACP 集成与 fs/terminal 委托边界（2026-10-19）

**背景**：v2.1 通过 Agent Client Protocol 接入 Zed/Neovim；ACP client 可以提供文件与
终端能力，若直接使用会绕过 qcode 的路径策略与沙箱。

**决定**

1. 新增 `qc acp`，stdio JSON-RPC；实现 `initialize`、`session/new`、`session/load`、
   `session/prompt`、`session/cancel` 与 `session/update`。
2. 默认把 `fs/read_text_file`、`fs/write_text_file`、`terminal/*` 委托给 client，以尊重
   未保存 buffer 与编辑器终端；client 不支持时回落本地工具。
3. 所有写操作仍走 qcode 审批；路径必须 absolute，canonicalize 后必须在 workspace 内。
   client 终端产生的网络流量不受 qcode 代理控制，属于 client 边界。
4. 事件映射：assistant chunk、tool call/update、plan（todo）、compaction、notice；
   `session/request_permission` 复用 `Approver`。
5. pin ACP schema 版本，CI 跑 compatibility kit；Zed/Neovim 手工验收后再宣布支持。

**后果**

- 编辑器获得原生体验，qcode 不必先做 GUI；但 ACP 协议仍在演进，适配层需隔离。
- 委托路径必须持续测试路径穿越、symlink、Windows 绝对路径。
- 不在 v2.1 做多 agent 会话或 ACP registry 发布。

## ADR-0025：v2 分期与 GUI 位置（2026-10-19）

**决定**

- v2.0：Windows、egress proxy、`web_search`、subagent 正式化、分发/更新。
- v2.1：ACP、MCP resources/prompts、ACP 兼容性。
- v2.2：图片输入、checkpoint/回滚、GUI spike、macOS 网络 hard mode。
- v3：完整 GUI；服务端/多租户推迟。

**GUI**

- v2.2 只做 2 周 spike，同一组 3 个界面（transcript、diff、审批）分别用 egui 与
  GPUI Kit 实现，记录性能/CJK/编辑器/体积/API 稳定性；v3 再决定并完整开发。
- 独立 `qcode-desktop` crate，消费 core 事件投影，不改 TUI。
- 当前倾向：产品级 GUI 用 GPUI Kit；内部/轻量面板用 egui；以 spike 为准。

**后果**

- v2.0/v2.1 不被 GUI 拖慢；Windows 与网络边界优先。
- GUI 决策延后，但 spike 的验收标准已固定，避免无限期讨论。

## ADR-0026：web_search 作为独立工具与多后端（2026-10-19）

**决定**

- `web_search` 是 qcode-tools 里的普通工具（`WebSearch`），不是 provider 的一个开关：
  模型直接调用，结果带 `title` / `url` / `snippet`，与 `web_fetch` 互补（搜索定位、
  fetch 取正文）。
- 后端用枚举而非动态插件：`brave`（默认）/ `tavily` / `searxng`。Brave 与 Tavily 是
  托管 API，key 只从环境变量读（默认 `BRAVE_API_KEY` / `TAVILY_API_KEY`，可用
  `api_key_env` 改名）；searxng 是自建/本地实例，用 `base_url`（缺省
  `http://localhost:8888`，显式写 `http://` 时自动补 `/search`）。
- 配置在 `[search]`（`enabled` / `backend` / `api_key_env` / `max_results` / `base_url`），
  默认 `enabled = true`。缺 key（searxng 缺 `base_url`）时**不注册工具**并在
  `qc doctor` 里 warn：模型看不到一个必然失败的工具，比运行时 401 更好。
- 出口与审批复用现有面：`ApprovalClass::Network`（与 `web_fetch` 同一条 ask 记录）、
  `ToolContext.network` 的代理 client（`allow_host` 把后端 host 加入 session
  allowlist）；没有 network 时退化为直连 client（单测用）。
- 响应解析不看 `Content-Type`：Brave/Searxng 可能回 `text/html`，统一 `response.json()`
  再按后端形状校验；错误体截断 500 字符，凭据绝不回显。

**后果**

- `[providers]` 与 `[search]` 的 key 都是 env 名字，`.qcode/config.toml` 可以进版本库
  （已有 `deny` 规则拦 `.env*`）。
- 新增后端只改 `web_search.rs` 的 match 与 `key_env_name`，不触碰 engine/TUI。
- searxng 的 `base_url` 若指向 loopback，需要 `[network] allow_private = true` 才能穿过
  egress 的 SSRF 拦截——这是有意的失败关闭。

## ADR-0027：Windows 沙箱用写受限令牌 + 能力 SID（2026-10-20）

**背景**

- v2.0 要求 Windows 上 `run_shell` 也有一条内核边界（ADR-0025）。AppContainer 连读都要预
  授权，loopback 默认封闭还要 `CheckNetIsolation` 提权，与本产品"任意路径读 + 本地 egress
  代理"直接冲突；mxc 需要 24H2+ 与 feature 开关，回落的仍是 AppContainer 形状。
- 机制先用隐藏子命令 `qc sandbox-probe` 在真机实测（M0），再落实现（M1）。探针保留在仓库里，
  作为这套机制的可重跑证明。

**决定**

1. 进程模型沿用"不动 trait"的 A 方案：`Sandbox::command()` 返回
   `qc sandbox-run --workspace <w> --temp <t> --cwd <c> [--extra <p>]... -- <program> <args>`，
   helper 建令牌、铺 ACE、拉起子进程并透传退出码；自身失败打印一行
   `qc-sandbox-run: <detail>` 后 exit 127，绝不降级成不受限 spawn。
2. 令牌：`CreateRestrictedToken(0x1|0x4|0x8)`，restricting SID = 登录 SID + `Everyone` +
   工作区 SID + 本次 temp SID；失败依次降级 `0x9` / `0x8` / `0`，全失败即 `Setup` 报错。
   `lpEnvironment` 必须是 NULL（显式环境块实测 `ERROR_INVALID_PARAMETER`）。
3. 能力 SID：`S-1-4-x-y`（non-unique authority），由 (canonical 路径, 域) 的 FNV-1a 派生。
   工作区 SID 跨 session 稳定，ACE 一次铺好后 `tree_granted` 命中即跳过（O(1)）；temp SID
   每次运行新建（`%LOCALAPPDATA%\qcode\sandbox\tmp\<pid>-<ts>`，不可写时回落会话 temp 下的
   私有子目录），两个会话不能互写 temp。
4. 授权：`GetNamedSecurityInfoW` + `SetEntriesInAclW` + `SetNamedSecurityInfoW`，给能力 SID
   一条 `FILE_ALL_ACCESS` + `OI|CI` 继承 ACE。并发用命名互斥量
   `Local\qcode-sandbox-grant-<sid>` 串行化 read-merge-write，否则两个 qc 同时启动会互相覆盖
   DACL。ACE 是常驻的，由 `qc sandbox grant <路径>` / `qc sandbox revoke <路径>` 显式铺设与
   撤销（撤销要逐对象删 ACE：继承写下的副本不会随根上的 ACE 一起消失）。
5. 令牌默认 DACL：把 temp SID **追加**进去而不是替换。子进程自己创建的匿名管道按默认 DACL
   设安全描述符，缺了这条第二遍访问检查不过，piped stdio 全废（M0 实测）。
6. 进程创建：`CreateProcessAsUserW` + `CREATE_SUSPENDED` → 加入 kill-on-close job object →
   `ResumeThread`。stdio 从 helper 继承（`GetStdHandle` + `SetHandleInformation(INHERIT)`，
   句柄缺失时退回 NUL）；不能加 `CREATE_NO_WINDOW`，否则 DLL init 阶段 `0xC0000142`。
7. 启动 preflight：`WindowsAcl::check()` 真的经 helper 跑一次 `cmd.exe /c exit 0`，失败
   `SandboxError::Preflight` 让 qc 拒绝启动。

**M0 实测（本机未受限 pwsh，2026-10-20）**

- 非管理员可以建 `0xd` 受限令牌并 `CreateProcessAsUserW` 起子进程（child exit 0）。
- 只挂能力 ACE 就够：工作区与私有 temp 内写/删/嵌套写全 `allowed`，工作区外的写、删、改名
  全部 `拒绝访问 (5)`。原因：写受限只做写检查，而 restricting SID 不必为父目录的
  `FILE_DELETE_CHILD` 共同签字，删除逃逸被堵住。
- 匿名管道（孙进程 piped stdio）在"追加 temp SID 到默认 DACL"后 `allowed`；替换默认 DACL
  则全废 —— 这正是把默认 DACL 设为"追加"的原因。
- Low 完整性标签打不上：`SetNamedSecurityInfoW(SACL)` 返回 1314（标签在 SACL，需要
  `WRITE_OWNER`/`SeSecurityPrivilege`）。因此标签层**没有**进实现，`token` 与 `full` 两模式
  观察结果一致。
- 未覆盖：父进程已是 Medium 且能写 `%PUBLIC%` 时的删除面（本机 `%PUBLIC%` 不可写，测不到）。

**后果与边界**（`describe()` / README / doctor 都标 `partial`）

- 只限制写：读与网络不受限（网络仍靠 egress 代理的 env，shell 可以绕过）。
- 硬链接是文件对象别名，路径限制无效；受限孙进程打不开命名管道，Node `stdio: 'pipe'`
  会 EPERM（继承/匿名管道正常）。
- 永久 ACL 变更：ACE 常驻，改名后的旧 SID 成为惰性残留；`qc sandbox revoke` 会遍历整棵树
  逐对象删掉该 SID 的 ACE。
- `[sandbox] network` 对 windows-acl 不生效，配置注释里已写明。

**验证**

```powershell
cargo build -p qcode
.\target\debug\qc.exe sandbox-run --workspace .tmp\acltest\ws --temp .tmp\acltest\tmp --cwd .tmp\acltest\ws -- cmd.exe /c "echo hi"
.\target\debug\qc.exe doctor        # [ok] sandbox: windows-acl (partial): ...
```

在嵌套/受限 shell 里跑会得到 `SetNamedSecurityInfoW(..): win32 5` 并 exit 127 —— 失败关闭，
不会退回不受限执行（本仓库的自测就是在这样的 shell 里做的）。

**补充：MCP 与 TMP 交接（2026-10-20）**

**补注（2026-11-11）**：下面这段的前提「server 因此和 `run_shell` 一样被 helper 拉起」
在 2026-11-11 被推翻——同一份 helper，但**授权集不再同一份**。MCP server 拿不到
`run_shell` 那种「整个 workspace 读写」的边界，见 ADR-0066。

- MCP stdio server 也走同一道边界：`qcode_mcp::connect_all_with` 接受一个
  `Confine` 回调（`main.rs` 用 `Sandbox::command` 构造），server 因此和 `run_shell`
  一样被 helper 拉起；`connect_all` 保持原签名（无 confine）给测试用。
- 子进程拿不到独立环境块（`lpEnvironment` 必须 NULL，见上），所以 helper 在 spawn 前把
  `TMP`/`TEMP`/`TMPDIR` 指到已授权的私有 temp。这也意味着需要自己缓存目录的 MCP server
  （`npx` 的 npm cache 之类）要进 `extra_writable`，否则只能写 workspace 与私有 temp。

**已知问题：私有 temp 建目录被按 exe 路径拦（2026-10-20）**

开发机上 `qc.exe` 在 `%LOCALAPPDATA%\qcode\sandbox\tmp\...` 与 `%TEMP%\qcode-sandbox-tmp\...`
下 `create_dir_all` 一律 `拒绝访问 (5)`。对照实验把范围缩到「按镜像路径」：

- `qc whoami` 在两边打印同一个令牌：`integrity S-1-16-4096`（Low）、`restricted sids 0`、
  `app container 0`，`TEMP`/`LOCALAPPDATA` 也完全相同；
- 同一份二进制，放在仓库里（`target\debug\qc.exe`、`.tmp\qc-copy.exe`）两个基座都 `error 5`；
  复制到 `C:\Users\qiuzh\qc-copy.exe` 后两个基座都 `ok`；
- `%TEMP%` 的 DACL 没有 deny（`Get-Acl` 的 Sddl 里只有 SYSTEM/Administrators/用户 的
  继承全权和一个域的继承 ACE），同一 shell 下的 `cmd /c mkdir` 与 `New-Item` 对**已存在**
  目录也会报成功/已存在，所以早先那两条「pwsh 能建」不构成反证。

结论：同样的令牌、同样的父目录，只有镜像路径不同就出现两种结果，因此拦截层在进程之外
（安全产品/策略按镜像路径或目录做限制），不是 qc 的 ACL 逻辑，也不是我们读错的令牌字段。
按用户要求不再深挖该机器；`qc whoami` 与 `qc doctor` 的 `[ok] token` / `[warn] temp`
两行保留了复现所需的全部事实。

沙箱不能因此不启动，所以 `private_temp()` 是三级回落：`%LOCALAPPDATA%\qcode\sandbox\tmp` →
`%TEMP%\qcode-sandbox-tmp` → **工作区内 `.tmp\qcode-sandbox-tmp`**；命中第三级时打一条 warn
并写明用了哪条路径，`describe()`/`qc doctor` 里也能看到实际 temp。工作区本来就是已授权根，
所以第三级不扩大写边界（代价是 temp 落在仓库里，`/.tmp` 已在 gitignore 中）。

## ADR-0028：config schema 版本化与未知键告警（2026-10-20）

**决定**

1. 顶级键 `version`，缺省为 1，`CONFIG_VERSION = 1`。没有该键的文件**就是**当前版本：
   老配置不需要迁移就能加载，版本号不是必填项。
2. 声明的版本高于 `CONFIG_VERSION` 时**启动失败**（`ConfigError::VersionTooNew`，错误里给出
   文件、声明版本、本 build 支持的上限，以及「升级 qcode 或删掉 version」）。项目层
   `<workspace>/.qcode/config.toml` 同样受检——仓库不能替用户打开一个用户版 qcode 读不懂的 schema。
3. `version = 0` 视为非法文档（版本从 1 起）。
4. 未知键只告警、不失败。`unknown_keys()` 对已知表逐键比对（顶层与
   `approval` / `context` / `mcp` / `skills` / `sandbox` / `network` / `search`）；
   `providers.*`、`pricing.*`、`mcp.servers[]` 的子键是用户命名空间，不校验。
   告警走 `tracing::warn!`（`RUST_LOG=info` 可见），stdout 的 JSONL 不受影响。
5. 加载顺序：读文件 → 解析成 `toml::Table`（未知键检查需要原始文档）→ 反序列化 →
   `check_version` → 未知键告警 → `mcp.validate()`。
6. `qc config edit` 的种子文件写入 `version = 1`；`qc config show` 与 `qc doctor` 都打印
   声明版本与本 build 支持的上限。

**理由**

- 配置是数据：新版本添加的可选键（更多 search 参数、额外 sandbox 根）应当能被旧 build
  原样加载并忽略，硬失败会让「降级回滚 qcode」变成不可能的操作。
- 但键名写错（`[search] bakend`）会被 serde 静默丢弃、退化成默认值——这是真正危险的一类，
  所以至少要在日志里点名到 `段.键`。
- 版本检查只保留一个方向：拒绝更新的版本。旧 build 无法理解新语义时，猜着跑比拒跑风险更大。
- 单向要求使迁移成本恒为零：本 ADR 之后新增的每个键都只需 `#[serde(default)]`。

**后果**

- `KNOWN_KEYS` 是手写清单，加段/加键时必须同步；遗漏会让合法键被误报。测试
  `every_section_key_the_build_reads_is_known` 用一份覆盖全部段的文档守着它。
- `version` 之外的已知键不含任何必填项：配置文件整体仍可以是空文件。
- 未知键不进入 `--json` 事件流，只进日志；需要机器可读诊断时用 `qc config show` 的版本行。

**验证**：`a_config_without_a_version_is_the_current_schema`、
`a_newer_config_version_is_rejected_with_the_supported_one`、`a_zero_config_version_is_rejected`、
`a_project_config_cannot_declare_a_newer_version`、
`typos_are_reported_as_unknown_keys_but_still_load`、`every_section_key_the_build_reads_is_known`。

## ADR-0029：自动更新以内置 `qc update` 为载体（2026-10-20）

**决定**

1. 更新器是二进制的一部分（`crates/qcode/src/update.rs`），不额外分发脚本：
   `qc update --check` 只报告，`qc update` 下载并就地替换当前可执行文件。
2. 发布物只提供**裸二进制**与一份 `SHA256SUMS`。资产名固定为
   `qc-<version>-<os>-<arch>[.exe]`，`os`/`arch` 与 `std::env::consts` 一致
   （`windows|macos|linux` × `x86_64|aarch64`），由 `asset_name()` 与两个发布脚本
   各写一半（`release_asset` / `Get-ReleaseAsset`，两处必须同步；`update.rs` 的单测
   固定这个形状）。归档（`.tar.gz`/`.zip`）是给人的，更新器不解压任何东西。
3. 信任模型：走 `https://api.github.com/repos/<repo>/releases/latest`（或 `[update] base_url`
   指定的同形状镜像），资产只从 API 返回的 `browser_download_url` 取；下载后必须匹配
   `SHA256SUMS` 里的 sha256（sha256 在 `update.rs` 内自带实现，不引依赖）。
   校验和缺失或不符 → 不落盘、报错退出。
4. 替换方式：先把正在运行的 exe 改名为 `<exe>.old-<unix 秒>`，再把写好的
   `<exe>.new-<unix 秒>` rename 到目标名；目标原本不存在（新装）时直接写，不留备份。
   旧文件不自动删除，`cleanup_backups` 只在成功路径清理自己那次的 staged 文件。
5. 启动时只做「便宜的检查」：结果缓存在 `<data-dir>/update-check.json`，超过
   `[update] check_days`（默认 7）天才发一次 3s 超时的请求；失败静默。
   它只往 stderr 打一行通知，绝不进入 `--json` 事件流，也绝不让 `qc run` 失败。
   `enabled = false` 或 `check = false` 完全关闭（企业内网默认关）。
6. 会话内 HTTP 走 egress 代理这条规则（ADR-0023）**不适用于更新器**：它是 bootstrap 通道，
   必须在用户配置损坏、出口策略拦掉一切的时候仍能工作。代价写在「后果」里。

**理由**

- 手工下载 Releases 再替换是当前唯一的升级方式；在 Windows 上这还是最容易被杀软拦住的
  一类操作。把「检查 + 校验 + 替换」做成同一条命令，才有版本一致性可言。
- 不解压归档 = 没有 zip-slip、没有 tar 解包器、没有 `tar` 依赖；更新器只要会写文件。
- 校验和必须与资产在同一个 release 里，否则「镜像挂了但旧文件还在」这类故障无法区分。
- 版本比较只比数字核心（去掉 `-rc1` 这类后缀按数字段比较），不实现 semver 预发布优先级：
  qcode 的预发布只是标注，不参与选择。

**后果**

- 信任锚点是 GitHub + release 里的 `SHA256SUMS`：账号被攻破时两者都会被替换，
  这与 `curl | sh` 同级。没有签名、没有 transparency log。
- 没有增量更新（每次 ~全量下载）、没有 `qc update --rollback`（`.old-<ts>` 是手工退路）、
  没有后台常驻检查（只在 `qc run`/`qc update` 时发生）。
- 更新器绕过会话出口策略是**已知偏离**：它只连 `base_url` 与 API 返回的资产 URL，
  不读用户 allowlist，也不写 network.jsonl 审计。需要审计的环境应关闭 `[update] check`
  并手工升级。这条要在 v2.1 重新评估（把更新器挂到 egress 上需要先有「无配置也能建代理」的路径）。
- 没有 Apple 公证 / Windows 代码签名：macOS 的 `notarize-macos.sh` 已在仓库里，
  但 CI 还没有 secrets；Windows 上首次运行会撞 SmartScreen。
- 从 GitHub 之外的渠道（brew/scoop/winget）装的副本，`qc update` 会替换包管理器管理的
  文件，包管理器随后会认为「文件被改过」。README 明确写了这一点：用包管理器装就别开自动更新。

**验证**：`qc update --check`（未发版时得到「no published release」而不是网络错误）、
`versions_compare_numerically_not_lexically`、`a_prerelease_is_older_than_its_release`、
`sha256_matches_known_vectors`、`install_replaces_the_target_and_keeps_a_backup`、
`a_fresh_target_is_written_without_a_backup`、`a_stale_cache_produces_a_notice`、
`tests/update.rs` 的四条端到端（本地 HTTP 服务扮演 release API：check 报告较新/相同版本、
校验通过则替换目标并留备份、校验不符则目标原样、不可达时给出 HTTP 错误）。

## ADR-0030：发布流水线统一产出归档、裸二进制与安装清单（2026-10-20）

**决定**

1. `.github/workflows/release.yml` 在 `v*` tag 上按矩阵构建：`ubuntu-latest`（linux amd64/arm64）、
   `macos-14`（darwin arm64 + 交叉 amd64）、`windows-latest`（windows amd64/arm64 msvc）。
   每个 job 调 `build-for-release --archive --release`（PowerShell 版同参数 `-Archive -Release`），
   上传整个 `dist/`。
2. 两个发布脚本新增「发布物」职责：`--release` 额外产出
   `dist/qc-<version>-<os>-<arch>[.exe]` 并维护 `dist/SHA256SUMS`（LF 行尾）。
   归档命名保持既有约定 `qcode_<version>_<os>_<arch>[_libc][.ext]`。
3. `publish` job 做三件事，全部在合并后的 `dist/` 上：
   - **规整**：Windows msvc 目标的 `_msvc` 后缀与包内顶层目录改成
     `qcode_<version>_windows_<arch>`（清单模板只认这一个名字）；
   - **重算** `SHA256SUMS`：覆盖 `dist/` 里所有文件，而不是合并各 job 各自的那份；
   - **渲染清单**：`packaging/render.sh` 从 `dist/` 的归档实际字节算 sha256，
     填进 Homebrew formula / Scoop manifest / winget 三件套，输出 `dist/manifests/`。
4. `render.sh` 缺任何一个平台归档就失败——宁可发布失败，也不生成指向不存在文件的清单。
5. 发布前校验 tag 与 `[workspace.package] version` 相等；不相等直接失败。
   `gh release create` 上传裸二进制、归档、`SHA256SUMS` 与 `dist/manifests`。
6. 清单落地方式是**渲染产物 + 人工提交**：Homebrew 进 tap 仓库（`qiuzhanghua/tap`），
   Scoop 进 bucket，winget 按三件套提 PR。仓库里存的是 `.in` 模板，不存成品。
7. 不做 npm wrapper：多一条供应链与一个 registry 账号，只换来「`npx qc` 能装」这一种入口。

**理由**

- 清单里的 sha256 必须来自真正上传的字节。手工抄写是这类发布最常见的事故源，
  而「从 `dist/` 现算」把这类事故变成不可能。
- 归档名在 CI 里被 msvc 后缀带偏过一次，规整步骤让「包管理器看到的名字」与
  「update.rs 看到的名字」互不干扰。
- 模板而不是成品：版本一 bump，成品清单就全部过期；模板只有一个来源
  （`Cargo.toml` 的版本 + `dist/` 的字节）。

**后果**

- 任一平台构建失败会让整个 release 失败。这是有意的：半套清单比没有清单更糟。
- Windows arm64 依赖 runner 上的 MSVC ARM64 工具链（`aarch64-pc-windows-msvc`）。
  runner 镜像变动导致该目标失败时，只能先去掉这个资产（更新器对缺失资产报 410 并给出
  可用名字，不是静默跳过）。
- macOS 归档未公证（缺 secrets），Scoop/winget 的包未签名：首次安装会有系统警告。
- `dist/manifests` 是 gitignore 的产物；需要留档时随 release 附件保存。

**验证**：`./build-for-release --release` 在本机产出 `dist/qc-0.5.0-windows-x86_64.exe`
与 `dist/SHA256SUMS`（`sha256sum -c` 可过）；`./packaging/render.sh` 在六个归档齐全时
输出 5 份清单、缺归档时报错；release workflow 的 publish job 对 tag/版本不一致会失败。

## ADR-0031：跑飞兜底——无进展检测与预算（2026-10-20）

**背景**：一次技能实跑在 `exceeded max turns (8)` 处停住，而这 8 轮里有一半是把同一条
命令换个写法再试；把 `max_turns` 调到 40 能让它跑完，但也让「模型原地打转」花掉 40 轮的钱。

**决定**

1. **无进展检测**（引擎层）：连续 3 轮都发出**工具名与参数完全相同**的调用即终止 turn，
   写 `error(no_progress)` 事件、`turn_ended(failed)`，返回 `EngineError::NoProgress`。
   只比连续轮次：第 4 轮换了参数就是进展，计数清零。
2. 判据用「调用签名」而不是「结果文本」：结果相等需要按工具归一化（`git status` 两次输出
   相同往往正当），而参数相同加结果相同是同一个事实的两个投影。签名即 `name + JSON(args)`。
3. 阈值是常量 `MAX_IDENTICAL_ROUNDS = 3`，**不做成配置**：一个能关掉保护的开关会让人在
   卡住时先关它，而不是改方法。真需要重复三次以上同一条命令时，改参数（带计数器）即可。
4. **预算**：`[agent] max_turns`（当时默认 8；ADR-0045 调至 64）与 `[agent] max_cost_usd`（默认无）进配置，
   `--max-turns` / `--max-cost` 覆盖；TUI 没有这些 flag，从此都读配置。
   `max_cost_usd` 仍需 `[pricing."<model>"]`，否则启动即报错（原有语义不变）。
5. 停止顺序：模型不再调工具（正常）→ 取消 → 超预算 → 无进展 → 超轮数。先报更具体的原因，
   用户才知道该改什么。
6. **不做**（本轮）：命中上限自动续一个预算窗口。（ADR-0060 后结论不变：仍然不自动续，
   只是「继续」变成一键、默认焦点，且预算命中不再是 `failed`。）它会把「原地打转」伪装成「任务很长」，
   等无进展检测跑一段时间、有了真实数据再评估；时间预算目前只有 `--timeout`（TUI 无）。

**后果**

- 合法的「重试同一条命令看是否偶发失败」最多两次；第三次会被拦。这是有意的取舍：
  连续三次同样输入说明模型没有新信息。
- `no_progress` 与 `max_turns` 都是 turn 级失败，不写 agent 级状态；会话可继续
  （再发一条消息即新 turn，预算重新计算）。
- TUI 的时间上限仍未实现：`TuiOptions` 里没有 deadline，加它需要在事件循环里竞速一个
  计时器，留到有真实需求时再做。

**验证**：`identical_rounds_are_caught_but_different_ones_reset`、
`a_signature_tracks_the_tool_and_the_arguments`、
`the_turn_budget_defaults_to_eight_and_can_be_raised`（含 `max_cost_usd` 解析）；
手工：让模型连续调用同一条命令，第 3 轮出现 `error(no_progress)` 并停止。

## ADR-0032：`qc acp` —— ACP v1 over stdio（2026-10-20）

**范围**：新 crate `qcode-acp`，把引擎接到 Agent Client Protocol 上，入口 `qc acp`。
目标是编辑器（Zed、Neovim）能直接拉起 qcode 当 agent，与 CLI/TUI 共用同一引擎。

**决定**

1. 传输是 stdio 上的每行一个 JSON-RPC 2.0 对象（`jsonrpc.rs` 只做信封分类：
   有 id + method 是请求，有 method 无 id 是通知，有 id 无 method 是响应）。
2. 方法集：`initialize`、`authenticate`（无认证，直接成功）、`session/new`、
   `session/load`、`session/prompt`、`session/cancel`（通知）。
   未知方法回 `-32601`，参数错误回 `-32602`。
3. 能力协商：`protocolVersion` 回显客户端版本（它高于 v1 时回 v1，由客户端决定是否断开）；
   `agentCapabilities.loadSession = true`；`promptCapabilities` 里 image/audio 为 false、
   embeddedContext 为 true；`authMethods` 为空。
4. `session/new` 用请求里的 `cwd` 建会话并写 `session_started`；`cwd` 必须是绝对路径。
   `session/load` 读 JSONL 事件并逐个重放成 `user_message_chunk` / `agent_message_chunk` /
   `tool_call` / `tool_call_update`，然后这个会话可以继续 `session/prompt`。
5. `session/prompt` 用 `run_turn` 跑一轮：先拼 prompt 文本（text 与带 text 的 resource 直接
   用，resource_link 写成 `[name](uri)`；image/audio/二进制 resource 直接回 `-32602`，
   不静默丢内容），然后把引擎事件映射成通知（下表），最后按结果回 `stopReason`：
   `cancelled` / `max_turn_requests`（命中 max_turns）/ 其它 `end_turn`。
6. 审批走 `session/request_permission`：三个选项 `allow-once`、`allow-session`
   （kind `allow_always`）、`reject-once`；客户端回 `selected` 或 `cancelled`。
   `allow-session` 只对**本轮**有意义：qcode 不跨轮记住选择，客户端应自己缓存
   `allow_always` 并在下次直接作答（这是 ACP 客户端的常规做法）。
7. `session/prompt` 在独立任务里跑，读循环同时继续读 `session/cancel`；
   取消与 `request_permission` 用 `tokio::select!` 竞速，取消先赢。
8. 事件映射：`assistant_message` 的 Text→`agent_message_chunk`、Reasoning→
   `agent_thought_chunk`（空文本丢弃）；`tool_call`→`tool_call`（kind 由工具名映射，
   `rawInput` 带参数）；`tool_result`→`tool_call_update`（`completed`/`failed`，
   content 是文本，`rawOutput` 是输出字符串）；`error`→一条 `agent_message_chunk`
   文本（失败不能是静默的）。`plan`/`usage_update`/`notice` 本轮不发。

**没有做（明确记录）**

- `fs/*` 与 `terminal/*` 委托：工具仍在本机执行，忽略客户端能力位。委托要先把工具层
  抽象成"执行器"，属于 v2.2 之后的事。
- 客户端传来的 `mcpServers` 被忽略：MCP server 仍由 qcode 配置决定并预先启动。
- 图像/音频 prompt、`session/set_mode`、`session/list`、notices、compaction 更新。
- 逐 token 流式：每条 assistant 消息发一个 chunk，`on_delta` 传 `None`。
- ACP 会话里没有 `subagent` 工具：它的 approver 在构建工具时固定，而客户端只能替当前
  会话作答。

**风险**

- 没有在本机对着 Zed 手工验收（无编辑器环境）；协议形状按 v1 文档与 schema 实现，
  smoke 用的是真实 stdio + mock provider。Zed 的接入配置以它自己的文档为准。
- `session/load` 重放的是完整事件流，长会话会给客户端推很多通知；v1 先这样。

**验证**：`qcode-acp` 9 个单测（信封分类、update 判别字段、permission outcome 解析、
工具名→kind、prompt 拼接与拒绝）+ 5 个集成测试（initialize 协商、prompt 流式回答与
`end_turn`、工具调用先要权限再回报 `completed`、拒绝则不执行、`session/load` 重放
user/agent 两条 chunk）；手工 smoke：`qc acp --config .tmp/acp-mock.toml` 喂
`initialize` + `session/new` 两行，回 `protocolVersion: 1` 与一个新 sessionId；
`qc doctor` 打印 `acp: protocol v1 over stdio`。

## ADR-0033：MCP 的 env、resources 与 prompts（2026-10-20）

**决定**

1. `[[mcp.servers]]` 增加 `env`（字符串表），在 spawn 时 `envs` 到子进程。
   顺序是**先 config env，后出口代理变量**：server 自己写的 `HTTPS_PROXY` 不能把业务流量
   从 qcode 的代理下偷走。
2. resources 与 prompts 用**工具**暴露，因为引擎只认识工具。每个能力两个工具：
   `mcp__<server>__list_resources` / `read_resource`（参数 `uri`）、
   `mcp__<server>__list_prompts` / `get_prompt`（参数 `name`、可选 `arguments`）。
   列表工具每次调用都向 server 现取，不做缓存。
3. 能力探测靠**试一次**：握手后调 `resources/list` 与 `prompts/list`，成功才注册对应工具，
   报错只记 `debug` 并跳过。没有引入"读 server capabilities 决定注册"的第二套判定——
   rmcp 的 capability 结构与我们的注册逻辑会重复表达同一件事，而试一次的代价是两次请求。
4. 请求参数用 serde 从 JSON 构造（`json!({"uri": ...})` → `ReadResourceRequestParams`），
   不直接写 struct 字面量：rmcp 的协议结构体加字段时这里不用跟着改。
5. 结果按 JSON 文本返回（`serde_json::to_string_pretty`），沿用 32KB 头尾截断与
   `full_output`。审批级别与 MCP 工具一致：`External`，默认 ask。
6. 参数缺失是**工具错误**（`status=Error` + 说明），不是空结果：`read_resource` 没有
   `uri`、`get_prompt` 没有 `name` 都要报出来。

**后果**

- 一个声明两种能力的 server 会多出 4 个工具；模型看到的工具数变多，但名字带前缀，
  审批规则可以按名字精确放行。
- `list_*` 是"现取"，server 端资源变化立刻可见，代价是每次调用一次往返。
- 探测失败只有 debug 日志：默认日志级别下看不到"这个 server 没有 resources"，
  需要时用 `RUST_LOG=debug`。
- 不做 `resources/subscribe`（更新推送）、不把 prompts 自动塞进 system prompt：
  prompt 模板由模型显式 `get_prompt` 取用。

**验证**：`qcode-mcp` 14 个单测 + 13 个集成；新增两条集成用 stub server 的
`MCP_STUB_CAPS` 环境变量反向证明 `env` 真的传到了子进程：设成
`tools,resources,prompts` 时四个额外工具出现且 `read_resource` 返回 stub 正文、
缺 `uri` 报错、`get_prompt` 返回渲染后的消息；不设时仍然只有 7 个工具。

**实现注记（2026-11-11，§3 与「后果」第二条被 ADR-0068 §4 取代）**

- §3 的「试一次」**已作废**（ADR-0068 §4 的 B-2 修订）：**本 ADR 不再有任何探测**。
  握手的 `capabilities` 是唯一来源，与协议版本无关。「声明与实际是否一致」这个问题移交
  `qc doctor --probe`——它按需运行、两边都问、并做比对，而这正是 §3 当初「试一次」做不到的
  （它只问、然后把答案当真相，从不与声明对照）。
- **第一版曾把这条收窄到 legacy 分支，那次收窄也已作废。** 保留一句当时的理由以免有人重提：
  「野外的 2025 时代 server 经常把 `capabilities` 写得不准」。这句话是猜测——真机实测的
  `wordcraft-cli 0.15.2`（2025-06-18）声明与行为完全一致，见 ADR-0068 §4 的 B-2 注记。
- 「后果」第二条「探测失败只有 debug 日志」**已无对象**（不探测了）。但同一诉求换了位置：
  连接期那行 `mcp capabilities read from the handshake` 是 `info`，`RUST_LOG=info` 下能直接
  看到每个 server 的三个能力布尔值，不需要 `debug`。
- 「后果」第四条「不把 prompts 自动塞进 system prompt」**不变**，且现在有了新的相邻物：
  `instructions`（握手自述）会注入，`prompts`（模板）不会。两者不是一类东西，见 ADR-0068
  §3——这条区分现在有两处测试守着，不要再合并。

## ADR-0034：checkpoint / rollback 只记录「即将被改的文件」（2026-10-20）

**背景**：agent 改坏代码时，`sessions fork` 只能分叉对话，不能把工作区还原。

**决定**

1. checkpoint 不是工作区快照，而是**写入前的文件副本**：引擎在每个工具调用执行前，按工具名
   推断它将要改哪些路径（`write_hints`：`write_file`/`edit_file` 的 `path`，`apply_patch`
   里的 `*** Update/Add/Delete File:` 与 `*** Move to:`），把当前内容存进
   `<data-dir>/snapshots/<session>/<turn>/files/<相对路径>`；不存在的文件记进同目录的
   `absent.txt`。同一 turn 内同一路径只记第一次，那才是回滚要还原的状态。
2. 回滚到 turn N = 取 `>= N` 的每条路径**最早**的那份副本写回，`absent` 的路径删除；
   没被 agent 碰过的文件不动。
3. 入口：`qc sessions checkpoints <id>`（按 turn 列出记录）、
   `qc sessions rollback <id> --to <turn> --yes`；TUI 里 `/checkpoint` 与 `/rollback <turn>`。
   CLI 回滚用的是**会话记录的 cwd**，不是当前目录。
4. 引擎通过 `TurnContext.snapshots: Option<&SnapshotStore>` 拿到 store：`qc run`、TUI、
   `qc acp` 都传，subagent 继承同一个 store；记录失败只 warn，绝不因此让写入失败。
5. 拒绝越界路径：相对路径先拼 workspace 再规范化，`..` 与工作区外的绝对路径直接丢弃。
6. 回滚不碰 git：未提交改动会被文件内容覆盖。这是有意的——它是「回到某个 turn 之前」的
   文件级还原，不是 git 操作。

**边界（必须知道）**

- 只有三个内置写工具被记录。`run_shell` 里的 `Set-Content`/`sed -i`、MCP 工具的写入、
  子进程的副作用**都不会**被回滚——引擎不知道它们动了哪些文件。
- 没有快照 GC：每次写入都留一份副本，长期使用要手工清 `<data-dir>/snapshots`。
- 目标路径当前是目录时跳过并报告，不递归删除。
- 只还原文件内容，不还原权限位、符号链接目标与 mtime。

**验证**：`qcode-core` 6 条（提示路径解析、新建文件记 absent 并被删除、按 turn 取最早副本、
越界路径丢弃、同 turn 首写优先、缺失会话无副作用）；`qcode-acp` 新增 1 条端到端
（脚本让 provider 调 `write_file`，断言 store 出现 turn 1 的 absent 记录，回滚后文件被删除）；
`qcode-tui` 1 条（`/checkpoint` 与 `/rollback 3` 解析成 UiAction，缺参数给提示）。

## ADR-0034 补充：GC 与 `run_shell` 覆盖（2026-10-20）

**决定**

1. `[agent] snapshot_shell`（默认 `true`）：执行 `run_shell` 之前对整个工作区做一次**受限**
   快照——跳过 `.git` / `target` / `node_modules` / `dist` / `.tmp` / `.qcode-data` / `.qc-data` /
   `.venv` / `__pycache__` / `.cargo-home` / `.rustup-home` / `.toolchains`，单文件超过 4 MB
   跳过，一个 turn 的累计总量超过 64 MB 置 `capped` 并停止（每轮只打一次 warn）。
   同一 turn 内已存过的路径不再复制，但**已占用的字节计入该 turn 的额度**，所以一个 turn 里
   第 2、3 条 shell 不会各自再拿到一份 64 MB。这样 rollback 也覆盖 shell 与 MCP 驱动的编辑，
   而不是只有三个内置写工具。
   `TreeReport::added_bytes` 区分两种 `capped`：本调用真的截断了（>0，打 warn）vs 该 turn
   在之前就已满、本次什么都没加（==0，降到 debug）。后者也覆盖修复前留下的超额 turn。
2. `qc sessions gc --older-than N --yes` 删除会话时顺带删掉该会话的 snapshots，并按现存会话
   列表清理孤儿树（`SnapshotStore::sweep`）；报告每个会话的快照体积与总释放量。
3. 树快照**不记录 absent**：shell 新建的文件在回滚时不会被删除（只有写工具记录的 absent
   会被删）。这是有意的不对称——树快照只知道「当时存在什么」。

**后果 / 边界更新**

- 大仓库上每条 shell 命令都要走一遍工作区（有上限）；`snapshot_shell = false` 可关。
- 快照随 turn 累积，每个 turn 最多 64 MB；被跳过的目录（`target`、`.cargo-home` 等）里的改动
  不会被回滚。工具链缓存必须跳过：文件沙箱只允许写工作区，`CARGO_HOME` 就落在源码旁边
  （本仓库的 `.cargo-home` 有 1.6 GB / 6.4 万文件），不跳会让上限全花在 registry 源码上，
  真正的代码反而进不了快照。
- 遍历按字典序进行：截断时保留的是稳定的前缀，同一棵树每次截断得到同一批文件。
- 修复前留下的超额 turn 不会被迁移：它们仍然计入各自那一轮（因此那一轮不再写入），旧缓存
  需手动删或随 `qc sessions gc` 清掉。新 turn 从自己的额度开始。
- gc 只清理「会话已删除」与「没有对应会话」的快照，不按天数清理仍在用的会话。

**验证**：新增 4 条 core 测试——树快照能还原 shell 式编辑（改一个、删一个、新建一个）、
超过 4 MB 的文件被跳过并计数、gc 删除会话 + 清理孤儿树、shell 开关默认关闭且可打开。

## ADR-0035：图片输入只走一条路——`--image` + 多模态消息（2026-10-22）

> 状态：部分被 ADR-0061 取代——图片改为落盘（内容寻址存储）、事件日志存引用、写入时
> 归一化、能力门控从报错改为占位降级。§1 的 `--image` 输入面与 §4 的压缩口径不变。

**背景**：DeepSeek 当前模型支持 vision，`Capabilities.images` 早已预留，但用户消息一直是纯文本。

**决定**

1. 输入面只有 `qc run --image <path>`（可重复）：读文件、按扩展名定 mime（PNG/JPEG/WebP/GIF），
   超过 8 MB 直接报错，base64 后作为 `Attachment { mime_type, data_base64 }` 交给引擎。
2. `ContentBlock` 增加 `Image { mime_type, data_base64 }`；引擎把它挂到**最后一条 user 消息**上。
   OpenAI 兼容层在有图片时把 `content` 从字符串换成 typed parts
   （`{"type":"text"}` + `{"type":"image_url","image_url":{"url":"data:<mime>;base64,..."}}`），
   没有图片时保持纯字符串——兼容端最认这个形状。
3. 能力门控在引擎：`provider.capabilities().images == false` 且带图，直接
   `EngineError::ImagesUnsupported`，不把图片发出去再让服务端 400。
4. 压缩：`estimate` 里按 `base64.len()/4` 计图片的 token 量级；摘要输入**丢弃**图片块
   （摘要模型读不了，base64 还会把输入撑爆）。
5. 事件日志仍只存文本（`user_message.attachments` 留空），所以 resume 重放的消息**没有图片**。

**没做 / 边界**

- TUI 没有附件上传（粘贴/拖拽），ACP 也没有 image prompt（`promptCapabilities.image = false`）；
  两处都留到真正需要时再说，避免先做一套没有验证路径的交互。
- 图片不落盘：只在本次请求的内存里存在；`--image` 的文件仍需用户自己保管。
- 不支持 HEIC/AVIF 等格式；不做缩放/压缩，用户传多大就发多大（只挡 8 MB 上限）。

**验证**：`qcode-providers` 2 条——带图消息序列化成 `content` 数组（text + image_url 的
data URI），纯文本消息仍保持字符串；`cargo test -p qcode-providers --lib` 13 条全绿；
引擎门控与压缩改动由既有 core 测试覆盖（56 条）。

## ADR-0035 补充：TUI 粘贴图片（2026-10-22）

**决定**

1. TUI 里 `Ctrl-V` 读 Windows 剪贴板的 `CF_DIB`，转成 RGBA 后由内置的极简 PNG 编码器
   （`crates/qcode/src/png.rs`）编码——离线环境拿不到 `png`/`flate2`/`arboard`，而截图
   是几帧几百 KB，用 stored（未压缩）deflate 输出的文件偏大但一定合法。
2. 只处理 `BI_RGB` 的 24/32 位位图，两种方向都支持；alpha 一律置 255（`CF_DIB` 的 32 位
   像素 alpha 通常是 0，直接用它会粘贴出全透明图）。其它格式/压缩方式返回明确错误。
3. `Ctrl-V` 只产生 `UiAction::PasteImage`：UI crate 没有剪贴板权限，读取与编码在 CLI 的
   事件循环里做，成功后把 attachment 放进 `pending_images`，下一条消息提交时一并发送
   （`RunOptions.attachments`）。提示行会显示尺寸、KB 与待发数量。
4. 与 `--image` 共用同一条下游路径（`Attachment` → `ContentBlock::Image` → 多模态 parts），
   所以能力门控、压缩、日志限制完全一致。

**边界**

- 仅 Windows；macOS/Linux 的粘贴返回 "only implemented on Windows"。
- 不做缩放/压缩，只受 provider 的体积限制；超大截图会被 8 MB 的 `--image` 同类限制挡住
  （粘贴路径没有单独的大小检查，靠 base64 后 provider 的上限）。
- 非 TUI 前端（ACP）仍没有图片输入。

**验证**：`qcode` 新增 6 条（`decode_dib` 三条：24 位倒序、32 位/正序、拒绝 8 位与压缩；
`png` 三条：CRC32/Adler32 已知向量、PNG 签名与 IHDR/IEND、IDAT 里的 zlib 头与原始行）；
`qcode-tui` 新增 1 条（`Ctrl+V` 映射成 `PasteImage` 且不改输入框）。`cargo test -p qcode --bins`
67 条、`-p qcode-tui --lib` 31 条全绿；clippy 干净。

## ADR-0036：前端与内核的边界——先进程内，后 IPC（2026-10-22）

**背景**：讨论 GUI spike 时提出一个问题——能不能把 TUI / ACP 拆开，让 GUI 探测一个「只带
ACP 的内核」是否在运行，在就直接用、不在就自己起；探测靠端口还是进程名？这个问题值得先
写清楚，因为它决定了 GUI 是一个「进程」还是一个「客户端」。

**决定**

1. **内核 = `qcode-core` + `qcode-tools` + `qcode-net` + `qcode-mcp` + 一次性装配**
   （provider、egress、policy、store、skills）。CLI、TUI、ACP、GUI 都是**前端**；
   `qcode-acp` 是前端的协议适配器，不是内核 API。因此「只带 ACP 的内核」这个说法作废。
2. **stdio ACP 天生一对一**：Zed 拉起 `qc acp` 时给两个管道，进程寿命绑定那一个客户端，
   没有端点可被第二个进程接上。所以「探测有没有 ACP 内核在跑」在 stdio 模型下没有意义，
   即使探测到也无法复用。
3. **第一阶段（GUI spike 期间）不做 daemon**：GUI 是新二进制，在**自己进程内**嵌入内核，
   和 TUI 一样。代价是每个前端各有一份 MCP server、egress 代理与连接池；换来零 IPC、
   零多客户端语义、完全隔离。以当前规模（内核进程几十 MB、启动几百 ms），这个代价小于
   引入 daemon 的正确性成本。
4. **第二阶段是纯重构**：把「装配内核」抽成 `Kernel` 外观（`new(配置)` → `run_turn`、
   `open_session`、`events`），CLI/TUI/ACP/GUI 不再各自拼 `Deps`。这一步不引入 IPC，
   但它决定以后能不能平滑加 daemon。
5. **第三阶段（只有被实测需求触发才做）**：`qc kernel` 守护进程 + 「先连、连不上再 spawn」。
   触发条件必须可测量：GUI 冷启动不可接受（>1s）、MCP server 启动昂贵、或用户明确要求
   TUI 与 GUI 打开同一条会话。若痛点只是「启动慢」，优先做预热/懒加载，而不是 IPC。
6. **发现与传输（如果走到第三阶段）**：
   - 发现用**运行时文件**：Windows `%LOCALAPPDATA%\qcode\run\kernel.json`，Unix
     `$XDG_RUNTIME_DIR/qcode/`，内容 `{pid, start_time, socket, protocol_version, user_sid}`；
   - 传输用**命名管道 / UDS**，不用 TCP。管道 DACL 只放当前用户 SID，UDS 放 0700 目录；
     socket 名带用户 SID，避免多用户撞名；
   - 握手校验 `protocol_version` 与 `pid + start_time`；连接失败、版本不符、pid 已被复用
     都视为过期：删文件、**spawn 新内核**，不杀旧进程。
   - **不用**「固定端口」，也不用「扫进程名」。前者等于把任意代码执行暴露给同机任意进程，
     还会撞端口、触发防火墙；后者只说明「有个 qc 在跑」，拿不到端点且有竞态。
7. **第三阶段之前必须先定的三条语义**（否则一定返工）：
   - **会话归属**：一个 session 只有一个 owner 客户端，其他客户端只读观察；
   - **审批归属**：`session/request_permission` 只发给 owner，不广播（避免「还没看完就被
     另一个窗口点了允许」）；
   - **生命周期**：空闲超时退出、崩溃后重连并 replay、前后端版本不一致时拒绝连接。
8. **ACP 永远保留 stdio-child 模式**：Zed 的设计就是自己 spawn；最多让 ACP 适配器内部
   支持「转发到已存在的内核」，对外仍是 stdio 子进程。
9. **GUI 与 TUI 真正该共享的是状态投影，不是进程**：`qcode-tui` 里的 `App`（事件投影）与
   `ui.rs`（ratatui 渲染）已经分离，GUI 应复用前者。若抽取 `qcode-ui-state` 失败（投影与
   ratatui 耦合太深），那么**投影抽取优先于 GUI，也优先于 daemon**。

**理由**

- ACP 的 stdio 语义决定了它不可能成为共享内核的协议；把 GUI 需求硬塞进 ACP 的形状
  （编辑器会话、权限弹窗）会牺牲 GUI 需要的能力（多会话、并行线程、更细的流式粒度）。
- 多客户端共享会立刻引入 owner/审批/生命周期三个问题；在没有实测需求前解决它们，是在为
  假想负载付复杂度。
- 安全上，本地 IPC 的信任边界是文件系统权限，不是「端口没被发现」。TCP 方案在 Windows 上
  连「同用户」都不保证，直接排除。

**后果**

- GUI spike 期间会有两份内核进程（TUI 与 GUI 各一），MCP server 与 egress 代理各起一份；
  这可接受，且与 ACP 的 stdio 模型一致。
- `Kernel` 外观是后续 daemon 的前置条件；如果一直没做，它仍然让四个前端共用同一装配路径，
  减少「TUI 能跑、ACP 不能跑」这类分叉。
- 本 ADR 的第三阶段是条件性决定：触发条件写在上面，命中时再开新 ADR 记录协议细节
  （帧格式、鉴权、多客户端广播、owner 切换）。

**验证**

- 第二阶段完成后：CLI/TUI/ACP 的所有测试保持不变且通过，且它们只通过 `Kernel` 构造引擎
  依赖（代码评审检查没有第二处 `build_tools`/`build_egress` 装配）。
- GUI spike 的验收项：用同一事件流驱动 egui 版 transcript（复用投影）、三个界面原型
  （流式 transcript 含 CJK/IME、diff 逐 hunk、审批浮层）、1 万事件性能基线、
  产物体积/许可证/Windows 可用性/API 稳定性（egui vs GPUI Kit）。

## ADR-0037：GUI 目标是对齐 DSH 主界面；先做 UI 桥，再做 A/B spike（2026-10-22）

**背景**：GUI spike 的目标升级为「至少达到 DeepSeek Harness 主界面的效果」——一个信息密集、
带 Markdown/代码高亮、工具卡片、审批浮层、流式文本、图片粘贴、用量计费与多面板的界面。
上一轮对比过 egui 与 GPUI Kit；用户批准放开一次网络刷新依赖。

**决定**

1. **先做与框架无关的桥，再选框架**。新增 `crates/qcode-ui-bridge`：内核侧
   `KernelPort`（`send_event`/`send_child`/`send_delta`、`approver()`、`answer()`、
   `take_commands()`）与 UI 侧 `UiPort`（`events`、`commands`）。审批按 `call_id` 路由
   （`UiCommand::Answer` → `KernelPort::dispatch`），不把 oneshot 塞进事件里，这样 GUI 可以在
   任意任务里作答；UI 消失时审批 fail-closed 为 `Deny`，turn 取消时立即返回 `Deny` 而不挂起。
   这个 crate 不知道任何渲染框架，TUI 以后可以改用它，GUI 直接用它。
2. **框架选择走 A/B spike，不辩论定稿**。候选与定位：
   - **GPUI Kit**（Longbridge，Apache-2.0，v0.7.x，[gpui-kit.com](https://gpui-kit.com/)）：
     建在 Zed 的 GPUI 上，声称覆盖 macOS/Windows/Linux/WASM，75+ 组件，自带虚拟列表/数据表、
     Rope+Tree-sitter+LSP 编辑器、Dock、AccessKit 无障碍、headless UI 集成测试。信息密集型
     coding agent 需要的东西基本齐了，是**上限最高的候选**。
   - **egui**：成熟、轻、immediate mode，与「mpsc → 投影」几乎零适配；但没有内置编辑器/Dock/
     数据表，diff 与虚拟化要自研，CJK 需要打包字体。是**风险最低的兜底**。
   - **本地 Web UI（选项 C，暂不做）**：起 loopback 服务把事件流推到浏览器。DSH 本身就是 Web GUI，
     这条路线在产品形态上最接近目标，且天然支持远程/手机；代价是重做一套前端技术栈，并把
     ADR-0036 的「共享内核/IPC」问题提前。**如果原生两条都达不到 DSH 的观感，再启动 C。**
3. **spike 验收标准（提前写死，两天内跑完）**：
   1. 投影复用：`qcode-ui-state` 的 `App` 与 `qcode-ui-bridge` 驱动窗口，不复制投影逻辑；
   2. transcript 灌 10,000 条事件：首次打开 <1s、滚动不掉帧、内存 <300MB；
   3. 中文输入法：`你好，世界` 输入、光标移动、候选窗不遮挡、粘贴多行；
   4. 一个真实 diff（`apply_patch` 的 hunk）可读、可逐 hunk 折叠；
   5. 审批浮层：y/a/n 与按钮两条路径都通，键盘焦点可切换；
   6. 图片粘贴：`Ctrl-V` → `UiCommand::Prompt { attachments }`。
   判定：GPUI Kit 通过 1–6 且 Windows 上无窗口/IME 级坑 → 选它；否则 egui 通过 1–6 → 选 egui
   （diff 与虚拟化作为自研模块）。两条都不通过 → 启动选项 C。
4. **依赖刷新是一次性的、受控的**：执行一次带网络的 `cargo update` + `cargo fetch`，把
   `Cargo.lock` 一起提交；若仍保留 vendored registry，则同步更新该目录并在 README 记录命令。
   刷新后立刻离线验证 `cargo build --offline` 仍可用，避免把离线构建能力弄丢。

**理由**

- DSH 主界面的观感（Markdown、代码高亮、流畅滚动、主题）在浏览器里是最便宜的；在原生框架里，
  要么用组件齐全的 GPUI Kit，要么自己造编辑器与虚拟列表。egui 的强项是集成成本，不是观感上限。
- 桥先行让框架选择变成「可替换的窗口层」：无论选谁，投影、事件流、审批、图片粘贴都已就绪，
  spike 只写窗口代码，失败也不浪费。
- 审批按 id 路由而不是携带 oneshot，是 GUI 与传统 TUI 的关键差异（GUI 的作答发生在另一任务/
  另一帧），先把这层定对，后面不会返工。

**后果 / 风险**

- GPUI Kit 处于 0.7.x，GPUI 本身也在重构，升级要跟；GPUI 有自己的执行器，需要把 tokio 事件
  通过 `cx.notify()` 搬回 UI 线程（本轮桥已经把「谁在哪个线程产生事件」隔离掉）。
- egui 需要内置 CJK 字体，diff 视图要自研，长期维护成本高。
- 选项 C 会把 ADR-0036 的 owner/审批/生命周期问题提前，不走是因为成本，不是因为不认可。
- 一次性网络刷新后，若依赖版本漂移，需要在 CI 里固定 `Cargo.lock` 并跑 `--offline` 验证。

**验证**

- `crates/qcode-ui-bridge` 4 条测试：UI 看到提示并在 `call_id` 上作答后 turn 解除阻塞、
  取消时立即 `Deny` 不挂起、`UiCommand::Answer` 经 `dispatch` 路由（未知 id 为 no-op）、
  事件与 delta 保序。TUI/GUI 共用同一 API。
- spike 的 6 项验收标准如上，结果记入本 ADR 的后续补充。
**补充（2026-10-22，依赖刷新执行记录）**：`crates/qcode-gui` 已加入 workspace 并依赖 `gpui-kit 0.7.3`；
`cargo fetch` 在受限沙箱下因 schannel 报 `SEC_E_NO_CREDENTIALS` 失败，改用一次
`danger-full-access` 后成功（拉取 GPUI/GPUI Kit 依赖树，`Cargo.lock` 已更新）。随后
`cargo build -p qcode-gui --offline` 与既有测试在 `--offline` 下均通过，离线构建能力保持。
刷新命令：`cargo fetch`（一次性、需网络）。
**补充（2026-11-04，依赖刷新执行记录）**：`gpui-kit` 0.8.1 → 0.9.0（`gpui-base 0.9.0`、
`gpui-pre =0.3.11`，zed 快照 `08cbb88` → `f816e1f`）。GUI 源码零改动即可编译运行：
`gpui-kit` 本身 `src` 未变（只换依赖钉版），`gpui-base`/`gpui-component` 的 public 名字
无删除（分别新增 27 / 16 个），`gpui-pre` 只删了 3 个内部重排辅助。
`vendor/gpui-pre-windows` 同步到 0.3.11 并重贴 `IsWindow` 守卫（上游仍无此修复，
`close_one_window` 依旧由 `WM_GPUI_CLOSE_ONE_WINDOW` 驱动延迟剪枝）。升级后必须
`cargo update -p gpui-pre-windows`：vendor manifest 版本与 `gpui-pre` 的 `=0.3.11` 要求不一致时，
cargo 只打一行 `patch … was not used in the crate graph` 警告并静默使用 registry 版本，守卫随之消失。
行为变化集中在 `input/editor`（按行分隔符排版、可配置的 `CursorBlink`）、`MessageScroller`
（padding 计入滚动范围，滚动条能到底）、`PopupMenu`（子菜单在窗口右缘向左翻转）。
`metal 0.33` 已从依赖图移除（改用 `objc2-metal`），`block` 的 future-incompat 警告随之消失。
**补充（2026-11-11，依赖刷新执行记录）**：`gpui-kit` 0.9.0 → 0.9.1（`gpui-base 0.9.1`、
`gpui-component 0.9.1`、`gpui-kit-assets 0.9.1`、`gpui-pre =0.3.12`，zed 快照 `f816e1f` →
`d2a4fb6`）。上游把它定位为 backward compatible patch：不删公共名字，也不再像 0.9.0 那样动
theme 结构体的 `#[non_exhaustive]`，`crates/qcode-gui` 源码零改动即可编译运行（`gpui-base` /
`gpui-component` 只新增 `GridLayout`、`ColumnFixed::Right`、`TransitionState`、
`Text::on_task_toggle` 等）。`vendor/gpui-pre-windows` 同步到 0.3.12 并重贴 `IsWindow` 守卫
（上游仍无此修复，`close_one_window` 依旧由 `WM_GPUI_CLOSE_ONE_WINDOW` 驱动延迟剪枝）；0.3.12
的依赖集合与 0.3.11 完全一致，守卫的落点也没变，只是 `build.rs` 少编一个
`color_text_raster.hlsl`、`platform.rs` 撤掉了 DirectWrite 文本系统的 GPU-lost 挂钩。升级后
同样必须 `cargo update -p gpui-pre-windows`：vendor manifest 与 `gpui-pre` 的 `=0.3.12` 要求
不一致时，cargo 只打一行 `patch … was not used in the crate graph` 警告并静默使用 registry
版本，守卫随之消失。验证：`cargo test --workspace`、`make check`（fmt / clippy -D warnings /
test / cargo-deny）、`make check-platforms`（3 个交叉目标，其中两个 Windows 三元组会真正
type-check 这份 vendor 代码）。
### ADR-0037 spike 结论（2026-10-22）：GPUI Kit 通过

**实测结果**

| 验收项 | 结果 | 证据 |
|---|---|---|
| 1 投影复用 | 通过 | 窗口只持 `App` + 桥 + 输入框；`render` 里无转写状态 |
| 2 一万事件 | 通过 | `tests/spike.rs`：投影 ≈10 ms（预算 500 ms） |
| 2' 虚拟列表 | 通过 | `component::v_virtual_list` 只渲染可见区间 |
| 3 中文 IME | 通过（用户实测） | 组件 `Input` 走平台文本栈，微软拼音可正常输入 |
| 4 diff 视图 | 通过 | `DiffFile::parse` + `DiffState` + `Diff` 渲染真实 patch，不用自研 hunk |
| 5 审批浮层 | 通过 | `UiEvent::Approval` → 三按钮 → `UiCommand::Answer` → `KernelPort::dispatch` |
| 6 图片粘贴 | 通过 | `qcode-image` 读 `CF_DIB` → PNG → `Attachment` 随 `Prompt` 发出（按钮触发） |
| 体积 | 通过 | release ≈24 MB（debug 638 MB） |

**API 漂移清单（0.7.3 实测，供升级时对照）**

- `div().v_flex()`（文档示例）不存在，用 `.flex().flex_col()`；
- `flex_grow` 需要参数：`.flex_grow(1.0)`；
- `v_virtual_list` 只能从 `gpui_kit::component::v_virtual_list` 导入（`gpui-component::virtual_list` 模块是 private）；
- 输入渲染器叫 `Input`（不是 `TextInput`），`InputState` 在 `gpui-base::input::base::state`，构造 `InputState::new(window, cx)`；
- `set_value(value, window, cx)` 的参数顺序是值在前；
- 链接 gpui-kit 的 crate 里放 `#[test]` 会让 rustc **栈溢出**，单测必须放 `tests/` 集成测试（二进制另加 `#![recursion_limit = "512"]`）。

**决定**：GUI 采用 **GPUI Kit**（egui 作为不再启用的兜底，选项 C 的本地 Web UI 留作远程形态）。

**遗留（不影响结论）**

- `Ctrl-V` 键位绑定（现在靠按钮）与 `apply_patch` 结果自动填入 diff 视图；
- 引擎真正跑在窗口后面：需要把 `qc gui` 子命令接上，用 `KernelPort` 驱动一次真实 turn；
- 真窗口下的一万事件滚动帧率/内存未测（投影侧已达标）；
- release 24 MB 的发布策略（qcode 本体 + GUI 分开打包还是合并）。
## ADR-0036 / ADR-0037 收尾状态（2026-10-22）

**ADR-0036 第二阶段完成**：`crates/qcode-kernel` 拥有配置、策略、egress、沙箱、工具、skills、
provider 与 store，并由唯一的 `assemble(...) -> Assembled` 串起来。四个前端都只经它：

| 入口 | 状态 |
|---|---|
| `qc run` | 只经 `assemble` |
| `qc acp` | 只经 `assemble` |
| `qc tui` | 只经 `assemble` |
| `qc gui` | 只经 `assemble`，并把 `Assembled` 转成 `qcode_gui::Engine` |

只读命令（`doctor` / `sessions` / `config` / `update`）仍直接调用 kernel 的 `load_config` 等零件，
它们不构建内核。第三阶段（`qc kernel` 守护进程）按 ADR 的条件继续搁置。

**ADR-0037 验收完成**：六项验收全过（投影复用、1 万事件投影 ≈10 ms、虚拟列表、中文 IME
用户实测通过、真实 patch 的 diff 视图、审批浮层、图片粘贴），release 约 24 MB。GUI 走
GPUI Kit 已定稿，`qc gui` 现在能跑真实回合：引擎线程执行 `run_turn`，事件与审批经
`qcode-ui-bridge` 双向流动，Stop 按钮取消进行中的回合。

**仍未做**（都不阻塞结论）：`Ctrl-V` 键位绑定（当前用按钮）、`apply_patch` 结果自动填进 diff、
一万事件的真实窗口帧率/内存，以及 GUI 与 qcode 本体的打包策略。
## ADR-0038：GUI 打包策略——精简默认，GUI 走可选特性（2026-10-22）

**背景**：`qc gui` 让 `qc` 链接了 GPUI Kit。实测 release 体积：默认构建 **20.2 MB**，
带 `gui` 特性 **46.9 MB**（wgpu / accesskit / tree-sitter 等）。同一个二进制既要做服务器上的
CLI/ACP，又要做桌面窗口，两者需求冲突：服务器不需要 GPU 栈，也不该为它付出构建依赖
（Linux 上还要 X11/Wayland 头文件）。

**决定**

1. **默认构建保持精简**：`qcode-gui` 是 `qc` 的**可选依赖**，由特性 `gui` 打开
   （`qcode-gui = { ..., optional = true }`，`gui = ["dep:qcode-gui"]`）。默认 `cargo build`
   产出 20.2 MB、零 GPU 依赖的 `qc`。
2. **发布两个产物**：`qc`（默认特性）与桌面版（`--features gui`，同一份代码）。桌面版单独命名，
   例如 `qcode-desktop.exe`，避免用户下载 47 MB 却发现只有 CLI。
3. **`qc gui` 在精简构建里给出明确提示**：子命令保留（不被 `cfg` 掉），执行时打印
   「this build has no GUI support; download the desktop build」。当前实现是 `cfg` 掉整个子命令，
   按本 ADR 应改为「保留子命令 + 特性内实现」，避免 clap 报 `unrecognized subcommand`
   让用户以为装错了。
4. **`qcode-gui` 只作为库**：它的 `src/bin/placeholder.rs` 是 spike 遗留，等桌面产物就绪后删除，
   避免出现「两个都能开窗」的第三个二进制。
5. **平台顺序**：Windows 先行（GPUI 的 Windows 后端已在 Zed 上验证）；macOS 需另建构建与
   签名/公证流程（公证要求可执行文件带 hardening）；Linux 走 AppImage 或 deb，并在 CI 里
   明确 GUI 构建需要 X11/Wayland 开发包。
6. **更新机制**：`qc update` 继续只更新精简二进制；桌面版要么自带更新逻辑，要么由安装器
   （MSI/dmg/AppImage）负责。两者共享版本号。

**理由**

- 47 MB 与 GPU 依赖对服务器、CI 与 ACP 使用者都是纯负担；而桌面用户不在乎多下一个 27 MB。
- 用 Cargo 特性而不是两个 crate，保证四个前端（CLI/TUI/ACP/GUI）始终走同一个
  `qcode_kernel::assemble`（ADR-0036），不会分叉成两套代码。
- 保留 `qc gui` 子命令而不是让 clap 报错，是为了让「下载错了」这件事自解释。

**后果 / 边界**

- 发布流水线要产出两个二进制，并在 release notes 里说明差异；
- 精简构建的 `cargo tree` 里不应出现 `gpui*`（可作为 CI 断言）；
- 桌面版体积仍会随 GPUI 版本增长，必要时再评估 `lto = "fat"` / `strip = true` / 静态 CRT 等
  裁剪手段，本 ADR 不做承诺；
- Linux 的 GUI 构建依赖 X11/Wayland 开发包，服务器构建不得引入。

**验证**：`cargo build --release -p qcode` → 20.2 MB 且 `cargo tree -p qcode | grep gpui` 为空；
`cargo build --release -p qcode --features gui` → 46.9 MB 且 `qc gui` 可用（已实测两种构建均通过）。
## ADR-0039：密钥从「命令」取，而不是依赖各平台的环境变量持久化（2026-10-21）

**背景**：Windows 的 `HKCU\Environment` 是用户级、所有新进程可见（包括 GUI），但明文；macOS
没有对应物——`launchctl setenv` 不持久、shell rc 里的变量 Dock 启动的 GUI 读不到；Linux 上
`~/.profile` 只管 shell，GUI 依赖 systemd user environment 或 display manager 的会话文件。
结果是「同一个密钥，三个平台要三种存法，而且其中两种对 GUI 无效」。

**决定**

1. provider 配置增加可选字段 **`api_key_command`**：一个 argv 数组，执行后取 stdout（去尾部
   空白）作为密钥。解析优先级：`api_key_env` → `api_key_command` → 报错（若两者都配，先 env，
   doctor 会提示同时配置了）。
2. 命令**不经过 shell**，直接按 argv spawn，5s 超时；失败时只报告「命令退出码非 0 / 超时」，
   **不打印输出**（输出里可能就是密钥）。
3. 文档给出三平台等价写法：
   - macOS：`["security", "find-generic-password", "-w", "-s", "qcode", "-a", "deepseek"]`
   - Linux：`["secret-tool", "lookup", "service", "qcode", "user", "deepseek"]`（或 `pass show ...`）
   - Windows：Credential Manager / DPAPI 的封装脚本，或 `powershell -NoProfile -Command ...`
4. 仍然保留 `api_key_env`：CI、容器、已有的 `HKCU`/`setenv` 用户继续用；本 ADR 是新增能力，
   不是替换。
5. `qc doctor` 增加一行：报告密钥来源（env 名称 / 命令名 / 未配置），**永不打印值**。

**理由**

- 一套机制覆盖三平台，且天然避开「GUI 读不到 shell 变量」这个坑；
- 密钥存进系统密钥库（Keychain / libsecret / 凭据管理器），比明文注册表或 rc 文件安全；
- 不引 shell 就没有引号与注入问题，超时与错误处理也简单。

**后果 / 边界**

- 命令是用户自己配置的，等同用户执行的程序，不做沙箱限制；
- 每次进程启动会 spawn 一次命令（几毫秒到几十毫秒），不缓存到磁盘；
- 若用户把密钥同时放在 env 和命令里，以 env 为准，doctor 提示重复配置；
- Windows 上仍推荐把值放进凭据管理器，而不是继续用 `HKCU\Environment` 的明文。

**验证**：单元测试覆盖优先级（env 优先、命令回退、两者都缺时的报错文案、命令超时）；
`qc doctor` 能分别报告三种来源。

## ADR-0040：macOS 网络 hard mode（2026-10-21）

**背景**：ADR-0023 的代理是应用级边界，`--noproxy`/raw socket 可以绕过。v2.2 要求
macOS 先用 Seatbelt 把 shell 的出口收敛到代理端口。

**决定**

1. `[network] mode = "off" | "proxy" | "strict"`，默认 `proxy`。`off` 不启动代理；
   `strict` 是 hard mode。
2. `strict` 要求 `[network] enabled = true` 且 `[sandbox] mode = "sandbox-exec"`；其他
   sandbox 后端或 `mode = "none"` 直接拒绝启动，不静默退回应用级代理。`strict` 覆盖
   `[sandbox] network`（后者在 hard mode 下没有意义）。
3. kernel 先启动 loopback egress 代理，再把端口传给 `SandboxNetwork::ProxyOnly`；Seatbelt
   profile 只有 `(allow network-outbound (remote tcp "localhost:<port>"))`，其余网络由
   `(deny default)` 拒绝。SBPL 不接受字面 IP，所以规则写 `localhost`；代理仍监听
   `127.0.0.1`。
4. 作用域是 `run_shell` 子进程（`sandbox-exec` 只套在工具进程边界）；qcode 自身进程和
   MCP server 仍走应用级代理。Unix socket/DNS/直连都被拒，shell 只应使用 `HTTP(S)_PROXY`。
5. `qc doctor` 校验配置组合，并用无网络文件系统 profile 做 preflight；代理端口只在启动时
   才存在。profile 生成有单测；集成测试只在嵌套 `sandbox-exec` 返回 EPERM 时跳过，其它 preflight 失败视为失败，
   非嵌套 macOS 上会实际验证代理端口可达、其它 loopback 端口被拒。

**验证（2026-10-21，非嵌套 macOS）**：同时监听 `127.0.0.1:45678` 与 `:45679`，profile 只放行
`localhost:45678`。`sandbox-exec … nc -z 127.0.0.1 45678` 返回 0；`:45679` 返回 1（connection
denied）；不加 sandbox 时 `:45679` 返回 0。说明 `localhost` 过滤确实匹配 `127.0.0.1`，且直连
其它端口被拒。对应集成测试 `hard_mode_denies_other_loopback_ports`。

**后果**

- 启用后 `curl --noproxy '*'`、raw socket、DoH 在 shell 内应失败；使用代理的 curl/git/npm
  仍可用。代理进程本身不沙箱，allowlist 仍是应用级策略。
- `localhost` 过滤匹配 `127.0.0.1` 已在当前 macOS 验证；若未来系统行为变化，shell 的代理
  连接会被拒，这是 fail closed，不会放行直连。
- Linux 的 netns + relay、Windows WFP 仍未做；`strict` 在这两个平台明确报未支持。

## ADR-0041：Conversation v4 —— Turn 化、展示档位、思考与轮次导航（2026-10-21）

**背景**：GUI 当前把 session 投影成扁平的 `Block` 列表：`User` / `Assistant` / `Tool` /
`Notice` / `Skill` / `Todo` / `Subagent` 同级，没有 Turn、Step 或 Process 层级；`Block` 上
也没有 reasoning。`qcode-ui-state` 的 `apply_delta` 明确忽略 `Delta::Reasoning`，持久化的
`ContentBlock::Reasoning` 也没有进入 transcript 投影，因此“思考”不是显示得不好，而是根本
没有进入 UI 数据。GUI 的 `block_row` 对所有 Block 使用几乎相同的 `px_4 + py_2 +
TextView::markdown` 容器；用户消息、过程、工具输出和最终答案没有排版层级。结果是用户反馈的
“看不清、没有字体变化、没有折叠、没有思考显示、没有轮次选择”。

ADR-0037 已确定 GUI 主界面对齐 DSH。本 ADR 只把 Conversation 的对齐范围定下来；右侧 Dock、
Composer takeover、命令注册表不在这里做，由后续 ADR 分别处理。这样 Conversation 的状态机
可以先独立稳定，不会被输入区和侧栏的改动拖住。

**决定**

### 1. Turn 是 GUI transcript 的顶层单位

Transcript 从 `Block[]` 变为 `Turn[]`：

```text
Turn
  ├─ id / index
  ├─ status: running | completed | failed | cancelled
  ├─ started_at / ended_at
  ├─ input: 用户文本 + 附件
  ├─ process: ProcessItem[]
  ├─ answer: 最终 Assistant Markdown
  ├─ usage: tokens / cost / duration
  └─ deliverables: changed files / presented files
```

`ProcessItem` 至少包含：

```text
Reasoning | ToolCall | Notice | Subagent | Todo
```

现有 `Block` 保留为 TUI 兼容层；GUI 使用新的 Turn 投影。TUI 不要求在同一批改动里迁移，
可以先通过适配器从 Turn 投影生成现有 Block 视图。Turn 的 `id` 必须稳定；折叠、重放、分页
和重新加载后仍能定位同一个 Turn。

### 2. Reasoning 是一等 Block

- 持久化的 `ContentBlock::Reasoning` 进入 Turn 的 `process`；
- 流式 `Delta::Reasoning` 进入 transient reasoning 缓冲区；
- 最终 `AssistantMessage` 到达时，用权威 reasoning 替换流式预览，不能重复显示；
- provider 不返回 reasoning 时不显示该行；
- reasoning 不并入最终回答，不参与“复制答案”；
- 不展示 reasoning signature、加密字段或 provider 原始结构。

Reasoning 行的行为：

- 运行中：`Thinking…` 加轻量状态动画，默认只显示一行状态；用户点击后才展开当前流式内容；
- 完成：`Thought for 8s`，默认折叠；
- `Standard` 不显示 reasoning 预览，只显示标题行；
- `Detailed` 允许展开全文；
- 失败/中断保留已经产生的 reasoning，并标注“未完成”；
- 手动展开状态在展示模式切换时保留。

### 3. 三档展示模式

只做三档，不照搬 DSH 的四档：

| 模式 | 用户输入 | 过程 | 思考 | 工具 | 最终答案 |
|---|---|---|---|---|---|
| Compact | 摘要 | 一行摘要 | 隐藏 | 隐藏 | 显示 |
| Standard（默认） | 显示 | 一行摘要 | 只显示标题，默认折叠 | 收起，可展开 | 显示 |
| Detailed | 显示 | 展开 | 可展开全文 | 摘要 + 输出 | 显示 |

规则：

- 最终答案在任何模式下都显示；
- 当前运行中的 Turn 始终展开过程；
- 已完成 Turn 在 Compact 下整轮收起；
- Standard 下已完成 Turn 的过程收起为一行摘要，例如
  `Thought for 8s · 2 tools · 1 note`；
- 模式只影响 UI，不改变事件、日志、prompt 或 provider 请求；
- 每个 Session 记住自己的模式，写入 `gui.json` 的会话 UI 状态；没有记录时使用 Standard。

### 4. 四层折叠

1. **Turn 折叠**：Compact 下已完成轮次收起；Standard/Detailed 不整轮收起。
2. **Process 折叠**：完成后收起为一行摘要；运行中展开；失败/拒绝/中断自动展开对应失败项。
3. **Tool 折叠**：完成默认收起；失败/拒绝/中断默认展开；展开输出最大高度 320px，超出提供
   “Open full”。
4. **Code 折叠**：超过 20 行的代码块默认显示 12 行和 “Show N more lines”；复制按钮常驻。

折叠规则：

- 切换展示模式保留用户手动展开/收起的 Turn、Process、Tool 和 Code 状态；
- 新 Turn 使用当前模式的默认值；
- 折叠/展开不能把用户正在阅读的内容顶走，必须保持阅读锚点；
- 折叠只是显示层，不删除事件、不改变复制内容和模型上下文。

### 5. 排版 token

Conversation 使用独立排版层级，不再复用同一套 `TextView` 容器：

| Token | 用途 | 建议值 |
|---|---|---|
| text | 正文 / 助手回答 | 14 px / 22 px |
| text-secondary | 过程摘要、时间、usage | 13 px / 20 px |
| text-caption | Turn 编号、状态、脚注 | 11 px / 16 px |
| code | 代码块、工具输出 | 12 px / 18 px |
| heading-1/2/3 | Markdown 标题 | 相对正文 1.5 / 1.25 / 1.1 |
| inline-code | 行内代码 | 0.9em 正文 |

布局规则：

- 内容最大宽度 860–900 px；
- 段落间距 8 px，过程行间距 6 px，Turn 间距 24 px；
- 用户消息使用浅背景气泡；助手回答不套气泡，保持阅读感；
- 助手回答与过程行之间留 12 px；
- 工具输出、路径、usage 使用等宽字体；
- 错误、警告、成功使用语义色，不只依赖红点；
- 正文不使用大量色块，层级主要靠字号、字重、间距和弱化色。

### 6. 轮次选择与导航

**Header 选择器**：

```text
Turn 3 / 12 · 16:20 · 8s · $0.0012
```

点击打开 Turn 列表，支持搜索和跳转。

**Turn 轨道**：

- Conversation 右侧窄轨道；
- 每个 Turn 一个刻度；
- 当前 Turn 高亮；
- 失败 Turn 使用警告色；
- hover 预览：用户输入前 50 字、答案前 3 行、时间/耗时/工具数；
- 点击滚动到稳定的 Turn 锚点；
- 内容宽度小于 900 px 时隐藏轨道，Header 选择器仍可用。

**键盘**：

- `Cmd+Option+↑/↓` 上一轮 / 下一轮；
- `Cmd+Option+Home/End` 第一轮 / 最后一轮；
- `Esc Esc` 停止当前 Turn；
- 跳转后焦点留在 Conversation，不抢 Composer。

**滚动**：

- 只有用户位于底部时才自动跟随；
- 用户上滚后暂停跟随，显示 “Jump to latest”；
- 折叠/展开/切换模式保持当前阅读锚点；
- Turn 跳转使用稳定 Turn ID，不使用行号；
- 分页加载更早历史时，锚点补偿优先于几何位置。

### 7. 架构边界

- `qcode-ui-state` 增加 Turn / ProcessItem / Reasoning / Usage / Deliverables 投影；
- `qcode-gui` 增加 `ConversationController`：展示模式、折叠状态、轮次导航、滚动锚点；
- `TurnList`、`TurnRow`、`ProcessGroup`、`ReasoningRow`、`ToolCard`、`TurnRail`、
  `TurnPicker` 作为独立组件，不继续堆在 `Shell::block_row` 里；
- `MessageScroller` 继续承担可变高度虚拟列表，但行粒度从 Block 变为 Turn/ProcessRow；
- GUI 不直接改模型请求；本 ADR 全部是 UI-only，对 KV cache 无直接影响。

### 8. 范围与非目标

本 ADR 明确包含：

- Turn/Process/Reasoning 投影；
- 三档展示模式；
- 四层折叠；
- 排版 token；
- Header 轮次选择、Turn 轨道、键盘导航、阅读锚点。

本 ADR 明确不包含：

- 右侧 Dock；
- 完整插件/slot 系统；
- Workspace 树形、pin/archive/fork；
- Trajectory 独立视图；
- Composer takeover、审批/问题/计划队列；
- 命令注册表；
- 修改 session 日志格式或模型请求。

右侧 Dock 不做；文件/diff 继续使用底部 diff 抽屉和文件预览。  
Session 侧栏只做简化版：单层列表、状态点、搜索、展开更多；不做树形/pin/archive/fork。  
这些边界避免 Conversation v4 变成一次 GUI 全量重写。

**理由**

- 没有 Turn 模型，折叠和轮次导航只能靠特判，最终会变成不可维护的行号逻辑；
- Reasoning 是用户明确要求的能力，必须在投影层保留，不能只在 UI 里临时拼接；
- 排版层级是“看不清”的直接解药，比继续加按钮更有效；
- DSH 的强项是渐进披露和稳定导航，不是某个具体组件；
- 先做 Conversation，再做 Composer takeover 和命令注册表，避免同时改三个状态机；
- 三档模式、四层折叠和两处轮次入口已经覆盖主要使用场景；四档和完整 DSH slot 系统的
  边际收益不足以抵消复杂度。

**后果 / 边界**

- `qcode-ui-state` 的投影和测试会明显增加；
- TUI 需要保持兼容，不能强制迁移；
- Reasoning 会增加内存和渲染量，需要沿用可变高度列表、折叠和虚拟化；
- 折叠/展开必须保持锚点，否则会引入“内容跳动”问题；
- 展示模式需要持久化到 `gui.json` 的会话 UI 状态；
- 不改变 session 日志格式，但 UI 投影需要新增字段；
- 需要为 Turn 投影补投影测试、折叠状态测试、锚点/跳转测试和 10 万事件性能回归。

**验证**

1. 一个包含 reasoning、2 个工具和最终答案的 Turn，在 Standard 下：
   - 过程显示一行摘要；
   - `Thought for 8s` 默认折叠，不显示 reasoning 预览；
   - 最终答案可见；
   - 点击摘要/思考标题可展开 reasoning 和工具；
   - reasoning 先流式出现，最终被权威内容替换且不重复。
2. Compact 下过程隐藏，最终答案仍可见。
3. Detailed 下过程展开；失败工具自动展开。
4. N 个 Turn 的 Header 选择器和轨道数量一致；点击和键盘跳转正确。
5. 折叠/展开/切换模式不改变阅读位置。
6. 10 万事件投影测试仍通过；不产生额外模型请求。
7. 关键控件有 accessibility label；跳转后焦点不丢失。

**后续 ADR**

- ADR-0042：Composer 与 Pending Interaction  
  Composer takeover、Approval/PlanReview/Question 队列、Queue/Steer、`Cmd+Enter` 语义。
- ADR-0043：Command Registry  
  `/display`、`/thinking`、`/model`、`/permission`、`/plan` 的统一注册、可用性与“不静默退化”。
- ADR-0044：Session 浏览器最简  
  当前 workspace 的平铺列表；不做搜索、状态聚合、树形、pin/archive/fork。

## ADR-0042：Composer 与 Pending Interaction —— 一个输入面、一个待处理队列（2026-10-21）

**背景**：qcode 当前的 Composer 是“Textarea + Send/Stop”，运行中直接拒绝发送；审批是
Composer 上方的一张独立卡片；没有排队、插话、草稿持久化，也没有计划审阅和用户提问的统一
位置。ADR-0041 已把 Conversation 改为 Turn 模型，但 Turn 运行时会等待用户决策；如果审批、
计划、提问仍然散落在不同浮层，Composer 就无法成为真正的交互控制面。

DSH 的核心做法不是“审批做得更漂亮”，而是把 Approval / PlanReview / Question 都建模成
**Pending Interaction**：同一时间只有一个 active request，附着在 Composer 位置，答复后恢复
编辑器与焦点，请求本身属于 Session 投影。本 ADR 采用这个协议，但把 Steer 放到 P1，先保证
Queue 和审批可靠。

**决定**

### 1. Composer 的四种状态

Composer 由 `ComposerController` 拥有，状态为：

```text
Idle           可编辑，可发送
Running        turn 正在运行；可 Queue（P0）或 Steer（P1）
Pending        一个 Pending Interaction 占用 Composer
Restoring      决策已提交，等待 Host/engine 确认，编辑器尚未恢复
```

- `Pending` 时默认编辑器隐藏，显示交互卡片；草稿、选区、附件仍然保留，不丢失。
- 决策提交后进入 `Restoring`，只锁定该交互相关的提交，不阻塞窗口其他部分。
- 交互关闭/超时/取消后恢复编辑器、草稿和焦点；恢复时不能把光标跳到开头。
- 运行中的 Composer 仍然可编辑；发送动作根据状态变成 Queue 或 Steer，而不是直接拒绝。

### 2. Pending Interaction 统一队列

`PendingInteraction` 至少有三种：

```text
Approval      工具审批
PlanReview    计划审阅
Question      ask_user_question 类提问
```

统一字段：

```text
id / kind / session_id / created_at / deadline?
payload（tool、argv、cwd、rule、plan、questions…）
actions（允许/拒绝/要求修改/提交答案…）
```

规则：

- 每个 Session 一个 FIFO 队列，同一时间只有一个 active interaction。
- 同一 `id` 重复到达只保留一份；已关闭/已过期的请求不能再次回答。
- 新请求到达时，如果当前请求尚未结算，进入等待队列；侧栏/Composer 显示“还有 N 个待处理”。
- 当前请求结算后自动推进下一个，不依赖用户重新打开。
- Turn 取消/失败时，关闭该 Turn 的所有 pending interaction；未回答的按 fail-closed 处理。
- 交互请求属于 Session 投影；切换 Session 再回来仍能看到未完成请求。

### 3. Approval

- 占用 Composer 位置，显示 tool、summary、reason、rule、argv、cwd、sandbox。
- 操作：`Allow once` / `Always (this session)` / `Reject`。
- 键盘：`Enter` 允许一次，`Esc` 拒绝；保留 `Y/A/N` 作为别名。
- 审批只做临时决定；长期规则仍由配置/审批策略拥有，UI 不改规则文件。
- 多个审批按 FIFO 排队；一次只展示一个；已取消的请求立即从队列移除。
- 审批失败/超时 fail-closed；UI 不伪造“已允许”。

### 4. PlanReview

- 计划正文在 diff/plan 抽屉中打开；Composer 位置显示紧凑审批卡片。
- 操作：`Request changes` / `Approve`。
- `Request changes` 恢复编辑器，让用户直接输入修改意见，不提交批准。
- 计划批准是一次显式决策，不因为关闭卡片而视为批准。
- 关闭卡片只隐藏，不丢失请求；可从 Turn 末尾的计划卡片重新打开。

### 5. Question

- 支持单选、多选、自由文本、跳过、分页。
- 计时请求允许倒计时；聚焦、编辑或选择“慢慢回答”会暂停/转为无限等待。
- 提交一批答案后才算回答；关闭不是回答。
- 结束后可从对应工具行重新打开只读记录；未提交草稿按 Session 保存。
- 超时按既定协议拒绝，并将 Composer 恢复到可编辑状态。

### 6. 发送与运行中投递

保留 qcode 现有 `Cmd+Enter` 发送习惯，同时允许后续切换：

| 状态 | 主操作 | 键盘 |
|---|---|---|
| Idle | Send | `Cmd+Enter` 发送；`Shift+Enter` 换行 |
| Running | Queue（默认） | `Cmd+Enter` 按 `busy_delivery` 投递；`Cmd+Shift+Enter` 使用互补模式 |
| Running + Steer 能力 | Queue / Steer | 按钮或命令切换 |
| Pending | 由交互卡片决定 | 交互自己的 Enter/Esc/方向键 |

- `send_mode` 设置：默认 `cmd_enter`（当前行为）；可选 `enter`（Enter 发送、Cmd+Enter 互补）。
  两种模式都必须保留 `Shift+Enter` 换行。
- `busy_delivery` 设置：`queue`（默认）或 `steer`；Steer 只有在 engine 声明支持时显示。
- Queue 是 P0；Steer 是 P1，先保留协议和按钮位置，不伪造能力。
- 运行中主按钮在 Send / Stop / Queue / Steer 之间切换；Stop 始终可用。
- 双 Esc 停止当前 Turn，并保留排队消息。

### 7. Queue / Steer 语义

**Queue**

- 消息在 Composer 上方显示为待发送行，可编辑、删除、重新排序。
- 队列消息不立即进入 transcript；被 engine 接收后才产生本地回显和权威消息。
- Stop 后队列保留；用户可以继续编辑或清空。
- 同一时间只有一个 in-flight turn；队列按 FIFO 逐步投递。

**Steer（P1）**

- 在当前 turn 的下一个 step 边界注入；不保证在任意工具执行中途生效。
- 未注入前显示 pending 状态，可取消；已注入后按普通消息进入记录。
- engine 不支持时隐藏 Steer，不显示假按钮。

### 8. 乐观发送与失败恢复

- 发送时立即清空编辑器并显示本地回显；回显带 request id。
- 权威消息到达后原子替换回显；不能重复显示。
- 失败时按提交顺序恢复草稿；保留用户随后输入的文字与附件。
- 同一 request 的重复回显只结算一次。
- 切 Session/刷新后，未提交草稿按 Session 保留；已提交未确认的请求显示“发送中”。

### 9. 焦点与 IME

- Composer 启动时聚焦；发送/New session/resume 后重新聚焦。
- Pending 卡片出现时，只有卡片区域获得焦点；关闭后恢复到原草稿光标位置。
- IME 组合输入期间，Enter 只确认候选，不触发发送/审批。
- 关键控件有 accessibility label；审批/问题支持键盘导航与焦点恢复。

### 10. 协议与所有权

- engine/bridge 发出 `InteractionOpened` / `InteractionClosed` / `InteractionResolved`；
  UI 只维护队列与展示，不推断“是否仍可回答”。
- UI 发送 `Answer { id, decision }`、`Cancel { id }` 等命令；未知/过期 id 为 no-op。
- 取消 turn 时 engine 负责关闭所有 pending；UI 不永久卡在 `Pending`。
- 所有 pending interaction 与 turn 生命周期绑定；Turn 结束后不留下可回答的旧卡片。

**范围 / 非目标**

- 不做右侧 Dock；Plan 全文用现有 diff/plan 抽屉/弹层。
- 不做动态插件化 Composer；先固定四种状态和三种 interaction。
- 不做多客户端审批归属；P0 是单窗口。
- 本 ADR 不定义命令注册表；`/model`、`/permission`、`/display` 见 ADR-0043。
- 不改变现有审批策略与 fail-closed 语义。

**理由**

- Composer 是用户最常看的区域；把决策放在这里，比在多个浮层之间切换更接近 DSH 的体验。
- 一个队列 + 一个 active request，消除了“多个审批互相覆盖”“运行中无法回答”的旧问题。
- Queue 先于 Steer，是因为 Queue 不需要 engine 在 step 中途注入，风险低、可测试。
- 保留 `Cmd+Enter` 作为默认，避免再次改变用户已经习惯的 qcode 行为；`enter` 模式留为设置。
- Pending Interaction 属于 Session 投影，才能做到切会话、恢复、重开、超时一致。

**后果**

- Composer 从“输入框 + 两个按钮”变成显式状态机，测试量增加。
- 需要 engine/bridge 补 interaction 协议；现有 `UiEvent::Approval` 需要升级而不是继续打补丁。
- Queue 消息、草稿、待处理请求都需要持久化/恢复策略。
- Steer 需要 engine 的 inbox/step 边界支持；未实现前必须隐藏，不能假装可用。
- Composer takeover 会改变审批 UI 的布局和焦点测试。

**验证**

1. 运行中到达审批 → Composer 显示审批卡片 → Enter 允许 → turn 继续并完成。
2. 同一 turn 到达两个审批 → 一次只显示一个；回答后自动推进；重复 id 不重复显示。
3. 审批期间切 Session 再切回 → 请求仍在；草稿未丢。
4. Cancel turn → 所有 pending 关闭；Composer 恢复可编辑。
5. 运行中输入并 Queue → 当前 turn 结束后按 FIFO 投递；Stop 后队列仍保留。
6. Idle `Cmd+Enter` 发送；`Shift+Enter` 换行；IME 组合期间 Enter 不误发。
7. 发送失败 → 草稿恢复且不重复回显。
8. 审批 Enter/Esc、Y/A/N、焦点恢复和 accessibility label 通过。

## ADR-0043：Command Registry —— 斜杠命令、选择器与命令 chip（2026-10-21）

**背景**：qcode 现在没有斜杠命令；模型、权限、显示模式等能力只能通过按钮或配置文件进入。
用户希望引入命令注册表，并对齐 DSH 的 Composer 命令体验。DSH 的关键点不是“支持 `/`”，
而是：命令有类型、会话级目录、可用性、键盘导航、命令 chip、不静默退化为普通提示词。

**决定**

### 1. 注册表

新增 `CommandRegistry`，命令条目至少包含：

```text
id / title / description / aliases / icon / category
kind: Action | Insert | Picker | Backend
availability(session) -> enabled | disabled(reason)
argument_hint?
```

- `Action`：纯 UI 动作，例如显示模式、打开轮次选择，不进入 transcript。
- `Insert`：把文本/引用/命令 chip 插入草稿，不立即执行。
- `Picker`：打开选择器，例如模型、权限、显示模式。
- `Backend`：通过 `UiCommand` 交给 engine/kernel，例如 compact、plan、goal。
- 同一 id 只能注册一次；别名冲突显式报错。
- 注册项有 owner 与生命周期；移除 owner 时命令和菜单项一起移除。

### 2. `/` 触发

- 只在 Composer 开头或空白后触发；URL、路径、普通文本中的 `/` 不打开菜单。
- 菜单按类别分组，支持模糊搜索、前缀优先；空查询显示常用命令。
- 键盘：`↑/↓` 移动，`Tab`/`Enter` 接受，`Esc`/`Shift+Tab` 关闭；指针选择等价。
- `/` 菜单和 `@` 引用共用同一套 trigger/overlay，但 source 与解析器分开。
- 菜单打开期间 Composer 保持焦点，使用 `aria-activedescendant` 报告高亮项。

### 3. 命令 chip

- 已认领的命令是草稿开头的一个原子 chip，后面可以带普通参数。
- chip 与认领同生共死：删除 chip 恢复字面 token；撤销恢复 chip 与认领。
- 刷新/切 Session 后，按持久化的命令 token 重新解析；目录未就绪时保留字面 token。
- 命令名与参数之间用空格分隔；命令参数仍按普通文本编辑。
- chip 不改变命令的模型可见语义；是否进入日志由命令 owner 决定。

### 4. 不静默退化

- 已输入 `/` 且没有匹配命令时，不自动当普通提示词提交。
- Composer 显示 `Unknown command` 内联提示，Enter 不发送；用户可以：
  - 删除 `/` 改为普通文本；
  - 或显式选择“Send as text”。
- 命令存在但当前不可用时，菜单显示禁用与原因，Enter 不执行。
- 命令执行失败显示内联错误，不把命令文本悄悄发给模型。

### 5. 初始命令集

P0 只注册以下命令：

| 命令 | 类型 | 作用 |
|---|---|---|
| `/display compact|standard|detailed` | Action | 切换 Conversation 展示档位 |
| `/thinking on|off` | Action | 显示/隐藏思考行（可选） |
| `/turns` | Action | 打开轮次选择器 |
| `/stop` | Action | 停止当前 turn |
| `/model` | Picker | 选择模型/推理强度 |
| `/permission` | Picker | 选择会话权限预设 |
| `/help` | Action | 打开命令速查 |

后续接入：

- `/plan`、`/goal`、`/compact` 走 Backend 命令；
- 不把未实现的命令放进菜单，避免“看起来能用”。

### 6. 命令目录与可用性

- 命令目录按 Session 解析；不可用命令不显示或显示禁用原因。
- `Backend` 命令需要 engine 能力；能力缺失时不注册。
- UI 命令不改变模型请求，不写入 session 日志。
- Backend 命令产生的事件由 owner 负责；命令本身不直接构造 prompt。
- 命令菜单和快捷键速查共用同一份命令名称/别名，避免两套词表。

### 7. 选择器

- `Picker` 命令打开 Composer 内嵌弹层，不新开窗口。
- 选择器持有一份目录，支持搜索、分组、键盘导航、当前值高亮。
- 选中即提交；取消不改变当前值。
- 选择失败保留菜单与当前值，并显示可重试错误。
- 模型/权限选择只在下一 turn 生效；运行中的 step 保留启动时的值。

### 8. 持久化与历史

- 草稿中的命令 chip token 随 Session 草稿保存。
- 不要求命令历史；常用排序可以用静态 order。
- 命令目录刷新时，已认领 chip 重新解析；解析失败退化为字面 token，不执行旧命令。

**范围 / 非目标**

- 不做动态插件市场或第三方命令包。
- 不做完整 DSH 的 `popupSelect` 装饰/宿主命令生命周期；qcode 先做四类命令。
- 不做 `@` 引用的完整文件/会话目录；引用触发只预留同一管线，后续单独 ADR。
- 不改变模型请求；命令是否模型可见由 owner 决定。
- 不在本 ADR 做 Composer takeover；见 ADR-0042。

**理由**

- 命令注册表把“模型、权限、显示模式、轮次”从散落按钮收进一个可发现入口。
- 命令 chip 让命令可读、可编辑、可恢复，不会刷新后变成一段陌生文本。
- “不静默退化”避免用户以为执行了命令，实际却把 `/model` 当提示词发给模型。
- 四类命令足以覆盖 P0，避免一开始就做 DSH 的完整命令/装饰体系。

**后果**

- Composer 的 trigger/overlay/chip 状态增加，需要与 Pending Interaction 协调。
- 命令可用性依赖 Session 能力，UI 需要处理目录刷新和失败。
- 需要命令注册、别名冲突、键盘导航、chip 恢复的测试。
- 命令和快捷键速查要共享词表，避免命名不一致。

**验证**

1. 输入 `/display` 打开菜单，选择 Standard 后 Conversation 档位切换，且不发送模型请求。
2. `/model` 选择器按 provider 分组、搜索、键盘选择；选择只在下一 turn 生效。
3. `/permission` 切换当前 Session 权限；高风险选项要求确认。
4. 输入未知 `/foo`：显示 Unknown command，Enter 不发送；删除 `/` 后可正常发送。
5. 命令 chip 在切 Session/刷新后恢复；删除 chip 后恢复字面 token。
6. 不可用命令显示原因，Enter 不执行；选中后不发生静默提交。
7. 命令菜单、选择器、Esc/Enter/Tab 导航与 accessibility label 通过。

## ADR-0044：Session 浏览器最简——冻结当前 workspace 平铺列表（2026-10-21）

> 2026-10-21 追加：Task/Root 方向确认后，本 ADR 的 workspace 平铺列表只作为过渡实现；
> 新的领域模型见 ADR-0047。不要继续在这个浏览器上增加树形、搜索或 workspace 分组。

**背景**：DSH 的 Session 浏览器有分组、树形、pin、archive、fork、搜索、状态点、水位线、
拖拽排序、subagent 目录和 Schedule 标记。这些能力依赖一套稳定的 Workspace/Session 领域
模型；而 qcode 后续还会发生重大需求变化（工作区、远程、会话归属与协作形态都可能重做）。
现在把 Session 浏览器做深，等于把未来的领域模型提前锁死。

因此本 ADR 明确：**Session 浏览器冻结在最简形态，只服务当前单窗口、单 workspace 的使用。**
它不是最终形态，也不为未来的树形/搜索/协作预留 UI 抽象；需求变化时重新写 ADR，而不是在
这个浏览器上继续加特判。

**决定**

### 1. 只做当前 workspace 的平铺列表

- 侧栏只列 **当前 workspace** 的 Session；不列其他 workspace，不做多 workspace 同屏。
- 单层平铺，不分组、不树形、不折叠 workspace。
- 按最近活动时间倒序；不做手动排序、置顶、拖拽、水位线。
- 不做 subagent 子会话、Schedule 标记、未读完成提醒或后台 job 聚合。

### 2. 行内容只保留最小信息

每行只显示：

- 首条 user message 的标题；
- 相对时间。

不再扩展：

- turn 数、cost；
- 运行状态点、pending 审批点、unread/done 点；
- token 用量、provider/model；
- hover 卡片、上下文菜单、行内操作按钮。

当前已有 `turns/cost` 字段可以保留在数据层，但浏览器 UI 不再把它们作为主要信息；后续如果
需要，必须先改本 ADR。

### 3. 操作只有三个

- **New session**
- **Open workspace**
- **点击行 Resume**

规则保持现状：

- 运行中禁止 Resume/New session，先 Stop；
- 空 Session 在新会话页面处理；
- 当前 active Session 仍用 `●` 标记；
- Resume 后清空旧 transcript、重置 scroller、恢复输入焦点。

不做：

- rename、pin、archive、fork、delete、复制 ID；
- 搜索（标题/内容）；
- 右键菜单、行内 hover 操作；
- 多选、拖拽、批量操作。

### 4. Workspace 保持最小

- Header 只显示当前 workspace 路径。
- 保留 `Open workspace…` 原生选择器。
- 不做 workspace 树、最近列表、重命名、删除、归档、分组顺序。
- 如果现有最近 workspace 列表已经存在，可以保留为简单入口，但不再扩展；未来的 workspace
  模型变化时一并重做。

### 5. 不新增浏览器专用领域抽象

- 不引入 `SessionBrowser`、`WorkspaceTree`、`SessionStatus` 等新模型。
- 不扩展 `SessionSummary` 以便浏览器显示状态；浏览器只读 `Backend::sessions()` 和现有
  `title()`。
- 不把 DSH 的 slot/registry/watermark 搬过来。
- 侧栏不持久化布局、排序、展开状态、搜索历史。

### 6. 状态与 pending interaction 的边界

- 当前 Session 的 pending interaction 只出现在 Composer（ADR-0042），不聚合到侧栏。
- 如果用户切走，pending 请求仍属于该 Session；切回来时 Composer 会恢复。侧栏不显示“待审批”
  点，避免为此提前引入跨 Session 状态投影。
- 运行中标记只标记当前 active Session；不计算所有 Session 的运行状态。

**理由**

- Session/Workspace 领域模型还会大改；现在做树形、搜索、pin/archive 会迫使未来兼容旧 UI。
- 单窗口本地 P0 的真实需求是“新建、切换、继续”，不是管理几百个会话。
- 少做浏览器，可以把预算放在 Conversation（ADR-0041）和 Composer（ADR-0042）这两个直接
  影响日常使用的面上。
- 不引入状态聚合，避免为了侧栏点而提前改 `SessionSummary`/projection。
- 明确的“冻结”比含糊的“简化”更容易在 review 时拒绝范围蔓延。

**后果 / 边界**

- 会话多时只能滚动；没有搜索和归档，属于已知限制。
- 切走带 pending 的 Session 后，侧栏不会提示；用户必须切回该 Session 才看到 Composer 中的
  请求。
- 没有 workspace 最近列表/树形；打开其他 workspace 需要原生选择器。
- 未来若引入远程、多 workspace、团队协作、会话分享或大规模历史，必须重新写 ADR；不要在本
  ADR 上追加例外。

**非目标**

- DSH 的 Workspace 分组/树形/拖拽/水位线；
- pin、archive、fork、rename、delete、复制 Session ID；
- 标题/内容搜索；
- 状态点、未读、Schedule、subagent 目录；
- 多窗口/多 workspace 同屏；
- 任何浏览器专用持久化布局。

**验证**

1. 侧栏只出现当前 workspace 的 Session，按最近活动排序。
2. 行只显示标题和相对时间；不显示 turn/cost/状态点。
3. New session、Open workspace、Resume 三个动作可用；运行中 Resume/New session 被拒绝。
4. 没有搜索框、没有右键菜单、没有 pin/archive/fork/delete 入口。
5. 切换 workspace 后只列新 workspace 的 Session；旧列表不混入。
6. 侧栏不因 pending interaction 增加额外计数/标记；切回 Session 后 Composer 仍显示请求。
7. 现有 Session 相关测试保持通过；无新增浏览器专用后端字段。


## ADR-0045：GUI 的 Always 真正生效，默认预算提高到 64（2026-10-21）

**背景**：qcode GUI 的审批面板有 `Always (this session)`，但 bridge 只把答案映射成
`Answer::AllowSession`，engine 又把它当成普通 `Allow` 处理；只有 TUI 的 approver 自己维护了
`session_allowed`。结果是用户点多少次 Always，下一条 `run_shell` 仍然会再问。

同时，`[agent] max_turns` 的默认值是 8；一次项目分析很容易耗尽，用户机器上的配置又被设成
16，导致 turn 以 `exceeded max turns (16)` 失败。DSH 的长任务不是按 16 次 provider call 切段。

**决定**

1. `BridgeApprover` 维护会话级工具 allowlist。用户在 GUI 选择 `Always` 后，同一 Session 内
   同名工具直接返回 `AllowSession`，不再入队审批。
2. 已排队的同工具并行请求一并结算为 `AllowSession`，并发送 `ApprovalClosed`，避免用户回答
   完一个后还要回答其余同工具请求。
3. `NewSession` / `Resume` 清空该 allowlist；allowlist 不跨 Session。
4. `[agent] max_turns` 默认值从 8 提到 **64**。显式配置仍优先；`--max-turns` 仍可覆盖。
5. 这是提高上限，不是自动续跑；超过 64 仍以 `max_turns` 失败，用户可继续发消息或调高。

**非目标**

- 不做命令前缀 / 路径级的会话允许；`Always` 仍按工具名，和 TUI 一致。
- 不做 allow-always 持久化；会话结束即失效。
- 不做命中上限自动续一个预算窗口；自动续跑需要单独的 goal/round 设计。
- 不改变 engine 的 fail-closed、审批事件记录或 TUI 行为。

**后果**

- `run_shell` 点一次 Always 后，该 Session 内所有 shell 命令都不再问；这是方便与风险的直接
  交换，需要用户理解。
- 默认 64 次模型调用会增加失控任务的成本上限；`max_cost_usd` 和无进展检测仍是兜底。
- GUI 的 Always 与 TUI 的 `session_allowed` 语义一致。

## ADR-0046（草案）：审批 Grant 模型——Once / Turn / Session 与匹配粒度（2026-10-21）

**状态**：提议，待确认；本轮只设计，不实现。

**背景**：GUI 的 `Always` 修复后按工具名记住，`run_shell` 一旦允许，本 Session 内所有
shell 命令都不再询问。它减少了弹窗，但把“信任一条命令”放大成“信任整个 shell 工具”。
同时，分析项目时 qcode 仍然每条 `run_shell` 都可能 Ask；DSH 的审批次数少，主要靠 Session
permission/sandbox 和 `run_code` 的整段授权，而不是更多 Always 按钮。

本 ADR 只讨论 **grant（可复用授权）** 的模型，不讨论 sandbox 后端、组织级策略、远程审批、
自动续跑，也不实现代码。

**术语**

- **Approval**：一次工具调用的待决请求，对应一个 `call_id`。
- **Grant**：可复用的审批决定，由 `scope + matcher` 组成。
- **Scope**：`Once` / `Turn` / `Session`。
- **Matcher**：`tool`，以及可选的 `command_prefix` / `path_glob`。
- **Preset**：会话级权限档位（只读 / workspace-write / full-access），不是 grant。

**目标**

1. 降低同一 Turn、同一工具族的重复审批。
2. 不把“允许一条命令”静默放大成“允许所有命令”。
3. 多前端（GUI/TUI/ACP）共享同一套 grant 语义与审计。
4. 失败、取消、并行、切 Session 时行为可预测。

**非目标**

- 不实现 OS sandbox 或权限预设（另立 ADR）。
- 不做组织级策略、远程审批、审批委托。
- 不做 allow-always 的跨设备/跨用户持久化。
- 不做 run_code / PTC 的嵌套工具授权。
- 不改变 deny 优先、fail-closed、取消即拒绝的现有语义。

## 选项 A：Turn 级工具 Grant

**决定形态**

- 只有 `Turn` 作用域；matcher 只有 `tool`。
- 用户在审批面板可选：`Allow once` / `Allow this tool for this turn`。
- Turn 结束、取消、失败时清空；不跨 Turn、不跨 Session。
- 已排队的同 Turn 同工具请求一并结算。

**优点**

- 最小改动；不需要新的持久化 schema 或匹配器。
- 对“读文件、列目录、搜索”这类低风险高频工具很有效。
- 行为容易解释：只在本 Turn 内有效。

**缺点**

- 对 `run_shell` 仍然过宽：本 Turn 内所有 shell 命令都放行。
- 每个新 Turn 仍要重新授权；长任务仍会反复问。
- 不能表达“允许 `git status` / `cargo test`，但不允许 `git push`”。

**工作量**：约 1–2 天 / 1–2 轮。

**适用**：先缓解高频读工具；不适合作为最终模型。

## 选项 B：Scoped Grant（推荐目标）

**决定形态**

- 作用域：`Once` / `Turn` / `Session`。
- Matcher：`tool` + 可选 `command_prefix` 或 `path_glob`。
- UI 决策按风险分层：
  - 默认：`Allow once`
  - 低风险读工具：`Allow tool for this turn`
  - shell：`Allow this command prefix for this turn` / `for this session`
  - `Always this tool`：保留，但标为高风险，不默认推荐给 shell
- `Session` grant 只存在当前活动 Session 的运行时；**不跨 resume 自动恢复**。恢复后重新询问。
- 一个 grant 命中后，已排队的匹配请求一并结算并写审计。
- matcher 复用现有 `Rule` 的 `command_prefix` / `path_glob` 语义，不引入正则。

**示例**

```text
Allow once
Allow "git status" for this turn
Allow "cargo test" for this session
Allow read_file for this session
Allow all run_shell for this session  [danger]
```

**优点**

- 能真正减少审批，又不把整个 `run_shell` 放行。
- 与现有 policy 的 matcher 语义一致，迁移成本低。
- 可以逐步实现：先 Turn + tool，再加入 prefix/path 和 Session。
- 审计能回答“为什么这次没问”。

**缺点**

- 审批 UI 变复杂，需要 primary action + 二级 scope 选择。
- `command_prefix` 容易被绕过（`git status && rm ...`），必须明确是**前缀匹配，不是命令白名单**。
- Session grant 的生命周期、并行结算、审计字段需要新增。
- 需要处理 path 相对/绝对、workspace 内外、symlink。

**工作量**：约 3–5 天 / 3–5 轮（不含 sandbox）。

**适用**：qcode 当前最合适的增量模型。

## 选项 C：Session Permission Preset（DSH 方向）

**决定形态**

- 每个 Session 选择权限档位：`read-only` / `workspace-write` / `full-access`。
- 档位内的操作自动允许；越界操作或提权才 Ask。
- 不提供 per-call grant 菜单，审批只处理“提权/越界”。
- 与 OS sandbox 绑定，sandbox 是真正的执行边界。

**优点**

- 审批次数最少，用户心智最简单。
- 最接近 DSH 的“Session 权限”体验。
- 适合长期运行的 agent。

**缺点**

- 需要可靠的 sandbox 后端；qcode 的 sandbox-exec/bwrap 仍标注 experimental。
- shell 能通过子进程、网络、写越界路径绕过 naive preset；边界必须由 OS 强制。
- 跨平台（macOS/Linux/Windows）成本高。
- 工作量约 1–2 周 / 6–10 轮，且需要单独安全评审。

**适用**：sandbox 成熟之后；不建议现在直接做。

## 对比

| 维度 | A：Turn 工具 | B：Scoped Grant | C：Permission Preset |
|---|---|---|---|
| 减少审批 | 中 | 高 | 最高 |
| `run_shell` 安全性 | 低（整个工具） | 中（前缀/路径） | 高（依赖 sandbox） |
| UI 复杂度 | 低 | 中 | 低 |
| 持久化 | 无 | 运行时 Session | Session 配置 |
| 多前端一致性 | 容易 | 中 | 高 |
| 工作量 | 小 | 中 | 大 |
| DSH 对齐 | 低 | 中 | 高 |

## 推荐

**以 B 为目标，先实现 A 作为第一步。**

具体顺序：

1. **Phase 1（A）**：Turn + tool grant，同 Turn 同工具自动结算；默认仍 Once。
2. **Phase 2（B-lite）**：为 `run_shell` 增加 command_prefix grant；Session grant 只存在运行时。
3. **Phase 3（B-full）**：path_glob、审计字段、TUI/GUI/ACP 一致性、并行请求批量结算。
4. **Preset（C）暂缓**：等 sandbox 后端可用并单独写 ADR；不要在审批 UI 里假装有 sandbox。

**需要确认的产品决策**

1. `Always this tool` 是否继续保留？建议保留但标红，且不作为 shell 的默认选项。
2. Session grant 是否跨 resume？建议不跨；恢复后重新问。
3. Turn grant 是否跨 cancel？建议不跨。
4. `command_prefix` 是否接受 `&&`/`;` 复合命令？建议明确拒绝或只匹配第一段，不做 shell 解析。
5. 审计事件是否记录 matcher/scope？建议记录，否则无法解释自动放行。

**后果**

- 审批从“每次一个决定”变成“决定 + 作用域”，UI 和测试增加。
- `run_shell` 的默认建议不再是 Always；这会改变现有用户习惯。
- 若不做 C，长任务仍可能在不同 Turn 反复询问；需要靠 Turn grant 和低风险策略缓解。

## ADR-0047（草案）：Task / Root 领域模型——任务为核心、目录是可授权根（2026-10-21）

**状态**：Accepted（2026-10-21）。实现中：core Task/Root 模型、旧 Session 单 Root 迁移、
`task_configured` / `roots_changed` 事件、日志重建、GUI Task/Root 编辑、RootResolver、
文件工具权限、无根 Task 的 Turn-private shell、macOS 多根 shell sandbox、Root 编辑的
Turn-boundary pending 已落地；Linux/Windows 多根 sandbox 与完整 Task 浏览器仍待实现。
已确认：Task:Session = 1:1；Root label 仅 slug；
无根 Task 允许在 Turn 私有临时目录运行 shell；Root 变更等 Turn 结束；旧 Session 的 task_id
复用旧 session id；`@root` 别名；Windows 只读 = 不可写；保留 Workspace 作为兼容概念。

**背景**：当前产品以 Workspace 为顶层，Workspace 一定绑定一个目录，Session 挂在
Workspace 下。这个模型有两个硬伤：

1. **路径就是身份**：目录改名/移动后，Session 找不到原来的工作区。
2. **任务可能跨目录**：开发工具目录要读写、代码目录要读写、参考项目/文档只能读；任务也可
   完全不绑定目录，只是问问题。

因此把顶层从 Workspace 改为 **Task**，把目录降级为 Task 的 **Root**（可授权根）。

**术语**

- **Task**：一次用户任务，用户可见的顶层对象。
- **Session**：Task 的对话/事件日志视图；本 ADR 中 Task:Session = **1:1**。
- **Root**：Task 挂载的一个目录，带 `read-only` / `read-write`、稳定 ID 和别名。
- **Workspace**：兼容术语；旧模型的一个 Workspace 迁移为 Task 的一个 primary Root。

## 1. Task 与 Session 1:1

**决定**

```text
Task
  id            // 同时也是 Session id（同一 ULID）
  title
  roots: Vec<Root>
  primary_root  // 可选
  created_at / updated_at
  turn log
```

- 一个 Task 只有一个 Session；没有 `task.sessions` 列表。
- `task_id == session_id`，避免两套 ID 和映射。
- Task 是元数据 + Root 配置 + Turn 日志的聚合；Session 是它的运行时/投影视图。
- 子 agent（subagent / background job）的 Session 是**子会话**，不是用户可见 Task，也不进入
  Task 浏览器。
- 不做 Task 模板、一个 Task 多会话、Task fork、Task 分享；这些都会破坏 1:1 简化。

**理由**

- 1:1 让存储、恢复、浏览器、URL/链接都只需一个 ID。
- “任务”本身就是对话边界；需要另一条独立对话时新建 Task。
- 子会话仍可有自己的日志，但不参与用户级 1:1 约束。

**后果**

- 不能在一个任务里保留多个并行对话；这是有意的简化。
- 如果未来需要 Task 分叉，必须新 ADR 把 1:1 改成 1:N；不要在现有模型上偷偷加。
- Browser 的列表单位从 Session 变成 Task；ADR-0044 的 workspace 平铺只作过渡。

## 2. Root 模型

```text
Root
  id            // 稳定 ID，不是路径
  label         // 任务内唯一，例如 code / docs / reference
  path          // 可变
  access        // read-only | read-write
  primary       // 是否 shell 默认 cwd；最多一个
```

规则：

- 一个 Task 可以有 **0..N 个 Root**。
- `roots = []` 是合法的纯问答 Task：文件工具不可用；`run_shell` 仍可在 **Turn 私有临时目录**
  中运行（每 Turn 创建、Turn 结束清理，不跨 Turn 保留，也不把临时目录算作 Root）；仍可聊天、
  使用其他非文件工具。
- 每个 Root 的 `label` 在 Task 内唯一，且只允许 ASCII slug：`[a-z0-9][a-z0-9-]*`，
  最长 32 字符；中文和空格只出现在 title/display name，不出现在 `@label` 中。
- `primary` 最多一个。
- 没有 primary 但有 Root 时，shell 不可用或要求用户选 primary；v1 建议要求显式 primary。
- primary 可以是只读 Root；允许作为 shell cwd，但 `qc doctor` 要提示写入会失败。
- Root 的路径必须存在且是目录；添加时校验，不自动创建。
- v1 限制最多 **16** 个 Root；不允许 Root 互相嵌套/重叠。

### 2.1 Root 身份与目录改名

- Root 的身份是 `Root.id`，不是 `path`。
- 目录改名/移动后，Task 仍持有同一个 Root ID；用户通过 **relink** 指向新路径。
- 路径缺失时标记 `missing`，不自动猜测新目录，不静默新建 Root。
- 正在运行的 shell 若 cwd 失效，不静默切换路径；当前 Turn 结束后再 relink/重建。
- Session/Turn 日志记录 Root ID + 当时的 path 版本，便于审计和回放。

### 2.2 Root 别名与模型可见路径

- 模型和工具使用 `@label/path` 形式，例如 `@code/src/main.rs`、`@docs/README.md`。
- 兼容绝对路径：绝对路径必须 canonicalize 后落在某个 Root 内，否则拒绝。
- Shell 里使用真实绝对路径；系统提示给出 Root 别名到绝对路径的映射。
- 文件工具输出使用 `@label/path` 显示；避免同一路径在不同 Root 下产生歧义。
- 没有 Root 的 Task 不接受 `@...` 路径；文件工具直接报告“no roots”。

### 2.3 只读语义

- `read-only`：文件工具可读、不可写；shell 以只读方式挂载。
- `read-write`：文件工具可读写；shell 可写。
- 写只读根：直接拒绝，不弹“是否允许”的审批；要写就显式把 Root 改成 RW（用户操作）。
- 越界路径：拒绝；不自动提权。
- 符号链接：先 canonicalize；只读根里的 symlink 不能指向可写根绕过检查。
- 临时目录：系统临时目录与 **Turn 私有 temp** 可写；Turn 私有 temp 不跨 Turn 保留。
- 无根 Task 的 shell：cwd 为 Turn 私有 temp；可以读写该 temp，但看不到用户文件；网络仍按
  Session 网络策略。

### 2.4 Windows 的只读语义

- Windows 当前 ACL 后端只能限制**写**，不能限制读。
- 因此 Windows v1 的 `read-only` 语义是：**不可写，但仍可读**。
- UI/文档必须明确这一点；不能承诺“参考目录不可读”。
- 需要“不可读”时，Windows 暂不支持，或另立 ADR 用 AppContainer/Job Object 等实现。

## 2.5 持久化事件

- 新建 Task 时，在 `session_started` 后追加 `task_configured { roots }`。
- Root 变更时追加 `roots_changed { roots }`，记录完整快照而不是增量。
- `task_configured` / `roots_changed` 的 `turn` 使用当前最后 Turn 号，避免 SessionSummary 把
  turns 归零。
- 回放时以最后一个 `task_configured` / `roots_changed` 为准；旧日志没有这两个事件时，从
  `session_started.cwd` 派生单 Root。
- 事件 schema 升到 v2；旧 v1 日志仍可读。
- Root 事件只记录配置，不改变 Turn 内容；审批与 sandbox 在下一个 Turn 使用新配置。

## 3. 动态 Root 变更

用户确认：Root 可以在运行中增删/改权限，但必须显式审批，并**等当前 Turn 结束后生效**。

规则：

- 添加 Root：用户选择目录、label、access、primary；这是 **Task 配置变更**，不是工具审批。
- 删除 Root、RO ↔ RW、relink：先记录为 pending change，当前 Turn 继续使用开始时的权限快照；
  Turn 结束后统一应用。
- 当前 Turn 中途不停止 shell，也不扩大权限；Turn 结束后停止旧 shell、重建 sandbox。
- 如果外部已经把 cwd 删掉/改名，shell 可能自行失败；仍然不在 Turn 中途静默切换路径，
  Turn 结束后由用户 relink。
- 正在运行的 shell 不因 pending change 被中途 kill；等 Turn 结束。
- Root 变更写入 Task 的审计事件；Session 记录变更前后的快照。
- 模型不能自行添加/修改 Root；v1 只允许用户界面操作。
- Root 变更后 sandbox 必须重建；不能复用旧进程的权限。

## 4. 权限执行分两层

1. **逻辑层**：文件工具、路径解析、Root access 检查。
2. **OS 层**：`run_shell` 的 sandbox（macOS SBPL、Linux bwrap/Landlock、Windows ACL）。

两层必须同时实现：

- 只改 sandbox，`write_file` 会绕过。
- 只改文件工具，shell 子进程会绕过。

具体 sandbox 能力矩阵、SBPL/`--ro-bind`/Landlock 的多根实现放在 **ADR-0048**。

## 5. 与审批模型的关系

- Root access 是**权限配置**，不是 ADR-0046 的 grant。
- 添加/修改 Root 必须由用户显式操作，并可审计；不能用 `Allow once` 绕过。
- 在 RW Root 内是否还要逐条审批，由 ADR-0046 的 Turn/Session grant 决定；本 ADR 不规定。
- RO Root 的写入是拒绝，不是 Ask；否则用户会误以为“批准一次就能写只读目录”。

## 6. Workspace 兼容与迁移

- `Workspace` 保留为兼容术语，但不再是所有权边界。
- 旧 Workspace 会话迁移为 Task：
  - `task_id` 复用旧 session id（保持链接/恢复兼容），不生成第二套 ID；
  - 旧 cwd 成为唯一 Root，label = `workspace`，access = `read-write`，primary = true；
  - Root ID 新生成；旧 Session 记录建立映射。
- 旧路径失效时进入 `missing`，要求 relink；不按 basename 自动匹配。
- UI 可以继续把 primary Root 显示为 “Workspace”，但内部模型是 Task + Root。
- ADR-0044 的平铺浏览器只作为过渡；新的 Task 浏览器另立 ADR。

## 7. 非目标

- 一个 Task 多个 Session、Task 模板、Task fork/分享。
- Root 自动发现/自动重命名匹配。
- 重叠 Root、Root 内嵌套 Root、跨设备同步。
- Windows 的“不可读”只读语义。
- 模型自主修改 Root 权限。
- 完整 sandbox 后端实现（见 ADR-0048）。
- 远程/多用户 Task 所有权。

## 8. 工作量与阶段

- 领域模型 + 存储/migration：约 3–5 天。
- 文件工具多 Root + `@alias` 路径：约 3–5 天。
- Root 变更/审计/快照：约 2–3 天。
- Task 浏览器（替代 ADR-0044）：约 2–4 天。
- macOS/Linux 多根 sandbox：约 3–5 天。
- Windows 写限制与文档：约 1–2 天。

v1（单 Root 兼容 + 无根 Task + 最多 16 Root，macOS/Linux 完整，Windows 只写限制）约
**2–3 周**；完整跨平台/迁移/安全评审更久。

**推荐实现顺序**

1. Task/Turn 日志 1:1 + 单 Root 迁移（不破坏现有 usage）。
2. 无根 Task + `@alias` + 文件工具能力检查。
3. 多 Root 逻辑权限 + RO 拒绝。
4. macOS/Linux 多根 sandbox。
5. Root 动态变更/relink/审计。
6. Task 浏览器；最后再删 ADR-0044 的 workspace 假设。

## ADR-0048（草案）：多根 Sandbox 与路径能力（2026-10-21）

**状态**：Accepted（2026-10-21）。**已实现**（2026-11-08；Windows/Linux/macOS 三个平台
真机实测，见文末补充）。依赖 ADR-0047 的 Task/Root 模型。

**背景**：Task 可以有 0..N 个 Root，Root 有 `read-only` / `read-write`，并且无根 Task 仍允许
在私有临时目录运行 shell。qcode 现有 sandbox 是“单 workspace”假设：

- macOS [sandbox.rs](crates/qcode-tools/src/sandbox.rs:782) 的 SBPL profile 只接收一个
  `workspace`；`build()` 对 `extra_writable` 在 macOS 分支直接忽略。
- Linux `bwrap` 只把 workspace bind 成可写；额外目录只有可写，没有额外只读根。
- Linux Landlock 只给 workspace 加规则。
- Windows ACL 只限制写，读不受限。
- 文件工具 [fs.rs](crates/qcode-tools/src/fs.rs:91) 的 `path::resolve` 只接受 workspace 内路径。

**目标**

1. 让 shell 与文件工具都遵守 Task 的 Root 访问矩阵。
2. 只读根不可写，越界路径不可访问。
3. macOS/Linux 支持多根；Windows 明确只做写限制。
4. 无根 Task 的 shell 只在私有 temp 中运行。
5. Root 变更只在 Turn 边界生效，sandbox 不复用旧权限。

**非目标**

- Windows 的“不可读”只读语义。
- 允许 Root 重叠/嵌套。
- 跨设备/远程 sandbox、网络策略重构、持久 shell。
- 让模型自主修改 Root。

## 1. 权限矩阵

```text
Root(read-write)  → 文件工具读写；shell bind/bind rw
Root(read-only)   → 文件工具只读；shell bind ro
Turn 私有 temp     → shell cwd/读写；文件工具不可直接访问（除非作为 temp API）
无根 Task          → 无文件工具；shell cwd=Turn 私有 temp，只能读写 temp
System/toolchain   → 只读；Cargo/Rustup 等缓存默认可写（显式例外，须审计）
```

- Root 不允许重叠/嵌套；v1 最多 **16** 个。
- 绝对路径必须 canonicalize 后落在某个 Root 内；否则拒绝。
- `@label/path` 先解析 Root，再做 canonicalize + 越界检查。
- 只读根里的 symlink 不能指向可写根；canonicalize 后再判断。
- 越界路径直接拒绝，不弹 `Allow once`；要访问新目录必须显式加 Root。

## 2. 平台实现

### macOS `sandbox-exec`

- 生成一个 profile，而不是每个 workspace 一个 profile。
- `file-read*`：系统目录 + 所有 Root（RO/RW 都可读）+ Turn 私有 temp。
- `file-write*`：仅 RW Root + Turn 私有 temp + 显式 toolchain cache。
- RO Root 绝不能出现在 `file-write*` 的 `subpath` 中。
- 无根 Task：只读系统目录 + Turn 私有 temp；不包含用户 home。
- `extra_writable` 必须真正进入 profile。**已核实（2026-10-22）**：`profile_for_roots` 会把它并入 `(allow file-write*)`，
  `SandboxExec::with_roots` 也把 temp 与它一起传下去；此前「忽略」的判断已过时，现在由 macOS 专属测试
  `the_profile_writes_extra_roots` 守住。
- `sandbox-exec` 是实验/已弃用接口，预检失败必须 fail closed，不能退回无 sandbox。

### Linux `bwrap`

- 系统目录 `--ro-bind`。
- RO Root：`--ro-bind`。
- RW Root：`--bind`。
- Turn 私有 temp：`--bind` 或私有 tmpfs。
- cwd 必须是已 bind 的路径；无根 Task 使用 Turn 私有 temp。
- `--unshare-net` 由网络策略决定，不由 Root 矩阵决定。
- 多根时允许绝对路径；不做 alias 的 shell 重写。

### Linux `Landlock`

- 每个 Root 加规则：RO 用 `LL_EXECUTE|LL_READ_FILE|LL_READ_DIR`，RW 用完整允许位。
- 系统目录只读；temp/toolchain 可写。
- Landlock 不限制网络，网络边界仍需 bwrap 或审批。
- 多根 rule 必须在 `restrict_self` 前全部安装。

### Windows ACL

- 只限制**写**：RW Root + Turn 私有 temp + 显式 cache 可写；其他位置不可写。
- 读不受限，所以 RO Root 的语义是“不可写，但仍可读”。
- 无根 Task：只给 Turn 私有 temp 写权限；读仍不受限。
- UI/文档必须明确这个限制；不能承诺“参考目录不可读”。

## 3. 无根 Task 的 shell

- 为每个 **Turn** 创建私有 temp 目录（例如 `data/tasks/<id>/tmp/<turn>`），Turn 结束清理；
  不跨 Turn 保留。
- shell cwd 指向该 temp；可以读写 temp，不能看到用户文件。
- macOS/Linux：sandbox 只读系统目录 + temp；不 bind home。
- Windows：ACL 只允许 temp 写；读仍可能看到用户可读文件，这是 ACL 的限制。
- 文件工具在无根 Task 中不可用；模型得到明确“no roots”错误。
- 纯问答 Task 不需要 Root；这是合法状态，不是错误。

## 4. 路径能力层

- 文件工具统一经过 `RootResolver`：
  1. 解析 `@label/path` 或绝对路径；
  2. 找 Root（v1 不允许重叠，因此唯一）；
  3. canonicalize；
  4. 检查目标在 Root 内；
  5. 检查 access。
- 不允许通过 `..`、symlink、大小写差异绕过。
- 只读根写入在逻辑层先拒绝，再由 sandbox 兜底。
- 工具输出路径统一显示 `@label/path`；绝对路径用于 shell 和调试。

## 5. Root 变更与 sandbox 生命周期

- 每个 Turn 开始时冻结 Root 权限快照。
- Root 增删/RO↔RW/relink 进入 `pending_changes`；当前 Turn 不生效。
- Turn 结束后：
  1. 停止该 Turn 的 shell/子进程；
  2. 应用 pending changes；
  3. 重建 sandbox；
  4. 下一个 Turn 使用新快照。
- Root 变更必须写审计事件；Session 记录前后 Root 快照。
- 如果外部路径在 Turn 中失效，不静默 relink；Turn 结束后由用户处理。
- qcode 当前 `run_shell` 是逐命令进程；未来 persistent shell 必须同样在 Turn 边界重启。

## 6. 与审批的关系

- Root access 是权限配置，不是 ADR-0046 的 grant。
- 写入 RO Root：Deny，不是 Ask。
- 越界路径：Deny；要访问就加 Root。
- RW Root 内的具体命令是否 Ask，由 ADR-0046 的 Turn/Session grant 决定。
- 添加/修改 Root 是用户显式操作；模型不能通过 `Allow once` 提权。

## 7. 失败与可观测性

- sandbox 预检失败：拒绝启动该 Turn 的 shell，明确报错，不退回无 sandbox。
- 如果平台无法表达 Root 矩阵（例如 Windows 读限制），启动时明确说明能力降级。
- `qc doctor` 显示：backend、Root 数量、RO/RW、临时目录、toolchain 例外、网络策略。
- 审计事件至少记录：task_id、root_id、path、access、sandbox backend、turn。

## 8. 推荐实现顺序

1. RootResolver + 文件工具多根（先不碰 sandbox）。
2. 无根 Task + 私有 temp。
3. macOS SBPL 多根 RO/RW。
4. Linux bwrap 多根 RO/RW。
5. Linux Landlock 多根。
6. Windows ACL 写限制 + 能力提示。
7. Root pending changes / 审计 / qc doctor。

**工作量**：文件工具与路径 3–5 天；macOS 2–3 天；bwrap 2–3 天；Landlock 2 天；Windows 1–2 天；
测试与安全评审 3–5 天。v1 约 **2–3 周**。

## 9. 已确认的细节

1. Toolchain cache（Cargo/Rustup）**默认允许写**；它们不是 Root，必须在 `qc doctor` 与审计中
   列为显式例外。
2. Root 上限为 **16**。
3. 无根 Task 的 temp **不跨 Turn 保留**；每 Turn 创建，Turn 结束清理。
4. 只读 primary Root **允许**作为 shell cwd；写入会失败，`qc doctor` 给出警告。
5. Windows 的“只读 = 不可写，但仍可读”作为**正式产品语义**；UI 必须明确说明读不受限。
## ADR-0049：Task 是唯一的权限来源——cwd、primary_root 与 roots 的分工（2026-10-22）

**背景**：业务模型早已是「Task + 0..N Root」，没有 workspace 实体。但代码里 `workspace` 一词
同时承担了三种含义：GUI 的侧栏过滤（业务泄漏）、`RunOptions`/`assemble`/`sandbox` 的参数
（内部管道）、老 session 迁移出的 root label（兼容）。混淆的直接后果是 `run_shell` 的起始目录
在两处互相矛盾，以及 `ToolContext` 带着一个「启动目录」字段，让人误以为它是权限来源。

**决定**

1. **命名三分**，不得再用 `workspace` 指代其中任何一个：
   - `cwd`：进程/会话的启动目录。只用于记录（`SessionStarted.cwd`、状态栏）、加载配置与
     skills、以及 `RunOptions` 的载体；**不是**授权边界。
   - `primary_root()`：Task 里标记 `primary` 的 Root（没有则第一个），是「相对路径默认落在
     哪个 Root」的唯一答案。
   - `roots`：Task 的授权集合，唯一决定工具能读/写什么。
2. **`ToolContext` 只由 roots 与 turn temp 构成**：`ToolContext::for_roots(task.roots)`，
   逐个 `.with_temp_dir` / `.with_cancel` / `.with_network`。目录构造器降级为
   `ToolContext::for_test(dir)`，仅服务单测与 legacy 单根路径。
3. **`run_shell` 的起始目录**解析顺序固定为：调用里的显式 `cwd` → `primary_root()` →
   任意 root → turn 私有 temp → **报错**。绝不回落到 `cwd`：那等于给命令一个未授权的目录。
   审批提示里的 `cwd:` 用同一个函数计算，避免「提示与执行不一致」。
4. **GUI 不按目录过滤 Task**。侧栏列出全部 Task；`cwd` 只作为元信息显示。
5. **迁移**：老 session 的 `cwd` 变成唯一 Root，label 为 `LEGACY_ROOT_LABEL`（"workspace"）。
   该常量是唯一的遗留入口，删除它即可清掉历史包袱。
6. **无根 Task**（roots 为空）：只读系统目录 + turn 私有 temp；文件工具不可用，
   `run_shell` 在 temp 中执行。

**理由**

- 权限判定只看 roots，代码里任何「顺便用启动目录兜底」的写法都是提权；
- `cwd` 与 Root 拆开之后，`run_shell` 的 cwd 与审批提示可以共用一个函数，不会再各算一套；
- 迁移标签收敛到一个常量，未来清理有明确入口。

**后果 / 边界**

- `RunOptions.cwd` 仍存在（会话元信息与配置加载需要），但不再参与权限；
- `qc sessions list` 的 `cwd=` 是元信息，不代表归属；
- Windows 的多根 sandbox 仍按 ADR-0048 的「只做写限制」推进（另案）。

**验证**：`shell_cwd` 的两条断言（无根 → temp；有根 → primary root，即使有 temp）；
`ToolContext` 不再有 `primary_root` 字段（编译即证明）；GUI 不再有 `workspace_sessions` /
`workspace_tasks`；全量测试数量：core 67、tools 61、kernel 47、acp 9、gui 2、qc 26。
**补充（2026-10-22，Windows 多根已实现并实测）**：`WindowsAcl::with_roots` 按「只限制写」的语义
实现——`read-write` Root 进入可写集合，`read-only` Root 不进入（读本来不受限），没有可写 Root 时
只用 turn temp。`crates/qcode/tests/sandbox_roots.rs` 在普通桌面 PowerShell 上实测通过：向
RW Root 写文件成功、向 RO Root 写文件被 token 拒绝且未落盘。

注意 helper 的约束：ACL 后端通过 `qc sandbox-run` 执行命令，`current_exe` 必须是 `qc`；
`WindowsAcl::with_helper` 供测试指向真实二进制，测试因此放在 `qcode` 包内（`CARGO_BIN_EXE_qc`）。
**补充（2026-10-22，Linux 两个后端的多根也实现了）**：bwrap 把每个 Root 变成一次 bind
（`read-write` → `--bind`，`read-only` → `--ro-bind`），turn temp 作为额外的可写 bind；
`/tmp` 的 tmpfs 决策改为看「是否有任意 Root 在 `/tmp` 下」。Landlock 保留 `(path, writable)`
列表，按模式给规则（只读只拿 `LL_EXECUTE|LL_READ_FILE|LL_READ_DIR`），并提供 `with_roots`
按 Task 重建 ruleset。

WSL 实测：`cargo test -p qcode-tools sandbox::` 10 条全过，含 `a_read_only_root_is_mounted_read_only`
（bwrap 参数矩阵）、`linux_tests::{allows,denies}_writes_*`（真 bwrap 运行，断言的是「宿主文件未出现」
而不是「命令必须失败」——因为 bwrap 会在自己的 tmpfs 里创建 bind 的父目录，写入可能"成功"但不落宿主）。
Landlock 在 ABI 不足时会 skip，但规则构建路径已覆盖。

据此，ADR-0048 的多根矩阵除 macOS 真机复核外全部完成：Windows ACL（实测）、Linux bwrap（实测）、
Linux Landlock（规则构建通过）、macOS sandbox-exec（profile 测试就绪，待 Mac）。

**补充（2026-11-08，macOS 真机复核完成，并修掉 `$TMPDIR` 下的 RO 漏洞）**：
macOS arm64 上真机跑通 `cargo test -p qcode-tools --lib sandbox::`（18 条）与
`crates/qcode-tools/tests/tools.rs::shell_follows_the_root_matrix`，`sandbox-exec` 预检通过、
没有一条走 skip 分支。复核发现两处需要在 profile 里修的东西：

1. **只读根只是「不在白名单里」并不等于只读。** profile 对所有任务都放开了
   `/tmp`、`/private/tmp`、`/private/var/folders`、`$TMPDIR` 与 Cargo/Rustup 缓存，
   所以一个落在这些父目录下的 RO Root 仍然可写。Seatbelt 是「最后一条匹配的规则生效」，
   于是在 `(allow file-write*)` 之后为每个 RO Root 追加
   `(deny file-write* (subpath "..."))`，实测该规则确实覆盖父目录的放行
   （`a_read_only_root_refuses_writes_inside_a_writable_temp_tree` 用 `$TMPDIR` 里的
   RO 根断言「宿主文件没出现」；把 deny 行去掉这条测试立刻失败）。
2. **SBPL 比的是真实路径**：`$TMPDIR` 是 `/var/folders/...`，而 filter 要写
   `/private/var/folders/...` 才匹配。profile 里所有 root / temp / `extra_writable` /
   环境变量目录统一过 `canonicalize`（`profile_path`），不会因为一个 symlink 前缀而
   整段失效——此前 `/tmp` 与 `/private/tmp` 都手写进列表，属于同一个坑的补丁式绕过。

据此 ADR-0048 的 §2 平台矩阵除 Landlock 只到规则构建外全部有真机实测：Windows ACL、
Linux bwrap、macOS sandbox-exec（本机）。
## ADR-0050：配置文件的位置、便携模式与写入策略（2026-10-22）

**背景**：设置界面保存失败，报错指向 `~/.config/qcode/.config.toml.tmp-<pid>` 的
`拒绝访问`。排查后发现该目录**允许改写已有文件、但不允许新建文件**（Defender 服务未运行，
应是第三方 AV/EDR 或组策略拦未签名进程创建文件）。于是有人提议「优先把 `config.toml` 放在
可执行文件旁边，找不到再去用户目录」。逐平台评估后结论是：**不能作为默认行为**。

**决定**

1. **默认位置仍是用户配置目录**：`--config` → `QCODE_CONFIG` → 便携目录 → 用户目录。
   用户目录按 `XDG_CONFIG_HOME`（若设置）否则 `~/.config`，三平台统一用 `.config`，
   换取「一份配置三平台通用」的好处，代价是不完全符合 macOS/Windows 惯例。
2. **便携模式必须显式开启**：可执行文件旁存在 `qcode.portable` 标记文件时才启用，配置放在
   `<exe 目录>/data/config.toml`（子目录而非直接放旁边），这样升级安装目录不会顺手删掉配置。
3. **系统安装目录永不便携**：macOS 的 `*.app/Contents/...`（写入会破坏签名）、Linux 的
   `/usr`、`/opt`、`/app`、`/nix`、`/snap`（root 所有或只读镜像）即使有标记文件也跳过。
4. **写入策略**：优先原子写（临时文件 + rename）；若创建临时文件被拒，降级为「尽力备份
   `.bak` + 原地覆盖」；若连 `.bak` 也建不了，只提示不阻断——保存配置优先于留备份。
5. **诊断可见**：启动打印 `config: <path> (source: flag|env|portable|user)`，`doctor` 同样显示，
   避免「改了配置却不生效、又不知道读的是哪个文件」。

**理由**

- exe 旁可写只在 `target/debug`、便携解压这类场景成立；正式安装后普通用户无写权限，
  该策略只会把失败点换个位置；
- 便携目录共享给同一台机器的所有用户，API key 会交叉可见，且升级/清理容易误删；
- 自动读取「exe 旁的残留配置」会让用户目录里的修改静默失效，排查成本极高；
- 真正与平台无关的解法是第 4 条：写入降级与原子性解耦。

**后果 / 边界**

- 若某台机器连「改写已有文件」都禁止，仍会保存失败，此时只能用 `--config` / `QCODE_CONFIG`
  指向可写目录；错误信息会指明是哪个目录、哪一步失败；
- 便携模式下 `.bak` 与临时文件都落在 `<exe>/data/`，卸载/升级该目录需保留；
- 未做配置迁移：用户从便携切回用户目录（或反之）需要自己挪文件，ADR 暂不自动化。

**验证**：`config::discover` 的来源判定、`is_system_install` 的平台分支（Linux 用例单独 cfg）、
启动与 doctor 的来源行；`qcode-kernel` 50 条测试通过。
## ADR-0051：中文 IME 让 GUI 崩溃——fork `xim-ctext` 解 COMPOUND_TEXT（2026-10-24）

**背景**：Linux 上用 `qc gui`（X11 + IBus XIM）敲中文，主线程直接 panic：

```
thread 'main' panicked at zed-xim-0.4.0-zed/src/client.rs:237:67:
Encoding Error: UnsupportedEncoding
```

链路：`gpui-pre-linux` 用 `zed-xim` 做 XIM 客户端；`zed-xim` 对每条 preedit / commit
都写 `xim_ctext::compound_text_to_utf8(&data).expect("Encoding Error")`；而 `xim-ctext`
0.3.0 只认 UTF-8（`ESC % G`）和 ISO-2022-JP，其余字符集一律
`Err(UnsupportedEncoding)`。于是**只要输入法送来非 UTF-8 的 COMPOUND_TEXT，进程就死**。

这不是罕见输入。`LANG=zh_CN.UTF-8` 下 Xlib 的 UTF-8→COMPOUND_TEXT
（IBus/fcitx 的 XIM 服务端正是这么发的）把中文写成 `ESC $ ( A` + 7 位 GB 2312：

| 输入 | Xlib 输出（`Xutf8TextListToTextProperty`, `XCompoundTextStyle`） |
| --- | --- |
| `你好` | `1B 24 28 41 44 63 3A 43` |
| `你好, 世界!` | `1B 24 28 41 44 63 3A 43 1B 28 42 2C 20 1B 24 28 41 4A 40 3D 67 1B 28 42 21` |
| `你好😀` | `1B 24 28 41 44 63 3A 43 1B 25 47 F0 9F 98 80 1B 25 40` |
| `café` | `63 61 66 E9`（无转义，裸 ISO-8859-1） |

也就是说：**拼音候选一出现（preedit）就崩**，与是否提交无关。

**决定**

1. **vendor 一个 `xim-ctext`**（`vendor/xim-ctext`，MIT，上游 `Riey/xim-rs`），
   用 `[patch.crates-io]` 替换 crates.io 的 0.3.0。改的是解码器，不是调用方：
   上游那个 `.expect()` 在依赖里，我们改不到，也不该长期绑死 `gpui-pre-linux` 的补丁。
2. **解码改成「尽可能解、绝不失败」**：`compound_text_to_utf8` 保留 `Result` 签名（与
   `zed-xim` 的 API 一致），但永不返回 `Err`；不认识的字符集用有损解码（U+FFFD / windows-1252）
   降级，宁可能显示乱码也不能弄死进程。已支持的字符集（按 designator）：
   UTF-8（含只有开头没有 `ESC % @` 的情况）、GB 2312（按 GBK 解）、JIS X 0208（EUC-JP）、
   KS C 5601（EUC-KR）、ISO-8859-1…6，以及 `ESC - X` / `ESC . X` 的 96 字符集形式。
3. **不走「丢弃」路线**：把解不出来的 preedit 直接丢掉不会崩，但等于中文输入法只能打英文，
   比崩溃更糟。
4. **回归测试落在 fork 里**（它是 workspace member，`cargo test --workspace` 会跑）：
   用例直接固化上表那几串 Xlib 真实输出；另有 `never_fails` 穷举 1–4 字节含 `0x1B`
   的组合，保证任何 preedit 字节都不会 panic 或 `Err`。同时修掉上游两个隐患：
   `ESC % G` 少 `ESC % @` 时 `left.len() - 3` 会 panic / 吃掉末尾三字节，孤立 `ESC` 同理。
5. **CI 加一条守着 patch 的检查**（Linux job）：`cargo tree -p qcode-gui -i xim-ctext`
   必须指向 `vendor/xim-ctext`。若哪天 GPUI 升到 `zed-xim` 需要 `xim-ctext` 的新版本，
   patch 会静默失效、崩溃回归，这条检查会先把 PR 拦下来。

**后果 / 边界**

- fork 只覆盖解码；编码方向（`utf8_to_compound_text`）仍是上游的 UTF-8 转义，未改。
- 字符集是按 designator 与本机 Xlib 实测结果接的，不是完整的 X11 COMPOUND_TEXT 表：
  ISO 10646（`ESC % / I`）、JIS X 0212 等仍未实现，落到有损分支（能显示的部分保留）。
- cluster 之外的 preedit 样式（下划线、选中区）`zed-xim` 本来就丢，未在此 ADR 处理。
- Windows/macOS 不经过 `zed-xim`，patch 对它们无影响（`gpui-pre-linux` 是 Linux-only 依赖）。

**验证**：`cargo build -p qcode --features gui` 通过（补丁生效后 `zed-xim` 重编）；
`cargo test -p xim-ctext` 12 条全过（含上表 GB2312/日文/韩文/latin1/emoji 混合与穷举）；
`cargo clippy -p xim-ctext -p qcode-gui -p qcode --features gui -- -D warnings` 干净。

真机复现与回归（Ubuntu + GNOME X11 `:1`，`XMODIFIERS=@im=ibus`，`ibus engine libpinyin`，
xdotool 同级用 XTest 真敲 `nihao` + 空格选词 + `shijie`）：

- **未打补丁**（`[patch.crates-io]` 暂时 stash 掉）：敲完第一组拼音，进程当场 panic，
  就是最初报的那行 `zed-xim-0.4.0-zed/src/client.rs:237:67: Encoding Error: UnsupportedEncoding`。
  说明诊断的前半段（preedit 即崩、不是提交才崩）成立。
- **打了补丁**：同一串按键，进程存活；截屏 diff 显示输入框文字区变化；再按 `Cmd+Enter`
  发出去，会话日志里 `turn_started` 的 `input` 是 `"你好世界"`，`user_message` 同值。
  即中文 preedit → 提交 → 进 prompt 全链路通，不是「不崩但乱码」。

## ADR-0052：系统托盘——Linux 用 `ksni`，关闭最小化而不是退出（2026-10-28）

**背景**

GUI 目前是"单窗口、关窗即退"：`qcode-gui` 的 `run()` 用 `gpui_kit::open_window` 开唯一
窗口，窗口一关 GPUI 在 Linux 上按 `QuitMode::Default` 直接退出。`cmd-.` 停任务、审批
fail-closed 这些能力都在，但**一旦窗口没了，后台正在跑的任务就失去了可达的控制面**；
用户误点关闭只能重开、重连会话。

桌面上的同类工具普遍用托盘常驻解决这个问题（本机 v2rayN 的托盘菜单即为此形态，
用 DBus 实读到：显示主窗口 / 显示或隐藏 / 复制代理命令 / 系统代理三选一 / 订阅更新 /
退出）。qcode 需要的不是代理设置，而是**状态可见 + 任务可控 + 一个不误杀的退出入口**。

GPUI（`gpui-pre 0.3.9`）的能力边界决定了实现方式：

| 能力 | 现状 |
| --- | --- |
| 拦截关闭 | `Window::on_window_should_close(cx, f)`，返回 `false` 不关 |
| 最小化 | `Window::minimize_window()` |
| 窗口级隐藏/显示 | **没有**（只有 app 级 `App::hide()`，macOS 语义） |
| 零窗口存活 | `QuitMode::Explicit` 可让应用在无窗口时继续跑 |
| 系统通知 | `Platform::show_system_notification` |

**决定**

1. **P1 只做 Linux，用 `ksni`**（纯 Rust，StatusNotifierItem + dbusmenu，zbus 实现）。
   `tray-icon` 的 Linux 后端依赖 GTK + libayatana-appindicator，并且需要 GTK 主循环，
   与 GPUI 自持的事件循环冲突；`ksni` 只走 DBus，没有 C 依赖，X11 / Wayland 都能用。
   非 Linux 平台留 `TrayBackend` trait 的空实现，不改动现状。
2. **关闭按钮 = 最小化到托盘**：`on_window_should_close` 返回 `false` 并
   `minimize_window()`；窗口不销毁，UI 状态（滚动、对话框、输入框草稿）全部保留。
   **P1 不做"真隐藏"**（不占任务栏）：GPUI 没有窗口级 hide/show，做真隐藏要么销毁重建
   （需把 `app`/`backend` 从 `Shell` 里抽出来共享，且窗口级 UI 状态会重置），要么给
   gpui-kit 补 hide/show（Wayland 上也只能退化成最小化）。等 P2 有明确诉求再升级。
3. **菜单只放与任务相关的东西**，照 v2rayN 的分组语义：
   状态行（disabled）/ 显示主窗口 / 新建任务 / 停止当前任务（仅运行中可用）/
   分隔线 / 退出。不做代理、订阅、开机自启这类与本产品无关的项。
4. **状态来自 `qcode-ui-state`**：运行中 turn 数、pending approval 数、最近一次失败。
   图标三态（空闲 / 运行中 / 待审批），tooltip 显示活动任务标题；状态变化节流 300ms
   再 `Handle::update`，避免高频重绘。
5. **线程与命令通路**：ksni 跑在自己的线程（tokio）；菜单点击发 `TrayCommand`
   （`ShowWindow` / `NewTask` / `StopCurrent` / `Quit`）经 `mpsc` 交给 GPUI 的
   foreground task，复用现有 action（`cmd-n`、`cmd-.`）。
6. **退出只有一个真正的入口**：托盘"退出"（以及菜单里的退出 / `cmd-q`）才 `cx.quit()`；
   有任务在跑时先确认。审批仍然 fail-closed，托盘只提示"等待审批（N）"，不代答。
7. **托盘注册失败必须降级**：没有 `org.kde.StatusNotifierWatcher` 时（headless、精简
   Wayland 合成器）只记日志，恢复"关窗即退"的原行为，不能因为托盘挂了让主程序不可用。

**P2：Windows / macOS 的选型（实施前复核）**

P1 只落地 Linux，但两端的能力差异会影响接口设计，先把选型记下来：

- **Windows：`tray-icon` + `muda`**（`Shell_NotifyIconW`，`windows-sys` 后端，无 C 依赖）。
  托盘必须在有消息泵的线程上创建：专用线程创建 `TrayIcon`，用 `GetMessageW` 循环收
  回调；`muda` 的菜单事件从全局通道转成 `TrayCommand`。图标用 `.ico`（16/32/48 多尺寸，
  按 DPI 选）。
- **macOS：同样用 `tray-icon`**（`NSStatusItem`，objc2 后端）。**必须在主线程创建**，
  时机在 `NSApplication` 初始化之后、GPUI 进入 run loop 之前；菜单走 `muda` 的 NSMenu
  后端，回调在主线程触发，直接投递 `TrayCommand`。图标用 template image（纯黑 + 透明，
  `icon_as_template(true)`），由系统适配明暗主题与选中态。
- **不把 `tray-icon` 复用到 Linux**：它的 Linux 后端依赖 GTK + libayatana-appindicator，
  要跑 GTK 主循环，和 GPUI 的事件循环冲突，还会给 deb/rpm/AppImage 增加运行时依赖
  （见下面的备选）。
- **接口不变**：两端都实现同一个 `TrayBackend`（`update(snapshot)` + 命令通道），
  `TraySnapshot` / `TrayCommand` 与平台无关；GPUI 侧的关闭拦截与命令消费逻辑不变。
  macOS 具备 app 级 `App::hide()`，P2 可以把它作为“真隐藏”的实现，P1 先与 Linux
  保持一致的“最小化”。
- **风险**：两个后端的菜单回调线程不同（Windows 消息泵线程 / macOS 主线程），回调里
  只能发通道消息，不能直接碰 GPUI 状态；`muda` 的菜单事件是进程级全局通道，单窗口
  够用，多窗口要按 window id 过滤。

**备选方案（未采用）**

- **`tray-icon` 一套跨平台代码**：Linux 要拖 GTK 主循环，和 GPUI 抢线程模型，且给
  deb/rpm/AppImage 增加 `libayatana-appindicator3-1` 运行时依赖；收益是省一套实现，
  但风险与打包成本都更高。
- **销毁窗口 + 托盘常驻**（`QuitMode::Explicit` + 重建）：语义最接近"托盘应用"，但要
  重构 `Shell` 对 `app`/`backend` 的所有权，且重建后窗口级状态丢失，P1 不划算。
- **不做托盘，只用系统通知**：通知能提示"任务完成/等待审批"，但无法承载"显示窗口 /
  停止任务 / 退出"这些需要持久入口的操作。

**后果 / 边界**

- 最小化不是隐藏：窗口仍出现在任务栏/Dock。若用户明确要求"托盘常驻、任务栏不可见"，
  需要 P2 的销毁重建或 gpui-kit 补丁。
- GNOME 需要 AppIndicator 扩展才有托盘区（Ubuntu 默认自带；纯净 GNOME 需用户安装），
  文档要写清楚；KDE 原生支持 SNI。
- Wayland 下 `activate_window()` 可能被合成器拒绝聚焦（只高亮），这是平台限制，不做承诺。
- 只覆盖 Linux；Windows（`tray-icon`/`muda`，消息泵线程）与 macOS（`tray-icon`，主线程
  `NSStatusItem`）在 P2 通过 `TrayBackend` 扩展，主流程不改，详见上面的 P2 选型小节。
- 与 ADR-0049 的权限模型无关：托盘不引入新的文件/网络能力，也不改变审批语义。

**验证**

- 单测：`TrayState → 图标/状态行/菜单 enabled` 的纯函数；`TrayCommand` 到 action 的映射
  （fake backend），不依赖真实托盘。
- 集成：用 `busctl` 对 dbusmenu 的 `Event` 方法点菜单项（本 ADR 调研 v2rayN 时验证过这条
  路径可行），断言窗口最小化/恢复、停止任务、退出确认。
- 手动：关窗最小化、托盘恢复、运行中退出确认、`StatusNotifierWatcher` 缺失时降级、
  GNOME 与 KDE 各一次、X11 与 Wayland 各一次。
- 回归：`cargo test --workspace`、`cargo clippy --workspace -- -D warnings`，以及现有
  GUI 冒烟路径（新建会话、发消息、停止 turn、审批）。

**P2 实现更新（2026-10-28）**

- macOS（`NSStatusItem`）与 Windows（`Shell_NotifyIcon`）后端按本 ADR 的菜单模型补齐，
  三端共用 `TraySnapshot` / `TrayCommand`：状态行 / 显示主窗口 / 新建任务 /
  停止当前任务（仅运行中可用）/ 分隔线 / 退出。命令仍经 `mpsc` 交给 GPUI。
- 关窗语义保持“最小化而不是退出”；`ShowWindow` 增加了平台原生还原
  （macOS `deminiaturize:`、Windows `SW_RESTORE`），因为 GPUI 的
  `activate_window()` 不会还原最小化窗口。
- 图标改为三端统一的 QC 标记（macOS 模板 PDF、Windows 托盘 ICO、Linux
  `IconPixmap`，见 `crates/qcode/res/icons/`）。三态不再靠图标区分，改由菜单状态行
  与 tooltip 表达；若之后要恢复“图标三态”，需要为标记再做运行中/待审批两个变体。

## ADR-0053：会话投递与 turn 边界——Queue / Steer 的 P1 落地（2026-10-30）

**背景**

ADR-0042 把运行中的投递定为「Queue 默认、Steer 放 P1」，并要求 Steer「在下一个 step 边界注入」
「未注入前显示 pending、可取消」；ADR-0041 把 Turn 定为 GUI transcript 的顶层单位。本次按 0042
的 P1 落地了 engine 侧的 inbox（`next-turn` / `next-step` 两个队列）和 GUI 的队列 dock
（31ca808），随后暴露了两处 0041/0042 没有定义的行为：

1. 被注入的排队消息应该出现在 transcript 的什么位置？engine 只追加 `UserMessage`，没有新的
   `TurnStarted`，于是投影层只能靠「当前 turn 已有输入时又收到用户消息」来拆分（73ea83c）。
2. turn 结束时，还有没来得及注入的 steering 怎么办？如果不处理，它会留在 `next-step`，被
   **下一次 run 的第一个 step** 静默消费——出现在错误的轮次里，而 UI 的队列 dock 只渲染
   `next-turn`，用户期间完全看不到它。

**决定**

### 1. 两个队列由 core 拥有

`qcode_core::inbox::Inbox` 持有两个 FIFO：

- `next-turn`：普通追加，每条消息自己开一个新 turn；
- `next-step`：steering，注入当前正在跑的 turn。

UI 不复制队列语义：bridge 只发 `QueueChanged`（镜像 `next-turn` 用于渲染）和接收
`QueueOp`（Steer / MoveUp / MoveDown / Remove），由运行时对 Inbox 执行。

### 2. Queue 语义（重申 ADR-0042 §7）

- 运行中提交的消息进入 `next-turn`，不立即写 transcript；engine 接收后才产生权威的
  `user/message`；
- Stop / 取消后队列保留，可以由用户继续调整或清空；
- 同一时间只有一个 in-flight turn，turn 结束后按 FIFO 取下一条。

### 3. Steer 的注入点与 turn 投影

- engine 在**每次模型请求之前**（step 边界）claim 整个 `next-step`，把消息持久化为
  `user/message` 并追加到本次请求的 messages 末尾；工具执行中途不注入。
- 因为注入发生在同一个 turn 编号内、且没有新的 `TurnStarted`，投影层在「当前 turn 已有
  输入时收到用户消息」处**拆分出一个新的 Turn**（`id` = 事件 seq，`number` 复用父 turn）；
  该 run 的 `TurnEnded` 收口这次 run 打开的所有 turn。

### 4. leftover：steering 不跨 turn 边界

turn 结束时（完成 / 取消 / 失败，统一在 `finish()` 收口），`next-step` 里剩下的消息按提交
顺序**提升到 `next-turn` 队尾**，成为独立的一轮。这样：

- 不会出现「为 A 加的指令出现在 B 的请求里」的错位；
- 不会因为用户之后不再发消息而永久滞留；
- 提升后的消息有 `TurnStarted`、有自己的问题气泡，和「普通追加」走同一条运行时路径。

顺序选择队尾（而不是队首）：它本来就是在已有排队消息之后提交的，保持提交顺序；「更紧急」
由用户在队列里手动上移表达。

### 5. 结构约束（P0，无行为变化）

- `qcode-gui/src/runtime.rs`：engine 线程、session/turn 编排、队列策略、`UiCommand` 分发。
  **不依赖 GPUI**（只依赖 core / ui-bridge / tokio），因此可以在没有窗口的情况下测试，
  将来 TUI/其它前端也可以复用同一层。
- `qcode-gui/src/render.rs`：transcript / turn / block / markdown 以及 `Shell` 的渲染方法，
  只读状态（除视图自身的通知外不产生副作用）。
- `lib.rs` 只保留窗口状态与动作；`lib.rs` 从 5,078 行降到 2,502 行。

**备选方案（未采用）**

- **leftover 保持原样**（留在 `next-step`，被下一次 run 吸收）：实现最省，但消息错位、
  UI 不可见、可能永久滞留。
- **leftover 丢弃**：简单但会吞掉用户输入，不可接受。
- **提升到队首**：会把 steering 插到用户已排队的消息之前，打乱提交顺序。
- **立即在事件流里加显式 segment 标记**：见「后果」，需要一个独立的 event schema 决策。

**后果 / 已知差距**

- ADR-0042 §7 要求「未注入前显示 pending 状态、可取消」，**当时尚未实现**（见文末 P1 更新）：点「推送」后条目
  立刻从 dock 消失，直到被注入或提升才重新出现。补法是 P1 的可视化，不改变本 ADR 的队列
  语义。
- ADR-0042 §1 的 Composer 四态（ComposerController）与 §6 的 `busy_delivery` / `send_mode`
  设置也还没有落地；当前 GUI 是 `turn_in_flight` + queue dock 的简化版。
- 投影层的拆分仍是**启发式**：事件流里没有 run id / segment 标记，split turn 的 `number`
  与父 turn 相同、`id` 用 seq。显式 segment（`queued_id` / `turn_segment`）属于 event
  schema v4，需要单独 ADR；在那之前启发式是临时方案，不应被当作最终设计。
- dock 只渲染 `next-turn`，`next-step` 不可见（同第一条）。
- 取消时 leftover steering 同样会被提升并启动新的一轮：这是「不丢消息」的取舍，用户按
  Stop 之后仍可能看到自己刚推送的消息开始执行。

**验证**

- `qcode-core`：inbox 单测（7 个）覆盖 followup / steer / 移动 / 删除 / steer_queued /
  pending 判定 / leftover 提升的顺序与幂等。
- `qcode-providers/tests/engine_run.rs`：steering 在请求前注入（`steering_lands_in_the_running_turn_request`）；
  在最后一次请求中才到达的 steering 不进本次请求、而是进入下一轮队列
  （`steering_after_the_last_boundary_becomes_a_followup_turn`）。
- `qcode-ui-state`：steered follow-up 拆成独立 turn 的投影用例。
- `qcode-gui`：队列 dock 的 steer / 上移 / 下移 / 删除路径（集成测试 + 手动）。
- 手动：运行中 Queue → 当前轮结束后按 FIFO 执行；运行中 Steer → 下一 step 生效并拆成独立
  turn；在最后一轮请求期间 Steer → turn 结束后作为新的一轮出现；Stop 后队列保留。

**P1 更新（2026-10-30，同日）**

- 补齐「未注入前显示 pending、可取消」：bridge 新增 `UiEvent::SteeringChanged`（`next-step` 的
  镜像）与 `QueueOp::Unsteer`；dock 把等待注入的条目显示为「等待下一步」并带「撤回」按钮。
- 发布收敛：runtime 用 `publish()` 在快照变化时才发 `QueueChanged` / `SteeringChanged`，并每
  250ms 对比一次快照——engine 在另一个线程 claim steering，注入后条目会自动从 dock 消失。
- 撤回的条目回到 `next-turn` 队尾（与 leftover 提升一致，保持 FIFO 语义）。

## ADR-0054：事件 schema v4——显式 turn segment 与 steering 消息标识（2026-10-30）

**背景**

ADR-0053 定了 Queue / Steer 的语义：一次 `run_turn` 可以把 steering 注入运行中的 turn，
GUI 因此在同一次 run 里投影出多个 turn。但事件流里没有任何显式边界——引擎只追加
`UserMessage`，所以投影层只能靠「当前 turn 已有输入时又收到用户消息」来拆分（73ea83c）。
这带来两个后果：

1. 回放 v3 日志依赖同一个启发式；任何新的「turn 中途插入用户消息」来源都会放大它的脆弱性；
2. UI 无法把一条权威 `UserMessage` 与队列里的 pending steering 行关联，dock 只能靠
   250ms 轮询发现 engine 已经 claim。

**决定**

### 1. `UserMessage` 增加两个可选字段

```text
UserMessage {
  text: String,
  attachments: Vec<String>,
  queued_id: Option<String>,   // 对应 Inbox 里的消息 id
  starts_segment: bool,        // 这条消息开启一个新的 transcript segment
}
```

- 初始输入：`starts_segment = false`、`queued_id = None`；
- 引擎注入 steering 时：`starts_segment = true`、`queued_id = Some(message.id)`。

### 2. `EVENT_SCHEMA_VERSION` 3 → 4

- 纯增量：两个字段都带 serde 默认值（`#[serde(default)]`），旧日志缺字段时按 `false` / `None`
  读入；不迁移、不重写历史文件。
- **新读旧**（v3）：字段缺省 → 投影回退到现有启发式。
- **旧读新**（v4）：旧二进制忽略未知字段，启发式仍然能拆出同样的 turn（steer 消息依然是在
  turn 中途到达的 `UserMessage`），因此不需要强制升级。

### 3. 投影规则（ui-state）

- `starts_segment == true` → 直接 `split_turn`，不再看启发式；
- `event.v < 4` 且当前 turn 已有输入 → 沿用启发式（兼容 v3 日志）；
- v4 且 `starts_segment == false` → **不拆分**（显式优先，即使当前 turn 已有输入）。

`Turn.number` 继续取 `event.turn`（run 编号），split 出来的 segment 共享这个编号，只作为
没有输入文本时的兜底标题；一次 run 的 `TurnEnded` 继续收口该 run 打开的所有 segment。

### 4. UI 对账（依赖本 ADR）

Shell 收到 `UserMessage` 且 `queued_id` 非空时，立即从 `steering` 镜像里移除对应 id；
250ms ticker 保留为兜底（engine 在另一个线程 claim）。

**备选方案（未采用）**

- **新增 `EventKind::TurnSegmentStarted`**：所有消费者都要处理新变体，而边界信息本来就在
  那条 user 消息上，多一个事件只会增加同步点。
- **在 `Event` 信封上加 `segment: u32`**：每个事件都要变，改动面远大于收益。
- **把 `Event.turn` 改成 segment 编号**：`turn` 是 run 计数器，store 的索引、TUI 的状态栏都
  依赖它，不能改语义。
- **继续只用启发式**：来源一多就会错拆/漏拆，而且 UI 永远拿不到 steering 与消息的对应关系。

**后果**

- `UserMessage` 的所有构造点都要补两个字段（编译期强制）；改动集中在 engine 与各测试夹具。
- ui-state 需要同时保留显式与 v3 启发式两条路径，直到确认不再需要回放旧日志。
- 旧二进制读 v4 日志仍能工作，但用不上显式标记——可接受。
- 为 ADR-0042 §8 的「乐观回显」（request id）留好了 id 钩子，但本 ADR 不做回显。

**验证**

- engine：初始输入事件 `starts_segment=false` / `queued_id=None`；steering 注入事件
  `starts_segment=true` 且 `queued_id` 与 Inbox 中的 id 一致。
- ui-state：v4 显式 segment 正确拆分；v3 缺字段回退启发式；v4 且 `starts_segment=false`
  不拆分；`TurnEnded` 收口一次 run 的多个 segment。
- serde：v3 JSON（缺字段）反序列化成功；v4 事件序列化/反序列化往返；新字段不破坏旧 reader
  的解析。
- GUI：收到带 `queued_id` 的 `UserMessage` 后，对应 pending steering 行立即消失。

**TUI 对齐（2026-10-30）**

- `qc` 的 TUI 复用同一个 `core::Inbox`：运行中 Enter 排队（`UiAction::Queue`），Ctrl-S 推送
  （`UiAction::Steer`）；状态栏显示 `queued` / `steering` 计数。
- turn 结束后按 FIFO 自动开下一轮；leftover steering 由引擎在 `finish()` 提升到 next-turn，
  TUI 与 GUI 行为一致。
- 队列与 steering 的查看/调整：`/queue`（列出排队）、`/steer N`（推入运行中的 turn）、
  `/drop N`（删除）、`/steering`（列出等待注入的）、`/unsteer N`（撤回，回到队尾）。
  规则统一由 `core::SessionQueue` 提供，两端共用。

**实现更新（2026-10-30）**

- `send_mode` 落地为 `gui.json` 字段：`enter`（默认，保持既有手感：Enter 发送、Shift+Enter
  换行、Cmd/Ctrl+Enter 也可发送）或 `cmd_enter`（Enter 换行、Cmd/Ctrl+Enter 发送）。本文
  §6 原本把默认写成 `cmd_enter`；实现以 `enter` 为默认，避免改变已经习惯 Enter 发送的
  用户，`cmd_enter` 作为可选项保留。
- `busy_delivery` 落地为 `config.toml` 的 `[ui]`（GUI 与 TUI 共用）：`queue`（默认）或
  `steer`；`steer` 时运行中的发送直接进入当前 turn 的下一步，不再进队列。
- 两者都出现在 GUI Settings（Agent / Appearance 页），TUI 读取同一份配置。
- Composer 四态（§1）已实现：`qcode_ui_state::ComposerState`（Idle / Running / Pending /
  Restoring）。Pending 由审批卡片占用输入面；Restoring 从提交答案到 `ApprovalClosed` 之间
  锁定输入并显示「已提交，等待引擎确认…」，TUI 状态栏显示 `restoring`。PlanReview / Question
  仍不在范围内（引擎尚无对应交互）。

## ADR-0055：模型接入——路由 + 目录 + 注册表，模型选择两级化（2026-11-04）

**背景**

`docs/design/SETTINGS-MODELS-V2.md`（配套两份 UI 稿）盘点了设置页与模型接入的十类痛点，与
架构直接相关的有四类：

1. **表单反过来约束配置**：`forms.rs` 的 FIELDS 手写每个字段，加一个配置项要在 forms /
   setup / config_edit 三处同步，11 个页面已经不可维护。
2. **provider 是「map key + 名字 match」**：`resolve.rs` 的 `builtin()` 与 `build_provider()`
   按供应商名硬编码端点与协议，接一个 OpenAI 兼容网关（火山方舟、自建中转）必须改代码。
3. **模型选择是单级**：顶栏下拉 = 所有 provider 的 `default_model` 并集，选中后只知道模型名
   不知道路由；切换不落盘，重开会话要重选。
4. **凭证与 provider 表耦合**：密钥内嵌在 provider 行里存进钥匙串，但「存了没有、缺不缺」
   没有统一视图（存储规则见 ADR-0056）。

**决定**

### 1. 目录：内置路由是一张表

`crates/qcode-kernel/src/config/catalog.rs` 新增 `CATALOG`：每个内置供应商是一行
`CatalogEntry`（协议、base_url、`api_key_env`、默认模型、模型清单、可选的
`reasoning_split`）。用户配置缺字段时 `resolve` 回落到目录行；设置页与解析器读同一份事实
（`catalog_entries()` / `catalog_entry(route)`），不再各存一份内置值。

### 2. 协议显式化：按 `Protocol` 选适配器

新增 `Protocol::{OpenAi, Anthropic, Mock}`（配置写 `protocol = "openai"` 等）。解析顺序：
`providers.<route>.protocol` → 目录行 → OpenAI 兼容。`provider_registry.rs` 的
`build_provider_for` 按协议构造适配器，取代按名字 match 的 `build_provider`。平台型供应商
因此零代码接入：目录加一行，或用户在设置页填 base_url + protocol + 模型清单。

### 3. 注册表服务设置页

`provider_registry.rs` 同时提供 `ConfigurableProvider`（route、display_name、protocol、
configured、凭证状态）与 `list_configurable_providers(config)`；Provider 目录卡片直接渲染它，
`CredentialState::{Configured, Missing, Keyless}` 让「缺 key」「无需 key」可见。

### 4. 模型选择两级化

- **会话级**：`ModelSelection { provider, model }` 取代裸模型名。`UiCommand::SetModel
  { provider, model }` 切换后写会话记录，恢复会话自动还原；跨 provider 切换由
  `build_provider_for` 就地重建（保留会话、工具、策略与 egress proxy）。
- **默认级**：新命名空间 `[default-model] { provider, model }`（`DefaultModelConfig`）。
  会话内切模型同步写入（「切即默认」，写失败只告警）；设置页 provider 卡片的「设为默认」
  是唯一显式入口，写入时清掉遗留的 `default_provider`。
- **选择链**（`config/resolve.rs`）：

  ```text
  provider: --provider > QCODE_PROVIDER > [default-model].provider
            > default_provider（仅兼容旧配置）> 第一个可用 > mock
  model:    --model > QCODE_MODEL > [default-model].model（仅当它指向本路由或未指向任何路由）
            > providers.<route>.default_model > 目录默认 > models[0] > MissingModel
  ```

  「第一个可用」= 遍历 `config.providers`（按字母序、排除 `mock*`），取第一个端点合法
  （http/https）、凭据可用、且能推导出模型的 route。

### 5. 勘误：设置页改默认模型不生效（2026-11-10）

V2.1 收敛后留下一个缺口，用户实测踩到：在设置页把**默认 provider** 的「Default
model」改成 `DeepSeek-V4.1-Flash`，保存成功、表单显示新值，跑任务用的还是旧模型。

根因是两条写路径撞在同一个语义上：

- `[default-model].model` 在读时**优先于** `providers.<route>.default_model`（§4 选择链）；
- 设置页 provider 行的「Default model」只写后者，「设为默认」只改 provider 名（已经是
  默认时不渲染），底部模型菜单的「切即默认」是唯一真正生效的入口——但它不在设置页。

于是同一个 provider 内换默认模型，设置页**没有任何入口能生效**，而 UI 也没说清。

修法（两处，互补）：

- **保存时同步**：`setup/fields.rs::sync_default_model` 在 `apply_tables` 里，若被保存的
  provider 就是 `[default-model].provider`，把它的 `default_model` 写进
  `[default-model].model`。语义变成「一个 provider 只有一个默认模型」，两个字段不再是
  两个真相。行被清空（交回目录默认）时**不动**全局值：那是菜单单独钉的，一次无关的
  表格自动保存不该抹掉它。
- **设置页补入口**：顶部「当前默认：provider / model」变成可点按钮，展开按 provider
  分组的候选（读内存草稿，不按凭证过滤——设置页正是补凭证的地方），选中即写
  `[default-model]`（`pin_default`）。被 `[default-model]` 压住的 provider 行给出注脚
  「被全局默认『x』覆盖；保存后以这里为准」（`overridden_default_model`）。

同时补一条反向保护：设置页每次存表格都会重写整份配置，而 `[default-model]` 另有一个
写者（主窗口菜单）。`adopt_disk_default_model` 在写盘前取回磁盘现值——这次保存没改过
它就用磁盘的，改过（同步刚发生）才用草稿。没有这一条，「改完又被改回去」会从另一个
方向复现。

验证：`a_saved_provider_model_is_the_one_resolve_returns` 走**真实 `resolve`**，
断言保存后返回的模型变了（只断言「同步函数被调用过」可以全绿而用户仍看到旧模型）；
`a_table_save_keeps_the_model_the_menu_just_picked` 覆盖反向保护。

#### 5.1 仍然失败：界面根本不让你写下这个模型名

上面那版修完后用户复测仍然是「没有成功」。查证据：`~/.config/qcode/config.toml`
的 mtime 停在修复之前，且 `.bak` 与正文只差一个无关字段——**他的编辑一次都没有落盘**。
所以问题不在「写了被覆盖」，在「压根写不下去」。

真原因是模型名本身：`DeepSeek-V4.1-Flash` 既不在 `catalog`（deepseek 只有
`deepseek-v4-flash` / `-vision-exp` / `-pro`），也不在 `[[providers.deepseek.models]]`。
而两处「设模型」的界面都**只列已登记的模型**：

- 设置页顶部的「当前默认」选择器（§5 刚加的那个）：候选 = `models` ∪ `default_model` ∪
  目录模型；
- 主窗口底部的模型菜单：同一个来源。

于是这个模型名在界面上**没有任何位置可以输入**，用户只能手改配置文件。这不是「没生效」，
是「没有入口」——上一版修的是「入口不生效」，这一版补的是入口本身。

修法：

- 设置页选择器底部加一行「设为 {provider} 的默认模型」+ 输入框 + 「设为默认」
  （`SetupView::default_model_input`）。手输的模型落在当前默认 provider 上；没有默认
  时落在候选里的第一个。
- 底部模型菜单：搜索框里打一个列表中没有的名字时，多出一行「使用「x」（provider）」，
  Enter 同样生效。手输的模型名不该只能点鼠标。
- `pin_default` 现在**同时**写 `[default-model]` 与 `providers.<p>.default_model`
  （`pin_default_fields`）。只写前者的第一版有个延迟撤销：任何一次表格自动保存都会用
  行值把 `[default-model]` 同步回去，用户选完模型改个别字段就被改回来。

界面「只列已登记模型」这条规则本身是对的（不能凭空造模型），缺的是**逃逸口**：
模型名是新版本、网关别名、或者目录还没跟上时，必须能直接输入。

验证：`a_pinned_model_survives_the_next_table_save` 钉住「两处成对写」以及「后续
`sync_default_model` 同步不会撤回」；`the_synced_model_survives_a_real_write_and_read` 走真实
`write_config` → 读取 → `resolve`，证明落盘链路（`apply_config` 会写
`[default-model]`，不是只写内存）。

**备选方案（未采用）**

- **继续用 FIELDS 手写表单**：加一个配置项仍要改三处，正是最初的痛点。
- **保留按名字推断协议、每新增供应商写分支**：表达不了「同一协议多个网关」，也解释不了
  火山方舟这类平台型供应商。
- **直接跳到「schema 生成表单」**（设计文档 §4.6 的终态）：一次性重写 11 页风险过大；先让
  目录与注册表就位，表单仍手写，等页面收敛到 5 组再换。
- **只保留 `default_provider` + `providers.*.default_model`**：表达不了「模型属于哪个路由」，
  跨 provider 的同名模型会串。

**后果**

- 设置页新增 `crates/qcode-gui/src/setup/models.rs`（1,180 行）承载卡片与模型清单编辑，已超
  团队 1,000 行阈值，后续按「卡片 / 清单 / 探测」拆。
- `KNOWN_KEYS`（`config/load.rs`）必须跟着新命名空间走：v0.6.1 漏了 `ui` 与 `default-model`，
  写这两节的用户会看到 `unknown config keys are ignored` 警告（`84e12d7` 修，随 0.7.0 发布）。
- `default_provider` 与 `[default-model]` 会并存一段时间：读时前者只作兜底，写时清掉，
  迁移期结束后可删。
- 端点模型探测（`qcode-providers/src/discovery.rs`）要处理超时与 404：探测失败只影响候选
  清单，不影响已保存的模型。

**验证**

- `cargo test -p qcode-kernel config` 覆盖选择链顺序、`[default-model]` 的「限本路由」、
  第一个可用兜底、`UnknownProvider` / `MissingModel`。
- 设置页：Provider 目录显示凭证状态；「设为默认」写 `[default-model]` 并清 `default_provider`；
  顶栏跨 provider 切换时会话不中断。
- 手工：把 `[default-model]` 指到未配置的 route，启动回落到第一个可用，再回落到 mock。

**后续（2026-11-10）：id 与显示名**

用户再次报「设了 `deepseek-v4.1-flash` 还是不对」。直接打
`GET https://api.deepseek.com/models`（`/v1/models` 同样可用）得到的是：

```json
{"data":[
  {"id":"deepseek-flash","name":"DeepSeek-V4.1-Flash","context_window":1048576,
   "max_output_tokens":393216,"input_modalities":["text","image"], ...},
  {"id":"deepseek-v4-pro","name":"DeepSeek-V4-Pro", ...}]}
```

即 **id 是 `deepseek-flash`，`DeepSeek-V4.1-Flash` 是 `name`**。用户照着显示名填，
拿到的是 400；catalog 当时还写着 `deepseek-v4-flash`，等于把错的名字又坐实了一遍。
两处都已修：catalog 改成 `deepseek-flash` / `deepseek-v4-pro`（`b630898`），
探测结果只取 `id` 这一点保持——`name` 是给人看的，写进配置就会被当 id 用。

## ADR-0056：凭证存储——密钥不进命令行（2026-11-04）

**背景**

设置页保存密钥的实现最初把秘密交给外部命令：

- macOS：`security add-generic-password -U -s qcode -a <provider> -w <key>`
- Windows：`cmdkey /generic:qcode/<provider> /user:<provider> /pass:<key>`

`argv` 对同用户的任何进程可见（`ps -o args`、WMI `Win32_Process.CommandLine`），密钥在进程
存活期间等同明文；`cmdkey` 还会把凭据写进注册表。另外 `windows_credential_read_script`
用字符串拼接把 provider 名塞进 PowerShell 单引号串，名字里带 `'` 就能逃逸成任意 PowerShell。

**决定**

### 1. 配置文件只存引用

`providers.<route>.api_key_env`（环境变量名）或 `api_key_command`（读回命令的 argv，一行一个
参数），明文永远不落盘。

### 2. 存储后端：秘密只走 stdin 或进程内 API

| 平台 | 写入 | 读回（`api_key_command`） |
|---|---|---|
| macOS | Security framework：`SecKeychainAddGenericPassword`，重复项走 `SecKeychainItemModifyAttributesAndData`（即 CLI 的 `-U`） | `security find-generic-password -w -s qcode -a <route>` |
| Linux | `secret-tool store`，密钥写 stdin | `secret-tool lookup service qcode user <route>` |
| Windows | `CredWriteW`（`CRED_TYPE_GENERIC`、`CRED_PERSIST_LOCAL_MACHINE`），PowerShell 从 stdin 读密钥 | `powershell -NoProfile -Command <CredReadW 单行脚本>` |

macOS 选 FFI 而不是「让 `security` 从 stdin 读」：`security add-generic-password -w` 的值只能
作参数；省略值会两次交互式读密码（`readpassphrase`），有控制终端时走 `/dev/tty`——由
`cargo run` 之类启动的应用会卡住，由 Finder 启动又走 stdin，行为随启动方式变化。

### 3. 生成的脚本必须单行且转义

`api_key_command` 按行拆成 argv，所以生成的 PowerShell 不能含换行；provider/route 名一律经
`ps_single_quoted()`（`'` → `''`）插入。生成器放在不依赖平台的模块
（`setup/credentials.rs`）里，任何主机都能跑转义与「单行」单测。

### 4. 状态可见、失败不静默

`CredentialState::{Configured, Missing, Keyless}` 反映「已存 / 缺 / 无需」；写入失败把
`OSStatus` 或 `CredWrite` 结果回显在行内，不吞掉。

**备选方案（未采用）**

- **`security -w <key>` / `cmdkey /pass:<key>`**：argv 泄漏，正是本 ADR 要消除的。
- **把密钥写进 config 或注册表明文**：违反 ADR-0050「配置只存可审阅的引用」，`qc doctor`
  也一直把明文列为问题。
- **由应用自己读环境变量**：同用户进程仍可从 `KERN_PROCARGS2` / WMI 读到进程环境；只在用户
  显式配置 `api_key_env` 时保留这条路（CI 与容器）。
- **让 `security` 从 stdin 读密码**：见上，依赖调用方有没有控制终端。

**后果**

- 依赖系统凭据库可用：headless、锁屏或受限会话写不进去时要回落到 `api_key_env`；错误文案
  带上 `errSecInteractionNotAllowed` 之类的原始状态码，便于判断。
- macOS 首次读回仍可能触发钥匙串授权对话框（写入方是 `qc`，读回方是 `/usr/bin/security`）；
  测试覆盖了「写入方与读回方是不同进程」这条路径。
- Windows 读回每次都启动 PowerShell 并 `Add-Type` 编译 C#（约 1 秒级），且没有缓存——属性能
  问题，另案处理。
- `qc doctor` 的 Windows 提示从 `cmdkey` 改为「在设置页保存」，并点明 `cmdkey /pass:` 的泄漏
  风险。

**验证**

- macOS：`setup::tests::macos_keychain_store_keeps_the_secret_out_of_argv` —— 写入（含重复
  写入走修改分支）→ 用存下的命令读回 → 断言 argv 里没有秘密 → 删除探测项；钥匙串不可用的
  会话跳过而不是失败。
- Windows：`setup::tests::windows_keychain_store_round_trips_via_credwrite_and_powershell` ——
  `CredWrite` 写入后由 `CredRead` 脚本读回，结束时 `CredDelete` 清理。
- 跨平台：`windows_credential_scripts_quote_provider_names` 校验单行、`''` 转义、密钥只来自
  stdin。

## ADR-0057：工具改动落盘——schema v5 的 `ToolDiff` 与两处视图（2026-11-07）

**背景**

对话流里的工具行一直只显示「调用了什么、输出了什么」。DSH 的界面把写文件也当作一等的
transcript 事实：行头有 `+N −D` 与文件大小，展开就是红绿 diff，底部还有「查看」的全量
视图。我们此前只有一个从未生效的抽屉（`shell/mod.rs` 读的是 `arguments["patch"]`，而
`apply_patch` 的参数名是 `diff`），且只覆盖 `apply_patch`。

要让 GUI 与 TUI 都能显示真实改动，事实必须落在日志里：transcript 完全由 JSONL 重建，
运行时内存里的工具输出到不了回放路径。

**决定**

### 1. `ToolResult` 增加 `diff`，`EVENT_SCHEMA_VERSION` 4 → 5

```text
ToolResult {
  call_id, status, output, metadata,
  diff: Option<ToolDiff>,          // 纯增量，缺省即「非写操作 / 无从得知」
}

ToolDiff {
  patch: String,                   // 统一 diff，带 --- / +++ 头
  files: Vec<DiffFileStat>,        // path, kind(Created|Modified|Deleted), additions, deletions, bytes
  truncated: bool,                 // 超过 64 KiB，patch 已丢，只信计数
}
```

放在 `ToolResult` 而不是 `ToolMetadata`：metadata 描述「输出是怎么被裁剪的」
（`truncated`/`bytes`/`exit_code`/`full_output_path`），diff 是这次调用的一等结果。
`bytes` 取**新内容**的字节数，供行头的「1KB」使用，不去 `stat` 文件。

### 2. 产出在这里：工具自己算

- `edit_file`：内存里已有 before/after，直接对两段文本做 `similar::TextDiff`。
- `write_file`：写之前读一次旧内容（存在、< 256 KiB、UTF-8 才算），没有就是 `Created`。
- `apply_patch`：`PlannedOp` 补 `before`，落盘后**按实际结果**重算，而不是回声模型给的
  patch——echo 会漏掉 partial apply 与规范化后的空白。
- 失败、取消、被拒的调用不产出 diff。3 行上下文、`a/`+`b/` 前缀、缺失侧写 `/dev/null`，
  与 `DiffFile::parse` 的期望一致。

### 3. 消费侧容错两条路

- 新日志（v5）直接用 `ToolResult.diff`；
- 老日志（v <= 4）用 `qcode-ui-state::tool_diff::fallback` 从调用参数重建：`apply_patch`
  用它自己的 patch 原文（含统计），`write_file` 合成全绿，`edit_file` 只有两段片段 →
  保留计数、放弃 patch 文本（行号留空，不编造）。

### 4. 渲染：懒解析 + 明确失败

- GUI：行头 `+N −D`（仅新建时附大小）；展开区在命令/输出盒子**之上**插 `Diff`；卡片高度
  按 patch 行数取 `96..320px`（`Diff` 的 body 是 `flex_1`，父容器不给高度会塌成一条线）；
- `DiffState` 在「展开」或「查看」时才 `DiffFile::parse`，长会话只为看过的 patch 付费；
  会话加载时 `rebuild_inline_diffs` 一次性重建（回放不走实时钩子）；
- 解析失败的 patch 记入 `broken_diffs`，行保留计数但**不显示**「查看」，避免点了没反应；
- 底部抽屉改成「查看」全量视图（480px，只剩「关闭」），修掉 `patch`/`diff` 的死代码；
- turn 头从「执行过程 · N 项」换成按工具归类的摘要（修改了文件 / 执行了命令 / 已读取文件），
  无可归类工具时退回原文案；
- TUI：行头同样 `+N −D`，新增 `/diff`（列出带改动的调用）与 `/diff <n>`（打印 patch）。

**备选方案（未采用）**

- **解析流里的 `run_shell` 输出来造 diff**：只能靠 `git diff` 之类的启发式，跨平台不可靠，
  且看不到 `apply_patch` 这类直接写盘的工具。
- **UI 侧根据参数猜**：`edit_file` 的 `old_string`/`new_string` 只是片段，行号与上下文都
  无从得知——这正是 v4 日志回退路径的已知损失。
- **每条工具结果都存全量前后文件**：日志体积不可控，且可执行权限之外还多了隐私面。

**后果**

- 日志变大：每次写操作多几百字节到 64 KiB。超限时丢 patch、保留计数。
- 纯增量字段：旧日志照常读，旧二进制忽略未知字段；`v <= 4` 的投影启发式（`starts_segment`）
  不受影响。
- Windows 上 `write_file` 多一次读旧文件（< 256 KiB），可接受。

**验证**

- `crates/qcode-tools/src/diff.rs` 单测：修改/新建/删除/多文件/超限/空改动。
- `crates/qcode-tools/tests/tools.rs`：三种写工具的 diff 与 `apply_patch` 的 per-file 统计。
- `crates/qcode-core/tests/event_schema.rs`：`ToolResult` + `diff` 的完整 JSON 快照。
- `crates/qcode-providers/tests/tool_diff.rs`：用脚本化 provider 让模型真的调 `write_file`，
  从 `SessionStore::load` 断言 diff 进了日志、文件真的写了。
- `crates/qcode-ui-state`：v4 回退的三条路径、`/diff` 的列表/单条/越界。
- `crates/qcode-tui/tests/render.rs`：行头计数。
- GUI 实跑（v5 与 v4 各一条 fixture 会话）：行头 `+1 -1 · kb/README.md`、展开的红绿卡片、
  「查看」全量面板。

## ADR-0058：多任务并行——会话宿主、事件归属与 UI 槽位（2026-11-08）

**状态**：Accepted（2026-11-08）。**P0–P6 已实现**（2026-11-08；P6 见 ADR-0059）。
依赖 ADR-0053（Queue/Steer 语义）与 ADR-0057（日志是 transcript 的唯一权威）。
实现注记见本文末。

**背景**

GUI 现在同一时刻只能有一个任务在跑：切任务 = 取消当前 turn。

- **UI 闸**：`shell/session.rs:40-45`（新建）与 `:121-127`（恢复）遇到
  `app.status.running` 直接拒绝，提示「先停止」。
- **引擎闸**：`runtime.rs:115-128` 只有一个 `session` / `active` / `queue` /
  `published`；`:244-261` 收到 `NewSession` / `Resume` 时先 `token.cancel()` 再把
  命令塞进 `pending`，turn 结束才执行。
- **状态闸**：`shell/mod.rs:14-130` 的转录状态是单份的（一个 `App`、一个
  `ApprovalQueue`、一个 `queue`/`steering`、一个 `turn_in_flight`、一个 scroller、
  一套展开集），且 `poll` 只在 `render_main`（`render/main.rs:506`）里跑。

但「多个会话同时跑」在本仓库已经不是新能力：

- subagent 用同一份 store / provider / tools 在另一个 session 上跑 `run_turn`
  （`qcode/src/subagent.rs:28-47`）；
- 会话写锁是**按会话**的 flock（`qcode-core/src/store.rs:42-44`、`:279-305`），
  sqlite 索引有 busy_timeout（`store.rs:421-428`）；
- ACP 已经是多会话形状：`Agent { sessions: HashMap<String, SessionState{cancel,
  running}> }`（`qcode-acp/src/server.rs:107-124`），`prompt` 按 session 取
  `cwd` + `cancel`（`:241-268`），通知出口是 `client.notify(session_id, update)`
  （`:266-272`）。

缺的不是执行能力，是**宿主**。同时有三处事实缺归属：

1. `Delta`（`qcode-core/src/engine/mod.rs:41-50`）与 `ApprovalView`
   （`qcode-ui-bridge/src/lib.rs:56-65`）都没有 session 字段——只有
   `UiEvent::TurnFinished { session }` 带了；
2. allowlist 是全局 `HashSet<tool>`（`qcode-ui-bridge/src/lib.rs:143-144` 的
   `session_allowed`），并行后 A 任务的「Always」会替 B 任务放行；
3. `BridgeApprover` 没有超时：审批事件被丢弃 = 该 turn 永久挂起，会话锁不释放。

顺带修一个现存 bug：`apply_rebuild`（`shell/mod.rs:645-709`）换掉 backend 之后没有
对 active 会话 `resume`，保存设置后下一条消息会新开 session。

**决定**

### 1. 一个 `SessionHost` 是唯一的多会话权威

```rust
pub struct SessionHost {
    deps: Arc<Deps>,                    // store / provider / tools / policy / snapshots
    slots: Mutex<HashMap<String, Slot>>,
}
struct Slot {
    cancel: CancellationToken,
    queue: SessionQueue,                // 每会话投递（ADR-0053）
    phase: Phase,                       // Idle | Running | WaitingApproval
}
```

GUI / ACP / TUI / headless / 调度器都只是它的消费者。`Backend` trait 保留，但退化为
「UI 与 host 之间的适配层」，不再持有单会话状态。宿主只负责三件事：把命令路由到会话、
把事件打上会话标签、把空闲会话的后端收起来。

### 2. 事件一律带会话标签

```rust
pub struct SessionEvent { pub target: SessionTarget, pub event: UiEvent }
pub enum SessionTarget { Session(String), Draft }
```

`Delta` / `ApprovalView` **不加字段**：由外层携带，避免给每个变体都改签名，也不会在
新增变体时漏标。`SessionTarget::Draft` 指「还没写日志的草稿会话」——
`UiEvent::SessionChanged { id: None }` 之后的第一条消息才落盘。

顺序是契约：**同一会话内严格保序**（`App::apply` 的调用顺序就是 transcript 的顺序），
跨会话不保证。实现上每会话一路 mpsc，UI 侧轮转 drain——一条多生产者 channel 会让一个
话痨会话饿死其它会话，也让「后台会话只投递边界」的策略没地方落（ADR-0059 §5）。

### 3. 审批按会话隔离，且永不丢

- allowlist 从 `HashSet<String>` 改为 `HashMap<session, HashSet<tool>>`；
- 审批进**该会话槽位**的 `ApprovalQueue`，UI 只画当前任务的，侧栏给「待审批」徽标；
- 后台槽位必须一直消费事件，否则 turn 挂起（`BridgeApprover` 无超时）；
- 回答走 `UiCommand::Answer { session, call_id, allow, grant }`。

第 2、3 条把「`poll` 从 render 移出」从优化升级为硬前提（§5）。

### 4. UI 侧：每任务一个 `TaskUi` 槽位

槽位内容按 `shell/mod.rs` 现状归类：`app`（transcript / status / streaming 块）、
`units`、`scroller`、`selected_turn`、`approvals`、`queue`、`steering`、
`turn_in_flight`、`restoring`、`resetting`、`empty_draft`、`error_banner`、`notice`、
输入框文本 + `attachments` + `file_refs`、`expanded_process`、`expanded_code`、
`process_open_turns`、`collapse_all`、`collapse_to_list`、`inline_diffs` +
`broken_diffs` + `diff_view`、`last_blocks`。

交换用 `capture(&mut self) -> TaskUi` / `install(&mut self, ui: TaskUi) -> TaskUi`
（返回被换下的那份，放回 `parked: BTreeMap<String, TaskUi>`）。字段仍是 `Shell` 的
顶层字段，只是「内容是当前任务的那一份」——`render/*.rs` 读的永远是 `self.app`、
`self.scroller`，**一行不改**。

反面的约束必须写死：**所有延迟回调都要按会话 id 校验**（`if self.active.as_deref()
!= Some(id) { return }`）或改走 host 命令，否则会在切换后写错槽位。今天
`wake_for_session`（`shell/session.rs:99-119`）与 `set_notice` 的 6 秒 timer
（`shell/mod.rs:584-601`）就是这种形状，需要逐个核对。

### 5. `poll` 移出 render，改成常驻 ticker

从 `render_main`（`render/main.rs:506`）挪到 `cx.spawn` + `background_executor()
.timer(33ms)`，或由 host 在事件到达时唤醒 `AsyncApp::update`。理由：窗口最小化 / 隐藏时
不 render，后台 turn 结束、审批到达、`TurnFinished` 无人处理。今天的注释
（`shell/mod.rs:571-575`：「`poll` 是在 render 里跑的……要等下一次交互」）就是这个洞的
自白；并行之后它会从「延迟」变成「挂起」。改完之后 `poll` 是幂等的 drain，可以从任何
地方调。

### 6. Stop 与退出

- `UiCommand::Cancel { session }` 只停指定会话；输入框的 Stop 传当前任务 id；
- 侧栏每个任务的 ⋯ 菜单加「停止」，后台任务也能停；
- 托盘 / 窗口退出 = 向所有会话 `cancel`，等各自 `TurnFinished` 再退；有 turn 在跑时
  仍走 ADR-0052 的确认；
- 强杀不需要恢复逻辑：日志是权威源（ADR-0057），下次 `resume` 重放。

### 7. 生命周期与重建

| 任务行状态 | 来源 |
|---|---|
| 运行中 | 槽位 `turn_in_flight` |
| 待审批 | 槽位 `approvals` 非空 |
| 未读 | 非活动会话在离开后产生了 turn 边界或最终消息 |
| 空闲 | 以上皆无 |

- 改名 / 删除 / 改 roots 必须按会话排队（现在只看全局 `running`：`shell/tasks.rs:95`、
  `:233`、`task_ops.rs:8`）；
- 配置变更（`reload_on_config_change`，`shell/mod.rs:606-643`）→ 标 `pending_rebuild`，
  等所有会话回到 `Idle` 后统一 `apply_rebuild`，并对每个活跃会话 `resume`，修掉背景里
  那个 bug；
- 空闲的非活动会话可以丢后端（drop runtime + bridge），重新聚焦时用 `resume()` 重放；
  正在跑的不丢。

### 8. 阻塞 IO 出 runtime 线程

`read_file` / `list_dir` / `write_file` 在 `async fn` 里直接 `std::fs`
（`qcode-tools/src/fs.rs:85`、`:331`、`:30-38`），而 GUI runtime 是
`new_current_thread`（`qcode-gui/src/runtime.rs:103-108`）。单会话只是偶尔卡一帧；
多会话会互相卡住（一个任务读大文件时，另一个任务的流式输出与取消全部停摆）。决定：这些
调用改 `spawn_blocking`。不选「host 换 `new_multi_thread`」——那只是把问题摊到更多线程
上，ACP / TUI / headless 仍各自为政。

### 9. 落地顺序

| 阶段 | 内容 | 依赖 |
|---|---|---|
| P0 | 阻塞 IO 改 `spawn_blocking`；`apply_rebuild` 保留 active 会话；turn panic 兜底发 `TurnFailed` | 独立可合 |
| P1 | `Engine` → `Arc` 共享；`SessionHost` 骨架（map + 按会话 `cancel`/`queue`）；`SessionEvent` 标签 | P0 |
| P2 | Shell 拆 `TaskUi` + `capture`/`install` + `parked` | P1 |
| P3 | `poll` 按会话路由并移出 render；dormant 槽只更新状态不 notify | P2 |
| P4 | 生命周期：徽标、按会话排队的改名/删除/改目录、`Cancel{session}`、退出聚合 | P3 |
| P5 | 后端可回收 + `focus` 时 `resume` 重放 | P4 |
| P6 | 定时触发（ADR-0059） | P5 |

**备选方案（未采用）**

- **每任务一个 backend（`LocalBackend::fork()`）**：`Engine` 全字段可 Clone，fork =
  clone 再 `runtime::spawn` 一个新线程 + 新桥，`runtime.rs` 与 bridge 完全不动，隔离由
  结构保证，改动最小。不选的理由是定时任务（ADR-0059）需要「谁在哪个会话跑」的全局
  视图：N 个独立后端里没有这个视图，只能在旁边再长一个注册表把它们串起来——那是宿主的
  弱化版，还要付 N 个线程 + N 个 250ms ticker 的空转。另外第一步就该确立事件标签，否则
  shell 的路由层要写两遍。
- **只切视图、不并行**：删 `running` 闸、后台槽位吃事件、切回重放。改动最小，但引擎仍是
  单会话，新任务里没法对话——不满足需求本身。
- **单 runtime 多会话但不抽宿主**：在 `runtime.rs` 里加 `HashMap<session, (token,
  queue)>`、删掉 `pending` 的取消-重发。能跑，但 GUI 成为唯一的多会话实现，ACP 那份
  `sessions` map 与它各写各的，调度器落地时还要写第三份。
- **给 `Delta` / `ApprovalView` 各加 `session` 字段**：每个构造点都要改，且新增变体容易
  漏；外层包一层只改一处。

**后果**

- 内存：每个活动任务保留一份 `App`（transcript units 是主要成本）。先不设上限，用 8 个
  任务实测；超预期再对非活动槽位做「只留最近 N 个 turn，切回从日志重建」。
- 事件顺序成为新契约（见验证）；
- 取消语义改为按会话：`UiCommand::Cancel` 与 `Backend::cancel` 签名变，所有调用点跟着改；
- 审批的「永不丢」只承诺投递，不承诺有人答：后台任务的审批会一直挂着，直到切回去。定时
  任务的超时规则见 ADR-0059 §4；
- `shell/mod.rs` 的顶层字段仍是「当前任务的那一份」，是明确的技术债，收敛留待后续。

**验证**

- `SessionHost` 单测：两个会话并发 `run_turn`，事件标签正确、互不串话；同会话第二次提交
  排队，不撞 `StoreError::Locked`（`store.rs:34`、`:850`）。
- 审批：A 会话「Always」不影响 B；后台会话的审批不丢，回答后 turn 继续。
- Shell：`capture` / `install` 往返后同一任务的 render 结果与切换前逐字段一致（草稿、
  滚动位置、展开集、审批队列、notice）。
- `poll` 移出 render：不调 render 直接注入 `TurnFinished`，侧栏状态仍更新。
- 重建：保存设置后 active 会话被 `resume`，下一条消息落在同一会话（今天会新开）。
- 手工：三个任务同时跑并来回切；后台任务审批到达时侧栏有徽标，切回能答；Stop 只停当前
  那个。

**实现注记（2026-11-08，P0–P1）**

1. **P0 全部落地，三处都在，可独立回滚**。
   - `qcode-tools/src/fs.rs` 的 `read_file` / `write_file` / `edit_file` / `list_dir`
     改走新增的 `blocking()`（`spawn_blocking` + panic 转 `ToolError::Io`）。顺带把
     `write_file` 的「读旧文本」也搬出去——它同样是一次同步读。
   - `qcode-gui/src/runtime.rs` 的 `TurnGuard`：`tokio::spawn` 里 panic 会走 Drop 而不是
     `done.send`，守卫把它变成一条 `TurnFailed`，`active` 因此一定被清掉。回归用例是
     `tests/backend.rs::a_panicking_turn_reports_turn_failed_and_keeps_the_loop_alive`
     （临时把守卫关掉，该用例会红 3 秒后失败，确认它测的是这件事）。
   - `apply_rebuild` 保留 active 会话这条在 `fe6a5d6` 已修，本次只核对未改。
2. **`SessionHost` 住在 `qcode-gui/src/host.rs`，不在 core**。P1 的消费者只有 GUI；
   等 ACP / TUI 真正接入（P3 之后）再决定是否上移，避免先抽一个只有一个实现的抽象。
   它不依赖 GPUI，用例在 `tests/host.rs`（12 条），`qcode-gui` 的 `test = false` 是
   ADR-0037 的约束。
3. **§2 的标签没有落成 `UiEvent` 的变体字段**，而是 `SessionEvent { target, event }`
   包一层，由 `KernelPort::tagged(target)` 产生只认识一个会话的 `TaggedPort`。
   `Delta` / `ApprovalView` 一行未改——这正是 §2 的意图，顺带保证「新增变体不会漏标」：
   根本没有不加标签的发法，`KernelPort` 上不再有 `send_event` 这类裸方法。
4. **`SessionTarget` 同时是 host 的键**（而不是 `String` + 哨兵值）：草稿态是个真正的
   枚举分支，不需要 `""` 这种假 id。
5. **§3 的审批隔离已落地**：allowlist 是 `HashMap<session, HashSet<tool>>`，「Always」
   只settle 同一会话里同工具的并行 prompt（`another_session_keeps_its_own_prompts_after_always`）。
   `clear_allow` 按会话清，不再是全局清空。
6. **一个 `Engine` 实例仍按窗口持有，但 turn 通过 `Deps` 的 `Arc` 共享**（§1 的
   「`Engine` → `Arc` 共享」）：`SessionHost::deps()` 给出唯一一份，turn 只 clone `Arc`。
   P2 起每会话槽位直接读它。
7. **P1 时还没做的部分**：`poll` 仍在 `render` 里（§5）、按会话排队改名/删除（§7）、
   `BridgeApprover` 的超时（§3 第二半）、后端回收与 `resume` 重放（§5）、定时触发（P6）。
   因此 P1 结束时**没有**让两个 turn 真正并行：`active` 仍是单槽，`Cancel` 已按会话
   路由但只有一个会话可停。

**实现注记（2026-11-08，P2）**

8. **§4 的落法与原文有一处偏离，是故意的：字段没有留在 `Shell` 的顶层**，而是
   `Shell { ui: TaskUi, parked: BTreeMap<String, TaskUi>, … }`，`render/*.rs` 里的
   `self.app` / `self.scroller` 改成了 `self.ui.app` / `self.ui.scroller`（314 处，
   纯机械、编译期可查）。理由：§3「后台槽位必须一直消费事件」+ §5「`poll` 按会话
   路由」要求*给一个不在屏上的会话*应用事件。若按原文用「顶层字段 + `capture`/
   `install` 逐字段搬运」，一次后台事件就要把 30 个字段（含两个 `Entity`）搬进搬出，
   而 `Entity` 的占位符得用 `cx.new` 现造——每来一条流式 `Delta` 都要造两个实体，
   P3 会直接卡在这一点上。改成子结构后 `install` 是纯 `mem::replace`，O(1)、不碰
   实体，后台更新就是 `self.parked.get_mut(&id)`。
   原文「render 一行不改」的*目的*（渲染层不引入路由错误）仍然守住了：渲染读的永远
   是「当前任务那一份」，没有任何 render 代码需要知道会话。
9. **`capture` 留占位视图**：`capture` = `install(fresh_task_ui(..))`，占位视图的
   `App` 继承 provider/model，状态栏不会闪空。`park_and_clear` 顺手 `active.take()`：
   草稿没有 id、没有可回去的地方，直接丢掉（原来也是丢的）。
   **2026-11-10 修订：这条是错的，已改**。草稿没有 id 不等于没有内容——丢掉的正是用户
   刚打的字。现在停放的键是 `SessionTarget`，草稿停在 `Draft` 槽位上，`+` 装回来（详
   见 ADR-0062 末尾「『回到任务』删掉了」那条注记）。
10. **每个视图有一个 `id`**（单调计数器），三处延迟回调按它校验：两个 observer
    （滚动同步、回车发送）、`set_notice` 的 6 秒 timer、`wake_for_session` 的轮询
    循环。这是 §4「所有延迟回调都要按会话 id 校验」的落地形式；用视图 id 而不是会话
    id，因为草稿没有会话 id 也必须可区分。
11. **切回去不再重放，除非日志动了**。`TaskUi.seen_seq` 记着这份视图见过的最新
    `Event.seq`；`resume` 先用 `backend.resume` 拿到日志（顺带完成引擎侧挂接），
    只有 `seen_seq >= 最后一条 seq` 才把停放视图装回去，否则丢掉它照旧重放。这条闸门
    是必需的：另一个窗口或 `qcode` CLI 可能趁本窗口停着不动给同一会话写了新 turn，
    那时停放视图是旧的。重放那条慢但永远对；恢复那条才是「滚动位置、展开集、草稿
    都还在」的来源。
12. **恢复时不再往转录里塞 "resumed xxxx"**：停放视图的转录会累积，每切一次多一行。
    恢复走状态栏的 6 秒 notice，只有真重放时才推转录行（与原行为一致）。
13. **删除任务会清 `parked`**，否则被删会话的转录与两个实体一直留在内存里。
14. **没有 cap**：`parked` 随访问过的会话数增长，每个是一份转录。ADR §后果 已接受，
    本次不设上限（留待实测后再定）。
15. **仍留在 `Shell` 上、按 §4 清单不进 `TaskUi` 的字段**（各有理由）：
    `pending_roots` 与 `pending_task_renames`（turn 边界统一落盘，P4 才按会话拆）、
    `copied_command` / `rail_popup` / `rail_popup_close_at`（几秒内自愈的悬停与复制
    提示）、`transcript_height`（每帧重新测量的 `Rc<Cell>`）、`busy_delivery`（配置）、
    `pending_send`（输入事件与下一帧 render 之间就消费掉）。
16. **P2 没有自动化用例**：`TaskUi` 里有 `Entity`，构造它要 `Window` + `Context`，
    而 `qcode-gui` 的 `test = false`（ADR-0037：test harness 会把 rustc 栈拓爆）。
    能给的保证是编译期的：`install` 里那个 `TaskUi { … }` 字面量**没有 `..`**，
    `TaskUi` 多加一个字段就编不过，不存在「新增字段忘了交换、切任务后串味」这种静默
    失败。运行时的往返一致性（草稿、滚动位置、展开集、审批队列、notice）仍需按
    「验证」一节手工确认。

**实现注记（2026-11-08，P3）**

17. **`poll` 搬进常驻 ticker**（§5）。入口是 `Shell::start_poll_ticker`，在 `run.rs`
    里 `open_window` 拿到实体之后启动一次（幂等，`ticker_running` 兜住重复调用），
    间隔自适应：有 turn 在跑 33ms，全空闲 150ms（runtime 的队列快照本来就是 250ms
    一拍，闲时空转 30 次/秒纯属浪费）。`render_main` 里那行 `self.poll(cx)` 删掉。
    - 顺带删掉 `wake_for_session`：它存在的唯一理由是「poll 只在 render 里跑，新建
      任务后要主动醒几次」。ticker 一帧内就把 `SessionChanged` 排空并自己 `notify`，
      这个 100ms×50 的循环没有意义了。
18. **路由表**（`shell/routing.rs` 的 `apply_to_ui` + `dormant.rs` 的
    `apply_to_dormant`）：按 `SessionEvent.target` 分三档——
    - 目标就是屏上那份视图 → 照旧应用，行变了就 `sync_scroller`，并 `notify`；
    - 目标是 `parked` 里的会话（用户去过）→ 应用到它自己的 `TaskUi`，**不 notify**
      流式增量，只在 turn 边界/审批这类「一行状态变了」的事件上请求一帧；
    - 目标是没有视图的会话 → 进 `Dormant`：只有状态（running）、审批队列、队列快照、
      失败提示，`Delta` 直接丢（切进去时从日志重放，ADR-0057）。
19. **`SessionChanged` 不分发**，它是窗口级事件：P3 时 runtime 仍是单槽，「引擎现在挂在
    谁身上」就是「显示谁」（P5 把语义收窄成「只回应窗口自己的 `Resume`/`NewSession`」，
    见注记 40）。给它做分发需要「给一个没打开过的会话造视图」，而那要
    `Window`（composer 是实体），`poll` 手上没有。真正的多槽切换（P5）之后它才需要
    按目标区分；那时也才有窗口上下文可用（切换本来就是用户操作，走 `resume`）。
20. **审批的「永不丢」现在真的成立了**：后台会话的 `Approval` 进 `Dormant.approvals`
    （或 `parked` 视图自己的队列），`resume` 时 drain 进新视图，再配合已有的
    `call_id` 路由（`BridgeApprover` 按 call_id 答，不绑线程），所以后台憋着的 prompt
    切进去就能答，答完引擎那边继续走。`tests/dormant.rs` 的
    `an_approval_arriving_in_the_background_is_kept` 钉住这条。
21. **`Dormant` 会自己收摊**：`is_settled()`（不 running、无审批、无队列、无错误）成立
    就 `remove`。否则一个窗口看十个会话在后台跑过，就留十个空条目。
    错误提示例外——它要留到用户看见为止。
22. **切走时 turn 的收尾事件落在它自己的视图里**：`TurnFinished` / `TurnEnded`
    按 tag 找到 `parked`，不会再去清屏上那份视图的 `turn_in_flight`。这是 P1/P2
    遗留的一个真 bug（单会话时看不出来，因为只有一个会话能跑）。
23. **`task_running(id)` 是三档合一**（屏上 / parked / dormant），侧栏行标记与 ⋯ 菜单
    共用它：后台在跑的任务行显示 `▶`。审批徽标留给 P4（注记 26，`task_state` 的
    `waiting_approval` 一起做）；P3 时只有 `dormant.approvals` 被 `resume` 消费。
24. ~~切走时仍被 `running` 闸拦着~~（`new_session` / `resume`）：runtime 收到 `Resume`
    会先 `token.cancel()` 再切，引擎侧仍是单槽。**P5 已摘掉**（注记 37）。
25. **P3 的自动化只覆盖 `Dormant`**（`tests/dormant.rs`，8 条）：路由与 ticker 需要
    GPUI 的 `Window`/`Context`，仍是手工验证（§验证 里那几条）。


**实现注记（2026-11-08，P4）**

26. **徽标（§7 的表）落成 `task_ops::task_state(running, waiting_approval, unread)`**，
    优先级就是表里的顺序：运行中 > 待审批 > 未读 > 空闲。三份事实按会话各自找家
    （`Shell::task_facts`）：屏上视图读自己，停放的读 `parked`，没打开过的读
    `Dormant`。侧栏行标记：屏上 `●`、后台在跑 `▶`、待审批 `⚠`、未读 `◉`、空闲 `○`；
    待审批另在标题后加「· 待审批」（一个 `⚠` 说不清在等什么）。
27. **未读是窗口级的集合**（`Shell::unread`），判据是 `task_ops::marks_unread`：
    会话**不在屏上**时收到 `TurnEnded` / `AssistantMessage` / `TurnFailed` 就打标；
    `resume` 时清。流式 `Delta` 不算（重放会由权威消息取代），`TurnStarted` 也不算
    （还在做，不是「有话说」）。删任务时连同 `parked`/`dormant`/未读一起清。
28. **改名/删除/改 roots 真的按会话排队了**（§7）。`should_queue_rename(turn_in_flight,
    active, task_id)` 换成 `task_ops::edit_must_wait(task_state(id))`：只有**这个任务**
    在跑才排队，别的任务在跑不再误伤（以前只要窗口里有一个 turn 在跑，改任何任务都要
    等）。`pending_roots` 从 `Option<(id, roots)>` 变成 `BTreeMap<id, roots>`，
    `TurnFinished` 只应用**完成的那个会话**的待处理目录，另一份原样留着。
29. **`Cancel` 带会话**：`UiCommand::Cancel { session: Option<String> }`（`None` = 草稿）
    + `UiCommand::CancelAll`；`Backend::cancel(session)` / `cancel_all()`。
    runtime 侧 `host.cancel(&target)` 总是执行（后台会话的 token 也停掉），
    turn 的 token 只在 `target == current` 时 cancel——引擎还是单槽，这是「诚实的一半」：
    接口按会话，实现按「当前那个会话」收敛。`tests/backend.rs` 的
    `cancel_is_processed_while_a_slow_turn_is_running` 现在先证明**错误的会话 id 不会
    停掉**在跑的 turn，再证明正确 id 会。
30. **侧栏 ⋯ 菜单加「停止」**，后台任务也能停：`Shell::stop_task(id)`。停止时的视觉
    收口（`mark_stopped`）也按会话找家——屏上、停放、dormant 三处各自清 running。
31. **退出聚合**（§6）：`request_quit` 用 `any_turn_running()`（三处合看）判断要不要弹
    ADR-0052 的确认；`confirm_quit` 发 `CancelAll`，只清**视觉**上的 `status.running`
    （`turn_in_flight` 不能动，它才是「引擎那个 turn 还没返回」，也就是要等的东西），
    置 `quitting`，由 `poll` 在 `any_turn_running()` 变假时 `cx.quit()`。另加 5 秒兜底：
    provider 一时不理会取消令牌时，退出不能变成卡住——日志是权威源（ADR-0057），强杀
    不需要恢复逻辑，这正是 §6 允许这么办的理由。
32. **`TurnFinished` 的 shell 级收尾按会话触发**：`apply_pending_roots(session)` /
    `apply_pending_task_renames(session)` 在 drain 里对每个 `TurnFinished` 执行，与「是不是
    屏上那个会话」无关。改名也只落**完成的那一个**：另一个会话还握着它自己的文件锁，
    替它落盘只会拿到 `Locked` 并把这次编辑丢掉。
33. **P4 的自动化**：`tests/task_ops.rs`（状态优先级、`edit_must_wait`、未读判据）5 条，
    `tests/backend.rs` 加了 `cancel_all_stops_the_running_turn` 并在原用例里加了
    「错 id 不停」。徽标渲染、`quitting` 的等待与 5 秒兜底需要 GPUI，仍是手工验证。

**实现注记（2026-11-08，P5）**

34. **runtime 从单槽改成按会话并行**（§7）。循环里的 `active: Option<CancellationToken>`
    / `session: Option<String>` / `pending: Option<UiCommand>` 换成
    `live: HashMap<SessionTarget, CancellationToken>`：每个在跑的 turn 一个条目，`current`
    只剩下「窗口在看谁」这一个意思。`start_turn!` 第一个参数是目标会话——草稿在第一个
    turn 变成会话，其余目标本来就带 id，所以后台任务的提问不会落到窗口正在看的那个任务上。
35. **`done` 通道带会话**：`(SessionTarget, Result<(), String>)`。`TurnGuard`（P0 的 panic
    兜底）也带上目标，否则 panic 后 `live` 里会留下一个永远不消失的条目。收尾一律
    `live.remove(&target)` + `host.end_turn(&target)` + 只 drain **那个会话**的 follow-up
    队列：以前这些都用 `current`，多个 turn 时会清错任务的状态。
36. **`UiCommand::Prompt { session }`**：提问也带会话（`Backend::prompt(session, …)`）。
    这是「并行」的必要条件——只按 `current` 路由的话，一次过期的界面切换就会把消息投到
    别人身上。`Cancel { session }` 的 `None` 现在明确表示「窗口正在看的那个」（草稿），
    runtime 用 `current` 兜底。
37. **切任务不再要求先停**（摘掉 P3 遗留的 `running` 闸）：`new_session` / `resume` 里那两处
    `if status.running { … 请先停止 … }` 删掉。`Resume` / `NewSession` 在 runtime 里也不再
    `token.cancel()` + `pending` 重发，只是重新指一下 `current`；离开的会话若空闲则
    `close_if_idle` 回收它的槽（见 39），在跑的话原样留着。切回来时如果它还在跑，
    `resume` 不清它的 "Always" 列表——同一个 turn 里问第二遍是噪音。
38. **「后端可回收」在这里的落法**：引擎是共享的（§1），所以会话没有自己的后端可 drop；
    它持有的是一个槽（cancel token + 队列）。`close_if_idle` 就是全部——空闲且队列空的槽
    直接忘掉，切回来时 `resume()` 从日志重放（ADR-0057），视图里的一切都重建得出来。
    这也让 `Dormant` 的自动收摊（注记 21）与槽的回收是同一件事的两种表现。
39. **配置变更等所有会话空闲**（§7，补上 P4 那段遗留）：新字段
    `Shell::pending_rebuild`。`reload_on_config_change` 发现还有 turn 在跑就只置位 + 提示，
    `poll` 在 `any_turn_running()` 变假时调 `apply_config_change` 真正重建并重新挂上屏上的
    会话。以前是直接 `cancel` 当前 turn 再重建——并行之后那样会连带掐掉别的任务的输出。
40. **`SessionChanged` 仍是窗口级**，但语义收窄了：它只在 `Resume` / `NewSession` 之后
    由 runtime 发出（后台 turn 启动不发），所以「引擎现在挂在谁身上」正好等于「显示谁」，
    不需要分发。广播 `SessionChanged` 反而是 bug——会把用户正在看的视图抢走。
41. **P5 的自动化**：`tests/backend.rs` 新增
    `two_sessions_run_at_the_same_time`（B 的 `TurnStarted` 必须早于 A 的 `TurnEnded`，
    且 A 是 `Completed` 不是 `Cancelled`——切走不等于停）与
    `stopping_one_task_leaves_the_other_running`（停 B 时 A 仍 `Completed`）。
    多槽视图切换、`pending_rebuild` 的等待仍要 GPUI，是手工验证。
42. **模型仍是窗口级的**：`UiCommand::SetModel` 换的是这条引擎任务的 `model` / `provider`，
    之后**任何**会话新起的 turn 都用它（包括后台任务排队后自动接着跑的那个）。按会话
    记模型要动 `Task` 与设置持久化，超出这次的范围；现在的行为至少是「所见即所用」：
    头部选择器显示的就是下一个 turn 会用的模型。写在这里是因为并行之后它第一次有了
    歧义（以前同时只有一个会话，没有「别人的 turn 用谁的模型」这个问题）。
43. **P6 已落地**（定时触发，ADR-0059）：前置——「谁在哪个会话跑」的全局视图——由
    `SessionHost` + `live` 提供，触发走同一个 `SessionQueue`；`queue` / `skip` 的撞车
    策略、错过计数与审批 deadline 都在 ADR-0059 的实现注记里。本 ADR 的并行骨架因此
    不再有未完成阶段。

## ADR-0059：定时任务——声明在配置、状态在 store、跑在会话宿主上（2026-11-08）

**状态**：Accepted（2026-11-08）。**已实现**（2026-11-09），依赖 ADR-0058。

**背景**

定时任务对宿主的要求比并行更硬，这也是 ADR-0058 选宿主而不选「每任务一个后端」的主要
原因：

1. 需要「谁在哪个会话跑」的全局视图，才能决定**排队还是跳过**；
2. 一个时钟，不是 N 个——每会话后端各带 ticker 的拓扑会把时钟摊开，还会在后台任务上
   空转；
3. 无人值守要统一出口：日志 + 未读 + 审计；
4. 无人应答的审批必须自己收场，否则占着会话锁到早上。

**决定**

### 1. 声明在配置，运行状态在 store

```toml
[schedules.nightly-review]
cron = "0 3 * * *"          # 五段，本地时区
task = "01J..."             # 复用一个已有任务
# new = true                # 或每次触发新建任务
prompt = "..."
overlap = "queue"           # queue | skip
approval_timeout = "30m"    # 0 = 等到有人答
enabled = true
```

`Config` 的字段都没有 `deny_unknown_fields`（`qcode-kernel/src/config/mod.rs:77-121`），
旧二进制读到 `[schedules]` 会忽略，符合 ADR-0050 的「配置只存可审阅的引用」。

**运行状态（`last_fired_at` / `next_fire_at` / `last_status`）落 store 的
`schedules` 表，绝不写回配置文件。** 硬理由：`reload_on_config_change`
（`shell/mod.rs:606-616`）按 mtime 变化触发重建，调度器每触发一次就改配置 = 每触发一次
就换 backend + 一次 `resume`，还会与用户在 Settings 里的编辑互相覆盖。

### 2. 触发与撞车

- 到期 → 向该会话 `submit`（走 `SessionQueue`，ADR-0053）。会话忙时按 `overlap`：
  `queue`（默认）排到当前 turn 之后，`skip` 丢弃并记 `schedule_skipped`。
  **不允许出现 flock 撞车报错**这种路径。
- `task` 指向的会话已被删除 → 禁用该条目并记 `schedule_orphaned`，不静默跳过。
- 时钟注入 `Clock` trait（测试用假时钟推进）。判断到期用单调时间，落库用墙钟，两者分开：
  系统时间被改或机器休眠时，墙钟只影响「下次何时」的显示，不影响本次是否已触发。

### 3. 错过不补跑

进程不在时窗口已过期 → 记一条 `schedule_missed`（带错过窗口数），**不补跑**，侧栏对应
任务行显示提示。`catchup = false` 是默认，字段留着但不实现。

### 4. 无人值守的审批有 deadline

由调度器触发的 turn，其审批带 `approval_deadline`：超时 → `deny`，turn 继续（日志里就是
一条 `status: Error` 的 `ToolResult`，`output` 写 `denied: approval timeout`，
`qcode-core/src/engine/tools.rs:295` 已有这条路径，**不需要改事件 schema**）。
人工触发的后台任务仍然无限等——人就在机器旁边。

### 5. 后台会话的投递策略

非活动会话只投递：turn 边界（`TurnStarted` / `TurnEnded` / `TurnFinished`）、最终的
`assistant_message`、失败的 `tool_result`、`Error` 与审批。**`Delta` 与中间过程全部
丢弃**（`Delta` 本来就不落盘）。跑一晚不等于把整晚的 token 流堆在内存里；切回去
`resume()` 从日志重建完整过程。

### 6. 宿主住在哪

本轮住在 GUI 进程里：托盘常驻（ADR-0052，关闭窗口 = 最小化），所以「窗口关了」不等于
「调度停了」；**退出进程则全部停止**，这是明确边界。系统通知不在本轮（不引入新依赖），
只做侧栏徽标与未读。`qc daemon`（无窗口宿主，复用 `SessionHost`）留 v2——到那时 GUI
降级为观察者，`[schedules]` 与状态表一行不改。

**备选方案（未采用）**

- **运行状态写进配置文件**：mtime 监听会把它变成重建风暴，还要与 Settings 的写入竞争。
- **每任务一个后端各自计时**：时钟散在 N 处，撞车只能靠 flock 报错，排队 / 跳过策略无处
  安放。
- **错过补跑**：休眠一夜后一次性放出一堆 turn，烧钱且互相争会话锁。
- **交给系统 cron 调 `qc -p`**：每次触发一个进程，无法复用会话的排队与审批队列；无终端
  时审批直接失败。

**后果**

- 无人值守链路仍靠 `[agent] max_turns` 兜底：审批被 deny 后模型可能反复尝试，turn 预算
  是最后一道闸。
- 无人看的会话会积累日志；保留策略不在本 ADR。
- `schedules` 表是新 schema 版本（store 迁移，自动补表）。
- 时区取本地时区；DST 跳变按「跳过该次、记 missed」处理，与 §3 同一路径。

**实现注记（2026-11-09）**

1. **判定是纯逻辑，住在 `qcode-core/src/schedule.rs`**：`CronSpec`（解析 + `matches` +
   `next_after`）、`tick(schedule, state, now, busy, task_exists)`、`recover`（错过计数）、
   `advance`（记录本次决定并排下个窗口）。所有函数把「现在」当参数，测试用假时间，
   `qc daemon` 将来可以复用同一套判定。`crontab(5)` 的细节都跟了：日与星期**都**受限时
   取「或」、`*` 不算受限、名字（`jan`/`mon`）、`7` 折成周日、`*/15` 与 `5/15` 两种步长。
2. **时区**：`now_local()` = UTC 现在 + `localtime_r` 给出的偏移，**每次现算**（不是启动时
   缓存），所以长开窗口遇到 DST 跳变时下个窗口跟着挪。没用 `time` 的 `now_local()`：那个
   特性文档里写着「多线程下不健全」，而 GUI 线程早就不止一个；`qcode-core` 本来就为会话
   锁链了 `libc`。
3. **窗口 vs 清醒**：判定需要区分「tick 到了」和「进程睡过去了」。wall clock 只用来算
   「下一个窗口何时」，另用单调时钟（`Instant`）记上一拍：间隔超过三拍就算「不在场」，
   走 `recover`（计数不补跑）。这样系统时间被改前改后都不会凭空多跑或少跑。
4. **运行状态落 store 的 `schedules` 表**（`name` / `last_fired_at` / `next_fire_at` /
   `last_status` / `missed_windows`，建表随 `index.sqlite` 的 `CREATE TABLE IF NOT
   EXISTS` 自动补）。配置文件的 mtime 因此**一次都不变**——ADR 里担心的重建风暴从根上
   没有了。表里留着的孤儿行由 `prune_schedules(declared)` 清掉。
5. **配置层**：`Config.schedules: BTreeMap<String, ScheduleConfig>`
   （`[schedules.<name>]`：`cron` / `task` / `new` / `prompt` / `overlap` /
   `approval_timeout` / `enabled`），`resolve_schedules` 逐条转成 `Schedule`，坏条目
   **逐条**报错而不是让整份配置加载失败：一个 cron 打错不该让窗口起不来。旧二进制读到
   `[schedules]` 只会忽略（顶层键已登记进 `KNOWN_KEYS`，不会打 unknown-key 警告）。
   Settings 里**没有** cron 编辑器：`[schedules.<name>]` 是「用户自己起名的表套表」，
   表单造不出键；`tests/schema.rs` 里显式跳过并写明理由。
6. **触发就是一次排队**：到期的条目把 `prompt` 交给该会话的 `SessionQueue`，与用户打字
   走同一条路（`Submission::Start` → `start_turn!`，忙则排队）。所以被触发的 turn 没有
   任何特殊路径：徽标、未读、事件路由、日志全都复用 P3–P5 的东西。`overlap = "skip"` 忙时
   直接记 `Skipped`；任务被删记 `Orphaned` 并保留在那里等用户处理，不静默跳过。
7. **`new = true` 由 `create_task(..., announce = false)` 建**：定时任务新建的会话必须落进
   侧栏，**不能抢走用户正在看的视图**——所以它不发 `SessionChanged`。
8. **无人值守的审批有 deadline**：`TaggedPort::approver_with_deadline(d)`（`BridgeApprover`
   多一个 `deadline` 字段，`None` 永远等）。超时 → 撤回 prompt + `Answer::Deny`，turn 继续
   走既有的「denied」工具结果路径，会话锁按时释放。只有定时 turn 挂 deadline：人触发的
   后台任务仍然无限等（人可能只是离开了键盘）。`time` 特性加了 `test-util` 才能用
   `#[tokio::test(start_paused = true)]` 把 30 分钟压缩成瞬间。
9. **侧栏提示**：`Backend::schedules()` 返回「声明 + store 里的运行状态」，
   `Shell::schedule_hints` 按任务 id 收好（错过 N 次 / 已失效 / 失败 / 配置有误），任务行
   在审批提示之后显示它。提示优先于「目录丢失」以外的其它后缀，避免一行里堆三句。
10. **本轮没做的**（都在 ADR 原文里就划出去或留到后续）：系统通知（不引入新依赖）、
    `qc daemon`（v2，GUI 关窗 = 托盘常驻仍然负责计时）、`catchup = false` 以外的补跑
   策略、Settings 里的 cron 编辑、`last_status` 的历史记录（只留最后一次）与
    `schedule_skipped` / `schedule_orphaned` 的独立事件（现在都写进 `last_status`，
    因为 `Event` schema 里没有这两类，ADR 也明确说「不需要改事件 schema」）。
11. **验证**：`crates/qcode-core/src/schedule/tests.rs` 18 条（字段语义、日/星期的或、
    非法表达式、`next_after` 跨月跨年与永不成立、窗口触发、忙时排队或跳过、禁用与孤儿、
    错过计数、状态编解码）；`config/schedule/tests.rs` 7 条（默认值、`new` 与 `task` 互斥、
    超时单位与 `0`、坏条目逐条上报、老配置兼容）；`store` 的 5 条（往返、覆盖非重复、
    带负载的状态、prune、手改坏时间）；`tests/backend.rs` 端到端 2 条（到期真的在它指向的
    任务上开了一个 turn；指向已删任务的条目只记录不跑）。`qcode-ui-bridge` 2 条钉住
    「定时审批超时 deny / 人工审批无限等」。

**验证**

- 假时钟：到期触发一次；同一窗口不重复触发；`next_fire_at` 落库后重启仍准确。
- 撞车：会话忙 + `queue` → 排在当前 turn 之后；`skip` → 记事件且不排队。
- 错过：时钟跳过 3 个窗口 → 一条 `schedule_missed`，零 turn 启动。
- 审批：定时 turn 的审批超时 → deny + turn 正常结束，会话锁释放。
- 回归：调度器写状态后**配置 mtime 不变**（防重建风暴）。
- `task` 被删除 → 条目禁用 + `schedule_orphaned`。

## ADR-0060：预算暂停——超限是「交接」，不是「报错」（2026-11-09）

**背景**：`max_turns` 命中时，GUI 弹的是红色「请求报错 · 完整信息」，正文是
`exceeded max turns (64): …`，只有一个「关闭」按钮。三处预算止停（轮数、成本、
无进展）走同一条失败通道：写 `error(...)`、`turn_ended(failed)`、返回 `Err`。可是
**预算用完不是错误**——任务可能完全正常，只是钱或步数到了，重试同样的输入必然原地
不动。用户真正想知道的是「做到哪了、还差什么」，而这些数据全在手里：usage / cost、
本轮的 `tool_call` 序列、v5 起每个 `tool_result` 的 `diff`、最后一段助手文本。
附带一个真 bug：GUI 的「Turns 超限」提示用 `app.turns.len()`（用户消息条数）当轮数，
而 `max_turns` 是单条消息内的模型调用数——长任务不提示，短任务乱提示。

**决定**

1. **四态**：`TurnStatus` 增加 `Paused`。预算命中落 `Paused`，**无进展与真错误仍落
   `Failed`**（前者是方法错了，继续只会重复；后者重试可能成功）。落盘 schema 5 → 6；
   v5 日志无新字段，serde 缺省即可回放，不回填。
2. **成本提前刹车**：`max_cost_usd` 从「已花超才停」改为 `used >= limit * 0.95` 即停，
   剩下 5% 留给收尾调用。`max_turns` 仍在「恰好用完、下一轮将超」时停。
3. **临近时先说**：新增 `EventKind::BudgetHint`，首次跨过 75%（或剩 ≤ 8 轮）发一次，
   带**引擎自己的** `turns / max_turns / cost_usd / max_cost_usd / window`。GUI 的提示卡
   与 TUI 状态行都只读它，`app.turns.len() >= threshold` 的口径 bug 随之删除——
   提示不会在短任务上误报，长任务也不会漏报。
4. **停时交接**：可推导的不落盘，不可推导的落盘。
   - `qcode_core::progress::report(events, turn) -> ProgressReport`：纯函数，零成本、
     可复现，GUI / TUI / ACP / `qc run` 共用（工具调用数、按路径去重并累加计数的
     `DiffFileStat`、命令列表、失败工具行、截断到 600 字的最后文本）。
   - `PauseInfo.summary`：停之前给模型**一次无工具的收尾调用**，让它写「已完成 /
     未完成 / 下一步」。这次调用**不计入 `max_turns`**（否则「为了总结又把预算花光」），
     成本**照实记账**；估算放不下或调用失败就跳过，`summary = None`，退化为只剩
     `ProgressReport`。UI 按小标题轻解析，解析失败整段显示——不赌模型出 JSON。
5. **事件形状**（schema v6）：`TurnEnded{status, usage, cost_usd, pause}`；
   `PauseInfo{reason, limit: Turns(u32) | Usd(f64), used: {turns, cost_usd}, window, summary}`。
   `window` 必须进日志：配置会被改、价表会换，事后无法从事件流反推「这是第几个预算窗口」。
6. **「继续」不是伪造用户消息**：继续 = 新 turn（预算天然重置），但 transcript 不能
   出现用户没说过的话。`UserMessage` 加 `continuation: Option<Continuation{turn, window, raise}>`，
   UI 渲染成「继续上一次暂停的任务」的灰底提示条，不再是用户气泡。
7. **`window` 由引擎数**：`run_turn` 在 `begin_turn` 之前读日志，用
   `budget_window(&events) + 1` 与最后一个事件的 turn 号**覆盖**前端传来的值。前端算错、
   传旧值、甚至伪造一个大值，都拿不到新预算窗口。
8. **三键卡片，琥珀色，不写「报错」**：`继续`（primary）/ `收尾`（再跑一次，只让它写
   交接）/ `先这样`（把卡收起，暂停态留在日志里）。卡片内容是 `ProgressReport`
   （改了哪些文件 `+N −D`、跑过的命令、失败的工具行）+ `summary` + 一行预算口径
   （`已用 64/64 轮` 或 `已达成本预算 $5.00 的 95%`）。GUI 在对话流里渲染（随 turn 滚动，
   而不是挤在输入框上方抢焦点），TUI 渲染在 transcript 末尾，`/continue` 与 `/wrap`
   是同一动作的键盘入口。
9. **无人值守链路默认停，不等**：
   - `qc run`：退出码 **75**（`EX_TEMPFAIL`，区别于通用失败 1、取消 130、超时 124），
     打印摘要 + 预算口径 + 继续方式；`--json` 在流末多打一条
     `{"type":"pause","data":{session_id, reason, limit, used, window, summary}}`。
   - `--continue`（`--resume <id>` 必需，prompt 可省）接着上次的任务跑；
     `--raise-turns N` / `--raise-cost USD` 只抬这个窗口的上限，`--raise-persist`
     才写回 `config.toml`。
   - ACP：`stop_reason = max_turn_requests` **保留**（协议语义正是「requests 用尽」），
     但**不再先发 error chunk**——交接摘要走 `agent_message_chunk`，客户端读到的是
     可继续的正常收尾。
   - 子 agent：不抛错。`SubagentOutcome.text` 追加
     `[child paused: used 20/20 turns — the task above is incomplete; …]`，
     父 agent 自己决定再派一次还是收工，避免把半成品当成品。
10. **不自动续**：维持 ADR-0031 §6 的立场（自动续会把「原地打转」伪装成「任务很长」），
    但把「继续」做成一键、默认焦点，体验差距被抹平而原则还在。
11. **回放卡片只读**：重放不产生副作用；历史 `Paused` 只显示内容，不发任何注入或网络
    调用。

**备选方案（未采用）**

- 继续伪造一条用户消息：transcript 会说谎，且导出/分享的日志里会出现「你没说过的话」。
- 前端数 `window`：三条链路（`qc run` / 子 agent / ACP）各数一遍必然分叉，且能被伪造。
- 把 `ProgressReport` 也落盘：它能从事件流精确重算，落盘只会在 schema 演进时变成第二份
  真相。
- `no_progress` 也可继续：它是「方法错了」，继续等于允许原地烧钱。
- 单独一条 `pause` 事件而不是塞进 `turn_ended`：turn 的结局只该有一个权威记录，两处写
  一定会有一处先写后崩。

**后果**

- `EngineError::MaxTurns` / `MaxCost` 不再是 `Err`：调用方必须读 `outcome.pause`，
  否则会把暂停当成功（子 agent 的拼接文本、`qc run` 的退出码、ACP 的 `stop_reason`
  都在这一处收敛）。
- 收尾调用花的是真实 token：一次 `Paused` 比过去「直接报错」贵一点，但换来可继续的
  交接；估算放不下时会跳过。
- 状态行与提示卡的轮数口径统一到 `budget_hint`，`round_hint_threshold` 字段与
  `ROUND_HINT_THRESHOLD` 常量删除。
- 预算窗口（`window > 1`）会出现在三处：暂停卡片、状态行、`qc run --json` 的 pause 行。
- 老日志回放不产生卡片动作，pause 缺失时一切按 v5 行为（`Failed` 无 `pause`）。

**验证**

- `qcode-providers/tests/cost.rs`：超成本在工具执行前暂停（`PauseReason::MaxCost`、
  `PauseLimit::Usd`、无 error 事件、无 tool call、`TurnStatus::Paused`）；95% 刹车
  （$14.30 / $15）；成本累计跨轮仍暂停。
- `qcode-providers/tests/tool_loop.rs`：第二次运行带 `Continuation` → 事件里
  `window = 2`，且最后一次请求的消息以交接指令结尾（不是污染 system）。
- `qcode-ui-state`：`/continue` 无暂停时只给提示、不启动 turn；有暂停时产生
  `UiAction::Continue` / `WrapUp`；运行中拒绝；`PauseCard` 折叠同一路径的多次改动
  （`修改了 1 个文件（+15 −3）`）、取首行失败输出、字符安全截断。
- `qcode-tui/tests/render.rs`：暂停卡片渲染出 `64/64`、`第 2 个预算窗口`、
  `修改了 1 个文件`、`summary`、失败行与 `/continue` 提示，状态标签 `paused`。
- `qcode-core`：`progress::report` 的单测（聚合、截断）。

## ADR-0061：附件——引用进日志，字节进内容寻址存储（2026-11-08）

**背景**

ADR-0035 把图片输入收成一条路，并顺手划了三条边界：图片只在内存、事件日志只存文本、
resume 重放没有图片。本 ADR 取代这三条边界与 ADR-0035 §5；§1 的 `--image` 输入面保留。
当时只有 `qc run --image` 和 Windows 的 `Ctrl-V`，这些边界是合理的——
没有前端能显示缩略图，落盘只是浪费。

GUI 落地后这三条边界开始漏：

- composer 粘贴一张图，界面上只有一行 `attached 1568x1024 (228 KB), 1 pending`。发送后
  转录区也没有图——**用户看不到自己刚发的那张图**。
- 关窗再开，同一段对话里的图彻底消失，重建出来的消息里也没有。
- 文件走另一条路：`[附件文件]\n<paths>` 直接拼进 prompt 文本（`shell/session.rs`）。路径
  没有校验（存在、可读、不是目录），模型拿到的是一个未经确认的字符串；沙箱里那个路径
  可能根本读不到，模型却仍会"总结"它。
- provider 层把原图 base64 原样发出去：3.4 MB 的 Retina 截图变成 ~4.5 MB base64，只受
  8 MB 上限保护。

两条新事实让落盘变得可行：GPUI 有 `gpui_pre::img()`（`ImageSource` → 元素），能真的渲染
缩略图；`qcode-image` 已有一个无外部依赖的 PNG 编码器（编码侧够用，解码侧还缺，见决定 5）。

**决定**

1. **一个引用类型，两种形态。**

   ```rust
   pub enum AttachmentRef { Image(ImageRef), File(FileRef) }
   pub struct ImageRef { id: [u8; 32], mime: ImageMime, bytes: u64,
                         width: u32, height: u32,
                         origin: Option<OriginRef>, name: Option<String> }
   pub struct OriginRef { id: [u8; 32], bytes: u64, width: u32, height: u32 }
   pub struct FileRef   { id: [u8; 32], name: String, bytes: u64 }
   ```

   `id` 是内容 sha256（存 32 字节，序列化成十六进制）。`ImageRef` 自带尺寸和字节数，所以
   列表渲染、token 估算、上限校验都不读盘。`origin` 指向**未归一化的原图**，只在真的发生
   缩放或重编码时出现；原图与归一化图字节相同时两者同 sha，存储层自然只有一份。

2. **日志只存引用，字节进内容寻址存储。** 目录 `$data/attachments/v1/`：

   ```
   objects/<sha 前2位>/<sha256>       归一化图片（发给模型的那份）
   originals/<sha 前2位>/<sha256>     原图（未缩放、未重编码）
   files/<sha 前2位>/<sha256>/<name>  逐字节文件（同内容只存一份）
   tmp/                               写入中的临时文件
   ```

   落盘：写 `tmp/` → fsync → rename 就位（同一文件系统内原子）→ 目标已存在则丢弃新文件
   （去重）→ 目标设只读。分片目录的理由是 `readdir`：一个目录塞十万个对象会拖慢列表。

3. **schema v6 → v7，但不需要迁移。** `UserMessage.attachments` 从 `Vec<String>` 改成
   `Vec<AttachmentRef>`。该字段从引入起就没写过非空值（`history.rs`、`store.rs`、
   `compact.rs` 三处都写 `vec![]`），所以老日志里它要么缺失要么空数组，`#[serde(default)]`
   就够了；空数组仍然不写。

4. **先落盘，再发送。** 附加动作（粘贴 / 选择 / 拖拽）当场归一化、落盘、拿 ref，composer
   里显示缩略图或文件卡片；发送时只带 ref。失败＝卡片变红、可重试、不产生半张图。好处是
   发送瞬间不卡，重试也幂等（同一个 sha 第二次写入直接命中）。

5. **归一化在写入时做一次，四种格式统一。** 引入 `image`（MIT/Apache-2.0 双许可，
   `default-features = false` + `png`/`jpeg`/`webp`/`gif` 四个 feature）负责解码、EXIF
   摆正、缩放和编码。参数：长边 2048、存下来的单图 2 MB、永不丢 alpha。编码规则：
   有透明像素 → PNG（`CompressionType::Best`）；不透明 → JPEG，质量阶梯 85 → 70 → 55；
   一轮都压不进 2 MB 就把长边乘 0.75 再来一轮，最多八轮。

   已经达标（长边 ≤ 2048、字节 ≤ 2 MB、EXIF 无旋转）的图**原样放行**，不做无意义的
   重编码——小截图与 2×1 的测试图都走这条路，所以 `unchanged` 为真时 `origin` 为 `None`。

   解码前的输入上限是 **32 MB**（`MAX_IMAGE_BYTES`）：它只负责"一次粘贴最多占多少内存"，
   真正的落盘上限是上面那个 2 MB。原图保留在 `originals/`（见决定 2），`origin` 只在真的
   发生缩放或重编码时写；"查看原图"直接读原图，不重新解码归一化副本。

   实现注记：`qcode-image` 原来那个"stored deflate" PNG 编码器不进附件链路——它是给
   剪贴板兜底的（4 字节/像素），拿来存归一化图会把 2 MB 预算烧光；归一化统一用 `image`
   的 PNG/JPEG 编码器。

6. **回放是重建，不是缓存。** `ContentBlock::Image` 的形状**不变**（仍是
   `Image { mime_type: String, data_base64: String }`），引用在重建这一步展开：

   ```rust
   pub fn messages_from_events_with<F>(events: &[Event], load: F) -> Vec<Message>
   where
       F: FnMut(&ImageRef) -> Option<Vec<u8>>;
   ```

   `history` 遍历 `UserMessage.attachments` 里的 `AttachmentRef::Image`，用 `load` 取字节、
   编码成 base64 放进 `ContentBlock::Image`；`load` 拿不到对象就丢掉这张图（引用仍留在
   日志里，下次能补）。turn loop 传 `|image| store.read_image(image).ok()`，provider 只
   看得到一种 wire 形状。

   原本打算让 `ContentBlock` 长出 `ImageSource { Inline | Stored }`，实现时否掉了：那样
   provider 层要么认识 store、要么再被投影一次，测试也得拖一个 store 进来。把读盘收在
   「日志 → messages」这一处等价且少一层；等出现第二个要区分来源的消费者（比如「查看原图」
   走 GUI 而不是回放）再引入。

7. **文件也进存储，但模型看到的是句柄。** 文件逐字节落盘（不归一化、不解析），prompt 里
   给一段固定文案，说清楚：文件名、字节数、sha256 前 8 位、只读路径、"要改先复制"、
   "委派子代理时把路径写进 prompt"。路径解析不出来（沙箱 / 远端执行）时换一句明说
   "读不到，别声称读过"。这条替换当前的 `[附件文件]\n<paths>`：从"用户给的字符串"变成
   "我们写的、校验过的副本"。

   单文件上限 **64 MB**：超过则当场拒绝并提示"先拆分或压缩"，不产生临时文件。复制走流式
   （分块读 → 写 `tmp/` → fsync），不把整个文件读进内存；中途失败就删掉临时文件。

   实现注记：GUI 在选文件/拖文件的那一刻就落盘（输入框上方显示名字、大小、×），发送时
   只传引用；`qc run --file <PATH>` 走同一条路。事件日志里存 `AttachmentRef::File`，
   回放时用 `file_handle_text` 重建同一段句柄文案——所以 resume 之后模型仍拿得到路径。

8. **能力门控改为降级。** 模型 `capabilities().images == false` 时不丢弃图片，换成一行
   占位文本：`[image omitted: this model accepts text only; <mime>, <bytes> bytes. The image
   stays in the session log, so a model that accepts images can replay it.]`。日志里仍是引用，
   换成支持视觉的模型重放就能看到原图。ADR-0035 §3 的"直接报错"改成"降级为文本"——报错
   让用户没法继续，占位只损失一张图。

9. **压缩照旧不看图片字节；请求有预算，超了就从最旧的开始卸载。** `compact.rs` 里图片按
   `base64.len()/4` 估算，摘要输入里替换为占位文本。另外每个请求有图片预算
   （`ImageBudget { max_bytes: 20 MB, max_images: 20 }`，按 base64 长度算），超了就把**最旧**
   的图片换成占位文本 `[image omitted to fit the request image limits; …]`，最新的那张
   最后才走——用户刚发的那张总能看到。

   卸载**不写日志**：日志里仍是引用，下一个还有余量的请求会把它读回来。因为重建是确定的
   （同样的 messages 进、同样的占位出），重放不需要额外事件，也就不必动事件 schema。

   实现注记：`offload_images` 与"模型不支持视觉"时的 `omit_images` 是两条独立路径——前者
   预算问题，后者能力问题，占位文案也不同，读请求日志时能分清是哪种原因。

**边界（明确不做）**

1. **不做 PDF / docx / 表格解析。** 文件只给路径，抽文本交给 agent 跑工具。一旦附件层开始
   理解格式，就要背 PDF 解析、扫描件 OCR、加密文档、字体子集——这些坑的收益远小于"把路径
   交给一个会写代码的模型"。
2. **只处理 8 位 sRGB 的静态首帧。** 16 位深度、CMYK、HEIC/AVIF 一律拒绝并给出明确错误；
   动图（GIF/WebP 动画）只取首帧，其余帧丢弃；不做色彩管理（不转 ICC），不做无损 WebP 编码。
3. **不做 GC。** 删除会话不回收对象，对象只增不减。后续 `qc doctor --gc` 扫"日志里没有
   引用"的对象，而不是在删除路径上做引用计数——那条路要处理崩溃中途、并发会话、跨会话
   共享三种情况，收益不成比例。
4. **不做跨会话共享语义。** 同一 sha 在不同会话指向同一份字节（去重是存储层的事），但
   引用永远属于某一条会话日志。
5. **TUI 不渲染图片。** 只显示 `[图片 1568x1024, 228 KB]`；渲染留给 GUI。
6. **ACP 的 `promptCapabilities.image` 暂时保持 false。** 日志能重放之后技术上没障碍，但
   这是一条新的输入面，等 GUI 路径稳定后再开。

**验证**

- `qcode-attachments`（新 crate，纯逻辑 + 临时目录）：写入→读回字节一致；同内容二次写入
  不产生第二个对象；`<2>` 分片路径可预测；文件名清洗（`../`、Windows 全路径、控制字符、
  超长 UTF-8）；只读位真的只读；对象被删后读取报"缺失"而不是 panic；文件上限 64 MB 通过、
  64 MB + 1 字节被拒，且 `tmp/` 里不残留半个文件。
- `qcode-attachments` 的归一化：PNG/JPEG/WebP/GIF 各解一张，尺寸与 mime 以**解码结果**为准
  （声明成 PNG 实际是 JPEG 的输入要被识破）；EXIF 方向 6 的 JPEG 摆正后宽高互换；带 alpha
  的 PNG 缩放后 alpha 仍在；超 2 MB 时质量阶梯真的降到下一档；`origin` 与 `id` 可分别读回，
  字节相同时只落一份。
- `qcode-core`：带 ref 的 `UserMessage` 序列化快照（v7）；`messages_from_events_with` 用
  store 作 loader 时从带图日志重建出 `ContentBlock::Image`，loader 返回 `None` 时只是丢图、
  不报错；不带 `attachments` 的老日志仍解析成功；压缩后摘要输入里没有 base64。
- `qcode-providers`：回放重建出的 user 消息，与首次发送时直接把字节塞进去的结果**逐字节
  相同**——这是"回放等于首次发送"的硬保证。
- `qcode-gui`：粘贴→缩略图出现；选择文件→卡片显示名字与大小；发送后转录区用户消息带图；
  重启后同一会话仍显示。
- 手工：`--data-dir` 临时目录跑完整流程，再删掉对象文件，确认 GUI 显示"图片已不可用"而
  不是崩溃。

**待定**

1. 归一化参数：2048 长边 / 2 MB / 三档质量是否合适。视觉模型那边单图 token 有上限，1568
   通常已够，可能还要往下调。
2. `qc doctor --gc` 的触发方式与保留策略：`originals/` 是否一起回收；"仍被引用但很久没打开"
   的对象要不要按时间清扫。
3. ACP 何时开图片输入。
4. 解码依赖选型：`image` 一把梭（一个 crate 覆盖四种格式，代价是间接依赖较多），还是
   `zune-jpeg` + `image-webp` + `png` 组合（体积更小，维护面更大）。

## ADR-0062：内建终端——独立执行体、单入口多 tab、状态只在内存（2026-11-09）

**背景**

`qc gui` 现在只有「任务」一种工作对象。想在同一个窗口里敲命令就得切到系统终端，
丢掉 cwd 上下文。

第一约束不是「能不能画格子」，而是**不能拖慢已有两件事**：任务 turn 的流式输出，
和 ADR-0059 的定时 tick。后者不是理论风险——`runtime.rs` 用「相邻 tick 间隔 >
`3 × beat`」判定进程离开（`away`），一旦成立就只重新排期、**不做任何决定**，孤儿与
错过窗口状态都写不下去。任何跑在 `LocalBackend` 那个 `current_thread` runtime 里的
阻塞 I/O（比如读 PTY）都会让这个判定误触发。11-06 加固调度测试时已把这条机制查实。

第二个事实：`gpui-kit 0.9.0`（含 `gpui-component` / `gpui-base`）**没有任何终端
组件**。两个 crate 的完整组件清单里最接近的 `empty.rs` 是「空状态占位」。所以终端
必须自己集成，没有现成控件可用。

**决定**

1. **独立 crate，独立执行体，绝不进 `LocalBackend` 的 runtime**

   新建 `crates/qcode-terminal`，只依赖 `gpui-kit` + `alacritty_terminal`，**不依赖
   `qcode-core` / `qcode-gui`**。这样 TUI / `qc run` / ACP 的构建不含终端，终端也能
   脱离窗口做 headless 测试。

   每个终端一个 OS 线程，读 PTY master、喂 `vte::Parser`、更新 `Term`，并发出
   `Title` / `PtyWrite` / `ChildExit` / `Wakeup`。UI 线程只做一件事——在 `Term` 的
   锁内取一次快照，解锁后绘制。

   这根线程**自己写**，不沿用 `alacritty_terminal::event_loop::EventLoop`：它会丢
   数据（成因、实测数字与改法见下面第 7 条）。复用的是 `tty::new`（会话创建、
   `setsid`、控制终端、Windows 上的 `ConPTY`）和 `Term` 状态机，这两块才是难的部分。

   这个隔离是「不影响任务与定时任务」的全部依据：终端的读、解析、UTF-8 解码全在
   那一根线程上，`LocalBackend` 的 tick 得不到任何额外负载。**不允许**把 PTY 读取
   放进 `runtime::spawn` 的 runtime，哪怕是 `spawn_blocking`——`current_thread`
   运行时下阻塞任务同样会推迟 tick。

2. **单入口 + 内容区多 tab**

   侧边栏在「设置」与「我的任务」之间插一个「终端」按钮，**只有一行**：

   - 当前没有终端 → 新建一个并切入；
   - 已有终端 → 切到**上次活跃的那一个**（不是新建，也不是第一个）。

   多个终端用内容区顶部的 tab 切换（`gpui-component/tab.rs`）。

   `Shell` 新增视图维度 `view: View { Task, Terminal }`，但**不复用 `active`**：
   `active` 现在的含义是「活跃任务 id」，`task_state()`、`hovered_task`、
   `task_menu`、`schedule_hints`、`pending_task_renames`、
   `round_hint_dismissed_tasks` 全按它索引。切到终端时 `active` **不清空**，切回来
   必须回到原来那个任务。

   终端视图下整块右侧被 `TerminalView` 取代：输入框、rail 圆点、队列 dock、
   transcript 都不渲染——它们都是任务的产物。

   **不显示忙碌状态**（不要 tab 上的运行点）。tab 标题取 `Term` 的 `title()`
   （`vim` 自己会设，`alacritty_terminal` 免费给），为空时退到 shell 名。

   **tab 标题单行、13pt**（2026-11-10 修订）：shell 把标题设成当前目录，那是一条
   路径——放开换行后 tab 条会变成两三行的块，把内容区往下推。标题因此
   `whitespace_nowrap()` + `text_ellipsis()` + `max_w(180)`，行高 `sp(18)`，整条
   tab 走 `px_2 / py(3)`（高度 ~26px）。字号用 `sp(13)` 而不是终端的
   `COMPOSER_TEXT_SIZE`：它是内容区的**说明文字**，跟着窗口其它小字一起缩放，
   与 `vim` 画出来的字不该一样大。关闭按钮 `×` 与末尾的 `+` 定成 14/20px 的方
   形可点区，不再靠 padding 撑，鼠标靶子因此与视觉一致。

3. **状态只在内存，cwd 继承取实时值**

   不持久化：关窗口/重启即消失，不引入 daemon 或 tmux 式 attach。终端 I/O 不进
   会话事件日志，所以没有「恢复上次终端」这回事，UI 与文档都要说清。

   新建终端的 cwd 优先级：

   ```
   当前活跃终端的实时 cwd（若目录仍存在）
     > [terminal] start_cwd（若存在）
     > home
   ```

   关键词是**实时**：用户 `cd` 之后 shell 自己的 cwd 变了，新终端要继承这一处，不能
   在创建时缓存。取法：拿 shell 的 pid，再查它的 cwd（`sysinfo`；Linux 读
   `/proc/<pid>/cwd`，macOS `proc_pidinfo`，Windows 读 PEB）。

   三个已知坑，实现必须写明：

   - **失效目录必须跳过**。老终端的 cwd 被删掉后直接交给 `spawn_command`，shell 会
     根本起不来。
   - **符号链接会丢**。`/proc/<pid>/cwd` 给的是物理解析后的路径；从 `~/work-link`
     进去，新终端会开在 `/mnt/.../work`。可接受，文档里说明。
   - **只能拿到 shell 的 cwd，不是前台进程的**。`cd /tmp && python` 时 shell 的 cwd
     仍是 `/tmp`（正确）；前台进程自己 `chdir` 了则继承不到。够用。

   `alacritty_terminal 0.26` 的 `Term` **不暴露 cwd**（公开方法里有 `title` /
   `clipboard_*` / `set_hyperlink` / `mode`，没有 cwd），所以 OSC 7 追踪不可用，
   见「备选方案」。

4. **渲染：canvas 自绘 + damage 重绘**

   `gpui-pre` 的 `canvas()`（prepaint 拿 bounds，paint 里发 `Scene` 原语）配
   `WindowTextSystem` 度量：

   - 列数 = `width / advance('M')`，行数 = `height / line_height`；resize 时同时
     `Term::resize` 与 `TIOCSWINSZ`，并防抖（否则 `vim`/`htop` 反复重排）。
   - 每帧先画脏行背景 quad，再把「同样的 fg/bg/属性」的连续单元格拼成一个 run
     排版一次。**绝不逐字符 layout**。
   - 用 `Term` 的 `damage` + `TermDamageIterator` 只重绘脏行；没有输出时整帧跳过。
   - 字体默认取「等宽且 CJK 覆盖」的家族（Sarasa Term/Mono/Fixed 之类，1:2 度量，
     一个汉字正好两格）；系统没有就退回 `theme.mono_font_family`，`[terminal] font`
     可显式覆盖。JetBrains Mono 没有汉字，靠比例字体兜底会让整行偏离网格刻度
     ——实测过：`汉字A` 渲染成 `汉 字 A`，一半是 spacer 格被当成空格，一半是回退字体
     的 advance 不等于两格。

   - **实现注记（字体选择，2026-11-09 修订）**：现在的规则是**两个族**——Latin 用
     `theme.mono_font_family`（app 的代码字体，`[gui] code_font` 可配），CJK 用
     `theme.font_family`，也就是**Composer 里那段文字用的同一个族**。`[terminal] font`
     非空时两半都用它。

     原来不是这样，而 1:2 度量逼出来的：整行只用**一个**字体 shaping，于是每个字形
     都必须来自同一族同一字号。代码字体的 ASCII 是 0.6 em，汉字是 1 em，两格 1.2 em
     与一个汉字 1 em 差 0.2 em，一行汉字就会累积漂移。当时的解法是**整行**改用一个
     1:2 的 CJK 等宽字体（Sarasa 系列），代价是终端里的西文变成没人挑过的字形，和窗口
     其它地方（Composer、代码块）明显不同。

     改成**按段定位**后这个约束消失了：每段按 `advance × column` 落在自己的列上，宽度
     不再靠 shaping 累加，于是两段可以用不同的族、不同的字号。CJK 那一段的字号按测量
     得到——`advance('中')` 在 Latin 字号下的宽度为 `e`，则 CJK 字号 = `font_size ×
     2 × advance / e`，让一个汉字正好占两格。缺字或测量失败时退回 Latin 字号（列位置
     仍来自网格，所以不会漂移）。

     取舍：**失去跨段 shaping**，即跨越两段颜色的连字不再成形。实践里不存在（连字都在
     同一色段内），换来的是「同一行可以两种字体」。这与 terminal-ts 走 run 级字体的
     判断一致，只是它还要 Nerd Font 优先级那套，我们暂时不需要。

     **字号**同理：终端原来画在**原始缩放轴**上（开箱 14 px），Composer 画的是
     `15 × 轴 / 14`（开箱 15 px），两者从来不等，而且每一步缩放都不等——差值随缩放
     变大。现在两处都走同一个 `COMPOSER_TEXT_SIZE`（`lib.rs`）与
     `composer_text_size()`，终端不再自己算字号。设计尺寸集中在常量里，两处调用点
     没法再漂开；回归测试按三种缩放轴比对这个关系，并显式断言「不是原始轴」。

     另：`CJK_MONO_FONTS` 自动探测已删除——它存在的理由（1:2 度量）已经不需要了，而
     「装了 Sarasa 就整屏换成它」正是让用户觉得字体不如 Composer 的原因。想用 Sarasa
     的人可以显式写 `[terminal] font`。
   - `~/space/opentj/terminal-ts`（同一作者的另一份实现，Tauri + xterm.js）在这件事上
     走得更细，留作后续参照：它在 `src/lib/fonts.ts` 里用 DOM 测量 + 一个故意缺失的
     sentinel 家族来探测「这个字体到底装没装」（阈值 0.1px），因为 WebKit 上首族若未
     安装，canvas 会直接退到比例字体、根本轮不到链里后面的家族。它的顺序是
     「Nerd 连字字体 → 普通连字等宽 → 纯 Nerd Font → Menlo 兜底 → CJK 组（Sarasa
     领头）」，并注明两条经验：Sarasa 的汉字正好是西文 advance 的两倍；**Sarasa 必须
     排在 Nerd Font 之后**，否则 oh-my-posh 的 PUA 图标（U+E0B0–U+E0B6 等）会被它的
     CJK 映射抢走，渲染成汉字（例如 `瞵間`）。
   - 本实现**没有**走「一条 fallback 链」的路线：GPUI 的 `shape_line` 一次给一行、
     一个 `Font`，链里的第二族不会只在缺字时生效。要拿到 terminal-ts 那种「连字字体
     画西文、CJK 落到 Sarasa」的效果，得改成 run 级字体（`TextRun::font`）+ 按列定位
     每段——那会放弃整行 shaping 的连字与 kerning，留给真正需要时再做。

5. **输入编码：mode 感知，且有成文的快捷键归属**

   GPUI `KeyDown` → VT 序列是**全新的工作量**，也是 bug 密度最高的地方。
   `Term::mode()` 返回 `TermMode`，编码**按终端当前模式决定**：应用光标键
   （`\x1b[A` vs `\x1bOA`）、bracketed paste（`\x1b[200~`…`\x1b[201~`）、
   鼠标上报、`modifyOtherKeys`，都不自己猜。

   | 终端聚焦时 | 行为 |
   |---|---|
   | 除下列全部按键（含 `Ctrl+C`/`Ctrl+Z`/`Tab`/`Esc`/功能键） | 转发给 PTY |
   | `Ctrl+Shift+T` / `Ctrl+Shift+W` / `Ctrl+Tab` | 新建 / 关闭 / 切换 tab |
   | `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | 字号缩放（与全窗口一致） |
   | `Cmd+C` / `Ctrl+Shift+C` | 复制选中文本 |
   | `Cmd+V` / `Cmd+Shift+V` / `Ctrl+Shift+V` | 粘贴 |
   | 侧边栏点击 | 切视图 |

   **`Ctrl+V` 不在上表，是故意的**：readline 把它绑成 quote-next，抢走就废了它。
   所以非 macOS 平台的粘贴只能落在 `Ctrl+Shift+V` 上。

   粘贴这条有个容易踩空的机制：**绑定先于 `on_key_down` 分发**。终端的粘贴本来写在
   key-down 监听里（`is_paste`），而 key-down 只在没有任何绑定认领这个按键时才触发；
   窗口又恰好把 `Ctrl+Shift+V` / `Cmd+Shift+V` 绑给了「粘贴图片」（`PasteImage`）。
   结果是终端那条路径永远轮不到，非 macOS 上等于**没有粘贴键**（这个问题在 0062
   落地后一直存在，靠 `tests/shortcuts.rs` 里「图片粘贴的每个按键都必须在 Terminal
   上下文重新绑定」的断言守住）。**凡是窗口自己绑掉的按键，终端若想要，就必须在
   `Terminal` 上下文深度上重新绑一次**——上下文更深的绑定优先。

   中文输入：组合器已经有 `vendor/xim-ctext` 补丁（ADR-0051）修正 XIM 预编辑，
   终端复用同一条输入路径；新增的是**预编辑文本在字符网格里怎么显示**（格子里画
   preedit，不进 `Term`）。

6. **生命周期与关闭**

   - 关 tab / 退出窗口 → 杀**整个进程组**：`SIGHUP` 宽限后再 `SIGKILL`。

     **必须打整组，不能只打死接子进程**。不做也「看起来正常」：shell 被杀，它
     fork 出去的东西被 init 收养，用户看不见，只会在下次 `ps` 时发现一堆 `sleep`。
     已实测：`Pty::drop` 只对直接子进程发 `SIGHUP`，所以这一步得自己补；发完
     `SIGKILL` 还要**等到组真的空了**才从 `close()` 返回，否则「关闭」只等于「信号
     发出去了」，紧接着检查的人会看到一个还没被回收的残留。

     用 `SIGHUP` 而不是 `SIGTERM`：这是终端挂断的语义，也是 xterm / alacritty 关闭
     终端时发的信号；shell 捕获它来写 history，`SIGTERM` 触发不了那段。此处改掉
     ADR 早先写的 `SIGTERM`。

     实现在 `qcode-terminal`（`libc::killpg`），**不复用**
     `qcode-tools/src/shell.rs` 的 `process_group(0)`：那里的 `Command` 是我们自己
     建的，要显式建组；终端的子进程由 `tty::new` 起，内部已 `setsid()`，组 id 等于
     pid，没有可设的组。

     顺序有讲究：**先打组、后 `join`**。子进程一旦被 `wait` 掉，pid 就可回收，此时
     `killpg` 可能打到无关的组。实测对 `trap '' HUP` 的 shell，宽限 100 ms 后
     `SIGKILL` 兜底，整体关闭仍在 2 s 内。

     所以子进程**已经自然退出**时反过来不能再打组（pid 可能已回收）。这不漏杀：
     子进程是会话首进程，它退出时内核对控制终端的前台进程组发 `SIGHUP`，后台作业
     自己会死。这条假设有测试兜着
     （`a_background_job_dies_when_the_shell_exits_on_its_own`），不是空口断言。
   - kill 不能在 UI 主线程做。
   - 关闭前的确认：**一次性**查前台子进程（不是周期轮询），有进程在跑时弹确认。
     这与「不显示忙碌状态」不冲突——不做每帧/周期探测，只在关闭那一刻查一次。
   - 读线程自己报 `ChildExit`，但 ConPTY 在主 handle 未关时可能不送 read-EOF，
     Windows 上要实测「子进程先退出 → 释放 master → 读到 EOF」的顺序。
   - 窗口「关闭即最小化到托盘」（ADR-0052）时终端**继续跑**，与任务在后台跑一致。

7. **不沿用 `EventLoop`：它会丢数据（实测，已改自持读线程）**

   `alacritty_terminal` 的 `EventLoop` 看起来能直接用，实测后**不能**。两个接线陷阱
   之外，还要加上一条会丢用户数据的设计缺陷——它们共同决定了「读循环自己写」。

   | 陷阱 | 表现 | 正确做法 |
   |---|---|---|
   | `Msg::Resize` 只调 `pty.on_resize` | 子进程收到 `SIGWINCH`，网格没变 | 自己再 `Term::resize` |
   | `Options::drain_on_exit` 无效 | 写完即退出的命令，输出全丢 | 靠读线程收尾，不靠它 |
   | 拿不到锁就回去重读 | 渲染每帧抢锁时，最后一段输出被丢弃 | 读线程**阻塞**等锁 |

   第一条用 `stty size` 验过：子进程报 `30 100`，`Term` 仍是 `80×24`——两半互不
   蕴含。alacritty 的窗口层自己 resize `Term`，事件循环只负责 ioctl。

   第三条是**数据丢失**，也是这一版重写读循环的直接原因。`EventLoop::pty_read` 的
   形状是：

   ```
   loop {
       read(pty)                        // 拿到 "alpha\nbravo\n"
       match term.try_lock_unfair() {   // 渲染线程每帧持锁
           None => continue,            //   → 不解析，回去再读
       }
       parser.advance(已读字节)          // 只有拿到锁才走这里
   }
   ```

   子进程随后退出时，下一次 `read` 在 Linux 上返回 `EIO`，函数**带着未解析的缓冲
   直接返回**；之后 `drain_on_exit` 再读时管道已空，于是 tab 是白的。这不是理论风
   险：20 并发、每 2ms 读一次网格，**200 个会话里 166 个丢了输出**；完全不碰锁则
   0/200。用户看到的是「跑完 `make` 切回窗口，最后几行没了」。

   改法：读线程**自己持有 `Term`**，用阻塞 `lock()` 而不是 `try_lock` + 重试——争用
   只会延迟解析，绝不丢弃已读字节。配套：

   - 写走独立线程和独立 fd（Unix 上 `dup`），打字不必等解析，程序不读输入也不会
     卡住帧回调；
   - 子进程退出仍用 `tty::new` 已建好的、level-triggered 的 `SIGCHLD` 管道，
     不用自己装信号；
   - `TIOCSWINSZ` 在写线程的 fd 上做（同一个终端），`EIO`/`Ok(0)` 都当作正常收尾：
     先把尾巴 `pump` 干净，再报 `ChildExit`；
   - `close()` 在 `SIGCHLD` 被收割**之前**对进程组发 `SIGHUP`，等 100ms 再
     `SIGKILL`，并等到组真的空掉才返回——否则 `close()` 只意味着「信号发出去了」，
     而不是「进程树没了」。pid 可能被回收，所以子进程已退出时不再 `killpg`。

   读网格仍用**闭包**而不是返回锁 guard：`FairMutex` 的锁会阻塞读线程，返回 guard 很
   容易被跨帧持有，等于让子进程的输出等一整帧。闭包拿不走，借用自然在结束时结束。

   自己写读循环后，又有两个只有实测才会暴露的坑（都已修，都有回归测试）：

   - **master fd 是 `O_NONBLOCK`**（`tty::new` 设的），所以往「暂时不读输入」的程序写
     会返回 `WouldBlock` 而不是阻塞。把它当致命错误——第一版就是这么写的——比卡住更
     糟：写线程退出、channel 接收端析构，此后每个 `Session::write` 都返回 `false`。
     实测：`sh -c 'sleep 2; cat > f'` 只收进 16 KiB 就拒写，之后敲的字**永远到不了**
     子进程（用户感受是「粘了一大段之后键盘突然全废」）。现在 `WouldBlock` 只表示
     「等」，字节留在写线程里重试，队列在粘帖期间增长，`Session::write` 对调用方仍是
     非阻塞；只有真错误才结束线程。
   - **一次 `pump` 必须有预算**。`yes` 让 pty 永远非空，若 `pump` 循环到「读不到数据
     为止」，它就不返回：退出事件看不到、`stopped` 看不到（刷屏的 tab 关不掉），而且
     `Wakeup` 若只在整批结束时发一次，就一个都不会发——网格在更新，却没人被通知重绘，
     表现是「刷屏时终端一片空白」。现在按 64 次读为一批返回外层，并且**每次读到数据就
     发 `Wakeup`**。

8. **边界：不沙箱、不审批、不进日志**

   - 终端是**用户自己敲的 shell**，不走 `[sandbox]`、不走审批。否则没法用。
   - 必须显式和 agent 的 `run_shell` 区分：设置页与文档写一句「沙箱只约束 agent
     发起的命令，终端不受约束」。不写清会让人以为沙箱开着终端也受限。
   - 终端 I/O **不进会话事件日志**（量太大，且含用户隐私）。代价是终端里的操作
     没有审计留痕，这与 agent 工具调用可回放不同。
   - **副作用**：终端里改文件不产生 snapshot（且 `[agent] snapshot_shell` 已默认
     关闭），所以 `qc sessions rollback` **无法回滚终端里的改动**。文档必须写明。

9. **必须回答终端的「反向查询」**

   `alacritty_terminal` 把三类查询交给 `EventListener`，**不回答就等于终端哑了**：

   | 事件 | 谁在用 | 不回答的后果 |
   |---|---|---|
   | `ColorRequest`（`OSC 10/11/12 …?`） | vim 开机就问前景/背景色 | 超时后按猜的配色画 |
   | `PtyWrite`（`DSR 6n`、`DA`） | vim、tmux、任何用 DSR 定位的程序 | 卡在等响应 |
   | `ClipboardLoad` / `Store`（`OSC 52`） | 复制粘贴 | 剪贴板走不到系统 |

   录制下来的 vim 字节流里这几个查询都在（`\x1b]10;?\x07`、`\x1b]11;?\x07`，以及
   `\x1b[6n`、`\x1b[c`），所以这不是理论问题。

   接线要求：`ColorRequest` 的回复要用**主题的**前景/背景色（索引 256/257，`OSC 4`
   的调色板查询按索引取），格式化函数由 `alacritty_terminal` 提供；`PtyWrite` 的字节
   原样写回 PTY。core crate 不持有主题，所以它只把请求记录下来并提供
   `answer_colours()`，由 GUI 侧传入主题色。

   `OSC 52` 同理：crate 记录 `ClipboardStore` 的文本与 `ClipboardLoad` 的格式化闭包，
   提供 `take_clipboard_stores()` / `answer_clipboard(Option<&str>)`，GUI 的 ticker 用
   真剪贴板回答。**`Osc52` 从 alacritty 的 `OnlyCopy` 改成 `CopyPaste`**：只开拷贝时
   `tmux`/`ssh` 用不了系统剪贴板，而这正是 OSC 52 存在的理由；代价是 pty 上的程序
   能把剪贴板读走，本 ADR 认为这是用户选择内建终端的合理代价，记在这里供以后收窄
   （比如只对可信会话开、或首次询问）。

10. **配置与资源上限**

   新增 `[terminal]` 段。注意配置有**两层键白名单**（`config/load.rs` 顶层键 +
   段内键），不加会在启动时告警：

   ```toml
   [terminal]
   shell = ""              # 空 = 探测
   start_cwd = ""          # 空 = home
   max_scrollback = 10000  # 每终端行数
   max_terminals = 8       # 硬上限
   ```

   **shell 探测：优先「当前正在用的 shell」，不是 `$SHELL`**。

   `$SHELL` 是**登录 shell**，只是一个继承来的环境变量，并不反映你现在在哪。实测：
   登录 shell 是 zsh、在 bash 里运行程序，`$SHELL` 仍报 `/usr/bin/zsh`。所以照
   terminal-ts 那样直接用 `$SHELL`，用户在 bash 里敲 `qc gui` 会得到一个 zsh 终端。

   非 Windows 的顺序：

   ```
   [terminal] shell（在 PATH 上） > 父进程链里的交互式 shell > 用户数据库的登录 shell
     > $SHELL > /bin/sh
   ```

   **登录 shell 排在 `$SHELL` 前面**，与 wezterm/portable-pty 的顺序相反，这是刻意
   的。GUI 应用没有父进程链可查时，`$SHELL` 是**启动它的那个东西**留下的：launchd、
   服务管理器、安装脚本。它们给的值经常不是用户现在用的 shell——macOS 上实测
   `$SHELL=/bin/bash` 而账户早就是 zsh，于是终端一开就是 bash，还附带一句
   "The default interactive shell is now zsh"。用户数据库是 `chsh` 维护的那份，比继承
   来的变量可信；`$SHELL` 只在用户库里没有答案时（容器、精简镜像）兜底。

   登录 shell 用 `getpwuid_r` 取，读 `/etc/passwd` 只作为兜底：macOS 的账户在
   Open Directory 里，文件里根本没有这一行——照文件读会在最该正确的机器上返回
   "不知道"。

   子进程的 `SHELL` 变量会被改写成实际启动的那个 shell，否则终端里的嵌套 shell 和
   `$SHELL`-aware 脚本会继续读到一个没人运行的答案。

   父进程链：从自身 PID 向上走（上限 12 层），取第一个 basename 落在白名单里的
   进程的 `exe` 路径。白名单是 `bash` / `zsh` / `fish` / `nu` / `elvish` / `xonsh` /
   `tcsh` / `csh` / `ksh` / `mksh` / `oksh` / `yash` / `pwsh`。

   **`sh` 与 `dash` 刻意不在白名单里**。它们是脚本解释器，`sh -c …`、`#!/bin/sh`
   的启动器都会出现在父链上——实测我的工具链就是 `python3 → sh → qc`，照「最近一个
   shell」取会拿到 dash。跳过它们不会漏：真的拿 dash 当登录 shell 的人，`$SHELL`
   会兜住同一个值。实测两种场景（正常启动、双层包装壳）都正确解析到
   `/usr/bin/zsh`。

   平台实现：Linux 读 `/proc/<pid>/comm`、`/proc/<pid>/exe`、`status` 的 `PPid`；
   Windows 没有等价的 shell 链，维持探测：

   macOS 改为**一次 `ps -A -o pid=,ppid=,comm=` 取表** + `libc::proc_pidpath` 取精确
   路径。原计划用 `sysctl(KERN_PROC_PID)`，但 `libc` 在 Apple 平台上**没有导出
   `kinfo_proc`**；自己声明那个约 640 字节的结构体、在编译和运行都做不到的环境里赌
   字段偏移，读错就是静默的垃圾值。`ps` 的输出格式稳定，解析逻辑是纯函数，所以在
   Linux 的 CI 里也能拿真实输出去测它；一次调用的代价，在一次终端创建面前可以忽略。

   已知边界：非交互 shell 执行单条 `-c` 命令时可能 `exec` 掉它，从而**不出现**在父链
   里（实测 `bash -c qc` 的链条直接从 `qc` 开始）。交互 shell 一定等子进程，所以真实
   场景（用户在自己 shell 里敲 `qc gui`）不受影响；脚本场景下链条答不出来，退回
   `$SHELL`，两者本来也无从区分。

   已端到端实测：`$SHELL=/usr/bin/zsh`，从 bash 启动时探测结果为 `/usr/bin/bash`。

   **Windows 那条分支在这里编译不到**（开发机上没有 rustup、没有 Windows 目标），只做
   了代码审查。它的逻辑是独立的十几行（按 `pwsh` → `powershell` → `cmd` 依次在 `PATH`
   上找，找不到落 `COMSPEC`），与 Unix 路径不共享代码，所以一旦有问题也是浅层的编译
   错误而非行为错误。合并前需要在能编译 Windows 目标的环境里过一次 `cargo check`。

   | 平台 | 顺序 |
   |---|---|
   | Windows | `pwsh` → `powershell` → `cmd` |
   | 非 Windows | 配置 → 父链 → 登录 shell → `$SHELL` → `/bin/sh` |

   每一层都对失效值宽容：路径不存在、不可执行、读不到父进程，一律跳到下一层。
   用户配了 `shell` 却不在 PATH 上时落回探测结果——不能让一个手误就开不了终端。

   探测器要能注入（一个取 `(pid, comm, exe)` 链的 trait），否则这段逻辑只能靠真进程
   树测，覆盖不到「包装壳 / 嵌套 shell / 无 shell」这些分支。

   **后续（2026-11-10）**：这套探测被抽成 `crates/qcode-shell` 供 `run_shell` 共用，见
   ADR-0064。抽取之后这里只剩 `pub use`，顺序与理由不变；`[terminal] shell` 同时是工具
   侧解释器的覆盖，避免「终端是 zsh、agent 的命令跑在 sh 里」。

   上限不是形式主义：GUI 空闲已 293MB RSS，每个终端 scrollback 约 2–4MB。开 8 个
   +15MB 可接受；不限的话 20 个终端就是 +400MB。

**备选方案（未采用）**

- **WebView 里跑 xterm.js**（即 `qiuzhanghua/terminal-ts` 的做法）：qcode 用 GPUI，
  没有 WebView；引入等于多一套运行时（Linux 上是 WebKitGTK），桌面包会显著变大。
  terminal-ts 的 VTE 与渲染能力**全部**在 xterm.js 里，是 JS，搬不过来。
- **`portable-pty`**（terminal-ts 用的 `0.8`）：`alacritty_terminal` 的 `tty` 模块与
  `Term` 同源，尺寸/信号/ConPTY 处理一致，少一处语义鸿沟；Alacritty 本体在 Windows
  上有大量实测。代价是放弃 terminal-ts 已跑通的 Windows 环境细节——但那些照样要
  移植（见「后果」）。
- **侧边栏每个终端一行**（与任务同一列表）：需要把 `active` / `hovered` / `menu` /
  `schedule_hints` 等按 task id 索引的状态全部升一层，改动面大；且需求已确认为
  「单入口 + tab」。
- **持久化进程**（daemon + attach，tmux 模式）：无需求，复杂度高一个量级。
- **`vte` + 手写状态机**：`alacritty_terminal` 是 Alacritty 本体在用的，`vim` /
  `htop` / `tmux` 的边角（备用屏幕、滚动区、鼠标上报、wide char）被反复验证过。
- **OSC 7 追踪 cwd**：更可靠（能跟到子 shell、`sudo -i`），但
  `alacritty_terminal 0.26` 不暴露 cwd，且需要注入 shell 集成脚本。留作后续增强。

**后果**

- `Cargo.lock` 多出 9 个包：`alacritty_terminal 0.26.0`（**Apache-2.0，
  `deny.toml` 已 allow，不用改**）、`vte 0.15`、`rustix-openpty 0.2`、`miow 0.6`、
  `home 0.5`、`cursor-icon 1.0`、`signal-hook 0.4`（与既有 0.3 并存）、
  `windows-sys 0.59`（与既有 0.48/0.52/0.60/0.61 并存，Windows 专用），以及
  `qcode-terminal` 本身。`rustix` 从 1 个版本变 2 个（`0.38.44` + `1.1.5`）。
  这些全是纯 Rust，**没有 fontconfig / X11 / GPU 相关**，不会重演 10-19 那次 CI
  因缺 `-dev` 包红了十天的事故——已实际 `cargo check -p qcode-terminal` 验证通过。
- 必须**钉死精确版本**（`=0.26.0`），理由与现有 `rmcp = "=3.2.0"`、
  `ratatui = "=0.29.0"` 相同：它 minor 之间会破（0.24 → 0.26 已改过 API）。
- `sysinfo` 已是间接依赖（`0.31.4`，由 `zed-scap` 在 Linux 屏幕捕获路径引入），但
  Windows / macOS 构建里未必存在，**必须显式声明**，不能靠传递依赖。
- 终端只进 `gui` 特性：`qcode-terminal` 由 `qcode-gui` 依赖，TUI / `qc run` / ACP
  构建不含它，精简版发布包不因此变大。
- Windows 与 macOS 的 PTY 路径首版**未实测**，标为已知风险；读线程的
  `ChildExit` / EOF 行为在 ConPTY 下要单独验（Windows 分支按
  `EventedReadWrite` 走互斥的 `Pty`，不能被 `dup` 拆分）。
- 终端 IME 是 `xim-ctext` 那条老 bug 的新暴露面（ADR-0051 修的是组合器，终端是
  第二处）。
- 侧边栏新增一个按钮，`Shell` 新增 `view` 与 tab 状态，`render_main` 外面加一层视图
  分支。任务侧的状态一个都不改。

**从 terminal-ts 移植的部分（MIT，已确认可复用）**

不是搬 VTE / 渲染，而是搬这几处踩过坑的细节：

- `spawn_shell` 的环境处理：`TERM=xterm-256color`、`COLORTERM=truecolor`；**以及
  `portable-pty` 在 Windows 上从注册表重建环境、会丢掉会话注入的 `PATH`，必须把父
  进程 `PATH` 重新灌回去**——qcode 用 `cot` 注入 PATH，这个坑一模一样。
- `pwsh` 的 `$PSStyle.OutputRendering='Ansi'` 特判（否则 ConPTY 下 powershell 首屏
  无颜色），以及 `-NoLogo -NoProfileLoadTime`。
- `resolve_start_cwd` 的「跳过不存在的目录」。
- attach 缓冲语义：listener 注册前的输出先攒着，且**必须在同一把锁内**决定「立即发
  还是缓冲」，否则缓冲前缀会被实时输出超车。后台新建终端时会用到（本 ADR 不做后台
  新建，但事件通道的订阅时序同样要保证不丢首个提示符）。

`kill_tree`（sysinfo 进程树倒序 kill）不移植——进程组更干净。

**验证（拟）**

1. **状态机 headless**：录 `vim` / `htop` / `tmux` / `less` 的原始字节流，喂给
   `Screen`，断言网格文本、光标位置、`TermMode` 位。不碰 UI。
2. **调度隔离（最关键）**：一个终端持续 `yes` 刷屏，同时放一个 orphan 计划待跑，
   断言调度仍在一个 beat 内落 `Orphaned`——直接复验 ADR-0059 的 `away` 判定没被
   拖慢，接上 11-06 加固的
   `a_schedule_pointing_at_a_deleted_task_is_recorded_not_run`。

   已实现为 `a_flooding_terminal_does_not_starve_the_scheduler_beat`：orphan 在
   1.07 s 落定（beat 是 1 s），即没有拖慢。断言的有效性用变异测试验过——在
   `runtime.rs` 的 beat 分支里插 4 s 忙等，该测试立刻失败。没有这一步，这类测试
   在「本来就不可能失败」时会永远绿。
3. **cwd 继承**：A 里 `cd /tmp` → 新终端起在 `/tmp`；把 `/tmp` 换成已删除的目录 →
   落到 `start_cwd` / home，且 shell 能起来。
4. **shell 探测**（注入假进程链，不依赖真进程树）：父链是 `bash` → 取 bash；
   父链是 `sh → zsh`（包装壳）→ 取 zsh 不是 sh；父链无 shell → 落 `$SHELL`；
   `$SHELL` 指向不存在的路径 → 落 `/bin/sh`；配置的 shell 不在 PATH → 落回探测。

   已实现 23 个用例。除上面几条，还覆盖了：父链成环、链条超长、父进程读不到、
   `(deleted)` 后缀、`/etc/passwd` 按 uid 匹配、服务账号（`nologin`）不算 shell、
   `ps` 表解析（含带空格的 `comm`、畸形行）。另有两条打真实系统的：探测结果必须是
   可执行的绝对路径；`ps` 解析结果与 `/proc` 对同一进程的 ppid 必须一致——后者让
   macOS 那条唯一实现路径也在 Linux CI 里被真实数据跑过。
5. **按键编码**表驱动：`Ctrl+C`、方向键（两种光标键模式）、`F1`–`F12`、`Alt+x`、
   bracketed paste、`Shift+Tab`。

   **尚未实现**：`modifyOtherKeys`（`CSI > 4 ; m`，vim 打开它以便没有传统编码的组合键
   也送得出去）与 kitty 键盘协议。两者缺席都不会让键失效——退回 `ESC` 前缀是 xterm 关掉
   它们时的行为，程序都认——代价是没有传统编码的组合键仍是有歧义的，且 `Ctrl+I` 与
   `Tab` 同为 `HT`（这一点 xterm 也一样）。

   已实现 28 个用例，其中 4 条**直接和本机 terminfo 对表**（`infocmp xterm-256color`
   的 `kcuu1` / `kcbt` / `kf1`–`kf12` / `kLFT` 等）：程序查 terminfo 来认按键，所以与
   它一致才算对。这 4 条用变异测试验过——把 `F5` 的数字或 Shift 的修饰参数改错，它们
   立刻失败。只和自己对表的编码器测试，写错了也照样绿。
6. **生命周期**：关 tab 后进程组全部消失（无孤儿）；托盘最小化后终端仍在跑。

   关 tab 不弹确认：它等价于关掉一个终端窗口——`SIGHUP` 先到，程序自己决定怎么收尾
   （`vim` 会写 swap 并退出），不听话的才升到 `SIGKILL`。「里面还有前台任务时先问一句」
   留给待定，因为「前台任务」在 pty 语境里没有可靠的判据（进程组里有几个成员不等于
   有交互任务）。

   已实现 10 个用例。其中 `closing_escalates_past_a_child_that_ignores_hangup` 覆盖
   `SIGHUP` → `SIGKILL` 的升级；三个涉及进程标记的用例用「本进程 pid + 用例编号」生成
   唯一的 `sleep` 参数——固定标记会被上一次崩溃运行留下的孤儿永久污染，而同一进程内
   多个用例共用一个 pid 会让 A 用例的断言盯上 B 用例的子进程（两者都实际发生过）。
7. **不丢输出 / 不卡死**：子进程写完即退出、同时有渲染线程每帧抢网格锁时，末尾输出
   必须完整；输入缓冲满时只能变慢，不能变哑；刷屏时必须持续发 `Wakeup` 且仍能关闭。

   已实现为 `tests/no_lost_output.rs`：20 并发 × 5 轮，读者每 2 ms 抢一次锁。修复前
   这一条稳定失败（更狠的压测是 200 个会话里 166 个丢输出），修复后 0/200，且把
   「输出 60 KiB 后立刻退出」的版本也验了尾巴。

   输入侧是 `tests/input_backpressure.rs`：往「延迟 1 s 才开始读」的 `cat` 灌 1 MiB，
   然后断言 `write()` 仍在接受、粘帖后的按键仍能到达、`close()` 不卡。三条在旧行为下
   都会失败（`write()` 在 16 KiB 处被拒、marker 丢失、粘帖只剩 11 658 字节）。刷屏侧
   是 `session.rs` 里的两条：`yes` 期间 `wakeups() > 10` 且网格有内容，关掉后进程组
   为空。后者用变异测试验过——去掉预算并恢复「整批发 `Wakeup`」，前者立刻报
   `0 wakeups`。
8. **资源**：8 个终端各跑 `yes`，断言帧时间与 RSS 在预算内。

   已实现为 `tests/resource_budget.rs`：两条确定性用例随常规测试跑（滚动上限确实是
   `scrollback + screen_lines`，且封顶后视图仍能回到实时画面），一条 `#[ignore]` 基线
   由 `scripts/perf-session.sh` 跑。实测 8 个刷屏 tab 共 156 MiB、快照均值 284 µs；
   断言的是**形状**而非字节数——封顶后两秒再刷增长必须为 0，footprint 不得超出
   `terminals × scrollback × columns × size_of::<Cell>()` 的两倍。绝对值随机器与窗口
   宽度变化，拿死数字当预算只会在宽窗口上误报、或放过真正的泄漏。

9. **手动回归清单**（自动化够不着的交互，每次动到终端渲染或输入都要过一遍）：

   | 做什么 | 应该看到 |
   | --- | --- |
   | 侧栏「终端」/ `Cmd+T` | 切到终端视图并出现一个 tab，提示符一两帧内出来 |
   | `Cmd+T`（已在终端视图） | 回到任务视图，shell 照旧在后台跑 |
   | 在没发出去的草稿上点别的任务行 | 状态栏提示「已暂存」，按 `+` 装回刚才打的字 |
   | 打字、`Ctrl+C`、方向键 | 回显、打断、命令历史都正常 |
   | 拉窗口边角 | 文字随宽度重排，`resize` 后程序收到新尺寸（`vim` 的行列号会变） |
   | `seq 1 500` 后滚轮上滑 | 顶行回退，光标消失；一打字回到现场 |
   | 拖选一段、`Cmd+C`、再粘贴 | 高亮跟着鼠标、剪贴板是该段文字、粘贴落在光标处 |
   | 中文输入法打一句话 | 提交的文字进 pty；候选框浮层尚未实现（见待定） |
   | 粘贴多行到不开 bracketed paste 的程序 | 弹「粘贴 N 行？」，取消则什么都没发生 |
   | `printf '\033]52;c;aGk=\007'` 后 `pbpaste` | `hi` |
   | `printf '\033[31mred\033[0m'` | 红色可见（深色画布） |
   | 开两个 tab，`Ctrl+Tab` / `Ctrl+Shift+Tab` | 高亮与内容跟着循环 |
   | `Cmd+W` 关掉最后一个 tab | 回到任务视图，`ps` 里没有残留的 shell |
   | 在后台 tab 启动 `vim` | 切回去时已经画好，不是空白等一帧 |

**实现注记：终端的右下留白（2026-11-09 修订）**

用户反馈「右边和下边看上去很古怪」。查下来不是缺边框，是**网格余量全堆在了右下**：
网格按整格取整，`floor(w / advance)` 与 `floor(h / line_height)` 的零头全留在右和
下。默认字号下 `line_height = 15 × 1.4 = 21px`，所以下边最多多出 21px、右边最多
9px，而且**改窗口大小时按 21px 一格跳**。内容贴左上、空隙甩右下，读起来就是「没画完」。

修法不是加边框盖住它（边框外面那块还是空的），而是**把余量对半分到两侧**：`grid_layout`
返回列数、行数和一个 `grid_origin`。上下留白因此对称（奇数余量差 1px，无法对半）。

关键约束：`grid_origin` 必须同时喂给三处，否则会出现「显示位置和点击位置不一致」——
这比留白难看严重得多：

- `paint_frame` 的所有定位（文字、选择高亮、光标、preedit）
- `terminal_point`（点击 → 格子，`position - origin`）
- `bounds_for_range`（输入法候选框锚点）

三个都从 `GridGeometry.origin` 派生，所以只在 prepaint 里把中心点算对、存一次即可；
`a_click_maps_back_to_the_cell_under_it` 用同一个 origin 反推，防止将来只改绘制不改命中。

窗口极小时 `max(20)`/`max(4)` 会让网格比视口大，此时余量为负——偏移钳到 0，否则
cell 0 会被画到元素外，点那里映射不到任何格子。

**面板边框**：body 加左/右/下三边框 + 下圆角（8px），读作「窗口里的一个面板」而不是
「一块颜色漏到窗口边缘」。**不加第四条边**：tab 条已有 `border_b_1`，再加一条会在几
像素内出现两条平行细线，看着像画错了。同理上圆角不做——没有上边可以拐。

边框色用 `rgba(0x7f7f7f20)` 而不是 `theme.border`：这条线是两个深色面之间的**分隔**，
不是控件轮廓，应该只是一点边缘暗示。`theme.border` 是给控件用的，在这里显得像画了个框。
同一个值已经是项目里的既有惯例（transcript、turn rail、设置页分隔线都用它）。

**实现注记：侧栏布局（2026-11-09 修订）**

原来整栏是一个滚动区，三个入口（设置、终端、我的任务）都在滚动内容里。任务一多，
「终端」和「设置」就被滚出可视区——和「入口常驻」的意图相反。现在分三段：

- **顶部固定**：`我的任务` + `+`
- **中间滚动**：任务列表（唯一可滚的区，`flex_1` + `min_h(0)`，否则 flex 项不肯
  收缩，底部被顶出屏幕）
- **底部固定**：`终端`（`SquareTerminal` 图标）与 `设置`，上方一条 hairline 隔开——
  上面是内容，下面是工具

两个细节是刻意的：

- **设置带 `ExternalLink` 角标**。终端是切主视图，设置是开独立窗口；并排放在底部会
  被读成一类东西，角标是唯一不靠文字就能说清差别的地方。
- **`回到任务` 已删除**（2026-11-10 修订，见本节末那条注记）。它当初被留着是因为
  侧栏可以 `Cmd+B` 收起，收起后任务行和「终端」入口都不可见，而 `Cmd+T` 只有进没有出。
  现在 `Cmd+T` 是双向开关，而它想保护的那件事（切走时把草稿丢掉）在 `park_and_clear`
  里真正修掉了。

**顺带修掉：设置关闭会丢最后一个改动（真实缺陷）**

设置是自动保存（输入后 250ms 防抖写入）。关闭路径原本是：

```rust
// 定时器醒来时
if view.closing { return; }        // 直接返回，不 flush
// 关闭窗口时
view.closing = true;               // 没有 flush pending_auto_save
```

于是**在设置里改完一个值、250ms 内关窗口，这次修改静默丢弃**——点右上角 X 和关窗口
都一样，而这屏上写着「自动保存 · 修改即时写入」。

修法是**无条件先 flush 再关**，而不是问用户「要保存吗」：自动保存语义下不存在「未保存
的改动」，只存在「还没触发的定时器」，问用户等于让他回答程序自己能回答的问题。真正值得
打断的只有**写盘失败**（磁盘满、配置只读），那时窗口留着、原因显示在设置页顶部；再按
一次关闭就放行，坏掉的配置文件不该把人困在设置窗口里。

三个开窗口的地方（向导、`qcode settings`、GUI 的设置）原本各写一遍关闭流程，抽成
`wire_close_handler` 一处；否则加 flush 时漏掉一个就是又一条丢数据的路。

`Esc` 关闭设置：绑在 `Settings` key context 上（只在这个窗口生效），走和 X 完全相同的
路径（含 flush）。输入框里的 `Esc` 会先取消自身状态再 `propagate`，所以焦点在输入框里
也有效；唯一的糙边是**中文输入法组字中按 Esc：候选取消和关窗会同时发生**——多数平台
的输入法在候选框层面就吃掉 Esc，但这一点没法在这里验证。

**待定**

1. **渲染与任务的帧预算分配**：终端刷屏（`yes`）时每帧都要重绘，可能拖慢同帧的
   transcript 打字机效果。倾向「终端不聚焦时降到 100ms 一次」，具体阈值等实测。
2. **`max_terminals` 与 scrollback 的默认值**：8 / 10000 是估的，现在有实测了——
   数值本身仍待定，因为这是产品权衡而非技术约束。

   实测（`crates/qcode-terminal/tests/resource_budget.rs`，80 列、`Cell` 24 字节）：
   一个填满的 10000 行 scrollback 约 **18.3 MiB**（10000 × 80 × 24 B），8 个刷屏
   tab 共 **156 MiB**，与理论下限 150 MiB 只差 4%（解析器、pty 缓冲、线程栈）。
   关键是**已封顶**：再刷两秒，footprint 增长 0 KiB，所以「内存会不会涨」有确定答案
   ——不会，只要 scrollback 上限不变。

   需要注意的两点：

   - 这个数字**随窗口宽度线性增长**。80 列是测试里的保守值；真实窗口 200 列时单个
     tab 就是 45 MiB，8 个 366 MiB。若 `max_terminals = 8` 要保留，scrollback 或许
     该按列数反比调（或默认降到 2000–5000 行，用户可配）。没有实测数据前不擅自改
     默认值。
   - 帧成本不是瓶颈：一次快照（24 行 `contents()`）均值 **284 µs**、最差 **340 µs**，
     对 16 ms 的帧预算只占 2%。8 个 tab 同时刷屏也不是「每帧都扫 8 个」——只画当前
     tab。

   补第 8 条时又发现一个偶发竞态（约 1/11 概率，`session` 里那条 wakeup 用例暴露）：
   `on_wakeup` 在 `spawn` 之后才安装，若子进程的首个输出先到，这条 `Wakeup` 就永远丢了
   ——**没有任何重绘被安排**，tab 会一直空着直到别的事件顺带刷新它。GUI 走的就是这个
   顺序，所以这是真实缺陷而不是测试问题。修法是 `Events` 记下「有通知在无回调时到达」，
   `on_wakeup` 安装后立刻补发一次；重放后清标记，所以第二次安装不会重复触发。回归用例
   不靠概率：子进程写完就阻塞在 `read` 上，保证通知一定先于安装发生。

   顺带保留一个可复现的回归：`history_is_capped_so_a_flooding_child_cannot_grow_memory_forever`
   是**非 ignored** 的确定性断言（滚到最底也只能到 `scrollback + screen_lines`），
   因为它验的是不变量而不是机器性能。把 scrollback 上限放大 1000 倍做变异，ignored
   基线立刻报「再刷两秒涨了 3 GB，所以有泄漏」。
3. **Windows ConPTY 的 EOF 时序**：需要在真机上验；不通过则要加「子进程退出 → 释放
   master」的显式步骤（terminal-ts 用两个线程解决）。
4. **探测到的 shell 要不要当 login shell 起**：`alacritty` 在 Linux 上裸起（不加
   `-l`），在 macOS 上则用 `/usr/bin/login` 包一层以注册 tty 会话。一旦我们显式传
   shell，这两个默认行为都不再适用，等于要在两个平台上各做一个决定（`-l` 会不会让
   `.zprofile` 重复执行、`login` 包装是否还要保留）。
5. **是否需要「终端里也能触发 agent」**（比如把一段命令输出发给任务）：本 ADR 明确
   不做，但侧边栏已经是单入口，将来加「在终端里选中 → 发给当前任务」的成本不高。

**实现注记：「回到任务」删掉了，因为它保护的是另一个 bug（2026-11-10 修订）**

用户第一次问的是「这个按钮为什么留着」。当时给的理由是「侧栏收起后没有出路」——站不
住：`Cmd+B` 能把侧栏调回来。真正的理由在别处，而且比按钮大得多。

**这条按钮一直挡着一个丢数据的缺陷。** `park_and_clear` 只按 `active` 停放：
`if let Some(id) = self.active.take() { parked.insert(id, view) }`。还没发出第一条消息
的草稿没有会话 id，于是 `take()` 是 `None`，那份视图——连同用户刚打的字、粘贴的图片、
挑的文件——**直接 drop**。这在终端上最容易撞到（`Cmd+T` 看一眼 shell，再点侧栏的任务行
想回去），但它跟终端无关：在草稿上点任何一条任务行都会丢。

所以删按钮前先修根因：**草稿也停放**。

- `parked` 的键从 `String` 换成 `SessionTarget`（`Session(id)` / `Draft`）。这不是新
  发明——事件分流早就在用这个枚举当标签（§2、实现注记 4），「草稿」在那里就已经是一个
  真正的分支。用同一个类型当停放键，草稿不必借一个哨兵字符串（`""` 或 `"draft"`）：
  一个真名叫 `draft` 的会话和草稿在类型上就不相等，`a_session_named_draft_does_not_
  share_the_draft_slot` 钉住这条。
- **空草稿仍然丢掉**（`park_slot` 返回 `None`）：留着它只会让下一次 `+` 装回一个空输入
  框而不是建任务，表现就是「加号坏了」。
- **回去的路是 `+`**。停放的草稿不该只躺在内存里没人能到：`new_session` 在建新会话之前
  先 `unpark_draft`，装回那份草稿并提示「继续未发送的新任务」。`+` 的语义本来就是「给我
  一个能写的新任务」，那份草稿正是它。
- **切换时明说**。`resume` 里换走一份有内容的草稿会在状态栏留一句「未发送的新任务已暂
  存，按 + 回到它」——字从屏幕上消失却一声不响，等于让用户重打一遍。
- 反过来也补上了：屏上就是一份没发出去的草稿时按 `+`，不再把它挤成停放视图，而是提示
  「发送它，或清空后再新建任务」。

**`Cmd+T` 同时改成双向开关**，这是本 ADR 当初给删按钮开的第二个条件（第一个是「侧栏
收起时保留图标入口」，见下）。原来 `open_terminal` 在已有 tab 时还会再开一个 tab，所以
`Cmd+T` 只能进不能出；现在终端视图里按它就是回任务视图，任务视图里按就是去终端（没有再
多开一个——新 tab 走 tab 条末尾的 `+`，那里本来就有一个鼠标入口）。

**第一个条件没有照做，理由如下**：侧栏收起状态**只能**用 `Cmd+B` 进入（`sidebar_visible`
唯一的写者就是那个 action，界面上没有收起按钮）。也就是说，一个停在「侧栏已收起 + 终端
视图」的人，一定是知道 `Cmd+B` 的人；再按一次就回来了。为一个只有热键能进的状态单独做
一套图标入口，是在给不会到达它的用户建通道。若将来侧栏有了鼠标收起按钮，这条要重新考
虑——那时收起状态才真的可能被一个不知道任何热键的用户踩到。

> **2026-11-10 追记**：上面那个「将来」到了。ADR-0063 给侧栏加了鼠标折叠按钮与收起态的
> 图标轨，「收起状态只有热键能进」这个前提不再成立，因此「收起时保留图标入口」由
> ADR-0063 兑现。本节保留原文，读到这里请接着看 ADR-0063。

顺带一个代价：`parked` 的查找不再是 `get_mut(&str)`。读事件那条路径每次要构造
`SessionTarget::Session(id.to_string())`（一个 `String` 分配），比原来多一次分配。要消
掉它得给 `SessionTarget` 实现 `Borrow<str>`，而 `Draft` 分支不可能借成 `&str`——为一条
每帧几十次、单次几十纳秒的路径引入这种不对称不值。

## ADR-0063：侧栏折叠——图标轨、双段手风琴、定时任务第一次有界面（2026-11-10）

**状态**：Accepted（2026-11-10）。本轮**只做界面**：定时视图是只读看板，没有增删改，
也没有「立即运行」（见 §5 与 §8 的 G）。依赖 ADR-0058（多任务）、0059（定时任务）、
0062（内建终端）。

**来源**：`black-box`（折叠交互的形态观察自 dsh 的侧栏折叠）、`derived`（其余为本文推导）。
按 SOURCES.md，「借交互模式」不等于「借实现或文案」：本文不引用 dsh 的界面文字与代码，
所有 UI 字样仍自创。

**背景**

三件事凑到一起，才让这次改动有必要，而不是「看着别家好看就抄一个」。

1. **上一轮留了一个条件，这次被触发了。** v0.8.0 删「回到任务」时，ADR 里明确写了没做
   「侧栏收起时保留图标入口」的理由：收起状态**只能**用 `Cmd+B` 进入（`sidebar_visible`
   唯一的写者是那个 action），所以踩到这个状态的人必定知道怎么出来；并且注明「若将来侧
   栏有了鼠标收起按钮，这条要重新考虑」。**本 ADR 正是那个「将来」**——折叠一旦有鼠标入
   口，就可能被一个不知道任何热键的用户踩到，「入口全消失」立刻从可接受变成丢数据级别的
   问题（他连终端都回不去，除了猜 `Cmd+B`）。所以图标轨不是锦上添花，是那条注记的兑现。
2. **现在的「折叠」太贫。** `sidebar_visible = false` 只是把整个 panel 从 `h_resizable` 里
   拿掉：宽度让出来了，可达性一起没了。折叠要的本来是「让出宽度、不丢入口」，现在只做到
   前一半。
3. **定时任务（ADR-0059）至今没有界面。** 它的运行状态只在它指向的那一行任务上加一句提
   示（`ScheduleRow::hint`，`render/sidebar.rs:243`）。这意味着：没有指向任务的条目（
   `new = true`）**完全不可见**；看不到下次何时触发；看不到有几条；`enabled = false` 的条
   目只有翻配置文件才知道。数据早就有（`Backend::schedules()`），缺的是一块地方放。

**决定**

### 1. 侧栏两态，收起态是固定宽度的图标轨

`sidebar_visible: bool` 的语义从「有没有侧栏」改成「侧栏是展开还是图标轨」。收起不再把
panel 从布局里拿掉，而是渲染一条**固定宽度**的轨（不与 `SIDEBAR_MIN_WIDTH` /
`SIDEBAR_MAX_WIDTH` 共用范围，拖动把手在收起态不出现）。`Cmd+B` 与轨上的展开按钮是同一
个状态的两种入口。

宽度写进常量（初值 48px），不放进 `GuiPreferences`：这是布局常数，不是用户偏好；能调的
是展开态的宽度，那个已经由拖动把手改了。

**图标轨里没有 `+`，因此折叠态不能新建任务。** 这不是省略，是选定行为：新建任务的入口
只属于「我的任务」段头，而段头只在展开态存在。代价见 §8 的连带后果。

### 2. 顶部一行：logo + 产品名 + 折叠按钮

- logo 复用已有的 `qcode-mark-current.svg`——它本来就是 `currentColor` 的，随主题走，
  `render/transcript.rs:161` 已经在「工作中」那一行用过同一份字节。
- 产品名写 **`AI 工具`**（2026-11-10 定）。这是个临时名，与二进制名 `qc`、crate 名
  `qcode` 不是一回事：界面上的中文标签（我的任务、定时任务、终端、设置）是**功能名**，
  这一个是**产品标识**。将来定下正式名时只改这一处字符串。
- **`+`（新建任务）从这一行挪到「我的任务」段头**：头部属于应用，`+` 属于任务段；
  「定时任务」段将来在自己的段头上放操作时，位置也是对称的。
- 折叠按钮与 logo/产品名同一行右端，图标用一对 chevron（展开态指向收起方向，轨态反向）。

### 3. 正文是两段手风琴：互斥，且**总有一段开着**

侧栏正文从「一个滚动区」变成**两个段**：

```
我的任务  [+]          ← 段头，点击切换
  ⋯ 任务行 ⋯
定时任务               ← 段头
  ⋯ 定时条目 ⋯
```

- 两段**互斥**：点「定时任务」段头 → 任务段收起、定时段展开；反之亦然。这就是「乒乓」。
- **不存在「两段都收起」**（2026-11-10 定）。状态是 `enum SidebarSection { Tasks,
  Schedule }`，**不是** `Option<...>`；缺省 `Tasks`。
- **点已经展开的段头不做任何事。** 这是「总有一段开着」的必然结果，不是遗漏。为了让「按
  了没反应」不被当成坏掉，展开的段头必须有明确的展开态样式（chevron 朝下 + 段头高亮），
  一眼看出它不是个待按的东西。
  备选是「点当前段头 = 折叠整个侧栏」，那样「按了总有反应」，也少一个猜测；不采用的理
  由：段头的职责是**切换哪段**，折叠侧栏已经有专门的按钮和 `Cmd+B` 两个入口，再塞一个
  隐式入口会让「点段头会怎样」变得不可预期。

**只有展开的那一段可滚动**，段头本身不滚、两段的段头都始终可见。这是 v0.8.0 修过的同一
个坑：原来整栏一个滚动区，任务一多，「终端」「设置」被滚出屏幕。现在多一段，若还让整栏
滚，收起的那段会被顶得上下浮动。

### 4. 图标轨：五格，**一律只切视图**（2026-11-10 定，方案 A2）

自上而下：**展开 / 任务 / 定时任务 / 终端 / 设置**。

| 图标 | 点下去发生什么 | 侧栏 |
|---|---|---|
| 展开 | 轨 → 展开态，落在**上次停留的段** | 变宽 |
| 任务 | 主区切到任务视图 | **保持折叠** |
| 定时任务 | 主区切到定时任务视图 | **保持折叠** |
| 终端 | 主区切到终端视图 | 保持折叠 |
| 设置 | 打开设置窗口 | 保持折叠 |

**四个功能图标语义完全同质：只切主区视图，从不动侧栏宽度。** 这样不会有人疑惑「为什么这
个变宽了那个没有」。曾考虑过「点段图标顺带展开侧栏」（方案 A1），理由是段就在侧栏里、要
它就得让它可见；不采用是因为它与「任务/定时任务不过是两种主区视图」这个更简单的模型相
悖，而且用户明确选了 A2。

轨上点「任务」/「定时任务」时，若当前段不是它，**段状态跟着改**（但不展开）：这样下一次
点展开时落在这个段上。改的是记忆中「上次在哪段」，不是可见布局。

### 5. 定时任务：本轮只做界面，只读

编辑定时条目必须**写配置文件**（`[schedules.<name>]` 是唯一声明处），而
`reload_on_config_change` 按 mtime 重建 backend（`shell/mod.rs:606-616`）。ADR-0059 §1
已经为「不要写回配置」立过硬理由：写一次配置 = 换一次 backend + 一次 `resume`，还要与用
户在设置页的编辑抢同一份文件。

所以本轮**没有**增删改，也**没有**「立即运行」（后者虽不动配置，但需要给 `Backend` 加可
变方法；2026-11-10 定为本轮不做）。定时视图是**只读看板**，每条显示：

| 字段 | 说明 |
|---|---|
| 名字 | `[schedules.<name>]` 的表名 |
| cron | 原文，不翻译成人话——用户写的就是它 |
| 下次触发 | 相对时间（「3 分钟后」），渲染时用当前时间算 |
| 上次结果 | 成功 / 失败 / 跳过 / 错过 |
| 错过次数 | `missed_windows`，非零才显示 |
| 指向的任务 | 可点，点了 `resume` 那个任务 |
| `overlap` / `approval_timeout` | 触发撞车与无人值守审批的行为 |
| 失效原因 | `broken` / `Orphaned` 原文，与任务行上的 `hint` 同源 |

`new = true` 的条目没有指向任务，任务列显示「每次新建任务」。没有 `[schedules]` 时给空
状态，**明说**在 `config.toml` 的 `[schedules.<name>]` 里声明——诚实地说出真相，比放一个
假的新建按钮好。

### 6. 收起态带徽标

轨上的图标带点：

- **任务**：有任何任务处于待审批 / 未读 / 后台在跑 → 一个点。
- **定时任务**：有条目的 `hint()` 非空（配置错、失效、错过、失败）→ 一个点。

理由：折叠的代价是看不见列表。若收起后什么信号都没有，「折叠」就成了「静默丢信息」——
错过的定时、憋着的审批都要靠用户想起来去展开才发现。这也是 ADR-0059 §3 把错过做成侧栏
提示的同一个动机。**不带数字**：48px 宽的轨里数字读不清，也会随字号缩放溢出。审批与未读
也不分色——同一个点是「需要你注意」，展开后自然分得清。

### 7. 折叠不动「当前任务」，但记住自己的状态

折叠/切换段/切换视图**一律不触碰** `active`、`self.ui`、`parked`。这条不是新原则，是
ADR-0058 §4 与 ADR-0062 §2 已经定过的同一条：视图维度与任务身份是两件事。折叠尤其容易
破这条——它看起来像「把所有东西收起来」，但收起的是**侧栏的显示**，不是用户的活儿。

**但侧栏自己的两个状态要记忆**（2026-11-10 定）：折叠与否、当前在哪一段，写进
`GuiPreferences`。理由：折叠现在有了鼠标入口，每次启动都回到展开态会显得坏——用户按了
折叠键表达的正是持久偏好。

### 8. 连带后果

- **新建任务的路径变长了，这是 A2 的直接代价。** A2 下图标只切视图，所以折叠态无论点哪个
  图标都不会让 `+` 出现。要新建任务：点「展开」→ 若上次停在「定时任务」段，还要点一下
  「我的任务」段头 → 才看到 `+`。两步到三步。
  这与 §1「折叠态不能新建」是同一条决定的两面，用户已确认接受。留一个观察点：若实际使用
  中这个路径真的碍事，最小改动是让展开按钮落在「上次的段」之外再记一个「新建任务」的入
  口，而不是把 A2 改回 A1。
- **撤销一条旧注记。** ADR-0062 末尾「第一个条件没有照做」那段的前提（收起状态只有热键能
  进）从本 ADR 起不再成立，那段保留原文并已加追记指向本文，免得后人读到相互矛盾的结论。
- `MainView` 从 `{ Task, Terminal }` 变 `{ Task, Schedule, Terminal }`；
  `render/mod.rs:174` 的 match 与 `render_main` 的分派都要穷尽。ADR-0062 §2 的规则同样适
  用于定时视图：定时视图下不渲染输入框、rail 圆点、队列 dock、transcript——它们都是任务
  的产物。
- **不要每帧调 `Backend::schedules()`**：它读配置与 store。缓存一份 `Vec<ScheduleRow>`，
  按现有 `refresh_tasks` 的节奏刷新；「下次触发」这类相对时间在渲染时用当前时间算，于是
  「3 分钟后」会自己走动而不需要额外刷新。
- 侧栏的两个宽度常量、轨宽、段头样式集中在一处，避免 `render/sidebar.rs` 里散落的
  `px(30.0)` / `sp(14.0)` 继续增殖。
- 图标轨的五个图标要检查资源是否在 `AllAssets` 里（`run.rs:7` 用的是全套 1830 个，不是
  只有 106 个的默认包 `Assets`）。`folder-kanban` 已经确认只在前者里——若哪天换成
  `Assets`，图标会静默变空。

**备选方案（未采用）**

- **折叠=整块消失（现状）**：宽度让出来了，入口全没了。就是背景第 2 条。
- **A1：段图标顺带展开侧栏**：少一次点击，但五个图标语义不再同质。§4。
- **两段都可收起（`Option` 的 `None`）**：更自由，但「空侧栏」没有用处，且会让「点当前段
  头该怎样」多一个分支。§3。
- **定时只做侧栏段、不做主区视图**（点条目 = `resume` 它指向的任务）：更便宜，而「一条定
  时的内容其实就是它那个任务」这个直觉成立。不采的原因：`new = true` 的条目没有指向任何
  任务，点它无处可去；且下次触发/错过次数这些状态无处安放。
- **侧栏常驻、只折叠内容区**：与折叠的目的（让出宽度）相反。
- **把「设置」也做成主区视图**：设置现在是独立窗口，还带着「关闭 = 隐藏到托盘」的托盘语
  义（ADR-0052）。为了让轨上五个图标同质而改掉窗口结构，代价与收益不成比例。轨上的设置
  图标仍开那个窗口，悬停提示说明「在新窗口中打开」。
- **收起态宽度也做拖动/可调**：48px 的轨没有可调余地，可调的是展开态。

**验证计划**（实现时按这个验收）

1. 折叠态渲染出五格，顺序固定；点每一格的效果符合 §4 那张表，**且侧栏宽度不变**。
2. 折叠态**不存在** `+`：新建任务只能从展开态的「我的任务」段头进入。
3. `Cmd+B` 在展开态与轨之间切换，状态与鼠标按钮一致（同一个字段，不是两套）。
4. 两段互斥且总有一段开着：点另一段头会切换；点当前段头无变化（且段头有展开态样式）。
5. 两段都收起这种状态**构造不出来**（类型上就不存在，不是靠运行时判断）。
6. 折叠前后切到终端再回来：任务视图的滚动位置、展开的代码块、**没发出去的草稿**都还在
   （v0.8.0 的那条回归路径，`parked` 的 `SessionTarget::Draft` 槽位）。
7. 任务多到溢出时，两段段头与「终端」「设置」不被顶出屏幕（v0.8.0 修过的同一类回归）。
8. 轨宽与展开宽度在三种字号缩放下都不重叠；轨上图标不因徽标而位移。
9. 重启后折叠态与当前段都回来了（`GuiPreferences`）。
10. 定时视图：无 `[schedules]` 时给空状态并指出配置位置；`new = true` 的条目在没有任务行
    时可见；`broken` / `Orphaned` / `Missed` 三种状态都显示出来。

**讨论后确认的清单（2026-11-10）**

| 项 | 结论 |
|---|---|
| A 段图标是否同时展开侧栏 | **A2**：只切视图，不动侧栏宽度 |
| B 允许两段都收起吗 | **不允许**，总有一段开着，缺省「我的任务」 |
| C 产品名 | **`AI 工具`**（临时名，只此一处字符串） |
| D 段的正式名字 | **「定时任务」** |
| E 设置图标 | 保持打开独立窗口，悬停提示说明 |
| F 收起态徽标 | **做**，任务与定时任务各一个点，不带数字、不分色 |
| G 「立即运行一次」 | **本轮不做**，定时视图纯只读 |
| H 快捷键 `Cmd+1/2/3` | **未定**，本轮不加；`Cmd+T`/`Cmd+B`/`Cmd+,` 保持现状 |
| I 折叠态与当前段持久化 | **记忆**，写进 `GuiPreferences` |

**尚未决定、留给实现时再看的**

- H 的快捷键：等界面落地、实际用一段时间再判断是否需要直选键。
- 正式产品名（现为 `AI 工具`）。

## ADR-0064：shell 环境——解释器跟着终端探测，环境采样一次（2026-11-10）

**状态**：Accepted（2026-11-10）。本轮只改 `run_shell` 一侧：解释器探测、环境采样与注入、
名字级审计。`[shell]` 只有采样开关，**没有解释器这一项**。

**来源**：`derived`（由现有代码、实测与用户反馈推导）；`black-box`（`env -0` 采样、
「值不进日志、只记名字」的审计形态参考自 dsh 的 shell 集成方式）。

**背景**

`qc gui` 从 Dock / Finder 启动时，进程环境是 launchd 给的：没有 `JAVA_HOME`、`GOROOT`、
`COT_HOME`，`PATH` 是最小集。用户在自己的终端里跑 `c`（`. ~/cot/bin/activate`）之后才有
那套变量；GUI 里没有这一步，于是 `run_shell` 跑出来的命令看不到任何工具链。

更糟的是解释器。`crates/qcode-tools/src/shell.rs` 的 `shell_invocation()` 固定返回
`("sh", ["-c", command])`。`sh` 不认 `[[ ]]`、`set -o pipefail`、数组与关联数组；用户从
zsh 里粘过来的命令会以「语法错误」失败，而报错看起来像是命令本身写错了。

第三条是安全边界。同一文件里 `command.env_clear()` 之后只注入 12 个白名单变量，注释写着
“so API keys in the parent process cannot be read by `env`”。这条边界是故意的，也是本 ADR
要**撤回**的东西——见「撤回的安全假设」。

而「探测」这件事代码里早就有了：`crates/qcode-terminal/src/shell.rs` 的
`detect_with(&dyn Probe, Option<&str>)`，顺序是「配置 → 父进程链 → 登录 shell
（`getpwuid_r`；macOS 账户在 Open Directory 里，`/etc/passwd` 查不到）→ `$SHELL` →
`/bin/sh`」。终端视图用它决定开哪个 shell，`run_shell` 却另走一条硬编码的 `sh`。同一个
问题两个答案——先把这个修掉，才谈得上环境。

**决定**

1. **解释器不配置，跟终端用同一套探测**

   把探测部分抽成 `crates/qcode-shell`（只依赖 `libc`）：`detect`、`detect_with`、
   `System`、`enclosing_shell`、`login_shell`、`clean_path`、`resolve_in_path`、
   `process_cwd`、`resolve_start_cwd`。`crates/qcode-terminal/src/shell.rs` 变成
   `pub use qcode_shell::*;`，现有调用点一行不改。

   `qcode-tools` 依赖 `qcode-shell`，**不依赖** `qcode-terminal`：后者带着
   `alacritty_terminal`，为了取一个 shell 路径把终端模拟器拖进工具链不值得。

   `run_shell` 调 `detect_with(&System, terminal_shell_override)`：`[terminal] shell`
   是同一次探测的覆盖，终端与工具共用一个壳。探测不到（Windows 或极端容器）时退回今天
   的 `sh -c`。

   配套：`shell_invocation()` 从 `("sh", ["-c", cmd])` 变成 `(detected, ["-c", cmd])`。
   不加 `-l`：环境由第 3 条的采样提供，每条命令都登录一次只会重复跑 rc、变慢且注入噪声。

2. **`[shell]` 只有采样开关**

   ```toml
   [shell]
   env_sampling = "auto"          # auto | off
   env_command = ""               # 覆盖采样脚本；空 = 按 shell 自动拼
   env_timeout_ms = 5000
   env_ttl_secs = 600             # 0 = 只在会话开始与 rc 变化时重采
   env_include = ["COT_*", "JAVA_HOME", "GOROOT", "M2_HOME"]
   env_exclude = ["_", "PWD", "OLDPWD", "SHLVL", "RANDOM", "SECONDS"]
   env_sample_sandboxed = false
   audit = "names"                # names | off
   ```

   没有 `program`。用户要换壳就去改 `[terminal] shell`——那是同一个探测的覆盖，两个
   消费者都跟着变，不会出现「终端是 zsh、工具是 bash」。

   自动拼出来的采样命令：zsh / bash / fish 用 `{shell} -lic 'env -0'`；sh 系（dash、
   ash）用 `{shell} -lc 'env -0'`（没有 rc 可读，`-i` 只会带来噪声）。

3. **采样一次，注入子进程**

   会话开始（第一个 turn 之前）采一次，结果缓存在 `ShellEnvCache` 里；`RunShell` 每次
   启动子进程时把快照写进环境。采样命令在**独立进程**里跑，超时与退出码都要检查，绝不
   让 turn 等它超过 `env_timeout_ms`。

   注入顺序（后者覆盖前者）：

   ```
   env_clear()
   → ENV_ALLOWLIST（今天的 12 项，作为「没配采样时」的基线）
   → 快照
   → env_include 从 qc 进程直通（仅当 qc 自己带着这些变量）
   → TMPDIR / TMP / TEMP 与网络代理（必须由我们赢，否则沙箱与代理会失效）
   ```

   `env_exclude` 在快照落缓存前生效，用于去掉 `PWD`/`SHLVL`/`_` 这类每条命令都变的项。

4. **解析按 NUL 分块，不按行**

   `env -0` 用 NUL 分隔，`PATH` 里的空格、带换行的证书、`PS1` 都不会把解析打散。解析
   规则：按 NUL 切块 → 每块以**第一个** `=` 分成名字与值 → 名字为空、含空格/换行、或
   整块没有 `=` 的丢弃。这一条同时解决「`-lic` 跑 rc 时把欢迎语 / `echo` 输出混进
   stdout」的问题：噪声块没有 `=`，自然被丢掉。

5. **失效与降级**

   顺序即优先级：

   1. 会话开始必采（`env_sampling = "auto"` 时）；
   2. **rc 指纹**：每个 turn 开始 `stat` 一次 `$ZDOTDIR/.zshrc`、`~/.zshrc`、
      `~/.zprofile`、`~/.zshenv`、`~/.bash_profile`、`~/.bashrc`、`~/.profile` 的
      mtime+size，任一变化即失效；
   3. **TTL**：`env_ttl_secs` 到期重采（覆盖 `nvm use`、换 toolchain 这类不碰 rc 的变化）；
   4. **手动刷新**：`qc shell-env --refresh` 与设置页按钮。

   采样失败（非零退出、超时、解析空）**不中断命令**：沿用上一份快照并记 `warn`；首次就
   失败则保持白名单基线，并在会话顶部给一条 notice——不能静默降级。

6. **白名单仍在，但排在快照之下**

   `ENV_ALLOWLIST` 保留，作为「`env_sampling = "off"` 或采样失败」时的兜底，也负责那些
   不管用户 shell 怎么配都必须存在的变量（`HOME`、`TERM`）。顺序见第 3 条：快照可以覆盖
   白名单，但 `TMPDIR` 与代理永远最后写。

7. **审计只记名字，值不进日志**

   会话日志新增事件：

   ```json
   {"type":"shell_env_sampled","data":{"source":"command","program":"/bin/zsh",
     "count":43,"added":["JAVA_HOME","GOROOT"],"removed":["CARGO_HOME"],
     "duration_ms":214,"cached":false,"error":null}}
   ```

   外加一条会话摘要：`环境：zsh -lic（43 项，PATH +12，JAVA_HOME 新增）`。

   `qc shell-env` 在本地打印一张表（名字 + 脱敏值：前 4 个字符 + `…`）；`--full` 才显原值，
   且只在 tty 下允许。

   **system prompt 里不注入环境明细**：太贵，而且一旦进了上下文就等于进了模型可见面。
   只注入一行「shell 环境来自 `zsh -lic`，完整列表用 `qc shell-env`」。

8. **采样默认不进沙箱**

   采样要拿的是「用户终端里的真实环境」，而 macOS 的 SBPL 会挡掉不少路径，沙箱内采出来的
   世界会缺东西。所以 `env_sample_sandboxed = false` 是默认；在意「采样这一步也不该越权」
   的人可以打开它，代价是快照与终端里的实际环境会有差异。

   注意这不改变 `run_shell` 的沙箱：采样只是读环境，命令本身仍在原来的 sandbox 里跑。

**实现注记（2026-11-10）**：审计目前落在进程日志（`tracing::info!`，只带
`program`/`count`/`added`/`removed`/`duration_ms`），还没有写进会话日志。原因是采样发生在
`RunShell` 执行期，那一刻手里没有会话写入者；要发 `shell_env_sampled` 事件就得把 writer
穿进工具层，代价大于收益。会话日志事件列在 P2，可观测性先由 `qc shell-env` 与设置页的
「采样一次」承担。

**撤回的安全假设**

本 ADR 明确撤回 `shell.rs` 顶部那条「`env_clear` 防止 API key 被 `env` 读到」的目标。采样
之后，`DEEPSEEK_API_KEY`、`GITHUB_TOKEN` 这类变量会进 agent 的 shell 环境：

- agent 可以直接 `echo $DEEPSEEK_API_KEY`，也可以被仓库里的 `AGENTS.md`、skill 文本、
  依赖包里的提示词诱导把它写进文件或网络请求；
- 这不是「漏洞」，是**选定的行为**：用户认为「工具链可用」比「密钥不可见」重要。

补偿与边界：

- `env_sampling` 可以设成 `"off"`，回到今天的白名单行为；
- **值永远不进日志**（审计只记名字），所以泄漏面是「模型上下文与它写的文件」，不是日志；
- `qc shell-env` 的 `--full` 只在本机 tty 下打印原值；
- 不做 secret 过滤：半吊子的过滤（按 `*_KEY` 匹配）会给人虚假的安全感，也让
  `DATABASE_URL` 这种带口令的变量漏掉。要么采、要么不采，用户选。

**边界**

- 只认 `env -0` 的输出格式；`env_command` 若输出别的格式，解析结果就是空的（审计里有
  `count: 0` 可查）。
- 不做「每条命令重采」：那是 A 方案的代价（慢、噪声），本 ADR 明确不做。
- 不做 per-project 环境文件（`.qcode/env`）：与 shell 漂移，且要用户手工维护。
- Windows 的采样模板不在本轮范围：`pwsh` 下没有 `env -0`，等有人真在 Windows 上要。
- 采样失败不改变退出码、不阻塞 turn；命令照跑。
- 环境快照**不写进 session 日志**，因此 `qc sessions rollback` 与它无关。

**验证计划**

1. `env -0` 解析：值里含空格 / 换行 / `=`；重复键后者胜；缺 `=` 的块丢弃；rc 噪声丢弃。
2. `include` / `exclude` 的 glob 语义（`COT_*` 前缀、精确名、大小写敏感）。
3. `env_command` 非零退出 → 保留旧快照 + 审计 `error`，命令仍可跑。
4. `env_command` 超时 → 同上，且不拖慢 turn。
5. rc 文件 mtime 变化 → 下一次取样重新执行（临时目录伪造 rc）。
6. 解释器跟随探测：`[[ 1 == 1 ]]` 在 zsh 下通过、在 `sh` 下失败（证明壳真的换了）。
7. 快照的 `PATH` 覆盖白名单，但 `TMPDIR` 与代理仍由我们胜出。
8. `env_sampling = "off"` → 与今天逐字节一致（回归基线）。
9. 真机：会话里 `echo $JAVA_HOME` 有值；`LANG=zh_CN.UTF-8 zsh -c 'echo $LANG'` 携带；
   故意 `echo ${DEEPSEEK_API_KEY:0:4}` 有输出（记录这是选定行为）。

**备选方案（未采用）**

| 方案 | 为什么不做 |
|---|---|
| 每条命令都 `-lic` | 每个命令跑一次 rc：慢、刷屏、`cd` 之类副作用重复执行 |
| 只放宽白名单 | Dock 启动的父进程里**根本没有**这些变量，白名单放宽也拿不到 |
| AGENTS.md 里写「先 `source ~/cot/bin/activate`」 | 靠模型自觉，不可验证，且每次会话都要烧上下文 |
| 默认走 direnv / mise | 外部依赖；而「装 direnv 之前就得有环境」是鸡生蛋 |
| 每个仓库 `.qcode/env` | 与用户 shell 漂移，成为第二份真相 |

**待定**

- `env_sampling = "auto"` 的首个 turn 成本（100–400ms）是否值得改成「先跑、后台采、下一
  turn 生效」。先按同步做，用真机数据再判断。
- Windows 的采样模板与 ConPTY 的配合。
- 是否允许项目级关闭（`.qcode/config.toml` 的 `[shell].env_sampling = "off"`）——倾向允许。
- `qc shell-env --full` 是否需要二次确认（当前只要求 tty）。

## ADR-0065：配置文件只写覆盖项——schema 2（2026-11-10）

**状态**：拟议。分三期落地，第一期只加 `schema` 与 `qc config migrate`。

**来源类别**：derived。来自现状代码（`crates/qcode-kernel/src/config`）与用户反馈「配置
文件应该如何简化」——今天的默认文件一百来行，其中大半是 resolver 本来就能填的默认值。

**背景**

一个刚装好的 qcode 会写出这样一份文件：`[providers.deepseek]` 里有 `base_url`、
`api_key_env`、`default_model`，还有一张 `[[providers.deepseek.models]]` 表，每项重复
`id`、`name`、`context_window`；`[default-model]` 又写一遍 provider 与 model；
`[sandbox]` 管进程沙箱而 `[network]` 管出口，用户的心智模型却是「这个 shell 能不能上网」。

三个具体的痛：

1. **默认模型有两个来源。** `[providers.X].default_model` 与 `[default-model]` 都声明默认，
   谁赢要靠 `sync_default_model` 在写盘时替用户对齐（ADR-0055）。这是把一个数据模型问题
   变成了同步问题。
2. **`models` 表在重复 catalog 已经知道的事。** `/models` 与内置 catalog 能给出
   `name`/`context_window`/`max_tokens`，文件里再抄一遍，一旦上游改名就变成两份真相
   （见 ADR-0055 的「后续：id 与显示名」）。
3. **网络与沙箱分裂。** 用户配的是「让 agent 能装包」，而这既要有出口代理（`[network]`）
   又要决定 `run_shell` 是否进沙箱（`[sandbox]`），两处分开写容易只改一半。

**决定**

1. **`schema = 2` 取代 `version`。** 缺省视为 1；v1 文件按 v1 解释，写回仍写 v1，直到用户
   显式 `qc config migrate`。理由：读兼容可以永远做，写升级必须由用户点一次。
2. **provider 段只写覆盖项。** `[providers.deepseek]` 里可以只有 `api_key_env = "DEEPSEEK_API_KEY"`；
   `base_url`、协议、`models` 缺省取 catalog。没写的键不是「空」，是「用默认」。
3. **删 `[providers.*].default_model`。** 默认模型只由 `[default-model]` 或设置页卡片上的
   「设为默认」写；读 v1 时把它当作 `[default-model]` 的候选，写 v2 时不再产出。
4. **`models = ["deepseek-flash", "deepseek-v4-pro"]` 简写。** 只有需要覆盖元数据时才写表
   （`models = [{ id = "…", name = "…" }]`）；探测结果写回时只用简写。
5. **`[sandbox]` 收编 `[network]`。** 新键 `network = "on" | "off" | "strict"`（`strict` 即今
   天的 deny 出口），旧的 `[network]` 段读入时映射过来，v2 不再写出。
6. **审批规则用简写表。** `allow = ["run_shell:echo *"]`、`deny = ["run_shell:rm -rf *"]`，
   等价于今天的规则表；需要「一次/会话」这类粒度时才退回表。
7. **`pricing` 仍须显式声明，缺省时由 `qc doctor` 提示。**
   原方案是「缺省取内置 catalog」，实现时否掉了：仓库里没有可信的价格来源，猜一个数会让
   `--max-cost` 默默算错，比留空更糟（留空时成本显示为 `—`，用户知道它不知道）。
   因此 `[pricing]` 保持今天的语义——写了才算，没写就是 `None`；`qc doctor` 在缺省时给一行
   「没有价格，`--max-cost` 不会生效；在 `[pricing]` 里声明或忽略这条」。等有了可核对的价目表
   （例如随 release 分发的快照，带来源与日期）再把缺省值加回来。
8. **写回时删空表。** 与默认值相同的段不落盘；`qc config init` 写出的是空文件加注释。
9. **`qc config show --effective` 是唯一权威视图。** 打印合并后的值与来源（file/env/flag/catalog），
   `--json` 给脚本用。

**实现注记（2026-11-10）**

- 键名：`Config.schema` 带 `#[serde(alias = "version")]`，两种拼写都读得懂。`migrate` 遇到
  用 `version` 的文件**保留这个键名**、只把值改成 2——改名会用 `toml_edit` 的 `insert`
  把键上挂着的注释一起丢掉；而 alias 让两种拼写等价，保留旧名不损失语义。
- `models` 的两种写法由 `models_from_entries_or_ids` 统一接受（`["id"]` 或
  `[{ id, name, context_window }]`，可混写）；`migrate` 只把「只有一个 `id`」的表折成
  字符串数组，带元数据的表原样保留。
- 网络：`[sandbox] network` 接受 `off`/`proxy`/`strict`/`open`，也接受旧布尔（`true` 读作
  `open`，`false` 读作「没表态」）。这四个值折成 `Config::network_plan() -> NetworkPlan { proxy, strict, raw }`，
  `build_egress` 与 `sandbox_network` 都只读这份计划，不再各读一个键。缺省（没写这个键）时仍按
  `[network] mode/enabled` 推导，所以 v1 文件的行为不变。`migrate` 把 `[network] enabled/mode`
  折成 `[sandbox] network` 并删掉这两个键，**保留** `allow`/`allow_private`/`unknown`/`audit`——
  这个开关回答的是「能不能上网」，不是「能上哪些主机」。
- 审批简写：`[approval] allow = ["run_shell:echo *"]` / `deny = [...]` 在 `build_policy` 里展开成
  `Rule`（`run_shell` 用 `command_prefix`，其他工具用 `path_glob`，裸工具名只匹配工具），id 为
  `allow-0`/`deny-1` 这样的序号；`[[approval.rules]]` 保留给需要 scope 或多条件的情况。
- 写回：空段不写。`ConfigEdit::apply_config` 末尾的 `prune_empty_sections()` 删掉空的
  `[providers]`/`[pricing]`，以及既无 `rules` 又无 `allow`/`deny` 的 `[approval]`；只有
  `id` 的模型表写成 `models = ["id"]`，带 `name`/`context_window`/`max_tokens` 的才写
  `[[providers.<name>.models]]` 表。新建文件写 `schema = 2`，已有 `version` 的文件保持
  那个拼写（`schema` 是它的别名）。
- 设置页写回时**不改 schema**：`ConfigEdit::apply_config` 只在文件里本就有 `schema`、
  或完全没有版本键时才写 `schema`，否则保持原样——升级只发生在 `qc config migrate`。

**不变式**

- v1 与 v2 都能读，任何读路径都不因 schema 不同而改变语义。
- catalog 是默认值的唯一来源；文件只覆盖，不复制。
- `qc config show` 永远显示**合并后**的结果，不显示「文件里写了什么」。

**迁移**

- `qc config migrate`：读 v1 → 写 v2，先写 `config.toml.bak`，幂等；只动用户文件
  （`~/.config/qcode/config.toml`），项目级 `.qcode.toml` 只读、不改。
- 认不出的键保留并在 stderr 列出，不静默丢弃。
- 迁移不自动执行：`qc config show` 在 v1 文件上提示一句「可以 `qc config migrate`」。

**分期与验收**

| 期 | 内容 | 验收 |
|---|---|---|
| P1 | `schema` 字段、`--effective`、`migrate` 的机械部分（删 `default_model`、`models` 简写） | v1 文件 migrate 后逐字段等价；再读一遍不报 unknown key |
| P2 | `[sandbox] network`、审批简写、pricing catalog | 旧 `[network]` 读入等价；简写规则与规则表展开结果相同 |
| P3 | 写回删空表、设置页按 v2 渲染 | `qc config init` 产物为空文件；设置页保存不写默认值 |

**备选方案（未采用）**

| 方案 | 为什么不做 |
|---|---|
| 不动，只在注释里说明默认值 | 文件仍是两份真相；用户改 A 忘记改 B 的问题不会消失 |
| 只做 `--effective`，不改 schema | 解决了「看不清」但没解决「写重复」，文件只会更长 |
| 换成 JSON5 / YAML | 换格式不解决数据模型重复，还多一个迁移 |
| 全部收进 CLI flag | 用户要的是可版本化的项目配置，不是 shell history |

**待定**

- `version` 是否保留为 `schema` 的别名（旧脚本可能写 `version`）。
- 项目级 `.qcode.toml` 是否也升 schema 2，还是永远按 v1 只读。
- `migrate` 是否提供 `--dry-run` 输出 diff（倾向提供）。

## ADR-0066：MCP server 的沙箱——运行时 r+exec，写入只给私有 HOME（2026-11-11）

**状态**：拟议。`[[mcp.servers]]` 原有字段不变，新增四个可选字段；不写新字段的配置行为
应当与今天**不同**（默认值从「什么都进不去」变成「推导着进得去」），因此这是一次可用性
修复，不是兼容性破坏。

**来源**：`derived`。来自一次实测、一次文档与代码的矛盾，以及三轮方案讨论被否掉的过程。

**背景**

本文件 §4.10 在此之前写着「server 进程默认不套 FS 沙箱」，实现却是无条件套：
`crates/qcode-kernel/src/lib.rs` 把 `run_shell` 用的那个 `Sandbox` 直接做成 `Confine`
闭包传给所有 MCP server。两边谁也没赢——用户在真机上遇到的是最坏的那种组合：
**沙箱在生效，但没有为 MCP 场景做过授权。**

实测（Linux + Landlock，2026-11-11）：

| 布置 | 结果 |
|---|---|
| `command = "/home/q/cot/lib/wordcraft_0.15.2_linux_amd64/bin/wordcraft"` | `Permission denied` |
| `command = "<workspace>/bin/wordcraft"`（符号链接到上面那处） | `No such file` |
| `command = "/tmp/wordcraft/bin/wordcraft"`（拷一份过去） | ✅ 19 个工具 |

原因单纯：`crates/qcode-tools/src/sandbox/linux.rs` 的只读白名单是
`/usr /bin /sbin /lib /lib64 /etc /opt /sys /proc /run /dev`，**`/home` 不在里面**；
而 Landlock 的 `path_beneath` 规则按**解析后的真实路径**判定，`/home/q/cot` 不在集合里，
符号链接过不去（实测报 `No such file` 而不是 `Permission denied`，同一条规则的两种表现）。
所以「拷到 `/tmp` 就好了」不是巧合：`/tmp` 在写白名单里。也就是说，**这个失败模式与用户
把 server 装在哪强相关**——装在 `/usr/local` 能用、装在 `~/.local` 不能用，而 `~/.local/bin`
恰恰是 pipx / uv / npm 的默认安装位置。

ADR-0027 的补充段只记了 Windows 一侧的同类做法，Linux 与 macOS 连这条「实现偏离了决定」
的注记都没有；这一段本 ADR 一并补上（见 ADR-0027 的补注）。

**被否掉的三个方向**（记下来，避免下一个人重新提）

1. **撤沙箱。** 第一版 ADR 草案就是这个方向，理由是「回到 4.10 的原文」。否决：MCP server
   是第三方代码，`npm i` / `pip install` 装来的，信任对象是「装即信它做它声称的事」，
   不是「信它无人值守地读写 `$HOME`」。撤掉之后唯一的补偿是工具审批，而审批覆盖不到
   server 在**握手期**自己做的事——今天已经能观察到 server 在 `initialize` 之前就读文件。
2. **放宽到 `$HOME` 只读。** 只是把上一版的洞缩小到「读全部 `$HOME`」。`~/.ssh/id_rsa`、
   `~/.aws/credentials`、`~/.config/gh/hosts.yml` 全在 `$HOME` 下，等于给每个 server 发一张
   通读证。同一轮实测里 wrapper dump 出 119 个继承变量（含 `GITHUB_TOKEN`、
   `DEEPSEEK_API_KEY`、`SSH_AUTH_SOCK`），说明「环境里的好东西」本来就已经在漏。
3. **只让用户手写 `read_paths`。** 能跑，但第一次上手必然报错：用户不知道 `pipx` 的真身是
   `~/.local/pipx/venvs/<name>/bin/python`、也想不到 `<pkg>/node_modules/.bin/x` 的边界。
   把「推导」推给用户，等于把 §4.10 的问题原样保留，只是换了个报错位置。

**决定**

### 1. 默认 confine，逐 server 一份授权集

`[mcp] sandbox = "confine"` 是默认值。每个 server 在 spawn 前算出一份 `GrantSet`：三个
不相交的集合，**此外一律无**。

| 集合 | 来源 |
|---|---|
| runtime（只读 + 执行） | 由 `command` / `args` 推导，`read_paths` 补充 |
| write | 任务 workspace roots（沿用现状）+ 该 server 的私有 HOME + `writable_paths` |
| 环境变量 | 与上面两个集合同源（见 §5） |

`GrantSet` 是纯数据、可打印、可序列化。`qc doctor` 打印的那一份和 spawn 时用的必须是
**同一个结构体**，不允许「文档说 A、实际给 B」。

### 2. runtime 的推导规则

按顺序：

1. `canonicalize` 解析 `command` 的符号链接，得到真实文件。
2. 读 shebang。`#!/usr/bin/env node` 要再解析出 `node` 的绝对路径。没有 shebang 的
   native binary 到此为止。
3. 向上归到**生态根**，而不是给整个父目录：

   | 形态 | 授权 |
   |---|---|
   | `<venv>/bin/python`（含 pipx `~/.local/pipx/venvs/<n>`） | 整个 `<venv>` 子树 r+x |
   | `<pkg>/node_modules/.bin/<x>` | 该 `<pkg>` 根 r+x |
   | `<target>/debug/<x>`（自编译的 stub） | **只给这一个文件** r+x |
   | 其余 | 只给该文件所在目录 r+x，且要求目录深度 ≥ 2 |

4. **危险根一律拒绝**，需要显式 opt-in：推导结果若等于 `/`、`$HOME`、`/home`、`/Users`，
   或是 workspace 的祖先，报错并让用户写 `read_paths`。这一条是防「自动推导把整机送出去」，
   不是为了拦用户——用户真想给，`read_paths = ["/"]` 也写得出来，但那是人写的、能 diff 的。

### 3. 合成 HOME

每个 server 一个私有目录 `<data_dir>/mcp/<server>/home`，**读写**授权；spawn 时把
`HOME`、`XDG_CONFIG_HOME`、`XDG_CACHE_HOME`、`XDG_DATA_HOME`、`TMPDIR`/`TMP`/`TEMP`
指过去。

这一步同时解决三件事：

- `npx` / `uvx` / pipx 需要写 cache，以前只能靠把 server 挪进 workspace 或 `/tmp` 来让
  它们跑起来（实测就是这么绕过去的）；
- 真实 `$HOME` 作为**变量值**消失，未声明的子目录一律不可达——`~/.ssh`、`~/.aws` 默认既读
  不到、也「不知道该去哪儿读」；声明的凭据子树是唯一例外（§4）；
- `TMPDIR` 不再依赖 ADR-0027 里那条「helper 在 spawn 前改写 `TMP`/`TEMP`/`TMPDIR`」的
  Windows 特例，三平台统一。

### 4. 凭据预设（`credentials`）

只给 `$HOME` 里的**具体子路径**，按语义命名，不让用户记路径：

| 预设 | 只读子树 | 透传的环境变量 |
|---|---|---|
| `github` | `~/.config/gh`、`~/.config/git` | `GITHUB_TOKEN`、`GH_TOKEN`、`GH_HOST` |
| `aws` | `~/.aws` | `AWS_PROFILE`、`AWS_REGION`、`AWS_DEFAULT_REGION`、`AWS_CONFIG_FILE`、`AWS_SHARED_CREDENTIALS_FILE`、`AWS_ACCESS_KEY_ID`、`AWS_SECRET_ACCESS_KEY`、`AWS_SESSION_TOKEN` |
| `ssh` | `~/.ssh` | `SSH_AUTH_SOCK` |
| `npm` | `~/.npmrc` | `NPM_TOKEN`、`NODE_AUTH_TOKEN` |
| `netrc` | `~/.netrc` | — |
| `docker` | `~/.docker/config.json` | — |

被选中的子树在合成 HOME 里各放一个**符号链接**（`<home>/.config/gh -> ~/.config/gh`），
这样 `gh` 按 `$HOME/.config/gh` 找得到，Landlock 又只放行了那一个真实子树。只读，所以
`gh` 想刷新 token 会失败——这是有意的，写回真实凭据库应该走 `qc` 自己（ADR-0056），
不是让 server 代劳。

`credentials` 里只写名字不写值：preset 只扩展成「路径 + 变量名」，密钥仍然只从进程环境或系统
凭据库来，符合 ADR-0018 与 ADR-0056「密钥不落盘、不进 argv」的既有约束。

### 5. 环境变量与授权集同源

`GrantSet` 决定哪些变量透传到子进程：预设里点名的变量 + 一份基础白名单
（`PATH`、`LANG`/`LC_*`、`TZ`）+ ADR-0023 的代理四件套
（`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`）。**其余一律不传。**

代理四件套必须留：ADR-0023 第 3 条依赖「给 shell/MCP 注入代理变量」来把 MCP 的出口纳入
审计；如果净化时把它们一起清掉，ADR-0023 会当场失效，而且失败是静默的（server 直连成功，
审计里什么都没有）。具体白名单的完整定义与 `instructions` 的处置见 ADR-0068，本文只钉
「同源」这条不变式。

### 6. 逃逸阀：`sandbox = "off"` 保留

`[mcp.servers] sandbox = "off"` 让**这一个** server 不进 OS 沙箱。这是显式 opt-out，
不是错误回退——`Sandbox::command` 的既有契约是「实现可以拒绝，调用方不得在出错后回落到
不受限命令」，本条款不违反它：拒绝仍然拒绝，只有配置里明写 `off` 才不套。

代价必须可见：启动时 `warn` 一条、`qc doctor` 把该 server 标 `partial`、事件日志记一次。
默认值永远是 `confine`。

### 7. 报错要说清楚是哪条路径

`Permission denied` 直接透出是这次最难查的地方。改为：沙箱拒绝时把**被拒的路径**和
**建议加的 `read_paths`**一起报出来，`qc doctor` 另给一行「运行时目录不在授权集内」
（今天的行为是 `warn` + `skipping MCP server`，一句轻描淡写就把 server 丢了）。

### 8. 配置形状

```toml
[mcp]
# "confine"（默认）| "off"——后者只影响没单独写的 server
sandbox = "confine"

[[mcp.servers]]
name = "wordcraft"
command = "/home/q/cot/lib/wordcraft_0.15.2_linux_amd64/bin/wordcraft"
# 自动推导之外的补充（只读 + 执行）
read_paths = ["/home/q/cot/lib/wordcraft_0.15.2_linux_amd64"]
# 默认已有 workspace + 私有 HOME；这里补其它需要写的目录
writable_paths = []
# 语义化的凭据授权
credentials = ["github"]

[[mcp.servers]]
name = "legacy"
command = "/opt/legacy/mcp"
sandbox = "off"        # warn + doctor partial
```

**不变式**

- 授权集是可打印的纯数据；spawn 只消费它，不在 spawn 路径上重新推导。
- 补不上就不启动：推导失败一律拒绝，唯一例外是显式 `sandbox = "off"`。
- `HOME`（以及 `XDG_*`、`TMPDIR`）指向的目录必须在 write 集合里，否则就是
  「env 说 A、只授权 B」——这是最难查的一类错。
- 真实 `$HOME` 不出现在子进程环境里（`sandbox = "off"` 除外，此时 `HOME` 仍是真实的）。
- Windows 的 `windows-acl` 后端**只限写**，读不受限（ADR-0027）。因此凭据预设在那里的
  效果是「写不进去」，不是「读不出来」；文档与 `doctor` 必须如实写，不能假装三平台一样。

**实现**

| 文件 | 改动 |
|---|---|
| `crates/qcode-mcp/src/grant.rs`（新） | `GrantSet::derive(command, args, read_paths, credentials) -> Result<GrantSet, GrantError>`：canonicalize、shebang、生态根、危险根检查。纯逻辑，可单测，不依赖 `qcode-tools` |
| `crates/qcode-mcp/src/config.rs` | `McpServerConfig` 加 `read_paths` / `writable_paths` / `credentials` / `sandbox`；`McpConfig` 加 `sandbox`；`validate()` 加「`off` 与 `confine` 之外的取值报错」 |
| `crates/qcode-mcp/src/lib.rs` | `Confine` 回调加一个 `&GrantSet` 参数；`connect_all_with` 每 server 先 `derive`，再按 grant 设环境与 HOME，最后交给 `confine`。crate 仍然不依赖 `qcode-tools`（回调而非类型依赖） |
| `crates/qcode-kernel/src/lib.rs` | 闭包从「一个 `confine_sandbox` 套全部 server」改为按 grant 现算：`sandbox.with_roots(任务 roots + runtime 只读根, 私有 temp)` |
| `crates/qcode-tools/src/sandbox/mod.rs` | ~~**不改**。`with_roots(&[Root], temp)` 已经支持只读根，Linux 侧 `RootAccess::ReadOnly` 只给读位、不给写位——正好是这里要的~~ → **改了**：`with_roots` 多了第三个参数 `OutsideRoots`。原判断「不改」错在只看了「有没有只读根」，没看「多给了什么」——见下方 §1 写集修订注记 |
| `crates/qcode/src/commands/*` | `qc mcp add` 加 `--read-path`、`--write-path`、`--credential`（可重复）、`--sandbox`；`qc doctor` 逐 server 打印授权集 |

**后果**

- 「装即信」的前提变硬了：以前装上去就能跑（如果路径恰好落在白名单里），现在**必须**声明
  凭据。这是有意的——读不到 `~/.ssh` 是功能，不是缺陷。
- 合成 HOME 让 server 的状态与真实配置隔离：同一个 server 在 qcode 里和手跑一遍看到的
  cache 可能不同。doctor 要能打印私有 HOME 的路径，用户才知道去哪儿看。
- 首次使用需要一次「推导 → 报错 → 加 `read_paths`」的来回。设计上接受这个来回，因为它只
  发生在**装新 server** 时；对比今天，是每次都要把 server 挪到 `/tmp`。
- `[sandbox] extra_writable` 是 `windows-acl` 专用的（`crates/qcode-kernel/src/config/mod.rs`
  的文档注释就写着），Linux 下它根本不生效——这也是为什么「已经加了 `extra_writable`
  还是 `Permission denied`」。本 ADR 不扩展它的语义，MCP 的授权走 `read_paths` /
  `writable_paths`，`extra_writable` 保持只服务 `run_shell`。
  **（已过时，见 §2 修订注记 2026-11-11 补记）** 最后半句是对的——它仍然只服务
  `run_shell`，MCP 走自己的授权；但前半句把「Linux 后端丢弃这个列表」当成了设计，
  而那是 `let _ = extra_writable` 的副作用。所谓「已经加了 `extra_writable` 还是
  `Permission denied`」不是用户该预期的行为，是 bug。四个后端现在都认它。
- 远程 MCP（Streamable HTTP，另篇）没有子进程，本 ADR 完全不适用；它的边界是出口代理。

**验证**

- 单测（纯逻辑，无沙箱）：推导表逐行覆盖——shebang 的 `env node` 形态、venv、
  `node_modules/.bin`、单文件、无 shebang；危险根（`/`、`$HOME`、workspace 祖先）逐个报错；
  `sandbox = "off"` 时 grant 仍被算出来（doctor 要打印）。
- 集成（Linux，真 Landlock）：把 stub server 装在 `$HOME` 下的临时目录 →
  **(a)** 默认配置能启动并列出工具（今天会 `Permission denied`）；
  **(b)** 写 workspace 外被拒；
  **(c)** 读 `~/.ssh/id_rsa` 被拒；
  **(d)** npx 形态的 server 首次运行能建 cache（合成 HOME 生效）。
- 真机：`/home/q/cot/lib/wordcraft_0.15.2_linux_amd64/bin/wordcraft` 原址，不拷 `/tmp`，
  19 个工具。
- macOS / Windows：`extra_writable` 语义不变；Windows 只验「写不进去」那一条，并在
  doctor 里如实标 `partial`。

**分期**

| 期 | 内容 | 验收 |
|---|---|---|
| P1 | `grant.rs` 推导 + 合成 HOME + 逐 server `with_roots` + 可诊断报错 + `sandbox = "off"` | wordcraft 原址可用；`README` §沙箱那段（「`run_shell` 与 MCP stdio server 都由对应工具包裹」「`npx` 的 cache 要加进 `extra_writable`」）按新授权集改写 |
| P2 ✅ | `credentials` 预设与合成 HOME 里的符号链接 + `qc doctor` 打印授权集 | `credentials = ["github"]` 后 gh 类 server 可用；不带预设时读不到 |
| P3 | `qc mcp add` 的新 flag 与写回 | `add` 写的配置能被 `show --effective` 读回，字段不丢 |

**备选方案（未采用）**

| 方案 | 为什么不做 |
|---|---|
| 撤沙箱（回到 §4.10 原文） | server 无人值守读写 `$HOME`；审批覆盖不到握手期（见「被否掉的三个方向」1） |
| `$HOME` 只读 | 等于给每个 server 一张凭据通读证（同上 2） |
| 只暴露 `read_paths`，不推导 | 首次上手必错，问题只是换了位置（同上 3） |
| 用 bwrap 而不是 Landlock | bwrap 要建挂载命名空间，与「复用 `run_shell` 的后端」冲突；且 macOS/Windows 还是得各写一套 |
| 逃逸阀只做 CLI flag、配置里不让写 | 用户明确要求保留在配置里：装新 server 的来回要能在版本化的项目配置里表达 |
| 给每个 server 一份真实 `$HOME` 的 bind | 隔离归零，等于方案 1 |

**实现注记（2026-11-11，P1 + P2 的一部分）**

落地的是：`grant.rs`（推导 / 危险根 / 合成 HOME / 凭据链接）、`config.rs` 的四个新字段与
`sandbox` 枚举、`lib.rs` 的 `env_clear` + 白名单 + 逐 server inline `GrantSet`、
`qcode-kernel` 的 `mcp_confine` + `mcp_roots`、以及 `qc mcp add --read-path/--write-path/
--credential/--sandbox`。五处与本文原文有偏差，都是实现时撞出来的：

1. **授权以目录为单位，不是文件为单位。** §2 的推导表写着「`<target>/debug/<x>`
   只给这一个文件」。`Sandbox::with_roots` 只吃 `Root`（目录），改它要动三个后端；而
   `<target>/debug` 这一层已经足够窄，且二进制常要读同目录的 `.so`。所以统一按目录授权，
   §2 那句按「只给该文件所在目录」理解。
2. **Linux 后端本来就允许写 `/tmp` 与 `/var/tmp`。** `sandbox/linux.rs` 的 `install_rules`
   无条件把这两个和 `writable_paths()`（`CARGO_HOME` 等）加成可写。所以本 ADR 说的
   「写只给 workspace + 私有 HOME」准确说法是「workspace + 私有 HOME + `/tmp` + `/var/tmp`
   + cargo 目录」。这是既有的、与 `run_shell` 共享的行为，本 ADR 没有收紧它；要收紧是另一件事。
   它同时意味着**测试不能用 `tempfile` 目录冒充「workspace 之外」**——`/tmp` 本来就是可写的，
   断言会假绿。集成测试因此改用 `CARGO_TARGET_TMPDIR`（仓库 `target/` 下）。
3. **`Sandbox::command` 返回值依赖 sandbox 还活着。** `Landlock::command` 的 `pre_exec`
   捕获 `ruleset_fd`，而 `Drop for Landlock` 会 `close` 它。原来的写法
   `with_roots(..)?.command(..)` 一行表达式会在 `spawn()` 之前 drop 掉 sandbox，子进程
   于是死在 `exec` 前，报 `EBADFD`（`os error 77`）——看起来像权限问题，其实不是。
   `run_shell` 一直把 `with_roots` 的结果绑在局部变量上，所以没暴露。修法两处：给 trait 的
   `command` 补上「sandbox 必须比 `spawn()` 活得久」的文档，以及让 `mcp_confine` 把每个
   server 的 sandbox 停在 `Confine` 盒子里（生命周期覆盖 `connect_all_with` 的全部 spawn）。
4. **`XDG_CONFIG_HOME` 必须与凭据链接的位置一致。** 第一版把 `XDG_CONFIG_HOME` 指向
   `<home>/config`、把凭据链接放在 `<home>/.config/gh`，于是 `gh` 会去 `$XDG_CONFIG_HOME/gh`
   找 token 而找不到——`credentials = ["github"]` 成了一个什么都不做的选项。现在三处统一用
   XDG 的默认布局（`.config` / `.cache` / `.local/share`）。
5. **私有 HOME 必须先 `canonicalize`。** `--data-dir ./data` 这种相对路径会让 `HOME` 变成
   相对路径，server 起来后行为不可预期（实测就是握手直接失败）。`derive` 现在建目录并
   canonicalize，失败即 `GrantError::HomeUnavailable`。

另外两条不属于偏差但值得记：

- **GUI 设置页会清空新字段，如果不管它。** `qcode-gui/src/setup/save.rs` 的
  `apply_tables` 先 `servers.clear()` 再按表格重建，表格里没有授权集这几列。当时按
  `providers[..].headers` 的既有做法从 `self.config` 里按名字取回，否则用户每次在设置页
  点保存都会静默丢掉 `read_paths`。**（已由下面的 P2 收尾注记取代：列补上了，取回逻辑随之
  删除。）**
- **真机验证的结果与预期不同。** 在 `/home/q/.local/lib/qc-probe/bin/probe`（一个真实的
  `$HOME` 下安装位置）上，默认配置能启动并注册 3 个工具——这正是要修的失败类。
  但 `wordcraft 0.15.2` 的 `bin/wordcraft` 是**桌面应用**（`--control PORT` 才是给 agent 用的
  接口），不是 MCP server：给它 `DISPLAY`（经 `env = { DISPLAY = "${DISPLAY}" }`，白名单本来
  就不传它）之后它自己起来了、日志里能看到启动横幅和走代理的更新检查，但永远不回
  `initialize`。所以「wordcraft 原址 19 个工具」这条验收**在本机无法成立**，原因不在沙箱：
  它根本没实现 MCP。

**P2 收尾注记（2026-11-11，授权集进设置页的表格）**

P2 的验收是「`qc doctor` 打印授权集」，但同一份授权集在 GUI 设置页上是一直是**只读**的：
表格没有这四列，保存时靠「按名字从旧 config 里取回」把它们硬扛过去。能跑，但两件事不对：
用户改不了 `read_paths`（只能去手写 TOML），而且「界面上看得到的值」与「实际生效的值」是
两个不同的东西——正是本 ADR §1 那句「不允许文档说 A、实际给 B」要消灭的形态。

- **四列**：`read_paths`、`writable_paths`、`credentials`、`sandbox`。前三列是 `Lines`
  （一行一项），`is_lines_cell` 同步更新——单行 `Input` 会静默吃掉 `\n`，把两项粘成一项，
  这条有既有单测把 `FieldKind::Lines` 与 `is_lines_cell` 强制对齐。
- **`sandbox` 是三态而不是两态**：`inherit` / `confine` / `off`。字段是 `Option`，写死成
  显式 `confine` 会让这个 server 从此不跟随 `[mcp] sandbox` 的全局开关——用户在全局关掉
  沙箱、某个 server 却还在 confine 里，且界面上看不出来。`inherit` 就是 `None`。
- **取回逻辑删掉了**。列与「从文件复制」并存，会让可见值静默输给不可见值。留着的两处取回
  现在只有 `bearer_token_env_var` 与 `headers`（ADR-0067 P3 还没做列），并在注释里点名。
- **映射抽成 `mcp_server_from_cells(McpRowCells, previous)`**，纯函数、不需要窗口。原来这段
  逻辑埋在 `apply_tables` 里，而 gpui 的表格行要 `Entity<InputState>`，没有测试可以构造——
  也就是说「设置页写出的配置」这条路此前一条测试都覆盖不到。参数收进 `McpRowCells` 结构体
  不只是为了过 clippy 的 7 参数上限：`read_paths`/`writable_paths` 同类型、`args`/`env`/
  `credentials` 同类型，位置参数调换能编过，只会表现为「server 读得到、写不了」。
- **`CredentialPreset::from_label` / `McpSandboxMode::from_label` / `::ALL` 移进
  `qcode-mcp`**。此前 CLI 的 `--credential` 在 `qcode/src/commands/settings.rs` 里手写了一份
  六分支 match，GUI 再写一份就是第三份。现在三处（CLI flag、GUI 选择器、配置文件）共用同一
  张表，错误消息也从 `ALL` 生成。认不出的名字返回 `None` 而不是猜：把拼错的名字当成
  `github` 会实打实地多给一个凭据子树。
- **测试 6 条**：`mcp_server_from_cells` 的完整映射（含 `env` 里没有 `=` 的行被丢弃、名字两端
  空格被裁、空名字不成行）；无列字段的取回与「原有远程 server 不会被改成 stdio」；GUI 侧
  `credentials`/`sandbox` 单元格的解析（未知名字不写进配置、`inherit` → `None`）；保存后
  重新加载的往返（走 `write_config` + `config::load`，即真的落盘再读回，而不是拿 `Config`
  跟自己比）；`inherit` 不写出 `sandbox` 键；以及一条结构性断言——**四个授权字段必须都有
  列**，这条钉的是「以后加字段忘了加列 → 保存即静默删除」，也就是当初那个取回 hack 存在的
  理由。
- 变异验证两次：`read_paths` 不从行里读 → 映射测试失败；删掉 `credentials` 列 → 列覆盖
  测试失败并打印现有的列名。

**§1 写集修订注记（2026-11-11，MCP 的写集严格等于那三个集合）**

§1 写的是「三个不相交的集合，**此外一律无**」。实现没有做到，而且是双向的偏差：

- **文档窄、代码宽。** `with_roots` 重建的沙箱仍然带着 `run_shell` 的两项额外授权：
  Linux 侧 `install_rules()` 无条件给 `/tmp` 和 `/var/tmp` 写位，再无条件并上
  `writable_paths()`（`$CARGO_HOME`、`$RUSTUP_HOME`、`$CARGO_TARGET_DIR`、`~/.cargo`、
  `~/.rustup`）；macOS 侧同样把 `/private/tmp`、`/var/folders`、那几个 cache 变量和
  `$TMPDIR` 都写进 `(allow file-write*)`。这些与 server 的 grant **无关**——同一个 `Sandbox`
  实例既服务 `run_shell`（那里它们是对的，编译器要写 cache），也服务 MCP（那里它们不该存在）。
- **最重的一条是可写 `$CARGO_HOME/config.toml`。** 那个文件里有 `[build] rustc-wrapper`，
  也就是**下次 `cargo build` 时执行任意代码**。一个 `npm i` 或 `pip install` 装来的第三方
  server 能改它，这不是「多给了一个临时目录」。

实测（wordcraft-cli 0.15.2，本机 Landlock）：MCP 服务进程写 `/tmp/qc-wc/outside/` 成功、
写 `$CARGO_HOME/` 成功、写 `/home/q/space/opentj/qc-escape-probe/` 被拒。也就是说沙箱本身
有效，只是**多给了两处不该给的**。

**决定**

`Sandbox::with_roots` 多一个参数 `OutsideRoots`，两个取值：

| 取值 | 谁用 | 含义 |
|---|---|---|
| `BuildCaches` | `run_shell` | roots + 构建缓存 + **宿主** `/tmp` |
| `Nothing` | MCP server | 只有 roots |

必须是**调用方显式写**，不给默认值：这个量同时服务两个需求相反的调用方，任何默认都会让其中
一个静默错下去。三个后端同一语义：

- **Landlock**：`Nothing` 时不加 `/tmp`、`/var/tmp`，也不并 `writable_paths()`。
- **bwrap**：`Nothing` 时不 bind 任何 cache，也不再把 `TMPDIR` 强设成 `/tmp`（那样会把
  §3 的私有 HOME 顶掉）。**私有的 `--tmpfs /tmp` 保留**——它不是宿主的 `/tmp`，走了也
  不会落到宿主上，而按 `/tmp` 硬编码的运行时（Node、部分 Python 包）离开它会直接崩。
  这一条是有意的不对称：宿主的 `/tmp` 是洞，私有的 tmpfs 不是。
- **macOS**：`Nothing` 时写块里去掉 `/private/tmp`、`/tmp`、`/var/folders` 与那几个 cache
  变量；**读**权限保留（二进制可能就住在 `$CARGO_HOME` 下，去掉会变成另一种「server 起不来」）。
- **windows-acl**：本来就只给「可写 roots + temp」，从来没有 cache 授权，语义上已经是
  `Nothing`。参数照样接受，只是 `debug_assert` 一句，免得第四个后端将来悄悄不同意。

三个后端的 `describe()` 现在都带上写集（`writes: roots only` / `roots + build caches +
host /tmp`）。理由同 §1 那条不变式：`doctor` 打印的东西必须就是实际生效的东西，
只说「landlock」而不说给了哪些写权限，等于把这次的 bug 又留一遍。

**为什么不给 MCP 留 `/tmp`**

§3 已经把 `TMPDIR`/`TMP`/`TEMP` 指到私有 HOME，而私有 HOME **本身就在 write 集合里**
（实测 server 进程的 `TMPDIR` 确实是 `<data_dir>/mcp/<server>/home`）。所以「临时目录不够用」
这个理由在这里不成立。真有个 server 硬编码 `/tmp`，正确做法是它自己的
`writable_paths = ["/tmp/<its-own-dir>"]`——显式的、能 diff 的、只给它一个子目录，
而不是替所有 server 默认打开宿主的 `/tmp`。

**验证**

- 真机单测（Landlock，同一份 kernel 跑两次 `with_roots`）：`BuildCaches` 能写 `/tmp` 与
  `$CARGO_HOME/config.toml`，`Nothing` 两者都 EACCES。之前这里是**测不出来**的：既有测试
  `writes_and_reads_stop_at_the_grant` 的注释写着「`tempfile` 在 `/tmp`，而 Landlock 无条件
  授权 `/tmp` 和 `/var/tmp`，所以『外面』得挑别处」——测试绕开了被 bug 覆盖的那块，于是
  bug 一直在。现在那条测试把 `/tmp` 也断言进去，并把这段注释改写成它真正想说的话。
- bwrap 参数断言（两个取值各一条，且互相为对照）：`Nothing` 没有 cache 的 `--bind`、没有
  `--setenv TMPDIR`，但仍有私有 `--tmpfs /tmp`。
- macOS profile 断言：拿整段 `(allow file-write* ...)` 逐个路径比对（第一版把 `)\n` 当成
  块结束，只截到第一个 `subpath`，是测试自己的 bug，靠「该出现的路径没出现」发现的）。
- 端到端（真 Landlock + 真子进程 + 真 `tools/call`）：`$CARGO_HOME/config.toml` 写不进去。
- 变异验证四次：kernel 改回 `BuildCaches` → 端到端两条同时失败；Landlock 改成无条件授权 →
  两条真机单测失败；macOS 改成无条件授权 → profile 断言失败。
- **`run_shell` 未受影响**：`BuildCaches` 是它的老路径，全量 1021 条测试里 shell 相关全绿；
  真机上 `qc` 的 `mcp__wordcraft__*` 仍然 19 个工具、`new_document` + `save_document` 写出
  6531 字节的真 `.docx`（`file` 认作 Microsoft Word 2007+）。

**待定**

- `credentials` 预设表要不要可扩展（用户自定义 preset），还是只做固定的这几条。
- 合成 HOME 里的凭据符号链接在 macOS `sandbox-exec` 下是否需要显式放行 target（`file-read`
  按解析后路径匹配，预期够用，需实测）。
- ~~`writable_paths` 指向真实 `$HOME` 的某处（例如某个 server 的日志目录）时要不要 warn。~~
  **已做（2026-11-11）**：不是 warn 而是拒绝，见下面的修订注记——`~` 展开后 `["~/"]` 与
  `["~/Documents"]` 只差两个字符，正是需要拦的形状。
- **已做（2026-11-11）**：`qc doctor` 现在有 MCP 段，逐 server 一行打印授权集，并把四个
  「配置了但没用」的情形变成 warn：`read_paths`/`writable_paths` 指向不存在的路径、
  `credentials` 预设在本机无对应文件、`sandbox = "off"`、以及 `[sandbox] mode = "none"`
  时 `[mcp] sandbox = "confine"` 形同虚设（最后这条是写 doctor 时才发现的：内核用
  `[sandbox] mode` 造 MCP 沙箱，`[mcp] sandbox` 单独开不出来，doctor 若照配置念
  「confine」就是这次要消灭的那种「文档说一套、代码做一套」）。这些一律用 warn 而不是
  fail：起不来的 server 只丢自己的工具，会话照常，doctor 不该因此退出非零。
  `describe()` 的凭据段同时改成「已授予的 `~/...` + 未命中的 `preset(not found)`」，
  以便区分「要了」与「拿到了」。
- ~~`qc doctor` 还没有能力探测的段（`discover` / `resources/list` 是否真的通）：那要等
  ADR-0067 的传输层落地，现在 doctor 只做静态检查，不连 server。~~ **已做（2026-11-11）**：
  `qc doctor --probe` 真连、真问、并把「声明与实际是否一致」两个方向都报出来。默认仍是
  静态检查（连 server 会启动每个 stdio server、为远程 server 起出口代理，不该是默认动作）。
  见 ADR-0068 §4 的 B-2 修订注记。

## ADR-0067：MCP 远程传输与协议版本——Streamable HTTP，discover 优先并回退 initialize（2026-11-11）

**状态**：已实现（2026-11-11），P1 / P2 / P3 全部完成。
`[[mcp.servers]]` 的 `command` 由必填改为「与 `url` 恰好二选一」；老配置
一行不改继续可用。协议版本一次对齐到 2026-07-28，但保留 `initialize` 回退——理由见 §3。

**来源**：`derived`（现有代码与 rmcp 3.9.0 的实际 API）、`black-box`（规范正文与官方 Registry
的公开样本；按 SOURCES.md 只借事实，不借实现与文案）。

**背景**

### 1. 「v1 只做 stdio」的前提没了

ADR-0002 与 §5 选型都写着「`rmcp = "=3.2.0"`，v1 只用 stdio + tools，streamable HTTP 与
legacy SSE 不进 v1」。这条决定的隐含前提是「MCP server 都跑在本机」。事实不是这样：

- 官方 Registry（`registry.modelcontextprotocol.io`）抽样 100 条：**约 93 条只有 `remotes`**
  （`type = "streamable-http"`），纯 stdio 的只有 7 条。
- 六个真实端点用 GET 探活（规范要求 GET 到 Streamable HTTP 端点返回 `text/event-stream`
  或 **405**）：`mcp.context7.com/mcp`、`api.aard.ai/mcp`、`mizan.ac/mcp`、
  `events.aero/mcp/events`、`api.adadvisor.ai/mcp` 都是 405 + JSON-RPC 错误体；
  `mcp.snag.ac/mcp` 是 404 `Cannot GET /mcp`（活，但不合规）。
- qcode 今天的表现：配置里写 `url = "..."` 报 `missing field 'command'`——`command` 是
  `String`（`crates/qcode-mcp/src/config.rs:23`）而不是 `Option`。

也就是说：**按今天的实现，绝大多数已登记的远程 server 装不上**，而「只做 stdio」是 2026-10-19
的决定，那时还没有这条样本。

### 2. 开 HTTP feature 会重新打开三条 RUSTSEC

`Cargo.toml` 里那次 3.2.0 → 3.9.0 的 bump 是为 RUSTSEC-2026-0457 / 0461 / 0462，注释写得很清楚：

> all three are Streamable-HTTP / OAuth paths; this crate only enables `client` +
> `transport-child-process`, so they were never reachable here

**本 ADR 正好要让它们可达。** 所以「bump 过了」不等于「安全」——这三条要在 3.9.0 上重新过
`cargo deny`，并且 OAuth 相关的那条（如果修法是把 OAuth 挪到 `auth` feature 之后）要确认我们
不开 `auth` feature 时那条路径确实关着。这是本 ADR 的**前置条件**，不是后续事项。

**已核（2026-11-11）**：前置条件闭合，三条分别落位——

| 告警 | 结论 |
|---|---|
| RUSTSEC-2026-0457（OAuth client SSRF） | 不可达。代码在 `#[cfg(feature = "auth")]` 后（`transport.rs:96`），而 `reqwest` feature 只展开 `["__reqwest", "reqwest?/rustls"]`，不牵 `auth`。`cargo tree -e features` 确认该 feature 未开 |
| RUSTSEC-2026-0461（服务端 HTTP transport 的无界工具缓存） | 不可达。在 `transport-streamable-http-server` / `server-side-http` 后，我们只开 `transport-streamable-http-client*`，`server` 也未开 |
| RUSTSEC-2026-0462（客户端响应体无界缓冲） | **可达，且已修**。9.0.0 起 JSON 体限 16 MiB、非成功体限 64 KiB。所以 `rmcp = "=3.9.0"` 这个精确 pin 从此是有承载力的：降级即重新打开一个可达的 DoS |

`cargo deny check`（advisories/bans/licenses/sources）在开 HTTP feature 之后复跑通过，
三条都不在 `ignore` 列表里——它们是被版本和 feature 选择排除的，不是被静音掉的。

### 3. 协议版本换代，而且不能单轨

2026-07-28 是破坏性的：删掉 `initialize` 握手、删掉 session、新增 `server/discover`、
`resultType`、`ttlMs` / `cacheScope`（SEP-2549）、MRTR；废弃 Roots / Sampling / Logging。

但存量不是。实测 wordcraft 停在 **2025-06-18**，`server/discover` 回 `-32601`。

于是只有三种可能：

| 方案 | 后果 |
|---|---|
| 只走 `initialize` | wordcraft 能用，但拿不到 `ttlMs`/无状态；新 server 也还是慢（多一个握手往返） |
| 只走 `discover` | wordcraft 直接不可用 |
| **探测 + 回退** | 两者都能用，代价是 legacy 多一次探测往返 |

第三种是规范自己在向后兼容指引里给的路径，而且 rmcp 3.9.0 已经把它做成一个枚举——
`ClientLifecycleMode::{Initialize, Discover, Auto}`，其中 `Auto { preferred_versions,
legacy_version }` 就是「先探测 `server/discover`，对端报 legacy 或在
`DEFAULT_AUTO_DISCOVER_TIMEOUT`（10s）内不回则回退 initialize」。**双轨是库给的，不是我们造的。**

**决定**

### 1. 传输二选一

`[[mcp.servers]]` 的 `command` 改为 `Option<String>`，新增 `url`；`validate()` 要求两者
**恰好一个**，都没有、或都有，都是配置错误。

`command` 变 `Option` 是源码级 breaking（`qcode-mcp` 的公开字段类型变了），配置层不破坏：
老配置只写 `command`，新代码照旧。是否因此 bump 到 v0.9.0 由发布时决定，与本 ADR 的语义无关。

### 2. HTTP client 必须来自 `Network::client()`

`StreamableHttpClientTransport` 有两个构造入口：

- `new_simple(url)` —— 内部用 `C::default()` 造 reqwest client；
- `new(client, config)` —— 用调用方给的 client。

**只用后者。** 用前者等于让 MCP 的 HTTP 直接出网：ADR-0023 §2 的名单今天是
「provider、`web_fetch`、`web_search`」三项，MCP 一旦走进程内 HTTP 而不在名单里，就是
**完全绕过 egress allowlist、`network.jsonl` 审计与 SSRF 私网拦截**——而且是静默的，
唯一症状是「审计文件里少了这些请求」。

这条同时要求 `qcode-mcp` 依赖 `qcode-core` 的 `Network` trait（已有的
`crates/qcode-core/src/network.rs`），或者由 CLI 传一个 `reqwest::Client` 进去。选后者更贴合
本 crate 现有的「回调而非类型依赖」风格（`Confine` 就是这么做的）：新增
`HttpClient = Arc<reqwest::Client>` 参数，与 `Confine` 并列。

### 3. 协议版本：`Auto`，首选 2026-07-28，回退 `initialize`

```rust
ClientLifecycleMode::Auto {
    preferred_versions: vec![ProtocolVersion::V_2026_07_28],
    legacy_version: Some(ProtocolVersion::V_2025_11_25),
}
```

`legacy_version` 取 **`V_2025_11_25`** 而不是 `V_2025_06_18`：rmcp 把
`LATEST_WITH_INITIALIZE` 定义成 `2025-11-25`——`initialize` 握手能回声的最高版本就是它，
再往后（`NO_INITIALIZE` = 2026-07-28）生命周期已经搬进每请求的 `_meta`，没有握手可答。
这只是**客户端在回退时声明的上限**；wordcraft 只支持到 2025-06-18，握手时按它自己的版本
协商成功，不受影响。

回退的两条触发路径（`Auto` 分支里各一条）：探测收到**可关联的非 modern JSON-RPC 错误**
（`DiscoverOutcome::Legacy`，`-32601` method not found 就是这一类），或探测在 10s 内无响应。
传输层错误与 modern 拒绝**不回退**，直接报错——这两种情况重试握手也是白费。

握手超时沿用 `HANDSHAKE_TIMEOUT_SECS = 20`；`Auto` 自己的探测超时是 10s，两者不冲突
（探测 10s 内没回即回退，回退再失败才计入 20s）。

### 4. 无状态与会话恢复

- `allow_stateless = true`：2026-07-28 没有 session，服务端可能不发 `Mcp-Session-Id`。
  关掉这个选项会让无状态 server 在 3.9.0 的 transport 里被判为「会话没建立」。
- `reinit_on_expired_session = true`：legacy server 的 session 会过期（HTTP 404），
  3.9.0 自带一次重建 + 重试。这是**必需的**，否则长会话里 server 一重启 qcode 就废了。

`max_concurrent_requests` / `control_request_timeout` / `session_recovery_timeout` 用默认值；
`call_tool` 外层仍由我们自己的 `timeout_secs` 包一层（ADR-0002 §3 不变）。

### 5. `ttlMs` / `cacheScope` / `list_changed`：工具表变成有寿命的

这条是**唯一一处必须改架构的地方**，也是 ADR-0002 后果段被作废的原因。

2026-07-28 规定可缓存结果带 `ttlMs` 与 `cacheScope`；`tools/list` 的结果是其中之一。
词义：

| `ttlMs` | 含义 | 我们的做法 |
|---|---|---|
| 缺失（legacy） | 没有寿命信息 | 进程生命周期内不重取（等于今天的行为） |
| `0` | 不可缓存 | 每个 turn 边界重取 |
| `n > 0` | `n` 毫秒内新鲜 | 过期后下一个 turn 边界重取 |

另外 `notifications/tools/list_changed` 一到，**立即**把该 server 的缓存标脏。

两个实现细节，都是踩过的坑：

1. **不能用 `list_all_tools()`。** 它返回 `Vec<Tool>`，把 `ListToolsResult` 里的
   `ttl_ms` / `cache_scope` / `result_type` 全丢了（`PaginatedResult` 宏给的是
   `ttl_ms: Option<u64>`，而 `list_all_tools` 只 `tools.extend(result.tools)`）。要自己
   循环 `list_tools()` 并保留第一页的 `ttl_ms`。
2. **工具表不能中途换。** 它决定了发给模型的工具定义，正在跑的 turn 里换掉会让模型看到
   一套、执行时是另一套。所以缓存失效**只在 turn 边界生效**，并且发一个事件
   （`McpToolsRefreshed { server, added, removed }`）。已经消失的工具被调用 → 明确的
   工具错误（「该工具在上一个 turn 后不存在了」），不是 panic，也不是静默重试。

`cacheScope = "public"` 在我们这里没有额外含义：qcode 没有跨进程的工具缓存，所以
`public` 与 `private` 行为相同。写明这点，避免下一个人以为漏了实现。

### 6. 认证只从环境取

新增 `bearer_token_env_var`（变量名，不是值）。`headers` 只允许非敏感头，`validate()` 拒绝
名字匹配 `authorization|cookie|token|key|secret|password` 的 header。

这条是给 ADR-0018「密钥从不落盘，因此无需额外脱敏」补前提：那条成立的前提是「没有地方能
写密钥」，而 HTTP 的 header 表就是这样一个地方。与 ADR-0056 的立场一致。

### 7. 配置形状

```toml
[[mcp.servers]]
name = "context7"
url = "https://mcp.context7.com/mcp"
bearer_token_env_var = "CONTEXT7_TOKEN"   # 可选

# 可选；只放非敏感头，敏感的一律走 bearer_token_env_var
[mcp.servers.headers]
Accept = "application/json, text/event-stream"

[[mcp.servers]]
name = "wordcraft"
command = "/home/q/cot/lib/wordcraft_0.15.2_linux_amd64/bin/wordcraft"   # 原样，不受影响
```

`qc mcp add` 加 `--url`、`--bearer-token-env`、`--header K:V`（可重复）。与 `--command`
互斥，由 clap 的 `conflicts_with` 表达，`validate()` 再查一遍（TOML 边界比 CLI 宽）。

`url` 与 `sandbox = "off"` 同时出现是配置错误：远程 server 没有子进程，ADR-0066 的
`GrantSet` 不适用，`off` 是空操作——静默接受会让用户以为关掉了什么。

### 8. `qc doctor` 打印协商结果

逐 server 一行：传输（stdio / streamable-http）、协商到的协议版本、`ttlMs`、
以及 HTTP 是否经代理。最后一项是 §2 的可见性保障：用户能自己确认远端流量确实被审计了。

**后果**

- **启动延迟**：远程 server 不再有进程 spawn，通常比 stdio 快；但 legacy server 多一次
  `discover` 探测往返（10s 上限内）。ADR-0002 后果段「随 enabled server 数量线性增长」
  仍然成立。
- **工具表动态化**是本 ADR 最贵的改动：`build_registry` 之后的工具集合不再是常量，
  引擎需要在 turn 边界比对。ADR-0002 的「只列一次」正式作废。
- **`resultType` 与 MRTR**：2026-07-28 的 `tools/call` 可能返回 `InputRequired`
  （多轮往返），而 §4.10 已定「elicitation 不用于审批」。本 ADR 的做法是：`InputRequired`
  → 工具错误，说明「该 server 要求多轮往返，qcode 不支持」，并把原始 payload 记进日志。
  不静默重试，不当作空结果。
- **远程 server 的安全边界只剩出口代理**。「server 是第三方代码」这一点没变，但约束手段从
  文件系统换成了 host allowlist；没进 allowlist 的主机在 `unknown = "ask"` 下对 MCP 是
  **直接拒绝**（ADR-0023 的实现状态注记：shell/MCP 没有交互式 ask）。首次配一个远程 server
  要手写 `[network] allow`，这一点必须写进文档。
- RUSTSEC-2026-0457/0461/0462 重新进入可达面，`cargo deny` 必须复跑（§2 的前置条件）。

**验证**

- 本地合规 probe server（Python，`uvicorn` 一个最小 Streamable HTTP 实现，`--features` 无关）
  → `discover` 成功、走 2026-07-28、`tools/list` 带 `ttlMs`。**不依赖外网**，进
  `crates/qcode-mcp/tests/`——现有测试目录只有 `mcp_stdio.rs` 与 `engine_integration.rs`，
  HTTP 侧是空的。
- 只认 `initialize` 的 stub（wordcraft 形态）→ `Auto` 回退成功，`qc doctor` 显示 2025-06-18。
- 缓存三条：`ttlMs = 0` 每个 turn 重取、`ttlMs = 5000` 五秒内不重取、
  `list_changed` 立即失效且只在 turn 边界生效（用 `MockProvider` 跑两个 turn 断言）。
- 出口代理：probe server 起在 127.0.0.1，测试配置 `allow_private`（否则被 SSRF 拦截挡在
  更前面，测不到代理），断言 `network.jsonl` 里有这条 CONNECT；
  另加一条**负向**测试——把 `Network::client()` 换成默认 client 时这条断言必须失败
  （证明测试真的在测代理，而不是恰好都通）。
- 401：断言错误里带 `WWW-Authenticate`，不是一句 `request failed`。

**分期**

| 期 | 内容 | 验收 |
|---|---|---|
| P1 ✅ | `command`/`url` 校验 + `Network::client()` 接线 + `Auto` 生命周期 + doctor 打印 | probe server 直连可用；legacy server 回退成功；`network.jsonl` 有记录 |
| P2 ✅ | `ttlMs`/`cacheScope`/`list_changed` + turn 边界刷新 + `McpToolsRefreshed` 事件 | 三条缓存断言通过 |
| P3 ✅ | `qc mcp add --url` 与新 flag | 写回的配置能被 `mcp list` / `doctor` 读回 |

**P1 / P3 实现注记（2026-11-11）**

- **stdio 仍走 `Initialize`，不加 `Auto`。** 探测一个 stdio server 要冒的风险是不对称的：
  回 `-32601` 的会被识别为 legacy 并回退（可以），但 3.9.0 里对 `server/discover` 回一个
  合法 JSON-RPC 结果之外的响应会走 `ExpectedInitResult` —— 那是**硬错误，不回退**；而完全不
  回的 server 每次连接都要白等 10s 探测超时。`Auto` 目前只给远程用。
- **`describe()` 与 doctor 的「是否经代理」是三态**：`[network] enabled = false` 时没有代理，
  也就没有审计文件，这一行必须说「nothing is audited」而不是默认的「audited」。
- **doctor 不连 server。** §8 想要「协商到的协议版本」需要真连一次；那意味着在一条诊断命令里
  起全部 stdio server 或另起一个 egress 代理。折中是：doctor 只做静态判断（transport、是否
  经代理、host 是否在 allowlist、token 变量是否设置），**协商结果在启动日志里**
  （`mcp server ready protocol=2026-07-28`）。
- **`qc mcp list` 的分行按 transport 分开写。** 原来一行打印 `command={:?}`，远程 server 会
  显示成 `command=None`，读起来像一个坏掉的 stdio server。

**P2 实现注记（2026-11-11）**

- **「turn 边界」被实现为 `run_turn` 的入口，并且是快照。** `ToolRegistry::refreshed()` 返回
  一个**没有 source** 的克隆，整轮 turn 都用它；`ctx.tools` 仍是 `&ToolRegistry`，下一轮再问
  一次。这样 mid-turn 换表在类型层面就不可能，不需要额外的守卫。首个模型请求发出前的那次
  刷新就是边界本身。
- **`list_changed` 需要自定义 `ClientHandler`。** `()` 的默认实现把通知丢掉，所以我们serve
  时传 `ListChangedHandler`，它只做一件事：把一个 `Arc<AtomicBool>` 置脏。服务类型因此从
  `RunningService<RoleClient, ()>` 变成 `…<ListChangedHandler>`，`McpTool` / `McpExtra`
  改用统一的 `crate::Service` 别名。
- **刷新失败保留旧表。** SEP-2549 允许在 re-fetch 失败时继续用过期结果；把整个 server 的
  工具因为一次网络抖动清空，比表老一轮糟得多。dirty 位不清，下一轮再试。
- **`cacheScope` 明确不实现。** qcode 没有跨进程工具缓存，`public` 与 `private` 行为相同。
  这条写在 `finish()` 的日志注释里，避免下一个人以为漏了。
- **`rmcp` 自己的 `tools/list` 响应缓存与我们的 TTL 对齐**（同一个 `ttlMs` 同时驱动两边的
  `expires_at`），所以「我们判定过期 → 真的发出请求」之间不会出现假刷新。
- **分页自己写。** `Peer::list_all_tools` 返回 `Vec<Tool>`，把 `ttl_ms` / `cache_scope` /
  `result_type` 全丢了。现在按 `nextCursor` 循环，**只取第一页的 `ttlMs`**，并且遇到重复
  cursor 或第 64 页就停（一个不停换 cursor 的 server 否则能把 turn 预算耗光）。
- **stdio 侧的 `list_changed` 由 stub 触发**（`notifications/tools/list_changed` 走 stdin/stdout
  的 JSON 行），HTTP 侧要发 SSE 事件，P2 没有为它单独造 fixture；两条路径共用同一个
  `ClientHandler`，差别只在传输层。
- **GUI 的 MCP 表格只有一列 `command`**，远程 session 共用它（`server.command.or(url)`）。
  保存时按上一行的形态决定写回哪个字段，所以在一个远程 server 的单元格里打字不会把它悄悄变成
  stdio。`bearer_token_env_var` / `headers` 从文件原样保留——表格没有这两列，而
  `apply_config` 会整段重写 `[[mcp.servers]]`，忘记保留就是保存一次删一次。
- **两处配置写入都补全了 ADR-0066 的字段。** `apply_config` 之前只写 `command/args/env`，
  保存一次 GUI 设置就会把 `read_paths` / `credentials` / `sandbox` 抹掉；这个洞在本次改动里
  一并堵上（那是既有 bug，不是本 ADR 引入的）。

OAuth（Dynamic Client Registration / Client ID Metadata Documents）**不在本 ADR**：
2026-07-28 已把 DCR 标记废弃，改推 Client ID Metadata Documents，那是一个独立的认证流程，
单独立项，不要和传输搅在一起。

**备选方案（未采用）**

| 方案 | 为什么不做 |
|---|---|
| stdio→HTTP 桥接脚本 | 已验证可用（30 行 Python 转发 `initialize`/`tools/list`/`resources/list`/`prompts/list`），但每个 server 一个 wrapper、多一个子进程、多一层 ADR-0066 的沙箱授权，且把「远程」伪装成「本机」——排错时看到的是桥不是 server。只当过渡手段 |
| 2024-11-05 legacy SSE | 规范已废弃；`client-side-sse` 是被 `transport-streamable-http-client` 顺带引入的，不是我们主动支持 |
| 只走 `initialize`（不探测） | 拿不到 `ttlMs` 与无状态收益，且新 server 永远多一个握手往返 |
| 只走 `discover` | wordcraft 这类存量 server 直接不可用（实测 `-32601`） |
| `new_simple(url)` | 见 §2：静默造直连 client，绕过整个出口策略 |
| 工具表每个 turn 无条件重取 | 每个 turn 多一次往返；`ttlMs` 存在就是为了避免这个 |

**待定**

- `preferred_versions` 要不要同时列 `2025-11-25`。现在只列 2026-07-28，靠「server 对
  `discover` 报 -32601 → 回退」识别 legacy；但一个**实现了 `discover` 却在
  `supported_versions` 里只报到 2025-11-25** 的 server 会走 `select_protocol_version`
  返回 `None` → `NoCompatibleProtocolVersion`，而不是回退。这种 server 存不存在还不知道。
  P1 实现严格照此：只列 `2026-07-28`。真撞上这类 server 时，症状会是一条明确的手册错误，
  而不是静默降级——那正是先想要的行为。
- SSE 流式响应在 `max_sse_event_size`（默认值）之外要不要单独设上限——qcode 对工具输出有
  32KB 截断，但那是**结果**，不是传输层单帧。
- 同一个 server 的远程与本地形态并存时（例如既想 `npx` 又想 `url`）是否允许两条配置项，
  还是强制用户起两个名字。

## ADR-0068：MCP server 的环境白名单与自述注入（2026-11-11）

**状态**：P1 / P2 / P3 与 §4 已实现（2026-11-11）；§4 于同日按 B-2 注记修订为
「一律信握手声明、探测改为 `qc doctor --probe`」。
`[[mcp.servers]] env` 的语义收窄（新增 `${VAR}` 引用）；`instructions`
从「丢弃」改为「注入 system prompt」。承接 ADR-0066 §5（授权集与环境同源）留的接口。

**来源**：`derived`（实测泄漏、grep 证实零消费、ADR-0033 的探测逻辑已过时）；
`black-box`（只借「server 有自述、客户端应当转达」这一事实）。

**背景**

### 1. 每个 MCP server 继承 qcode 的全部环境

`crates/qcode-mcp/src/lib.rs:143-148` 只有两句 `envs`，没有 `env_clear`：

```rust
command
    .envs(server.env.iter())
    .envs(env.iter().map(|(key, value)| (key, value)))
```

对照 `crates/qcode-tools/src/shell.rs:258`——`run_shell` 是 `env_clear()` 之后按
`ENV_ALLOWLIST` 注入的。**同一个进程里，两条路径的边界不一样，而更宽松的那条跑的是第三方
代码。** 实测用一个 dump 包装器启动 server，子进程里数到 **119 个变量**，其中包含：

| 变量 | 为什么会在这里 |
|---|---|
| `GITHUB_TOKEN` | 用户在 shell 里 `export` 的 |
| `DEEPSEEK_API_KEY` | qcode 自己的 provider 密钥 |
| `SSH_AUTH_SOCK` | 登录会话给的，能直接签 git 提交 |

这不是理论风险：MCP server 是 `npm i` / `pip install` 来的，安装即信任（§4.10），但「信任它
做它声称的事」不等于「信任它无人值守地拿走凭据」。而且 ADR-0023 已经决定给 MCP 注入代理变量
——也就是说我们**已经在**管它的环境了，只是一直管得一塌糊涂：注入 4 个想要的，剩下 119 个
不想要的全跟着去。

### 2. server 的自述被整个丢掉

`InitializeResult.instructions`（`Option<String>`）和 2026-07-28 的
`DiscoverResult.instructions` 都在协议里存在。qcode 一个都不读：`grep -rn instructions
crates/qcode-mcp/src crates/qcode-kernel/src` 是**空**的。

实测 wordcraft 在握手里给了一段 912 字节的自述（说明它有哪些工具、该怎么组合使用）。这段
文字被代理截获过，但从没进过上下文——模型只能靠工具名和 JSON Schema 猜用法。

### 3. 能力探测的方式已经过时

ADR-0033 §3 决定「能力探测靠**试一次**：握手后调 `resources/list` 与 `prompts/list`，成功才
注册」。当时这么选是对的，理由是「rmcp 的 capability 结构与注册逻辑会重复表达同一件事，而
试一次的代价是两次请求」。

2026-07-28 之后这个理由不成立了：`server/discover` 的返回里**直接带 `capabilities`**，一个
字段就能读到。继续试一次等于明知有答案还去问。实测 wordcraft 明说没有 prompts，我们仍然打
了一次 `prompts/list`，换来一个 `-32601`。

**决定**

### 1. 环境：白名单制，与 GrantSet 同源

spawn 前 `env_clear()`，然后按**固定顺序**注入，后者覆盖前者：

| 顺序 | 来源 | 说明 |
|---|---|---|
| 1 | 基础白名单 | `PATH`、`LANG`/`LC_*`、`TZ`，Windows 另加 `ENV_WINDOWS` 那组（`SystemRoot`/`SystemDrive`/`ComSpec`/`PATHEXT`/`APPDATA`/`LOCALAPPDATA` 等）。沿用 `shell.rs` 的清单，不另立一套 |
| 2 | 合成 HOME | `HOME`、`XDG_CONFIG_HOME`、`XDG_CACHE_HOME`、`XDG_DATA_HOME`、`TMPDIR`/`TMP`/`TEMP`（ADR-0066 §3） |
| 3 | `credentials` 预设点名的变量 | ADR-0066 §4 的表 |
| 4 | `[[mcp.servers]] env` | 用户显式写的 |
| 5 | 出口代理四件套 | `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`——**最后**，理由见下 |

第 5 步必须在最后，这是 ADR-0033 §1 的既有结论（「server 自己写的 `HTTPS_PROXY` 不能把业务
流量从 qcode 的代理下偷走」）。ADR-0023 依赖这一点才能把 MCP 的出口纳入审计；顺序错了，
ADR-0023 当场失效，而且失败是静默的——server 直连成功，`network.jsonl` 里什么都没有。

**不用正则黑名单匹配 `/KEY|PASSWORD|SECRET|TOKEN/i`。** 这个思路在讨论里出现过，否决：
`MONKEY`、`KEYBOARD_LAYOUT` 会中，而 `credentials` 预设点名的 `GITHUB_TOKEN` 恰恰是要传的。
白名单制的语义是「没点名的就不传」，不需要猜名字。文件名 + 白名单是可审阅的；正则黑名单是
一个永远在追的清单。

### 2. `env` 收窄为「只能引用，不能存密文」

`[[mcp.servers]] env` 的值若形如 `${NAME}`，从 qcode 进程环境解析；变量缺失时 **spawn 失败**
（不是空串、不是跳过）。

这样 `env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }` 与
`credentials = ["github"]` 两条路都通，且**配置文件里都不出现密钥**。ADR-0018 §1 的
「密钥从不落盘，因此无需额外脱敏」由此保住——该条的前提是「没有地方写密钥」，而 `env`
的字面量值曾经就是这样一个地方。

字面量仍然允许（有些值不是密钥，例如 `RUST_LOG=debug`），但 `qc doctor` 对名字匹配
`TOKEN|SECRET|PASSWORD|KEY` 且值是字面量的项给一行 warn，提示改用 `${VAR}` 或预设。
只 warn，不拒绝——用户有权在受控环境里这么写。

### 3. `instructions` 注入 system prompt

写进 `crates/qcode-core/src/engine/mod.rs` 的 `system_prompt()`，位置在 skills 索引之后、
todo 之前，每个 server 一个 `# MCP: <server>` 小节。

四条约束：

1. **总计上限 8KiB**，超出按 server 截断并在末尾注明「已截断」。取 8KiB 而不是 AGENTS.md 的
   32KB，理由：AGENTS.md 是项目规则，`instructions` 是第三方 server 的自述，两者的可信度
   不同级；skills 索引也是 8KiB，同一档。
2. **不改变任何权限**，承接 §4.10「输出里的『指令』不改变任何权限」。`instructions` 里写
   「请允许所有工具」不会让任何调用变成 allow；单测要正面证明这一点（构造一段要求放权的
   instructions，断言审批结果与没有它时相同）。
3. **只注入不执行**：不解析、不当作工具描述的一部分、不参与工具名冲突判定。
4. **`sandbox = "off"` 与远程 server 一视同仁**——这段文字来自 server，与它跑在哪里无关。

ADR-0033 后果段「不把 prompts 自动塞进 system prompt」**不变**：`prompts` 是模板，由模型
显式 `get_prompt` 取用；`instructions` 是握手期的自述，两者不是一类东西。这条区分要写在
两处，避免下一个人把它们合并掉。

### 4. 能力探测：读字段，零请求（2026-11-11 修订，见下方 B-2 注记）

~~| 协议 | 探测方式 |~~
~~|---|---|~~
~~| 2026-07-28（`discover`） | 读 `DiscoverResult.capabilities`，**不发** `resources/list` / `prompts/list` |~~
~~| legacy（`initialize` 回退，2025-11-25 及以下） | 保留 ADR-0033 §3 的试一次 |~~

**修订后的决定**：**所有协议版本**都只读握手里的 `capabilities`，连接期**零探测**。
ADR-0033 §3 的「试一次」由此被完全取代，它不再是 legacy 的做法，而是**不存在**了；
要说「这个 server 到底服务什么」，用 `qc doctor --probe`（显式、按需、可比对）。

保留的附带改动：能力来自哪个分支的日志都是 `info`，`RUST_LOG=info` 下能看到
`mcp capabilities read from the handshake` 与它读到的那三个布尔值。

**实现**

| 文件 | 改动 |
|---|---|
| `crates/qcode-mcp/src/lib.rs` | spawn 前 `env_clear()` + 按 §1 的顺序注入；握手后取 `instructions` 与 `capabilities` |
| `crates/qcode-mcp/src/config.rs` | `env` 值的 `${VAR}` 解析（在 `validate()` 之后、spawn 之前，纯函数可测） |
| `crates/qcode-core/src/engine/mod.rs` | `system_prompt(agents_md, skills, mcp_instructions, todos)`——**签名变更**，调用点只有 `turn.rs:374` 与 `tests.rs` 的 5 处调用 |
| `crates/qcode-kernel/src/lib.rs` | 把 `connect_all_with` 返回的 `instructions` 透传给 engine options |

**后果**

- **所有现有 stdio server 的环境都变了。** 依赖继承环境的 server 会失败，报错必须是
  「缺变量 X，加到 `[[mcp.servers]] env` 或 `credentials`」，不是一句 spawn 失败。
  这是本 ADR 唯一的破坏面，也是为什么它和 ADR-0066 的合成 HOME 必须同批落地——单独上
  `env_clear()` 会让 `npx` 类 server 直接跑不起来。
- `no_progress` 一类检测不受影响，但 `credentials` 缺失导致的失败会在启动期暴露，而不是
  第一次 `tools/call` 才暴露。这是改善。
- system prompt 变长（最多 8KiB × server 数）。多 server 场景要观察上下文占用；
  `compact::estimate_tokens` 只数 `messages`，**不含 system prompt**，所以状态栏看不出来
  ——这条要单独补：把指令段也计入，否则多挂几个 server 是无声的预算消耗。
- `instructions` 一旦注入，profile / 事件日志里会多一份 server 提供的文本。它进
  `transcript` 还是只在 `system` 里，需要在实现时定：**只在 system**，它不属于对话内容。

**验证**

- **环境泄漏（核心断言）**：dump 包装器 server 打印 `env -0` 到 stderr，测试读该文件并断言
  包含的变量集合**恰好等于**白名单 + 预设 + `env` + 四件套。实测泄漏的 119 个变量里，
  `GITHUB_TOKEN` / `DEEPSEEK_API_KEY` / `SSH_AUTH_SOCK` 必须在**不包含**的那一侧。
- 顺序：`env = { HTTPS_PROXY = "http://evil" }` 时子进程拿到的仍是 qcode 的代理 URL。
- `${VAR}`：设 `QCODE_TEST_TOKEN=abc` → server 收到 `abc`；不设 → spawn 失败且消息里点名
  缺失的变量。
- `instructions`：stub server 返回 912 字节自述 → `system_prompt()` 里出现，且
  8KiB 截断生效；构造「要求放权」的 instructions → 审批结果与无 instructions 时相同。
- 探测次数：modern stub 断言 `resources/list` **零次**调用；legacy stub 断言一次。

**分期**

| 期 | 内容 | 验收 |
|---|---|---|
| P1 | `env_clear()` + 白名单 + 顺序 + 可诊断报错 | dump 断言通过；`npx` 形态 server 仍可用 |
| P2 ✅ | `instructions` 注入 + 8KiB 预算 + 权限无关性 | stub 自述进 prompt；放权单测通过 |
| P3 | `env` 的 `${VAR}` 与 doctor warn | 字面量密钥配置被提示 |

**备选方案（未采用）**

| 方案 | 为什么不做 |
|---|---|
| 沿用继承 + 正则黑名单 | 见 §1：`MONKEY`/`KEYBOARD` 误杀，`GITHUB_TOKEN` 漏判，且清单永远不完整 |
| 全清空、只给 `[[mcp.servers]] env` | 每个 server 都要手写 `PATH`，`npx` 直接不可用；白名单的意义就是不用重复写 |
| `instructions` 不注入，只在 `qc mcp list` 里给人看 | 模型仍然靠猜；采集了不用等于没采集 |
| `instructions` 与 skills 共用一个预算 | 两者可信度不同级，共池会让一个第三方 server 挤掉项目技能索引 |
| `instructions` 注入同时解析成结构化提示词 | 那是执行 server 的指令，违反 §4.10 |

**实现注记（2026-11-11，P1 + P3 已落地；P2 见下一条）**

- §1 与 §2 已实现：`env_clear()` 后按 白名单 → 合成 HOME → 凭据变量 → `[[mcp.servers]] env`
  → 出口代理 的顺序注入；`env` 值支持整值 `${NAME}`，缺变量时 spawn 失败并点名（实测在
  真机上用一个 `$HOME` 下安装的 server 验证：dump 出的变量集合只剩白名单那几项）。
- §3 `instructions` 注入与 §4 的 modern 能力探测当成一批做（它们依赖 ADR-0067 的
  `discover`/`capabilities`）：两条都已完成，见下面两条注记。
- **白名单会连带清掉 `DISPLAY`。** 实测：这个开发机有 `DISPLAY=:1`，而 whitelist 只留
  `PATH`/`LANG`/`TZ`/`LC_*`，于是需要显示服务的 server（wordcraft 这类桌面应用）启动即死，
  报「neither WAYLAND_DISPLAY nor DISPLAY is set」。这不是缺陷，是 §1 的直接后果，解法
  正是 §2 给的：`env = { DISPLAY = "${DISPLAY}" }`，实测有效。文档与报错要能指向这一点。
- **P3 已做（2026-11-11）**：`qc doctor` 对「名字像密钥且是字面量」的 `env` 项给一行 warn，
  提示改写成 `${NAME}`。名字按 `_`/`-` 分段匹配（`TOKEN`/`SECRET`/`PASSWORD`/`KEY` 等），
  而不是 §1 里被否掉的那个子串正则——`MONKEY`、`KEYBOARD_LAYOUT` 不该中，而
  `GITHUB_TOKEN`、`API_KEY` 必须中，分段匹配两者都对。只在值是字面量时告警，
  `${VAR}` 形式正是解药，不报。这条 warn 同时是 §2「只 warn 不拒绝」的落点。

**P2 实现注记（2026-11-11，`instructions` 注入 + 8KiB 预算）**

- **注入走 `ToolSource::prompts()`，不走 `RunOptions`。** 实现表里写的是「kernel 把
  `connect_all_with` 返回的 instructions 透传给 engine options」，实际选的是让 MCP 的
  `ToolSource` 自己报（默认实现返回空）。理由：`RunOptions` 有 28 个构造点，CLI / TUI /
  GUI / ACP / subagent 各一份，加字段意味着 28 次补写，而漏掉的那一处是**静默**的——
  subagent 就会看不到任何 server 自述。挂在 registry 上则一处接通、所有前端与子 agent 同时
  生效，且「instructions 属于已连接的 server」这个事实直接体现在类型上。
- **一个真 bug，被测试当场抓到。** 第一版在 `run_turn` 里直接用 `ctx.tools.server_prompts()`，
  而 `ctx.tools` 在 P2 里已被换成刷新快照，快照**故意不带 source**（防递归），于是
  `server_prompts()` 恒为空、instructions 一条也进不了 prompt。现在在换快照之前读一次：
  instructions 来自握手、整局不变，每个 turn 读一次既正确又便宜。
- **预算按 §3.1 的全局 8 KiB**（`待定` 里那条「全局还是每 server」按全局落地）：一个 server
  能吃掉全部预算，下一个就得到一整段「已截断」的提示。截断**注记也计入预算**——调用方能依赖
  的不变量是「返回串不超过 8 KiB」，不是「正文不超过 8 KiB，注记白送」。截断落在字符边界上
  （中文一个字符三字节，按字节切会切出半个字）。
- **`# MCP: <server>` 的位置**：AGENTS.md → skills 索引 → MCP 自述 → todo，与 §3 一致。
  空串与纯空白的 instructions 不产生小节：一个只有标题的小节纯属浪费 token。
- **日志只记名字与字节数**，不记正文。理由同 ADR-0064 记变量名不记变量值：日志活得比会话长，
  而这段文字是第三方内容。
- **`compact::estimate_text_tokens`**：`Delta::Context` 之前只估 `messages`，system prompt
  完全不计。ADR-0068 后果段点名要补，现在把 system 一起算进去，否则多挂几个 server 是无声的
  预算消耗。
- ~~§4 未做~~：见下一条注记。

**§4 实现注记（2026-11-11，第一版：按生命周期分流；已被下面 B-2 注记取代）**

> 保留下面的内容只为解释「为什么当时那样写」。**当前实现按 B-2 注记，不再分协议。**

- **判据是 `ProtocolVersion::has_initialize()`，不是版本号比较。** 它回答的正是「这条连接
  是不是 `discover` 答的」：`false` 即 2026-07-28 起无握手的生命周期，`capabilities` 来自
  `DiscoverResult`；`true` 即 legacy。自己写 `>= "2026-07-28"` 也能对（版本是日期串），
  但那是把 rmcp 的知识抄一遍，将来新增版本要跟着改。
- **modern 分支读 `capabilities`，零请求。** 映射是纯函数 `declared_extras()`：
  `resources.is_some()` / `prompts.is_some()` 各决定两个工具，一对一，没有协商也没有猜。
  实测一个声明「只有 tools」的 2026-07-28 server 过去要挨两次 `-32601`，现在是 0 次。
- **legacy 分支保留 ADR-0033 §3 的试一次**，这是刻意的：野外的 2025 时代 server 经常把
  `capabilities` 写得不准，握手里的那个字段在那里不值得信。ADR-0033 注记里记了这条收窄。
  legacy 两次失败日志从 `debug` 提到 `info`（§4 要求）。
- **stdio 仍走 `initialize`**（ADR-0067 实现注记），所以 stdio server 一律落在 legacy 分支、
  照旧试一次。这不是漏改：`Auto` 在 stdio 上被关掉是有原因的（见那条注记），生命周期的
  选择决定了探测方式，§4 的分流只是如实地跟着它。
- **`peer_info` 缺失时走向「问」**。这种情况几乎不可能发生（`serve` 握手成功必有
  `peer_info`），但失败方向要选对：多两次请求，好过静默丢掉 resources/prompts 工具。
- **验证**：`http_probe_server` 新增 `--capabilities` 开关（默认不声明，正好给零探测断言
  一个对象）。四条测试：modern 零次 `resources/list` / `prompts/list`、legacy 各一次、
  modern 声明了就拿到四个工具且仍是零次列表请求、modern 没声明就一个多余工具都没有。
  两条单测钉住 `declared_extras` 的四格表与 `has_initialize` 的边界。变异验证：
  把 modern 分支关掉 → 零探测与「声明即得」两条失败（审计日志里能看到
  `resources/list` 真的又发出去了）；让 `declared_extras` 恒返 `(false, false)` →
  「声明即得」失败。

**§4 修订注记（2026-11-11，B-2：一律信声明，探测改为显式诊断）**

触发这次修订的是真机实测，不是重新讨论：用 `wordcraft-cli 0.15.2`（**2025-06-18**，装在
`$HOME` 下）跑通了整个 stdio 链路，得到两个事实。

1. **本 ADR 立项时引用的那条证据，其实没被修好。** 背景段写「实测 wordcraft 明说没有
   prompts，我们仍然打了一次 `prompts/list`，换来一个 `-32601`」。但 wordcraft 是 legacy，
   第一版 §4 让它走「保留试一次」——请求照发，只是日志从 `debug` 升到 `info`。问题看得见了，
   没被解决。
2. **而且它恰恰是「legacy 声明可信」的反例。** 它的 `initialize` 返回
   `capabilities: { resources: {}, tools: {} }`，与其实际行为完全一致：声明有 resources、
   探测成功；声明无 prompts、探测得 `-32601`。第一版拒绝信任 legacy `capabilities` 的理由
   ——「野外的 2025 时代 server 经常写得不准」——在这个真实样本上站不住，它当时是猜测。

**修订后的决定**

| 项 | 内容 |
|---|---|
| 连接期 | **零探测**。`peer_info.capabilities` 是唯一来源，不再区分协议版本 |
| 声明 vs 实际的比对 | `qc doctor --probe`，显式、按需 |
| `peer_info` 缺失 | 注册 0 个额外工具 + 一条 warn（不猜；用 `--probe` 查） |

**实现**

- `finish()` 删掉整段探测，改为 `declared_extras(&capabilities)`；`ServerHandle` 多存一份
  `capabilities`，供探针比对。连接期日志改成一行 `mcp capabilities read from the handshake`
  带 `protocol` / `resources` / `prompts` 三个字段——这是「判定依据」的唯一可见处。
- `probe_capabilities()`：新公开 API，对每个启用的 server 走**与连接完全相同**的路径
  （`connect_server`，含 grant / env 白名单 / confine / egress client），然后**两个 list 都问**
  ——包括声明说不的那些。跳过未声明的会让「服务了但没声明」在构造上不可检测，而那正是这次
  要能看见的失败。
- `ProbeAnswer` 分三态而不是 bool：`Supported` / `Refused(<消息>)` / `TimedOut`。两种不匹配的
  用户可见损坏是相反的，混成一个「不一致」会丢掉这个区别：

  | 声明 | 实际 | 用户看到 |
  |---|---|---|
  | 有 | `Refused` | 两个工具存在，**每次调用都失败** |
  | 无 | `Supported` | **什么都没有**（工具静默缺失，最坏的形态） |

  后者在旧实现里同样看不见：旧探测只问、然后把答案当真相，从不与声明对照。
- `qc doctor --probe`（新 flag，默认关）。doctor 需要两个缝：`sandbox::build` + 
  `mcp_confine`（逐 server 沙箱），以及 `build_egress` 的 client（远程 server）。用
  `build_egress` 而不是裸 `reqwest::Client` 是硬要求——ADR-0067 §2 就写在这份代码里，
  在诊断命令里绕过代理等于让 `--probe` 走出与真实运行不同的网络路径，那它测的就不是真东西。
  实测对着本地 probe server 跑：不带 allowlist 时 egress 回 `HTTP 403 not-allowlisted`，
  加上 `[network] allow` 后才拿到握手——路径确实一致。
- **失败一律 warn 不 fail**，沿用 ADR-0066 对 MCP 全段的规矩：一个 server 连不上只丢它自己的
  工具。

**代价**

- 「声明漏了但实际支持」的 server 会静默少两个工具，直到有人跑 `--probe`。这是本修订换掉
  两次往返所付的钱，也是 `--probe` 存在的理由。ADR-0033 §3 里那句「试一次」由此作废。
- stdio server 仍然只走 `initialize`（ADR-0067），所以它们全部是 legacy；但探测不再因此发生。

**验证**

- 反转既有用例：`a_legacy_server_is_still_probed_once` → `a_legacy_server_is_not_probed_either`
  （`initialize` 1 次、两个 list **0** 次）；新增 `a_legacy_server_gets_its_declared_extras_without_a_probe`
  （声明了就注册，仍然零请求）。
- 探针三条（HTTP probe server 新增 `--serve-caps`，用来造「服务但没声明」）：
  说真话时逐项一致；`--serve-caps resources` → 报「没声明却应答」；`--capabilities prompts`
  → 报「声明了却 `-32601`」。
- 变异验证两次：把 legacy 分支改回探测 → 反转后的两条失败；让 `probe_capabilities` 复用声明
  而不真问 → 探针三条全部失败。
- **真机**：`wordcraft-cli 0.15.2` 上 `qc doctor --probe` 输出
  `declares tools=true resources=true prompts=false · registered list_resources, read_resource
  · instructions 912 bytes`，无 warn（声明属实）；`qc run` 的 info 日志里不再出现
  两次 list 请求，工具数仍是 19（17 native + 2），与原行为一致。

**待定**

- `credentials` 预设之外，用户能否自己定义 preset（与 ADR-0066 的同一条待定合并处理）。
- ~~`instructions` 的 8KiB 是**全局**还是**每 server**~~：P2 按全局落地（`mcp_prompt`），多
  server 时先进先得、后来的被截断并在末尾注明。真要每 server 配额时再改，`ServerPrompt` 列表
  是现成的切分点。
- 是否把 `instructions` 的 hash 记进事件日志，便于 replay 时解释「当时模型看到的是哪一版自述」。
- 白名单要不要加 `DISPLAY`/`WAYLAND_DISPLAY`/`DBUS_SESSION_BUS_ADDRESS` 的「按需透传」开关，
  还是坚持「要就写 `env`」。倾向后者：默认不传显示服务句柄更安全。

## ADR-0066 §2 修订注记：配置里的 `~`，以及写侧的危险根（2026-11-11）

**问题**：同一份 `config.toml` 里，`~` 有三套语义。`[skills] roots` 认 `~/x`，并且**还把
相对路径按 workspace 解析**；`[sandbox] extra_read/writable` 什么都不校验（相对路径由
`bwrap` / `sandbox-exec` 按 qcode 的 cwd 解释，landlock 则因为 `path.exists()` 为假而在启动
时报「不存在」）；`[mcp.servers] read_paths/writable_paths` 强制绝对路径，`~/x` 直接被拒。
用户写 `~/.config/qcode/config.toml` 时无从知道该用哪一种。

**决定**

1. **`~` 只认两种写法**：`~` 与 `~/x`，展开成真实 home。`~user/x` 不展开（需要 passwd
   查找，且「给别的用户的 home 授权」从来不是 per-task 配置想表达的事），裸相对路径也不展开
   （配置文件是用户级、workspace 是每任务一个，「相对于什么」没有稳定答案）。剩下的写法
   被 MCP 的 `validate()` 当作非绝对路径拒绝，报错里点名 `~user/...` 不支持。
2. **展开点在校验之前，且只有一个实现**：`expand_tilde`（纯函数，home 由调用方传入）+
   `expand_paths`（遍历五处列表）。调用点是 `load` / `merge_project_file` /
   `validate_document`——正好是原先调 `mcp.validate()` 的三个地方，所以「展开是校验的前提」
   这条不变量没有新增概念面。
3. **不用 `#[serde(deserialize_with)]`**。它把不变量提到类型层（任何 `McpServerConfig`
   都绝对），代价是 `qcode-mcp` 得自己读环境算 `$HOME`——那个 crate 不依赖
   `qcode-kernel`，于是仓库里就有两个 home 解析器。ADR-0068 §1 清掉的正是这类两套规则。
   流程级不变量的漏网是「直接 `toml::from_str::<Config>`」的代码，而那种代码后面紧跟着
   `validate()`，等于有兜底。
4. **`writable_paths` 补 `dangerous()`**。读侧一直有；写侧只做了 canonicalize + push。
   `~/` 展开后等于 `$HOME`，而能写 `$HOME` 的 server 可以塞 `.gitconfig` 的
   `core.hooksPath`、shell rc 一行、`~/.config/gh/hosts.yml` —— 下一条命令就生效，不需要
   第二次写，所以「只读凭据 + 一次写」的隔离在写侧是全开的。只检查用户写的条目：
   workspace 与私有 HOME 是无条件授予的，把它们也纳入检查会让「从 `$HOME` 启动 qcode」
   导致每个 server 都起不来。
5. **`[skills] roots` 保留相对路径**。技能根是「要扫描的目录」而不是授权，`.qcode/skills`
   写在仓库里是有用的；所以它用同一个 `expand_tilde`，再叠自己的 workspace 相对规则。

**代价**

- `writable_paths` 里的 workspace 祖先（例如 workspace 是 `/w/a/b` 时写 `/w`）以前能用，
  现在被拒。这是 `dangerous()` 本来的定义，读侧一直如此；写侧对齐后属于行为变更，写进
  CHANGELOG。
- 配置文件里 `~` 的展开与**工具参数里的 `~` 无关**。后者由那个 MCP server 自己解析：实测
  wordcraft（Rust 实现）把 `~/x.docx` 当字面路径，在 workspace 下建一个名叫 `~` 的目录并
  静默写入——因为 workspace 可写，所以不报错。文档只能给出「要可靠就传绝对路径」，这点
  必须写清楚，否则 `~` 支持会被误读成「工具参数也能用 `~` 了」。

**验证**

- `tilde_expands_only_for_this_user`（六种写法逐条）、`tilde_without_a_home_leaves_the_path_alone`、
  `expansion_is_idempotent`、`every_configured_path_list_gets_expanded`（五处列表全覆盖）、
  `a_relative_mcp_path_is_still_rejected_after_expansion`（`~/x` 通过、`Documents` 与
  `~user/x` 仍报错）、`skill_roots_keep_expanding_and_keep_resolving_against_the_workspace`。
- 写侧两条：`["~/"]` 展开成 home 被 `DangerousWriteRoot` 拒绝且报错文案点名
  `writable_paths`；`["~/Documents"]` 正常授予。
- doctor 四条（集成）：`~/Documents` 存在时打 `~/Documents → /abs`；不存在时点名它在
  **展开后**的位置；`["~/"]` 报「your home directory」；`~user/x` 在 config 行 `[fail]`。

**待定**

- `[sandbox]` 的 `extra_read` / `extra_writable` 仍然没有加载期校验（相对路径只在 landlock
  启动时以「不存在」失败）。为它们加「必须绝对」会在 `mode = "none"` 的用户身上把原本无害
  的配置变成加载失败，所以这轮只做展开，不动校验。

**2026-11-11 补记：写回会固化 `~`**

§2 的展开点是加载，所以内存里的 `Config` 只有绝对路径。而 `ConfigEdit::apply_config` （GUI
设置页与首启向导保存时走的唯一路径）把 `Config` 序列化回 TOML——于是「保存任意一个设置」
都会把 `writable_paths = ["~/Documents"]` 写成 `/home/q/Documents`。用户没改过那一条、界面上
也看不出来，等把配置拷到另一台机器才发现它只对 `/home/q` 成立。§1 想要的「跨机器可移植的
写法」因此活不过一次保存。

**修法**：写侧不再取 `Config` 的拼法，而是**保留文件里已有的拼法**。`spell_paths` 拿新值
（绝对）去和旧数组里的字面量逐个比，比的是 `expand_tilde(字面量) == 新值` 而不是字符串相等
——两种拼法是同一个路径，而文件里那种才是更好的。匹配按值、按消费（一对一则移除），所以
排序变化不会改拼法，重复项各留各的写法；没匹配上的回退到绝对形式（= 修复前的行为，绝不
丢路径）。落点两处：`set_path_array`（`[sandbox]` 两条 + `[skills] roots`）与
`existing_server_path_literals`（`[[mcp.servers]]` 的 `read_paths` / `writable_paths`，
必须在 `set_array_of_tables` 整体替换之前读出来）。

**没有采用的方案**

| 方案 | 否决理由 |
|---|---|
| `Config` 存原文 + 展开值（dual-field） | 能一并解决将来的 `$VAR`，但要把「路径」从一个 `PathBuf` 变成一对值，改动面覆盖 `validate()`、grant 推导、doctor、GUI 每个读 `PathBuf` 的地方；而 bug 的范围只是「写回时别覆盖原文」。等真要做 `$VAR` 时再做，那时它才有第二个理由 |
| GUI 显示 `~/x`（把 home 前缀缩写） | 只改显示不改内存，仍要在写侧做同一件事；而且让界面写回**未展开**的 `~/Documents`，会让 `self.config` 里出现非绝对路径——grant 推导对非绝对路径是**静默丢弃**（`canonicalize()` 失败即跳过），于是「界面上有、实际没授权」。比原来的 bug 更难查 |
| 保存前重新读一遍文件、只写差异键 | `apply_config` 的语义就是「把内存态写成文件」，改成增量 diff 会让删除操作（清空 `extra_read`）无法表达 |

**代价**

- 用户在界面里把 `~/Documents` 手工改成 `/home/q/Documents`（同一台机器上语义完全相同的
  两个拼法）后，存盘仍写 `~/Documents`。这是刻意的：界面无法区分「想换拼法」和「只是
  保存了别的东西」，而 `~` 的形式在下一台机器上仍然正确。
- 设置页的输入框显示的是**展开后**的绝对路径（`fields.rs` 取 `path.display()`），与文件里
  的 `~/x` 不一致。纯显示问题：文件内容是对的，且界面里直接输入 `~/x` 也会被原样写入。
  要一致就得让界面看得见原文，那是上面被否决的 dual-field。

**验证**

- `a_mcp_tilde_path_survives_a_save_that_did_not_touch_it`、`every_tilde_path_list_keeps_its_spelling`
  （五处列表全覆盖）、`a_tilde_path_that_changed_is_rewritten`（改过的必须写全，否则修复就
  退化成「永不重写」）、`reordered_paths_keep_their_own_spellings`、
  `unmatched_values_fall_back_to_the_absolute_form`。
- 第一条在**关掉修复**时确实失败，报 `the tilde was frozen into an absolute path`——不是
  恒真断言。

## ADR-0070：MCP 的 `command` 走 shell 环境快照的 PATH（2026-11-11）

**状态**：已实现（2026-11-11）。

**问题**

`[[mcp.servers]] command` 要写绝对路径，而这个机器上的路径带版本号：
`/home/q/cot/lib/wordcraft_0.15.2_linux_amd64/bin/wordcraft-cli`。每次升级都要改配置，
换台机器还要改用户名。裸名字 `wordcraft-cli` 本来是该写的东西，但它在两种启动方式下表现
不同：

- 终端里启动 `qc`：`PATH` 来自你的 shell，找得到；
- 从 Dock 启动 `qc gui`：`PATH` 是 launchd 的最小集，找不到 → `CommandNotFound`。

ADR-0064 已经为 `run_shell` 解决过一次（`{shell} -lic 'env -0'` 采样一次）。MCP 侧从来没
接上：`GrantSet::derive` 与 `base_env()` 各读一次 `std::env::var_os("PATH")`，读的都是 qcode
进程自己的那份。

**决定**

1. **裸 `command`（不含 `/`）按 shell 快照的 `PATH` 解析**；绝对/相对路径完全按字面，行为
   不变。`GrantInput.path: SearchPath` 与 `ConnectOptions.path: Option<&OsStr>` 携带它。
2. **推导与启动必须用同一个值**，这条是本 ADR 的核心约束。`Command::new` 拿的是**裸名字**
   （见 `connect_stdio`），所以真正的查找发生在子进程里、由子进程的 `PATH` 完成。如果
   `derive` 用快照 `PATH` 而子进程拿的是 qcode 的 `PATH`，沙箱会为"另一个文件"建根——要么
   起不来，要么跑的不是被授权的那一个。因此 `connect_stdio` 在 `env_clear()` → 白名单之后
   覆盖 `PATH`，位置在 `server.env` 之前（server 自己写 `PATH` 仍然赢）。
3. **只在需要时采样**。`mcp_needs_path_lookup` 为假（没有任何启用中的 stdio server 用裸名字）
   就不跑 shell。采样在 `spawn_blocking` 里做：`assemble` 被 await 在 GUI 的 UI executor 上，
   阻塞它等于冻窗口。这次为此把 `tokio` 加进 `qcode-kernel` 的正式依赖（此前只在
   dev-dependencies 里）。
4. **拿不到快照就退回旧行为**（qcode 自己的 `PATH`），不报错：`env_sampling = "off"` 是用户
   的显式选择，采样失败也只是少一次机会。`qc doctor` 会把这件事说明白。
5. **`grep` 一份判定，不写两份**：`mcp_needs_path_lookup` / `mcp_path_from_snapshot` 放在
   kernel，`qc doctor` 调它们。doctor 各自写一份的话，迟早变成「doctor 采样、实跑不采样」，
   而两边都不会报错——ADR-0068 收拾过的正是这类"两处拼写漂移"。
6. **失败文案点名查的是哪份 `PATH`**。`command "x" was not found` 单看无法区分"拼错了"和
   "这个 GUI 的 PATH 比你的终端小"，后者恰恰是这个 ADR 存在的原因。

**被否掉的方案**

| 方案 | 否决理由 |
|---|---|
| 配置里支持 `$VAR` / `${VAR}` 展开 | `env` 里已有 `${NAME}` 表示"从 qcode 环境读"，再加一种内嵌展开就有两种变量语法；而且从 Dock 启动时 `WORDCRAFT_HOME` 同样不在环境里，展开只会把"找不到文件"换成"找不到变量"，一个都没解决。ADR-0068 §2 的"只认整值"也不该为了它松掉 |
| 让用户自己写软链接 | 能用（`~/.local/bin/x -> ~/cot/lib/<版本>/bin/x`），但每个 server 一条，且软链接本身还是要维护；快照方案一次解决所有 server |
| MCP 继承整个 shell 环境 | ADR-0068 §1 的 `env_clear()` + 白名单是刻意的（第三方进程不拿用户的密钥）。本 ADR 只借 `PATH` 一个变量——它不是密钥，而且正是"找不到程序"的唯一原因 |
| 在 `resolve_program` 里静默合并两份 `PATH`（快照 + 进程） | 子进程只认一份。合并后 `derive` 可能命中只有 qcode `PATH` 里才有的目录，而子进程找不到 → 违反第 2 条 |

**代价 / 已知边界**

- **只有 `PATH` 过桥**。`command` 里写 `${VAR}` 仍然不被展开（`MissingEnv` 的语义只对 `env`
  表生效）；从 Dock 启动时 `WORDCRAFT_HOME` 之类的变量依旧不在。要变量就等 dual-field
  （文件原文 + 运行时展开值）那一轮。
- **子进程的 `PATH` 变了**：从"qcode 的"变成"你的 shell 的"。这与 ADR-0064 对 `run_shell`
  做的事一致，但对已有的 MCP 配置是一次行为变更——若 rc 文件里的 `node` 与 qcode 启动时的
  `node` 不同版本，server 现在看到的是前者。这正是用户在自己终端里看到的世界。
- **`PATH` 里的目录不会因此变成可写根**。只有解析出的那一个程序的上游目录进 `runtime`
  （只读 + 执行），且仍过 `dangerous()`；`writable_paths` 与它无关。
- Windows 上 `resolve_program` 仍不补 `PATHEXT`（`node` 不会自动试 `node.exe`）。这是既有
  缺陷，本 ADR 不做也不加重。

**验证**

- 单元（`qcode-mcp`）：同一个名字在两处安装、只有给定 `PATH` 里那处在 `runtime` 里；
  缺失时报错点名 `shell environment's PATH`；`SearchPath::from(None)` 回退到进程 `PATH`。
- 端到端（真 Landlock + 真子进程，`crates/qcode-kernel/tests/mcp_grant.rs`）：
  `command = "probe"` + `with_path(<bin 目录>)` 起得来（3 个工具）；同一配置不给 `PATH`
  则被跳过。这两条合起来钉的是"推导与启动用同一份 `PATH`"——只改一半时它们会失败。
- doctor 三条：假 `env_command = "printf 'PATH=...\0'"` → 打 `mcp path` 且 grant 落在该目录；
  `env_sampling = "off"` → warn 且 `not found` 文案含 PATH；绝对 `command` + 一个必然失败的
  `env_command = "exit 1"` → 完全不出 `mcp path` 行（证明没有采样）。

**待定**

- `[shell] env_include` 的 `COT_*` 等前缀要不要也过桥给 MCP。目前只过 `PATH`：那是"起不来"
  的唯一原因，其余的属于"第三方进程该不该看见你的变量"，应留给 ADR-0068 的口子
  （`env = { ... }` 与 `credentials`）。
- `PATH` 过桥后，`qc doctor` 里"这个 server 的 runtime 根为什么是 `~/cot/lib/...`"已经能
  看出来，但"哪一条 `PATH` 条目命中的"还没有单独打印。若有人提出，加一行即可。

---

## ADR-0066 §2 修订注记 2026-11-11 补记：`extra_writable` 在 Linux 上是真的没生效

**发现方式**：用户按上一轮注记把

```toml
[sandbox]
extra_read = ["~/Documents"]
extra_writable = ["~/Documents"]
```

写进配置，然后得到「读成功、写 `Permission denied`」。这个组合正是"配置写错了"和"代码没接上"
最难分辨的形状——只写 `extra_read` 的那一半起了作用，读得到的文件就在同一个目录里。

**根因**：不是配置问题，是 `crates/qcode-tools/src/sandbox/mod.rs` 把参数扔了。

```rust
SandboxMode::Landlock => {
    let _ = extra_writable;                       // ← 丢弃
    let sandbox = Landlock::new(primary_root, extra_read)?;   // 签名里没有写列表
}
SandboxMode::Bwrap => {
    let _ = extra_writable;                       // ← 同样
}
```

`extra_writable` 从来只服务 `sandbox-exec` 与 `windows-acl`。`let _ =` 让编译器闭嘴，于是没有
任何警告；`Landlock::new` 的签名里也没有它，于是这条链上没有任何一处会提醒你。上一轮注记把
这个状态写成了"既有行为"——那不是判断，是当时没查 `mod.rs` 就下的结论。

**改法**

1. **`Landlock::new(workspace, extra_read, extra_write)`**：`extra_write` 的每一条都按
   `allowed`（= `landlock_handled(abi)`，与读写 task root 完全相同）装规则。不用手挑写入掩码：
   `WRITE_FILE` 单独给出去连"写一个新文件"都不够，而 `REFER` 必须两端都在才允许 rename。
2. **`Bwrap` 同形**：`extra_write` 走 `--bind`，并且**不受 `OutsideRoots` 开关影响**——
   那个开关管的是"qcode 自己加的构建缓存"，这里管的是"用户要的"。已经作为 task root 绑过的
   路径跳过（bwrap 对重复目的地报错）；`extra_write` 落在 `/tmp` 下时也不再挂私有 tmpfs，
   否则授权会被遮住。
3. **`describe()` 打出 `extra write: [...]`**，与 `extra read` 对称。ADR-0066 §1 要的是
   "打印出来的授权 = 实际执行的授权"，`qc doctor` 只能看这一行。
4. **加载期拒绝危险根**（见下）。

**危险根的检查放在加载期，三条形状**

`[sandbox] extra_writable` 是**配置文件级**的，没有按 task 收窄的余地，所以 `extra_writable
= ["~/"]` 这个笔误对每一个 task（包括 `sandbox = "on"` 的 MCP server）都成立。`~/` 展开后
就是 home 本身，而 home 里有 `~/.ssh` 与 `~/.aws`。

拒绝 `/`、home 目录本身、`/home` / `/Users` / `/root`，`SandboxConfig::validate()` 在
`load` / `merge_project_file` / `validate_document` 三处都调，位置与 `mcp.validate()` 相同
（展开之后——展开前 `~/` 是相对路径 `~/`，和谁都不相等）。

**判定只有一份**：抽到 `qcode_core::paths::dangerous_write_root`，`qcode-mcp` 的
`dangerous()` 改成调它并只保留自己那条"MCP 才知道 workspace 的 parent"判断。两边各写一份的
话，会漂移成"server grant 拒 `/`、沙箱给 `/`"，而两边都自洽，没有测试会发现。

**没收的**：仍然**不要求 `extra_writable` 必须绝对**。相对路径在这里不会命中任何已解析的
root，等于什么都不授予，而把它变成加载失败会让 `mode = "none"` 的用户今天能加载的配置明天
报错。这正是上一轮"待定"里那条顾虑，这里只回答了其中的安全问题，没有回答拼写问题。

**顺带收的**：`real_home()` 三份（kernel 的 `home_dir`、mcp 的 `real_home`、以及现在这份）
合成 `qcode_core::real_home`，另外两个变成薄委托。`~/` 现在出现在三个 section 里，三份解析器
就是三个"`~` 到底指哪儿"的答案。Windows 上统一取 `USERPROFILE` 优先（kernel 原来 `HOME`
优先）：在 MSYS/git-bash 里 `HOME=/home/q` 是给 POSIX 层看的，原生进程把它当路径展开是错的。

**代价 / 已知边界**

- **Landlock 会合并规则，宽的那条赢**。`extra_writable = ["~/Documents"]` 加上
  `~/Documents/proj` 作为只读 task root，`proj` 实际可写。这不是新行为（两个嵌套 task root
  一直如此），差别在于 `[sandbox]` 这份列表对单个 task 不可见——也是危险根必须在加载期拒绝
  的第二个理由。
- **写入是整棵子树递归的**，没有"只写这一个文件"。要更细就得等按 task 的目录授权。
- **`windows-acl` 与 `sandbox-exec` 行为不变**，这一轮只是把 Linux 补上，外加三者的危险根
  判定统一。
- `extra_writable` 里不存在的路径现在**启动报错**，而不是像以前那样被静默跳过（`extra_read`
  一直是报错的）。静默跳过读起来就是"这个设置被忽略了",而这正是本次要修的症状。

**验证**

- `extra_write_grants_a_writable_subtree_outside_the_roots`（真 Landlock + 真子进程，
  走 `with_roots`，因为 `run_shell` 每轮走的是那条路）：不列则拒绝、列了则能 `printf` +
  `mv`（一条同时需要 MAKE_REG / WRITE_FILE / REFER）、**同父目录下未列的兄弟目录仍然拒绝**、
  `describe()` 含 `extra write`。
- `extra_write_rejects_a_missing_root`、`extra_write_rejects_the_filesystem_root`。
- bwrap 三条：`extra_write` 在 `OutsideRoots::Nothing` 下仍然是 `--bind`（MCP server 的形状）、
  已是 task root 时不重复绑定、落在 `/tmp` 下时不挂私有 tmpfs。
- 配置四条：`/` 被拒、`/home`|`/Users`|`/root` 被拒、home 本身被拒而子目录通过、端到端
  `validate_document` 让 `~/` 失败且报错文案点名展开后的路径。
- `qcode_core::paths` 四条：root、home、home 的父目录、没有 home 时只退化掉 home 那一条。

**顺带修掉的既有测试缺陷**：`extra_read_grants_a_subtree_outside_the_system_allowlist` 里
`run_script` 用 `.expect("run")` 包 `command.output()`，而 Landlock 拒绝 `execve` 时子进程
根本没起来，`output()` 返回 `EACCES`——于是"执行被拒绝"这条断言在它最该成立的环境里 panic，
测试从写下那天起就没真正跑过（在本机 HEAD 上确认失败）。现在把 spawn 期的 `PermissionDenied`
也算作拒绝。

## ADR-0066 §2 修订注记 2026-11-11 补记之二：伪终端（`[sandbox] allow_pty`）

**问题**：Landlock 的设备白名单只有 `null`/`zero`/`full`/`random`/`urandom`/`tty`。`/dev/ptmx`
不在里面，于是任何终端形状的程序在第一次 `openpty` 就 `EACCES`。规模是可以量的：本仓
`qcode-terminal` 的集成测试全部用 `openpty` 起子进程，在 Landlocked `run_shell` 里 23 个全红。
`/dev/ptmx` 自己 mode 0666（`crw-rw-rw- root:tty`），所以唯一拒绝者就是 Landlock——不是设备
权限，也不是缺设备节点。

**为什么不能像 `bwrap` 那样顺手 **：`bwrap --dev` 挂的是 `newinstance,ptmxmode=0666,mode=0620`
的**私有** devpts，分配出来的 slave 只有该 mount 能看见，所以那边 PTY 一直可用且免费。
Landlock 没有 mount namespace，能给的只有**宿主**的 devpts，而这不便宜：

`/dev/pts/0..3`（写这份注记时这台机器上的实测数量）属于用户已有的终端窗口，owner 是该用户
本身，unix 权限本来就允许写——现在被拒绝纯粹因为 Landlock。一旦授予写权限，受限进程就能往
那 4 个终端里打字，也就是**往一个没有被沙箱约束的 shell 里输入命令**。那不是"多读一个目录"，
是从沙箱里出去的完整通道，而沙箱的全部意义是"只有 workspace"。

**决定**

1. **`[sandbox] allow_pty`，默认 `false`。** 打开时给 `/dev/ptmx`（字符设备 → 文件权限位，与
   其它设备同处理）和 `/dev/pts`（目录 → 读位 + `WRITE_FILE` + `IOCTL_DEV`）装规则。
   `MAKE_*`/`REMOVE_*` 故意不给：没有谁需要创建 pty 节点，而 devpts 是 root 所有。
2. **`IOCTL_DEV` 是这道题最容易漏的一位，而且漏了不会一眼看出来。** 终端用
   `ioctl(TIOCSWINSZ)` 报告尺寸，ABI 5+ 把它纳入 handled 掩码。只给 `WRITE_FILE` 的结果是：
   `open` 成功、`read`/`write` 成功、pty 起得来、**第一次 resize 才 `EACCES`**。第一版掩码
   就是这样，`qcode-terminal` 的测试依旧全红但错误从"打不开 ptmx"变成了"spawn 失败"，
   靠 `strace` 才定位到 `ioctl(4, TIOCSWINSZ) = -1 EACCES`。
   掩码因此抽成 `landlock_pty_dir_rights()`，测试断言的是**这个函数**而不是测试里抄的一份
   ——抄一份正是让这一位漏网的机制。
3. **`bwrap` 不看这个开关**，因为它已经做到了更好的版本（私有 devpts）。`mod.rs` 的分支里
   写的是理由，不是 `let _ = allow_pty`——后者正是上一个补记里那个 bug 的形状。
4. **`describe()` 打出 `pty: host devpts`**，`qc doctor` 能看到（ADR-0066 §1：打印的授权必须
   等于执行的授权）。措辞带 `host`，因为这个差别就是全部风险所在。
5. **`mode = "none"` / `windows-acl` 无事可做**：前者没有边界，后者只限制写，PTY 本来就可用。

**规则叠加，以及它对验证的影响**

Landlock 的 ruleset **只能收紧**：`restrict_self` 叠在外层之上，内层的授权无法放宽外层拒绝
的东西。所以"在受限环境里跑 PTY 测试"有两层，而外层通常就是 qcode 自己的 `run_shell`。

这一点踩过一次：新测试在 agent 会话里跑时明明给了 `allow_pty = true` 仍然 `EACCES`，第一反应
会去怀疑掩码算错了——实际是外层沙箱没开。测试因此改成**先做一次无规则基线探测**：若
`/dev/ptmx` 在没有本 ruleset 的情况下就已经打不开，跳过并**说明是外层沙箱**，而不是把自己的
环境问题报成功能失败。

**代价 / 已知边界**

- 打开后，受限进程可以往同 uid 的**任意** pty 写，包括不属于它的终端。这是这个开关的全部
  代价，也是它默认关的唯一原因。
- 没有"只给本进程自己的 pty"这种粒度：Landlock 规则在 `restrict_self` 前静态装好，而 pty
  编号要等 `openpty` 之后才知道。真要这个粒度就得学 `bwrap` 用 mount namespace——那是另一
  件事，不是这个开关的延伸。
- `extra_writable` 与本项互不影响：前者是文件路径，后者是设备。

**验证**

- `allow_pty_decides_whether_a_terminal_can_be_allocated`（真 Landlock + 真子进程，走
  `with_roots`）：`false` 时 `openpty` 必须失败（否则测试没意义）、`describe()` 不含 `pty`；
  `true` 时完整走 `open /dev/ptmx → TIOCGPTN → TIOCSPTLCK → open /dev/pts/N → write`，且
  `describe()` 含 `pty`。能走到这一步本身还证明 `/dev/pts` 那个掩码是内核接受的——目录上用了
  非目录专属位，若内核判 EINVAL，`for_roots` 会直接失败而不是返回 ruleset。
- `allow_pty_is_off_unless_asked_for`（缺省 false / 显式 true）、`every_sandbox_key_is_known`
  （新增键没漏进 `KNOWN_KEYS`——上一个补记记过这个错）。
- **未验证**：从"沙箱内的沙箱"里无法证明 `true` 那一半。端到端确认需要在配置里打开它、重启
  `qc`，然后跑 `cargo test -p qcode-terminal`——届时那 23 个应当转绿。

## ADR-0071：图片能力属于「路由」，不属于协议——修好 OpenAI 兼容路由永远看不到图（2026-11-11）

**状态**：已实现（2026-11-11）。

**问题**

用户在 DeepSeek 路由下发一张图，模型回「this session only accepts text input」。可这条路由
的模型**能看图**：`POST https://api.deepseek.com/v1/chat/completions` 带 `image_url` 的
data URL 返回 200 且正确描述图片；`POST https://api.deepseek.com/anthropic/v1/messages`
带 Anthropic `image` block 同样 200 且正确（2026-11-11 用真 key 各测三次）。

真正的断点在适配器的一行常量：`OpenAiCompatConfig` 的 `capabilities()` 里
`images: false` 是**硬编码**。引擎按 `capabilities().images` 决定「发图还是发占位」，
于是每张图在请求构造阶段就被换成一行 `[image omitted: …]`，`openai_compat.rs` 里早就写好
的 `image_url` 线格式代码永远走不到。Anthropic 适配器则是另一个硬编码 `true`——同一个
错误的两个方向：协议被当成了能力的来源。

**决定**

1. **`images` 变成配置项，缺省继承目录。** `ProviderConfig.images: Option<bool>`，
   `CatalogEntry.images: bool` 给内建路由一个默认答案（openai / deepseek / anthropic 为真，
   ollama / minimax 为假），`Resolved.images` 是两者合一的结果。目录没收录的路由缺省**为假**
   ——手写网关背后是什么模型，qcode 不知道。
2. **适配器只转述配置，不再自己判。** `OpenAiCompatConfig.images` /
   `AnthropicConfig.images` 来自 `Resolved.images`，`capabilities()` 原样返回。
3. **降级语义不变（ADR-0061 §8）。** 为假时仍然发占位文本而不是报错；改的只是「谁说了算」。
4. **设置页加一列 `images`。** 布尔列只有两态，表达不了「继承」，所以 `setup/save.rs` 只在
   **与目录不一致时才写**这个键：没动过开关就不会把今天的目录答案冻结进配置文件。
   `config_edit.rs` 的 `set_optional_bool` 同理（`None` 不落键）。
5. **顺带修掉 `protocol` 根本不落盘。** `ConfigEdit::apply_config` 重建 provider 表时从来没写
   `protocol`，而设置页的保存走的正是它——把路由切到 Anthropic 端点、保存、再打开，协议列
   又是目录默认值。两个三态字段现在在同一处写入，`None` 都是「继承」。
6. **DeepSeek 不需要换协议。** 此前给出的临时建议——把 deepseek 路由改成
   `protocol = "anthropic"` + `base_url = .../anthropic`——现在只是可选路径，不再是看图的
   前提：两条端点都能看图，Chat Completions 那条现在也真的把图发出去了。

**验证**

- `qcode-kernel`：`resolve_reports_whether_the_route_takes_images`（目录真/假、未收录路由
  缺省假、`images = false` 关掉目录的真、`images = true` 打开未收录路由）；
  `the_built_provider_reports_the_resolved_image_capability`（两种协议各构建一次 provider，
  `capabilities().images` 跟随配置而不是常量）。
- `qcode-providers`：`capabilities_carry_the_configured_image_support`；既有的
  `an_image_turns_the_message_content_into_typed_parts` 钉住 `image_url` 的线格式。
- 端到端（真 key，release 二进制，`--image` 一张 128×128 纯蓝方块 PNG）：
  默认配置 → 「Deep blue colour.」；`images = false` → 「Cannot see image.」；
  后再改回 `true` → 「The square is blue.」三条路径各自对应一个开关状态。

**待定**

- 目录里的 `images` 是按**厂商**填的（DeepSeek 全绿），但真实粒度是**模型**。
  `SETTINGS-MODELS-V2.md` §4.4 已经要把能力挪到模型条目上；等模型级元数据落地，
  这里的 `CatalogEntry.images` 应降级为「该路由是否需要用户显式声明」，不再是唯一来源。
- `[providers.X.headers]` 里的 `authorization` 会与 `api_key_env` 派生出的 `x-api-key`
  **同时发送**（注释声称的 replace 语义不成立）。这次没有改动：修它要决定「谁赢」，
  且会改变已验证可用的配置行为。

## ADR-0072：对话内容的选择与复制——所见即所复，按钮补虚拟列表（2026-11-12）

**状态**：已实现（2026-11-12）。

**问题**

用户问「怎么在对话里选中并复制」，查下来是三件独立的事：

1. **拖选已经有了，复制没有。** `TextView` 的 `selectable` 默认就是 `true`，qcode 没有任何
   一处关掉它；`TextView` 自己作为参与者注册进窗口级选择（`selection_adapter.rs` 用
   `in_paint_order()` 注册，所以跨块选择按绘制顺序拼接，一条回答被拆成多个 `TextView`
   也能连起来选）。真正缺的是**触发复制的键**：全仓只有一处 `cmd-c`，限定
   `Some("Terminal")`，而 `run.rs` 的注释写着「everywhere else Cmd+C belongs to the text
   fields and the transcript」——gpui 自身没有默认的 copy 动作（默认键位里只有
   `ctrl-c` → `Gamma`），所以 transcript 的高亮出来了，`Cmd+C` 什么也没进剪贴板。
2. **代码块的复制按钮被显式清空。** `markdown_with_folds` 里
   `.code_block_actions(|_, _, _| div())`——上游留的口子，被填成了空。
3. **虚拟列表 + 滚动清选区，把「选一大段」变成敌意操作。** 对话是
   `ListState` 虚拟列表，视口外的块不渲染，而选择系统的参与者是「渲染并注册了的元素」，
   所以拖到视口外拿不到屏幕外的文字；`render/main.rs` 又有意
   `on_scroll → TextSelection::clear`（否则鼠标拖到一半丢 up 事件时，滚轮会把幻影选区
   一路拖长）。两者叠加 = 长回答基本选不全。

**决定**

1. **`Cmd+C` / `Cmd+A` 用根 `on_key_down`，不用 `KeyBinding`。** 这是本次唯一的非直觉选择：
   GPUI 的绑定优先级是「上下文深度优先、注册顺序次之」，而**没有上下文谓词的绑定会拿到
   满深度**，与任何满深度匹配的谓词打平；平局按注册序，**后注册的赢**。app 的 `bind_keys`
   在 `gpui_kit::init` 之后跑，所以根级 `cmd-c` 会压过输入框自己的 `Copy`（`Input` 上下文
   里的 `cmd-c`/`ctrl-c`）——用户会突然无法在输入框里复制。key-down 监听只在**没有绑定
   消化这个按键**时才跑到，那正好就是「焦点不在输入框」的情况。
2. **没有选区时什么都不做。** 另一种做法是清空剪贴板来「证明快捷键生效了」，那会毁掉用户
   刚才在别处复制的东西。
3. **选区复制 = 所见即所复。** 走的是 `SelectionFormat::Plain`（默认值），因此框选一个
   折叠的代码块只复制可见的十二行。
4. **代码块按钮复制全文。** 与第 3 条**故意不同**：折叠是阅读辅助，一个按下去只还你
   四十行里十二行的复制按钮是陷阱。两条路径的差异写进注释与手工清单。
5. **回答底部三个 chip**：`复制`（纯文本）、`Markdown`（源）、`整轮`（问 + 答，只有在
   这条回答属于某一轮时出现）。整轮**不含执行过程**（思考、工具调用）——那是用户主动折起来
   的东西，不是他说的「这一轮」。
6. **纯文本由 `copy_text` 自己算。** 渲染后的 plain text 只存在于 `TextView` 的布局里，
   按钮够不着，所以按钮拿到的是 markdown 源。规则是启发式，和 `markdown_segments` 一个立场：
   **删掉粘贴后会变噪音的标记，保留只是少见的**。fenced code 取正文、丢围栏；标题、引用、
   无序列表去掉标记（`- ` → `• `）；行内代码去反引号；链接 `[文本](url)` → `文本 (url)`
   （url 就是链接的内容，丢掉等于把引用变成空词）；表格与分隔线原样保留（拍平就没法读了）。
   **单 `*` 与单 `_` 一律不动**：`snake_case` 和 `2 * 3 * 4` 在回答里比斜体常见得多，
   误删是真丢字符。`**`/`__`/`~~` 只在**本行内配平**时才删，不成对就当字面量。
7. **`code_block_actions` 里自己画语言标签。** 上游在 hook 存在时不画语言
   （`node.rs` 的 filter 带 `code_block_actions.is_none()`），所以 hook 里把语言排在
   复制 chip 左边，否则代码块会失去语言标注。
8. **复用 `copied_flash`。** 命令卡片原来的「已复制」1.5 秒反馈机制直接给所有按钮共用
   （字段从 `copied_command` 改名，语义从「工具调用 id」变成「按钮 id」）。

**验证**

- 纯函数（`crates/qcode-gui/tests/copy_text.rs`，12 条）：标题/引用/列表标记、配平与不配平
  的强调、单个 `_` 与 `*` 不动、行内代码与落单反引号、链接与图片、fenced code 正文逐字、
  **折叠代码块仍复制全部 30 行**、空行折叠、表格原样、整轮拼接与缺一半、空输入。
- 快捷键判定（`crates/qcode-gui/tests/shortcuts.rs`，3 条）：
  `copy_and_select_all_take_the_platform_modifier_alone`（各自平台修饰键；**另一平台的修饰键
  不算**——macOS 的 `Ctrl+C` 是 shell 的打断，读成「复制对话」会把按键吞掉）、
  `shifted_or_alted_copy_is_not_the_transcript_shortcut`、`an_unmodified_key_is_not_a_shortcut`。
- 交互部分**自动化够不着**：`crates/qcode-gui/tests/` 里没有任何窗口（`spike.rs` 开头解释了
  为什么不能在 lib 里开 `#[test]`），拖选 + 剪贴板必须手过一遍。

  | 做什么 | 应该看到 |
  | --- | --- |
  | 拖选一段回答，`Cmd+C`（Linux `Ctrl+C`），粘到别处 | 剪贴板是这段渲染后的文字，不含 `**` |
  | 跨两条消息拖选 | 按阅读顺序拼接，中间换行 |
  | 不选任何东西按 `Cmd+C` | 剪贴板不变（先到处复制一段再试） |
  | 在输入框里选中文字按 `Cmd+C` | 复制的是输入框里那段；对话选区不受影响 |
  | 鼠标在对话里，`Cmd+A` | 整个可见对话被选中 |
  | 点代码块右上「复制」 | 按钮变「已复制」约 1.5 秒，剪贴板是纯代码（无围栏、无行号） |
  | 折叠一个 >20 行的代码块，再点「复制」 | 剪贴板是**全文**，不是折叠后那 12 行 |
  | 框选同一个折叠代码块，`Cmd+C` | 只有可见的 12 行（这是有意的差异） |
  | 点回答下「复制」 | 纯文本：标题不带 `#`、代码块带正文、链接带 url |
  | 点「Markdown」 | 源样，`**` 和围栏都在 |
  | 点「整轮」 | 先是用户的问题，空行，然后是回答；不含思考与工具行 |
  | 回答里含 `snake_case`、`2 * 3 * 4`，点「复制」 | 这两处一个字都不少 |
  | 折叠/展开思考过程后 | 复制按钮仍在原位，回答的可选文本跟着变 |

**待定**

- **边缘自动滚屏（原讨论的 Q3）没做。** 框架有 `AutoScroll`，但在虚拟列表下「拖到边缘
  继续滚，新进入的行算不算进选区、按什么顺序」都要重新定义，是这条线里最贵的一项。
  现在的手工补救是按钮（整轮、单块），不是选择本身。
- **右键菜单没做。** 发现性靠三个常驻 chip 补上了；右键菜单要处理「点在选择上不应关掉
  选择」的边界，且会引入第一次使用 `ContextMenu`。
- **`Cmd+A` 只在窗口的选择状态已经存在时生效**（`TextSelection::select_all` 找不到状态就
  no-op）。该状态由 `TextView` 首次绘制时创建，所以实际上是「第一帧之后都有」，但这也是
  为什么没有把它做成更强的「全选到剪贴板」——那需要另一套状态。
- **流式输出期间没有 chip**：那段文字还没有成为 `Block::Assistant`，没有稳定 id 可挂。
- **没有 `Shift+方向键` 的键盘选择**：框架把重活放在鼠标事件里，做它等于自己实现一套
  光标模型，这次不做。
- **表格的纯文本形式仍是管道表**。想变成对齐的列，需要按渲染宽度补空格，那是
  `plain_text_from_markdown` 目前故意不碰的东西。

## ADR-0073：退出时杀掉 run_shell 的进程组——detached 线程让 `kill_on_drop` 永不执行（2026-11-12）

**状态**：已实现（2026-11-12）。

**问题**

用户反馈：AI 查 GitHub 状态卡很久时退出 qc，相关进程没被杀死。

查下来不是「取消没送到」——取消链路是好的（`signal_group` 打整个进程组，
SIGTERM→SIGKILL）。真正的原因是**退出路径根本没有清理这一步**：

1. 引擎跑在 detached 线程上（`runtime.rs:188`，`JoinHandle` 被丢弃、从不 join）。
2. `cx.quit()` → `run()` 返回 → `main` 返回 `ExitCode` → 进程结束 → 该线程被
   **直接终止，不展开栈**。
3. 于是 `run_shell` 与 MCP 上的 `kill_on_drop(true)`（`shell.rs`、`mcp/lib.rs`）
   **在退出场景里一次都不会跑**。
4. 全仓没有任何退出钩子：`on_app_quit` / `on_window_closed` / `impl Drop for Shell`
   全部 grep 为空。

`run_shell` 的子进程因此必然活过 qc，两个条件缺一不可：

- **stdin 是 `/dev/null`**（`shell.rs`）：等不到 EOF；
- **不写输出**：不会撞上已关闭的管道拿到 EPIPE。

正好是「卡在网络上」的形状。实测（父进程 `_exit` 跳过一切析构）：以 `run_shell`
的确切方式（`stdin=DEVNULL`、管道、`process_group(0)`）起的
`sh -c 'sleep 600'` **连同孙子一起存活**，被重挂到 init。

**一个必须记下来的错误结论**

排查时我先断定「MCP server 必然泄漏」，因为它是长驻连接、无进程组、无关闭、
`kill_on_drop` 又跑不到。**这是错的。** 现场 `ps` 里 `wordcraft-cli mcp` 的父
进程是一个**活着的** `qc gui`，属于正常状态；而再查上一次那对进程，两个都已消失
——wordcraft 跟着 qc 死了。

机制是 **stdin EOF**：MCP 的 stdin 是管道，qc 一死内核关掉 fd，守规矩的 MCP
server 读到 EOF 就自己退出。实测复刻（`wordcraft-cli mcp`，管道 stdio，父进程
跳过析构）：**t+1s 已退出**。所以这不是 `kill_on_drop` 的功劳，也不需要修。
`run_shell` 之所以不同，正是因为它把 stdin 指向 `/dev/null`。

**决定**

1. **进程级登记表 + 同步 SIGKILL**（`qcode-tools/src/child_groups.rs`）。
   `run_shell` 起进程后登记该进程组、命令被 reap 时由守卫注销；退出时
   `kill_all()` 把还登记的组整组 SIGKILL。同步且微秒级，能塞进
   `on_app_quit` 的预算（`gpui::SHUTDOWN_TIMEOUT` = **200ms**）——这个预算是
   硬约束，所以优雅关闭（等 MCP 握手之类）在这里根本不可行。
   - **SIGKILL 而非 SIGTERM**：整个回合的取消路径已经先发过 SIGTERM 并给了
     2 秒；还留在这里的说明完全没走到那条路径，没有「以后再来升级」的人了。
   - **pid==0 不入表**：`kill(0, …)` 打的是 qcode 自己的进程组，正好是这次要
     避免的反面。
   - 守卫的 `Drop` 负责注销，是为了**避免 pid 复用**：一个已结束的组若留在表
     里，退出时可能 SIGKILL 掉一个无关的新进程。
2. **挂 `on_app_quit`，一处覆盖所有退出路径**（`gui/run.rs`）。包括托盘退出、
   设置窗口退出、平台自己发起的退出。返回的 `Subscription` **必须 `mem::forget`**
   ——它是「观察者还注册着」的唯一凭据，落出作用域就等于立刻注销，清理会静默失效。
3. **`PR_SET_PDEATHSIG` 作为兜底**（`shell.rs` 的 `pre_exec`），**仅 Linux**
   （`#[cfg(target_os = "linux")]`）。覆盖退出钩子跑不到的情况：崩溃、OOM、
   `kill -9`。
   - 必须是 `target_os = "linux"` 而不是 `unix`：`libc::prctl` 只在 libc 的
     `linux_like` 下定义，macOS 上根本没有 `PR_SET_PDEATHSIG` 这个常量。第一版
     写成了 `unix`，macOS 会编译失败。
   - **诚实说明它的边界**：`PR_SET_PDEATHSIG` 不跨 `fork` 继承，只保直接子进程
     （sandbox 包装器或 `sh`），孙子照样被重挂。所以它是兜底，**不是**修复本身。
   - 真正完整覆盖异常退出的是 cgroup v2 的 `cgroup.kill`，但**它是 Linux 内核
     特性，非 Linux 上不存在**，且要用户委托权限，这次不做。

**跨平台说明（cgroup v2 不是可移植方案）**

| 平台 | 退出钩子（`kill_all`） | 异常退出的兜底 |
| --- | --- | --- |
| Linux | ✅ `kill(-pgid, SIGKILL)` | `PR_SET_PDEATHSIG`（只管直接子进程） |
| macOS | ✅ `kill(-pgid, SIGKILL)` | **无**。没有 cgroup，也没有 PDEATHSIG。 |
| Windows | ✅ `taskkill /PID <pid> /T /F` | **无**。见下 |

所以两件事要分清：**修复本身（登记表 + 退出钩子）跨平台成立**，因为它只依赖
「进程组 / 进程树」这种各平台都有的东西；**兜底只有 Linux 有**。

- Windows 的正确兜底不是 cgroup，而是 **Job Object**：`CreateJobObject` +
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`。最后一个句柄关闭（进程死掉）时内核杀掉
  job 里所有进程，且**覆盖整棵子树**——比 PDEATHSIG 还强。目前没做，`taskkill`
  是「取消」用的权宜之计（注释里也这么写着）。
- macOS 没有等价物。可选的只有非内核方案：子进程侧 `kqueue` 监听父进程
  `EVFILT_PROC`/`NOTE_EXIT`（要求子进程配合）、`launchd` 作业。都不够用，所以
  macOS 上老老实实接受「qc 崩溃时 `run_shell` 的子进程可能残留」。

**已验证的、和被否决的**

- `DRAIN_GRACE`（5s×2）与 GUI 的 5s 强退预算不匹配，取消路径最坏 ~14s。**没有
  改**：退出钩子已经无条件杀掉进程组，drain 随后立刻拿到 EOF，慢的那条腿不再
  决定泄漏与否。
- **没有让引擎线程可 join**：那才是最「正确」的修法（`kill_on_drop` 才真正生效），
  但要动引擎启动方式，且 200ms 装不下优雅关闭。留作后续。
- **MCP 不加进程组**：查清它靠 stdin EOF 自退后，这一项没有依据。为它加信号只是
  在解决一个不存在的问题。
- 窗口 ✕ / `Cmd+W` **不是退出**，是隐藏到托盘（ADR-0052）。那种情况下 qc 和工具
  都还在跑，属于设计行为——排障时要先用 `ps` 确认 `qc gui` 还在不在。

**验证**

- `qcode-tools/tests/child_groups.rs`（3 条，独立测试二进制：`kill_all` 排空的是
  **进程级**登记表，会和 `tools.rs` 里并发测试的子进程互相干扰）：
  `kill_all_takes_down_the_whole_group_including_grandchildren`（shell 先 `echo $!`
  报出孙子 pid，`kill_all` 后 leader 与孙子都必须消失——只杀 leader 的实现在这一
  条上会红，已用变异验证：把 `kill(-pid)` 改成 `kill(pid)` 后该测试失败并指名
  那个孙子）、`a_reaped_group_is_not_left_in_the_registry`、
  `a_zero_pid_never_enters_the_registry`。
- `qcode-tools/tests/pdeathsig.rs`（3 条）：测试二进制把自己 re-exec 成 helper 角色
  （起一个子进程后阻塞），再由测试 `kill -9` 它，子进程的生死就是观测结果。
  有 signal → 必须死；**无 signal → 必须活**（对照组，证明断言能分辨，否则前一条
  是巧合）。`helper_spawns_one_child_and_blocks` 在正常跑测时是空操作。
- `child_groups.rs` 内的两个纯逻辑用例：守卫登记/注销、pid 0 不入表。
- **未验证的部分**：`pre_exec` 那一行的「接线」本身无法自动化——`PR_GET_PDEATHSIG`
  只能由进程自己查询，外部无法读出。它只有编译期保证 + 上面那个机制测试。

**待定**

- **手工回归**（自动化够不着，窗口没有可用的 headless 测试）：
  让 AI 跑一条卡住的命令（`ping -c 100 1.1.1.1` 即可），从托盘退出 qc，确认
  `ps -eo pid,ppid,cmd | awk '$2==1'` 里没有残留的 `ping`/`sh`。
- 异常退出（`kill -9` qc）后**孙子**仍会残留，这是第 3 条的已知边界。要真正收口
  得上 cgroup v2。
- GUI 的 5s 强退与工具 14s 最坏取消路径仍然不对称。现在无害（钩子保证进程组会
  死），但「最后一个回合正常落盘」这件事在慢命令上仍可能来不及——日志是权威源
  （ADR-0057），所以没有恢复问题。

## ADR-0074：`kill -9` qcode 之后不泄漏进程——三平台各用各的内核原语（2026-11-12）

**状态**：已实现。Linux 有自动化回归（`tests/procguard.rs`）；macOS 由同一份测试在
CI 的 `macos-latest` 上覆盖，但**尚未在真机上观察到通过**；**Windows 仍然零运行时
验证**，文末清单必做。

**问题**

ADR-0073 收掉了「正常退出」这条路：`on_app_quit` 里 `kill_all()` 同步 SIGKILL
所有登记的进程组。但它解决不了**退出钩子根本不跑**的情况——崩溃、OOM、
`kill -9`。数据流停在「至少有一个人得活着才能清理」，而那个前提恰好被打破。

对比 ADR-0073 的结论，缺口在三平台上大小完全不同：

下表的「缺口」是第一版的状态，保留下来是为了说明**为什么**落到这套方案上；最终
形态见「决定」一节和后面的补记。

| 平台 | 缺口 |
|---|---|
| Linux | 有 `PR_SET_PDEATHSIG`（仅组 leader，孙子仍逃）；退出钩子管正常退出 |
| Windows | **什么都没有**。`signal_group` 是 `taskkill /T /F` 子进程，靠 qcode 活着才能跑 |
| macOS | **什么都没有**。libc 连 `PR_SET_PDEATHSIG` 常量都不给 Apple 目标 |

**一个先被否掉的方案：靠 systemd 兜底**

排查时看起来最省事的一条：qc 本来就跑在 systemd 的 transient scope 里，
`KillMode=control-group`，那 qc 死了 systemd 不该把 cgroup 里剩下的都收了吗？

实测否掉。这台机器上确认属性确实是 `KillMode=control-group`、`Delegate=no`：

```
systemctl --user show app-ghostty-surface-transient-910552.scope
  → Slice=app.slice  Delegate=no  KillMode=control-group
```

但 `systemd-run --user --scope` 起一个 `setsid sleep 602 &` 后再让 scope 主进程
退出，实测 **t+1/2/3/5s 累积仍存活**，而 `/proc/<pid>/cgroup` 显示它仍挂在这个
「已死」的 scope 里，是该 scope 唯一的成员。

原因：**scope 的存活由 cgroup 是否为空决定，不是由主进程是否退出决定**。残留
进程让 cgroup 一直非空，scope 就一直「活着」，`KillMode` 永远等不到触发的时机。
`Type=simple` 的 service 才会在 main 退出时停止单元。桌面应用不这样启动，所以这
只能算部署选项，不是修法。

**决定**

一句话：**让「该杀谁」的决定权落在 qcode 之外——优先给内核，其次给一个活过
qcode 的进程。** 三平台各用各的原语，`child_groups` 仍是共同的登记来源。

1. **Windows：每命令一个 job object + `KILL_ON_JOB_CLOSE`**
   （`qcode-tools/src/win_job.rs`）。
   - 句柄由 qcode 私有，不可继承（`CreateJobObjectW` 传 null 属性 + null 名字），
     所以「最后一个句柄关闭」= 「qcode 进程结束」，无论怎么结束。内核动手，不看
     qcode 是否还在跑代码。
   - **每命令一个 job，不是一个全局 job**：`TerminateJobObject` 没有「只杀某个
     进程」的形式，共享 job 会让取消一个回合时误杀同时在跑的其它命令，而
     `run_shell` 明确允许并行（每回合一个调用）。每命令一个 job 还顺手解决了
     ADR-0073 遗留的预算问题——原来 `kill_all` 每组要 fork 一个 `taskkill`，
     200ms 预算下几个组就超；现在是一组一次系统调用。
   - 注册点在 `spawn()` 之后（`LiveGroup::register_in_job`），因为 job 必须在
     子进程能启动任何东西之前就存在。
   - **正常结束时不关句柄**（`Drop` 里查 `ActiveProcesses`）：句柄一关就是
     `KILL_ON_JOB_CLOSE`，等于杀掉该命令留在后台的东西，这与 Unix 现有行为不
     一致。只有确认 job 已空（`Some(0)`）才关；查询失败按「可能还活着」处理。
     留下的句柄由进程终止时内核关闭——正是要的保证。
   - `taskkill /T /F` 保留为**兜底**：没有 job 的 pid（注册失败、或根本没注册）。

2. **macOS：guard 进程**（`qcode-tools/src/procguard.rs`）。
   - `qc` 用哨兵参数 `__qcode-procguard <qc_pid>` 重新 exec 自己（与
     `windows_acl` 的 helper 同一套既有模式）。`main` 在 **tokio runtime 和 clap
     之前**应答它——guard 是一个裸的 `kqueue`+`read` 循环，给它异步运行时既是
     浪费也是它不该有的线程来源。
   - guard 自己 `setsid()`：脱离 qc 的会话和进程组，所以针对 qc 的
     `kill(-pgid)`/挂断都碰不到它。（因此 spawn 时**不能**设 `process_group(0)`，
     否则它是组长，`setsid` 会 EPERM。）
   - 两个死亡触发器，任一即可，都进同一个 `finish`：`kqueue` 的
     `EVFILT_PROC`/`NOTE_EXIT`（内核事件，`kill -9` 也触发，不是轮询），以及
     stdin 管道 EOF（qc 死则内核关fd；Rust 的 pipe 是 CLOEXEC，孙子不会误留写端）。
   - **正确性的关键不是「能不能发现」，而是「发现时它知道的事全不全」**。
     qc 登记完一个组然后死掉，如果 guard 只按已读到的表去杀，最后那次登记就被漏
     掉——同一个 bug 换了个更小的窗口。所以 `finish` **先排空管道再杀**。这成立
     是因为管道里已写入的字节在写端消失后仍在：对一个已经不存在的进程，不存在
     「还在飞行中」的写。排空后不可能漏。
   - 登记/注销由 `LiveGroup::register`/`Drop` 通过管道发 `+pgid` / `-pgid`；
     文本行协议，半行只是被忽略，不会解同步。
   - 惰性启动：`main` 调用 `procguard::enable()` 只置标志，首次登记才真的 spawn。
     这同时让**测试二进制不会把自己当 guard 拉起来**（测试的 `current_exe` 是
     harness，不认识哨兵参数）。
   - 管道写失败 → 丢弃 channel（关写端 → 那个 guard 自己退），下次登记重启一个。
     静默失去保护比重启糟。

3. **Linux：`PR_SET_PDEATHSIG` + 同一个 guard**（2026-11-12 补记，见下）。
   `PR_SET_PDEATHSIG` 不跨 `fork` 继承，只覆盖组 leader，所以单靠它 `kill -9` qc
   之后孙子仍会逃。guard 在 `spawn()` 后登记整组，qc 一死就 `killpg` 打整组——孙子
   在这里被覆盖。两层并存不是冗余：`PDEATHSIG` 不需要任何别的进程活着、也不等调度，
   且 guard 被杀时它仍然有效。

**一个顺带修好的、原本没人注意的洞**

`kill_all` 只挂在 GUI（`qcode-gui/src/run.rs` 的 `on_app_quit`），CLI/TUI 侧
**根本没有退出钩子**。之前这意味着 TUI 里正常退出也可能留下进程组。

新机制把这条顺带补上了，因为它不依赖 qcode 的退出钩子：Windows 进程结束时内核关闭
job 句柄，macOS/Linux 结束时管道 EOF（或 pidfd 可读）唤醒 guard——两者在**任何**退出
路径上都生效，包括正常退出。补记之后 Linux 也在这一列里，所以三个平台的 CLI/TUI
退出路径现在都不再依赖那个只挂在 GUI 上的钩子。

**顺带修掉了一个既有测试泄漏**

`tests/pdeathsig.rs` 的对照组（`without_the_signal_the_child_outlives_its_parent`）
每次跑都会漏一个 `sleep 600`：它 SIGKILL 的是 shell 的 pid，而 shell **fork** 了
`sleep`。一个关于「进程不泄漏」的测试自己泄漏，而且残留进程正好会出现在本 ADR
回归清单用的那些 `ps` 判读里，所以改成让 helper 把子进程放进独立进程组、清理时
`killpg`。**改前改后都验证过**：改前跑完 `pgrep -af 'sleep 600'` 有残留，改后为零，
两条断言的结论（有信号必须死 / 无信号必须活）没有变。

**补记 2026-11-12：Linux 从「不做」改成「做」，用同一份 guard**

第一版里 Linux 是「保持原样 + 把话说清」，理由是要么上 PID namespace（重、改变子
进程的 pid 视角、依赖 userns），要么再写一套。两个理由都不成立：

- guard 的**主体与平台无关**——状态机、管道协议、spawn、排空后 `killpg`、退出码，
  全是同一份代码。只有「告诉我这个 pid 什么时候死」这一个原语不同：macOS 是
  `kqueue` + `EVFILT_PROC`/`NOTE_EXIT`，Linux 是 `pidfd_open`
  （`syscall(SYS_pidfd_open)`，5.3+）加 `poll`，`poll` 同时盯 stdin。于是门从
  `cfg(target_os = "macos")` 放宽成 `cfg(any(target_os = "macos", target_os = "linux"))`，
  只在 `mod watch` 分叉。
- 代价只是多一个常驻小进程（惰性启动，第一次登记才 spawn），与 macOS 相同。

`pidfd_open` 不是 libc 的函数，只有 syscall 号（四个出货 Linux triple 都有：x86_64
gnu/musl、aarch64 gnu/musl），所以走 `libc::syscall`。拿不到就退化成只看 stdin EOF——
`poll` 会把 fd 为 -1 的条目忽略，不需要第二个分支。

改完之后三条平台的差距只剩一处：**Unix 两个平台杀的是进程组，`setsid` 自己的孩子能
逃**；Windows 的 job 不看进程组，逃不掉。收口要靠 cgroup v2 的 `cgroup.kill`，而它需要
可写的 `cgroup.subtree_control`——实测这台机器上 qc 所在 scope 是 `Delegate=no`（见上文
systemd 那一节），桌面应用不能指望这个前提。所以这是「已知且记录」的差异，不是待办。

**验证的代价反过来了**

有了 Linux 的运行时，就能**自动化**验证「`kill -9` qc 之后整组消失」——正是 ADR-0073
当初列进「只能手工回归」的那一条。`tests/procguard.rs` 因此是 `harness = false` 的真
二进制：guard 角色是「自己被重新 exec 成 `__qcode-procguard <pid>`」，而 libtest harness
会把那个参数当成未知 flag，永远走不到分派。同一个文件 `cfg(unix)`，所以 CI 的
`macos-latest` 现在会**真跑**一遍 macOS guard，不再只是编译它。

**被否决 / 未采用的**

- **systemd scope 兜底**：见上，实测不成立。
- **共享一个全局 job**：`TerminateJobObject` 会误杀并发命令。
- **Windows 上 `CREATE_SUSPENDED` + assign + resume** 来彻底消除「spawn 到
  assign 之间」的窗口：`std::process::Command` 不暴露线程句柄，无法 resume。窗口
  是两次系统调用之间（微秒）对着一个还没启动完的子进程，接受并记录为残余。
- **macOS 用 `fork()` 而不是 exec 自己**：多线程进程 fork 后只能做
  async-signal-safe 的事，要写成无分配、无 panic 的 C 风格代码，比多一个 exec 更
  容易出 UB。
- **让 guard 也跑 Linux**（用 pidfd 替代 kqueue）：能补上第 3 条的缺口，但本次按
  约定不动 Linux。

**验证**

- `cargo test -p qcode-tools`：130 lib + 各集成测试全绿。
- `procguard` 的 6 条纯逻辑用例（跨平台跑）：登记/注销往返、
  **一行跨两次 read 不得提前生效**、写端中途死亡留下的半行必须被忽略（截断的
  pid 可能正好是活的）、pgid 0 永不登记（`killpg(0,…)` 会打 guard 自己）、
  垃圾行不改状态、注销不存在的组无害。
- `tests/child_groups.rs` 3 条照旧（`kill_all` 端到端：leader 与孙子都必须消失）。
- **macOS 代码在这台开发机上没有编译器**（无 rustup、无 Apple target），所以加了
  `scripts/check-macos-guard.sh`：从源码里**逐字抽出**真实的 `mod guard` 函数体
  （不手抄，无法漂移），配一份按 libc 0.2.191 `unix/bsd/apple` 签名复刻的 `libc`
  垫片，`rustc --emit=metadata` 做类型检查。做过反向对照：把 shim 里
  `kevent` 的 `eventlist` 改成 `*mut i32`，立刻 E0308。**它只证明类型对得上，
  不证明常量值对、也不证明运行时行为。**
  - 说清楚它的定位：CI 的 `test` job 矩阵里有 `macos-latest`，所以 macOS 侧**本来
    就有真编译器覆盖**（`ci-clippy.sh` + `ci-test.sh`）。这个脚本不替代那件事，
    它给的是 **Linux 上的即时反馈**和**对 `mod guard` 提取位置的自检**（模块结构
    一变就红，而不是悄悄抽到一段不相干的代码）。已接进 CI 的 ubuntu job。
- **`kill -9` 的端到端回归（`tests/procguard.rs`，`cfg(unix)`，`harness = false`）**：
  三个角色同一份二进制——guard（被重新 exec 成 `__qcode-procguard <pid>`）、
  inner（扮演 qc：`enable()`、起 `sh -c 'sleep 600 & echo $!; wait'` 于独立进程组、
  登记、阻塞）、outer（`kill -9` inner 后断言）。两条：
  - 有 guard → leader 与**孙子**都必须消失；
  - **无 guard → 孙子必须存活**（对照组；它要是也死了，上一条就不能归因于 guard）。
  - 变异验证过：把 `LiveGroup::register` 里的 `procguard::register(pid)` 去掉，
    该测试立刻失败并报出「leader gone: false, grandchild gone: false」。所以它不是
    一条会自己变绿的空断言。
  - 跑完顺带查过：没有残留的 `sleep`，也没有残留的 guard 进程（guard 清理完自己退出，
    否则每跑一次 qc 就多一个常驻进程，那是新 bug）。
- Windows 侧仍然只有编译保证：`cargo clippy --target x86_64-pc-windows-gnullvm
  -p qcode-tools --all-targets -- -D warnings` 干净（**改前是红的**，见 ADR-0075）。


**手工回归清单**

Windows（**必须做**，这是唯一没有任何运行时验证的一侧）：

1. `qc gui`，让 AI 跑 `ping -n 100 127.0.0.1`。
2. 任务管理器 → `qc.exe` → **结束任务**（不是关窗口）。
3. `tasklist | findstr ping` 必须为空，`sh`/`pwsh` 也要为空。
4. 重复一次但用 `kill -9` 等价的强杀，确认同样干净。
5. **反向对照**（证明断言能分辨）：把 `win_job::Job::adopt` 里的
   `AssignProcessToJobObject` 注释掉，重跑第 1–3 步，`ping` 必须残留。
   不残留说明测的不是这条路径。
6. **后台进程语义**：跑 `start /b timeout /t 60`，等命令正常结束，确认那个
   `timeout` 仍在（`Drop` 里 `ActiveProcesses != 0` 时不关句柄）——这是与 Unix
   保持一致的行为，被误改成「关句柄」的话这一步会红。
7. **取消路径**：跑一条长命令，点取消，确认只有它死、并发的另一条命令还活着。

Linux / macOS（`tests/procguard.rs` 已经覆盖了机器能验证的部分，包括 `kill -9`
之后孙子必须死、无 guard 时必须活。下面这些是**测试够不到**的：真 GUI、真
`run_shell`、真平台调度）：

1. `qc gui`，让 AI 跑 `ping -c 100 1.1.1.1`，然后 `kill -9 <qc pid>`。
2. `ps -eo pid,ppid,pgid,command | grep -E 'ping|sh -c'` 必须为空。
   重点看**孙子**：这是 `PDEATHSIG` 漏掉的那一层，`killpg` 该补上。
3. 确认 guard 自己也退出了：`ps -eo pid,command | grep __qcode-procguard` 为空。
   （它应在 EOF / `NOTE_EXIT` / `pidfd` 可读之后自行退出，不该留下常驻进程。）
4. **从托盘正常退出**（不是 `kill -9`）：确认 `kill_all` 仍然生效，且没有 guard 残留。
5. **CLI/TUI 也要试**：`qc run` 起一条长命令，退出，确认进程组同样被收掉——
   这条路径上**没有**退出钩子，全靠 guard，和 GUI 走的不是同一条代码。
6. **`setsid` 逃逸（已知差异，不是回归）**：跑一条自己 `setsid` 的命令，确认它
   **会**残留。这不是要修的 bug，是 `killpg` 的边界；记录下来是为了下次有人报
   「还是泄漏」时能立刻分辨类型。
7. macOS 额外记一笔：`tests/procguard.rs` 会在 CI 的 `macos-latest` 上跑，但**本次
   还没有在任何 macOS 机器上观察到它通过**。CI 第一次跑它时要看结果；红了先怀疑
   guard，不要怀疑测试。

三平台通用：

- `ps -eo pid,ppid,command | awk '$2==1'` 里不该出现 `sh -c`/`ping`/`cargo`。

**残余风险（诚实清单）**

- **Unix 两个平台的 guard 保证是用户态的**：guard 被杀（`kill -9` 两个 pid）就没了；
  guard 自己崩溃则静默失去保护（管道写失败会重启，但崩溃与写失败不是一回事）。
  Windows 的内核保证没有这个问题。
- **Unix 上 `setsid` 能逃**：guard 杀的是进程组。Linux 的 `PDEATHSIG` 也一样只到
  leader。Windows 的 job 不看进程组，是三者里唯一没有这个洞的。收口要 cgroup v2，
  前提（可写 `cgroup.subtree_control`）桌面应用不能保证。
- **Linux `PDEATHSIG` 与 guard 之间有先后**：`PDEATHSIG` 立即杀 leader，guard 稍后
  杀整组。所以在极短窗口里 leader 已死、孙子仍在——最终都为 0，但「瞬间」不等于
  「同时」。
- **macOS / Linux pid 复用**：`Drop` 注销与进程真正退出之间有窗口，同一风险
  ADR-0073 已记录在 `child_groups` 上，未新增。
- **Windows spawn→assign 的微秒窗口**。
- **Linux 无 `pidfd_open` 的内核（<5.3）**：退化成只看 stdin EOF。功能等价，但如果
  某个子孙意外持有了写端（Rust 不会，别的语言写的沙箱包装可能会），就会退化成
  「qc 死而不清理」。这条只能靠 EOF 之外的信号兜，没做。
- **Windows 的运行时验证仍然是零**：这一侧的每一条都是我按文档推的，只有编译器
  点头。上面清单里 Windows 那 7 步不是可选项。
- 机器断电 / 内核 panic：三个机制都不承诺。

## ADR-0075：`cfg` 门要落在**用者**和**被用者**同一侧——`mod linux` 的 `test` 让 macOS/Windows 的测试构建变红（2026-11-12）

**状态**：已实现。

**问题**

`cargo clippy --target x86_64-pc-windows-gnullvm -p qcode-tools --all-targets`
在 Windows 上报 17 个错误，全是 `cannot find value LL_EXECUTE` 一类，指向
`sandbox/linux.rs` 和 `sandbox/tests.rs`。先确认不是自己带进来的：`git stash -u`
掉全部改动复跑，报的是同一批错误。所以是既有缺陷，而且意味着 CI 的
`windows-build` 里 `cargo test --workspace --no-run` 那条也红着。

根因是**同一件事的两侧用了不同的门**：

| 位置 | 门 | 后果 |
|---|---|---|
| `sandbox/mod.rs` | `#[cfg(any(target_os = "linux", test))] mod linux;` | **任何平台**的 `cfg(test)` 构建都编译整个模块 |
| `landlock_pty_dir_rights` 等函数 | `#[cfg(any(target_os = "linux", test))]` | 同上，会被编译 |
| `LL_EXECUTE` … `LL_READ_DIR` 五个常量 | `#[cfg(target_os = "linux")]` | **只有 Linux** 才存在 |

于是非 Linux 的测试构建里，函数在、它引用的常量不在。错误报在**用者**身上
（`landlock_pty_dir_rights` 的函数体），让人以为是那几行写错了，而不是门装错了。

**决定**

把五个常量放宽成 `#[cfg(any(target_os = "linux", test))]`，与用者对齐；**不是**
把 `mod linux` 的 `test` 去掉。

理由：那个 `test` 是有意的。Landlock 权限位的组合运算（`landlock_file_rights`、
`landlock_handled`、pty 目录权限）是纯整数运算，跟内核无关，在任何机器上都值得断言；
`landlock_pty_dir_rights` 的文档注释也写明了「抽取出来是为了让测试断言这个表达式本身，
而不是手抄一份副本」——手抄副本正是当初 `IOCTL_DEV` 缺失能躲过测试的原因。去掉
`test` 会让这些断言只剩下 Linux 上能跑，等于把既有覆盖砍掉。

在 `sandbox/mod.rs` 和常量定义处都加了注释，说明「模块在非 Linux 的测试构建里也会
编译，所以模块内任何只属于本平台的东西都必须自己带 `cfg`」——下一次有人在这里
加 Linux 专属项时，坑就写在旁边。

**验证**

- 改前：Windows `--all-targets` 17 个错（`git stash` 证明与本次改动无关）。
- 改后：`cargo clippy --target x86_64-pc-windows-gnullvm -p qcode-tools --all-targets
  -- -D warnings` 干净。
- 没有放宽任何运行时行为：`cargo clippy --workspace --all-targets` 与
  `cargo test --workspace` 在 Linux 上照旧全绿（1156 passed）。
- 这条缺陷是顺带发现的，与 ADR-0074 的进程清理没有因果关系；写在这里是因为它们
  同一个提交，且都是「一个门装错地方导致另一个平台静默变红」这一类。

## ADR-0076：GNOME 的 dock 图标来自 `.desktop`，不是 `_NET_WM_ICON`（2026-11-12）

**状态**：已实现。代码（`window_icon.rs`）、桌面条目与安装脚本、发布归档都已落地；
用户目视确认待做。

**问题**

Linux 上 qc 的窗口在左侧 dock 里显示的是主题兜底图标（一个齿轮），macOS 和
Windows 都正常。README 里没有任何地方提过这件事，代码里也没有任何地方设置过图标。

**先查清楚是谁在画**

第一版的修法是给 `WindowOptions.icon` 塞一张 PNG——gpui 的 X11 后端只在拿到这个
字段时才写 `_NET_WM_ICON`，而整个 crate 的 `WindowOptions` 都是从
`..Default::default()` 构造的，`icon` 一直是 `None`。改完 `xprop` 确实能看到
`_NET_WM_ICON(CARDINAL) = Icon (256 x 256)`，**但 dock 里的齿轮没变**。

于是去查 GNOME 自己怎么选图标，而不是继续猜。`libshell-14.so`：

| 符号 | 出现次数 |
|---|---|
| `application-x-executable` | 有（兜底图标名，就是那个齿轮） |
| `fallback-app-icon` | 有 |
| `_NET_WM_ICON` | **0** |
| `meta_window_get_icon` | **0** |
| `meta_window_get_wm_class` | 有 |

`libshell` 是提供 `ShellApp` 的库。它**完全不读窗口图标属性**，只拿 `WM_CLASS`
去解析 `.desktop` 文件，再画那个文件的 `Icon=`。`_NET_WM_ICON` 只有「任务栏自己
画窗口图标」的桌面（KDE、Xfce、Cinnamon）才用。

而我们两样都没有：`WM_CLASS` 从未设置过（`WindowOptions.app_id` 是 `None`，且
X11 后端的创建路径根本不读它），系统里也没有 `qcode.desktop`。所以 shell 的图标主题
查询落在 `application-x-executable` 上——齿轮。

**决定**

三件事一起做，因为少一件都是齿轮：

| 层 | 做什么 | 谁读它 |
|---|---|---|
| `window.set_app_id("qcode")` | 写 `WM_CLASS`（`window_icon::identify`） | GNOME Shell 解析 `.desktop` 的入口 |
| `res/linux/qcode.desktop` | `Icon=qcode`、`StartupWMClass=qcode`、`Exec=qc gui` | shell 画图标、点图标启动 |
| `res/icons/linux/hicolor/*/apps/qcode.png` | 装进图标主题（8 个尺寸） | `Icon=` 的解析目标 |

`_NET_WM_ICON`（`window_icon::attach`）保留：它修的是 KDE/Xfce 那条路径，代价只是
一次 PNG 解码和 8 KiB 二进制体积。

**为什么必须三处同名**

`WM_CLASS` 的 class 段、`.desktop` 的 basename、`Icon=` 的值必须一致。改其中
任何一个而没改其他两个，表现是静默的齿轮。所以：

- `window_icon::APP_ID = "qcode"` 是唯一常量，`identify` 从它取值；
- 一个单测解析**随包提交的** `qcode.desktop`，断言 `Icon=` 和 `StartupWMClass=`
  都等于 `APP_ID`、`Exec=` 仍是 `qc gui`。变异（把 `StartupWMClass` 改成 `qc`）
  会让它立刻红。

**补记：装了条目还是齿轮，根因是 `TryExec`**

第一次装上后 `--check` 全绿，但 dock 还是齿轮。取实际像素才确认：dock 里没有任何
一块是品牌蓝 `#2F6BFF`（48×48 那个蓝块是 `#1A73E8`，别的应用）。

继续查 GIO，而不是猜缓存：`g_desktop_app_info_new_from_filename` 对**装好的那份
文件**返回 NULL——整条条目被判无效。二分出一行：

| 变体 | GIO |
|---|---|
| `Exec=qc gui`（`qc` 不在 PATH） | NULL |
| `Exec=/abs/path/qc gui` | 有效 |
| `Exec=/abs/path/qc gui` + `TryExec=qc`（不在 PATH） | **NULL** |

GLib 的规则是：`Exec` / `TryExec` 指向的程序**找不到就丢掉整条条目**（规范里
`TryExec` 明写「找不到则条目视为无效」）。条目被丢掉，窗口就没有可归属的应用，
shell 画兜底图标——**表现为「图标没装」，实际是条目无效**。

而当时的安装脚本只重写了 `Exec=`，把 `TryExec=qc` 原样留着，`qc` 又不在 PATH 上
（开发构建就是 `./target/release/qc`）。所以这个坑对**每一个从源码跑的人**都成立，
对发行包（`qc` 在 PATH）不成立——这也是它在测试里不容易暴露的原因。

修法：`TryExec` 与 `Exec` 永远一起重写（`try_exec=$exec_cmd`），并在 `--check` 里
断言两个键都能解析；单测再加一条「`TryExec` 必须与 `Exec` 的程序同名」，变异
（`TryExec=qc-gui`）即红。


**为什么不在程序里自动安装**

`qc` 自己能写 `~/.local/share`，但（a）以用户身份悄悄写用户的 home 是个产品决定，
不该由修图标顺手做掉；(b) 装上之后发现名字不对，用户既不知道发生了什么也不知道删
什么。改为 `scripts/install-linux-desktop.sh`：显式、可 `--uninstall`、可 `--check`
自查，`--system` 支持打包场景。`--check` 会读**每个在跑的实例**的 `WM_CLASS`——
只读第一个会读到旧二进制，把「没装好」误报成 OK。

**验证**

- 单测 4 条：PNG 解码/正方形/非空白、`attach` 真的写进字段、桌面条目三处同名。
- `xprop -id <win> WM_CLASS` → `WM_CLASS(STRING) = "qcode", "qcode"`（新二进制）。
- `desktop-file-validate` 通过；归档内容含 `qc` + `share/applications/qcode.desktop`
  + `share/icons/hicolor/<8 个尺寸>/apps/qcode.png`。
- Windows 两个 triple 经 `scripts/ci-cross-check.sh` 类型检查（`identify`/`attach`
  在那两个平台是恒等函数）。
- GIO 真值：`g_desktop_app_info_new_from_filename` 对装好的条目返回非 NULL
  （这是「条目有效」的唯一可信判据，`--check` 的规则与它对齐）。
- **未验证**：Wayland 会话下的表现（`attach` 被忽略，靠 `.desktop` + 图标主题，
  理论上与 X11 同路径），以及真机 dock 的目视确认。

## ADR-0077：日志写文件——GUI 与 CLI 共用 `logs/qc.log`（2026-11-12）

**状态**：已实现。

**问题**

日志只往 stderr 走，`logging.rs` 一共 13 行：

```rust
tracing_subscriber::fmt()
    .with_env_filter(EnvFilter::try_from_default_env().unwrap_or("warn"))
    .with_writer(std::io::stderr)
    .with_target(false)    // 没有模块来源
    .without_time()        // 没有时间戳
```

三个缺陷和一个更根本的缺口：

1. **`.desktop` 启动时零日志。** 桌面条目是 `Exec=qc gui` + `Terminal=false`，
   stderr 被 gnome-shell 继承，最好的情况是埋进 user journal（还得知道去翻
   gnome-shell 的 unit），最坏情况直接丢弃。而用户报 bug 最常见的场景恰恰是
   **点图标启动**——那个场景没有任何日志可看。这是本次要解决的核心问题，也是
   促使 CLI 也一并写文件的原因：`qc run` 从编辑器/脚本里被调用时同样没有人看
   stderr。
2. **`.without_time()`**：写进文件后无法定位「什么时候出的错」。
3. **`.with_target(false)`**：看不出是 GUI、kernel 还是 MCP 打的。
4. **没设 `with_ansi`**：默认是开的，重定向到文件时把 `\x1b[33m WARN\x1b[0m`
   原样写进去。实测抓到的日志里就是这个形状，`grep`/`less` 全是乱码。

另外两件同源问题，本文一并定规矩（但不做迁移，见「不在本文范围内」）：

- `eprintln!` **71 处**绕过订阅器：不受 `RUST_LOG` 管、不能过滤，`Redactor`
  也不作用于它们。
- `Redactor` 只在 2 个 `map_err` 里手调，**没有接成 `Layer`**，所以任何
  `tracing::warn!(%error)` 都可能把 API key 写进日志。而 `logging.rs` 的模块
  注释自己写着「defense in depth」。

**参照：cot_cli 的 log4rs 用法**

看了 `~/space/rust/cot_cli`（log4rs 1.4.0）。它的配置（`static/bin/log4rs.yml`，
40 行）只有一个 console appender，`file:` / `rollingfile:` 全是**注释掉的模板**，
用户 `~/.cot/log` 目录从未存在过——rolling 从未真正启用。它的亮点在规则而非规模：

| 规则 | 做法 | 判断 |
|---|---|---|
| 显式 `target: stderr` | log4rs 的 ConsoleAppender **默认 stdout**，踩过 | **抄** |
| appender 名字从 `stdout` 改成 `console` | 「名字叫 stdout 却写 stderr 只会误导下一个读配置的人」 | **抄** |
| pattern 带 `{f}:{L}` + 模块 | 定位到文件行号 | **抄** |
| 配置里 root=trace，实际级别由 `LOGGING_LEVEL` 定，默认 `off` | 配置定天花板，环境变量定实际 | **部分抄**（见下） |
| 三层降级加载（CWD → exe 目录 → 内嵌） | 开发可改、装完可改 | **不抄** |
| 升级不覆盖配置（`FORCE_COPY` 故意不含它） | 文件留给用户改 | **不抄**（我们没有这个文件） |

**决定：不引入 log4rs，只加 `tracing-appender`**

三个理由：

1. **log4rs 建立在 `log` facade 上，没有 span。** qcode 是异步 agent 运行时
   （turn → tool call → 子进程），现有 67 处 `tracing::*` 里跨度的父子上下文
   是排查「卡在哪一步」的主力工具；换过去等于丢掉它或手搓。
2. **换库要动 67 个调用点**，换来的「声明式配置 + 热重载」，`tracing-subscriber`
   都能给，而 `log` facade 给不了 span。
3. **配置重复**。qcode 已有 `config.toml` 和 GUI 设置页；再引一套 YAML +
   `serde_yaml` + 文件发现逻辑，是重复建设。

log4rs 真正多出来的只有 size-based 触发（`fixed_window` roller）。我们用
`warn` 默认级别，日志量是小数量级；daily + 保留 7 个已把总量封死，不值得为此
引入整套 roller。

**装配**

`logging::init(config_path)` 在 `Cli::parse()` 之后调用一次，装两层：

| 层 | 目标 | ANSI | 时间 | 模块 | 过滤 |
|---|---|---|---|---|---|
| A | stderr | **开**（终端可读） | 开 | 开 | `EnvFilter` |
| B | `logs/qc.log` | **关**（否则文件里永久带转义） | 开 | 开 | 同一个 `EnvFilter` |

级别来源，优先级从高到低：

1. `RUST_LOG`（保留现有逃生口，完整 `EnvFilter` 语法，含 `target=level`）
2. `config.toml` 的 `[log] level`
3. `warn`

**为什么 `[log]` 进 `config.toml` 而不是单独一个 log4rs 式文件**：qcode 已经有
配置系统和 GUI 设置页，级别应该能被那里管；多一个文件就多一套发现逻辑和一份
「存在即不写」的升级规矩。

**为什么默认写文件（不抄 `off`）**

cot_cli 默认 `off` 对**纯 CLI** 成立：用户就在终端里，stderr 看得见。对 qcode
**不成立**：GUI 用户没有终端可看。所以默认写文件，级别默认 `warn`。

**多进程：共享一个文件为什么是安全的**

qcode 是多进程的，这是本设计的硬约束：

| 进程 | 写日志吗 |
|---|---|
| `qc run` / `qc list` … | 是（各自独立主进程） |
| `qc gui`（workspace 切换会 re-exec） | 是 |
| `__qcode-procguard` | **否**——`guard_main_if_requested()` 在 `logging::init()`
  之前就 return，天然不写（既有设计，是对的） |
| MCP 子进程（任意命令） | 不写 `qc.log`，有自己的 `logs/mcp-*.log` |
| 终端里的 shell、`run_shell` 子进程 | 继承 stderr，不自己打日志 |

`tracing_appender::rolling` 的轮转机制让这件事安全：它**按日期新建文件名**
（`qc.2026-11-12.log`）而不是 rename 当前文件，用 `append(true).create(true)` 打开。
所有进程当天追加的是同一个 inode，`O_APPEND` 保证每次写入原子；跨天时各进程各自
算出新文件名，不需要协调。

两个已知残留风险，接受：

- **保留清理可能删到别人正开着的旧文件**。POSIX 上 unlink 后 fd 仍有效，无害；
  Windows 上会失败，清理跳过即可。
- **不用 `non_blocking`**。`tracing-appender` 的异步 writer 在进程被 `kill -9`
  时可能来不及 flush，而「用户强杀 / 崩溃」恰恰是想看日志的场景。日志量本来就低，
  用同步 writer 换「不丢」。

**稳定路径**：`RollingFileAppender` 的 `latest_symlink("qc.log")` 让
`logs/qc.log` 永远指向当天文件，`qc logs` 只认这一个路径，不用自己算日期。

**`resolve_data_dir` 的警告与 init 顺序**

`resolve_data_dir` 在数据目录不可写时会探测并**回落到工作区的 `.qcode-data`**。
日志文件就住在数据目录里，鸡生蛋。解法两层：

1. **用 `data_dir_hint` 而不是 `resolve_data_dir`**：前者只预测路径，不创建目录、
   不探测、不回退。否则 `qc skills list` 这种从不碰存储的命令会因为「可能要写日志」
   而在用户仓库里造出 `.qcode-data/`——沙箱里尤其难看。
2. **惰性文件层**：`init` 先装好 stderr 层和一个空的文件层（`MakeWriter` 背后的
   appender 是 `OnceLock`，未安装时写入丢弃），之后 `attach_file` 才建目录、装
   appender。这样 `attach_file` 之前的日志（含 `resolve_data_dir` 的回落警告）
   落在 stderr 上。它们落不进文件，因为文件的位置正是它要报告的东西——有意的取舍。

`attach_file` 失败走 `tracing::debug!` 而不是 stderr：沙箱里 `qc skills list` 可能
落到只读目录，一个从不写日志的命令不该为此报错。`qc logs` 会报告「没有日志文件」，
那才是用户会去看的地方。

`resolve_data_dir` 现在复用 `data_dir_hint` 的第一个候选，两者不会对「默认在哪」
产生分歧。

**不在本文范围内**

1. **`eprintln!` 的迁移**分两步做完：本文只产出分类表
   （`docs/design/LOGGING.md`），理由是 cot_cli 的教训——`output-streams.md` 审计了
   68 处，其中 **23 处是该留在 stdout 的产物**，一律改成日志是错的。分类之后的迁移见
   [ADR-0079](#adr-0079eprintln-迁移d-类-38-处归入-tracing2026-11-12)。
2. **`Redactor` 接成 `Layer`** 单独立项，已完成 —— 见 [ADR-0078](#adr-0078凭证脱敏接进日志格式化不再靠调用点记得2026-11-12)。
   当时拆开是因为它是安全项，与输出流归属是两件事，混在一起会让两边的回归测试都变模糊。
3. **`qc logs` 的 `--follow`** —— 当时划在范围外，理由「`tail -f` 已经够」站不住：
   那要先 `--path-only` 拿路径再喂给 `tail -f`，两个命令一次复制，而 `tail -f` 还不处理
   轮转与「文件尚未出现」。已实现，见 [ADR-0080](#adr-0080qc-logs---follow2026-11-12)。

**验证**

- 单测：级别优先级（`RUST_LOG` > `[log] level` > 默认）、`[log] level` 的非法值
  退回默认而不 panic、`log_path` 与 `attach_file` 返回同一路径、文件层写出的行
  **不含** `\x1b[`（ANSI 已关）而 stderr 层**含**。
- 手工：`qc logs` 打印路径与最近若干行；`qc doctor` 一行日志不写 stdout。
- 反证：把 `with_ansi(false)` 去掉，ANSI 断言必须红。

## ADR-0078：凭证脱敏接进日志格式化——不再靠调用点记得（2026-11-12）

**状态**：已实现。

**问题**

ADR-0077 让日志落盘并保留 7 天，`Redactor` 却仍然只在 **2 处** `map_err` 里手调：

```rust
.map_err(|err| redactor.redact(&err.to_string()))?;   // commands/gui.rs、commands/run.rs
```

它保护的是「返回给调用者、最终由 `main` 的 `eprintln!("error: …")` 打出来的字符串」，
**完全不经 `tracing`**。也就是说：日志通道一点保护都没有。任何
`tracing::warn!(%error, …)`、`?base_url`、`?Some(key)` 都会原样进文件，躺七天。

这不是假想。实现本文时用一份 `base_url = "http://127.0.0.1:9/v1?key=sk-…"`
的配置跑了一次必然失败的 turn，未脱敏版本的日志文件里有 **3 行明文**：

```
INFO qc::commands::run: provider resolved provider=e2e model=e2e-model base_url=Some("http://127.0.0.1:9/v1?key=sk-e2e-…")
WARN qcode_core::engine::stream: provider failed before the first token; retrying attempt=1
     error=network error: error sending request for url (http://127.0.0.1:9/v1?key=sk-e2e-…/chat/completions)
```

第二行是**引擎自己在重试**时打的，和 `run.rs` 那两处手调毫无关系——手调一处也挡不住它。
这就是「靠调用点记得」的根本缺陷：漏一处就开了口子，而这类代码恰恰会随重构增多。

**决定：在格式化阶段脱敏，包 `FormatEvent` 而不是包 writer**

`tracing` 的 `Layer::on_event` 拿到的是**只读**事件，没法改写字段或消息。能改的是渲染
结果，所以包的是 formatter：

```rust
impl<S, N> FormatEvent<S, N> for RedactedFormat {
    fn format_event(&self, ctx, mut writer, event) -> fmt::Result {
        let redactor = qcode_kernel::redact::current();
        if redactor.is_empty() {
            return self.inner.format_event(ctx, writer, event);  // 无 secret：直接透传
        }
        let mut line = String::new();
        self.inner.format_event(ctx, Writer::new(&mut line), event)?;
        writer.write_str(&redactor.redact(&line))
    }
}
```

三个设计点，每个都对应一种「换个写法就会漏」：

1. **包 formatter，不包 writer。** `fmt` 是分多次 `write_str` 吐一行的（时间戳、级别、
   模块、消息各一次）。writer 层的过滤器看到的是碎片，一个 secret 被切在两次写之间就
   匹配不到。先渲染成整行再扫，看到的就是用户会看到的东西。
2. **扫整行，不只扫 `message`。** key 走字段的场景是真实的：provider 把 key 放进
   查询串，而 `?base_url` 就是这么被记录的（上面那行 INFO）。`Debug` 格式化同样能
   带出 key（`?Some(key)`）。
3. **真的先渲染一遍。** 代价是每条事件一次 `String` 分配。默认 `warn` 级别下这是
   小数量级，而正确性不能省。没有 secret 时走提前返回，连分配都省掉——CLI 没解析到
   key 的常见情况零开销。

**决定：脱敏搬到 `qcode-kernel::redact`，在 `assemble` 里安装**

原本 `Redactor` 在 `crates/qcode/src/logging.rs`（bin crate）。问题是**装不上**：
`qcode-kernel` 不能依赖 bin crate，而 secret 是在 `config::resolve` 之后才知道的。

更要紧的是**入口不止两个**。`qc acp` 是长驻无头服务，所有 provider 错误都进日志，
却从来没有装过 secret——正是「靠记得」必然漏掉的那一个。逐个入口补调用点是治标的，
下一个入口还会漏。

所以搬到 `qcode_kernel::redact`，并在 `assemble` 里装：

```rust
let resolved = config::resolve(&loaded, &overrides, &|key| std::env::var(key).ok())?;
redact::install(resolved.api_key.clone());
```

`assemble` 是 `run` / `tui` / `gui` / `acp` **唯一必经**的地方。装在这里，忘记是不可能
的，这就是 `qcode-core::paths::log_dir` 那条理由的重演：跨 crate 的横切关注点要落在
所有调用方都依赖的那一层。

**决定：secret 集合只增不减**

`install` 取并集而不是替换。`qc gui` 在设置页换 key 后重新解析（`gui_rebuild_hook`
是原地换引擎，不是重启进程），此刻手里是新 key，但**几十毫秒前捕获的错误字符串里
可能还是旧 key**。忘掉旧的正好把这条要堵的漏给重新打开。

**两处手调保留，它们不是冗余**

`error: {err}` 走的是 `main` 的 `eprintln!`（ADR-0077 把它归为 A 类），不经过
`tracing`，formatter 层看不到。所以 `gui.rs` / `run.rs` 仍然需要拿一个 `Redactor`
手动脱敏那条字符串——但改成 `redact::current()` 取**同一个**集合，不再各自
`Redactor::new(...)` 一份副本。

| 通道 | 谁保护 |
|---|---|
| `tracing` → stderr / 文件 | `RedactedFormat` |
| 返回给 `main` 的错误 → `eprintln!` | 两处手调，用同一个集合 |
| `print_event` 的 JSONL（stdout） | **不在本文范围**，见下 |

**保留没被过滤掉的风险：`is_empty` 快路径**

secret 集合为空时（CLI 没解析到 key、或 keyless provider）直接透传。这是性能选择，
也意味着「集合还没装」的窗口里日志不脱敏。该窗口是 `init` 到 `assemble`
之间——那时还没有任何代码持有 key，所以不构成泄漏路径。

**不在本文范围**

1. **stdout 的 JSONL 事件流不脱敏。** 它是给程序消费的产物，provider 返回的原始
   body 可能就是消费者要的东西；擅自改写会让下游解析出错。要脱敏也该在事件构造处
   而不是流层，这是另一个决定。
2. **MCP 服务器的 token 不作为 secret。** 现在按环境变量**名**传递
   （`bearer_token_env_var`），值从不进日志。但 `env: { K = "${TOKEN}" }` 的透传值
   会随 `McpServerConfig` 的 `Debug` 暴露——目前没人 `tracing::debug!(?config)`。
   等有泄漏路径了再决定，因为「按变量名像不像 token 来判断」是个需要单独评审的启发式。
3. **`qc logs` 的回溯脱敏不做。** 文件在写入时已经干净。

**验证**

- 纯规则单测在 `qcode_kernel::redact`（最长优先、每处都替换、短 secret 不过滤、
  重复 install 不丢旧值）；格式化路径的单测在 `logging`（`Display` 字段、`Debug`
  字段、URL 内 key、同一行两次出现）。
- **5 个变异全部被唯一对应的测试杀死**：不脱敏 → `secrets_never_reach_a_rendered_line`；
  只替换第一处 → 同上；去掉最长优先排序 → `overlapping_secrets_are_masked_longest_first`；
  `install` 改成替换 → `installing_again_keeps_the_earlier_secrets`；短 secret 不过滤 →
  `secrets_too_short_to_be_real_are_ignored`。其中两个变异第一轮**存活**了（多事件各含一次绕过了「只替换
  第一处」，输入恰好长在前掩盖了缺排序），测试因此被改写——留下的版本才真的钉住行为。
- **集成测试 `crates/qcode/tests/log_redaction.rs` 跑真实二进制**：一份 key 写在
  `base_url` 查询串里的配置 + 指向关闭端口，一次必然失败的 turn。断言三件事——日志里
  真的出现了 `base_url=`（否则测试是空转）、出现了 `<redacted>`、且没有明文；stderr 的
  `error:` 行同样没有明文。

  这个测试是**事后补的**，起因是变异测试发现了一个真实缺口：把 `assemble` 里那行
  `redact::install(...)` 删掉，六个单测**全部照过**——没有任何测试覆盖「两个部件是否
  接上了」。而被删的正是整件事的核心接线。补上之后再删同一行，集成测试立刻在
  `base_url` 断言处分红；去掉 `run.rs` 那处手调，则在同一测试的 stderr 断言处分红。
  教训写在这里而不是只留在提交信息里：**单测覆盖零件，装配线要另外覆盖。**

## ADR-0079：`eprintln!` 迁移——D 类 38 处归入 `tracing`（2026-11-12）

**状态**：已实现。

**问题**

ADR-0077 立了规矩（`stdout` = 产物，`stderr` = 叙述），并把 74 处 `eprintln!` 分成
A（保留）/ D（该迁）两类，产出分类表 `docs/design/LOGGING.md`。当时**只分类不迁移**，
理由是「有一半的判断是这条消息到底该不该存在，那是产品判断」。

现在做迁移。目标不是「把 `eprintln!` 换成 `tracing`」，而是让诊断类消息**能落盘**：
桌面用户从 `.desktop` 启动时看不到 stderr，`qc.log` 是他们唯一的机会。

**范围**

`qcode-gui` 优先：它的生产代码 `eprintln!` 全属 D 类，可见性最差、歧义最小。顺带清掉
`qcode` 的 `commands/gui.rs`（6 处）、`qcode-core/src/store.rs`（1 处），以及分类表
「顺手记下的问题」里那几条。

结果：**38 处迁移**，剩余 33 处全部为 A 类，D 类归零。明细见 LOGGING.md「迁移结果」。

**`qcode-gui` 原本没有 `tracing` 依赖**

一处都没有。加进 `Cargo.toml`，并确认 `qc gui` 是**同进程**调用 `qcode_gui::run`
（不是 fork 一个子进程），所以 GUI 的事件和 CLI 走同一个订阅器、进同一个
`logs/qc.log`。这决定了整件事是否成立，写在依赖旁边。

**三个分类表没覆盖到的判断**

**1. 用户输入校验失败不能用 `warn`。**

分类表把 `setup/save.rs` 的 5 处都算 `warn`。但其中两处是表单/标量值"还不合法"，
**每次防抖窗口都会触发**——用户把 URL 打到一半就报一次。而原因已经显示在设置页顶部。
放 `warn` 会让真正的写盘失败淹没在输入噪音里。所以：

| 位置 | 性质 | 级别 |
|---|---|---|
| `save.rs:484,533,580` | 写 `config.toml`/`gui.json` 失败 | `warn` |
| `save.rs:494,516` | 用户输入还不合法 | **`debug`** |

判据：**这是我们的失败还是用户的中间状态？** 前者 `warn`，后者 `debug`（且用户已经
在界面上看到了）。

**2. 撤掉 `QCODE_GUI_KEY_DEBUG`。**

6 处调试探针包在这个环境变量里——它正是「没有级别机制」时的替代品。现在
`[log] level`/`RUST_LOG` 就是那个机制，两个开关并存只会让下一个人不知道按哪个。
全部改为 `debug!`，**变量删除**。

这不是纯重构：`QCODE_GUI_KEY_DEBUG` 是 GUI 的一个私有后门，删掉它意味着少一个未
文档化的开关。净收益——`RUST_LOG=qcode_gui=debug` 本来就能做到同样的事，而且能写进
配置文件、能被 `qc logs` 之外的读者理解。

**3. 一处探针在记录用户输入。**

`send_prompt` 原本记 `value.chars().take(20)`：用户输入的前 20 个字符。ADR-0077 之前
它一闪而过，之后它会**在文件里躺 7 天**。而它没有诊断价值——判断「prompt 没发出去」
靠的是 `len` 和 `running`，不是用户打了什么字。改成只记长度和状态。

这是迁移过程中唯一一次「顺手改掉行为」，理由是和 ADR-0078 同一个：落盘改变了泄漏的
代价，原来无害的东西现在不是。

**一处刻意不删**

`render/mod.rs` 根节点的 `on_mouse_down` 里只有一个 `eprintln!("root mouse down")`，
是标准的调试残留。日志删了，**注册保留**（空闭包 + 注释说明原因）。

根节点是否消费鼠标按下会影响焦点与事件冒泡，而这是 GUI 行为，无法用单测验证，
我也没有交互式证据。删一行打印是安全的，删一个监听器是没法验证的行为变更——不在这篇
ADR 的业务范围内。

**顺带修的（分类表「顺手记下的问题」）**

- `commands/gui.rs` 的 `" WARN config directory …"` 手搓级别名（还带前导空格）→
  `tracing::warn!`。`RUST_LOG` 现在能管它，`grep WARN` 也能找到它。
- 同文件里 `config=` 路径**打印了两次**（一次带 source、一次裸的）→ 合并成一条带
  `source` 字段的 `info!`。第二次本来就没告诉读者任何新东西。

**验证（这次唯一有意义的验收是「GUI 真的写了日志」）**

单测在这里帮不上忙——迁移的理由就是「桌面用户看不到 stderr」，所以必须真的跑起来看：

```
DISPLAY=:1 RUST_LOG='qcode_gui=debug,qc=info' qc --data-dir … gui
→ 2026-11-12T06:03:17Z INFO  qc::commands::gui: gui: config discovered config=… source="flag"
→ 2026-11-12T06:03:17Z INFO  qc::commands::gui: gui: config resolved exists=true provider=mock …
→ 2026-11-12T06:03:18Z DEBUG qcode_gui::shell: window ready (focus fix + concurrent engine) pid=…
```

- 默认 `warn` 下 GUI 启动**没有迁移带来的噪音**，文件里只有真实告警（含 `wgpu_hal`
  的 Vulkan 驱动警告——那正是「窗口画不出来」时要看的东西，保留）。
- `cargo test --workspace` 110 个测试二进制通过；clippy 覆盖了
  `-p qcode --features gui`（`gui` 不是默认 feature，只 check `-p qcode` 会漏掉
  `commands/gui.rs` 里那段 `#[cfg(feature = "gui")]`）与 `-p qcode-gui`。

**两条流程教训**

1. **`gui` 不在默认 feature 里。** 我第一次 `cargo check -p qcode` 通过了，但那**没有
   编译**被迁移的那段代码。改动落在 `#[cfg]` 后面时，`--features` 是必须的，不是可选的。
2. **`cargo test --workspace` 会把 `target/debug/qc` 覆盖成不带 GUI 的构建。** 端到端
   复验时 `qc gui` 直接输出 `this build has no GUI support` 并退出 2，日志文件是空的
   ——看起来像「迁移没生效」，实际是二进制换了。跑 GUI 验收前必须重新
   `cargo build -p qcode --features gui`。这类假阴性比真阴性更费时间：它指向错误的方向。

**不在本文范围**

- **A 类 33 处不动**，理由见 LOGGING.md：审批提示必须无条件出现（`RUST_LOG=off` 不能
  吞掉「按 y 确认」），进度叙述加时间戳纯属噪声。
- **测试代码 46 处不动**：测试自述用 `eprintln!` 是它们该有的形态。

## ADR-0080：`qc logs --follow`（2026-11-12）

**状态**：已实现。

**问题**

ADR-0077 交付了 `qc logs`（打印路径 + tail），明确把 `--follow` 划在范围外，理由是
「`tail -f` 已经够」。这个理由站不住，只是当时没空：

`qc.log` 在数据目录里，路径是 `~/.local/share/qcode/logs/qc.log`（或 `--data-dir` 指定的
地方）。用户为了看实时日志得先 `qc logs --path-only` 拿路径，再把这个路径喂给
`tail -f`——两个命令、一次复制粘贴，而 `qc logs` 明明已经在那个文件上了。`tail -f`
也不处理这里特有的两件事：**轮转**（每天换 `qc.<date>.log`，`tail -f` 会一直盯着旧的
那一天）和**文件尚未出现**（先开 `qc gui`、再开 `qc logs` 是更自然的顺序，
`tail -f` 直接报 `No such file`）。

**决定：轮询 200ms，不引入 `notify`**

`notify` 已经在 `Cargo.lock` 里（别的依赖带进来的），但用法上的账不划算：

| | 轮询 | inotify |
|---|---|---|
| 网络文件系统 / bind mount | 正常 | 常失效（内核事件不跨主机） |
| 依赖 | 0 | 新增直接依赖 + 平台差异 |
| 延迟 | ≤200ms | 亚毫秒 |

日志是给人看的，200ms 无感；而在容器和远程目录里恰好是**最需要看日志**的场景，
那里 inotify 不工作。这也是 ADR-0077 一贯的取舍（同步 writer 而非 `non_blocking`）：
宁可损失吞吐，也不要在异常路径上失灵。

**`Follower` 单独成型，为了可单测**

有意思的行为全在**两次读之间**：跨天了、文件变短了、文件还没出现。这些跑二进制都
覆盖不到（要么等一天，要么不可能）。所以拆出一个 `Follower`，`poll()` 返回
`(notice, text)`，七个单测直接对着它。

三个必须处理的文件系统变化：

| 变化 | 如何发现 | 处理 |
|---|---|---|
| 新文件出现（换天了） | 每次重新 `current_log()` 解析路径 | 从 0 读，打一行 `--- following qc.<date>.log ---` |
| 同一路径文件变短 | `len < offset` | 从 0 读并**说明**「已替换（轮转或截断）」 |
| 文件还不存在 | `metadata` 失败 | 静默等下一轮 |

第二行是**只在有软链的平台上**才会走到：`qc.log` 这个软链在轮转后指向新文件，路径
字符串不变，`len < offset` 是唯一线索。而 Windows 建不了软链（ADR-0077 已处理降级），
那里是**路径变化**分支。两条路都要有。

**只消费整行**

先 `read_to_end`，再找最后一个 `\n`，只把到它为止的部分交给输出、偏移量也只前进到
那里。理由有两个，第二个更硬：

1. 一次写可能落在行中间，打印半行会产出**文件里不存在的行**。
2. 多字节字符会被切断成两半，`from_utf8` 直接失败。CJK 日志是常态（本仓库的注释、
   错误信息大量中文），所以有专门的单测：在 `连` 的 3 个字节中间切开，前半不得打印，
   后半到达后整行完整输出。

**`qc logs` 不再写日志**

端到端测出来的 bug：`main` 对所有命令都调 `logging::attach_file`，于是 `qc logs`
**先把今天的 `qc.<date>.log` 和 `qc.log` 软链创建出来**，然后报告「没有日志文件」——
永远报不出来，`--follow` 的「等待第一行」也永远等不到。

修法是 `Command::Logs` 跳过 `attach_file`。不只是修 bug，也是对的职责划分：一个
**显示**文件的命令不该写它。代价是 `qc logs` 自身的诊断只到 stderr——它唯一的诊断是
「读这个文件出错」，本来就该直接说给正在看的那个人。

**`| head` 不能 panic**

`follow_forever` 用 `writeln!`/`write!` 到 stdout 并检查错误，管道关闭时安静返回而不是
让 `println!` 在 stdlib 里 panic。`qc logs -f | grep x` 是很自然的用法。

**验证**

- 7 个单测：追加只显示一次、半行等待、多字节不切断、轮转换文件、文件变短重来、
  文件后出现、`skip_to_end` 不重复。**5 个变异各被唯一对应测试杀死**——其中「不做
  整行切分」第一轮显示通过，一查是我的替换脚本没匹配上（静默空操作），重新施加后
  两个测试同时红。这类假绿比假红更危险，之后所有变异都先断言替换确实生效。
- 端到端（真实进程写日志）：另一个 `qc run` 写日志时 `-f` 看到新行；空数据目录下先起
  `-f` 会打印等待提示并在文件出现后跟上；跟随中截断文件打印「已替换」；`| head -3`
  安静退出。
- `qc logs` 在空目录**不创建任何东西**（`ls` 为空），修好后 `no log files in …` 与
  `waiting for the first line in …` 两条提示都能出现。

**不在本文范围**

- **`--since`/`--level` 过滤**：`grep` 够用，加过滤就要处理多行记录（本仓库的
  `wgpu_hal` 输出就是一条事件两行）。
- **多文件跟随**（`-F` 之于 `tail`）：一次只看当天。要看历史，`qc logs -n 2000` 或者
  直接打开上一个 `qc.<date>.log`。

## ADR-0081：工具也能交图——`read_file` 读图片时返回图像，不再拒成「二进制」（2026-11-12）

**背景**

ADR-0061 让用户能把图片贴进输入框，引擎把它作为 `image_url` 送到模型。于是出现一个
说不通的状态：**同一个模型，用户贴的图能看，磁盘上的图看不了**。

起因是一次排查。我截图之后想看图，`read_file` 回答：

```
binary file; refusing to read as text
```

当时的错误结论是「这个模型/这个会话不能处理图形」。实测推翻了它——同一套配置，
`qc run --image red.png "什么颜色"` 对真 API 返回「红色」，请求体里是完整的
`image_url`。所以差别不在模型，在**入口**：用户附件走「消息」，磁盘文件走「工具」，
而工具层没有任何一条路能把图片交回去。`grep -rni image crates/qcode-tools/src/` 除了
一个 HTTP 夹具，一处都没有。

这不是「能不能上传」的问题（上传一直是好的），是「读不读得到」的问题。

**决定**

1. **`read_file` 遇到图片返回图像本身**，按魔数识别 PNG/JPEG/WebP/GIF，不看扩展名：
   存成 `.dat` 的截图仍是 PNG，名字叫 `.png` 的文本仍是文本。工具描述同步改了，
   否则模型不会想到去读图。

2. **字节走 `ToolOutput::images`，引用进事件**。工具不理解存储层在哪——它只交出
   `ToolImage { mime_type, data, label }`；引擎把它写进附件存储并记录
   `ToolResult::images: Vec<ImageRef>`。这与 ADR-0061 §4 对用户附件是同一套分工：
   **日志只存引用，不存图片字节**。

3. **图片作为紧跟工具结果之后的 user 消息送出**。工具结果消息在所有我们说的线格式里
   都只允许文本，图片放不进去；user 是唯一各方都接受的角色。为此抽了
   `history::image_followup`，让「重放日志」和「正在跑的这一轮」共用同一段组装逻辑
   ——端到端第一版就是栽在这里：事件里存了图，但同轮路径只把文本推进内存消息，所以
   模型只有在**恢复会话**时才看得到图。两条路径必须同一处生成。

4. **纯文本模型不受影响**：`images = false` 时既有的 `omit_images` 把图像块换成占位，
   工具写的那行文字仍然说明「读了一张图」。实测确认请求里一个 `image_url` 都没有。

5. **schema v11**（`ToolResult::images`，additive）。旧读者重放日志只是少了图，工具
   的文本摘要还在。

**代价**

- 图片进上下文就有 token 成本，`offload_images` 的预算（20MB/20 张）同样约束它，
  老的先降级。
- 一个 4MB 的截图会被 base64 成 5.3MB 塞进请求。**没有**加尺寸上限，理由是预算已经
  在请求前生效，而砍掉一半的图比一张大图更糟；真遇到大图撑爆上下文，先看
  `offload_images` 而不是另立门槛。

**验证**

- 端到端（假服务器录请求体）：同轮路径 request 2 出现
  `user -> ['text', 'image_url']`；`--resume` 后重放路径同样带图；`images = false`
  时全为 text。
- 真 API 闭环：模型自己调 `read_file red.png` → 答「红色」（同一张图直接用
  `--image` 也答「红色」，见上）。
- 单测：工具层 3 个（图片按内容识别、伪装扩展名双向、非图片二进制仍拒），
  history 2 个（图片重放成 user 消息、blob 丢失时只丢图不丢轮次）。
