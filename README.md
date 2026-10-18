# coder（工作代号）

终端 coding agent，Rust 实现，clean-room。**当前状态：M1 核心循环可用**
（多轮工具调用、审批、workspace 路径封闭、事件日志与 replay）。

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

# 离线冒烟（mock provider，不调用工具）
./target/debug/coder run "say hi" --json --data-dir .tmp/data

# 离线演示工具循环：list_dir，不联网
./target/debug/coder run "demo" --provider mock-tools --yes

# 真实 provider：可以读写文件、跑命令
export DEEPSEEK_API_KEY=...
# 不写 --model 时，DeepSeek 默认使用 deepseek-flash
./target/debug/coder run "看一下 src/lib.rs 并总结" --provider deepseek --yes
```

`--yes` 自动批准策略为 ask 的调用。没有它时：终端里逐次确认，非 TTY 环境默认拒绝。

## 当前能力

- **工具循环**：`read_file` / `write_file` / `edit_file` / `list_dir` / `run_shell`，
  多轮调用直到任务完成。
- **审批**：三值策略（allow / ask / deny），规则匹配工具名与命令前缀；
  每次决策写事件，含命中的 rule id。
- **安全边界**：所有路径限制在 workspace 内（`..` 与 symlink 逃逸被拒）；
  shell 环境清洗、独立进程组、超时与 `kill_on_drop`。
- **事件日志**：append-only JSONL，`seq` 连续校验，崩溃尾行丢弃；
  `sessions show` 可完整 replay。
- **Provider**：mock + OpenAI-compatible（OpenAI / DeepSeek / Ollama / vLLM），
  支持文本、reasoning、工具参数增量、usage、取消与错误映射。
- **诊断**：`coder doctor` 检查配置、凭据、数据目录与 toolchain。

尚未实现：TUI、MCP、上下文压缩、子 agent、OS 沙箱。

## 配置

```toml
# ~/.config/coder/config.toml
default_provider = "deepseek"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"   # 只存变量名，不存 key
default_model = "deepseek-flash"

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

优先级：CLI flags > 环境变量（`CODER_PROVIDER` / `CODER_MODEL` / `CODER_BASE_URL`）>
配置文件 > 内置默认。keyless 服务把 `api_key_env` 设为 `""`。
`default_model` 可省略：DeepSeek 缺省使用 `deepseek-flash`，其他 provider 缺省会报错并要求显式指定。
`RUST_LOG=info` 打开 stderr 诊断日志；stdout 永远是干净的 JSONL。

常用 flag：`--yes`、`--cwd`、`--max-turns`、`--data-dir`、`--config`。

## 文档

- [设计决策](docs/design/DECISIONS.md) —— 唯一事实来源
- [v1 实现规格](docs/design/SPEC.md) —— 架构、事件模型、工具契约
- [评测方案](docs/design/EVAL.md) —— 离线 fixture 与 5 条任务
- [M0 任务清单](docs/design/M0-TASKS.md) —— 基础设施与人工验收
- [M1 任务清单](docs/design/M1-TASKS.md) —— 工具循环、审批与下一步
- [资料来源](docs/design/SOURCES.md) —— clean-room 白名单

## 布局

```
crates/coder-core        domain、event、store、state、policy、tool、engine
crates/coder-providers   provider 实现（mock + OpenAI-compatible）
crates/coder-tools       read_file / write_file / edit_file / list_dir / run_shell
crates/coder-mcp         MCP client（M2）
crates/coder-cli         coder 二进制、配置解析、doctor
crates/coder-tui         ratatui 前端（M2）
```

## License

MIT，见 [LICENSE](LICENSE)。版权归 qiuzhanghua。第三方依赖许可证用 cargo-deny 审计（M1 待办）。
