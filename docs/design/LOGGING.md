# 输出流归属：`eprintln!` 分类表

配套 ADR-0077。**本文只分类，不改代码。** 迁移是一次单独的改动，因为它有一半的判断是
「这条消息到底该不该存在」，那是产品判断，不是机械替换。

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
| 1 | `qcode-gui/src/render/terminal_view.rs:1119` | `debug` | `capture 100 frames: {:?}` — 基准测试残留 |
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

## 不在本文范围

- **实际迁移**：等这份表被认可后单独做，按 crate 分批。**先做 `qcode-gui`：它的 32 处
  生产代码 `eprintln!` 全属 D 类**，而桌面用户对 stderr 的可见性最差——迁移收益最高、
  歧义最小。
- **`Redactor` 接成 `Layer`**：安全项，单独立项（ADR-0077）。
- **测试代码的 46 处**：不迁。测试自述用 `eprintln!` 是它们该有的形态。
