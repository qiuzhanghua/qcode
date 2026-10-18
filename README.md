# coder（工作代号）

终端 coding agent，Rust 实现，clean-room。**当前状态：M0 完成**
（headless 单轮、事件落盘与 replay、OpenAI-compatible 流式 provider、配置分层、doctor）。

> 与 OpenAI / Anthropic / Google 无关联。产品名在发布前会更换，`coder` 只是代号。

## 快速开始

```bash
# 工具链入口（cot）
c

# 依赖缓存放 workspace 内（文件沙箱限制，非全局安装）
export CARGO_HOME="$PWD/.cargo-home"

cargo build --workspace
cargo test --workspace

# 环境自检
./target/debug/coder doctor

# 单轮 headless，输出 JSONL 事件
./target/debug/coder run "say hi" --json --data-dir .tmp/data

# 会话列表与 replay
./target/debug/coder sessions list --data-dir .tmp/data
./target/debug/coder sessions show <id> --data-dir .tmp/data
```

## 当前能力

- 事件日志：append-only JSONL，`seq` 连续校验，崩溃尾行丢弃。
- 状态机：`Idle / Streaming / AwaitingApproval / RunningTool / Compacting / Cancelling`。
- Provider：脚本化 mock + OpenAI-compatible（SSE，覆盖 OpenAI / DeepSeek / Ollama / vLLM），
  支持文本、reasoning、工具参数增量、usage、取消与错误映射。
- CLI：`run` / `sessions list` / `sessions show` / `doctor`；
  `--provider` 选择 mock 或真实 provider。
- 诊断：`coder doctor` 检查配置、凭据、数据目录与 toolchain。

尚未实现：工具循环、审批、TUI、MCP、压缩。

## 配置

```toml
# ~/.config/coder/config.toml
default_provider = "deepseek"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"   # 只存变量名，不存 key
default_model = "deepseek-chat"
stream_usage = true
```

```bash
export DEEPSEEK_API_KEY=...
./target/debug/coder run "explain this repo" --json

# 覆盖配置
./target/debug/coder run "hi" --provider ollama --model qwen2.5-coder \
  --base-url http://localhost:11434/v1
```

优先级：CLI flags > 环境变量（`CODER_PROVIDER` / `CODER_MODEL` / `CODER_BASE_URL`）>
配置文件 > 内置默认。keyless 服务把 `api_key_env` 设为 `""`。
`RUST_LOG=info` 打开 stderr 诊断日志；stdout 永远是干净的 JSONL。

## 文档

- [设计决策](docs/design/DECISIONS.md) —— 唯一事实来源
- [v1 实现规格](docs/design/SPEC.md) —— 架构、事件模型、工具契约
- [评测方案](docs/design/EVAL.md) —— 离线 fixture 与 5 条任务
- [M0 任务清单](docs/design/M0-TASKS.md) —— 已完成项与人工验收步骤
- [资料来源](docs/design/SOURCES.md) —— clean-room 白名单

## 布局

```
crates/coder-core        domain、event、store、state、provider port、engine
crates/coder-providers   provider 实现（mock + OpenAI-compatible）
crates/coder-tools       内置工具（M1）
crates/coder-mcp         MCP client（M2）
crates/coder-cli         coder 二进制、配置解析、doctor
crates/coder-tui         ratatui 前端（M2）
```

## License

Apache-2.0，见 [LICENSE](LICENSE)。第三方依赖许可证用 cargo-deny 审计（M1 接入）。
