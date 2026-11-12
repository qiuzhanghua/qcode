# 输出流归属：`eprintln!` 分类表

配套 ADR-0077 / ADR-0079。**状态：已迁移完毕。**

本文先只做分类（ADR-0077），再把 D 类迁走（ADR-0079）。分两步是因为有一半的判断是
「这条消息到底该不该存在」，那是产品判断，不是机械替换——先把它写下来，迁移时才不用
每处重新争论一遍。

下面是分类时的判断（保留作依据）。**结果见文末「迁移结果」**。

## 为什么要先分类

`cot_cli` 的教训（`docs/design/output-streams.md`）：逐处审计 68 个输出点之后，
**23 处是该留在 stdout 的产物**，43 处才该迁 stderr。据此推出一条规则：

> **stdout = 产物，stderr = 叙述。**
> 产物是别的程序要消费的（JSONL 事件流、`qc list` 的表格、`--version`），
> 叙述是给人看的（进度、审批请求、错误）。

`eprintln!` 已经满足「不污染 stdout」。所以**迁移不是流的问题，是可见性与可过滤性的问题**：
`eprintln!` 绕过订阅器，因此

- 不受 `RUST_LOG` 管，**关不掉**；
- 不受 `Redactor` 管（ADR-0077 里那个待办）；
- **不落盘**，所以桌面版用户（`Terminal=false`，stderr 埋进 journal 或被丢弃）永远看不到。

于是第二个判断维度：

> **A 类（保留 `eprintln!`）**：这是程序对「正在跑它的那个人」说的话。
> 交互提示、进度叙述、进程退出原因——加上时间戳和模块名反而是噪声，
> 而且必须**无条件出现**（`RUST_LOG=off` 不能把「按 y 确认」吞掉）。
>
> **D 类（迁 `tracing`）**：后台失败的诊断。用户没主动要求、终端里往往已经看不见、
> 排查时恰恰需要时间戳与模块名。**桌面版唯一的可见机会就是落盘。**

## 全量清点

| 位置 | 处数 |
|---|---|
| 生产代码 | **74** |
| 测试代码（`tests/`、`tests.rs`、`*_tests.rs`） | **46** |
| 合计 | **120** |

> 更正：此前口头说过「71 处」，那是估的。实测 120 处，其中测试代码 46 处。
> 测试里的 `eprintln!("skipping: …")` 是测试自述，不是产品输出，**不在迁移范围**。

测试代码分布（全部 **A**，不迁）：`qcode-tools/src/sandbox/tests.rs` 22、
`qcode-tools/tests/procguard.rs` 7、`qcode-terminal/tests/` 5、
`qcode/src/tests/sandbox_roots.rs` 3、`qcode-gui/src/setup/tests.rs` 3、
`qcode-shell/tests/detect.rs` 2、`qcode-kernel/src/config/tests.rs` 2、
`qcode-tools/tests/tools.rs` 1、`qcode-kernel/tests/mcp_grant.rs` 1、
`qcode-terminal/tests/no_lost_output.rs` 1。

## 生产代码逐处

### A 类：保留 `eprintln!`（35 处）

| 处数 | 位置 | 理由 |
|---|---|---|
| 8 | `qcode/src/output.rs:186,207,226,235,243,256,259,265` | `qc run` 的人读叙述通道：`[cancel]` `[timeout]` `[tool]` `[approval]` `[mcp]` `[budget]` `[turn]`。stdout 留给 JSONL |
| 8 | `qcode-core/src/approval.rs:82,83,85,88,91,93,95,98` | 审批提示，**必须无条件出现**：`RUST_LOG` 不能吞掉「按 y 确认」。加时间戳纯属噪声 |
| 3 | `qcode-core/src/question.rs:233,234,236` | 同上（`ask_user` 的选项列表） |
| 4 | `qcode/src/commands/run.rs:342,352,358,377` | `[update]` `[timeout]` `[cancelled]` 叙述，与 `output.rs` 同性质 |
| 1 | `qcode/src/commands/run.rs:160` | `[config] cannot persist the raised limit` — 用户刚发起的动作失败了，要当场知道 |
| 1 | `qcode/src/commands/settings.rs:91` | `warning: {err}` — 见下面「顺手记下的问题」 |
| 1 | `qcode/src/commands/misc.rs:176` | `--full needs a terminal; printing names only` — 解释降级模式 |
| 1 | `qcode/src/commands/gui.rs:81` | `this build has no GUI support; …` + 退出码 2 = 退出原因 |
| 1 | `qcode/src/main.rs:104` | `error: {err}` — 进程退出原因，最后一句 |
| 3 | `qcode/src/logging.rs:152,181,250` | **自举期**：日志系统自己启动失败/级别写错。此时没有可用的 logger，只能 `eprintln!`。这是自指的必要例外，加注释说明 |
| 1 | `qcode/src/sandbox_probe.rs:74` | `probe!` 宏，Windows 实验命令的产物 |
| 2 | `qcode-eval/src/main.rs:159,166` | `qc-eval` 的用法错误 |
| 1 | `qcode-tools/src/windows_acl.rs:352` | `qc-sandbox-run: {detail}` + 退出码 = 退出原因 |

### D 类：应迁 `tracing`（39 处）

| 处数 | 位置 | 目标级别 | 理由 |
|---|---|---|---|
| 6 | `qcode/src/commands/gui.rs:120,128,137,146,150,157` | `info` / `warn`(128) | 启动诊断。`Terminal=false` 启动时**一个字都看不到** |
| 1 | `qcode-core/src/store.rs:729` | `warn` | `cannot prune schedule tasks` — 后台清理失败 |
| 1 | `qcode-gui/src/lib.rs:187` | `warn` | `cannot save gui.json` |
| 5 | `qcode-gui/src/setup/save.rs:484,494,516,533,580` | `warn` | 自动保存失败/跳过。桌面用户唯一的可见机会 |
| 1 | `qcode-gui/src/setup/mod.rs:514` | `warn` | `cannot save gui.json` |
| 4 | `qcode-gui/src/tray/{linux,windows}.rs:216,220,99,103` | `warn` | 托盘起不来。**和本次会话最初那个图标问题直接相关**：这正是要看的那行 |
| 9 | `qcode-gui/src/runtime.rs:112,153,195,258,278,326,924,942,1186` | `warn` | 调度/默认模型/切 provider 失败 |
| 1 | `qcode-gui/src/run.rs:23` | `info` | `killed {killed} leftover process group(s) on quit` |
| 4 | `qcode-gui/src/shell/session.rs:24,97,251,383` | `debug` | `new_session handler blocks=…` — 明显是开发期探针 |
| 2 | `qcode-gui/src/shell/view.rs:517,619` | `warn` | 设置/侧栏状态保存失败 |
| 1 | `qcode-gui/src/shell/mod.rs:293` | `debug` | `window ready pid=…` |
| ~~1~~ | ~~`qcode-gui/src/render/terminal_view.rs:1119`~~ | — | **分类时判错了**：它落在 `#[test] #[ignore] fn capture_cost()` 里，是测试自述，属 A 类。见「迁移结果」 |
| 1 | `qcode-gui/src/render/main.rs:1465` | `debug` | `key=… platform=…` — 按键探针 |
| 1 | `qcode-gui/src/render/mod.rs:61` | `warn` | `cannot save window geometry` |
| 1 | `qcode-gui/src/render/mod.rs:76` | — | `qcode-gui root mouse down` — 调试残留，**建议直接删，不迁移** |

**D 类里 12 处是 `debug`/残留**（`shell/session.rs` 4、`terminal_view.rs` 1、
`render/main.rs` 1、`render/mod.rs` 2、`shell/mod.rs` 1，加 `runtime.rs:278` 的
`imported {added}`）——这些本来就不该用 `eprintln!`，迁移时先问「还需不需要」，
再问「什么级别」。

## 顺手记下的问题（迁移时一并处理）

1. **`qcode/src/commands/gui.rs:120` 手搓了 `" WARN "` 前缀，还带一个前导空格。**
   ```rust
   eprintln!(" WARN config directory {} denies new files: {}", …);
   ```
   这正是 `tracing` 已经做好的事，而且手工版本没法被 `RUST_LOG` 关掉。迁 `tracing::warn!`。
2. **`gui.rs:120` 与 `gui.rs:137` 重复打印同一个 `config=` 路径**（137 紧接着无条件再打
   一次）。迁移时合并。
3. **`qcode/src/commands/settings.rs:91` 用 `eprintln!("warning: {err}")`** —— 小写的
   手搓级别名。终端的 `WARN` 和这里的 `warning:` 两种拼法，搜索时漏掉一种。
4. **`qcode/src/output.rs:186` 的消息以字面 `\n` 开头**（为了和上文的进度行隔开）。
   迁 `tracing` 时这个换行会变成独立空行记录，得重新安排格式——这是 A 类留 `eprintln!`
   的又一条实际理由。
5. **`qcode-gui/src/render/mod.rs:76` 的 `root mouse down` 是调试残留**，没有迁移价值。

## 迁移判据（一次判定，不必每次重议）

```
这条消息是「跑 qc 的人」正在等的回答吗？
├─ 是 → A：eprintln!，无条件，不加时间戳
└─ 否 → 后台诊断 → D：tracing
        ├─ 有东西失败了，且用户可能想知道 → warn
        ├─ 状态推进，排查时要看        → info
        └─ 开发期探针                  → debug（先确认还要不要）
```

例外只有一处：**日志系统自身的失败**走 `eprintln!`，因为没有 logger 可用。

## 结案

三件事全部完成，本文到此为止：

| 原先的待办 | 结果 |
|---|---|
| 迁移 D 类 | 38 处迁移、D 类归零，见文末「迁移结果」与 ADR-0079 |
| `Redactor` 接成 `Layer` | 见 ADR-0078 |
| 测试代码的 46 处 | **不迁**（有意保留）：测试自述用 `eprintln!` 是它们该有的形态 |
| `qc logs --follow` | ADR-0077 划在范围外，后补做，见 ADR-0080 |

关于「`Redactor` 之后 D 类理由才成立」——这条**成立并已兑现**：D 类的依据是「桌面用户
唯一的可见机会就是落盘」，而落盘的代价是泄漏窗口从「stderr 一闪而过」变成「文件躺
7 天」。ADR-0078 补上脱敏之后，这个依据才是完整的；两者必须一起成立，否则 D 类的迁移
是把 bug 从「看不见」变成「看得见但泄密」。顺序上先做脱敏（0078）再迁移（0079），是
有意的。

分类判据（上文「迁移判据」一节）保留自用：以后新增输出点时照着判，不必重新讨论。

## 迁移结果（ADR-0079）

### 数字

| | 处数 |
|---|---|
| 分类时的「生产代码」grep 命中 | 74 |
| 其中实为 `#[test]` 内的基准测试（`terminal_view.rs:1119`） | −1 |
| **真实生产调用** | **73** |
| ADR-0078 已迁（`logging.rs` 的两处启动失败改为 `debug!`） | −2 |
| 本轮迁移（分类表的 D 类 39 处，减去上面那个误判） | **−38** |
| **剩余生产调用** | **33** |

33 处**全部是 A 类**：`output.rs` 8、`approval.rs` 8、`question.rs` 3、
`commands/run.rs` 5、`qcode-eval` 2、`commands/gui.rs` 1、`commands/misc.rs` 1、
`commands/settings.rs` 1、`logging.rs` 1、`main.rs` 1、`sandbox_probe.rs` 1、
`windows_acl.rs` 1。

**D 类剩余 0。** 测试代码 46 处不动。

### 迁移中冒出来的三个判断（分类表没覆盖到的）

**1. 用户输入校验失败不能用 `warn`。** 分类表把 `setup/save.rs` 的 5 处都算作
`warn`，但其中两处是「表单值还不合法」和「标量值不合法」——**每次防抖窗口都会触发**，
用户打一个 URL 打到一半就报一次。而它们的原因已经显示在设置页顶部了。放 `warn` 会让
真正的写盘失败淹没在输入噪音里，所以这两处定为 **`debug`**；另外三处（写 `config.toml`
/ `gui.json` 真失败）才是 `warn`。

**2. `QCODE_GUI_KEY_DEBUG` 环境变量撤掉了。** 6 处调试探针包在它里面——那正是「没有
级别机制」时的替代品。现在有 `[log] level`/`RUST_LOG` 了，两个开关并存只会让下一个
人不知道按哪个。全部改为 `debug!`，变量删除。**GUI 因此少了一个私有开关**，这是
净收益：`RUST_LOG=qcode_gui=debug` 本来就能做到，而且还能写进配置文件。

**3. 一处探针顺手删掉了用户内容。** `send_prompt` 原本记 `value.chars().take(20)`，
即用户输入的前 20 个字符。ADR-0077 让日志存活 7 天之后，这成了「把用户的话写进文件」，
而它没有任何诊断价值——判断「prompt 没发出去」靠的是 `len` 和 `running`，不是用户打了
什么字。改成只记长度。

### 一处刻意**不**删的东西

`render/mod.rs` 根节点的 `on_mouse_down`：它原本只有一个 `eprintln!("root mouse down")`，
看起来是标准调试残留。日志删了，但**注册保留**（改成空闭包并注明原因）——根节点是否
消费鼠标按下会影响焦点和事件冒泡，而这是无法用单测验证的 GUI 行为变更。删掉一行打印是
安全的，删掉一个监听器不是。

### 补记：这次迁移漏掉的类——stdout 上的叙述

迁移的靶子是 `eprintln!`，因为分类表的假设是「stdout 上只有产物」。**那个假设没有人
去验证**，于是漏掉了一类反方向的违规。复查整个 `qc` 主程序，共两处：

**其一，`commands/gui.rs` 的启动摘要：**

```rust
println!("gui: provider={} model={} sandbox={} tools={} skills={}", …);
```

它是 `9d111dd feat(gui): add the qc gui subcommand` 的脚手架产物——那时的窗口还挂着
placeholder kernel，这行是「我确实装配了内核」的开发期确认。kernel 接上之后这行留了下来。

代价正是 ADR-0077 要解决的那件事：`.desktop` 启动 `Terminal=false`，stdout 被丢弃，
**恰恰是用户想知道「内核解析成了哪个 provider、多少个工具」的时候看不到它**；而且它
绕过 `Redactor` 与 `RUST_LOG`，还混进 stdout 让人误以为那是 `qc gui` 的输出。

改成 `tracing::info!`，与同文件里 `starting TUI` 一行同形（结构化字段、落盘、可过滤）：

```
INFO qc::commands::gui: gui: kernel resolved provider=deepseek model=deepseek-flash
    sandbox=landlock tools=35 skills=8
```

**其二，`update.rs` 的交互提示与下载进度：**

```rust
print!("install {} and restart? [y/N] ", release.version);
std::io::stdout().flush()?;
println!("downloading {}", asset.url);
```

提示写在 stdout，与 `approval.rs:100` 的 `eprint!("  allow? [y/N] ")` 形状相同、流不同。
`qc update > out.log` 且 stdin 仍是终端时，问题进了文件、终端上什么都没有——命令干等着，
看起来就是卡死。`downloading` 是进度，脚本解析 stdout 要的是结果（`installed …`）而不是
每个字节区间的播报；`[update]` 的通知本来就走 `eprintln!`。两处均改为 stderr。

`--check` 的 `update available` / `up to date` 留在 stdout：那是脚本读的产物，现有测试也在
断言它们。

`crates/qcode/tests/stdout_channels.rs` 守住这个不变式，三条规则：

1. **提示必须走 stderr。** `print!`（不带换行）是「问人一句话」的形状，`qc` 主程序里不允许
   出现在 stdout。`update.rs` 就是这样踩的：`qc update > out.log` 且 stdin 仍是终端时，
   问题进了文件、终端上什么都没有，命令干等着——看起来就是卡死。对照 `approval.rs:100`
   用的是 `eprint!`。
2. **`println!` 只允许出现在白名单文件里**（`PRODUCT_FILES`），每条都注明谁消费这份输出。
   不在名单上的文件要写 stdout 就得先做决定：改走 `tracing`/`eprintln!`，或者说明读者是谁。
   顺带检查白名单没有过期条目，免得旧名字被新文件复用后悄悄获得 stdout 权限。
3. **`qcode-gui/src` 一律不得写 stdout**——它的产物是那个窗口。

用源码扫描而非运行，是因为这些路径要 `gui` feature、显示器或活的 release 服务器，而规则
必须在人人都能跑的构建里成立；文件里另有一条自测，确认分类器能分清 `print!`/`println!`/
`eprintln!`/注释，免得断言因为什么也没扫到而空过。测试模块不在范围内（那只是测试在自述）。

### 验证

- `qc gui` 在本机 X（`:1`）上真跑起来，日志确实落进 `qc.log`：
  `INFO qc::commands::gui: gui: config discovered …`、
  `DEBUG qcode_gui::shell: window ready …`。这是这次迁移唯一有意义的验收——迁移的理由
  就是「桌面用户看不到 stderr」。
- 默认 `warn` 级别下 GUI 启动**不产生迁移带来的噪音**，文件里只有真实告警
  （含 `wgpu_hal` 的 Vulkan 驱动警告——那正是「窗口画不出来」时要看的东西，保留）。
  **注：默认级别已于 ADR-0082 改为文件 `info` / 终端 `warn`。** 当时这条顾虑只在终端成立，
  桌面用户没有终端；文件保持 `info` 才有记录价值。GUI 启动的那几条 `info` 从此会落进文件，
  这正是它们存在的理由。
- `cargo test --workspace` 110 个测试二进制通过；`cargo test -p qcode-gui --all-targets`
  通过；clippy（含 `-p qcode --features gui`、`-p qcode-gui`）与 fmt 干净。

## 会话边界与级别分层（ADR-0082）

`tracing` 接进来之后，日志文件才第一次**可用**；可用之后立刻显出三处不足，这一节记结论。

### 两个去向，两个默认级别

| 去向 | 默认 | 理由 |
|---|---|---|
| `logs/qc.log` | `info` | 桌面启动没有终端可回滚，文件是唯一的记录；只有告警的日志回答不了「它做了什么」 |
| stderr | `warn` | 交互时不该把每条 `session created` 念到用户提示符上 |

显式设置（`RUST_LOG` 或 `[log] level`）同时作用于两者：写 `RUST_LOG=debug` 的人就是要盯终端。
只有「谁都没说」才分叉，所以 `[log] level` 的类型是 `Option<String>`——必须能区分「没写」与
「写了 `warn`」。

⚠️ 默认值有**两处**：`qcode/src/logging.rs` 的常量与 `qcode-kernel` 的 `default_log_level`。
只改前者没有效果，serde 会把配置填满。

### 边界行

```
INFO qc::lifecycle: qc session starting pid=313667 version="0.8.7" command=acp data_dir=/…
INFO qc::lifecycle: qc session ending   pid=313667 exit=143 reason=sigterm uptime_ms=3994
```

- 只有这两条带 pid；`reason` ∈ `normal` / `sigint` / `sigterm` / `sighup` / `panic`。
- **不记 argv**：启动时 `redact::install` 还没武装，`--api-key` 会原样落盘。只记子命令名。
- 必须在 `attach_file()` 之后——之前写的会被 `FileTarget::Nowhere` 丢掉，等于写了不落盘。
- `qc logs` 自己豁免（读取不该创建被读的文件），它的边界行只在终端。

**硬杀（`SIGKILL`/`TerminateProcess`/OOM）不跑任何代码**，靠下次启动补账：扫日志尾部 256 KB，
发现「有 start 无 end」就补一条 `warn`。

### `qc logs` 显示本地时间

文件保持 UTC（轮转边界是 UTC 硬编码，改不了），显示时转本地。逐行查偏移，不是启动时定一次
——后者跨 DST 会错一小时。非时间戳行与解析失败的行原样输出：宁可显示 UTC，不丢行。

代价：`cat`/`grep`/粘贴给别人的仍是 UTC，`qc logs` 的输出与原文件不逐字节相同。见 ADR-0082。
