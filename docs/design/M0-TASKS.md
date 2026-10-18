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

### M0-8 OpenAI-compatible provider ✅

`crates/coder-providers/src/openai_compat.rs`、`src/sse.rs`。

- `reqwest 0.13` + 手写 SSE 解码（按字节缓冲，UTF-8 跨 chunk 安全）。
- 文本、`reasoning_content`、工具参数增量、usage、finish_reason 全部映射到 `StreamEvent`。
- 状态码映射：401/403 → Auth，429 → RateLimit（带 Retry-After），
  400 上下文超限 → ContextOverflow。
- 取消用 `select!` + drop 响应，错误以流内 `Err` 返回。
- 验收：`tests/http_smoke.rs` 用本地 TCP 服务回放真实 HTTP 响应，覆盖 200 流式与 401。

### M0-9 配置与凭据 ✅

`crates/coder-cli/src/config.rs`。

- 三层优先级：CLI flags > 环境变量 > `~/.config/coder/config.toml` > 内置默认。
- 凭据只存环境变量名（`api_key_env`），key 永不写入配置或事件；
  `OpenAiCompatConfig` 的 `Debug` 输出 `<redacted>`。
- 内置 `openai` / `deepseek` / `ollama` 的 base URL 与 key 变量名；
  自定义 provider 通过 config 或 `--base-url --model` 提供。
- `--config`、`--provider`、`--model`、`--base-url`、`--no-stream-usage` 全部接通。
- `stream_options` 被服务端拒绝时自动去掉并重试一次。
- 验收：7 个配置优先级/错误提示测试；冒烟验证 mock 路径与缺 key 提示。

### M0-12 录制 SSE 回放测试 ✅

- `tests/fixtures/openai_text.sse`、`openai_tool_call.sse`，纯文本 fixture，无网络。
- `SseDecoder` 单测覆盖：跨 chunk、多事件同 chunk、CRLF、注释、多行 data、
  UTF-8 被切开、未完成尾事件丢弃。
- 工具 fixture 验证参数分片拼接后是合法 JSON，并在 `[DONE]` 时产出 `ToolCallDone`。
- 验收：`cargo test -p coder-providers` 14 个测试全绿（10 单元 + 4 集成）。

### M0-11 CI ✅（cargo-deny 待补）

- `.github/workflows/ci.yml`：macOS + Linux 矩阵，fmt / clippy `-D warnings` / test，
  外加 MSRV 1.95 的 `cargo check`。
- `cargo-deny` 暂未加入：需要先确认全部传递依赖的许可证清单，避免提交一个必红的 job。
  作为 M1 的独立任务。

### M0-10 日志与脱敏 ✅

`crates/coder-cli/src/logging.rs`。

- `tracing` + `tracing-subscriber`：默认 `warn`，`RUST_LOG=info` 打开细节，全部写 stderr。
- stdout 只保留 JSONL；冒烟验证日志与事件流分离。
- `Redactor` 掩码已知密钥（长串优先），用于错误输出。
- 验收：3 个 redaction 测试；`RUST_LOG=info` 下 JSONL 仍是 5 行。

### M0-13 `coder doctor` ✅

`crates/coder-cli/src/doctor.rs`。

- 检查数据目录可写、配置可解析、provider / model / base URL、凭据是否存在、
  rustc 版本、sandbox 状态。
- 缺 key、目录只读、配置解析失败都会给出可操作提示并以非零退出。
- 验收：本地冒烟输出 `all checks passed`。

### M0-14 文档与 LICENSE ✅

- `LICENSE`（Apache-2.0 全文）与 `NOTICE`。
- README 增加配置示例、优先级说明与 doctor 用法。
- 验收：按 README 的快速开始可直接跑通 mock 路径。

## M0 验收（人工）

计划内任务全部完成。真实 provider 这一段需要你的 key：

```bash
export DEEPSEEK_API_KEY=...
./target/debug/coder doctor
./target/debug/coder run "say hi" --provider deepseek --model deepseek-chat --json
```

检查四件事：

1. `doctor` 报告 `all checks passed`，且 key 显示为 present 而不是内容。
2. `run` 输出 5 条事件，`seq` 从 1 连续到 5。
3. `coder sessions show <id>` 能完整 replay。
4. 报错信息里不出现 key 明文。

注意：工具调用尚未实现（M1），真实模型只适合纯文本问题；要求调用工具时会显式失败。

## 环境约定

- 工具链入口是 `c`（cot 激活），`cargo` 解析到 `~/cot/repo/cargo/bin`。
- 文件沙箱只允许写 workspace，因此 `CARGO_HOME` 固定在 `./.cargo-home`
  （已在 `.gitignore`）。这是沙箱约束，不是全局安装。
- 依赖版本由仓库内 `Cargo.lock` 锁定；`rig-core` 在 M2 引入时 pin 死 `=0.44.0`。

## 风险

- SSE 解析的边界情况（跨 chunk、UTF-8 切断）最容易出隐性 bug，M0-12 必须覆盖。
- 真实 endpoint 的 usage 字段各家不一致，`Usage` 全用 `Option` 是对的，不要提前加默认值。
- 不要把 `provider_raw` 做成 `serde_json::Value` 的“随便塞”——M1 回传 thinking 块时它会变成契约。
