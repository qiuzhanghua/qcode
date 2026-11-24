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
| 管理器 | rustup（cot 的 rust 插件就是） | `repo/cargo/bin/{rustc,cargo}` 是 `rustup` 的符号链接，`RUSTUP_HOME=/home/q/cot/cache/rustup`，所以 `rustup target add` 直接可用。**解包的发行版**（老式 cot 插件）没有 rustup，那种机器装 target 要另想办法（见下） |

## 已安装的 target

`make check-platforms` 只检查**非宿主**的 target，所以列表要和发布矩阵对得上。
`Makefile` 的 `PLATFORMS` 有 8 个名字，减去宿主 `x86_64-unknown-linux-gnu`
还剩 **7 个**，本机 7 个都装上了（其中 musl 两个只用于 check，见「musl」一节）：

```bash
rustup target add \
  aarch64-unknown-linux-gnu  x86_64-unknown-linux-musl  aarch64-unknown-linux-musl \
  x86_64-pc-windows-gnullvm  aarch64-pc-windows-gnullvm \
  aarch64-apple-darwin       x86_64-apple-darwin
# 宿主 x86_64-unknown-linux-gnu 自带
```

这段以前漏过一次 `x86_64-unknown-linux-musl`（矩阵里 linux 有 4 项：gnu/musl ×
amd64/arm64，照着抄只有 3 项），后果是 `make linux-musl-amd64` 在 `--list` 里显示
「sysroot 里没有该目标的 std」——看起来像工具链坏了，其实是这一行少抄了一个 triple。
对账办法是两边都数一遍：`--list all` 的 linux 目标数 == `Makefile` 的 `PLATFORMS` 里的
linux 目标数。

只有**解包的发行版**（没有 rustup）才需要仓库里的脚本把 `rust-std-*.tar.xz` 解进 sysroot：

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
| `*-pc-windows-gnullvm` | llvm-mingw 的 `<arch>-w64-mingw32-clang` | 有（`llvm-mingw_20261117`，clang 23.1.6；两个 triple 的 check + clippy 均通过） |
| `*-apple-darwin` | **zig**（自带 macOS SDK：libSystem stub 与头文件） | 有（0.17.1），两个 triple 的 check + clippy 均通过 |

llvm-mingw 没有对应的 `install-*.sh`（只有 zig 有）。本机的装法是 cot 插件
（`cot i …/llvm-mingw_20261117_linux_amd64.tar.zst`），它同时把 `bin/` 放进 `PATH` 并导出
`LLVM_MINGW_DIR`——正好喂上 `find_mingw_clang` 的查找顺序「PATH 优先，其次
`$LLVM_MINGW_DIR/bin`」。

手工装则从 release 页取 **`llvm-mingw-*-ucrt-ubuntu-*-x86_64.tar.xz`**，解开后同样把
`bin/` 加进 `PATH` 或导出 `LLVM_MINGW_DIR`。下面这几个包的命名区分的**不是 CPU 架构，
而是宿主**，下载页上最容易挑错：

| 包 | 宿主 | |
| --- | --- | --- |
| `…-ucrt-ubuntu-22.04-x86_64.tar.xz` | Linux x86_64 | **要的就是这个**。上游只出到 22.04，老 glibc 向前兼容：实测它要 `GLIBC_2.34`，24.04 是 2.39 |
| `…-ucrt-ubuntu-22.04-aarch64.tar.xz` | Linux aarch64 | 换的是宿主 CPU |
| `…-ucrt-<arch>.zip` | Windows | 里面全是 `.exe`，在 Linux 上是死文件 |
| `…-ucrt-macos-universal.tar.xz` | macOS | |

两条查找路径都不在时，`--list`/`check-platforms` 会明说 Windows 两个 triple 没被检查，
而不是假装绿。

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
sudo apt install musl-tools        # 可选：musl 的 C 驱动（只有宿主架构，见「musl」一节）
```

- 前一行是 **GUI 的字体/输入栈**：`cargo build --workspace` 会连 `qcode-gui` 一起编，
  运行时包不够，要 `-dev` 包提供的 `.pc` 与不带版本号的 `lib*.so`。常见报错与缺的包对照
  见 README「Linux 构建依赖」。
- `bubblewrap` 只影响 `cargo test -p qcode-tools --lib sandbox::tests::linux_tests`；
  没有它，那条测试会被跳过而不是失败（CI 里装了，所以 CI 上是真跑）。

## musl：只做 check 与无 GUI 包的 build，不出发布件

`*-unknown-linux-musl` 在本机是**验证目标**，不是发布目标。`.github/workflows/release.yml`
的矩阵只有 `linux-amd64` / `linux-arm64`（两个 gnu），`scripts/build-linux-in-container.sh`
里也写着容器路径不覆盖 musl——`Makefile` 的 `PLATFORMS` 列着 musl 两个名字，但没有哪条
流水线产出它们。

能跑通的部分：

```bash
eval "$(scripts/musl-env.sh)"                            # 宿主架构（x86_64）
eval "$(scripts/musl-env.sh aarch64-unknown-linux-musl)" # 或指定

cargo check --workspace --target x86_64-unknown-linux-musl   # 17 个 crate 全过
cargo build --target x86_64-unknown-linux-musl -p qcd        # 静态 PIE，能直接跑
```

`qcd` 产出的确实是 musl 静态件（`file` 报 `static-pie linked`），没有 gpui 依赖，
所以这条路是真的可用，不是「check 绿了就转圈」。

**`qc` 链接不出来**，这是结构性限制，不是缺包：

```plain
rust-lld: error: unable to find library -lxkbcommon
```

gpui 在 Linux 上硬链接 `xcb` / `xkbcommon` / `xkbcommon-x11`（链接行里按名字要），这几个
库只有 glibc 版本；Debian 不提供 musl 编的整套，自己编一套属于另一件事。`--list` 因此把
musl 两个目标列为 skip 并写明原因——以前它显示 `ready`，而真编到链接才炸。

`musl-env.sh` 打印四样东西，每样都对应一个「看起来像仓库 bug」的报错：

| 项 | 没有它时报什么 |
| --- | --- |
| `CC_<triple>` / `CXX_<triple>` | `cc-rs: failed to find tool "x86_64-linux-musl-gcc"`（cc-rs 按名字找，宿主 gcc 顶不上） |
| `RUST_FONTCONFIG_DLOPEN=1` | `pkg-config has not been configured to support cross-compilation` 的 panic |
| `CARGO_TARGET_<TRIPLE>_LINKER` | 默认用宿主 `cc` 链接 → `cannot find -lxcb` / `-lc++` |
| `RUSTFLAGS=-Lnative=…` | `rust-lld: unable to find library -ldl`（`rust-lld` 不读 musl 驱动的 spec） |

工具链怎么来，按优先级：

1. 真的交叉 gcc：`musl-tools`（只有宿主架构、**没有 `g++`**）或 musl.cc 的整套
   （每架构 ~115 MB）。在 PATH 上就直接用，`scripts/install-musl-gcc.sh` 也会跳过它。
2. 缺的部分由 zig 补：`scripts/install-musl-gcc.sh` 把 `.toolchains/musl-gcc/bin/` 里
   缺的 `<arch>-linux-musl-{gcc,g++}` 做成指向 `scripts/zig-cc-musl.sh` 的符号链接
   （`--check` 只报告，自带 smoke test：真编一个 `.o` 再确认是 ELF）。之所以要 `g++`：
   `freetype-sys` 在 pkg-config 失败时退回编自带的 C++ 源码，而 cc-rs 找的是
   `<arch>-linux-musl-g++`，`musl-tools` 不提供。

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
rustc / cargo   /home/q/cot/repo/cargo/bin/         （rustup 的符号链接，rustup 1.29.3）
rustup 家目录    /home/q/cot/cache/rustup            （RUSTUP_HOME；RUSTUP_DIST_SERVER=rsproxy.cn）
sysroot         /home/q/cot/cache/rustup/toolchains/stable-x86_64-unknown-linux-gnu
zig             /home/q/cot/lib/zig_0.17.1_linux_amd64/zig
llvm-mingw      /home/q/cot/lib/llvm-mingw_20261117_linux_amd64/（cot 插件；bin/ 在 PATH，LLVM_MINGW_DIR 同指这里）
```

这段是 2026-11-24 复核的结果（llvm-mingw 那一行是同日晚些时候补的）。旧的写法是
`repo/cargo/bin` 为「解包发行版、无 rustup」、sysroot 在
`lib/rust_1.100.0_linux_amd64_gnu`——cot 的 rust 插件换成 rustup 之后这两条都不成立了，
`rustup target add` 也随之可用（这正是上面「已安装的 target」那段现在能直接跑 `rustup`
的原因）。

`build-for-release` 的 `find_zig` 按 `$ZIG` → `PATH` → 仓库 `.toolchains/` → cot 的插件目录
查找，`scripts/install-zig.sh` 装到 `.toolchains/` 正是为了进这条查找链。

## 自检清单

按顺序跑，任一条红了就先修它：

```bash
rustc -vV | grep '^host'                 # 宿主 triple
rustup target list --installed           # 或看 sysroot/lib/rustlib/
command -v x86_64-w64-mingw32-clang      # Windows 交叉 C 编译器
command -v zig && zig version            # darwin 交叉 C 编译器
scripts/install-musl-gcc.sh --check      # musl 的 C/C++ 驱动（缺什么补什么）
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
