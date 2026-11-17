# 开发环境

一台机器要能**构建、测试、发布** qcode，需要哪些工具，以及每个工具为什么在列表里。
判断标准是「能不能跑 `make check` 与 `make check-platforms`」，不是「装过 Rust 就算」。

这份文档只写**已验证**的组合，每条都注明验证方式。没验证过的一律标注为未验证。

## 一句话版本

```bash
make check             # fmt / clippy -D warnings / test / cargo-deny
make check-platforms   # 宿主编不了的那几个 target 的类型检查
```

两条都绿，这台机器就算就绪。缺工具时两条命令会**说出缺哪一个**（而不是在 build script
深处报一个看不懂的错），照着提示补即可。

## Rust 工具链

| 项 | 值 | 说明 |
| --- | --- | --- |
| rustc | 1.100.0 | 本机验证版本；`Cargo.toml` 的 MSRV 也是 **1.100**（与容器镜像、CI `msrv` job 同一版，只有一个数字） |
| edition | 2024 | 需要 1.85+，MSRV 已高于此 |
| 宿主 | `x86_64-unknown-linux-gnu` | 本文所有验证都在此宿主上完成 |
| 管理器 | rustup 或不带 rustup 的发行版 | cot 工具链是解包的发行版、**没有 rustup**，装 target 要另想办法（见下） |

## 已安装的 target

`make check-platforms` 只检查**非宿主**的 target，所以列表要和发布矩阵对得上。
发布矩阵是 8 个目标（`Makefile` 的 `PLATFORMS`），本机把 6 个原生目标 + 2 个 darwin
都装上了：

```bash
rustup target add \
  aarch64-unknown-linux-gnu     aarch64-unknown-linux-musl \
  x86_64-pc-windows-gnullvm     aarch64-pc-windows-gnullvm \
  aarch64-apple-darwin          x86_64-apple-darwin
# 宿主 x86_64-unknown-linux-gnu 自带
```

没有 rustup 的机器（cot 工具链）用仓库里的脚本把 `rust-std-*.tar.xz` 解进 sysroot：

```bash
scripts/install-rust-std.sh aarch64-apple-darwin rust-std-1.100.0-aarch64-apple-darwin.tar.xz
```

**std 必须与编译器同版本**，否则加载时报一个既不说版本也不提这个脚本的错。

## C 交叉编译器

Rust 侧装好 std 还不够：`libsqlite3-sys` 与 `aws-lc-sys` 的 build script 要一个能为目标
平台产出目标文件的 C 编译器。缺了它，`cargo check --target …` 会在 build script 里失败，
而报错指向某个 C 库，**一个字都不提工具链**——看起来像仓库的 bug。

| 目标平台 | 需要 | 本机验证 |
| --- | --- | --- |
| `*-pc-windows-gnullvm` | llvm-mingw 的 `<arch>-w64-mingw32-clang` | 有（`llvm-mingw_20261006`），两个 triple 的 check + clippy 均通过 |
| `*-apple-darwin` | **zig**（自带 macOS SDK：libSystem stub 与头文件） | 有（0.17.1），两个 triple 的 check + clippy 均通过 |

darwin 那条的两个坑（都已写进 `scripts/zig-cc-darwin.sh`，改脚本前先看它）：

1. cc-rs 认出「像 clang」后会追加 `--target=arm64-apple-macosx`（Apple 拼法），
   而 zig 只认 `aarch64-macos`，遇到不认识的架构名直接失败并打印可选架构列表；
2. 编译器语言不能靠 `$1` 传：cc-rs 的家族探测（`<cc> -E detect_compiler_family.c`）用的
   是同一个值，带位置参数的包装会把探测弄坏。改用 `ZIG_CC_TOOL`。

zig 没装时：

```bash
scripts/install-zig.sh            # 装到 .toolchains/（已 gitignore），打印 bin 目录
scripts/install-zig.sh --check    # 只报告，不联网
```

## Linux 系统依赖

只有 Linux 需要；Windows / macOS 跳过这一节。

```bash
sudo apt install libfontconfig-dev libfreetype-dev \
                 libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev
sudo apt install bubblewrap        # 沙箱的运行时测试
```

- 前一行是 **GUI 的字体/输入栈**：`cargo build --workspace` 会连 `qcode-gui` 一起编，
  运行时包不够，要 `-dev` 包提供的 `.pc` 与不带版本号的 `lib*.so`。常见报错与缺的包对照
  见 README「Linux 构建依赖」。
- `bubblewrap` 只影响 `cargo test -p qcode-tools --lib sandbox::tests::linux_tests`；
  没有它，那条测试会被跳过而不是失败（CI 里装了，所以 CI 上是真跑）。

## 环境变量

不需要手工设。两个自动生效的入口：

- 配置与数据目录：`QCODE_CONFIG` / `QCODE_DATA_DIR`（见 README「配置」）；
- 图形启动的环境补救：Linux 的 `.desktop` 与 macOS 的 LaunchServices 启动拿不到交互式
  shell 的环境（`~/.zshrc` 里 `source` 的密钥文件不在其中）。qcode 启动时用登录 shell
  采一次环境并补上缺的变量，日志里是
  `restored environment from the login shell … added=N sampled=M`。
  关掉它：`[shell] env_sampling = "off"`。见 ADR-0094。

## 这台机器上的实际布局（cot 托管）

不是要求，只是本机的事实，便于对照：

```plain
rustc / cargo   /home/q/cot/repo/cargo/bin/         （解包发行版，无 rustup）
sysroot         /home/q/cot/lib/rust_1.100.0_linux_amd64_gnu
zig             /home/q/cot/lib/zig_0.17.1_linux_amd64/zig
llvm-mingw      /home/q/cot/lib/llvm-mingw_20261006_linux_amd64/bin/
```

`build-for-release` 的 `find_zig` 按 `$ZIG` → `PATH` → 仓库 `.toolchains/` → cot 的插件目录
查找，`scripts/install-zig.sh` 装到 `.toolchains/` 正是为了进这条查找链。

## 自检清单

按顺序跑，任一条红了就先修它：

```bash
rustc -vV | grep '^host'                 # 宿主 triple
rustup target list --installed           # 或看 sysroot/lib/rustlib/
command -v x86_64-w64-mingw32-clang      # Windows 交叉 C 编译器
command -v zig && zig version            # darwin 交叉 C 编译器
pkg-config --modversion fontconfig freetype2 xcb xkbcommon xkbcommon-x11
cargo fmt --all --check
make check
make check-platforms
```

`make check-platforms` 的三种结局都要会读：

| 输出 | 含义 |
| --- | --- |
| `cross-check: N target(s) type-check` | 通过。注意上面的 `checked, but not over the whole workspace` |
| `not checked (target std not installed)` | 缺 std，先装 target |
| `not checked (no cross C compiler on PATH)` | 缺 llvm-mingw 或 zig，按括号里说的补 |
| `error: no non-native target was checked` | **一个都没检查**，绿色的空转，必须修 |

## 交叉检查覆盖到哪里

`scripts/ci-cross-check.sh` 在 Linux 宿主上能覆盖 4 个 target，但**不是全 workspace**：

- Windows 两个 triple：全覆盖（llvm-mingw 在位时）；
- darwin 两个 triple：17 个 crate 覆盖 15 个，排除 `qcode-gui` 与 `qcode`。

排除的原因是 `gpui-pre-apple` 的 build script 是
`#[cfg(any(target_os = "macos", target_os = "ios"))] fn main()`，而 **build script 按宿主
平台编译**：Linux 上 `main` 是空的，Apple 专属的 `include_str!(…stitched_shaders.metal)`
永远没有文件可读。这不是缺库或缺 SDK，是「需要一个 macOS 宿主」，配置层绕不过去。

被排除的 `qcode` 里就有 `boot.rs` / `commands/gui.rs`——`#[cfg(target_os = "macos")]` 与
Linux 共用同一份源码的地方。那部分只有 CI 的 `test (macos-latest)` 与
`build (macos-26, …)` 兜着。脚本会把这句打出来，不会让一个绿勾暗示「darwin 全查过了」。

CI 的 ubuntu job 也跑这条（ADR-0095），所以它不再是「本地自愿」的门禁。

## 相关文档

- [设计决策](design/DECISIONS.md) —— ADR-0095（darwin 交叉检查）、ADR-0094（图形启动补环境）
- [README](../README.md) —— 快速开始、Linux 构建依赖、发布构建
- `.github/workflows/ci.yml` —— 门禁的真实定义，本文只是它的说明
