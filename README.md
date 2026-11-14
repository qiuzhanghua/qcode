# qcode

Rust 实现的 coding agent，共享 kernel，提供 TUI、GPUI 桌面 GUI 与 ACP 接入。
**当前版本：v0.10.1**。设计决策见 [docs/design/DECISIONS.md](docs/design/DECISIONS.md)，
GUI 形态见 [docs/design/GUI-V3.md](docs/design/GUI-V3.md)。

> 与 OpenAI / Anthropic / Google 无关联。产品名 qcode，命令名 qc。

## 快速开始

```bash
# 工具链入口（cot）
c

# TUI（无子命令、终端里敲；真实 provider 由 config/env 选择）
./target/debug/qc

# 桌面 GUI：左侧“我的任务”，每个 Task 可挂多个目录
# 没有 gui 子命令（ADR-0086）——前端由启动形态决定，这个标记模拟「点图标」
./target/debug/qc --launched-from-gui

# TUI 上的工具循环演练
QCODE_PROVIDER=mock-tools ./target/debug/qc

# 依赖缓存放 workspace 内（文件沙箱限制，非全局安装）
export CARGO_HOME="$PWD/.cargo-home"

cargo build --workspace
cargo test --workspace

# 环境自检
./target/debug/qc doctor

# 有没有新版本（只报告，不下载）
./target/debug/qc update --check

# 离线冒烟（mock provider，不调用工具）
./target/debug/qc run "say hi" --json --data-dir .tmp/data

# 离线演示工具循环：list_dir，不联网
./target/debug/qc run "demo" --provider mock-tools --yes

# 离线演示 todo_write：transcript 里出现 checklist
./target/debug/qc run "demo" --provider mock-todo

# 交互式审批演练：run_shell 默认 ask（在真实终端里跑；y 执行，n 拒绝）
./target/debug/qc run "demo" --provider mock-approval

# MCP：stdio server 或远程 Streamable HTTP server，工具以 mcp__<server>__<tool> 暴露
# [[mcp.servers]]
# name = "filesystem"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
#
# [[mcp.servers]]                     # 远程：command 与 url 恰好二选一
# name = "context7"
# url = "https://mcp.context7.com/mcp"
# bearer_token_env_var = "CONTEXT7_TOKEN"   # 只写变量名，不写 token
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

# 配置与 MCP 管理
./target/debug/qc config show
./target/debug/qc config edit
./target/debug/qc mcp list
./target/debug/qc mcp add filesystem --command npx --arg=-y --arg=@modelcontextprotocol/server-filesystem
./target/debug/qc mcp add wordcraft --command ~/cot/lib/wordcraft/bin/wordcraft \
    --read-path ~/cot/lib/wordcraft --credential github       # 授权集同款 flag
./target/debug/qc mcp remove filesystem

# 真实 provider：可以读写文件、跑命令
export DEEPSEEK_API_KEY=...
# 不写 --model 时，DeepSeek 默认使用 deepseek-flash
./target/debug/qc run "看一下 src/lib.rs 并总结" --provider deepseek --yes
```

GUI 的基本模型：

- **Task : Session = 1:1**；Task 是用户看到的工作单元，session 是磁盘上的事件日志。
- 一个 Task 可以挂多个目录（UI 叫“目录”，底层类型是 `Root`）；其中一个设为主目录。
- Task 行 hover 显示 `…` / `+`：重命名、删除 Task、给该 Task 加目录；不切换当前 Task。
- 文件工具按 Task 目录授权；shell 的 cwd 用主目录；无目录 Task 在私有 Turn temp 中运行。
- Composer：`Enter` 发送，`Shift+Enter` 换行，`Cmd/Ctrl+Enter` 兼容发送。
- Settings 保存后重建 backend，provider/model 立即同步。

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
的沙箱；审批浮层还展示 rule id、argv 与 cwd，diff 工具的展开输出按红/绿着色。
审批浮层里 `y` 允许、`n` 拒绝。子 agent 进度按每次调用分组，父工具行显示
`N nested`（折叠时 `N nested (folded)`）；Ctrl-B 切换最近一组，`/subagents` 列出各组、
`/subagents <n>` 单独折叠/展开。输入 `/skills` 列出发现的 skill，`/skills <name>`
直接查看正文；输入 `/<skill-name>`（可带任务，如 `/dev-env-inventory 盘点本机`）
直接激活：正文作为用户消息提交，不必等模型决定是否调用工具。激活不经过 `skill` 工具、
也不新增事件类型，事件日志里保留完整的 user_message。助手文本按 ≤60Hz 合并流式显示，
完成后落盘为一条 `assistant_message`。非 TTY 环境会拒绝启动 TUI 并提示改用 `qc run`。

## Linux 构建依赖

`cargo build --workspace` 会连 `qcode-gui` 一起编译，GUI 的字体/输入栈要通过
pkg-config 找系统库；只有运行时包（`libfontconfig1` 等）不够，还需要开发包
（Windows / macOS 不需要）：

```bash
sudo apt install libfontconfig-dev libfreetype-dev \
                 libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev
```

常见的缺包报错：

| 报错 | 缺的东西 |
| --- | --- |
| `Package fontconfig was not found in the pkg-config search path` | `libfontconfig-dev` 的 `fontconfig.pc` |
| `pkg-config` 找不到 `freetype2` | `libfreetype-dev` 的 `freetype2.pc`；没有就退回编译自带的 FreeType，按 **C++** 编，还要有 `c++` |
| 链接期 `unable to find library -lxcb` / `-lxkbcommon` / `-lxkbcommon-x11` | 对应 `-dev` 包提供的不带版本号的 `lib*.so` |

装好后可以核对 pkg-config 是否都能查到：

```bash
pkg-config --modversion fontconfig freetype2 xcb xkbcommon xkbcommon-x11
```

### 中文输入（X11 / XIM）

Linux GUI 的输入法走 XIM（`gpui-pre-linux` → `zed-xim`）。中文 preedit 是 GB 2312 编码的
COMPOUND_TEXT，而 crates.io 上的 `xim-ctext` 0.3.0 只认 UTF-8/ISO-2022-JP，会在主线程
`panic!("Encoding Error")`。仓库里 vendor 了修好的解码器（`vendor/xim-ctext`，见 ADR-0051）：

```bash
# 确认补丁生效：输出里应是 vendor/xim-ctext 而不是 crates.io
cargo tree -e normal -p qcode-gui -i xim-ctext
```

IBus 需要 `XMODIFIERS=@im=ibus`（`ibus-daemon --xim`），Wayland 会话下 GPUI 走的是
wayland 后端，不经过 XIM。

## 提交前自检

**提交前跑 `make check`**（等价于 fmt / `clippy -D warnings` / test / cargo-deny）。
它和 `.github/workflows/ci.yml` 是同一组命令，本地过不了的提交在 CI 上也过不了，
区别只是 CI 要等十几分钟才发现。改过平台 cfg 门控的话，再加 `make check-platforms`
（见下）。

最容易漏的两步：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

`--all-targets` 会把**测试代码**也纳入 lint，只跑 `cargo check` 或 `cargo test`
都发现不了。历史教训：新写的测试里一个 `let b = ...` 没用上就足以让 CI 红，
因为 `-D warnings` 把 `unused_variables` 也当错误。

还有一类**本机完全看不见**的：平台 cfg 门控掉的分支。`make check` 只编译 host
target，`#[cfg(windows)]`、`#[cfg(target_os = "macos")]` 的代码在本机根本不参与
编译，悬空参数、拼错的符号、漏改的调用点都只能等 CI 报错。

**改过 cfg 门控就跑 `make check-platforms`**（等价于 `scripts/ci-cross-check.sh`）。
它用 `cargo check --target` 把这几处门控在本机过一遍类型检查——`cargo check` 不需要
链接器，所以只要有该 target 的 std（cot 的 rust 已经装好）就能跑，几秒钟出结果。
检查的正是出货用的三元组：`build-for-release` 打的是 `x86_64/aarch64-pc-windows-gnullvm`，
不是 msvc；本仓库没有 `cfg(target_env)` / `cfg(target_vendor)` 门控，所以这两者在这里
等价。darwin 三元组也在列表里，但需要 `rustup target add` 才有 std，没有就跳过并明说
跳了哪些（跳过不是静默成功：一个 target 都没检查时脚本会失败）。

历史教训：给 `Sandbox::with_roots` 加第三个参数时，`crates/qcode/tests/sandbox_roots.rs`
（`#![cfg(windows)]`）漏改了调用点。Linux 上 `cargo test --workspace` 全绿——那个文件
压根没被编译。`make check-platforms` 第一次跑就报了 `E0061`。

这条命令只覆盖 **Windows**（darwin 需要 `rustup target add`，而且从 Linux 编 Apple 框架的
C shim 从没验证过）。macOS 门控的代码只能用改 cfg 的办法在本地过一遍：

```bash
# 临时把 target_os = "macos" 改成 "linux"（macos.rs 里的 #[cfg] 一并去掉），
# 跑 clippy -D warnings，然后 git checkout 还原
cargo clippy -p qcode-tools --all-targets -- -D warnings
```

历史教训：这条只翻了一半（只翻了 profile 测试模块）就以为验过了，结果 CI 在 macOS 上报
出两处——一个 `#[test]` 属性被划走导致函数「从未使用」，一对重复的 `use` 变成 unused
import。**翻 cfg 要翻全部相关模块，并且用 clippy 而不是只看测试是否通过。**

这条命令替代不了 CI 的 macOS/Windows job（它们跑在真系统上，macOS 还会真跑测试），
所以仍然要等 CI 绿。它挡不住的另一类：门控本身没编译错、但**运行时**才炸的东西——
`debug_assert!`、`cfg` 分支里的逻辑。`windows-build` 只做 `cargo test --workspace
--no-run`（编译测试但从不执行），所以 Windows 侧的 `windows_acl` 单测在 CI 里
至今一条没跑过。改 Windows 运行时代码时，别把「CI 绿」当成「测过了」。

改完 cfg 门控，也可以顺带把这些行过一遍，确认自己改的是不是真落在门控里：

```bash
git diff <base>..HEAD -- '*.rs' | grep -E '^[+-].*cfg\(' | grep -v '^[+-][+-]'
```

历史教训：`1a20d6b` 的 `show_window_later` 只有 macOS/Windows 分支用 `cx`（取
foreground executor 把原生激活推迟到本次 update 之后），Linux 分支只剩
`window.activate_window()`，参数于是悬空，ubuntu job 红而 macOS 本地全绿。分支里
确实用不上的参数写 `let _ = cx;` 说明是有意为之：改名 `_cx` 会丢掉与另一分支照签名
对照的可读性，`#[allow(unused)]` 则会把将来真忘了用的参数一起放过。

另外，**只推一个远端等于没跑 CI**。`gh` 与 `gitee` 是两个独立仓库，GitHub 上
不存在那个 commit 就不会触发 workflow。`31ca808` 那段「运行中排队与插话」
（`inbox.rs`，12 个单测）只推了 gitee，带着 fmt 与 clippy 错误存活了 7 天。
推完两端用 `git ls-remote` 确认指向同一个 commit：

```bash
git ls-remote gh refs/heads/main | cut -c1-7
git ls-remote gitee refs/heads/main | cut -c1-7
```

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

`--release`（PowerShell 版 `-Release`）在归档之外再产出自更新器消费的裸二进制
`dist/qc-<version>-<os>-<arch>[.exe]` 与 `dist/SHA256SUMS`。
**只有一个产物**（ADR-0086）：`qc` 既能在终端里当 TUI 用，也能双击开窗口，前端由启动
形态决定。发布矩阵里的桌面安装包目前只出 Windows；macOS 本地可以生成 unsigned `.app`，
见下文。打 tag（`v*`）由
`.github/workflows/release.yml` 跑完整矩阵、重算校验和、渲染安装清单并发布：
Homebrew formula / Scoop / winget 三件套由 `packaging/render.sh` 从 `dist/` 里
**实际归档的字节**生成（模板在 `packaging/`，成品只存在于 release 与各仓库 PR 里）。

Windows 上（PowerShell）：

```powershell
.\build-for-release.ps1
.\build-for-release.ps1 -Target aarch64-pc-windows-gnullvm -Archive
```

macOS 本地 unsigned app（不需要 Apple 账号）：

```bash
make app          # release -> dist/QCode.app
make app-debug    # debug，构建更快
open dist/QCode.app
```

`make app` 用 ad-hoc identity（`codesign --sign -`）签名，只用于本机 P0 验收；它不是
Developer ID 签名，也没有 notarize，`spctl` 会拒绝。分发给别人时需要 Apple Developer
Program 的 Developer ID 证书与 notarization。

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
- **notarize（占位）**：`notarize-macos.sh` 需要 Developer ID 证书与 app-specific
  password，只做 codesign + zip + notarytool submit；本机/CI 没有凭据时不运行。
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
  `glob` / `grep` / `run_shell` / `web_fetch` / `web_search` / `todo_write` / `subagent`，多轮调用直到任务完成。
  `glob` 与 `grep` 遵守 `.gitignore`，跳过隐藏与构建目录。
- **审批**：三值策略（allow / ask / deny），规则匹配工具名、命令前缀与路径 glob；
  `deny` 永远优先，其余按「最具体者胜」——**精确工具名 > 通配 > 无条件**，与规则在文件里的
  先后无关，所以一条 `mcp__*` 不会盖掉一条精确的 `deny`。每次决策写事件，含命中的 rule id。
  TUI 审批面板按 `a` 本会话放行该工具，不再逐次询问。
  `tool` 支持通配（`mcp__wordcraft__*` 一条覆盖整个 server，否则一个 server 的十几个工具
  要手写十几条——实际踩过：`mcp__wordcraft__*` 在支持之前是静默不匹配，直接落到
  `default: external tool: ask`）。写成非法 glob（如 `mcp__[__x`）会在**加载配置时**报错并
  点名 rule id，而不是安静地永不匹配：

  ```toml
  [[rules]]
  id = "allow-wordcraft"
  tool = "mcp__wordcraft__*"
  action = "allow"
  ```
- **跑飞兜底**：同一个工具带**完全相同参数**连续 3 轮（例如模型反复重跑同一条命令）即终止
  turn，写 `error(no_progress)`；`[agent] max_turns` 与 `[agent] max_cost_usd` 是另外两道预算。
- **安全边界**：文件工具限制在 Task 授权的目录内（`..` 与 symlink 逃逸被拒）；
  shell 环境清洗、独立进程组、超时与 `kill_on_drop`。macOS `sandbox-exec` 已按
  Task 的 RO/RW 目录重建 profile；Linux/Windows 多目录重建仍待补齐。
- **沙箱（实验）**：`[sandbox] mode` 可选 `sandbox-exec`（macOS）、`bwrap` 或
  `landlock`（Linux）、`windows-acl`（Windows）。`run_shell` 由对应工具包裹：默认 deny，
  只读系统路径与 workspace，只写 workspace / 临时目录 / Cargo 目录；`bwrap`/`sandbox-exec`
  可拒绝网络，`landlock` 只做文件系统（网络仍由审批控制），`windows-acl` 只限制写
  （读与网络不受限，硬链接与命名管道不在覆盖范围，因此报 `partial`）。四个后端都认
  `extra_writable`（除 workspace 与私有 temp 外额外可写的子树）。启用时先 preflight，失败则 `qc` 拒绝启动
  （嵌套沙箱里会 EPERM；bwrap 需要系统安装 bubblewrap，Ubuntu 24.04 还要放开
  unprivileged userns）；默认 `none`，审批仍是主边界。`windows-acl` 的授权是常驻 ACE：
  `qc sandbox grant <路径>` 可提前铺好，`qc sandbox revoke <路径>` 逐对象撤掉。
  沙箱的**读**允许集是固定的发行版目录（`/usr`、`/bin`、`/lib`、`/etc`…）加任务 roots，
  所以装在别处的工具链在 `mode = "landlock"` 下会被挡住——不只是写，连读都不行，`cargo`
  起不来。用 `[sandbox] extra_read` 把这些前缀加进来，**它是列表，每项一棵子树**：

  ```toml
  [sandbox]
  mode = "landlock"
  extra_read = ["~/cot", "/opt/tools"]
  ```

  这一项**只给读**（含可执行位，否则目录里的二进制跑不动）。给一个目录是完整决定：
  `extra_read = ["~"]` 同时把 `~/.ssh`、`~/.aws` 交给 agent，要窄就写窄。
  **`~` 在配置文件里由 qcode 展开成真实 home**（只认 `~` 和 `~/x` 两种写法；`~user/x`
  与裸相对路径不展开，仍然按「必须是绝对路径」处理）。所以上面这行在别的机器上也是对的。
  **它是 `[sandbox]` 级设置，因此也作用于 MCP server 的沙箱**（MCP 沙箱由同一个后端派生），
  也就是说这些前缀对第三方 server 同样可读。ADR-0066 §1 的「读集 = 推导出的运行时 +
  凭据子树」因此被放宽；如果只想让 `run_shell` 拿到它，就把它写窄到工具链目录本身。
  `extra_writable` 与 `extra_read` 一样由四个后端都认（Windows 不限制读，所以那里的
  `extra_read` 是空操作）。`extra_writable` 在 landlock 上拿到的是和读写 root 完全相同的
  权限集，所以「列进去 = 那个目录归 agent 用」，请写到子目录而不是 `~` 或 `/home`：

  ```toml
  [sandbox]
  mode = "landlock"
  extra_read = ["~/Documents"]      # 读
  extra_writable = ["~/Documents"]  # 同一个目录，写
  ```

  `/`、`/home`、`/Users`、`/root` 和 home 目录本身会在**加载期**被拒绝并点名原因
  （`extra_writable` 是配置文件级的，没有按 task 收窄的余地）；`~/` 展开后就是 home 本身，
  同样会被拒。不存在的路径在启动时也会报错，而不是被静默跳过。

  `allow_pty` 默认 `false`：受限进程不能分配伪终端（PTY）。要跑终端类程序——比如
  `qcode-terminal` 的集成测试，它用 `openpty` 起子进程——在 `mode = "landlock"` 下必须显式
  打开：

  ```toml
  [sandbox]
  mode = "landlock"
  allow_pty = true
  ```

  > 若你的程序会在 pty 上 `ioctl(TIOCSWINSZ)`（终端 resize、`vim`/`less` 这类全屏程序都会），
  > 还需要 `IOCTL_DEV`——`allow_pty` 已经一并给了，不必额外配置。

  **打开它的代价**：landlock 没有 mount namespace，只能共享**宿主**的 devpts，而
  `/dev/pts/N` 上你其它终端窗口的节点属于同一个 uid，所以这个授权等于"可以往未受限的
  shell 里输入"——一条从沙箱里出去的完整通道。因此它是显式开关而不是默认。`bwrap` 不需要
  它也不看它（`--dev` 挂的是 `newinstance` 私有 devpts）；有条件就用 `bwrap`。

- **MCP server 的沙箱是另一份授权（ADR-0066）**：同一个后端，但边界按 server 现算，
  而不是照抄 `run_shell` 的。运行时目录（由 `command` 推导：venv、`node_modules`、
  `<dist>/bin` 之类）**只读 + 可执行**；**写**只给 workspace 与每个 server 私有的
  `HOME`（`<data-dir>/mcp/<name>/home`，`XDG_*` 与 `TMPDIR` 也指向那里，所以 `npx`
  的 cache 不用再手工授权）。**读**不到真实 `$HOME`——`~/.ssh`、`~/.aws`、
  `~/.config/gh` 默认全都看不见，要哪一个就写 `credentials = ["github"]` 这类预设
  （只给该子树，并在私有 `HOME` 里放符号链接指过去，只读）。
  **`command` 可以写裸名字**（`wordcraft-cli`，不含 `/`），此时在两个地方查 PATH：
  qcode 自己的，以及——优先——**你 shell 环境里那份**（ADR-0070，就是 `run_shell` 用的
  同一份采样）。所以从 Dock 启动的 GUI 也找得到终端里才有的工具链，配置也不必随版本
  升级改路径。`[shell] env_sampling = "off"` 时退回 qcode 自己的 PATH。同一个 PATH 也
  交给子进程，否则"按 A 推导授权、按 B 启动进程"会指向两个不同的文件。
  `qc doctor` 会用同一份 PATH 推导并在起不来时点名查过哪份 PATH。
  推导之外还需要什么就写 `read_paths` / `writable_paths`；`/`、`$HOME` 和 workspace 的
  父目录不会被自动授予——`writable_paths` 里写这三样会被 grant 直接拒绝（`~/` 展开后就是
  `$HOME`，两个字符的笔误不该把整个家目录交出去）。两个字段都认 `~/x`，在加载时展开成
  真实 home，所以 `writable_paths = ["~/Documents"]` 是跨机器可移植的写法。
  **注意这只管配置文件**：把 `~/Documents/a.docx` 当参数传给某个 server 的 tool 时，
  qcode 不参与解析，是否认 `~` 完全看那个 server 自己（Rust 写的会把它当字面目录名，
  在 workspace 下建一个叫 `~` 的目录且不报错）。要可靠就传绝对路径。
  任何一个 server 都可以 `sandbox = "off"` 单独关掉沙箱，届时
  启动 warn、`qc doctor` 标出来。授权集在启动日志里以 `mcp server grant` 一行打印，
  `qc doctor` 会把每条配好的路径打成 `~/Documents → /home/q/Documents` 两种写法，
  并在目录不存在时说明它被跳过了。
  **写集严格等于上面那几个集合，此外一律无**：`run_shell` 需要的宿主 `/tmp` 和 Rust 构建
  缓存（`$CARGO_HOME`、`~/.cargo` 等）**不会**带给 MCP server——`TMPDIR` 已经指向私有
  `HOME`，而 `$CARGO_HOME/config.toml` 里的 `[build] rustc-wrapper` 等于任意代码执行。
  某个 server 确实要写别处，就写它自己的 `writable_paths`。
- **取消**：Ctrl-C 取消当前 turn（再按一次立即退出），流式响应立即停止，
  `run_shell` 对整组先 SIGTERM、2s 后 SIGKILL；已产生的输出保留并标记
  `cancelled`，事件日志以 `turn_ended(cancelled)` 收尾（退出码 130）。
- **超时**：`--timeout <秒>` 给整个 turn 设墙钟上限，超时走同一条取消路径
  （含进程组清理），退出码 124 以便与手工取消区分。
- **事件日志**：append-only JSONL，`seq` 连续校验，崩溃尾行丢弃；`turn_ended` 时
  `fsync`，同一 session 由 flock 保证单写者（第二个写者被拒绝，读仍可用）；
  resume 时给悬挂的 `tool_call` 补 `tool_result(interrupted)`。`sessions show` 可完整
  replay，`sessions fork` 分叉，`sessions gc --older-than-days N [--yes]` 清理旧会话。
- **大输出与索引**：工具输出被截断时，全文写到 `sessions/<id>/outputs/<call_id>.txt`，
  `tool_result.metadata.full_output_path` 记录路径；`index.sqlite` 缓存 session 元数据
  （turns、cost），索引缺失或与日志数量不一致时自动全量重建。
- **Provider**：mock + OpenAI-compatible（OpenAI / DeepSeek / Ollama / vLLM），
  支持文本、reasoning、工具参数增量、usage、取消与错误映射。首 token 之前最多 3 次
  指数退避（尊重 `Retry-After`），首 token 之后失败直接终止本 turn；只读工具并发由
  provider 的 `parallel_tools` 能力决定。
- **网络出口（v2 基础版）**：进程内在 `127.0.0.1` 起 CONNECT 代理，provider /
  `web_fetch` / `web_search` 强制经代理，失败关闭；域名 allowlist + 私网/loopback 拦截防 SSRF；
  shell/MCP 通过 `HTTP(S)_PROXY` 尽力而为。`unknown = "ask"` 由工具审批层处理
  （`web_fetch` 批准后加入 session allowlist）；shell/MCP 的未知域名直接拒绝。
  审计写 `<data>/logs/network.jsonl`；`[network] enabled = false` 才显式关闭。
- **AGENTS.md**：workspace 根的 `AGENTS.md` 注入 system prompt；超过 32KiB、非普通文件或
  symlink 会被拒绝（避免把 workspace 外内容喂给模型）。
- **Skills**：默认发现 `~/.agents/skills`、`~/.config/agents/skills`（`npx skills -g`
  安装的位置）、`~/.config/qcode/skills`、`[skills] roots`、`<workspace>/.agents/skills`
  与 `<workspace>/.qcode/skills` 下的 `SKILL.md`（公开 Agent Skills 格式）。system prompt
  只放 `name` / `description`；模型调用 `skill` 工具按需加载正文，或用 `file` 参数读
  `references/` 等附件。非法/超限/symlink 的 skill 跳过并告警，
  global < user < config < project(.agents) < project(.qcode)。`qc skills list` 可查看全部发现结果。
- **MCP（tools + resources + prompts，stdio 与 Streamable HTTP）**：启动时连接启用的
  server，`tools/list` 后以
  `mcp__<server>__<tool>` 注册，默认 ask（规则可按名称放行 / 拒绝）。`tools/call`
  有每 server 超时与取消；server 退出、协议错误只影响该 server，stderr 落
  `<data-dir>/logs/mcp-<server>.log`。输出同样 32KB 截断。
  server 声明了 resources / prompts 能力时，再注册
  `mcp__<server>__list_resources` / `read_resource` / `list_prompts` / `get_prompt`
  四个工具（能力靠握手时的一次 `list` 探测，服务器不支持就只丢这四个）。
  `[[mcp.servers]] env` 把变量追加到子进程环境，需要 token 或自建缓存目录的 server
  不必再写 wrapper 脚本；出口代理变量在这些之后注入，仍以 qcode 的代理为准。
  **远程 server 写 `url` 而不是 `command`**（二者恰好一个，ADR-0067）：走 Streamable HTTP，
  优先用 `server/discover`（协议 2026-07-28，无握手、结果带 `ttlMs`），对只认 `initialize`
  的存量 server 自动回退。HTTP 请求**只走出口代理**，因此同样受 `[network] allow`、
  `network.jsonl` 审计与私网拦截；没进 allowlist 的 host 会被 403 且不产生任何工具——先在
  `[network] allow` 里加一行。token 只写 `bearer_token_env_var = "VAR"`（变量名），
  `headers` 里出现 authorization/cookie/token/key/secret/password 会被配置校验拒绝。
  **工具表有寿命**：server 在 `tools/list` 里给的 `ttlMs` 决定多久重读一次（缺失=不重读，
  `0`=每个 turn 重读，`n`=n 毫秒后下一个 turn 重读），`notifications/tools/list_changed`
  立即置脏。重读只发生在 turn 边界，换表会写一条 `McpToolsRefreshed` 事件；已经被删掉的
  工具被调用时得到 `tool no longer available` 而不是 `unknown tool`。
  **server 的自述会进 system prompt**（ADR-0068）：握手里的 `instructions` 以
  `# MCP: <server>` 小节注入，总数封顶 8 KiB，超出截断并注明。这段文字只是文本——它不改变
  任何审批决定，显式 `deny` 规则照样 deny。
  **能力一律读握手里的 `capabilities`，连接期零探测**（不分协议版本）。想知道某个 server
  的声明是否属实，跑 `qc doctor --probe`：它按需连接、把 `resources/list` 与 `prompts/list`
  都问一遍，并报出两种相反的不匹配——「声明了却拒绝」（工具在、每次调用都失败）与
  「服务了却没声明」（工具静默缺失）。
  每个 server 的授权集（`read_paths` / `writable_paths` / `credentials` / `sandbox`）在设置页
  里有对应的列，也由 `qc doctor` 逐条打印；`qc mcp add --read-path/--write-path/--credential/
  --sandbox` 写的是同一份配置。
- **并行只读工具**：同一批里相邻的只读调用最多 4 路并发；写、shell 与 MCP 调用保持原位
  串行，`tool_result` 始终按调用顺序落日志，不随完成顺序变化。
- **apply_patch**：多文件 unified diff（create / delete / modify，支持 `\ No newline`）。
  先解析并计算全部文件再 staging + 备份 + rename；任一 hunk 失败则什么都不写。绝对路径、
  `..`、rename、symlink 逃逸拒绝；默认 ask。
- **checkpoint / rollback**：`write_file` / `edit_file` / `apply_patch` 动手之前，把目标文件
  当前内容记进 `<data-dir>/snapshots/<session>/<turn>/`（不存在的文件记为 absent）；
  `[agent] snapshot_shell`（默认 true）还会在每条 `run_shell` 前对整个工作区做一次受限快照
  （跳过 `.git`/`target`/`node_modules`/`.cargo-home` 等构建与 VCS 树，单文件 4 MB、每个 turn
  累计 64 MB 封顶），因此 shell 与 MCP 驱动的编辑也能回滚。
  `qc sessions checkpoints <id>` 看记录，
  `qc sessions rollback <id> --to <turn> --yes` 或 TUI 里 `/rollback <turn>` 还原；
  `qc sessions gc --yes` 会顺带清理已删会话的快照与孤儿树（详见 ADR-0034）。
- **GUI**：点图标/双击打开 GPUI Kit 桌面窗口（ADR-0037/0047/0048；`qc` 没有 `gui` 子命令，
  见 ADR-0086——终端里敲 `qc` 永远是 TUI）：与 `qc run` / `qc acp` /
  `qc tui` 使用同一个 `qcode_kernel::assemble`。左侧是“我的任务”，Task 可配置多个目录；
  支持 Task 新建/重命名/删除、目录增删/Relink/RO-RW/主目录、Turn 边界 pending Roots、
  审批浮层、图片粘贴、diff 视图、中断与 Settings 热重建。Composer 为 `Enter` 发送、
  `Shift+Enter` 换行。release 只提供一个 `qc`（ADR-0086）。
- **图片输入**：`qc run --image shot.png "看看这个"`（可重复，PNG/JPEG/WebP/GIF，单张 ≤8 MB）
  把图片作为多模态消息发给 provider；provider 的 `Capabilities.images` 为 false 时直接报
  `this provider does not accept image input`，不会先发再让服务端 400。
  TUI 里 `Ctrl-V` 直接粘贴剪贴板图片（Windows 的 `CF_DIB` → 内置 PNG 编码器），随下一条消息发出。
  事件日志只留文本，所以 resume 的历史不含图片；ACP 的图片输入尚未实现（详见 ADR-0035）。
- **web_fetch**：http(s) 文本抓取，15s 超时、5 次 redirect、100KiB 截断；非文本、二进制、
  URL 内凭据拒绝。默认 ask，不注入任何 API key；不做 SSRF 白名单（审批提示里有完整 URL）。
- **web_search**：`[search]` 开启后注册（默认 `brave`，也支持 `tavily` / `searxng`），
  返回 `title` / `url` / `snippet`，`max_results` 上限 20；key 只从环境变量读
  （`BRAVE_API_KEY` / `TAVILY_API_KEY`，或用 `api_key_env` 指定，或用 `api_key_command` 从 Keychain / libsecret / 凭据管理器取），searxng 需 `base_url`。
  默认 ask（`Network`，与 `web_fetch` 同一套审批与 session allowlist），走同一个出口代理；
  带 `page_age` 时输出新鲜度，凭据不进 transcript。
- **todo_write**：session 待办清单（pending / in_progress / completed），不写文件系统。
  每次成功调用后清单进入 system prompt，TUI 在 transcript 里渲染成 checklist；空数组清空，
  至多一个 in_progress，上限 50 项、每项 200 字符。resume / replay 从事件日志重建状态。
- **上下文压缩**：估算达到窗口 78% 时，保留最近 2 轮；先把旧工具输出中超过 4k 字符的
  部分驱逐出摘要输入，再把更早历史用同一模型摘要成 `compaction` 事件；replay / resume
  应用同一摘要。窗口由 `[context] window_tokens` 配置。
- **成本**：配置 `[pricing."<model>"]` 后，`turn_ended.cost_usd` 与人类模式输出都带估算；
  `--max-cost` 在模型响应后、工具执行前检查，**到 95% 即暂停**（不再花超才停），
  余下的留一次无工具收尾调用写交接摘要。
- **预算暂停（ADR-0060）**：`max_turns` / `max_cost` 用尽时 turn 落 `paused` 而不是
  `failed`——工作完好，用户决定继续。跨过 75% 时先发一次 `budget_hint`（GUI 提示卡、
  TUI 状态行都读它，不再数对话条数）。暂停时给出一份交接：`PauseInfo.summary`（模型的
  「已完成 / 未完成 / 下一步」，不计入轮数）+ 从事件流推导的改动与命令清单。
  `qc run` 退出码 **75**；`qc run --resume <id> --continue` 接着跑，
  `--raise-turns` / `--raise-cost`（`--raise-persist` 写回配置）只抬这一个窗口。
  TUI 用 `/continue`、`/wrap`；GUI 是三键卡片（继续 / 收尾 / 先这样）。
- **子 agent**：`subagent` 工具把独立任务交给新会话运行（同 provider / policy / approver、
  同 workspace、独立 JSONL；工具集去掉 `subagent`，深度固定为 1）。默认 20 轮、上限 40，
  返回最终文本与子 session id；子会话的 usage/cost 汇总进父 `turn_ended` 与 TUI 成本。
  TUI 里子 agent 的文本与工具调用以 `↳` 嵌套显示，不改父状态机。默认 ask（`External`），
  非交互模式需要 `--yes` 或策略放行 `subagent`。
- **ACP**：`qc acp` 在 stdio 上讲 ACP v1，编辑器可直接把 qcode 当 agent（Zed 的
  `agent_servers` 里 `command` 指向 `qc`、`args = ["acp"]`）。支持 `initialize` /
  `session/new` / `session/load` / `session/prompt` / `session/cancel`，
  引擎事件映射成 `session/update`（文本、思维、工具调用与结果），
  审批映射成 `session/request_permission`（allow-once / allow-session / reject）。
  工具仍在本机执行，目前不委托客户端 `fs/*`、`terminal/*`，也不接受客户端传入的
  MCP server；`qc doctor` 打印协商的协议版本。
- **诊断**：`qc doctor` 检查配置、凭据、数据目录、toolchain、`web_search` 配置，并对非 mock
  provider 做一次真实连通性探测（10s 超时；认证/请求错误记为 fail，网络错误记 warn）。
  MCP 也有一段：逐 server 打印它实际拿到的授权集（运行时目录 / 可写目录 / 凭据 / 私有
  `HOME`），并把四种「配了但没用」的情形标成 warn——`read_paths` 指向不存在的路径、
  `credentials` 预设在本机没有对应文件、`sandbox = "off"`、以及 `[sandbox] mode = "none"`
  让 `[mcp] sandbox = "confine"` 形同虚设。`env` 里形如 `GITHUB_TOKEN = "ghp_..."` 的
  字面量密钥也会被点名（值不打印），提示改写成 `"${GITHUB_TOKEN}"`。
  加 `--probe` 则会**真的连上去**：逐 server 打印协商的协议版本、握手里声明了哪些能力、
  以及因此注册了哪些工具，再把 `resources/list` 与 `prompts/list` 都问一遍做比对。默认不开，
  因为一次会话不该为每个 server 付这个往返；启动期的日志只有一行
  `mcp capabilities read from the handshake` 说明判定依据。
- **自动更新**：`qc update --check` 只报告；`qc update` 下载并就地替换当前二进制（旧文件留
  `<exe>.old-<时间戳>`）。发布物是裸二进制 `qc-<version>-<os>-<arch>[.exe]` 与 `SHA256SUMS`，
  只认 GitHub Releases API 给出的 `browser_download_url`，校验和不符则不落盘——没有归档解包器，
  也没有自签名单。启动时最多每 `[update] check_days` 天查一次，只在 stderr 提一句
  `notice: qc <x> is available`；`enabled = false` 或 `check = false` 可关（企业内网默认关）。
  用 brew / scoop / winget 装的那份交给包管理器升级，别用 `qc update` 覆盖它——
  包管理器会认为文件被改过。更新器只服务「自己从 Releases 下载的裸二进制」。

v0.10.1 状态：一个 `qc`（终端里是 TUI，点图标是窗口，ADR-0086）、Task/目录模型
（一个 Task 可挂多个目录，RO/RW 按目录授权）、
三端系统托盘与 Queue/Steer、Settings 热重建、附件内容寻址存储、内建终端、
可折叠侧栏、shell 环境采样、配置文件 schema 2、会话日志事件 schema 8 已落地。
多根 sandbox 三端齐备：macOS `sandbox-exec`、Linux `bwrap` 与 Landlock、Windows ACL
都按 Task 的 roots 逐目录建规则。逐版本明细见 [CHANGELOG.md](CHANGELOG.md)，
分发产物（发布工作流、SHA256SUMS、brew/scoop/winget 清单）见「发布构建」。

## 配置

```toml
# ~/.config/qcode/config.toml
# 架构版本。缺省即 1；声明比本 build 更新的版本会启动失败（先升级 qcode），
# 未识别的键只告警、不阻塞（新版本加的键能在旧 build 上原样加载）。
version = 1
default_provider = "deepseek"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"   # 只存变量名，不存 key
default_model = "deepseek-flash"

# Anthropic Messages API（claude 是等价别名）。base_url 不带 /v1 时自动补
# /v1/messages；若网关已经带 /v1，也会正确拼成 /v1/messages。
# [providers.anthropic]
# base_url = "https://api.anthropic.com"
# api_key_env = "ANTHROPIC_API_KEY"
# default_model = "claude-sonnet-4-5"

# 上下文窗口（token），用于 78% 压缩阈值；按模型实际窗口调整。
[context]
window_tokens = 128000

# 一次用户消息最多几次模型调用。正常结束是模型不再调工具，这只是防跑飞的兜底；
# 技能/勘察类任务给 30–50，`qc run --max-turns N` 覆盖它（TUI 用这个值）。
[agent]
max_turns = 64
# max_cost_usd = 0.50    # 需要 [pricing]；`--max-cost` 覆盖它，比轮数更贴近真实代价
snapshot_shell = true   # run_shell 前快照工作区（4MB/文件、64MB/轮封顶）；大仓库可关

# Web 搜索（v2.0）。enabled 默认 true；未配置 key 时不注册 web_search 工具，
# 由 qc doctor 提示。backend: brave | tavily | searxng。
# key 只从环境变量读：默认 BRAVE_API_KEY / TAVILY_API_KEY，可用 api_key_env 改名；
# 也可指向 .env 风格的变量名，不写进配置文件。searxng 自建实例用 base_url。
[search]
enabled = true
backend = "brave"
# api_key_env = "BRAVE_API_KEY"
max_results = 5     # 1..=20
# base_url = "http://localhost:8888"   # searxng 必填；brave/tavily 可选覆盖端点

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

# MCP：stdio server 需 command，远程 server 需 url，二者恰好一个（ADR-0067）。
# enabled 默认 true，timeout_secs 默认 120。
# 工具名自动加 mcp__<server>__ 前缀；审批规则按完整工具名精确匹配。
# server 声明 resources / prompts 能力时，额外注册
# mcp__<server>__list_resources / read_resource / list_prompts / get_prompt 四个工具。
[mcp]
sandbox = "confine"   # 默认；每个 server 都可以单独覆盖成 "off"

[[mcp.servers]]
name = "filesystem"
# `command` 写裸名字（不含 `/`）时走 PATH 查找，用的是**你的 shell 环境**里那份
# PATH（ADR-0070，与 run_shell 同一份采样），所以从 Dock 启动的 GUI 也找得到。
# 绝对值路径则完全按字面解析，不做任何查找。
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
# 子进程不再继承你的环境（ADR-0068）：只有 PATH/LANG/TZ/LC_*/合成 HOME/出口代理会传下去。
# 值写成 ${NAME} 表示从 qcode 的环境读，密钥因此不进配置文件。
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }
enabled = true
# 沙箱授权（ADR-0066）：运行时目录由 command 推导，读不到真实 $HOME。
read_paths = ["/opt/some-tool"]   # 可选，推导之外的只读+执行目录
writable_paths = ["~/Documents"]  # 可选；默认 workspace + 私有 HOME 已可写。
                                  # `~/x` 在加载时展开，`$HOME` / `/` 会被拒绝
credentials = ["github"]          # 可选：github / aws / ssh / npm / netrc / docker
sandbox = "confine"               # 可选，覆盖 [mcp] sandbox

# 远程 server（ADR-0067）：没有子进程，因此 args/env/read_paths/writable_paths/
# credentials/sandbox 一律不许出现（配置校验会拒绝——它们在这里是空操作）。
# 它的唯一边界是出口代理，所以该 host 必须进 [network] allow。
# [[mcp.servers]]
# name = "context7"
# url = "https://mcp.context7.com/mcp"
# bearer_token_env_var = "CONTEXT7_TOKEN"   # 变量名；token 本身不进文件
# [mcp.servers.headers]                     # 可选，只放非敏感头
# Accept = "application/json, text/event-stream"

# Skills：默认开启；enabled = false 可完全跳过发现。
# 默认还会扫描 ~/.agents/skills（npx skills -g）、~/.config/agents/skills、
# ~/.config/qcode/skills、<workspace>/.agents/skills、<workspace>/.qcode/skills。
# roots 是额外目录（可选），按顺序覆盖 global/user；project 仍最高优先级。
[skills]
enabled = true
# roots = ["~/space/skills/skills"]

# 应用级出口策略（v2）。provider 的 host 会自动加入 allowlist。
[network]
mode = "proxy"          # off | proxy | strict；strict 仅 macOS sandbox-exec
enabled = true
allow = ["api.openai.com", "api.deepseek.com", "registry.npmjs.org"]
allow_private = false   # 本地 Ollama/测试才打开
unknown = "ask"         # ask | deny | allow；shell/MCP 的未知域名按 deny
audit = true

# strict 网络 hard mode：需 mode = "sandbox-exec"（macOS），Seatbelt 只放行
# loopback 代理端口，shell 直连被拒。
# OS 沙箱（可选，默认 none）。sandbox-exec 仅 macOS，bwrap/landlock 仅 Linux，
# windows-acl 仅 Windows（只限制写；启动时 preflight，失败则拒绝启动）：
[sandbox]
mode = "none"        # none | sandbox-exec | bwrap | landlock | windows-acl
network = false      # proxy 模式下放网开关；strict 时忽略，只允许代理端口
extra_writable = []  # 所有后端：除 workspace 与私有 temp 外额外可写的根（整棵子树）
allow_pty = false    # landlock：允许受限进程分配伪终端（默认关，见下）
extra_read = []      # 所有后端：额外只读（含可执行）的根，整棵子树递归。
                     # 这两项都认 `~/cot`，在加载时展开成真实 home。
                     # `/`、home 本身、`/home`、`/Users`、`/root` 不能写进 extra_writable

# 自动更新（v2）。enabled 默认 true；check = false 时只手动 update --check。
# check_days 是启动时查询新版本的最小间隔（结果缓存进 data-dir）；repo / base_url
# 让 fork 与内网镜像可用，base_url 需是 GitHub Releases API 形状。
[update]
enabled = true
check = true
check_days = 7
repo = "qiuzhanghua/qcode"
# base_url = "https://api.github.com"
```

`qc config show` 打印合并后的有效配置（只显示 `api_key_env` 变量名，不显示密钥）；
`qc config edit` 用 `$EDITOR`/`$VISUAL` 打开用户配置文件。`qc mcp list` 读取合并配置，
`qc mcp add/remove` 直接重写用户配置里的 `[[mcp.servers]]`（会丢失该文件的注释）。

`qc skills list` 打印实际发现结果与被检查的根，排查“skill 没生效”时先跑它。

`[[approval.rules]]` 的 `tool` 写完整工具名（如 `mcp__filesystem__read_file`）来放行单个
MCP 工具；规则按工具名精确匹配。

规则字段：`tool`、`command_prefix`、`path_glob`（匹配 `path` 参数）、`action`、
`scope`（`user` / `project` / `session`，默认 user）。匹配顺序：任一 deny 胜出；否则
约束最多、scope 最窄、前缀/glob 更长者优先。

项目配置放在 `<workspace>/.qcode/config.toml`，叠加在用户配置之上：可覆盖
providers / pricing、追加 MCP server、追加 skills roots，并追加审批规则；文件里出现
`[context]` / `[sandbox]` 时才覆盖对应段（缺省不会把用户值重置为默认）。项目规则强制
`scope = "project"`，且 `allow` 会降级为 `ask`——仓库不能给自己提权。项目文件同样受
`version` 约束：声明超出本 build 支持的版本会拒绝启动，未识别键只告警。

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
`default_model` 可省略：DeepSeek 缺省使用 `deepseek-flash`，Anthropic 缺省使用 `claude-sonnet-4-5`，其他 provider 缺省会报错并要求显式指定。
`RUST_LOG=info` 打开 stderr 诊断日志；stdout 永远是干净的 JSONL。

常用 flag：`--yes`、`--cwd`、`--resume`、`--max-turns`、`--max-cost`、`--timeout`、`--skill`、`--data-dir`、`--config`、`--approval-policy`、`--include-raw`（`--json` 下保留 `provider_raw`）。
`--max-cost` 必须有对应 `[pricing."<model>"]`，否则启动即报错（避免把预算当成无限制）。

## 文档

- [更新日志](CHANGELOG.md) —— 版本变更
- [设计决策](docs/design/DECISIONS.md) —— 唯一事实来源
- [v1 实现规格](docs/design/SPEC.md) —— 架构、事件模型、工具契约
- [v2 实现规格（草案）](docs/design/V2-SPEC.md) —— Windows、网络出口、ACP、GUI 分期
- [GUI v3 实现日志](docs/design/GUI-V3.md) —— Task/目录、Task 管理、发送键与 Settings 同步
- [评测方案](docs/design/EVAL.md) —— 离线 fixture 与 5 条任务
- [性能基线](docs/design/PERF.md) —— 10 万事件会话的 load / list / replay
- [M0 任务清单](docs/design/M0-TASKS.md) —— 基础设施与人工验收
- [M1 任务清单](docs/design/M1-TASKS.md) —— 工具循环、审批与取消
- [M2 任务清单](docs/design/M2-TASKS.md) —— TUI、压缩、AGENTS.md 与 MCP
- [M3 任务清单](docs/design/M3-TASKS.md) —— apply_patch、web_fetch、打包与 sandbox 取舍
- [Skills 设计](docs/design/SKILLS.md) —— 发现、渐进披露与安全边界
- [ACP 接入](docs/design/ACP.md) —— Zed / Neovim 配置、验收清单与兼容性测试
- [资料来源](docs/design/SOURCES.md) —— clean-room 白名单

## 布局

```
crates/qcode-core        domain、event、store、task/roots、policy、tool、todo、engine
crates/qcode-providers   provider 实现（mock + OpenAI-compatible + Anthropic Messages）
crates/qcode-tools       read_file / write_file / edit_file / apply_patch / list_dir / glob / grep / run_shell / web_fetch / web_search / todo_write / subagent / skill
crates/qcode-mcp         MCP client（stdio + Streamable HTTP，tools/resources/prompts）
crates/qcode-eval        eval runner（qc-eval：fixture + planted/hidden + checks）
crates/qcode             CLI 包（包名 qcode，二进制 qc）、配置解析、doctor
crates/qcode-kernel      kernel assembly、config、provider/tool/sandbox 组装
crates/qcode-tui         ratatui 前端（状态投影、渲染；TUI 入口在 qc）
crates/qcode-ui-state    共享 UI 状态/事件投影
crates/qcode-ui-bridge   GUI engine 命令/事件桥
crates/qcode-gui         GPUI 桌面 GUI（Task 侧栏、目录编辑、审批、diff）
crates/qcode-acp         ACP v1 服务端（`qc acp`：stdio JSON-RPC，供编辑器接入）
packaging/               安装清单模板（Homebrew / Scoop / winget）与 render.sh
.github/workflows/       ci.yml（门禁）、release.yml（tag 发布）
```

## License

MIT，见 [LICENSE](LICENSE)。版权归 qiuzhanghua。第三方依赖的许可证与已知漏洞用
`cargo deny check` 审计（配置见 [deny.toml](deny.toml)，CI 同步运行）。
