# qcode GUI v3 — 需求与设计

> 状态：需求讨论稿，2026-10-21。基于 ADR-0036/0037/0038/0039 与现有 `qcode-gui` spike。
> 目标：macOS 桌面版从 spike 变成日常可用的 chat-first coding agent，不是 IDE。

## 1. 已确认的产品决策

| 决策 | 选择 |
|---|---|
| 定位 | Chat-first 桌面 agent |
| 首发平台 | macOS 先行 |
| 窗口/会话 | 单窗口 + 会话侧栏 |
| 远程 | 预留 `Backend` trait；P0 只实现 `LocalBackend`，不做 IPC/server |
| 工作区 | 原生选择器 + 最近列表 |
| 侧栏范围 | 只列当前 workspace 的会话 |
| 设置 | 完整设置页，**所有字段都用可视化表单**，不要求用户编辑 TOML |
| 首次启动 | 配置文件不存在时弹出 **分页 Setup 向导**，覆盖所有可配置内容 |
| 新会话 | 草稿态：第一次发送才写 session 日志 |
| 模型切换 | 按 turn 切换 |
| provider / sandbox / network | 改动需要重启 kernel |
| Provider 支持 | OpenAI-compatible + Anthropic Messages（`anthropic`，`claude` 为别名） |

## 2. 目标与非目标

### 目标

- 把 TUI 的能力完整搬进窗口：流式对话、工具卡片、审批、diff、图片、停止、resume。
- 只做 GUI 明显更好的部分：会话侧栏、diff/回滚审查、多模态输入、可视化设置。
- 首次启动即可完成全部配置，不需要用户先手写 TOML。
- 与 CLI/TUI/ACP 共用 `qcode_kernel::assemble`，不复制 provider/工具/存储逻辑。
- 所有会话/turn 变更经 `Backend`，为未来远程留出替换点。

### P0 非目标

- 文件树、编辑器、终端面板（IDE-like）。
- 多标签、多窗口、多 workspace 同屏。
- 远程 server / 多用户 / 团队协作。
- 插件市场、移动端、Windows/Linux 首发。
- notarize（Apple 凭据未就绪）。

## 3. 启动与首次配置

### 3.1 启动决策树

```text
config_path = --config 或 ~/.config/qcode/config.toml
    │
    ├─ 文件不存在 ──────────────► Setup 向导（first run）
    │
    ├─ 文件存在但 load 失败 ────► Repair 对话框：错误 + Open Setup / Open in Editor / Quit
    │                             绝不静默覆盖原文件
    │
    ├─ 文件存在但 provider/key 不可用 ► Setup 向导（prefilled repair）
    │
    └─ 有效 ────────────────────► 主窗口（恢复 last workspace/session）
```

- `--config` 指向的文件不存在时，向导写这个路径，而不是默认路径。
- 配置有效但没有任何可用 provider/key 时也进入 Setup，而不是让用户在主窗口里反复失败。
- Setup 保存后先 `config::load` + `assemble` 校验；失败留在向导内显示错误。
- 关闭 Setup 窗口 = 不写文件并退出。

### 3.2 Setup 向导（分页）

Setup 是独立窗口，左侧步骤栏 + 右侧当前页内容；**不是一页长表单**。

| # | 页面 | 内容 | 必填 |
|---|---|---|---|
| 1 | Welcome | 配置路径、data dir、将创建/修改的文件；Start | — |
| 2 | Provider | 预设（DeepSeek/OpenAI/Anthropic/Ollama/custom）、provider 名称、base_url、stream_usage | 是 |
| 3 | Model & Key | default_model、model 列表、key 来源（env / Keychain / command）、Test connection | 是 |
| 4 | Agent & Context | max_turns、max_cost_usd、context.window_tokens、snapshot_shell | 默认值 |
| 5 | Network & Sandbox | network mode/enabled/allow/private/unknown/audit；sandbox mode/network/extra_writable | 默认值 |
| 6 | Search | enabled、backend、api_key_env、max_results、base_url | 可选 |
| 7 | MCP | servers 列表：name/command/args/env/enabled/timeout_secs | 可选 |
| 8 | Skills | enabled、roots 列表 | 可选 |
| 9 | Approval & Pricing | approval rules 列表；pricing 表（input/output/cache read/write） | 可选 |
| 10 | Update & Appearance | update 字段；theme/font/density | 默认值 |
| 11 | Review | 所有页面摘要、警告、Save & Start | — |

交互规则：

- **Back / Next**；Next 先校验当前页，错误就地显示，不跳页。
- 可选页可以 Skip；必填页不通过则不能进入 Review。
- 左侧步骤栏显示 completed / current / error；Review 可点回任意页修改。
- 已有环境变量或 Keychain 条目预填，并标记来源。
- “Test connection” 可选，发一次极小请求，可能计费；默认不自动执行。
- 保存前统一 `Config::load` + `MCP::validate` + provider resolve；保存后 `assemble` 失败则回到 Review 并显示错误。
- 向导状态只在内存；关闭 = 不写文件。P1 可加“保存草稿”。

### 3.3 所有可配置内容（全部可视化）

| Section | 字段 | P0 表单 |
|---|---|---|
| 顶层 | `version`（自动）、`default_provider` | Provider 页 |
| `providers.*` | `base_url`、`api_key_env`、`api_key_command`、`default_model`、`stream_usage` | Provider + Model & Key 页；provider 列表可增删 |
| `pricing.*` | input/output/cache-read/cache-write USD per Mtok | Pricing 表；按 model 增删行 |
| `approval.rules[]` | `id`、`tool`、`command_prefix`、`path_glob`、`action`、`scope` | Approval 列表；每行一个规则，可增删 |
| `context` | `window_tokens` | Agent & Context 页 |
| `agent` | `max_turns`、`max_cost_usd`、`snapshot_shell` | Agent & Context 页 |
| `mcp.servers[]` | `name`、`command`、`args`、`env`、`enabled`、`timeout_secs` | MCP 列表；args/env 是可增删的键值行 |
| `skills` | `enabled`、`roots` | Skills 页；roots 是路径列表 |
| `sandbox` | `mode`、`network`、`extra_writable` | Network & Sandbox 页；路径列表 |
| `network` | `mode`、`enabled`、`allow`、`allow_private`、`unknown`、`audit` | Network & Sandbox 页；allow 是列表 |
| `search` | `enabled`、`backend`、`api_key_env`、`max_results`、`base_url` | Search 页 |
| `update` | `enabled`、`check`、`check_days`、`repo`、`base_url` | Update 页 |
| GUI 偏好 | theme、font size、code font、density、recent workspaces、window geometry、last session | Appearance；写 `gui.json` |

- 不提供“Advanced TOML”作为主路径；所有字段必须有明确的表单控件。
- 动态集合统一用 **列表编辑器**：Provider、Pricing、Approval、MCP、Skills roots、Network allow、Sandbox extra_writable。
- 未知字段/未知 section 不丢弃：保存时用 `toml_edit` 原样保留，并在 About 里提示“存在未识别配置”。
- schema 覆盖测试：遍历 `KNOWN_KEYS`，每个键必须绑定到某个表单字段；新增配置字段必须同时加表单，否则测试失败。

### 3.4 Provider 支持

P0 内置两类协议：

1. **OpenAI-compatible**：OpenAI、DeepSeek、Ollama、vLLM、自定义网关。
   - endpoint：`{base_url}/chat/completions`
   - 认证：`Authorization: Bearer …`
   - 支持流式文本、tool calls、usage（可选 `stream_usage`）。
2. **Anthropic Messages**：`anthropic`（`claude` 别名）。
   - endpoint：`{base_url}/v1/messages`（base 已带 `/v1` 时只补 `/messages`）
   - headers：`x-api-key`、`anthropic-version: 2023-06-01`
   - 支持流式文本、tool_use / tool_result、图片（base64）、usage 与 cache token。
   - 默认模型：`claude-sonnet-4-5`，可在 Model & Key 页修改。

Provider 表单字段：

| 字段 | 说明 |
|---|---|
| name | 配置名，也是 `--provider` 的值 |
| preset | openai / deepseek / anthropic / claude / ollama / custom |
| base_url | 自定义/网关时必填；preset 可预填 |
| api_key_env | 环境变量名；与 Keychain 二选一 |
| api_key_command | argv 列表；Keychain 模式自动生成 |
| default_model | 新会话默认 model |
| stream_usage | OpenAI-compatible 的 usage chunk 开关；Anthropic 忽略 |

“Test connection” 使用 provider 的最小请求；CI 只跑纯 fixture/转换测试，不调用真实 API。

### 3.5 写入与密钥

- 配置写入用 `toml_edit`：保留注释、顺序和未知字段；文件不存在时写带注释的模板。
- 原子写：同目录临时文件 → fsync → rename；已有文件先备份 `.bak`；Unix 权限 0600。
- 保存前用 `config::load` + `MCP::validate` + provider resolve 校验；任何错误都留在向导内。
- API key 二选一：
  - 环境变量：只写 `api_key_env`，不写值；
  - Keychain：`security add-generic-password -U -s qcode -a <provider> -w <key>`，
    配置写 `api_key_command = ["security", "find-generic-password", "-w", "-s", "qcode", "-a", "<provider>"]`。
- 密钥输入框 masked，不写日志、不进 transcript、不进错误信息；Keychain 失败不回退明文。
- MCP `env` 可能含明文 secret：表单明确警告；P1 再把 MCP secret 接到 Keychain。

### 3.6 配置写入 API

当前 `qcode-kernel::config` 只有 `mcp_add/remove` 和手写 TOML；需要补一层：

```rust
pub fn save_document(path: &Path, edit: impl FnOnce(&mut toml_edit::DocumentMut))
    -> Result<(), ConfigError>;
```

- 读写都经过同一路径；未知 section/键保留。
- 新增 schema 覆盖测试：遍历 `KNOWN_KEYS`，确保每个键在表单中有绑定。
- 配置损坏时只读打开，不自动修复；Repair 流程由用户确认。

## 4. P0 功能需求

### 4.1 Workspace 与会话

- 原生文件夹选择器（`NSOpenPanel`，可用 `rfd` 薄封装）；最近 workspace 写 `gui.json`。
- 单窗口；切换 workspace 时关闭当前会话视图，运行中先确认停止。
- 侧栏只列当前 workspace；显示标题（首条 user message 截断）、时间、turn 数、cost。
- 新建草稿；第一次发送时 `SessionStore::create` 并写入 session id。
- 恢复：`load(id)` → `App::reset()` → replay；恢复后仍可继续发送。
- 运行中的 session 禁止切换/恢复；先 Stop。
- 会话列表在 turn 结束、新建、恢复后刷新；按最近活动倒序。

### 4.2 对话与工具

- 流式 Markdown、代码块、工具卡片（可折叠）、todo、错误、notice、skill。
- 工具卡片：name、summary、status、耗时、输出（截断/展开）、`apply_patch` diff。
- 自动滚底；用户上滚后暂停自动滚动，提供 “jump to latest”。
- composer：多行、图片粘贴、`Cmd+Enter` 发送、`Shift+Enter` 换行；运行中显示 Stop。
- 发送时把图片作为 attachment；provider 不支持时显示明确错误。
- P0 不支持在运行中排队第二条消息；输入框可编辑但发送禁用。

### 4.3 审批与 diff

- 审批弹窗/面板：tool、argv、cwd、rule、sandbox、reason；Allow once / Always（本 session）/ Reject。
- 多个审批按到达顺序排队；一次只展示一个；键盘 Y/A/N。
- diff 只读：统一 diff、文件头、hunk 折叠；打开时滚动到相关文件。
- checkpoint/rollback UI 放 P1；P0 可在 CLI 用 `qc sessions rollback`。

### 4.4 设置

因为所有字段可视化，设置页从“5 tab”扩展为**分类导航**：

1. Providers & Models
2. Agent & Context
3. Network & Sandbox
4. Search
5. MCP
6. Skills
7. Approval & Pricing
8. Update
9. Appearance
10. About

- 与 Setup 共用表单组件和校验逻辑，不写两套。
- Provider、sandbox、network 改动显示 “restart to apply”；model 下一 turn 生效。
- 动态列表增删时保持稳定 key，避免输入焦点丢失。
- About：版本、config path、data dir、`qc doctor` 摘要、日志目录。

### 4.5 状态与 macOS 壳

- 状态栏：provider/model、workspace、tokens/cost、context 估算、sandbox/network 状态、running。
- `.app` bundle、图标、菜单：
  - App：About、Settings、Quit
  - File：New Session、Open Workspace、Close
  - Edit：Copy/Paste
  - View：Toggle Sidebar、Theme
- 快捷键：`Cmd+N`、`Cmd+O`、`Cmd+,`、`Cmd+Enter`、`Cmd+.`、`Cmd+W`。
- P0 unsigned；notarize 等凭据后补。

## 5. 架构

### 5.1 Backend 边界

```rust
trait Backend {
    fn workspaces(&self) -> Vec<PathBuf>;
    fn sessions(&self, workspace: &Path) -> Result<Vec<SessionSummary>>;
    fn new_draft(&mut self, workspace: PathBuf) -> Result<DraftId>;
    fn resume(&mut self, id: &str) -> Result<Vec<Event>>;
    fn prompt(&mut self, text: String, attachments: Vec<Attachment>);
    fn answer(&mut self, call_id: &str, allow: bool, session: bool);
    fn cancel(&mut self);
    fn events(&mut self) -> Receiver<BackendEvent>;
}
```

- `LocalBackend` 拥有 engine thread、`qcode-ui-bridge`、`SessionStore`。
- 视图只依赖 trait；P0 不做 IPC 协议，但命令/事件保持可序列化形状。
- `BackendEvent` 至少：`Event(Event)`、`SessionList`、`SessionLoaded`、`TurnState`、`Error`。

### 5.2 Session 生命周期

- GUI 启动时不再无条件 `store.create`；由 Backend 管理 draft/active session。
- `resume` 后 `App::reset` + replay；避免旧 transcript 叠加。
- `RunOptions.session_id` 使用当前 active session；model per turn。
- 运行中切换 session 需要先 cancel；turn 结束后刷新侧栏。

### 5.3 配置与 UI 状态

- `config.toml`：provider/agent/sandbox/network/search/MCP/skills/update/pricing/approval。
- `gui.json`（data dir）：theme、font、density、recent workspaces、window geometry、last session。
- 两者都原子写；`gui.json` 损坏时回退默认并提示，不阻塞启动。
- 默认 `qc` 不链接 GPUI；GUI 仍是 `--features gui` 的独立产物。

### 5.4 远程预留

- P0 只实现 LocalBackend；未来 RemoteBackend 复用同一 trait。
- 配置/密钥/workspace 选择在 P0 是客户端行为；远程时 workspace 列表可能来自 server，trait 留扩展点。
- 不引入 daemon、不定义 IPC 协议、不改 CLI/TUI。

## 6. P0 验收

1. 删除配置后启动 GUI：出现 **分页 Setup 向导**；Back/Next、逐页校验；完成必填页后生成可加载的 `config.toml`，进入主窗口。
2. 已有有效配置：直接进入主窗口，不弹向导；配置损坏显示 Repair，不覆盖原文件。
3. Setup 的每一页都能编辑对应字段；**不打开 TOML** 也能完成所有配置。
4. 所有 `KNOWN_KEYS` 有表单绑定；MCP/skills/approval/pricing/search/update 都能增删/编辑；未知字段保存后保留。
5. Keychain 配置：密钥不出现在 `config.toml`、日志、transcript；`qc doctor` 能读到 key。
6. 打开 workspace、最近列表、切换 workspace；侧栏只显示当前 workspace 会话。
7. 新建草稿 → 发送后落日志；恢复会话 transcript 完整；重启后能继续。
8. 流式输出、工具卡片、Stop、错误不崩 UI；审批 Allow/Always/Reject 可解除阻塞。
9. `apply_patch` diff 可读；图片粘贴可用；provider 不支持时错误明确。
10. 设置修改：model 下一 turn 生效；provider/sandbox/network 显示 restart 提示。
11. Anthropic provider：fixture 覆盖文本、tool_use/tool_result、图片、usage；真实 key 手工跑通一次。
12. 10 万事件打开/滚动不卡死；键盘可完成核心流程；IME/VoiceOver 基本可用。
13. 默认 `qc` 不链接 GPUI；桌面产物可单独构建；P0 unsigned 可本地运行。

## 7. 里程碑

| 阶段 | 内容 | 估时 |
|---|---|---:|
| G0 | Backend trait、LocalBackend、workspace/session 生命周期、侧栏骨架 | 4–5 天 |
| G1 | Markdown、工具卡片、composer、图片、Stop、状态栏 | 4–5 天 |
| G2 | 审批队列、diff、键盘操作 | 2–3 天 |
| G3 | 分页 Setup + 全字段设置表单 + 配置写入 + Keychain | 10–12 天 |
| G4 | macOS `.app`、菜单、快捷键、原生选择器 | 3–4 天 |
| G5 | 性能、a11y、测试、打包 | 3–4 天 |

总计约 5–6 周（单人 + AI）。全字段可视化表单是 G3 的主要成本。

## 8. 待定

- Setup 是否允许 “Skip for now” 进入 mock 模式？当前建议：允许 demo，但真实 provider 未配置时主窗口顶部持续显示 banner。
- MCP `env` 里的 secret 是否 P0 就接 Keychain，还是先明文警告。
- 主题是否只做 system/light/dark，还是允许自定义色板。
- Anthropic 默认模型 `claude-sonnet-4-5` 是否随发布更新。
- macOS notarize / 签名凭据。


## 9. 实现进度

- **G1 ✅ 2026-10-21**：`MessageScroller` 变高虚拟列表/跟随尾部、Markdown `TextView`、
  可展开工具卡片、流式预览、Send/Stop、底部状态栏。
- **G0 ✅ 2026-10-21**：`Backend` trait + `LocalBackend`；草稿/新建/恢复会话；侧栏只列当前
  workspace；运行中禁止切换；`App::reset_transcript`；后端生命周期测试（mock provider）。
- 后续里程碑见下面的 G2/G3/G4 状态；G1 的 Markdown/工具卡片已落地。


### G3 完成（2026-10-21）

- ✅ `ConfigEdit`：保留注释/未知字段、原子写、备份、验证；`apply_config` 覆盖全部配置 section。
- ✅ 表单 schema：11 页、所有已知配置键、Provider/Pricing/Rules/MCP 表定义；覆盖率测试。
- ✅ `SetupView`：11 页分页导航、标量输入、Switch、分段 Select、四类动态表（增删行）、Review/Save。
- ✅ GUI 偏好：`gui.json`（theme / font_size / code_font / density），损坏时回退默认。
- ✅ Keychain：provider 页可按 provider 写入平台密钥库，配置只落 `api_key_command`；macOS
  使用 `security`，Linux 使用 `secret-tool`，其他平台明确报未支持。
- ✅ `qc gui` 在配置缺失、无法解析或 provider/key 不可用时打开 Setup；保存后退出并 re-exec
  同一 argv（临时方案，尚非无缝窗口切换）。
- ✅ `run_settings` 复用同一套页面并提供分类导航；菜单入口留给 G4。
- ⚠️ 尚未做真窗口手工验收；Setup 的 Save & Start 仍是一次进程重启。


### G2 完成（除 diff 逐 hunk 包装，2026-10-21）

- ✅ 审批队列：多个 pending approval 按到达顺序展示，回答/取消后自动前进；弹层补充 rule/argv/cwd。
- ✅ `y` / `a` / `n` 在 Approval key context 下分别 Allow / Always / Reject；`Cmd+N` 新会话、`Cmd+,` 设置。
- ✅ diff 面板可关闭、可展开；逐 hunk 交互仍由 GPUI Kit Diff 组件提供，未单独包一层。
- ✅ mock provider 下 `qc gui` 启动冒烟通过（进程保持运行，日志输出 provider/model）。
- ✅ 侧栏显示首条 user message 标题、开始时间、turn 数、cost。
- ✅ 关键输入加 accessibility label（composer/model/设置字段/Keychain key）。
- ⚠️ 审批并发/快捷键仍需真窗口手工验收。

### G4 完成（除 .app 菜单/Dock/签名，2026-10-21）

- ✅ 侧栏 Settings 按钮打开同一套表单（`SetupView` settings 模式）；分类导航，不退出当前对话窗口。
- ✅ 启动时读取 `gui.json`，应用 light/dark、基础字号与等宽字号/字体；设置保存后立即刷新所有窗口。
- ✅ `Cmd+N` / `Cmd+,` / `Cmd+.`（停止）/ `Cmd+O`（打开 workspace）/ `Cmd+W`（关窗）/ `Cmd+B`（侧栏）/ `Cmd+Shift+T`（主题）。
- ✅ workspace 原生选择器 + 最近列表；切换通过 re-exec 实现。
- ✅ 设置 10 分类（About 复用 Review 页）；restart 提示；最近 workspace / last session / 窗口几何落 `gui.json`。
- ✅ composer 多行 Textarea、`Cmd+Enter`、运行中禁用 Send；模型输入按 turn 生效。
- ✅ 本地 unsigned `.app`：`make app` 生成 `dist/QCode.app`，ad-hoc 签名，不需要 Apple 账号；
  只用于本机 P0，Developer ID/notarization 仍未做。
- ⚠️ 原生菜单/Dock 未做；View 切侧栏/主题快捷键已做。
- ⚠️ workspace 切换通过 re-exec；最近 workspace、last session、窗口几何已入 `gui.json`。


### Conversation v4 Phase 1（2026-10-21）

- ✅ `Turn` / `Process` / `Reasoning` 投影：input、process、answer、usage、cost、status；`Block` 继续作为 TUI 兼容层。
- ✅ GUI 按 Turn 渲染：Turn 标题、用户气泡、过程摘要、最终答案、reasoning 行。
- ✅ Compact / Standard / Detailed 三档；过程默认折叠、失败自动展开；Turn / reasoning / tool 可手动展开。
- ✅ Header 轮次选择、上一轮/下一轮、可点击 Turn 列表、右侧 Turn 轨道；`Cmd+Alt+↑/↓` 跳转。
- ✅ 字号层级：正文 14/22、次级 13/20、代码 12/18；Turn 最大宽度 900。
- ✅ `gui.json` 持久化 `display_mode`。
- ✅ 代码块折叠/展开（>20 行折叠到 12 行）、Turn 轨道 tooltip 预览、折叠时近似阅读锚点；Turn picker 目前是 Header 内列表。

### DSH 风格对齐（2026-10-21）

- 过程、思考、工具行加入 Lucide 图标：chevron、brain、terminal、文件/搜索/待办等。
- 注册 `AllAssets` 图标资源。
- 用户消息改为右侧气泡（DSH 布局）。
- 工具输出改为代码块字体并保留列对齐；`SignedDuration { … }` 改为 `took 5.5s`。

### DSH 过程层次（2026-10-21）

- 去掉 `thought for … · N tools` 的过程根节点；过程按事件顺序平铺：思考 → 运行命令 → 思考 → 运行命令。
- 思考行显示首行预览；工具行使用中文标签与对应图标，成功不再显示 `ok`。
- 最终回答使用独立字号（15/24），与 13/20 的过程行区分。
- Standard 折叠细节、Detailed 默认展开、Compact 隐藏过程；流式 reasoning 在首次 delta 时占位，保持时间顺序。

### 默认折叠与批量操作（2026-10-21）

- 运行中的思考/工具默认不展开；Standard/Detailed 都以单行列表为默认。
- Header 下方并排：Jump to Last、Collapse all、Collapse to list。
- Collapse all：隐藏所有 Turn 的过程行，保留输入/最终回答。
- Collapse to list：保留过程列表（思考+运行命令），但强制所有详情收起。
- 点击单行会取消 Collapse to list 的强制收起。

### 浮动操作条（2026-10-21）

- 关闭 MessageScroller 自带浮动 ↓，在右下角使用自定义浮动操作条。
- 三个图标并排：ArrowDown 跳最新、ChevronsDownUp 全收起、ListCollapse 收起到列表；均带 tooltip/aria label。
- 保留 Composer 的 Send，不移除发送入口。

### 过程手风琴（2026-10-21）

- 同一时间只允许展开一个思考/工具行；展开新行会自动收起上一行。
- 默认、Collapse all、Collapse to list 均把所有过程行收为单行。
- Composer 保持不变。

### 审批与预算修复（2026-10-21）

- GUI 的 `Always (this session)` 现在真正记住工具；同 Session 同工具不再重复弹审批。
- 并行同工具审批在 Always 后一并结算。
- `[agent] max_turns` 默认从 8 提到 64；本机已安装配置也同步改为 64。

### Task 模型 Phase 1（2026-10-21）

- `qcode-core::task` 新增 `Task` / `Root` / `RootAccess`：
  - `task.id == session.id`
  - 最多 16 个 Root
  - label 仅允许 slug
  - 至多一个 primary Root
- `SessionStore::task()` 从事件流重建 Task；旧日志没有 task 事件时回退到 cwd 单 Root：
  - label `workspace`
  - access read-write
  - primary
  - path 来自旧 `SessionStarted.cwd`
- `SessionSummary::legacy_task()` 只用于没有加载日志的旧视图。
- 新 Task 追加 `task_configured`，Root 变更追加 `roots_changed`；事件 schema v2。
- GUI 侧栏入口从 “New session” 改为 “New task”。
- 多 Root、Root 持久化/relink、Task 浏览器仍未实现。

### Task 事件（2026-10-21）

- 新 Task 追加 `task_configured`，Root 变更追加 `roots_changed`。
- `SessionStore::create_task` 用于 GUI/ACP/TUI/subagent 新会话。
- `SessionStore::task` 从事件流重建 Task；旧日志回退到 cwd 单 Root。
- 事件 schema 升到 v2；旧 v1 日志仍可读。

### Task 侧栏视图（2026-10-21）

- GUI 侧栏从 `Vec<SessionSummary>` 改为 `Vec<TaskRow>`：
  - 每行持有 `Task`（roots/id）与底层 `SessionSummary`（turns/cost/时间）
  - 行显示 `任务标题 · 时间 · turns · cost · N roots`
  - 列表不再按 workspace 过滤；显示 data-dir 中的全部 Task
- 底层仍使用 `SessionStore`、`session_started` 和事件字段 `session`；Task 是用户层聚合。
- `Backend::sessions` 已改为 `Backend::tasks`，旧 `workspace_sessions` 仅保留为兼容/测试辅助。

### Root 显示/编辑（2026-10-21）

- Task 侧栏显示 active Task 的目录：`@label · RO/RW · 主目录`、截断路径。
- 可操作：
  - `+ Root`：原生目录选择器，自动生成 `root-N` slug
  - `RO/RW`：切换访问模式
  - `★/☆`：设置 primary root
  - `×`：移除 Root
- Root 变更写入 `roots_changed`，刷新 Task 视图。
- 运行中仍可编辑；变更先标为 pending，等当前 Turn 的引擎任务完全返回后写入
  `roots_changed`，下一个 Turn 生效。
- 文件工具已接入 RootResolver：`@label/path`、绝对路径、只读拒绝、无根拒绝。
- Shell 的 cwd 使用 primary Root，但多根 sandbox 尚未接入；越界访问仍依赖后续 ADR-0048 实现。

### RootResolver（2026-10-21）

- `ToolContext` 携带 Task roots，engine 从 `task_configured` / `roots_changed` 重建。
- `path::resolve_ctx` 支持：
  - `@label/path`
  - primary Root 相对路径
  - 绝对路径必须落在 Root 内
  - read-only 写入拒绝
  - 无 Root 时文件工具拒绝
- `read_file` / `write_file` / `edit_file` / `list_dir` / glob / grep / apply_patch 已切到 resolver。
- System prompt 注入 `@label` 到绝对路径的映射。
- Shell 的 cwd 取 primary Root；多根 shell sandbox 仍待 ADR-0048 实现。

### 无根 Task 的 shell（2026-10-21）

- 无 Root 的 Task 在每个 Turn 创建私有 temp：`<data>/tasks/<id>/tmp/turn-<n>`。
- 该 Turn 的 `run_shell` cwd 指向 temp，并设置 `TMPDIR`/`TMP`/`TEMP`。
- Turn 结束（包括失败/取消）自动删除 temp。
- 文件工具在无根 Task 中直接返回 `NoRoots`。
- 该 temp 仅用于无根 shell；多根 sandbox 的 temp 约束仍由 ADR-0048 处理。

### 多根 shell sandbox（2026-10-21）

- macOS `sandbox-exec` 已支持按 Task roots 重建 profile：
  - 所有 Root 可读
  - 仅 RW Root + Turn temp 可写
  - RO Root 不进入 `file-write*`
  - `extra_writable` 真正进入 profile
- `run_shell` 在 roots > 1 或无根 temp 场景使用按 Task 重建的 sandbox。
- Linux `bwrap`/Landlock 和 Windows ACL 的多根重建仍待实现；当前会 fail closed。

### Pending Root 变更（2026-10-21）

- Root 编辑不再要求 Stop。
- Turn 进行中编辑时，UI 显示 `pending` 和拟议 Roots，但当前 Turn 仍使用旧快照。
- `TurnFinished`（引擎任务真正返回、session lock 释放）后写入 `roots_changed`。
- 同一 Task 多次编辑以最后一次 pending 为准。
- 切 New task/Resume 会清空 pending。

### 我的任务侧栏（最终形态，2026-10-21）

- 左上只有 `Settings`；下面是 `我的任务` + 右侧 `+` 新建按钮。
- 任务列表显示 data-dir 中的全部 Task，不按 workspace 过滤。
- 点击任务：切换到该 Task、展开其目录列表，其他任务目录收起。
- 每个目录显示：`@label · RO/RW · 主目录`、路径、访问切换、主目录、移除。
- `+ 目录` 打开原生目录选择器；目录名自动转为 slug alias。
- UI 使用“目录 / Dir”，底层领域类型仍叫 `Root`。
- 新建 `+` 立即创建一个空 Task（无目录），可在其下添加目录或直接纯对话。

### 目录行最终交互（2026-10-22）

- 删除 `目录 · N` 标题行；Task 行不再显示目录数量。
- 每条目录一行：
  `@label  [RW/RO]  [★/☆]  [↗]  [×]`
- 完整路径不再常驻；hover `@label` 显示 tooltip。
- `[RW/RO]` 同时展示与切换访问模式。
- `[★/☆]` 同时展示与设置主目录；点击当前主目录不取消（B 方案）。
- `[↗]` Relink；保留 label/id/access/primary。
- `[×]` 先弹确认框：
  - 显示完整路径；
  - 说明不会删除磁盘文件；
  - 取消 / 移除。
- 目录列表相对 Task 行缩进，便于看出归属。
- 移除主目录后，如果还有其他目录，第一个自动成为主目录。
- 所有目录变更仍遵循 Turn 边界 pending。

### Task 管理（2026-10-22）

- Task 行只显示 marker 和完整标题，不显示时间/turn 数；标题占满整行。`…` 和 `+` 在 hover 时作为右上角浮层出现，不占布局宽度。
- `…` 下拉菜单：
  - 重命名
  - 删除任务
- `+` 给该 Task 添加目录；不切换 active Task，不影响当前对话。
  - inactive Task：立即写入 `roots_changed`
  - active Task 且 Turn 运行中：pending，Turn 结束后应用
- 原目录列表底部的 `+ 目录` 已删除；添加目录统一从 Task 行的 `+` 进入。
- 重命名使用 DSH 风格输入对话框：
  - Enter 保存，Esc/取消关闭
  - 空标题不允许
  - 显式标题持久化到事件日志；优先于首条 user message
- 删除任务使用确认对话框：
  - 删除本地 session 日志、输出目录和索引
  - 不删除任何目录中的文件
  - active Task 运行中禁止删除
  - 删除 active Task 后自动切到下一个 Task；没有则空状态
  - 清理指向该 Task 的 `last_session`
- Task 行不再显示目录数量。
- Event schema v3：新增 `task_renamed`，Task 增加可选显式 `title`。

### Task 菜单浮层（2026-10-22）

- Task 行标题限制为 6 个字符，不再显示时间、turn 数。
- `…` 菜单使用 `deferred(anchored())` 渲染为窗口级浮层，不再作为行内子元素。
- 菜单锚定在 Task 行右上方，向下展开，带窗口边缘回弹。
- 浮层有 `POPUP_PRIORITY`，不会被后续 Task 行覆盖。

### Task 行动作浮层（2026-10-22）

- Task 标题不再截断为 6 个字符，占满整行。
- `… +` 使用行内 absolute 浮层贴在 Task 行右上角（菜单本身仍用 deferred/anchored）。
- 非 hover 时 `visibility: hidden`，不占布局、不响应点击。
- hover 时通过 `group_hover` 显示，不改变标题宽度，不会左右跳跃。
