# M2 任务清单

> 周期：第 6–8 周。目标：TUI 前端、上下文压缩、AGENTS.md、MCP client（仅 tools）。
> 状态截至 2026-10-18。设计依据：[SPEC §14](./SPEC.md)、[ADR-0001](./DECISIONS.md)。

## M2 完成定义

- `qc`（无子命令）进入 TUI：状态栏、流式 transcript、工具调用折叠、审批浮层、多行输入。
- Esc 取消当前 turn；Ctrl-C 两次退出；审批在浮层内 allow / deny；取消复用 M1-9 路径。
- 上下文占用 ≥ 78% 触发压缩并写 `compaction` 事件，replay 后上下文一致。
- workspace 的 `AGENTS.md` 注入 system prompt；MCP tools 以 `mcp__<server>__<tool>` 暴露，默认 ask。
- `cargo fmt` / `cargo clippy -D warnings` / `cargo test` / `cargo deny check` 全绿。

## 任务

### M2-1 TUI 基础：状态投影与渲染 ✅

- `qcode-tui` 引入 `ratatui = "=0.29.0"`；`App` 只持有状态，渲染是纯函数
  `ui::render(frame, &App)`，便于 `TestBackend` 快照。
- `App::apply(&Event)` 把事件投影成 transcript：用户消息、助手文本、工具调用（默认折叠）、
  通知、审批浮层；`TurnEnded` 更新状态栏与成本。用户消息只由事件投影，提交时不重复插入。
- 键位状态机：Enter 提交、Esc 取消、Ctrl-C 两次退出、Ctrl-O 展开最近一次工具调用；
  `handle_key` 返回 `UiAction`，不直接做 IO。
- 验收：事件投影/键位单测 8 条 + `TestBackend` 渲染 4 条（状态栏、transcript、
  工具折叠、审批浮层）。

### M2-2 引擎接线 ✅

- `qc`（无子命令）启动 TUI；非 TTY 直接报错并提示 `qc run`。
- `run_turn` 作为 future 与键盘输入同一个 `select!` 轮询：事件经 unbounded channel 进 UI，
  审批经 oneshot 反向应答，Esc 取消当前 turn，Ctrl-C 两次退出（退出前最多等 3s 让进程组清理）。
- 每个 turn 使用新的 `CancellationToken`；多轮共享同一 session 与事件日志。
- 流式 delta 合并见 M2-9。
- 验收：TuiApprover 单测 3 条 + 非 TTY 拒绝集成测试 1 条；真终端手工会话确认；
  退出延迟（空闲仍等 3s）已在 87354c0 修复。

### M2-3 transcript 虚拟化与工具折叠 ✅

- 只构建靠近可见窗口的块：行数预算 = 区域高度 + 滚动偏移 + 8，硬上限 2000 块；
  展开的工具输出最多渲染 400 行，多出时提示剩余行数。transcript 再长，
  每帧工作量也只与窗口相关。
- `Paragraph::line_count`（ratatui `unstable-rendered-line-info`）得到换行后的总行数，
  底部锚定；`PageUp` / `PageDown`（每次 10 行）、`End` 回到底部，状态栏显示 `↑N`。
- 工具行显示名称、参数摘要、状态与耗时（ms / s）。
- 验收：60 行窗口滚动测试（PageUp 到顶、End 回底）、滚动偏移状态栏测试、
  滚动键单测；构建量受预算约束，不随 transcript 总量线性增长。

### M2-4 多行输入 ✅

- 引入 `tui-textarea = "=0.7.0"`（`default-features = false`, `features = ["crossterm"]`），
  与 ratatui 0.29 配套；编辑器自己处理光标移动、Home/End、粘贴与 CJK 宽度。
- Enter 提交；**Ctrl-N 换行**（终端普遍无法区分 Shift+Enter 与 Enter，所以给一个可靠键位），
  Shift/Alt+Enter 在支持 Kitty 键盘协议的终端里同样换行；启动时 push
  `DISAMBIGUATE_ESCAPE_CODES`，退出时 pop。
- 输入框随行数增高（最多占屏幕 1/3）；普通 End 留给编辑器，`Ctrl+End` 把 transcript 拉回底部。
- 验收：CJK 多行提交、Ctrl-N / Shift+Enter / Alt+Enter 换行单测、输入框增高渲染测试。
- 已知限制：IME 组合与系统粘贴仍需要真终端手工确认（本机沙箱无法分配 tty）。

### M2-5 上下文压缩

- 触发阈值 78%；摘要调用走 provider；写 `compaction` 事件（`replaced_from/to`、token 数）。
- 压缩后 replay 与 resume 的上下文一致。

### M2-6 AGENTS.md

- workspace 根 `AGENTS.md` 注入 system prompt；加大小上限与路径校验；记录命中来源。

### M2-7 MCP client（tools only）

- stdio JSON-RPC：`initialize` → `tools/list` → `tools/call`；工具名 `mcp__<server>__<tool>`。
- 默认 ask；请求超时、进程退出、协议错误都转成 `tool_result` 错误，不静默挂起。

### M2-8 并行只读工具

- 同一批 tool calls 中只读工具并发执行；写与 shell 仍串行；事件顺序稳定。

## 版本决定

- `ratatui = "=0.29.0"`：与 `tui-textarea 0.7`（依赖 `ratatui ^0.29`）配套，避免同一进程里
  出现两个 ratatui 版本。升级 ratatui 当独立任务，连同 `tui-textarea` 一起评估。
- 启用 `unstable-rendered-line-info` feature：仅用 `Paragraph::line_count` 做滚动锚定；
  升级时确认该 API 是否已稳定。

### M2-9 流式输出与 60Hz 合并重绘 ✅

- 引擎新增 `Delta` / `DeltaSink` 瞬时回调：文本与推理 delta 只送前端，不进事件日志；
  完成后的 `assistant_message` 仍是唯一持久化来源。
- TUI 把 delta 追加到流式缓冲，按 16ms ticker 合并重绘（≤60Hz）；`assistant_message`
  与 `turn_ended` 到达时清空缓冲，取消时保留引擎给出的部分文本。
- 验收：引擎 delta 转发测试、App 缓冲替换测试、流式预览渲染测试。
