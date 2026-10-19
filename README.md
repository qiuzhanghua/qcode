# qcode

终端 coding agent，Rust 实现，clean-room。**当前状态：M1 完成，M2 进行中（TUI 已接线）**
（多轮工具调用、审批、workspace 路径封闭、事件日志与 replay）。

> 与 OpenAI / Anthropic / Google 无关联。产品名 qcode，命令名 qc。

## 快速开始

```bash
# 工具链入口（cot）
c

# TUI（无子命令；真实 provider 由 config/env 选择）
./target/debug/qc

# TUI 上的工具循环演练
QCODE_PROVIDER=mock-tools ./target/debug/qc

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

TUI 键位：Enter 发送、**Ctrl-N 换行**（支持 Kitty 键盘协议的终端里 Shift+Enter 也可）、
Esc 取消当前 turn、Ctrl-C 两次退出、Ctrl-O 展开/折叠最近一次工具输出、
Ctrl-U / Ctrl-D（或 PageUp / PageDown）滚动 transcript、Ctrl+End 回到底部；
审批浮层里 `y` 允许、`n` 拒绝。助手文本按 ≤60Hz 合并流式显示，完成后落盘为一条
`assistant_message`。非 TTY 环境会拒绝启动 TUI 并提示改用 `qc run`。

## 发布构建（多平台）

产物命名沿用 tdp 插件约定：`qcode_<version>_<os>_<arch>[_gnu|_musl|_msvc][.exe]`，
统一落 `dist/`（已 gitignore）。版本只有一个来源——workspace `Cargo.toml` 的
`[workspace.package] version`；本机目标构建完会跑一遍 `qc --version` 对账，
产物名与二进制报的版本对不上直接失败。

```bash
c                    # 激活 cot（rust + 各目标 std 已装好）
make list            # 看矩阵与就绪状态
make                 # 本机平台
make all             # 本机能产出的全部平台
make windows-arm64   # 单个平台（也可 make linux-musl-amd64）
make archive         # 全部平台，额外打 tar.gz / zip
make check           # 发布门禁：fmt / clippy / test / cargo-deny
make release         # 门禁通过后打全部平台归档
```

Windows 上（PowerShell）：

```powershell
.\build-for-release.ps1
.\build-for-release.ps1 -Target aarch64-pc-windows-gnullvm -Archive
```

| 目标 | rust target | 前置条件 |
|---|---|---|
| `linux-amd64` | `x86_64-unknown-linux-gnu` | x86_64 的 cc |
| `linux-arm64` | `aarch64-unknown-linux-gnu` | `aarch64-linux-gnu-gcc`，或 zig（自动用 zig cc） |
| `linux-musl-amd64` | `x86_64-unknown-linux-musl` | sysroot 里的 `rust-lld`（静态链接，不依赖 glibc） |
| `linux-musl-arm64` | `aarch64-unknown-linux-musl` | 同上，另需 `aarch64-linux-musl-gcc`，或 zig（同上） |
| `windows-amd64` | `x86_64-pc-windows-gnullvm` | cot 的 llvm-mingw 插件（`x86_64-w64-mingw32-clang`） |
| `windows-arm64` | `aarch64-pc-windows-gnullvm` | 同上（`aarch64-w64-mingw32-clang`） |
| `darwin-amd64` / `darwin-arm64` | `*-apple-darwin` | 只能在 macOS 上构建（需要 Apple SDK） |

几个容易踩的点：

- **std 不是 cargo 现下的**：cot 的 rust 插件把交叉 std 一起装进了 sysroot；脚本先查
  `lib/rustlib/<target>/lib`，缺了就直说缺什么，不会把 cargo 的报错原样甩给人。
- **跨架构不只是 std**：`aws-lc-sys`（rustls 的 C 依赖）要编 C 源码，rust-lld 只管
  Rust 的链接步骤；所以 `linux-musl-arm64` / `linux-arm64` 还需要对应的**交叉 C 编译器**。
  有真的 `<triple>-gcc` 就用真的；没有就自动找 zig（`$ZIG` → `PATH` → `~/cot/lib/zig_*/zig`
  → `./.toolchains/`），生成 `zig cc -target aarch64-linux-{gnu,musl}` 的 wrapper。
- **为什么不用 llvm-mingw 编 aarch64 Linux**：llvm-mingw 只带 Windows 目标的 sysroot，
  它的 clang 虽然能 `--target=aarch64-linux-gnu`，但包里没有 aarch64 的 glibc/musl 头和库，
  到 `stdio.h` / `-lc` 就断了，改 `pack-llvm-mingw.py` 也补不出来。要 Linux 交叉就去装
  gnu 的 `gcc-aarch64-linux-gnu`、musl 的 `aarch64-linux-musl-cross`，或者用 zig —— 一份
  zig 自带两套 sysroot，不加 root 也能编。
- **zig 兜底要绕三个坑**（脚本生成的 wrapper 里都处理了）：cc-rs 会把
  `--target=aarch64-unknown-linux-gnu` 塞给编译器，而 zig 只认 `-target aarch64-linux-gnu`；
  zig 在 `-O0` 下默认插 UBSan，aws-lc-sys 的 jitterentropy 正好按 `-O0` 编，不关掉会在链接时
  缺 `__ubsan_handle_*`；rustc 给 aarch64 gnu 传的 `--fix-cortex-a53-843419` 是 GNU ld 专属，
  zig 的 lld 不认识。
- **windows-gnullvm 不需要 MSVC**：链接器是 llvm-mingw 的 clang。它自带的
  `libunwind.dll.a` 会让产物运行期依赖 `libunwind.dll`，所以脚本优先用
  `libunwind-static/<target>/libunwind.a`（pack-rust.py 抽出来的静态库），插到 `-L` 最前面；
  找不到静态库只警告，产物在干净机器上可能加载失败。
- **musl 用 rust-lld**：不需要额外 gcc，也不用 `cross`。
- 交叉产物跑不起来，脚本只做静态核对；`--version` 一致性检查只在本机目标上执行。
- **发布门禁**：`make check` / `./build-for-release --check` 跑 fmt、`clippy -D warnings`、
  `cargo test --workspace` 和 `cargo deny check`，与 CI 的 ci.yml 对齐；`make release`
  先过门禁再打全平台归档。cargo-deny 装在 `$CARGO_HOME/bin`，脚本会把它补进 PATH。
  advisory-db 默认从 GitHub 拉，连不上时会卡到超时：本地已有库则自动 `cargo deny --offline check`，
  更新库用 `cargo deny fetch`（给 git 配代理）；完全没有本地库时才需要一次在线拉取。
- 如果 cot 的 `CARGO_HOME`（`~/cot/repo/cargo`）与 `~/.cargo/config.toml` 里的换源定义
  冲突，或该目录不可写（沙箱），先 `export CARGO_HOME="$PWD/.cargo-home"` 再跑 ——
  这与上面「快速开始」里的做法一致。

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
- **AGENTS.md**：workspace 根的 `AGENTS.md` 注入 system prompt；超过 32KiB、非普通文件或
  symlink 会被拒绝（避免把 workspace 外内容喂给模型）。
- **上下文压缩**：估算达到窗口 78% 时，保留最近 2 轮，把更早历史用同一模型摘要成
  `compaction` 事件；replay / resume 应用同一摘要。窗口由 `[context] window_tokens` 配置。
- **成本**：配置 `[pricing."<model>"]` 后，`turn_ended.cost_usd` 与人类模式输出都带估算；
  `--max-cost` 在模型响应后、工具执行前检查，超限写 `error(max_cost)` 并以失败结束。
- **诊断**：`qc doctor` 检查配置、凭据、数据目录与 toolchain。

尚未实现：MCP、上下文压缩、AGENTS.md、子 agent、OS 沙箱；TUI 处于 M2 接线阶段。

## 配置

```toml
# ~/.config/qcode/config.toml
default_provider = "deepseek"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"   # 只存变量名，不存 key
default_model = "deepseek-flash"

# 上下文窗口（token），用于 78% 压缩阈值；按模型实际窗口调整。
[context]
window_tokens = 128000

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
crates/qcode-tui         ratatui 前端（M2：状态投影、渲染；TUI 入口在 qc）
```

## License

MIT，见 [LICENSE](LICENSE)。版权归 qiuzhanghua。第三方依赖的许可证与已知漏洞用
`cargo deny check` 审计（配置见 [deny.toml](deny.toml)，CI 同步运行）。
