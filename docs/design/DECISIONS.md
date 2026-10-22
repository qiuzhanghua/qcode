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

**验证**：`./build-for-release --release` 在本机产出 `dist/qc-0.1.0-windows-x86_64.exe`
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
6. **不做**（本轮）：命中上限自动续一个预算窗口。它会把「原地打转」伪装成「任务很长」，
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
   快照——跳过 `.git` / `target` / `node_modules` / `dist` / `.tmp` / `.qcode-data` /
   `.venv` / `__pycache__`，单文件超过 4 MB 跳过，总量超过 64 MB 置 `capped` 并停止（打 warn）。
   同一 turn 内已存过的路径不再复制，所以一个 turn 里第 2、3 条 shell 只付出遍历成本。
   这样 rollback 也覆盖 shell 与 MCP 驱动的编辑，而不是只有三个内置写工具。
2. `qc sessions gc --older-than N --yes` 删除会话时顺带删掉该会话的 snapshots，并按现存会话
   列表清理孤儿树（`SnapshotStore::sweep`）；报告每个会话的快照体积与总释放量。
3. 树快照**不记录 absent**：shell 新建的文件在回滚时不会被删除（只有写工具记录的 absent
   会被删）。这是有意的不对称——树快照只知道「当时存在什么」。

**后果 / 边界更新**

- 大仓库上每条 shell 命令都要走一遍工作区（有上限）；`snapshot_shell = false` 可关。
- 快照随 turn 累积；被跳过的目录（`target` 等）里的改动不会被回滚。
- gc 只清理「会话已删除」与「没有对应会话」的快照，不按天数清理仍在用的会话。

**验证**：新增 4 条 core 测试——树快照能还原 shell 式编辑（改一个、删一个、新建一个）、
超过 4 MB 的文件被跳过并计数、gc 删除会话 + 清理孤儿树、shell 开关默认关闭且可打开。

## ADR-0035：图片输入只走一条路——`--image` + 多模态消息（2026-10-22）

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

**状态**：Accepted（2026-10-21）。未实现。依赖 ADR-0047 的 Task/Root 模型。

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