# M1 任务清单

> 周期：第 3–5 周。目标：从单轮 echo 变成能读写代码、能跑命令的 agent。
> 状态截至 2026-10-18。

## M1 完成定义

- 模型能连续调用工具直到任务完成；每次调用都有审批决策与 `tool_result`。
- workspace 路径封闭（`..` 与 symlink 逃逸被拒）；shell 环境清洗；超时与最大轮数保护。
- 拒绝、超时、未知工具都以 `tool_result` 回到模型，不静默失败。
- `cargo fmt` / `cargo clippy -D warnings` / `cargo test` 全绿。

## 已完成

### M1-1 Tool 契约与注册表 ✅

`crates/coder-core/src/tool.rs`。

- `Tool` trait：`spec()`、`approval()`、`execute()`；`approval()` 不产生副作用。
- `ApprovalClass`：Read / WriteInside / WriteOutside / Execute，由工具根据参数计算。
- `ToolRegistry` 按名字索引，`specs()` 直接喂给 provider。

### M1-2 内置工具 ✅

`crates/coder-tools`。

- `read_file`：行号、二进制检测、2000 行 / 32KB 上限。
- `write_file`：临时文件 + rename 原子写，自动建父目录。
- `edit_file`：唯一字符串替换；不唯一时报错并要求补上下文或 `replace_all`。
- `list_dir`：跳过 `.git` / `target` / `node_modules`。
- `run_shell`：`sh -c`、独立进程组、`kill_on_drop`、默认 120s 超时、环境清洗。
- 路径封闭：词法规范化 + 最深存在祖先 canonicalize，`..` 与 symlink 逃逸都有测试。

### M1-3 策略引擎 ✅

`crates/coder-core/src/policy.rs`。

- 三值 allow / ask / deny；deny 永远最高；同值下最具体的规则胜出。
- 默认：读与 workspace 内写 allow；workspace 外写与 shell ask。
- 规则可匹配工具名与 `command` 前缀；未知命令一律走默认。

### M1-4 审批接口 ✅

`crates/coder-core/src/approval.rs`。

- `AutoApprove`（`--yes`）、`AutoDeny`（非 TTY 默认）、`InteractiveApprover`（stdin 确认）。
- 请求包含 call_id、工具名、摘要、原因、命中的 rule id。

### M1-5 Turn loop ✅

`crates/coder-core/src/engine.rs`、`history.rs`。

- 模型 → 工具 → 审批 → 工具结果 → 再问模型，直到没有工具调用。
- 历史从事件日志重建（`tool_call_id` 正确回传），为 resume 打底。
- 拒绝和工具错误作为 `tool_result` 回给模型，让模型自己纠偏。
- `usage` 跨多次 provider 调用累加到 `turn_ended`。
- 超过 `--max-turns` 写 error 事件并以 failed 结束。

### M1-6 CLI 接线 ✅

- `--yes`、`--cwd`、`--max-turns`；`--cwd` 同时作为会话 cwd 和工具 workspace。
- `approval.rules` 从配置文件读取。
- 非 TTY 环境默认 `AutoDeny`，CI 不会卡在确认提示上。

### M1-7 测试 ✅

- 48 个测试：core 策略/状态/历史，providers 工具循环 4 条（执行、拒绝、非 TTY 拒绝、max turns），tools 6 条（含 symlink 逃逸与环境清洗）。

## 待完成

### M1-8 glob / grep

- 用 `ignore` + `globset` 实现，遵守 `.gitignore`；输出上限与排序。
- 验收：大仓库里 `glob`/`grep` 不失控，结果可复现。

### M1-9 取消与进程清理

- 引擎层接 `CancellationToken`，Esc/超时统一走 kill 进程组。
- 验收：长命令取消后没有遗留子进程。

### M1-10 交互式审批回归

- 在真实终端里手工跑一次 `run_shell` 的 ask → allow / deny。
- 验收：审批文案能看懂、拒绝后模型能继续。

### M1-11 成本上限

- `--max-cost` 累计 usage 估价，超限终止。
- 依赖 provider 价格表；先做 OpenAI-compatible 的估算。

### M1-12 事件 schema 黄金测试

- 给每类事件加 JSON 快照，防止持久化格式悄悄变形。

## 已知限制

- 没有 OS 沙箱：shell 的安全性完全依赖审批与策略。
- 工具调用串行执行；并行只读工具在 M2 与 TUI 一起做。
- 没有流式 UI：headless 只输出最终事件流。
