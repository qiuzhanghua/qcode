# Changelog

## 未发布

### 保存设置不再把 `~` 固化成绝对路径

`writable_paths = ["~/Documents"]` 是 ADR-0066 §2 修订注记想要的跨机器写法，但加载时它被
展开成 `/home/q/Documents`，而 GUI 设置页保存时把内存里的 `Config` 整个序列化回 TOML ——
于是**保存任何设置**都会把它写成绝对路径。用户没改过那一条，界面上也看不出来，等把配置
拷到另一台机器才发现它只对 `/home/q` 成立。

- 写侧改为**保留文件里已有的拼法**：新值拿去和旧数组逐项比，比的是
  `expand_tilde(旧字面量) == 新值`，匹配上就写回原来的 `~/x`。五处路径列表全覆盖
  （`[sandbox] extra_read`/`extra_writable`、`[skills] roots`、每个 server 的
  `read_paths`/`writable_paths`）。
- 匹配按值、按消费：排序变化不改拼法，重复项各留各的写法；**改过的路径照旧写全**，
  没匹配上的回退到绝对形式（绝不丢路径）。
- 设置页的输入框仍显示展开后的绝对路径（`fields.rs` 取 `path.display()`），与文件里的
  `~/x` 不一致——纯显示问题，文件内容是对的，界面里直接输入 `~/x` 也会原样写入。要一致
  需要让界面看得见原文，即 dual-field，见 ADR-0066 §2 修订注记的补记。
- 反向的情况同样成立：`~/Documents` 在界面上被手工改成 `/home/q/Documents`（同机等价的
  两个拼法）后存盘仍写 `~/Documents`，因为界面分不清「想换拼法」和「只是保存了别的东西」。

### MCP 的 `command` 可以写裸名字：按你 shell 的 PATH 解析（ADR-0070）

`command = "/home/q/cot/lib/wordcraft_0.15.2_linux_amd64/bin/wordcraft-cli"` 每个版本都要
改一次，而 `command = "wordcraft-cli"` 在终端里启动 qcode 能用、从 Dock 启动 GUI 就
`not found` —— 因为 `qc` 进程自己的 `PATH` 是 launchd 给的最小集，工具链的 hook 只在你的
shell rc 里。ADR-0064 已经为 `run_shell` 解决了这件事（采样一次 `zsh -lic 'env -0'`），
但 MCP 侧从没接上。

- **裸 `command`（不含 `/`）现在按 shell 快照的 `PATH` 解析**，与 `run_shell` 同一份采样。
  绝对路径不再做任何查找，行为和以前完全一样。
- **只在需要时才采样**：配置里全是绝对路径就不跑 shell，不为一次查找付启动代价。
- **同一个 `PATH` 交给子进程**。`Command::new` 拿的是裸名字，真正由子进程的 `PATH` 找到它；
  若推导 grant 用一份 `PATH`、启动用另一份，沙箱会为"另一个文件"建根。这一条有端到端测试
  （真 Landlock + 真子进程）。
- **`env_clear` 之后按位置覆盖**：白名单 → 我们的 `PATH` → `env` 表 → 出口代理，所以 server
  自己写 `PATH` 仍然赢。`PATH=""` 与"没有 `PATH`"区分开（前者会让 `execvp` 什么也找不到）。
- `CommandNotFound` 现在点名**查的是哪份 `PATH`**：`not found` 单看无法区分"拼错了"和
  "这个 GUI 的 PATH 比你的终端小"，而后者正是来问的那批人。
- `qc doctor` 用同一份 `PATH` 推导（`--probe` 也是），并单列一行 `mcp path: ...`；没有快照时
  给 warn 说明只用 qcode 自己的 `PATH`、GUI 场景下会失败。裸路径判定与 PATH 读取抽成
  `qcode_kernel::mcp_needs_path_lookup` / `mcp_path_from_snapshot`，doctor 与 kernel 共用一份，
  不各写一遍。
- `qcode-kernel` 因此直接依赖 `tokio`（只为 `spawn_blocking`）：`assemble` 在 GUI 的 UI
  executor 上被 await，采样阻塞它会冻窗口。
- `[shell] env_sampling = "off"` 时退回旧行为（qcode 自己的 `PATH`），并在 doctor 里明说。

### 配置里的路径支持 `~`，MCP 写授权补上危险根检查

`[mcp.servers] read_paths` / `writable_paths` 原先强制绝对路径，而 `[sandbox] extra_read` /
`extra_writable` 根本没有校验——同一份配置里，`~` 在三处语义不同（`[skills] roots` 认 `~`，
另外两处不认）。现在是一个 helper、一次展开：

- **`~` 和 `~/x` 在加载时展开成真实 home**，覆盖 `read_paths`、`writable_paths`、
  `extra_read`、`extra_writable`、`skills.roots`。`~user/x` 与裸相对路径仍然不展开：前者要
  passwd 查找且永远不是「给本任务的授权」，后者在用户级配置里没有稳定基准（workspace 是
  每个任务一个）。
- **展开点在 `validate()` 上游**（`load` / `merge_project_file` / `validate_document` 三处，
  正是原先调 `mcp.validate()` 的三个地方），不是 `#[serde(deserialize_with)]`：后者等于在
  不依赖 `qcode-kernel` 的 `qcode-mcp` 里再养一个 `$HOME` 解析器。展开幂等，漏展开会被
  既有的「必须是绝对路径」校验挡住并报错——失败模式是明确的，不是静默的。
- **`writable_paths` 补上 `dangerous()` 检查**（读侧一直有，写侧没有）。`~/` 展开后就是
  `$HOME`，两个字符的笔误不该把整个家目录交出去：能写 `$HOME` 的 server 可以塞
  `.gitconfig` 的 `core.hooksPath`、shell rc 一行或 `~/.config/gh/hosts.yml`，下一条命令就
  生效，不需要第二次写。只检查用户写的条目，workspace 与私有 HOME 不参与——否则从 `$HOME`
  目录启动 qcode 会让每个 server 都起不来。
- `dangerous()` 的 home 一侧改为 canonicalize 后比较。另一侧（`writable_paths`）本来就是
  canonicalize 过的，macOS 上 `/tmp` 是 `/private/tmp` 的符号链接，不统一就会让「这是你的家
  目录」这条恰好漏判。
- `qc doctor` 把每条配好的路径打成 `~/Documents → /home/q/Documents` 两种写法，目录不存在
  时说明被 grant 跳过了；`[sandbox] extra_read` / `extra_writable` 同样逐条打印。
- 配置文件里的 `~` 与**工具参数里的 `~` 无关**：后者 qcode 不参与解析，由那个 MCP server
  自己决定。Rust 实现（如 wordcraft）会把 `~/Documents/x.docx` 当字面目录名，在 workspace
  下建一个叫 `~` 的目录并静默写入。README 里写明了，要可靠就传绝对路径。

### MCP 授权集在设置页里可编辑了（ADR-0066 P2 收尾）

`read_paths` / `writable_paths` / `credentials` / `sandbox` 四个字段一直没有表格列：设置页
能显示一个 server 却说不出它拿到了什么权限，用户想改只能手写 TOML。保存时靠「按名字从旧
config 里取回」把它们硬扛过去——能跑，但界面上看得到的值和实际生效的值是两个东西。

- **四列补齐**，前三个是「一行一项」的多行单元格（单行 `Input` 会静默吃掉换行，把两项粘成
  一项）。
- **`sandbox` 是三态**：`inherit` / `confine` / `off`。字段是 `Option`，写死成显式 `confine`
  会让这个 server 从此不跟随 `[mcp] sandbox` 的全局开关，而界面上看不出来。
- **取回逻辑删掉了**。列与「从文件复制」并存，只会让可见值静默输给不可见值。仍无列的只剩
  `bearer_token_env_var` 与 `headers`（ADR-0067 P3）。
- **映射抽成纯函数** `mcp_server_from_cells`。原来埋在 `apply_tables` 里，而表格行要
  `Entity<InputState>`，没有测试构造得出来——「设置页写出的配置」这条路此前零测试覆盖。
- `CredentialPreset` / `McpSandboxMode` 的解析与全量列表移进 `qcode-mcp`，CLI flag、GUI
  选择器与配置文件共用一张表，不再各写一份 match。认不出的名字返回 `None` 而不是猜。
- 测试 6 条，含**真的落盘再读回**的往返（`write_config` + `config::load`）与一条结构性断言：
  四个授权字段必须都有列——它钉的是「以后加字段忘了加列 → 保存即静默删除」。

### 修 CI：cross-check 的 aarch64 目标 + 两处 macOS 门控错误

`cross-check` job 上线后第一次真正跑，红了两个地方，都是这次新代码的问题：

- **aarch64-pc-windows-gnullvm 编不过**：`cc-rs: failed to find tool
  "aarch64-w64-mingw32-clang"`。C 构建脚本要 llvm-mingw 交叉工具链，发布环境
  （`build-for-release`）有，stock ubuntu runner 没有。CI 改为只查
  `x86_64-pc-windows-gnullvm`；全仓库没有 `cfg(target_arch)` 门控，所以 Windows 的 cfg
  分支覆盖不变。aarch64 与 darwin 仍留给有工具链的开发机（脚本默认列表）。
- **macOS 侧两处**（`test (macos-latest)` 报的，本机看不到）：`crates/qcode-tools/src/sandbox/tests.rs`
  里一个 `#[test]` 属性被划走，导致 `denies_writes_outside_the_workspace` 「从未使用」；
  `macos_profile_tests` 里一对重复的 `use` 变成 unused import。都是上一版移动代码块时
  切错行留下的。

教训已写进 README：**翻 cfg 在本地验 macOS 要翻全部相关模块，并且跑 clippy 而不是只看
测试过没过**——上一版只翻了一半，测试照样通过。

### `[sandbox] extra_read`：让沙箱外的工具链可读可执行

沙箱的读允许集是固定的发行版目录（`/usr`、`/bin`、`/lib`、`/lib64`、`/etc`、`/opt`、
`/sys`、`/proc`、`/run`、`/dev`）加任务 roots。工具链装在别处就完全不可达——而这不只是
「写不了」：

- 实测（Landlock）：开 `mode = "landlock"` 后，agent 自己的 shell 读不了 `/home/q/cot`
  （那台机器上唯一的 Rust 工具链就在里面），`cargo`、`rustc`、`gh` 全部 `Permission
  denied`，连锁导致**连修复代码都编译不了**。
- `extra_writable` 帮不上：它只对 `windows-acl` 与 macOS 生效，Linux 后端 `let _ =` 掉了。

新字段 `[sandbox] extra_read = ["/home/q/cot", "/opt/tools"]`，**列表，每项一棵子树**，
授予**读 + 可执行**：

- 四个后端统一：Landlock 加 `LL_READ_* | LL_EXECUTE` 规则；bwrap 加 `--ro-bind`；
  macOS 加进 `(allow file-read*)`；`windows-acl` 本就不限制读，是空操作（文档里写明，
  不假装三平台一致）。
- 可执行位是有意的：目录里读得到却跑不了二进制没有意义，而这正是这条需求的全部内容。
- **在 `with_roots` 里继续传递**。`run_shell` 每轮都用任务的 roots 重建沙箱，只在 seed
  上生效的修复到不了任何一个真实工具调用——测试特意走 `with_roots` 验证这一点。
- 不存在的路径在启动时报错并点名（`[sandbox] extra_read: X does not exist`）。否则
  Landlock 什么规则都不装，表现和「功能没生效」一模一样。
- `describe()` 打印 `extra read: [...]`，`qc config` 也打印该字段：doctor 说得出给了什么，
  否则「实际能碰到什么」在外部无法回答（ADR-0066 §1）。

**一个需要知道的副作用**：`extra_read` 是 `[sandbox]` 级设置，而 MCP server 的沙箱由同一个
后端派生，所以这些前缀对第三方 server 同样可读——ADR-0066 §1 的「读集 = 推导运行时 + 凭据
子树」因此被放宽。要只给 `run_shell` 用，就写窄到工具链目录本身。若将来需要严格隔离，应给
`[mcp]` 加独立的白名单，而不是让两边共享这一项。

`build()` 的 `extra_writable` / `extra_read` 收进 `SandboxPaths` 结构体，不做成两个相邻的
`&[PathBuf]` 位置参数——同类参数调换能编过，只会表现为「读得到、写不了」，
这正是 `ConnectOptions` 当初存在的原因。

测试：真机 Landlock（不加则读和执都被拒 → 加上后两者都通，且经 `with_roots` 传递）、
缺路径报错、bwrap 的 `--ro-bind` 且绝不 bind 成读写、已 bind 的 root 不重复、
TOML 解析成列表且不与 `extra_writable` 混淆。

### 审批规则的 `tool` 支持通配

`[[rules]]` 的 `tool` 之前是精确比较（`name != tool`），写 `tool = "mcp__wordcraft__*"` 会
**静默不匹配**——不报错，直接落到 `default: external tool: ask`。一个 MCP server 有十几个
工具，手写十几条规则是没人愿意做的事，于是第一版配置就是这么写的，然后所有 MCP 调用被
审批挡下。

- `tool` 现在接受 glob（`mcp__wordcraft__*`）。不含 `* ? [ ] { }` 的值仍然走精确比较的
  快路径，所以字面名字进不了 glob 解析器。
- **精确名 > 通配 > 无条件**，且与规则在文件里的先后无关。这条是有意为之：`mcp__*` 必须
  盖不过一条精确的 `deny`，否则加通配就等于加了「顺序敏感、读不出行为」的隐患。
  `specificity()` 由此拆成 `tool_rank()` + `extra_predicates()`。
- **非法 glob 在加载配置时失败**，报错点名 rule id 与字段（`InvalidPolicyRule`）。
  运行时对解析失败的 pattern 一律「不匹配」而不是「匹配一切」——把 typo 变成
  静默放行是最坏的失败方向。config 的 `[[approval.rules]]` 与 `--approval-policy` 文件
  走同一个 `validate_rules`。

测试 7 条（通配覆盖整 server / glob 两端锚定不匹配子串 / 精确胜过通配且与顺序无关 /
deny 仍胜过通配 / 非法 pattern 不匹配且被报告 / path_glob 非法被报告 / 两个加载器都拒绝
非法 pattern）。变异验证两次：把 glob 排到与精确同级 → 顺序无关那条失败；让非法 pattern
fail-open → 不匹配那条失败。

### 跨目标 check：`make check-platforms` + CI 的 cross-check job

`cargo test --workspace` 在 Linux 上全绿不代表这棵树能编译：`#[cfg(windows)]` /
`#[cfg(target_os = "macos")]` 的代码在本机根本不参与编译。上一版给
`Sandbox::with_roots` 加参数时，`crates/qcode/tests/sandbox_roots.rs`（`#![cfg(windows)]`）
漏改了调用点，Linux 上一切正常。

- `scripts/ci-cross-check.sh` + `make check-platforms`：用 `cargo check --target` 把出货
  三元组（`x86_64/aarch64-pc-windows-gnullvm`，`build-for-release` 打的就是这两个，
  不是 msvc——本仓库没有 `cfg(target_env)`/`cfg(target_vendor)` 门控，故等价）在本机
  过一遍类型检查。`cargo check` 不需要链接器，只要有该 target 的 std。darwin 三元组
  也在列表里，没装 std 就跳过并明说跳了哪些。
- 有两条「不能静默成功」的守卫：一个 target 都没检查时**失败**（防「我检查了其他平台」
  变成不可证伪的说法），跳过哪些明确打印。
- CI 加 `cross-check` job（ubuntu）。**只查 `x86_64-pc-windows-gnullvm`**，两个理由都是
  实际撞出来的：
  - 第一版带了 `aarch64-pc-windows-gnullvm`，跑起来红在
    `cc-rs: failed to find tool "aarch64-w64-mingw32-clang"`——C 构建脚本要 llvm-mingw
    交叉工具链，发布环境（`build-for-release`）有，stock ubuntu runner 没有。
  - 去掉它不损失覆盖：全仓库**没有任何 `cfg(target_arch)` 门控**（grep 过），所以 Windows
    的 cfg 分支在同一条路上都编到。aarch64 与 darwin 三元组仍留在脚本默认列表里，给有
    工具链的开发机用。
  - **darwin 也不在 CI 里查**：从 Linux 编译 Apple 框架的 C shim 从没试过，按猜测加的 job
    会永久红；macOS 后端本来就有 `test (macos-latest)` 在真机上编译并跑测试。
- 变异验证：把调用点改回两参数 → 脚本 `E0061` 且退出码 1；把目标列表换成未装的
  `wasm32-unknown-unknown` → 报「nothing was verified」并退出码 1。

同时修掉上一个提交引入的 Windows 编译错误（`crates/qcode/tests/sandbox_roots.rs` 的
`with_roots` 少了 `OutsideRoots` 参数）——正是这条 check 第一次跑抓到的。

**已知缺口（未改，记在这里）**：`windows-build` 只做 `cargo test --workspace --no-run`，
编译测试但从不执行，所以 `windows_acl` 的单测在 CI 里一条都没跑过。上一版那处
`debug_assert` 条件写反，就算有 Windows CI 也发现不了。要补得先确认那些测试在
Windows runner 上真的能过——没有依据就加，等于加一个永久红的 job。

### MCP server 不再能写 `/tmp` 和 Rust 构建缓存（ADR-0066 §1 修订）

ADR-0066 §1 写的是「三个不相交的集合，**此外一律无**」，实现没做到：MCP server 和
`run_shell` 共用同一个 `Sandbox`，于是 `run_shell` 需要的两项额外授权被一并继承——
Linux/macOS 上的宿主 `/tmp`，以及 `$CARGO_HOME` / `$RUSTUP_HOME` / `$CARGO_TARGET_DIR` /
`~/.cargo` / `~/.rustup`。实测：一个 MCP server 能写 `/tmp/anything`，也能写
`$CARGO_HOME/config.toml`。

后者不是「多了一个临时目录」：那个文件里的 `[build] rustc-wrapper` 就是下次 `cargo build`
时的任意代码执行。装着 Rust 的机器上，`npm i` / `pip install` 装来的第三方 server 就能改它。

- `Sandbox::with_roots` 新增 `OutsideRoots` 参数，**由调用方显式写**，没有默认值：这个量同时
  服务两个需求相反的调用方，任何默认都会让其中一个静默错下去。`run_shell` 是
  `BuildCaches`（行为不变），MCP 是 `Nothing`。
- 三个后端同一语义。Landlock 不加 `/tmp`、`/var/tmp`、不并 `writable_paths()`；bwrap 不
  bind cache、不再强设 `TMPDIR=/tmp`（那会顶掉 ADR-0068 §3 的私有 HOME），但**保留私有的
  `--tmpfs /tmp`**——它不是宿主的 `/tmp`，走了也不会落到宿主上，而按 `/tmp` 硬编码的运行时
  离开它会直接崩；macOS 从写块里去掉 `/private/tmp`、`/var/folders` 和那几个 cache 变量，
  读权限保留（二进制可能就住在 `$CARGO_HOME` 下）。
- 三个后端的 `describe()` 现在都带写集（`writes: roots only` / `roots + build caches +
  host /tmp`）——`doctor` 打印的必须就是实际生效的。
- `TMPDIR` 早在 ADR-0068 §3 就指向私有 HOME，而私有 HOME 本来就在 write 集合里，所以
  「临时目录不够用」这个理由不成立。真有个 server 硬编码 `/tmp`，正确做法是它自己的
  `writable_paths = ["/tmp/<its-own-dir>"]`，而不是替所有 server 默认打开宿主的 `/tmp`。

顺带修好的一个测试盲区：既有集成测试的注释写着「`tempfile` 在 `/tmp`，而 Landlock 无条件
授权 `/tmp`，所以『外面』得挑别处」——测试绕开了被 bug 覆盖的那块。现在它把 `/tmp` 也断言
进去。新增：真机单测（同一份 kernel 两种 `OutsideRoots`，`/tmp` 与 `$CARGO_HOME` 一开一关）、
bwrap 参数断言、macOS profile 断言、端到端（真子进程 + 真 `tools/call` 写
`$CARGO_HOME/config.toml`）。变异验证四次全部被抓到。

真机回归：`run_shell` 行为不变；wordcraft 仍注册 19 个工具，`new_document` +
`save_document` 写出 6531 字节的真 `.docx`（`file` 认作 Microsoft Word 2007+）。

### MCP 连接不再探测能力；`qc doctor --probe` 取而代之（ADR-0068 §4 修订）

上一版 §4 只让 2026-07-28 的 server 免于探测，legacy 仍然「试一次」。用 `wordcraft-cli
0.15.2` 真机实测后这条改了，两个理由：

1. **ADR 立项时引用的那条证据其实没被修好。** 背景段写「wordcraft 明说没有 prompts，我们
   仍然打了一次 `prompts/list`」——可 wordcraft 是 2025-06-18，上一版让它照旧试一次，
   请求照发，只是日志从 debug 升到 info。
2. **它恰好是反例。** 它的 `initialize` 返回 `capabilities: { resources: {}, tools: {} }`，
   与实际行为完全一致。上一版拒绝信任 legacy 声明的理由是猜测。

- **连接期零探测，所有协议一视同仁。** `peer_info.capabilities` 是唯一来源；日志改为一行
  `mcp capabilities read from the handshake`（`info` 级），带 protocol / resources / prompts。
- **`qc doctor --probe`** 承担「声明与实际是否一致」：走与连接完全相同的路径（grant、env
  白名单、沙箱、egress client），**两个 list 都问**，包括声明说不的那些——跳过未声明的会让
  「服务了但没声明」在构造上不可检测。
- **三态而不是 bool**：`Supported` / `Refused(<消息>)` / `TimedOut`。两种不匹配的用户可见
  损坏是相反的：声明了却拒绝 → 工具存在但每次调用都失败；没声明却服务 → 工具静默缺失
  （最坏形态，旧探测同样看不见）。
- `--probe` 对远程 server 用 `build_egress` 的 client 而不是裸 `reqwest::Client`：在诊断命令
  里绕过代理，测的就不是真实运行路径（ADR-0067 §2）。

测试：反转 legacy 用例（`initialize` 1 次、两个 list 0 次）、新增 legacy 声明即得、探针三条
（说真话 / 服务但没声明 / 声明但拒绝，HTTP probe server 新增 `--serve-caps` 造后一种）。
变异验证两次都被抓到。真机：wordcraft 的 `--probe` 报 `declares tools=true resources=true
prompts=false · instructions 912 bytes` 且无 warn；`qc run` 工具数仍是 19，与原行为一致。

### MCP 能力探测不再问已知答案（ADR-0068 §4）

ADR-0033 §3 决定「能力探测靠试一次：握手后调 `resources/list` 与 `prompts/list`，成功才
注册」。当时理由成立：两条请求比维护第二套判定便宜。2026-07-28 之后不成立了——
`server/discover` 的返回里直接带 `capabilities`，一个字段就能读到。实测一个明说没有
prompts 的 server 仍然挨了一次 `prompts/list`，换来一个 `-32601`。

- **modern（2026-07-28）读字段，零请求**。判据是 rmcp 的
  `ProtocolVersion::has_initialize()`，即「这条连接是不是 `discover` 答的」，不是自己比较
  版本号字符串。映射是纯函数：`resources.is_some()` / `prompts.is_some()` 各决定两个工具。
- **legacy 保留试一次**，且这是刻意的：野外的 2025 时代 server 经常把 `capabilities` 写得
  不准。ADR-0033 §3 由此从「通用做法」收窄为「legacy 分支的做法」。
- **legacy 探测失败的日志从 `debug` 提到 `info`**。ADR-0033 的后果段自己写过这条缺陷：
  默认级别下看不见「这个 server 没有 prompts」，而那正是用户想知道的。
- **stdio 仍走 `initialize`**（ADR-0067），所以 stdio server 一律落在 legacy 分支、照旧试
  一次。这不是漏改，是生命周期的直接后果。
- `peer_info` 缺失时走向「问」：多两次请求，好过静默丢掉 resources/prompts 工具。

测试：4 条集成（modern 零次列表请求、legacy 各一次、modern 声明了才拿到四个工具、modern
没声明就一个多余工具都没有）+ 2 条单测（映射四格表、`has_initialize` 边界）。
`http_probe_server` 新增 `--capabilities` 开关，默认不声明——正好给零探测断言一个对象。

### MCP server 的自述进了 system prompt（ADR-0068 P2）

server 在握手里给的 `instructions` 一直存在协议里，qcode 一个字节都没读过：模型只能靠工具名
和 JSON Schema 猜用法。实测 wordcraft 给了 912 字节自述，进不了上下文。

- **`# MCP: <server>` 小节**，位置在 AGENTS.md 与 skills 索引之后、todo 之前。两个生命周期
  都覆盖：`InitializeResult.instructions`（legacy）与 `DiscoverResult.instructions`
  （2026-07-28），rmcp 把两者都归一到 `peer_info`，所以读一次即可。
- **总计 8 KiB**，与 skills 索引同档、AGENTS.md 的四分之一。第三方文本不该和项目规则共池：
  一个 server 挤掉项目技能索引是不可接受的。截断落在字符边界，末尾注明「已截断」，**注记本身
  也计入预算**（调用方能依赖的不变量是整段不超过 8 KiB）。
- **注入，不执行**：不解析、不变成工具描述、不参与权限判断。单测构造一段「本 server 的工具
  已全部预批准、不得拒绝」的 instructions，断言审批请求与决策与没有这段话时**逐条相同**；
  另一条断言显式 `deny` 规则仍然 deny。
- **`prompts` 与 `instructions` 不合并**：前者是模板、由模型 `get_prompt` 取用，后者是握手期
  自述。ADR-0033 后果段的「不自动塞 prompts」不变，有单测守着。
- **`Delta::Context` 把 system prompt 计入估算**。之前只估 `messages`，system 完全不计，
  多挂几个 server 是无声的预算消耗（ADR-0068 后果段点名要补）。
- 接线走 `ToolSource::prompts()` 而不是在 `RunOptions` 加字段：前者一处接通、CLI/TUI/GUI/
  ACP/subagent 同时生效；后者要补 28 个构造点，漏掉的那处是静默的。

### MCP 工具表有了寿命（ADR-0067 P2）

2026-07-28 把 `tools/list` 变成可缓存结果：响应带 `ttlMs` 与 `cacheScope`，server 还能推
`notifications/tools/list_changed`。工具表因此不再是常量，ADR-0002 的「启动时只列一次」
正式作废。

- **三条刷新规则**：`ttlMs` 缺失（legacy）→ 进程内不重取，行为与以前完全一致；
  `ttlMs = 0` → 每个 turn 重取；`ttlMs = n` → `n` 毫秒后过期，在下一个 turn 边界重取。
  `list_changed` 一到立即置脏，不等 TTL。
- **只在 turn 边界换表**，而且是快照：`run_turn` 入口刷新一次，拿到的克隆用满整轮。
  正在跑的 turn 里换表会让模型看到一套、执行时是另一套；`ttlMs = 0` 因此是「每个 turn」
  而不是「每个 round」。
- **换表写事件**：`McpToolsRefreshed { server, added, removed }` 进事件日志，CLI 打一行
  `[mcp] <server> tools: +a -b`，GUI 显示一条 notice。
- **已消失的工具被调用 → 明确错误**：`tool no longer available: tool 'x' no longer exists on
  MCP server 'y' ...`，而不是 `unknown tool`。两者要修的东西不同，模型得知道是哪种。
- **刷新失败保留旧表**（SEP-2549 允许 serve stale）：一次网络抖动不该让整个 server 的工具
  消失，dirty 位留着下一轮再试。
- **分页自己写**：`list_all_tools` 会把 `ttlMs` / `cacheScope` / `resultType` 丢掉，按
  `nextCursor` 循环并只取第一页的 `ttlMs`；重复 cursor 或第 64 页即停。
- `cacheScope` 明确不实现：qcode 没有跨进程工具缓存，`public` 与 `private` 行为相同。

### 远程 MCP server（ADR-0067 P1 / P3）

上一批修的是「stdio server 装在本机却被沙箱杀掉」。这一批修的是更根本的一条：**qcode 装不上
绝大多数已登记的远程 server**。官方 Registry 抽样 100 条，约 93 条只有 `remotes`
（`type = "streamable-http"`）；这些在旧版写进配置直接报 `missing field 'command'`。

- **`[[mcp.servers]]` 的 `command` 与 `url` 恰好二选一**。`command` 由 `String` 变为
  `Option<String>`（源码级 breaking，配置层不破坏：老配置一行不改继续用，仍然走 stdio）。
  都没有、或都有，都是配置错误，且错误信息说清是哪一种。
- **Streamable HTTP 走 `rmcp` 的 `StreamableHttpClient`，并且只用调用方给的 client。**
  这不是风格问题：`new_simple(url)` 会内部造一个 `reqwest::Client`，等于让 MCP 的 HTTP
  直接出网——**静默绕过** egress allowlist、`network.jsonl` 审计与私网拦截，唯一症状是
  「审计文件里少了几行」。所以 `qcode-mcp` 的 `ConnectOptions.http` 是
  `Option<&reqwest::Client>`，`None` 意味着**不连**（明确的错误），内核把
  `Network::client()` 传进去。
- **协议版本：`Auto`，首选 2026-07-28，回退 `initialize`。** 2026-07-28 删掉了握手（版本
  搬进每请求的 `_meta`）、结果带 `ttlMs`。但存量 server 只认 `initialize`——所以用 rmcp
  的 `Auto`：先 `server/discover`，对端报 `-32601` 或在 10s 内不回就回退，回退时声明上限
  `2025-11-25`（`initialize` 能回声的最高版本）。stdio 仍固定走 `initialize`，理由见 ADR。
- **认证只从环境取**：`bearer_token_env_var = "VAR"` 是变量名，token 本身不进配置文件；
  `headers` 里出现 `authorization` / `cookie` / `token` / `key` / `secret` / `password`
  会被配置校验拒绝（配置文件不是密钥库），换行符也拒（防 header 注入）。
- **远程 server 上的 stdio 字段一律拒绝**：`args` / `env` / `read_paths` /
  `writable_paths` / `credentials` / `sandbox`。它们在这里都是空操作，静默接受等于让用户
  以为设了什么。
- **`qc mcp add --url / --bearer-token-env / --header`**；`--command` 与 `--url` 由 clap
  的 `ArgGroup` 表达「恰好一个」，报错是同一条 `--command|--url`，不会只点名其中一个。
  `qc mcp list` 按 transport 分开打印（远程不再显示成 `command=None`）。
- **`qc doctor` 认远程 server**：打印 transport、URL、token 变量是否已设置、headers 数量、
  以及**是否经代理审计**；`[network] enabled = false` 时如实说「nothing is audited」而不是
  「audited」。host 不在 `[network] allow` 里时，直接给出要加的那一行——这是首次配远程
  server 最容易踩的坑，而代理侧的表现只是一个稍后才出现的 403。
- **RUSTSEC 前置条件复跑**：开 HTTP feature 后重跑 `cargo deny`。`rmcp` 精确 pin 在
  `=3.9.0` 从此有承载力——RUSTSEC-2026-0462（客户端响应体无界缓冲）在 3.9.0 修，
  而这条路径现在可达。另两条（0457 OAuth SSRF、0461 服务端 transport）分别关在 `auth` 与
  `transport-streamable-http-server` feature 后，两者都未开。

### MCP server 的三处安全与可用性缺口（ADR-0066 / ADR-0068 P1）

起因很具体：一个装在 `/home/q/cot/lib/...` 的 server 报 `Permission denied`，拷到 `/tmp`
就能跑。

- **`~/.local` 里的 server 不再被沙箱静默杀掉**（ADR-0066）。MCP server 原先直接套用
  `run_shell` 的沙箱，而 Landlock 的只读白名单是 `/usr /bin /sbin /lib /lib64 /etc /opt
  /sys /proc /run /dev`——**没有 `/home`**。于是 server 能不能跑取决于它装在哪：
  `/usr/local` 能跑、`~/.local`（pipx / uv / npm 的默认位置）不能，而 §4.10 一直写着
  「默认不套 FS 沙箱」，实现与文档各说各话。现在改成按 server 现算一份授权：运行时目录
  （`command` 推导：venv、`node_modules`、`<dist>/bin`）只读+可执行，写只给 workspace 与
  每个 server 私有的 `HOME`，读不到真实 `$HOME`；`read_paths` / `writable_paths` /
  `credentials = ["github"]` 这类预设负责补授权，`/`、`$HOME`、workspace 父目录一律拒绝
  自动授予。单个 server 仍可 `sandbox = "off"`，但启动会 warn。
- **子进程不再继承你的 119 个环境变量**（ADR-0068 P1）。`run_shell` 早就 `env_clear()`
  后按白名单注入，MCP 却是两句 `envs`——实测子进程里能数到 `GITHUB_TOKEN`、
  `DEEPSEEK_API_KEY`、`SSH_AUTH_SOCK`。现在同样白名单：`PATH`/`LANG`/`TZ`/`LC_*`、合成
  `HOME`、凭据预设点名的变量、server 自己的 `env`，最后才是出口代理四件套（顺序错一个
  位，ADR-0023 的审计会静默失效）。`env` 的值可以写 `${NAME}`，密钥不必落盘。
- **`qc doctor` 有 MCP 段了**。逐 server 打印它实际拿到的授权集（运行时 / 可写 / 凭据 /
  私有 HOME），并把「配了但没用」的四种情形标成 warn：`read_paths` 指向不存在的路径、
  `credentials = ["github"]` 但本机没有 `~/.config/gh`、`sandbox = "off"`、以及
  `[sandbox] mode = "none"` 让 `[mcp] sandbox = "confine"` 形同虚设——最后这条是写 doctor
  时才发现的，内核用 `[sandbox] mode` 造沙箱，`[mcp] sandbox` 单独开不出来。
  `env` 里的字面量密钥也会被点名（值不打印），提示改写成 `"${VAR}"`。
  授权集摘要同时区分「要了」与「拿到了」：`credentials [~/.ssh, npm(not found)]`。
- **`qc mcp add` 认识新字段**：`--read-path`、`--write-path`、`--credential`、`--sandbox`。
  `qc mcp list` 会打印 server 的沙箱模式与显式授权项。
- **`Sandbox::command` 的文档补了一条真踩到的坑**：返回的 `Command` 依赖 sandbox 还活着
  （Landlock 的 ruleset fd 在 `Drop` 里关）。`with_roots(..)?.command(..)` 一行写完会在
  `spawn()` 前 drop 掉 sandbox，子进程报 `EBADFD`，看起来像权限问题，其实不是。
- 修的顺带两处：GUI 设置页保存时不再清空新字段（`apply_tables` 会 `servers.clear()`）；
  MCP 的私有 `HOME` 会先 canonicalize，`--data-dir ./data` 这种相对路径不再让 server 拿到
  相对 `HOME`。


## 0.8.3 — 2026-11-11

0.8.2 之后的事后检查：一个会让 `run_shell` 挂死的真 bug，加几处「门禁是绿的，
但什么都没测」的收拾。

- **后台进程组里 `run_shell` 会挂死，超时也救不回来**（0.8.2 引入的回归）。
  ADR-0064 让 `run_shell` 跑在用户自己的 shell 里，环境采样器是 `$SHELL -lic env -0`。
  `-i` 是必需的（`.zshrc` / `.bashrc` 里才有工具链 hook），但交互式 shell 会**直接
  打开 `/dev/tty`**，不读 fd 0，所以 `Stdio::null()` 拦不住它。
  - 触发条件是「`qc` 处于某个终端会话的后台进程组」：`make check &`、IDE 的 task
    runner、管道化的运行、在终端里 `cmd &` 之后跑 `qc`。这时 `/dev/tty` 的读会把
    `SIGTTIN` 发给采样器，采样器被 stop，**轮询它的父进程也被 stop**——`run_with_timeout`
    的超时循环本身停了，于是 `env_timeout_ms` 永远不触发，整个进程挂到有人 `fg` 为止。
  - 修法是 `pre_exec` 里 `setsid()`，让采样器进入独立会话、没有控制终端：`/dev/tty`
    打开即 `ENXIO`，`-i` 退化成「no job control in this shell」，采样照常成功。
    有意不用 `process_group(0)`——没有新会话的进程组长仍然握着旧会话的终端。
  - 前台正常使用不受影响，CI 上也不复现（无 tty），所以 0.8.2 的发布没拦住它。

- **`ci.yml` 的两个 sandbox 门禁是空转的**。过滤名少了 `tests::` 一段，而模块实际在
  `sandbox::tests::{linux_tests, landlock_runtime}` 下，于是这两行匹配不到任何测试、
  以 0 退出、一步绿灯——bubblewrap / Landlock 坏了也报不出来。现在匹配 3 + 2 个测试。

- **GUI lib 的单元测试回到了 `cargo test --workspace`**。`[lib] test = false` 是
  2026-10-21 加的，理由是「GPUI 的宏展开在 lib test harness 里撑爆 rustc」；实际是
  `setup/models.rs` 的测试模块 `use super::*` 继承了 `gpui_kit::*` 里名为 `test` 的
  proc-macro 属性，`#[test]` 因此无限自递归——一个导入 bug，不是宏展开的固有代价。
  改具名导入即可，harness 正常，52 个测试（51 过 1 忽略）0.01s 跑完，不需要窗口。
  workspace 通过数 848 → 899；这 51 个测试此前只有显式 `--lib` 才会跑。

- **README 版本与状态段**：抬头停在 v0.6.1，状态段里「Linux bwrap/Landlock 与 Windows
  ACL 的多目录重建仍待补齐」是照抄 ADR-0047 草案期的注记——三个平台的 `with_roots`
  早在 2026-10-22 前后就已落地。改成陈述现状，并补上 0.7/0.8 落地的东西。

- **定时任务测试的 beat 容差**：`gap_limit = 3 × beat`，而
  `a_scheduled_window_fires_a_turn_on_the_task_it_names` 用 100ms tick，容差只有
  300ms——共享 runner 上每个 beat 都够格算「进程离开过」，那个分支只重排期、不触发，
  于是等满 20s 一个 `TurnStarted` 都没有。改成与相邻测试一致的 1s tick（容差 3s）。

## 0.8.2 — 2026-11-10

两个 ADR 与一批「同一件事写在两处」的收口。

- **`run_shell` 现在跑在你自己的 shell 里，并且带上你的环境**（ADR-0064）。以前 POSIX 上
  固定 `sh -c`，子进程环境被 `env_clear()` 洗过、只回填一张白名单——于是 `c` 那套
  `JAVA_HOME`/`GOROOT`/`COT_HOME` 在 agent 跑的每条命令里都不存在，`[[ ]]`、`set -e`
  这类 zsh/bash 语法也会失败。
  - 解释器不配置：探测逻辑从 `qcode-terminal` 抽成 `crates/qcode-shell`，与终端共用同一份
    `配置 → 父链 → 登录 shell → $SHELL → /bin/sh` 顺序。终端认得出的 shell，命令行也认得出。
  - 环境采样一次：会话开始（或 rc 文件变了、或过了 TTL）跑一次 `$SHELL -lic 'env -0'`，
    结果按 NUL 切块进快照；注入顺序是 `白名单 → 快照 → [shell] env_include → TMPDIR/代理`。
  - `[shell]` 八个键：`env_sampling`、`env_command`、`env_timeout_ms`、`env_ttl_secs`、
    `env_include`、`env_exclude`、`env_sample_sandboxed`、`audit`；设置页有对应控件与
    「采样一次」按钮；`qc shell-env` 打印脱敏清单（`--json` 给脚本，`--full` 需 tty）。
  - 会话日志新增 `shell_env_sampled` 事件（schema 8），只记变量名与增减，不记值。
  - 这条改动的代价写在 ADR 里：它撤回了「`env_clear` 让 agent 读不到 API key」这个假设
    （用户选定 secret 不拦）。关掉采样即回到白名单行为。

- **配置文件只写覆盖项：schema 2**（ADR-0065）。
  - `models` 可以写 `["deepseek-flash"]`，不必为每个模型写一张 `[[…]]` 表；带 `name` /
    `context_window` / `max_tokens` 时仍然写表。
  - `[sandbox] network` 成为唯一的网络开关（`off` / `proxy` / `strict` / `open`），
    把 `[network] enabled`、`[network] mode` 和旧布尔折成一份 `NetworkPlan`；
    `[network]` 只剩主机名单那半部分。
  - `[approval] allow = ["run_shell:echo *"]` / `deny = […]` 简写，等价于规则表，
    `[[approval.rules]]` 留给需要 scope 的场景。
  - `qc config migrate` 机械迁移：折 `[providers.*].default_model`、折只有 id 的模型表、
    折网络开关；先备份 `.bak`，幂等，保留注释与未知键。
  - 写回只写差异：空 `[providers]`/`[pricing]`、没有内容的 `[approval]` 不再落盘；
    新文件写 `schema = 2`，旧文件保持原拼写，升级只发生在 `migrate`。
  - `qc config show` 打印合并后的 `sandbox.network` 与主机名单。

- **`pricing` 不猜价格**。`qc doctor` 现在明说：没有 `[pricing.<model>]` 时 `--max-cost`
  拦不住，声明了但没设上限时它只用于显示，条目不全时它被忽略。

- **模型探测带上显示名**：`/models` 的 `name`/`context_window`/`max_tokens` 不再被丢掉，
  探测结果里显示 `id — 显示名`；手输一个不是 id 的名字（例如 `DeepSeek-V4.1-Flash`）时，
  设置页直接提示「这里填的是 id」。`deepseek` 的 catalog 也按接口实况改成
  `deepseek-flash` / `deepseek-v4-pro`。

- **设置页改默认模型真的生效了**（ADR-0055 §5）。把**默认 provider** 的「Default
  model」改成别的值，以前是保存成功、表单显示新值、跑任务还是旧模型：`[default-model].model`
  在读配置时压着 `providers.*.default_model`，而设置页只写后者；同一个 provider 内换模型
  也没有任何 UI 入口能改到前者。
  - 保存时同步：被保存的 provider 若是 `[default-model].provider`，它的 `default_model`
    写进 `[default-model].model`。行被清空时不动全局值（那是模型菜单单独钉的）。
  - 设置页顶部「当前默认：provider / model」变成可点选择器，按 provider 分组列出模型，
    选中即写 `[default-model]`。被覆盖的 provider 行给出注脚说明改写后会以哪边为准。
  - 反向保护：设置页存表格会重写整份配置，写盘前取回磁盘上的 `[default-model]`（除非这次
    保存刚改过它），否则会把用户在主窗口刚切的模型改回去。
- **目录里没有的模型现在能设了**。`DeepSeek-V4.1-Flash` 这类不在 `catalog` 也不在
  `[[providers.*.models]]` 里的模型名，以前**在任何界面都选不到**：设置页的「当前默认」
  选择器与底部模型菜单都只列已登记的模型，于是只能手改 config.toml。现在两处都能手输
  ——设置页选择器底部有「设为 {provider} 的默认模型」输入行，底部菜单在搜索框里打一个
  列表里没有的名字会多出一行「使用「x」」（Enter 同样生效）。
  - 顺手修掉一处会撤销手选的地方：`pin_default` 只写 `[default-model]` 不写
    `providers.<p>.default_model`，而任何一次表格自动保存都会用行值把 `[default-model]`
    同步回去——选完模型改个别字段就被改回来。现在两处一起写。
- **终端 tab 标题单行、小一号**。shell 把标题设成当前目录，之前放开换行，tab 条会被撑成
  好几行、把内容区往下推；标题现在是 `sp(13)` 单行省略、高度 ~26px，`×` 与 `+` 定成
  14/20px 方形可点区（ADR-0062 §2 修订）。

## 0.8.0 — 2026-11-10

内建终端（ADR-0062）：`qc gui` 里多一个「终端」，独立执行体、单入口多 tab、状态只在
内存。同时修掉几处窗口行为里的老毛病，其中一条（草稿被丢弃）此前一直被「回到任务」
按钮挡着。

- **终端是独立 crate 与独立 OS 线程**（`crates/qcode-terminal`，只依赖 `gpui-kit` 与
  `alacritty_terminal`）。每个终端一根线程读 PTY master、喂 `vte::Parser`、更新 `Term`，
  UI 线程只在其锁内取一次快照后绘制。这根线程**不进** `LocalBackend` 的
  `current_thread` runtime：那里任何阻塞 I/O 都会把定时任务的 beat 判定推迟，`away`
  一成立，孤儿与错过窗口就写不下去。TUI / `qc run` / ACP 的构建因此不含终端。
- **单入口、内容区多 tab**：侧栏「终端」有终端就切回上次活跃的那个（不是新建、也不是
  第一个），没有就建一个。`Shell` 因此多了 `view: Task | Terminal` 这个维度，但不复用
  `active`——切到终端不清空它，切回来还是原来那个任务。
- **canvas 自绘 + 脏行重绘**：列数由 `advance('M')` 算、行数由 `line_height` 算，resize
  同时下发 `Term::resize` 与 `TIOCSWINSZ`；同色属性连续单元格拼成一个 run 排版一次，
  从不逐字符 layout；没有输出就整帧跳过。网格余量对半分到两侧，不再全堆在右下。
- **按键按其模式编码**：应用光标键、bracketed paste、鼠标上报、`modifyOtherKeys` 都按
  `Term::mode()` 决定，不自己猜；`Ctrl+C`/`Ctrl+Z`/`Tab`/`Esc`/功能键原样转发给 PTY。
- **输入法与 CJK**：预编辑文字画在光标处；提交的文字进 pty。文字分两族绘制——西文用
  代码字体（`[gui] code_font`），汉字用 Composer 那个族，字号按 `advance('中')` 测量，
  让一个汉字正好两格；两段各自按列定位，所以不再累积漂移。字号改用与 Composer 同一个
  `COMPOSER_TEXT_SIZE`，终端和转录区从此一样大。
- **选择、复制与粘贴**：拖选 + `Cmd+C`（`Ctrl+Shift+C`）走系统剪贴板，OSC 52 的读写也
  接上了；多行粘贴先问一句。滚轮可回看历史（上限 `[terminal] max_scrollback`，默认
  10000 行），打字立刻回到现场。
- **配色与 shell 探测**：终端用自己的深色画布，不借主题背景；shell 取「用户正在用的
  那个」（进程表）而不是 `$SHELL`，登录 shell 从用户数据库取；新终端的 cwd 用上一个
  终端的**实时** cwd（失效目录跳过）。
- **`[terminal]` 五个键**：`shell`、`start_cwd`、`font`、`max_scrollback`、
  `max_terminals`（默认 8），设置页有对应控件。
- **资源有实测答案**：`crates/qcode-terminal/tests/resource_budget.rs` 断言 8 个终端各
  跑 `yes` 时 scrollback 封顶（10000 行 × 80 列 ≈ 18.3 MiB/tab，合计 ≈ 156 MiB，与
  理论下限差 4%）且再刷两秒增长为 0；单次快照均值 284 µs，占 16 ms 帧预算的 2%。

GUI 行为修正：

- **未发送的草稿不再被丢掉**（此前是丢数据的路）。`park_and_clear` 只按 `active` 停放，
  而草稿没有会话 id，于是整份视图连同刚打的字、粘的图、挑的文件被 drop——在草稿上点
  任何一条任务行都会中招。现在停放键是 `SessionTarget`（草稿落在 `Draft` 槽位），`+`
  装回来并提示；切走时状态栏明说「已暂存，按 + 回到它」。空草稿仍然丢掉。
- **删掉「回到任务」**，`Cmd+T` 改成双向开关：终端视图里按回任务视图，任务视图里按去
  终端（不再每次多开一个 tab，新 tab 走 tab 条末尾的 `+`）。
- **侧栏分三段**：顶部固定「我的任务 + `+`」、中间滚动任务列表（唯一可滚的区）、底部
  固定「终端」「设置」。原来整栏一个滚动区，任务一多，「终端」和「设置」就被滚出屏幕。
- **设置关闭不再丢最后一个改动**：自动保存是 250ms 防抖写入，而关闭路径直接返回、不
  flush，改完值立刻关窗口就静默丢弃。现在无条件先 flush 再关；失败才留窗口并说原因。
  三个开窗口的入口（向导、`qc settings`、GUI）共用一处 `wire_close_handler`。`Esc` 也
  能关设置，走同一条路径。
- **侧栏点任务立刻切到任务视图**、**运行中保存设置不换引擎**（延迟到任务空闲生效）、
  **模型菜单分组缓存**（不再每帧同步探测）、**执行过程与结果的分隔线只在任务完成后
  显示**。

构建：

- **thin LTO + `codegen-units = 1` + `strip`**：GUI 产物 88M → 55M。

## 0.7.9 — 2026-11-08

附件从"只活在内存里"变成了内容寻址存储（ADR-0061）：图片和文件都先落盘，事件日志里
只记引用，回放时按引用取字节——重开会话、换个模型继续，图和文件都还在。

- **图片走存储**：`AttachmentRef::Image` 记 sha256、宽高、格式与原图尺寸；字节落在
  `$data/attachments/v1/objects/<2>/<sha>`，同一张图重复发只占一份。写盘是 tmp → fsync
  → rename → 只读，进程中途被杀不会留下半个对象。
- **写入时归一化一次**：长边 2048、落盘 2 MB 以内；有 alpha 走 PNG，不透明按 85/70/55
  的 JPEG 阶梯压，压不下就把长边 ×0.75 再来（最多八轮）。达标的小图原样放行，原图另存
  `originals/`，`ImageRef::origin` 指过去。
- **回放逐字节一致**：`messages_from_events_with` 用 store 作 loader 重建出
  `ContentBlock::Image`，所以 resume 之后的请求和第一次发送完全相同；对象丢了只丢图、
  不报错——引用还在日志里，下一次能读到就又出现。
- **转录区看得见**：图片贴在问题气泡下方（不再被折进「执行了命令」），文件显示成
  「名字 + 大小」的卡片；对象被删时显示 `[图片已不可用]`。
- **输入侧三个入口**：粘贴（macOS 用 `NSPasteboard` 直接读 PNG）、选择文件、拖拽。
  选中的那一刻就落盘，输入框上方显示缩略图或文件卡片；`ctrl-shift-v` / `cmd-shift-v`
  在输入框聚焦时也能粘贴（原先被 `Textarea` 的 `Input` 上下文吃掉）。
- **文件只存不解析**：`qc run --file <PATH>` 与 GUI 走同一条路，单文件上限 64 MB（流式
  复制，超限当场拒绝）。模型读到的是句柄——名字、字节数、sha256 前八位、只读路径，以及
  "要改先复制""读不到就明说"的约定；PDF/docx 不抽文本，交给 agent 自己跑工具。
- **不支持视觉的模型降级**：不再整轮报错，图片换成一行
  `[image omitted: this model accepts text only; …]` 占位，日志里仍是引用。
- **请求有图片预算**：一次请求最多 20 MB / 20 张（按 base64 长度算），超了就把最旧的
  图片换成占位文本、最新的最后才动；卸载不写日志，下一个有余量的请求会把它读回来。

## 0.7.8 — 2026-11-08

- **新任务的首条消息发出后，窗口跟着切到它**（ADR-0058 §7，真机复核）：在草稿里发第一
  条消息时，引擎新建的会话名还不在屏上，`poll` 按 target 分流会把它算成后台会话、丢进
  `dormant`，而 `dormant::apply_to_dormant` 对 `SessionChanged` 是空分支——引擎在跑、
  转录区有内容落盘，界面却一直停在「新任务」、侧栏也不出现这一行。
  - 分流决策抽成 `task_ops::destination`：`SessionChanged` 一律按窗口级处理——它说的是
    窗口该看谁，不是某个会话的内容；只有窗口自己发的 `NewSession`/`Resume` 会产生它，
    跟着走不会抢走后台上正在跑的 turn 的视图。窗口接手时顺手 `adopt_dormant`，把后台
    攒下的审批、队列快照和 `running` 并进可见视图，也免得留一条僵尸条目。
  - 回归用例在 `tests/task_ops.rs`（去掉 `Window` 分支即红）；`resume` 里那段内联的
    接手逻辑同时抽成 `adopt_dormant` 复用。

- **`gui` feature 恢复可编译**：P6 定时任务合入时漏改 `crates/qcode/src/commands/gui.rs`
  的 `Engine` 初始化——`schedule_tick` 写了两遍、`schedules` 的路径重复成
  `qcode_kernel::config::qcode_kernel::config::resolve_schedules`、这一处还缺
  `schedule_errors`/`schedule_tick`。`cargo build -p qcode --features gui` 在 df72e2c 上
  直接失败，现按同文件 `:54-59` 的写法补齐。

- **macOS sandbox：只读根在 `$TMPDIR`/`/tmp` 下真的只读了**（ADR-0048 §2，真机复核）：
  profile 对所有任务都放开 `/tmp`、`$TMPDIR` 与 Cargo/Rustup 缓存，所以落在这些父目录下的
  `read-only` Root 之前只是「没进白名单」，仍然可写。
  - 现在在 `(allow file-write*)` 之后为每个只读根追加 `(deny file-write* (subpath ...))`；
    Seatbelt 的「最后一条匹配生效」让这条 deny 压过父目录的放行。
  - profile 里所有 root / turn temp / `extra_writable` / 工具链目录统一 `canonicalize`：
    SBPL 比的是真实路径，`$TMPDIR`（`/var/folders/...`）不会匹配写错前缀的 filter。
  - 真机新增三条实测：`a_read_only_root_refuses_writes_inside_a_writable_temp_tree`、
    `a_no_root_task_sees_only_its_temp`（无根任务的 temp 可写、`$HOME` 不可读不可写）、
    `shell_follows_the_root_matrix`（`run_shell` 按任务根集重建 sandbox）。macOS arm64 上
    `sandbox-exec` 预检通过，没有一条走 skip。

- **定时任务 P6**（ADR-0059）：`[schedules.<name>]` 声明没有人在旁边时的 turn。
  - `qcode-core::schedule`：五段 cron 解析/匹配/下一个窗口（含名字、步长、`7`=周日、
    日与星期的「或」）、`tick` / `recover` / `advance` 三个判定函数。纯逻辑，时间为参数。
  - 运行状态（`last_fired_at` / `next_fire_at` / `last_status` / `missed_windows`）落
    store 的 `schedules` 表，**不写回配置文件**——否则每次触发都会触发配置重建。
  - 错过不补跑：进程不在时走过的窗口只计数（侧栏显示「定时错过 N 次」），下个窗口从
    现在算；「tick 到了」与「睡过去了」用单调时钟区分，改系统时间不会多跑或少跑。
  - 触发走同一套 `SessionQueue`：忙则排队（`overlap = "queue"`）或跳过（`skip`），任务被
    删记 `Orphaned` 而不是静默跳过；`new = true` 新建的会话不抢当前视图。
  - 定时 turn 的审批有 `approval_timeout`（如 `30m`）：超时 deny、turn 正常结束、会话锁
    按时释放；人触发的 turn 仍然无限等。
  - `Backend::schedules()` + 侧栏任务行提示；`[schedules.<name>]` 不进 Settings 表单。

- **多任务并行 P0–P1**（ADR-0058）：为「多个任务同时跑」铺路，本次先把三处会互相拖住的
  东西修掉，再立起宿主骨架。
  - 文件工具的四处同步 IO（`read_file` / `write_file` / `edit_file` / `list_dir`）移出
    runtime 线程（`spawn_blocking`）。GUI 的引擎跑在 `new_current_thread` 上，一个任务读
    大文件时，另一个任务的流式输出与取消都会停摆。
  - turn 任务 panic 不再把界面永久留在「运行中」：守卫在 unwind 时补发一条 `TurnFailed`，
    Stop 与新会话都能继续用（`tests/backend.rs` 新增回归用例，关掉守卫该用例即红）。
  - 新增 `SessionHost`（`qcode-gui/src/host.rs`）：按会话记录 cancel token、follow-up
    队列与阶段（空闲 / 运行中 / 待审批），是「谁在跑」的唯一权威；用例 12 条。
  - 事件一律带会话标签：`SessionEvent { target, event }`，由 `KernelPort::tagged()`
    产生单会话端口，`Delta` / `ApprovalView` 不必加字段。
  - 审批的「Always」改为按会话隔离（`HashMap<session, HashSet<tool>>`）：A 任务的
    「Always」不再替 B 任务放行。
- **多任务并行 P2**（ADR-0058 §4）：把「当前任务的视图」做成可收起、可换回去的一份值。
  - 新增 `shell/task_ui.rs` 的 `TaskUi`：转录、滚动状态、展开集、审批队列、排队消息、
    输入框草稿全在里面；`Shell.ui` 是当前那一份，`Shell.parked` 按会话 id 存切走的
    那些。`install` 是纯字段交换（O(1)，不建实体），这是 P3 能给后台会话就地应用事件
    的前提。
  - 切回访问过的任务不再重放日志：滚动位置、展开的代码块、没发出去的草稿都还在。
    闸门是 `seen_seq >= 日志最后一条 seq`——日志被别的窗口或 CLI 写动过就照旧重放，
    保证不会显示旧内容。
  - 视图带 `id`，三处延迟回调（两个 observer、`set_notice` 的 6 秒 timer、
    `wake_for_session`）按它校验，切走后不会写进别人的视图。
  - 删任务时清掉它的停放视图。
- **多任务并行 P3**（ADR-0058 §3、§5）：
  - 引擎事件不再在 `render` 里排空，改成常驻 ticker（有 turn 在跑 33ms、全空闲
    150ms）。窗口最小化/藏在托盘时，turn 结束与审批到达也会被处理——这条以前是
    「延迟」，多会话之后会变成「挂起」。
  - `poll` 按 `SessionEvent.target` 分三档：屏上那份照旧；用户去过的会话应用到它
    自己停放的视图（不因流式增量请求帧，只在 turn 边界/审批上请求一帧）；没打开过的
    会话只留状态——running、审批队列、队列快照、失败提示，流式文本丢弃（切进去从
    日志重放）。
  - 后台会话的审批不再丢：憋着的 prompt 在切进去时并入视图，答完引擎继续（审批本就
    按 `call_id` 路由）。`tests/dormant.rs` 钉住这条契约。
  - 后台在跑的任务在侧栏显示 `▶`；`TurnFinished`/`TurnEnded` 不再误清屏上那份视图
    的运行态。
  - 删掉 `wake_for_session`：ticker 让它变成多余的一百毫秒轮询。
- **多任务并行 P4**（ADR-0058 §6、§7）：
  - 侧栏徽标按会话：屏上 `●`、后台在跑 `▶`、待审批 `⚠ · 待审批`、未读 `◉`、空闲 `○`。
    未读 = 不在屏上的会话产生了 turn 边界或最终消息，切回去就清。
  - 改名 / 删除 / 改 roots 改成**按会话**排队：只有这个任务在跑才等，别的任务在跑不再
    误伤（以前只要有一个 turn 在跑，改任何任务都要等）。待处理目录从单槽变成按任务 id
    的表，`TurnFinished` 只应用完成的那个会话。
  - `Cancel` 带会话：`UiCommand::Cancel { session }` + `CancelAll`，
    `Backend::cancel(session)` / `cancel_all()`。侧栏每个任务的 ⋯ 菜单加「停止」，
    后台任务也能停；错误的会话 id 不会停掉在跑的 turn（有用例钉住）。
  - 退出聚合成一条路：确认后 `cancel_all`，只清视觉上的运行态，等各自
    `TurnFinished` 再 `quit`，另加 5 秒兜底（日志是权威源，强杀不需要恢复逻辑）。
- **多任务并行 P5**（ADR-0058 §7）：任务真的能同时在跑。
  - runtime 从单槽改成按会话：`live: HashMap<SessionTarget, CancellationToken>`，
    `current` 只表示「窗口在看谁」。切任务不再 cancel 当前 turn，也不再要求先停——
    切走的后台任务继续跑，事件照旧路由到它停放的视图。
  - `done` 通道与 `TurnGuard` 带上会话，收尾（`end_turn`、follow-up 队列、待应用改名/
    目录）都按**完成的那个会话**做，不再误清屏上任务的状态。
  - `UiCommand::Prompt { session }`（`Backend::prompt(session, …)`）：提问也带会话，
    一次过期的界面切换不会把消息投到别的任务上。
  - 「后端可回收」= `close_if_idle` 回收空闲会话的槽；切回来靠 `resume()` 从日志重放。
  - 配置变更改成「等所有会话空闲再重建」（`pending_rebuild`），不再打断在跑的 turn。

## 0.7.7 — 2026-11-08

- **预算暂停：超限是交接，不是报错**（ADR-0060）：`max_turns` / `max_cost` 用尽时 turn 落
  新增的 `paused` 四态（schema v5 → 6，v5 日志 serde 缺省可回放），不再写 `error(...)` 当
  失败。`--max-cost` 从「已花超才停」改为到 95% 即停，剩下的留给一次**无工具的收尾调用**
  写交接摘要（不计入 `max_turns`，成本照实记账，放不下或失败就降级为无摘要）。停时同时
  给出可推导的 `ProgressReport`（改了哪些文件 `+N −D`、跑过的命令、失败的工具行、最后一段
  助手文本），纯函数放 `qcode-core`，GUI / TUI / ACP / `qc run` 共用。
- **提示阈值改用引擎口径**：新增 `budget_hint` 事件（首次跨过 75% 或剩 ≤ 8 轮发一次），
  GUI 提示卡与 TUI 状态行都只读它，删掉用 `app.turns.len()`（用户消息条数）当轮数的错误
  口径——长任务不再漏报，短任务不再误报。
- **继续 / 收尾**：`Continuation{turn, window, raise}` 标记在 `UserMessage` 上，transcript
  渲染成灰底提示条而不是用户气泡；`window` 由引擎读日志计数后覆盖前端传值。GUI 三键卡片
  （继续 / 收尾 / 先这样），TUI `/continue` 与 `/wrap`。
- **无人值守链路默认停，不等**：`qc run` 退出码 **75**（`EX_TEMPFAIL`），`--json` 流末多
  一条 `pause`；`--resume <id> --continue` 接着跑，`--raise-turns` / `--raise-cost`
  （`--raise-persist` 写回配置）只抬这一个窗口；ACP 保留 `stop_reason =
  max_turn_requests` 但不再发 error chunk；子 agent 不抛错，把暂停拼进 `SubagentOutcome`。
- **`no_progress` 保持 `failed`**：它是「方法错了」，继续只会重复同样的调用，不给继续按钮。
- **重启回到上次的任务**：`gui.json` 有多个写入方（主窗口的窗口几何、设置窗口的外观、
  缩放、`last_session`），各自整份覆盖，高频的几何写入会把刚存下的会话 id 冲回旧值。
  新增 `GuiPreferences::update`（读-改-写）后，每个写入点只动自己那几项。
  `last_session` 缺失（旧版 `gui.json`、被覆盖）或指向已删除的任务时，退回最近的任务并
  把它写回去自愈，而不是停在空草稿上——那种情况下第一条消息必然落地成新任务。
- **保存设置不再丢上下文**：重建内核后重新 `resume` 当前任务，下一条消息继续同一个会话；
  turn 正在跑时先按 Stop 语义取消，界面不会永远停在 running。

## 0.7.6 — 2026-11-07

- **shell 命令块改版**：命令和输出拆成两块——命令是独立浅灰卡片（圆角 8px、`$` 提示符按行
  铺满、等宽字体、首行带状态点：成功绿 / 失败红 / 取消琥珀 / 运行中灰），右上角新增「复制」
  按钮，点完 1.5 秒内显示「已复制」，复制的是原始命令而不是渲染后的行。
- **历史导航刻度**：右侧 turn rail 从圆点改成右对齐的标尺刻度——每一轮一根 11×3 的
  浅灰短线（`#e2e2e2`），当前轮次是 28×3.5 的近黑长条，右端对齐、向左伸长；刻度间距按
  可用高度自适应（最多 14px，太密时按 stride 抽稀），不再限制最多 15 个；命中区域是整条
  轨道宽度 × 间距。悬停预览卡改为「按下即跳转」——弹层在按下时就会消失，`on_click`
  等不到抬起，所以用 `on_mouse_down`。
- **测试**：去掉 `crates/qcode-providers/tests/parallel_tools.rs` 里的墙钟断言，改由
  `Probe::max()`（同时在飞的调用数）和结果状态证明并发、上限与取消；两次
  `test (ubuntu-latest)` 的红跑都出在这三处计时比较上。

## 0.7.5 — 2026-11-07

- **工具改动落盘，对话流里能看见 diff**（ADR-0057）：事件 schema 升到 v5，
  `ToolResult` 新增 `diff`（`patch` + 每文件 `kind/additions/deletions/bytes`），
  `edit_file` / `write_file` / `apply_patch` 在实际落盘后按真实结果算出统一 diff；
  失败、取消、被拒的调用不产出。
- **GUI**：工具行头显示 `+N −D`（新建文件另附大小），展开后在命令/输出盒子之上渲染红绿
  diff（高度按行数取 96–320px），底部「查看」面板是同一份 patch 的全量视图；`DiffState`
  按需解析，回放会话在加载时重建。
- **TUI**：工具行同样带 `+N −D`，新增 `/diff` 列出本会话所有带改动的调用、`/diff <n>`
  打印单条 patch。
- **老日志**：v ≤ 4 没有 `diff` 字段，从调用参数回推——`apply_patch` 用它自己的 patch
  （统计准确），`write_file` 合成全绿，`edit_file` 只保留计数（行号留空，不编造）。
- **turn 头摘要**：从「执行过程 · N 项」改为按工具归类的中文摘要（修改了文件、执行了命令、
  已读取文件），无可归类工具时退回原文案。
- 修掉 `apply_patch` 抽屉从未显示的旧 bug（读的是 `arguments["patch"]`，实际参数名是
  `diff`），并删掉那套 Expand/Collapse 双按钮抽屉。
- **历史导航的悬停预览重做**：卡片从 300px 单段文字改成 520px 的两段式——首行去 markdown
  标记后作加粗标题，其余保留换行原样展示（不渲染 markdown），问题/回答之间加一条细分隔线；
  高度上限取主区域的一半（`transcript_height × 0.5`，220–620px），按 20px 行高换算行预算，
  超出以省略号收尾。点击整卡仍跳到对应 turn。
- **CI 诊断**：`scripts/ci-clippy.sh` 的 GitHub 注解里转义反引号，并往 job summary 写一份
  兜底摘要，红跑不再只剩一个退出码。
- **Linux**：`show_window_later` 的 `cx` 参数在非 Windows 分支从未使用，补上引用避免告警。

## 0.7.4 — 2026-11-06

- **工作区快照不再复制工具链缓存**：`run_shell` 前的树快照（ADR-0034 补充）原先把
  `.cargo-home` 当源码复制——文件沙箱只允许写工作区，`CARGO_HOME` 就落在源码旁边，
  本仓库的 `.cargo-home` 有 1.8 GB / 64,364 文件，占工作区文件的 99.6%。64 MB 额度
  几乎全花在 registry 源码上，真正的代码反而进不了快照，回滚实质失效。`TREE_SKIP`
  补上 `.cargo-home` / `.rustup-home` / `.toolchains`（与 `crates/qcode-eval` 的跳过
  名单对齐），并补 `.qc-data`。实测日志里 `files=2381 bytes=67050542` 那一轮，
  68 MB 中有 67 MB 是它。
- **快照容量改为按 turn 累计封顶**：原先只累加本次调用新写入的字节，`stored.exists()`
  命中的文件不计入，于是同一 turn 里每条 `run_shell` 都还能再拿满 64 MB——上限号称
  每轮 64 MB，实际一轮可滚到 GB 级。现在先数该 turn `files/` 已有字节
  （`stored_totals`）再扣额度，已超额直接返回 `capped`，不再遍历。
  实测某会话 44 轮累计 18,951 MB，其中 18,899 MB（99.7%）是 `.cargo-home` 副本。
- **截断可复现**：工作区遍历改为字典序（含跨目录，子目录逆序入栈），撞上限时保留的
  是稳定前缀，同一棵树每次截断得到同一批文件，不再取决于文件系统的枚举顺序。
- **告警不再刷屏且区分成因**：容量告警每轮最多一次；`TreeReport` 新增
  `added_files` / `added_bytes`，区分「本次真的截断」（`> 0`，打 `WARN`）与「该 turn
  之前就满、本次什么都没加」（`== 0`，降到 `DEBUG`）。升级前被灌满的 turn 不会再
  每轮重复报同一句，也不会误报成新截断。
- **关窗/托盘恢复时不再二次借用 `App`**：关闭窗口或从托盘恢复时控制台报
  `ERROR reading power state for frame request ... RefCell already borrowed`。原因是
  `on_window_should_close` 的 `cx.update` 持有 `&mut App`，回调内同步 `orderOut:`
  让 AppKit 立刻成为另一窗口的 key window 并同步派发 `windowDidBecomeKey:`，gpui
  预 macOS 在该回调里同步请求一帧，帧请求又要 `App::update`——撞上外层借用。Windows
  侧同源（`SW_HIDE` 会让窗口同步收到 `WM_ACTIVATE`）。现在各平台后端拆成
  `capture()`（只读句柄）与 `hide_native()` / `show_native()`，新增
  `tray::hide_window_later` / `show_window_later` 把原生调用推迟到本次 update 结束
  后的 foreground executor；关窗回调、`CloseWindowShortcut`、设置窗关闭、「显示主
  窗口」四处改用延迟版。Windows 隐藏改用 `ShowWindowAsync`。
- **测试**：新增 4 条 core 测试——树快照跳过工具链缓存、按 turn 计费（第二轮不重新
  发放额度、新 turn 重新计）、截断前缀稳定、已满的 turn 第二次调用 `added_*` 为 0。
  抽 `TreeLimits` 让测试缩小上限，不必往磁盘写 64 MB。
- **文档**：README 与 ADR-0034 补充更新跳过名单、按 turn 计费与截断稳定性；记录
  工具链缓存必须跳过的原因，以及修复前留下的超额 turn 不迁移、需手动清的事实。

## 0.7.3 — 2026-11-06

- **Provider 目录对齐 pi-ai**：内置目录按 deepseek-harness 的 pi-ai 模型清单更新
  ——openai 默认 `gpt-5.4`（补齐 gpt-5.4 / 5.3 / 5.2 / 5.1 / 5 / 4.1 / 4o 与 o 系列共
  20 个模型），deepseek 默认改为 `deepseek-v4-flash`（新增 `deepseek-v4-pro` 与
  `deepseek-v4-flash-vision-exp`），minimax 增加 `MiniMax-M2.7` 与
  `MiniMax-M2.7-highspeed`，anthropic 补齐 `claude-sonnet-4-6`、`claude-sonnet-5`、
  `claude-opus-4-5`~`claude-opus-5`、`claude-haiku-4-5`、`claude-fable-5`。用户在
  配置里显式写下的旧模型名仍原样生效。
- **Provider 设置**：模型选择菜单改为单层列表（供应商作灰色小字组标题，模型直接列在
  其下）；菜单顶部加搜索框，按模型名实时过滤（大小写不敏感，匹配不到模型的供应商整组
  隐藏），列表最多显示 10 行、超出出现纵向滚动条，↑↓ 移动高亮 / Enter 选中 / Esc 关闭
  ——长清单（openai 已有 20 个模型）不必再一路滚动；保存时校验至少配置一个模型（无需
  密钥的供应商豁免），失败阻止落盘并在设置页提示；保存 API 密钥时先把未存入的密钥写进
  钥匙串，再触发 `api_key_command` 与 `config` 落盘，凭证绿点立即生效；
  `api_key_command`、`extra_body`、`args`、`env`、
  `network.allow`、`skills.roots`、`search.api_key_command`、`sandbox.extra_writable`
  等按行填写的值改用多行输入框——单行框会剥掉换行、把 argv 粘成一项，保存后 key
  command 直接 `os error 3`。自定义供应商改为模态弹框编辑。
- **设置窗口**：标题统一为中文「设置」；重复点击「设置」复用已打开的窗口并置顶，不再
  叠加窗口，也修掉了同步派发点击时重复借用 `App` 的 panic（`RefCell already
  borrowed`）；正文宽度跟随滚动区，MCP 说明等长文本自动换行，不再把设置框撑大；关闭
  编辑框即清理探测状态，探测轮询不再回写已关闭的对话框。
- **输入框状态栏**：改显 `N turn(s) - M token(s)`，标题去掉 turn 计数，右侧去掉费用；
  接入 `Delta::Context` 转发后 `ctx%` 不再恒为 0；模型名字号调为 15px。
- **侧边栏与任务行**：未配置 provider 时点「+」不再静默失败，改为在状态栏提示先去
  Settings；删掉最后一个 provider 后主界面立即回到 `unconfigured` / `-`，不用等重启
  或再点一次；设置按钮改为齿轮图标 +「设置」；任务操作与添加目录按钮固定纯白背景、
  悬浮不再变色；点击任务行后操作按钮保持可见；任务菜单支持点击外部自动关闭；目录
  列表与移除确认去掉 `@` 前缀；状态栏移除「空闲」标识。
- **运行中排队与插话**：turn 跑着的时候仍可继续发消息，分两种语义——排队等当前 turn
  结束再作为新 turn 依次执行（FIFO，可上下移动、删除），或插话在当前 turn 的下一个
  step 边界被认领、作为 user 消息追加进这次请求，模型下一步就回答它；插话没赶上最后
  一个 step 边界时自动降级为下一轮排队，不会丢。已推送（被引擎认领前）和排队中的消息
  在对话流末尾乐观回显：推送过的按已发送的用户消息同款气泡上屏，排队的保持低调的
  「排队中」样式，引擎认领后由真实事件渲染的 turn 卡替换；队列 dock 只管理尚未推送的
  消息，推送即从 dock 移除。
- **沙箱**：Sandbox mode 只列出当前平台可用的方案（Windows `none` / `windows-acl`，
  Linux `none` / `landlock` / `bwrap`，macOS `none` / `sandbox-exec`），跨平台迁移来
  的配置值仍可解析；任务没有根目录时权限按钮隐藏，状态只保留「工作区可写」/「仅可
  读取」并统一作用于所有目录。
- **界面细节**：可折叠行统一间距——chevron、类型图标与标签同属一组（8px），标签到
  `· 预览` 为 4px，「思考」行与运行命令行节奏一致；设置窗口标题字号与自动保存提示
  样式微调；Windows 图标资源 ID 改为 1/2，`MAKEINTRESOURCE` 风格资源 ID 改用
  `without_provenance` 表达。
- **测试**：`deepseek_defaults_to_flash` 与 `parallel_tools` 里对机器负载敏感的断言
  改为与同测试内实测的基准耗时比较（并发重叠本身仍被校验），CI 负载下不再误报。
- **内部**：fmt / clippy 清零，含 clippy 1.99 新报的 `values()` / `contains()` /
  `nonminimal_bool` 告警、`collapsible_if` 与两处 Windows 专属告警。
- **文档**：新增 `docs/design/GUI-FONT-CROSS-OS.md`，记录 `.SystemUIFont` 在 Linux
  启动时的 WARN 成因与各平台默认字体；Linux 已实测，macOS / Windows 待补。README
  新增「提交前自检」，说明 `make check` 与 `ci.yml` 同源、`--all-targets` 会把测试
  代码纳入 lint，以及只推一个远端（`gh` / `gitee`）不会触发 CI。

## 0.7.2 — 2026-11-05

- **修复侧栏不刷新**：`SessionChanged` 到达时既没标脏、也没安排下一帧，而空闲时 GUI
  没有周期性轮询，只在用户交互触发 render 时 poll——点「+」后会话其实已经落盘，侧栏
  却要等下一次交互才更新（Windows 上通常抢在点击那一帧之前建好，Linux 和慢一点的
  机器上就一直是空列表）。现在 `SessionChanged` 处理完标脏并安排下一帧，新建任务后
  短暂唤醒重绘，等 `empty_draft` 被改掉即停。
- **修复恢复会话的模型错配**：`SessionStarted` 里的 model/provider 只是历史快照，
  恢复时用它覆盖运行时会界面显示当前模型、请求却带着旧模型名打到当前 provider
  （恢复 mock 会话后向 deepseek 发 `mock-1`，直接 400）。现在恢复保持当前
  model/provider 不变，pricing 也按当前模型计算。

## 0.7.1 — 2026-11-04

- **依赖**：GUI 升级到 `gpui-kit 0.9.0`（`gpui-base 0.9.0`、`gpui-pre` 0.3.11 快照，zed
  `f816e1f`）。GUI 源码零改动即可编译运行；`gpui-pre-windows` 的 vendor 补丁重新同步到
  0.3.11 并重贴 `IsWindow` 守卫——上游仍会在关窗时对已销毁的句柄调用 `RedrawWindow`，
  见 [longbridge/gpui-kit#3795](https://github.com/longbridge/gpui-kit/issues/3795)。
- **内部**：`metal 0.33` 换成 `objc2-metal`，release 构建的 `block`
  future-incompat 警告随之消失。

## 0.7.0 — 2026-11-04

- **模型接入重构**（ADR-0055）：内置供应商收敛为一张目录表（协议 / 端点 / 默认模型 / 模型
  清单），新增 `protocol = openai|anthropic` 显式声明，平台型供应商（火山方舟等）与任意
  OpenAI 兼容网关零代码接入；Provider 目录页可按路由增删改、编辑模型清单并「探测」端点
  模型；`[default-model]` 成为默认命名空间，会话内切模型「切即默认」，未设默认时回落到
  第一个可用供应商，再回落到 mock。
- **设置页重做**：Provider 目录改为卡片列表（协议徽标、端点、凭证状态、默认模型、设为
  默认 / 停用），新增「添加 Provider」向导；凭证状态（已存 / 缺失 / 无需 key）单独可见；
  输入框内嵌模型按钮与沙箱权限下拉（带展开箭头），Turns 超限时出现提示卡片。
- **安全**：API 密钥不再经过命令行。macOS 改用 Security framework 直接写入钥匙串，
  Windows 改用 `CredWriteW` 并把密钥从 stdin 送入 PowerShell（ADR-0056）；provider 名在
  生成的 PowerShell 里按单引号字面量转义，堵住脚本注入。
- **修复窗口关闭报错**：vendored 的 `gpui-pre-windows` 在 VSync 线程重绘前加了 `IsWindow`
  守卫，消除「无效窗口句柄 / window not found」。
- **修复配置诊断**：`[ui]` 与 `[default-model]` 加入 `KNOWN_KEYS`，v0.6.1 里写这两节的用户
  不再看到 `unknown config keys are ignored` 警告。
- **内部**：`ConfigError` 的 TOML 载荷装箱（Windows 上不再触发 `clippy::result_large_err`）；
  `async-executor` 升级到 1.14.1（1.14.0 已被 yank，`cargo-deny` 失败）；fmt / clippy 清零。

## 0.6.1 — 2026-10-31

- **工作中指示器**：QC 标记改为绕竖直轴的立体翻转（正交投影）——2.4s 周期连续翻两次面
  后停顿；转到背面时镜像并降低亮度，横向摆动提示转动方向（右侧先转）。

## 0.6.0 — 2026-10-31

- **系统托盘**：macOS / Linux / Windows 三端托盘（ADR-0052）。关闭窗口改为最小化到
  托盘，托盘菜单提供「显示主窗口 / 新建任务 / 停止当前任务 / 退出」；Linux 用
  ksni（StatusNotifierItem + DBusMenu），macOS 用模板图，Windows 用消息窗口。
- **统一 QC 图标集**：macOS ICNS 与模板图标、Windows ICO、Linux hicolor PNG 与
  symbolic 图标，应用图标与托盘图标同源。
- **Queue / Steer**：运行中的 turn 可以继续排队追加，或「推送」进当前 turn 的下一步。
  GUI 队列 dock 支持上移、下移、删除、推送；等待下一步的条目可见并可撤回。TUI 提供
  `/queue`、`/steer N`、`/drop N`、`/steering`、`/unsteer N`。
- **事件 schema v4**：`UserMessage` 增加 `starts_segment` 与 `queued_id`，投影层不再
  靠启发式拆分 turn（ADR-0054）；v3 旧日志仍可回放。
- **Composer 四态**：Idle / Running / Pending / Restoring。提交审批答案后到引擎确认
  之间锁定输入，避免插入新消息（ADR-0042 §1）。
- **设置**：`config.toml` 新增 `[ui] busy_delivery = queue|steer`（GUI/TUI 共享），
  `gui.json` 新增 `send_mode = enter|cmd_enter`；Settings 页可直接修改。
- **DSH 风格「工作中」指示器**：对话流末尾显示旋转的 QC 标记与
  「工作中，用时 1分23秒 ···」，秒数每秒刷新；旧的状态栏等待文案与「本次任务耗时」
  行移除。
- **界面细节**：turn 圆点浮窗与轮次导航、字号缩放快捷键按平台绑定、权限/报错提示框
  改版、文件与目录选择弹框跟随系统语言、thinking 折叠预览超长省略、排队消息拆成
  独立 turn、长会话新建提示。
- **内部**：按职责拆分大文件（gui 的 lib/render/shell/setup、cli 命令、config、
  engine、sandbox、ui-state），clippy 清零，CI 补齐 Linux GUI 依赖，Windows MSVC
  链接栈配置。

## 0.5.0 — 2026-10-22

- **Task 工作单元**：Task 与 Session 1:1；Task 可配置多个目录（底层 `Root`），
  其中一个作为主目录。旧 session 自动迁移为单目录 Task。
- **多目录文件边界**：文件工具按 Task 目录解析 `@label/path`；只读目录拒绝写入；
  shell cwd 使用主目录。无目录 Task 在 Turn 私有 temp 中运行 shell。
- **macOS 多目录 sandbox**：`sandbox-exec` profile 按 Task 的 RO/RW 目录重建；
  无根 temp 可读写。Linux/Windows 多目录重建仍会 fail closed。
- **GUI 我的任务**：左侧只保留 Settings 与“我的任务”；Task 行可 hover 显示
  `…` / `+`，支持重命名、删除、加目录，不切换当前对话。目录可 Relink、
  切换 RO/RW、设为主目录、移除（带确认）。
- **目录显示**：一行展示 alias 与操作，完整路径只在 hover 时显示；Task 行左对齐、
  完整标题、hover/active 背景与块间距。
- **Turn 边界 Roots**：运行中编辑目录进入 pending，Turn 结束后写入
  `roots_changed`；Task 删除会清理本地 session 与输出。
- **Composer**：`Enter` 发送、`Shift+Enter` 换行、`Cmd/Ctrl+Enter` 兼容发送。
- **Settings 热重建**：保存后重建 backend；provider/model 立即同步；恢复历史 Task
  时不再用旧的 `SessionStarted.model` 覆盖当前模型。
- **Task 标题**：新增显式 title 与 `task_renamed` 事件（event schema v3）。
