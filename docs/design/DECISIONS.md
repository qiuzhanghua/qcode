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
- server 进程默认不套 FS 沙箱；记录 argv 与来源；用户可配置关闭。
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
- rmcp 3.2.0（pin `=3.2.0`）：v1 只用 stdio + tools；streamable HTTP 与 2024-11-05 legacy SSE
  不进 v1。`call_tool` 外层自包 timeout（库没有 per-request deadline）。
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
2. 在构建 registry 时串行连接所有 enabled server，`tools/list` 后立即注册；握手上限 20s。
   失败只 `warn` 并跳过该 server（spawn / 握手 / list 都是）。
3. 每 server `timeout_secs`（默认 120）同时约束 `tools/list` 与 `tools/call`；取消走
   `CancellationToken`，两者都映射成 `tool_result`（error / cancelled）。
4. MCP 工具用 `ApprovalClass::External`，默认 ask；规则按完整工具名精确匹配。
5. stderr 每 server 一个日志文件；输出加 `[mcp:<server>]` 并做 32KB 截断。

**后果**

- 启动延迟随 enabled server 数量线性增长。后续若要并行连接，只改 `connect_all`，
  工具契约不变。
- `tools/list` 不缓存 TTL：进程生命周期内只列一次；长会话里 server 动态增删工具需重启 qc。
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
