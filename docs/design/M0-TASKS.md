# M0 任务清单

> 周期：第 1–2 周。目标：打通一条垂直切片——headless 单轮、事件落盘、可 replay。
> 状态截至 2026-10-18。

## M0 完成定义（DoD）

- `coder run "..." --json` 对**真实** OpenAI-compatible endpoint 跑通一轮。
- 输出 5 条事件（`session_started` / `turn_started` / `user_message` /
  `assistant_message` / `turn_ended`），`seq` 连续。
- 进程被杀后重启，`coder sessions show <id>` 能完整 replay；不完整的尾行被丢弃。
- `cargo fmt --check`、`cargo clippy -D warnings`、`cargo test` 全绿。
- CI 在 macOS + Linux 上跑通。

## 已完成

### M0-1 workspace 骨架 ✅

六个 crate：`coder-core` / `coder-providers` / `coder-tools` / `coder-mcp` /
`coder-cli` / `coder-tui`。根目录是虚拟 workspace，`resolver = "3"`，edition 2024。
验收：`cargo build --workspace` 通过。

### M0-2 事件模型 ✅

`crates/coder-core/src/event.rs`。

- 信封：`{v, seq, ts, session, turn, type, data}`，`type`/`data` 由 internally
  tagged enum + `flatten` 产生。
- 11 种事件、`ContentBlock`、`Usage`、`Decision`/`Scope`/`ToolStatus`/`TurnStatus`。
- `provider_raw` 字段保留 provider 原始载荷，为 thinking/reasoning 回传留位。
- 验收：`event_json_shape_and_roundtrip` 断言扁平字段与往返一致。

### M0-3 JSONL 存储与 replay ✅

`crates/coder-core/src/store.rs`。

- `create` 返回 `(id, session_started_event)`；`append` 追加并 flush；`load` 校验
  `seq` 连续性；最后一行损坏时丢弃（崩溃恢复契约）；`list` 汇总会话。
- 会话 id 用 ULID，兼作时间排序；id 只允许 ASCII 字母数字，避免路径穿越。
- 验收：`mock_turn_persists_and_replays`、`torn_final_line_is_dropped_on_load`。

### M0-4 状态机 ✅

`crates/coder-core/src/state.rs`。

- 6 个状态、11 个触发器；审批是状态而非 UI 弹窗；任意活跃状态可取消。
- 验收：4 个转移测试覆盖 happy path、审批分支、取消、非法转移。

### M0-5 Provider trait + MockProvider ✅

`crates/coder-core/src/provider.rs`、`crates/coder-providers/src/mock.rs`。

- `Provider` 只管能力声明与流式输出；审批/重试/预算/持久化都在调用方。
- `StreamEvent` 覆盖文本、reasoning、工具参数增量、工具完成、usage、结束。
- Mock 通过 channel + 1ms 间隔模拟流式，支持 `CancellationToken`。
- 验收：集成测试驱动完整 turn。

### M0-6 headless CLI ✅

`crates/coder-cli/src/main.rs`。

- `coder run [prompt] --json --provider mock --model ... --data-dir ...`，
  prompt 可从 stdin 读。
- `coder sessions list` / `coder sessions show <id>`。
- `--data-dir` 是 global flag，可放在子命令前后任意位置。
- 验收：冒烟输出 5 条 JSONL；`sessions show` 可 replay。

### M0-7 单轮引擎 ✅

`crates/coder-core/src/engine.rs`。

- `SessionWriter` 维护 `seq`/`turn`；`run_turn` 驱动一次流式调用并落盘。
- 工具调用暂不支持：写入 error 事件 + `turn_ended(failed)`，显式失败而不是静默丢弃。
- 验收：同 M0-6。

## 待完成

### M0-8 OpenAI-compatible provider

- 目标：真实 HTTP 流式，覆盖 OpenAI / DeepSeek / Ollama / vLLM。
- 范围：`reqwest` + SSE 解析；文本与工具参数增量；usage 映射；错误映射到
  `ProviderError`；取消时 drop 连接。
- 验收：对本地 mock SSE server 跑通；4xx/429/5xx/断流各有单测；不联网也能测。
- 依赖：无。估时：2–3 天。

### M0-9 配置与凭据

- 目标：user config + env + CLI 三层优先级；API key 只从 env/keychain 读。
- 范围：`config.toml` 解析、`--config`、`--model` 默认值、数据目录解析。
- 验收：优先级测试；key 不出现在任何日志/JSONL 中。
- 依赖：M0-8。估时：1 天。

### M0-10 日志与脱敏

- 目标：`tracing` + `RUST_LOG`；统一的 redaction 函数。
- 范围：stderr 日志、事件日志不含密钥、shell 环境清洗的前置工具函数。
- 验收：注入假 key，断言日志中不出现。
- 依赖：M0-9。估时：0.5 天。

### M0-11 CI

- 目标：macOS + Linux 矩阵，fmt / clippy / test / cargo-deny。
- 验收：PR 上全绿；缓存 `~/.cargo` 与 `target`。
- 依赖：无。估时：0.5 天。

### M0-12 录制 SSE 回放测试

- 目标：把一次真实响应的 SSE 分片存成 fixture，离线回放。
- 范围：`tests/fixtures/*.sse`；解析器单测覆盖：跨 chunk 的事件、注释行、
  多字节 UTF-8 被切开、`[DONE]`。
- 验收：确定性、无网络。
- 依赖：M0-8。估时：1 天。

### M0-13 `coder doctor`

- 目标：一条命令回答“为什么跑不起来”。
- 范围：数据目录可写、配置可解析、provider 凭据存在、rustc/cargo 版本、
  当前 sandbox 模式。
- 验收：缺 key / 目录只读时给出可操作提示。
- 依赖：M0-9。估时：0.5 天。

### M0-14 文档与 LICENSE

- 目标：README 快速开始、架构图、Apache-2.0 LICENSE 文件。
- 验收：新人按 README 能在 5 分钟内跑通 mock 路径。
- 依赖：无。估时：0.5 天。

## 环境约定

- 工具链入口是 `c`（cot 激活），`cargo` 解析到 `~/cot/repo/cargo/bin`。
- 文件沙箱只允许写 workspace，因此 `CARGO_HOME` 固定在 `./.cargo-home`
  （已在 `.gitignore`）。这是沙箱约束，不是全局安装。
- 依赖版本由仓库内 `Cargo.lock` 锁定；`rig-core` 在 M2 引入时 pin 死 `=0.44.0`。

## 风险

- SSE 解析的边界情况（跨 chunk、UTF-8 切断）最容易出隐性 bug，M0-12 必须覆盖。
- 真实 endpoint 的 usage 字段各家不一致，`Usage` 全用 `Option` 是对的，不要提前加默认值。
- 不要把 `provider_raw` 做成 `serde_json::Value` 的“随便塞”——M1 回传 thinking 块时它会变成契约。
