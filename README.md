# qcode

终端 coding agent，Rust 实现，clean-room。**当前状态：M1 完成，M2 进行中（TUI 基础）**
（多轮工具调用、审批、workspace 路径封闭、事件日志与 replay）。

> 与 OpenAI / Anthropic / Google 无关联。产品名 qcode，命令名 qc。

## 快速开始

```bash
# 工具链入口（cot）
c

# 依赖缓存放 workspace 内（文件沙箱限制，非全局安装）
export CARGO_HOME="$PWD/.cargo-home"

cargo build --workspace
cargo test --workspace

# 环境自检
./target/debug/qc doctor

# 离线冒烟（mock provider，不调用工具）
./target/debug/qc run "say hi" --json --data-dir .tmp/data

# 离线演示工具循环：list_dir，不联网
./target/debug/qc run "demo" --provider mock-tools --yes

# 交互式审批演练：run_shell 默认 ask（在真实终端里跑；y 执行，n 拒绝）
./target/debug/qc run "demo" --provider mock-approval

# 真实 provider：可以读写文件、跑命令
export DEEPSEEK_API_KEY=...
# 不写 --model 时，DeepSeek 默认使用 deepseek-flash
./target/debug/qc run "看一下 src/lib.rs 并总结" --provider deepseek --yes
```

`--yes` 自动批准策略为 ask 的调用。没有它时：终端里逐次确认，非 TTY 环境默认拒绝。
取消：Ctrl-C 一次取消当前 turn，再按一次立即退出。

## 当前能力

- **工具循环**：`read_file` / `write_file` / `edit_file` / `list_dir` / `glob` /
  `grep` / `run_shell`，多轮调用直到任务完成。`glob` 与 `grep` 遵守 `.gitignore`，
  跳过隐藏与构建目录。
- **审批**：三值策略（allow / ask / deny），规则匹配工具名与命令前缀；
  每次决策写事件，含命中的 rule id。
- **安全边界**：所有路径限制在 workspace 内（`..` 与 symlink 逃逸被拒）；
  shell 环境清洗、独立进程组、超时与 `kill_on_drop`。
- **取消**：Ctrl-C 取消当前 turn（再按一次立即退出），流式响应立即停止，
  `run_shell` 对整组先 SIGTERM、2s 后 SIGKILL；已产生的输出保留并标记
  `cancelled`，事件日志以 `turn_ended(cancelled)` 收尾（退出码 130）。
- **超时**：`--timeout <秒>` 给整个 turn 设墙钟上限，超时走同一条取消路径
  （含进程组清理），退出码 124 以便与手工取消区分。
- **事件日志**：append-only JSONL，`seq` 连续校验，崩溃尾行丢弃；
  `sessions show` 可完整 replay。
- **Provider**：mock + OpenAI-compatible（OpenAI / DeepSeek / Ollama / vLLM），
  支持文本、reasoning、工具参数增量、usage、取消与错误映射。
- **成本**：配置 `[pricing."<model>"]` 后，`turn_ended.cost_usd` 与人类模式输出都带估算；
  `--max-cost` 在模型响应后、工具执行前检查，超限写 `error(max_cost)` 并以失败结束。
- **诊断**：`qc doctor` 检查配置、凭据、数据目录与 toolchain。

尚未实现：TUI、MCP、上下文压缩、子 agent、OS 沙箱。

## 配置

```toml
# ~/.config/qcode/config.toml
default_provider = "deepseek"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"   # 只存变量名，不存 key
default_model = "deepseek-flash"

# 成本估算与 --max-cost 的价目表（USD / 百万 token）。
# 数值为示例，按服务商当前价目表填写；未配置的模型成本按 0 计。
[pricing."deepseek-flash"]
input_usd_per_mtok = 0.28
output_usd_per_mtok = 0.42
cache_read_usd_per_mtok = 0.014   # 可选，默认 0
cache_write_usd_per_mtok = 0.28   # 可选，默认 0

[[approval.rules]]
id = "allow-cargo-test"
tool = "run_shell"
command_prefix = "cargo test"
action = "allow"

[[approval.rules]]
id = "deny-force-push"
tool = "run_shell"
command_prefix = "git push --force"
action = "deny"
```

优先级：CLI flags > 环境变量（`QCODE_PROVIDER` / `QCODE_MODEL` / `QCODE_BASE_URL`）>
配置文件 > 内置默认。keyless 服务把 `api_key_env` 设为 `""`。
`default_model` 可省略：DeepSeek 缺省使用 `deepseek-flash`，其他 provider 缺省会报错并要求显式指定。
`RUST_LOG=info` 打开 stderr 诊断日志；stdout 永远是干净的 JSONL。

常用 flag：`--yes`、`--cwd`、`--max-turns`、`--max-cost`、`--timeout`、`--data-dir`、`--config`。
`--max-cost` 必须有对应 `[pricing."<model>"]`，否则启动即报错（避免把预算当成无限制）。

## 文档

- [设计决策](docs/design/DECISIONS.md) —— 唯一事实来源
- [v1 实现规格](docs/design/SPEC.md) —— 架构、事件模型、工具契约
- [评测方案](docs/design/EVAL.md) —— 离线 fixture 与 5 条任务
- [M0 任务清单](docs/design/M0-TASKS.md) —— 基础设施与人工验收
- [M1 任务清单](docs/design/M1-TASKS.md) —— 工具循环、审批与取消
- [M2 任务清单](docs/design/M2-TASKS.md) —— TUI、压缩、AGENTS.md 与 MCP
- [资料来源](docs/design/SOURCES.md) —— clean-room 白名单

## 布局

```
crates/qcode-core        domain、event、store、state、policy、tool、engine
crates/qcode-providers   provider 实现（mock + OpenAI-compatible）
crates/qcode-tools       read_file / write_file / edit_file / list_dir / glob / grep / run_shell
crates/qcode-mcp         MCP client（M2）
crates/qcode             CLI 包（包名 qcode，二进制 qc）、配置解析、doctor
crates/qcode-tui         ratatui 前端（M2：状态投影与渲染）
```

## License

MIT，见 [LICENSE](LICENSE)。版权归 qiuzhanghua。第三方依赖的许可证与已知漏洞用
`cargo deny check` 审计（配置见 [deny.toml](deny.toml)，CI 同步运行）。
