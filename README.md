# coder（工作代号）

终端 coding agent，Rust 实现，clean-room。**当前状态：M0 垂直切片已跑通**
（headless 单轮 + 事件落盘 + replay，provider 为 mock）。

> 与 OpenAI / Anthropic / Google 无关联。产品名在发布前会更换，`coder` 只是代号。

## 快速开始

```bash
# 工具链入口（cot）
c

# 依赖缓存放 workspace 内（文件沙箱限制，非全局安装）
export CARGO_HOME="$PWD/.cargo-home"

cargo build --workspace
cargo test --workspace

# 单轮 headless，输出 JSONL 事件
./target/debug/coder run "say hi" --json --data-dir .tmp/data

# 会话列表与 replay
./target/debug/coder sessions list --data-dir .tmp/data
./target/debug/coder sessions show <id> --data-dir .tmp/data
```

## 当前能力

- 事件日志：append-only JSONL，`seq` 连续校验，崩溃尾行丢弃。
- 状态机：`Idle / Streaming / AwaitingApproval / RunningTool / Compacting / Cancelling`。
- Provider port：能力声明 + 流式 delta；M0 只有脚本化 mock。
- CLI：`run` / `sessions list` / `sessions show`。

尚未实现：真实 provider、工具循环、审批、TUI、MCP、压缩。

## 文档

- [设计决策](docs/design/DECISIONS.md) —— 唯一事实来源
- [v1 实现规格](docs/design/SPEC.md) —— 架构、事件模型、工具契约
- [评测方案](docs/design/EVAL.md) —— 离线 fixture 与 5 条任务
- [M0 任务清单](docs/design/M0-TASKS.md) —— 已完成项与下一步
- [资料来源](docs/design/SOURCES.md) —— clean-room 白名单

## 布局

```
crates/coder-core        domain、event、store、state、provider port、engine
crates/coder-providers   provider 实现（M0：mock）
crates/coder-tools       内置工具（M1）
crates/coder-mcp         MCP client（M2）
crates/coder-cli         coder 二进制
crates/coder-tui         ratatui 前端（M2）
```

## License

Apache-2.0（LICENSE 文件在 M0-14 补齐）。
