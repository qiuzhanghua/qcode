# qcode v2 — 实现规格

> 状态：v2.2 完成（2026-10-21）；v3 方向待定。v1 代码功能已完成；真实 macOS notarize
> 仍缺证书，属于 v1 发布门禁，不阻塞 v2 规划。
>
> 依赖决策见 [DECISIONS.md](./DECISIONS.md)。v1 规格见 [SPEC.md](./SPEC.md)。
> 本文件只描述 v2 的范围、分期与验收，不包含实现代码。

## 1. 范围

**v2 包含**

- **Windows 原生**：CLI + TUI、PowerShell/cmd、Windows 路径与进程树、基础沙箱、打包。
- **统一网络出口**：本地代理 + 域名 allowlist + 审计；qcode 自有 HTTP 强制走代理，
  shell/MCP 尽力而为；v2.2 评估 OS 级强制模式。
- **`web_search`**：新增联网搜索工具，走同一出口策略。
- **subagent 正式化**：当前实验实现转为稳定能力（深度 1、预算、取消、usage 汇总）。
- **ACP（Agent Client Protocol）**：`qc acp`，接入 Zed / Neovim 等编辑器。
- **MCP resources / prompts**：在现有 tools 之上扩展。
- **图片输入**：用户消息附带图片。
- **checkpoint / 文件回滚**：事件中的 turn checkpoint marker 变为可用功能。
- **分发**：自动更新/版本检查、brew tap、npm wrapper、scoop/winget；macOS 签名+公证、
  Windows 代码签名。
- **GUI**：v2.2 做技术 spike；完整 GUI 放 v3。

**v2 不包含**

- 服务端 / 远程 / 多租户（推迟到 v3 或独立产品）。
- 移动端、Web 版。
- Windows 上的 OS 级网络硬隔离（WFP/AppContainer，推迟到 v3）。
- TLS 中间人解密（代理只做域名级策略，不解密内容）。

## 2. 分期

| 期 | 主题 | 核心交付 |
|---|---|---|
| **v2.0** | 跨平台 + 安全边界 | Windows、egress proxy、`web_search`、subagent 正式化、自动更新/分发 |
| **v2.1** | 集成 | ACP、MCP resources/prompts、MCP env、ACP 兼容性测试 |
| **v2.2** | 体验 + 硬化 | 图片输入、checkpoint/回滚、GUI spike、macOS 网络 hard mode、性能 |
| **v3** | 产品化 | 完整 GUI；服务端/多租户 |

### v2.0 验收

- Windows x86_64/aarch64 构建通过；`qc run` / TUI / `run_shell`（PowerShell 与 cmd）
  在 windows-latest CI 上有测试；Ctrl-C 能杀掉整棵子进程树（Job Object）。
- Windows 路径边界：`..`、symlink/junction、大小写、`\\?\` 前缀均有测试。
- 所有 qcode 自有 HTTP（provider、`web_fetch`、`web_search`）经本地代理；找不到代理时
  失败关闭，不回落直连。
- 代理 allowlist 覆盖：允许/拒绝、未知域名 ask/deny、重定向到未知域名拒绝、SSRF
  （private/loopback/link-local）拒绝；审计日志可读。
- shell/MCP 得到 `HTTP_PROXY/HTTPS_PROXY/ALL_PROXY`（含 session token）；文档明确
  “尽力而为，不是硬边界”。
- `web_search` 至少一个后端可用；结果带 URL，工具审批与 `web_fetch` 一致。
- subagent 稳定：深度 1、默认/最大轮数、成本上限、取消、usage/cost 汇总、TUI 分组；
  不再标 experimental。
- 自动更新/分发：至少 brew tap + GitHub Releases；macOS 产物签名/公证；Windows 产物
  有签名计划；`qc --version` 与产物名对账。
- 配置 schema 版本化：旧配置可加载，新增字段有默认值；`qc doctor` 报告迁移状态。

### v2.1 验收

- `qc acp` 通过 ACP compatibility kit；Zed 手工验收：新建/恢复会话、流式文本、
  工具调用、权限弹窗、取消、plan/update。
- `session/load` 能恢复 qcode 既有 JSONL 会话；`session/prompt` 与 `run_turn` 共用
  同一事件流。
- ACP client 提供的 `fs/*`、`terminal/*` 可委托；写操作仍走审批，路径必须 absolute
  且通过 workspace 校验。
- MCP resources/prompts 可列出并读取；MCP server 配置支持 `env`；仍不启用 sampling。
- ACP 文档：Zed / Neovim 安装与配置示例；协议版本 pin 在 `qc doctor` 可见。

### v2.2 验收

- 图片输入：至少 PNG/JPEG，经 provider 多模态接口发送；TUI 显示附件占位；超限/不支持
  的 provider 给出明确错误。
- checkpoint：`/checkpoint` 或等价命令创建标记；`/rollback <n>` 恢复工作区文件。
  rollback 不操作 git，按文件级 pre-image 还原；未提交改动会被覆盖（ADR-0034）。
- GUI spike：同一组 3 个界面（流式 transcript、diff 审查、审批浮层）分别用 egui 与
  GPUI Kit 实现；记录性能、CJK/IME、代码编辑、构建体积、API 稳定性；给出 go/no-go。
- macOS hard mode：Seatbelt 只允许连 loopback 代理端口，shell 直连被拒；文档说明
  Linux/Windows 的后续路径。
- 性能：10 万事件会话的 `sessions list`、TUI 打开、resume 时间有基线；索引不退化。
  2026-10-21 基线见 [PERF.md](./PERF.md)：load 570 ms、索引 list 210 µs、replay 4.5 ms。

## 3. 网络出口（ADR-0023）

### 3.1 威胁模型

防的是**误操作与模型乱拉数据**：未知域名、意外外传、SSRF、供应链脚本；不防本机恶意
代码。应用级代理无法阻止 raw socket、`--noproxy`、DoH、ICMP；这些必须在文档与 UI 中
明说。

### 3.2 架构

- qcode 启动时在 `127.0.0.1:0` 启动 HTTP CONNECT 代理，仅 loopback。
- 每个 qcode 进程一个 session token；token 写入代理 URL 的 userinfo，也注入子进程环境。
  v2.0 一个进程只服务一个会话；ACP 在 v2.1 支持多 session 时，改为 per-session token
  （或代理 URL 携带 session id），避免 allowlist 跨会话泄漏。
- provider / `web_fetch` / `web_search` 的 reqwest client 统一由 `net::client()` 构造，
  强制 `Proxy::all`；代码评审禁止其它 `reqwest::Client::new()`。
- shell / MCP 子进程注入 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`、`NO_PROXY`
  （localhost/127.0.0.1 直连）。**v2.0 的 MCP 使用 session 共享 token 与 allowlist**，
  不按 server 隔离；后续版本再加每 server token。
- 代理 fail closed：host:port 不在 allowlist 即拒绝；不尝试直连。
- HTTPS 只解析 CONNECT 的目标 host，不解密；审计不含 path/query。

### 3.3 策略

```toml
[network]
mode = "proxy"         # off | proxy | strict（v2.2 起 strict）
allow = ["api.openai.com", "api.deepseek.com", "registry.npmjs.org"]
allow_private = false
unknown = "ask"        # ask | deny；非交互环境强制 deny
audit = true
```

- provider 的 `base_url` host 自动加入 allowlist；项目配置只能收紧，不能新增 allow。
- `web_fetch` 未知域名：工具执行前触发审批（允许一次 / 本 session / 拒绝）；批准后加入
  **session allowlist**；代理对重定向到未知 host 直接拒绝。
- `web_search` 的 API host 固定加入 allowlist；结果 URL 不自动放行，跳转仍受策略约束。
- MCP 启动时 `npx` 等需要 registry 域名；每个 server 的额外域名写在配置里，v2.0 合并
  到 session allowlist。
- 默认端口 443/80；其它端口需显式 allow。
- `[sandbox] network` 继续控制 OS 沙箱是否放网；`[network]` 是应用级出口策略。hard mode
  下两者联动：沙箱只允许连本地代理端口。
- SSRF：域名解析到 private/loopback/link-local 默认拒绝（`allow_private = true` 才放行），
  防 `169.254.169.254` 一类元数据地址。

### 3.4 审计与事件

- `logs/network.jsonl`：时间、session、client（provider/shell/mcp/web_fetch/web_search）、
  host、port、结果、原因；不含 header/body。
- 交互式批准写入 session 事件（v2 事件 schema 增加 `network_requested` /
  `network_decided`），保证 replay 时策略决策可审计；非交互拒绝也写事件。
- `qc doctor` 检查代理可用、allowlist 解析、私网策略。

### 3.5 hard mode（v2.2，macOS 优先）

- macOS：Seatbelt 只允许 loopback 代理端口，其余 `(deny network*)`。
- Linux：bwrap `--unshare-net` + 命名空间内 relay（UDS 或 veth）；需要额外依赖，放
  v2.2 调研。
- Windows：WFP/AppContainer 成本高，v3 再评估；v2 只保证“应用级 + 环境变量”。
- hard mode 是 opt-in（`network.mode = "strict"`），默认 `proxy`。
- 实现状态（2026-10-21）：`strict` + `[sandbox] mode = "sandbox-exec"` 已接入。kernel 先启动
  egress 代理，再把端口传给 Seatbelt profile；profile 只含
  `(allow network-outbound (remote tcp "localhost:<port>"))`，其余由 `(deny default)` 拒绝。
  只作用于 `run_shell` 子进程；MCP 仍是应用级代理。`strict` 与 bwrap/landlock/windows-acl
  或 `mode = "none"` 组合会拒绝启动，不假装已隔离。

## 4. Windows

- **Shell**：优先 `pwsh`，退回 `powershell.exe`，最后 `cmd.exe`；不依赖 bash。命令用
  `-NoProfile -NonInteractive -Command`；超时/取消用 Job Object 杀整棵树。
- **路径**：canonicalize 后做 workspace 边界；处理 `\\?\`、大小写不敏感、junction/
  symlink；拒绝 ADS（`file:stream`）与设备路径。
- **沙箱**：v2.0 保持应用级路径校验 + Job Object；文件系统硬隔离需要 AppContainer，
  放 v3。网络走代理（尽力而为），文档明确不是硬边界。
- **TUI**：crossterm Windows 后端；IME、粘贴、CJK 宽度纳入测试；不捕获鼠标。
- **打包**：zip/msi；Authenticode 签名（证书到位后）；scoop/winget manifest。
- **CI**：`windows-latest` 跑 fmt/clippy/test + `qc run` mock + shell 进程树测试。

## 5. ACP

参考 [ACP overview](https://agentclientprotocol.com/protocol/v1/overview.md) 与
[Zed 的 ACP 发布文](https://zed.dev/blog/bring-your-own-agent-to-zed)。

- 新增 `qc acp`：stdio JSON-RPC 2.0，agent 作为 editor 子进程。
- 方法：`initialize`、`authenticate`（可选）、`session/new`、`session/load`、
  `session/prompt`、`session/cancel`；通知 `session/update`。
- 映射：
  - `session/new` → 创建 JSONL 会话；`session/load` → 现有 replay/resume。
  - `session/prompt` → `run_turn`；事件流映射为 message chunk、tool call/update、
    plan（`todo_write`）、compaction、notice。
  - `session/request_permission` ← 现有 `Approver`；拒绝/取消语义保持一致。
  - `session/cancel` → `CancellationToken`。
- **fs/terminal 委托**（已定）：
  - 默认委托给 client，尊重未保存 buffer 与编辑器终端。
  - 所有写操作仍走 qcode 审批；路径必须 absolute，canonicalize 后必须在 workspace 内。
  - client 不具备能力时回落本地工具；能力协商记录在 `initialize`。
  - 委托的 terminal 出网不受 qcode 代理控制，文档明确这是 client 边界。
- 版本：pin ACP schema；CI 跑 compatibility kit；`qc doctor` 打印协议版本。
- 不在 v2.1 做：多 agent 会话、ACP registry 发布（可后续）。

## 6. subagent 正式化

- 深度固定 1；工具集排除 `subagent`；默认 20 轮、上限 40 可配。
- 预算：每子 agent 的 max turns / max cost；超限以 failed/cancelled 收尾并回报父。
- 取消：父取消传播到子；子取消不杀父。
- usage/cost 汇总进父 `turn_ended`；TUI 按调用分组折叠。
- 事件与工具名进入稳定契约；`--json` 输出包含子 session id。
- v2.0 先串行子 agent；并行/多角色放 v2.1 评估。

## 7. GUI

- **v2.2 spike，v3 完整实现**。不进入 v2.0/v2.1 的关键路径。
- 架构：独立 crate/二进制 `qcode-desktop`，消费 core 的事件投影；不改造 TUI。
- Spike 同一组界面：
  1. 流式 transcript（长历史、CJK/IME、代码块）
  2. diff 审查（红绿、逐 hunk、跳转）
  3. 审批浮层（rule/argv/cwd、键盘操作）
- 对比维度：启动/滚动性能、代码编辑器能力、Markdown/diff、IME、无障碍、构建体积、
  API 稳定性、许可证、Windows/Linux 支持。
- 备选：
  - **egui**：通用 immediate-mode，生态成熟、API 稳；编辑器/富文本/diff 需较多自研。
  - **GPUI Kit**：基于 GPUI 的完整桌面框架，60+ 组件、代码编辑器、Markdown、虚拟列表、
    Dock，macOS/Windows/Linux，Apache-2.0；0.x API 仍在演进
    （[文档](https://gpui-kit.com/versions/v0.6.4/zh-CN/docs/)）。
- 倾向：产品级 GUI 选 GPUI Kit，内部面板/轻量工具选 egui；最终由 spike 决定。
- 决策门：spike 报告 + 原型评审后再开 v3 GUI；若 v2.2 资源不足，整体顺延。

## 8. 分发与更新

- macOS：Developer ID 签名 + notarytool 公证（v1 门禁）。
- Windows：Authenticode；zip/msi；scoop/winget。
- Linux：tar.gz/deb/rpm 择一；brew tap 覆盖 macOS/Linux。
- npm wrapper 只做下载器/启动器，不塞二进制进 npm 包。
- 自动更新：默认只检查版本并提示，不静默替换；企业环境可关。
- 版本与配置 schema 对账：`qc --version`、`qc doctor`、发布脚本三方一致。

## 9. 测试与评测

- Windows CI：单元/集成/TUI 快照；PowerShell 与 cmd 各一条 shell 任务。
- 网络：本地 TLS 测试服务器；allow/deny/ask、重定向、SSRF、代理不可用、token 错误。
- 绕过测试：`curl --noproxy` 记录为已知 bypass；hard mode 下必须失败。
- ACP：compatibility kit + Zed 手工清单。
- eval 增加：Windows 路径任务、`web_search` 任务、ACP 会话任务（v2.1）。
- 安全：路径/junction/ADS、私网地址、命令注入、MCP server 恶意输出。

## 10. 风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| Windows 文件/网络硬隔离成本高 | v2.0 只能应用级 | 明确边界；AppContainer 放 v3 |
| 代理可被绕过 | 安全预期落空 | 文档威胁模型；hard mode v2.2 |
| ACP 协议演进 | 返工 | pin schema；compatibility kit；适配层隔离 |
| GPUI Kit 0.x API 变动 | GUI 返工 | spike 评估；独立 crate；锁版本 |
| 范围膨胀 | v2 延期 | v2.0 只做上表；GUI/服务端不进关键路径 |
| MCP 启动需要网络 | 代理下 server 起不来 | 文档 allowlist 示例；错误信息给出所需域名 |
| 证书/公证 | 发布阻塞 | v1 门禁单列；脚本占位已就绪 |

## 11. 里程碑（粗估）

| 期 | 周 | 备注 |
|---|---|---|
| v2.0 | 8–10 | Windows 4–5，代理 2–3，web_search 1，分发/硬化 1–2 |
| v2.1 | 4–6 | ACP 2–3，MCP 1–2，测试/文档 1 |
| v2.2 | 4–6 | 图片 1，checkpoint 1–2，spike 2，hard mode/性能 1–2 |
| v3 | 8–12+ | GUI 完整实现；服务端另立 |

估时基于当前单人+AI 的节奏，不含证书等待与 GUI 正式开发。
