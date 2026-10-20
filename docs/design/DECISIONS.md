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
