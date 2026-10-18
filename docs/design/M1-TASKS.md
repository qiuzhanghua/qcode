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

`crates/qcode-core/src/tool.rs`。

- `Tool` trait：`spec()`、`approval()`、`execute()`；`approval()` 不产生副作用。
- `ApprovalClass`：Read / WriteInside / WriteOutside / Execute，由工具根据参数计算。
- `ToolRegistry` 按名字索引，`specs()` 直接喂给 provider。

### M1-2 内置工具 ✅

`crates/qcode-tools`。

- `read_file`：行号、二进制检测、2000 行 / 32KB 上限。
- `write_file`：临时文件 + rename 原子写，自动建父目录。
- `edit_file`：唯一字符串替换；不唯一时报错并要求补上下文或 `replace_all`。
- `list_dir`：跳过 `.git` / `target` / `node_modules`。
- `run_shell`：`sh -c`、独立进程组、`kill_on_drop`、默认 120s 超时、环境清洗。
- 路径封闭：词法规范化 + 最深存在祖先 canonicalize，`..` 与 symlink 逃逸都有测试。

### M1-3 策略引擎 ✅

`crates/qcode-core/src/policy.rs`。

- 三值 allow / ask / deny；deny 永远最高；同值下最具体的规则胜出。
- 默认：读与 workspace 内写 allow；workspace 外写与 shell ask。
- 规则可匹配工具名与 `command` 前缀；未知命令一律走默认。

### M1-4 审批接口 ✅

`crates/qcode-core/src/approval.rs`。

- `AutoApprove`（`--yes`）、`AutoDeny`（非 TTY 默认）、`InteractiveApprover`（stdin 确认）。
- 请求包含 call_id、工具名、摘要、原因、命中的 rule id。

### M1-5 Turn loop ✅

`crates/qcode-core/src/engine.rs`、`history.rs`。

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

### M1-8 glob / grep ✅

`crates/qcode-tools/src/search.rs`。

- `glob`：`ignore` 遍历 + `globset`；遵守 `.gitignore`，跳过隐藏与构建目录；
  无斜杠的模式同时匹配 basename；结果排序，上限 1000。
- `grep`：`regex` + `ignore`；跳过二进制与 >4MB 文件；输出 `path:line:text`，
  支持 `glob` 过滤与 `context`（0–5）；上限 200 条匹配。
- 验收：6 个测试覆盖 gitignore、basename、行号、context、glob 过滤、无匹配。

### M1-13 人类模式进度与构建环境 ✅

- 非 `--json` 时把 `tool_call` / `tool_result` / `approval` / `turn` 状态打到 stderr，
  长命令不再看起来像卡死。
- shell 环境放行 `CARGO_HOME` / `RUSTUP_HOME` / `CARGO_TARGET_DIR`，
  避免 agent 里的 cargo 用错缓存或撞沙箱。

### M1-9 取消与进程清理 ✅

- `RunOptions.cancel`（`CancellationToken`）贯穿引擎：流式响应与工具执行都可中断。
  取消不是错误：turn 以 `turn_ended(cancelled)` 结束，已产生的文本保留。
- `ToolContext.cancel` 让工具感知取消；引擎不 drop `execute` future，清理逻辑由工具自己跑完。
- `run_shell` 以 `process_group(0)` 启动，取消/超时时对整组先 SIGTERM、2s 后 SIGKILL，
  孙进程（`cargo` / `make` / `node` worker）不会成为孤儿；输出保留并标 `cancelled`。
- 审批中取消会写 deny 决策 + `cancelled` 结果；同一批里未执行的调用补 `cancelled` 结果，
  保持 `tool_call` → `tool_result` 一一对应。
- 已知限制：交互式审批用同步 stdin 读取，等待输入时第一次 Ctrl-C 要等该行返回；
  再按一次 Ctrl-C 立即退出（M1-10 一并处理）。
- CLI：Ctrl-C 取消当前 turn（再按一次立即退出），取消的 turn 以退出码 130 结束。
- 验收：引擎取消 2 条 + 进程组清理 2 条（取消与超时后后台 `sleep` 均消失）
  + CLI 端到端 1 条（真实 `qc` 进程对不响应的服务按 Ctrl-C，退出码 130 且 `turn_ended(cancelled)`）。

## 待完成（M1-10 起）

### M1-10 交互式审批回归

- 在真实终端里手工跑一次 `run_shell` 的 ask → allow / deny。
- 验收：审批文案能看懂、拒绝后模型能继续。

### M1-11 成本上限

- `--max-cost` 累计 usage 估价，超限终止。
- 依赖 provider 价格表；先做 OpenAI-compatible 的估算。

## 已知限制

- 没有 OS 沙箱：shell 的安全性完全依赖审批与策略。
- 工具调用串行执行；并行只读工具在 M2 与 TUI 一起做。
- 没有流式 UI：headless 只输出最终事件流。
