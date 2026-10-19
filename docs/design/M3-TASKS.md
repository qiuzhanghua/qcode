# M3 任务清单

> 周期：第 9–10 周。目标：`apply_patch`、`web_fetch`、sandbox runner（stretch）、打包。
> 状态截至 2026-10-19。设计依据：[SPEC §7](./SPEC.md)、[DECISIONS ADR-0005](./DECISIONS.md)。

## M3 完成定义

- `apply_patch` 能应用多文件 unified diff；整包先校验，失败时不落任何改动；拒绝绝对路径、
  `..` 与 rename。
- `web_fetch` 只取 http(s) 文本，100KiB 上限，默认 ask；URL scheme、凭据、内容类型有校验。
- 发布脚本能产出本机与交叉平台归档；`make check` 全绿。
- sandbox runner 是 stretch：本版**不实现**，审批仍是唯一执行边界（见 M3-3）。

## 任务

### M3-1 apply_patch ✅

- 输入 `diff`；解析 `---` / `+++` / `@@`，支持 create（`/dev/null`）、delete、modify。
- 路径经 `path::resolve` 限制在 workspace，绝对路径、`..`、symlink 逃逸、rename 全部拒绝。
- 全有或全无：先解析并计算所有文件的新内容，再 staging（临时文件 + 备份 + rename）；
  任何一个 hunk 的 context 不匹配，所有文件保持原样。
- `ApprovalClass::BulkWrite`，默认 ask（单文件 `write_file` 仍默认 allow）。
- 上限：diff 256KiB、20 个文件；输出 32KiB 截断。
- 验收：modify、create、delete、`\ No newline`、git preamble、删除行以 `--` 开头、坏 hunk
  不落盘、绝对路径/`..`/rename/symlink 逃逸/目标不存在或已存在 9 条测试。

### M3-2 web_fetch ✅

- 仅 `http` / `https`；拒绝 URL 内凭据；redirect 上限 5；总超时 15s。
- 只返回文本类内容（`text/*`、JSON、XML、`+json`/`+xml`）；二进制（含 NUL）直接报错。
- 流式读取，100KiB 上限并标记 `truncated`；非 2xx 仍返回状态与正文，但 `tool_result` 为 error。
- `ApprovalClass::Network`，默认 ask；不带任何凭据，也不做 SSRF 白名单（引擎没有可靠的
  出网边界；审批提示里能看到完整 URL）。
- 验收：本地 HTTP server 覆盖 200、404、非文本、非法 scheme/凭据、超大 body 截断 4 条测试。

### M3-3 sandbox runner ✅（抽象 + 实验性 macOS/Linux 后端，默认关闭）

- macOS `sandbox-exec` 已 deprecated 且嵌套调用 EPERM；Linux bubblewrap / Landlock 各有
  取舍（Landlock 管不住 unix socket，Ubuntu 24.04 还需 AppArmor profile）。
- v1 的边界仍是审批 + workspace 路径封闭；`run_shell` 已有独立进程组、环境清洗、超时与取消。
- 本次补齐 `qcode-tools::sandbox`：`Sandbox` trait、`NoSandbox`（默认）、`SandboxExec`
  （macOS）、`Bwrap` / `Landlock`（Linux），由 `[sandbox] mode` 显式开启。启用时用同一套
  规则做 preflight，失败即拒绝启动，不静默降级；默认 deny，网络默认拒绝（Landlock 仅
  文件系统，不接管网络）。
- 非嵌套 macOS 上已验证：workspace 内写入成功、workspace 外写入被拒、preflight 通过。
- Linux 后端通过纯参数单测 + `--target x86_64-unknown-linux-gnu` 交叉编译检查；bwrap
  运行时测试在 Ubuntu CI 装 bubblewrap 后执行，Landlock 测试直接用内核能力，preflight
  失败则跳过。`qc sandbox exec` 仍不提供。

### M3-4 打包 ✅

- `build-for-release` / `build-for-release.ps1` 已覆盖 8 个目标与归档；版本对账、
  std 预检、zig/llvm-mingw 兜底都在脚本里。
- `make check` 作为发布门禁（fmt / clippy / test / cargo-deny）；`make archive` / `--list`
  由脚本驱动。
- 本轮只验证 `make check` 与 `--list`；交叉产物仍按脚本提示缺工具链就直说。

## 遗留

- `todo_write`（session todo，TUI 渲染 checklist，状态按 SPEC §9 进入 system prompt）已在 M3
  收尾补齐；`web_search` 仍不在 v1 范围。
- `apply_patch` 不支持 rename / mode 变更 / 二进制 patch；需要时用 `write_file` 或 `run_shell`。
