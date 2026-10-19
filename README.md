# qcode

终端 coding agent，Rust 实现，clean-room。**当前状态：M3 完成（apply_patch、web_fetch；sandbox runner 保持 stretch）；M4 eval fixture + runner 已落地**
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

# 离线演示 todo_write：transcript 里出现 checklist
./target/debug/qc run "demo" --provider mock-todo

# 交互式审批演练：run_shell 默认 ask（在真实终端里跑；y 执行，n 拒绝）
./target/debug/qc run "demo" --provider mock-approval

# MCP：在 config.toml 里配置 stdio server，工具以 mcp__<server>__<tool> 暴露
# [[mcp.servers]]
# name = "filesystem"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
# 默认每次调用都要审批；server 起不来只丢它自己的工具，不影响 session。

# Skills：项目级放在 .qcode/skills/<name>/SKILL.md（公开 Agent Skills 格式）
# 用户级放在 ~/.config/qcode/skills/<name>/SKILL.md；global 默认扫 ~/.agents/skills。
# 启动时只注入 name/description；/<skill-name> 直接激活，skill 工具按需加载。
./target/debug/qc skills list   # 查看发现结果与被检查的根

# 直接激活一个 skill（等价于 TUI 里输入 /dev-env-inventory）
./target/debug/qc run "盘点本机" --provider deepseek --skill dev-env-inventory

# 会话恢复 / fork（id 来自 events 里的 session_started 或 sessions list）
./target/debug/qc run "继续" --resume <session-id>
./target/debug/qc resume <session-id>              # 在 TUI 里恢复
./target/debug/qc sessions fork <session-id> --at 42   # 打印新 session id
./target/debug/qc sessions gc --older-than-days 30     # dry run；加 --yes 才删除

# 真实 provider：可以读写文件、跑命令
export DEEPSEEK_API_KEY=...
# 不写 --model 时，DeepSeek 默认使用 deepseek-flash
./target/debug/qc run "看一下 src/lib.rs 并总结" --provider deepseek --yes
```

`--yes` 自动批准策略为 ask 的调用。没有它时：终端里逐次确认，非 TTY 环境默认拒绝。
取消：Ctrl-C 一次取消当前 turn，再按一次立即退出。

TUI 键位：Enter 发送、**Ctrl-N 换行**（支持 Kitty 键盘协议的终端里 Shift+Enter 也可）、
Esc 取消当前 turn、Ctrl-C 两次退出、Ctrl-O 展开/折叠最近一次工具输出、
Ctrl-B 折叠/展开最近一次子 agent 组、Ctrl-T 切换 ctx 显示（自动 / 百分比 / token 绝对值）、
Ctrl-U / Ctrl-D（或 PageUp / PageDown）滚动 transcript、Ctrl+End 回到底部；
顶部状态栏显示 provider:model、**上下文占用率（ctx）**、**sandbox 模式**、turn、成本与
运行状态（有子组折叠时显示 `child folded`）；ctx 默认随终端宽度在 `ctx 12%` 与
`ctx 15.4k/128k` 之间切换，Ctrl-T 可强制
切换，达到 50% / 78% 时变黄 / 变红；审批浮层里 `sandbox` 一行再次标明当前生效
的沙箱。审批浮层里 `y` 允许、`n` 拒绝。子 agent 进度按每次调用分组，父工具行显示
`N nested`（折叠时 `N nested (folded)`）；Ctrl-B 切换最近一组，`/subagents` 列出各组、
`/subagents <n>` 单独折叠/展开。输入 `/skills` 列出发现的 skill，`/skills <name>`
直接查看正文；输入 `/<skill-name>`（可带任务，如 `/dev-env-inventory 盘点本机`）
直接激活：正文作为用户消息提交，不必等模型决定是否调用工具。激活不经过 `skill` 工具、
也不新增事件类型，事件日志里保留完整的 user_message。助手文本按 ≤60Hz 合并流式显示，
完成后落盘为一条 `assistant_message`。非 TTY 环境会拒绝启动 TUI 并提示改用 `qc run`。

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

## 评测

`eval/fixtures/workspace` 是一个纯 std 的三 crate fixture；`eval/tasks/` 下 5 条任务
（修 bug / 加功能 / 重构 / 跨文件 API / 修 CI）各自带 `planted/` 坏状态和 `hidden/` 隐藏测试。
`qc-eval` 复制 fixture、跑 headless agent、恢复测试与脚本、注入隐藏测试，再执行 checks：

```bash
cargo build -p qcode-eval
./target/debug/qc-eval --list        # 任务与 prompt
./target/debug/qc-eval --task 01-fix-bug
./target/debug/qc-eval               # 全部任务；日志在 eval/runs/<ts>/
```

详见 [评测方案](docs/design/EVAL.md)。runner 用 `eval/policy.toml`（`--approval-policy`）
放行 shell 与 apply_patch；非 TTY 下其余 ask 一律 deny。`--agent-cmd` 可替换 agent 做离线自测。

## 当前能力

- **工具循环**：`read_file` / `write_file` / `edit_file` / `apply_patch` / `list_dir` /
  `glob` / `grep` / `run_shell` / `web_fetch` / `todo_write` / `subagent`，多轮调用直到任务完成。
  `glob` 与 `grep` 遵守 `.gitignore`，跳过隐藏与构建目录。
- **审批**：三值策略（allow / ask / deny），规则匹配工具名与命令前缀；
  每次决策写事件，含命中的 rule id。
- **安全边界**：所有路径限制在 workspace 内（`..` 与 symlink 逃逸被拒）；
  shell 环境清洗、独立进程组、超时与 `kill_on_drop`。
- **沙箱（实验）**：`[sandbox] mode` 可选 `sandbox-exec`（macOS）、`bwrap` 或
  `landlock`（Linux）。`run_shell` 由对应工具包裹：默认 deny，只读系统路径与 workspace，
  只写 workspace / 临时目录 / Cargo 目录；`bwrap`/`sandbox-exec` 可拒绝网络，`landlock`
  只做文件系统（网络仍由审批控制）。启用时先 preflight，失败则 `qc` 拒绝启动
  （嵌套沙箱里会 EPERM；bwrap 需要系统安装 bubblewrap，Ubuntu 24.04 还要放开
  unprivileged userns）；默认 `none`，审批仍是主边界。
- **取消**：Ctrl-C 取消当前 turn（再按一次立即退出），流式响应立即停止，
  `run_shell` 对整组先 SIGTERM、2s 后 SIGKILL；已产生的输出保留并标记
  `cancelled`，事件日志以 `turn_ended(cancelled)` 收尾（退出码 130）。
- **超时**：`--timeout <秒>` 给整个 turn 设墙钟上限，超时走同一条取消路径
  （含进程组清理），退出码 124 以便与手工取消区分。
- **事件日志**：append-only JSONL，`seq` 连续校验，崩溃尾行丢弃；`turn_ended` 时
  `fsync`，同一 session 由 flock 保证单写者（第二个写者被拒绝，读仍可用）；
  resume 时给悬挂的 `tool_call` 补 `tool_result(interrupted)`。`sessions show` 可完整
  replay，`sessions fork` 分叉，`sessions gc --older-than-days N [--yes]` 清理旧会话。
- **Provider**：mock + OpenAI-compatible（OpenAI / DeepSeek / Ollama / vLLM），
  支持文本、reasoning、工具参数增量、usage、取消与错误映射。首 token 之前最多 3 次
  指数退避（尊重 `Retry-After`），首 token 之后失败直接终止本 turn；只读工具并发由
  provider 的 `parallel_tools` 能力决定。
- **AGENTS.md**：workspace 根的 `AGENTS.md` 注入 system prompt；超过 32KiB、非普通文件或
  symlink 会被拒绝（避免把 workspace 外内容喂给模型）。
- **Skills**：默认发现 `~/.agents/skills`、`~/.config/agents/skills`（`npx skills -g`
  安装的位置）、`~/.config/qcode/skills`、`[skills] roots`、`<workspace>/.agents/skills`
  与 `<workspace>/.qcode/skills` 下的 `SKILL.md`（公开 Agent Skills 格式）。system prompt
  只放 `name` / `description`；模型调用 `skill` 工具按需加载正文，或用 `file` 参数读
  `references/` 等附件。非法/超限/symlink 的 skill 跳过并告警，
  global < user < config < project(.agents) < project(.qcode)。`qc skills list` 可查看全部发现结果。
- **MCP（仅 tools）**：启动时连接启用的 stdio server，`tools/list` 后以
  `mcp__<server>__<tool>` 注册，默认 ask（规则可按名称放行 / 拒绝）。`tools/call`
  有每 server 超时与取消；server 退出、协议错误只影响该 server，stderr 落
  `<data-dir>/logs/mcp-<server>.log`。输出同样 32KB 截断。
- **并行只读工具**：同一批里相邻的只读调用最多 4 路并发；写、shell 与 MCP 调用保持原位
  串行，`tool_result` 始终按调用顺序落日志，不随完成顺序变化。
- **apply_patch**：多文件 unified diff（create / delete / modify，支持 `\ No newline`）。
  先解析并计算全部文件再 staging + 备份 + rename；任一 hunk 失败则什么都不写。绝对路径、
  `..`、rename、symlink 逃逸拒绝；默认 ask。
- **web_fetch**：http(s) 文本抓取，15s 超时、5 次 redirect、100KiB 截断；非文本、二进制、
  URL 内凭据拒绝。默认 ask，不注入任何 API key；不做 SSRF 白名单（审批提示里有完整 URL）。
- **todo_write**：session 待办清单（pending / in_progress / completed），不写文件系统。
  每次成功调用后清单进入 system prompt，TUI 在 transcript 里渲染成 checklist；空数组清空，
  至多一个 in_progress，上限 50 项、每项 200 字符。resume / replay 从事件日志重建状态。
- **上下文压缩**：估算达到窗口 78% 时，保留最近 2 轮，把更早历史用同一模型摘要成
  `compaction` 事件；replay / resume 应用同一摘要。窗口由 `[context] window_tokens` 配置。
- **成本**：配置 `[pricing."<model>"]` 后，`turn_ended.cost_usd` 与人类模式输出都带估算；
  `--max-cost` 在模型响应后、工具执行前检查，超限写 `error(max_cost)` 并以失败结束。
- **子 agent**：`subagent` 工具把独立任务交给新会话运行（同 provider / policy / approver、
  同 workspace、独立 JSONL；工具集去掉 `subagent`，深度固定为 1）。默认 20 轮、上限 40，
  返回最终文本与子 session id；子会话的 usage/cost 汇总进父 `turn_ended` 与 TUI 成本。
  TUI 里子 agent 的文本与工具调用以 `↳` 嵌套显示，不改父状态机。默认 ask（`External`），
  非交互模式需要 `--yes` 或策略放行 `subagent`。
- **诊断**：`qc doctor` 检查配置、凭据、数据目录与 toolchain。

尚未实现：`web_search` 不在 v1 范围；sandbox 三个后端仍是实验性。

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

[[approval.rules]]
id = "deny-secrets-edit"
tool = "write_file"
path_glob = ".env*"
action = "deny"

# MCP：stdio server（v1 只支持 tools）。enabled 默认 true，timeout_secs 默认 120。
# 工具名自动加 mcp__<server>__ 前缀；审批规则按完整工具名精确匹配。
[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
enabled = true

# Skills：默认开启；enabled = false 可完全跳过发现。
# 默认还会扫描 ~/.agents/skills（npx skills -g）、~/.config/agents/skills、
# ~/.config/qcode/skills、<workspace>/.agents/skills、<workspace>/.qcode/skills。
# roots 是额外目录（可选），按顺序覆盖 global/user；project 仍最高优先级。
[skills]
enabled = true
# roots = ["~/space/skills/skills"]

# OS 沙箱（可选，默认 none）。sandbox-exec 仅 macOS，bwrap/landlock 仅 Linux：
# 启动时用同一套规则 preflight，失败则拒绝启动；landlock 不管网络。
[sandbox]
mode = "none"        # none | sandbox-exec | bwrap | landlock
network = false      # bwrap/sandbox-exec 生效；landlock 只做文件系统
```

`qc skills list` 打印实际发现结果与被检查的根，排查“skill 没生效”时先跑它。

`[[approval.rules]]` 的 `tool` 写完整工具名（如 `mcp__filesystem__read_file`）来放行单个
MCP 工具；规则按工具名精确匹配。

规则字段：`tool`、`command_prefix`、`path_glob`（匹配 `path` 参数）、`action`、
`scope`（`user` / `project` / `session`，默认 user）。匹配顺序：任一 deny 胜出；否则
约束最多、scope 最窄、前缀/glob 更长者优先。

项目配置放在 `<workspace>/.qcode/config.toml`，叠加在用户配置之上：可覆盖
providers / pricing、追加 MCP server、追加 skills roots，并追加审批规则。项目规则强制
`scope = "project"`，且 `allow` 会降级为 `ask`——仓库不能给自己提权。

`--approval-policy <path>` 读取独立策略文件：根级 `[[rules]]`，规则**替换** config 里的
`[[approval.rules]]`（不合并）。适合 CI / eval 固定权限；文件不存在或 TOML 非法会在创建
session 之前报错。非 TTY 下策略判为 ask 的调用仍然拒绝。

```toml
# eval-policy.toml
[[rules]]
id = "allow-shell"
tool = "run_shell"
action = "allow"
```

优先级：CLI flags > 环境变量（`QCODE_PROVIDER` / `QCODE_MODEL` / `QCODE_BASE_URL`）>
配置文件 > 内置默认。keyless 服务把 `api_key_env` 设为 `""`。
`default_model` 可省略：DeepSeek 缺省使用 `deepseek-flash`，其他 provider 缺省会报错并要求显式指定。
`RUST_LOG=info` 打开 stderr 诊断日志；stdout 永远是干净的 JSONL。

常用 flag：`--yes`、`--cwd`、`--resume`、`--max-turns`、`--max-cost`、`--timeout`、`--skill`、`--data-dir`、`--config`、`--approval-policy`。
`--max-cost` 必须有对应 `[pricing."<model>"]`，否则启动即报错（避免把预算当成无限制）。

## 文档

- [设计决策](docs/design/DECISIONS.md) —— 唯一事实来源
- [v1 实现规格](docs/design/SPEC.md) —— 架构、事件模型、工具契约
- [评测方案](docs/design/EVAL.md) —— 离线 fixture 与 5 条任务
- [M0 任务清单](docs/design/M0-TASKS.md) —— 基础设施与人工验收
- [M1 任务清单](docs/design/M1-TASKS.md) —— 工具循环、审批与取消
- [M2 任务清单](docs/design/M2-TASKS.md) —— TUI、压缩、AGENTS.md 与 MCP
- [M3 任务清单](docs/design/M3-TASKS.md) —— apply_patch、web_fetch、打包与 sandbox 取舍
- [Skills 设计](docs/design/SKILLS.md) —— 发现、渐进披露与安全边界
- [资料来源](docs/design/SOURCES.md) —— clean-room 白名单

## 布局

```
crates/qcode-core        domain、event、store、state、policy、tool、todo、engine
crates/qcode-providers   provider 实现（mock + OpenAI-compatible）
crates/qcode-tools       read_file / write_file / edit_file / apply_patch / list_dir / glob / grep / run_shell / web_fetch / todo_write / subagent / skill
crates/qcode-mcp         MCP client（stdio，仅 tools）
crates/qcode-eval        eval runner（qc-eval：fixture + planted/hidden + checks）
crates/qcode             CLI 包（包名 qcode，二进制 qc）、配置解析、doctor
crates/qcode-tui         ratatui 前端（M2：状态投影、渲染；TUI 入口在 qc）
```

## License

MIT，见 [LICENSE](LICENSE)。版权归 qiuzhanghua。第三方依赖的许可证与已知漏洞用
`cargo deny check` 审计（配置见 [deny.toml](deny.toml)，CI 同步运行）。
