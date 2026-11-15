# CLI 直接管理定时任务（ADR-0090 实现规格）

> 状态：**未实现**（规划，2026-11-15）。上游决策见 [DECISIONS.md](./DECISIONS.md) ADR-0090。
> 本文只写「怎么落」，不重复「为什么」；理由去 ADR 查。
> 兄弟文档：[SCHEDULE-REGISTRATION.md](./SCHEDULE-REGISTRATION.md)（ADR-0088，已实现）。

## 规划速览

六个步骤，前置是三个已拍定的取舍（§0）。每一步都能单独跑测试、单独算完成。

| 步骤 | 落点 | 依赖 |
|---|---|---|
| 0.3 取舍：`choose_workspace` 搬到 `qcode-core`（三处共用） | `qcode-core/src/paths.rs`、`qcode/src/commands/gui.rs` | — |
| 1 `schedule_create` 的 cwd 兜底（含 `qc run`） | `qcode-tools/src/schedule.rs`、`qcode-gui/src/shell/schedule.rs` | 0.3 |
| 2 两处上移：`driver_footer`、`parse_target`→`Target::parse` | `qcode-core/src/scheduler.rs`、`qcode-core/src/schedule.rs` | — |
| 3 CLI 参数面：`Command::Schedule` + 三个子命令 | `qcode/src/cli.rs`、`qcode/src/main.rs` | — |
| 4 命令实现：`cmd_schedule`（list / create / delete） | `qcode/src/commands/schedule.rs`（新文件） | 0.3, 1, 2, 3 |
| 5 测试：契约 + 跨进程 + cwd 兜底 | `qcode/tests/schedule_cli.rs`（新）等 | 4 |
| 6 收尾：README 对照表、ADR 状态、CHANGELOG | — | 4 |

**顺序上的硬约束**：0.3 必须先落，否则步骤 1 与步骤 4 会各自去抄一份根检查。

**范围边界**（不做，见 ADR-0090 §1）：`edit` / `pause` / `resume`、daemon、自然语言解析、
CLI 侧的 `--force`。

---

## 0. 三个已拍定的取舍

### 0.1 `--json` 直接序列化 `ScheduledTask`，不手抄字段

`ScheduledTask` 已经 derive 了 `Serialize`（`qcode-core/src/schedule.rs:1043-1044`），字段
含 `id` / `title` / `prompt` / `rule` / `target` / `overlap` / `status` / `next_fire_at` /
`cwd` / `missed_windows` / `last_receipt` / `delivery_log` 等。

**ADR-0090 §6 原本写「字段至少包含 …」，实现时改判为「原样序列化」**：手抄清单会和结构体
脱钩，而脱钩是静默的（加了字段没人知道 JSON 里没有）。代价是 `delivery_log` 最多 200 条
回执会一起进 JSON——`list` 的任务条数通常个位数，可以接受；若将来嫌大，加 `--json=full`
这类开关，而不是删字段。

外层包装固定为：

```json
{
  "driver": { "state": "running|stale|unknown", "at": "2026-11-15T15:00:00" },
  "tasks": [ /* ScheduledTask[] */ ]
}
```

`at` 在 `unknown` 时为 `null`。`create --json` 用 `{"driver": ..., "task": {...}}`。

**为什么不做顶层数组**：以后要加 `count` / `data_dir` / `schema` 这类元信息时，顶层数组
只能靠改结构来加，而 `.tasks[]` 可以加兄弟字段。脚本侧取法 `.["tasks"][0].id` 也不比
`.[0].id` 难写。

### 0.2 driver 文案只有两份，CLI 复用不新写

现状有**两份**驱动措辞：

| 位置 | 内容 | 可见性 |
|---|---|---|
| `DriverStatus::describe()`（`qcode-core/src/scheduler.rs:75`） | 一句话：「驱动在跑（定时任务会正常触发）」 | `pub` |
| `driver_footer()`（`qcode-tools/src/schedule.rs:35`） | 三件事：谁在触发、关窗 ≠ 停、不补跑 | **私有** |

CLI 如果新写一份，就是**第三份说法**——而 ADR-0090 §5 与验证 6 都要求三处说的是同一件
事。所以**第 2 步把 `driver_footer` 上移**到 `qcode-core/src/scheduler.rs`（挨着
`describe`），`qcode-tools` 改成调它。

不选「CLI 依赖 `qcode-tools` 去调」：`qcode-tools` 是工具实现 crate，CLI 只该依赖
`qcode-core` 的状态面——让一个纯展示函数把两个 crate 绑在一起，方向是反的。

上移之后两者的定位要说清：`describe()` 是一行摘要，`footer()` 是三段完整说明。
`list` 用 `describe()` 打第一行（列表要短），`create` / `delete` 用 `footer()`（一次动作
之后要把话说全）——与工具侧现在的用法一致（工具 `list` 用 `describe`，`create` 用
`footer`）。

---

### 0.3 `choose_workspace` 搬到 `qcode-core`，不各抄一份

**为什么**：步骤 1（工具兜底）和步骤 4（CLI 缺省）都要用它，而它现在住在 `qc` 二进制的
`commands/gui.rs:64-77` 里——`qcode-tools` 看不到它。各抄一份的后果是「缺省 workspace」
有三份实现，而它们必须永远一致（ADR-0090 §2 的整条论证都建立在这个一致性上）。

**改法**：

- 函数体一行不改，搬到 `qcode-core/src/paths.rs`（`real_home` 的邻居），`pub fn`。
- `gui.rs` 改成调它；GUI 侧那两处行为不变（回落时写 log 用的是返回的 `bool`）。
- 补两条单测（`paths.rs` 的 `mod tests` 里已有 `dangerous_write_root` 的先例）：
  根目录 + 有 home → `(home, true)`；非根目录 → `(自己, false)`。
- **Windows 的 `C:\\` 也靠 `parent().is_none()` 命中**，不要另外加平台分支。

**验收**：`cargo test --workspace` 全绿，且 `qc` 从 Dock 启动的 workspace 仍是家目录
（`gui_workspace` 的既有测试不动）。

---

## 1. 步骤 1：`schedule_create` 的 `cwd` 兜底

**问题**：ADR-0090 §2 / §11。`cwd: None` 到点回落到驱动进程的 workspace——GUI 里是
**GUI 启动时那个目录**（`runtime.rs:522-526`），从 Dock 启动时它是家目录（ADR-0085）；
而 headless 下「问用户」那道保护不存在（`qcode/src/commands/run.rs:224-228`，
`questioner` 仅在「非 `--yes` 且有 tty」时装配）。

注意这条**不是**在工具里另立缺省：走到 `schedule_create` 时 workspace 已经是 §2 的答案
（CLI 里是 pwd，Dock 启动的 GUI 里是 `~`），所以这里只是把「已生效的缺省」写成任务上的
显式值。**不要在这里再判一次根目录**——`/` 根本到不了这一步。

**改法**（`crates/qcode-tools/src/schedule.rs`，`ScheduleCreate::execute` 里 `cwd` 那段
之后）：

```rust
// ADR-0090 §11：兜底成本回合的工作目录。
// 优先级不变（显式给的 > 询问得到的 > 这里），只把「最后剩下 None」改成绝对路径：
// None 到点会回落到驱动进程的 workspace，而那是用户看不见的目录。
let cwd = cwd.or_else(|| {
    let resolved = ctx
        .shell_cwd()
        .and_then(|path| std::path::absolute(&path).ok());
    if resolved.is_none() {
        tracing::warn!("schedule_create: no working directory to fall back on; leaving cwd unset");
    }
    resolved.map(|path| path.display().to_string())
});
```

（`and_then` 而不是 `map(..).flatten()`：后者过不了 `clippy -D warnings`，仓库门禁里有这一条。）

要点：

- **不改既有分支**：`cwd` 已有值（模型给的或被询问到的）原样保留，只处理 `None`。
- `std::path::absolute` 不用 `canonicalize`：后者要求路径存在，且会解软链接——roots 可能
  是软链接，用户认得的是软链接那个名字。
- 兜底失败只 `warn` 不拒绝：见 ADR-0090 §11 第 3 条。
- **GUI 设置页同样要兜底**：`qcode-gui/src/shell/schedule.rs:504-512` 建任务时把
  `form.cwd`（用户填的目录，可为空）直传进 `ScheduleOp::Create`，`runtime.rs:1306-1318`
  原样交给 `NewTask`。为空时到点用的是 `engine.workspace`——**GUI 启动时那个目录**
  （`runtime.rs:522-526`，`cmd_gui` 里定的一次，不随 Task 切换变化）。

  所以 GUI 的兜底不是「修一个和 CLI 一样严重的错」，而是**把已经发生的回落写成显式值**：
  (a) `tasks.json` 里能看见它，(b) 以后 GUI 从别的目录启动也不改变这条任务的落点，
  (c) 三条入口（CLI / 工具 / GUI）的落盘规则一致。改法：`None` 时填
  `engine.workspace` 的绝对路径。

  这条不改也不算 bug（回落到 GUI 启动目录是合理的默认），所以**若步骤 1 的时间紧，它可以
  单独推**；但它与 CLI/工具那条是同一个字段的同一类决定，分开做容易变成三份说法。

**验收**：

1. `qc run`（非 tty）里调 `schedule_create`，`tasks.json` 的 `cwd` 是绝对路径，等于
   `--data-dir` 之外的当时 pwd；不是 `null`。
2. 在两个不同目录各跑一次，两条任务的 `cwd` 不同。
3. 显式 `"cwd": "/tmp"` 仍然写 `/tmp`（既有分支没被改）。
4. GUI 设置页新建任务，`cwd` 为空时落盘的是 GUI 的 workspace，不是 `null`。

---

## 2. 步骤 2：两处「工具私有 → core 公用」的上移

两件同类动作，一次做完：`driver_footer`（§0.2）与 `parse_target`（步骤 4.2 要用）。

**改法 A：`driver_footer`**：

- 把 `crates/qcode-tools/src/schedule.rs:29-53`（注释 + `driver_footer`）整段搬到
  `crates/qcode-core/src/scheduler.rs`，紧跟 `DriverStatus::describe()`；可见性改 `pub`。
- 注释里那句「（ADR-0088 §2）」保留，并补一句「ADR-0090 §0.2：CLI 也用这一份」。
- `qcode-tools/src/schedule.rs` 三处调用点（`:348` `:449` `:591`）改成
  `qcode_core::scheduler::driver_footer(ctx.driver)`。

**改法 B：`parse_target` → `Target::parse`**：

- 在 `qcode-core/src/schedule.rs` 的 `impl Target`（`:978`）里加
  `pub fn parse(text: &str) -> Option<Self>`，body 照 `qcode-tools/src/schedule.rs:68-81`
  原样搬（空串 / `new_once` / `new-once` → `NewOnce`；`new` → `New`；
  `session:<id>` 且 id 非空 → `Session`）。
- `qcode-tools` 删掉自由函数，改成调 `Target::parse`。
- **别名 `new-once` 要保留**：它是既有输入（工具参数、GUI 序列化都经过这里），删掉就是
  破坏性变更；搬家的目的只是换个位置。
- 好处不止省一份代码：`Overlap::parse`（`:361`）与 `TaskStatus::parse`（`:1014`）已经在
  core 里，`Target` 是三个里唯一还留在工具 crate 的——补上之后形状一致。

**验收**：

- `cargo test -p qcode-tools` 全绿（那三条文案断言不动，说明搬的是同一段字）。
- `crates/qcode/tests/schedule_registration.rs::cli_says_who_will_trigger_the_task` 仍绿
  ——它断言「只有 GUI 提供 / 退出进程才停 / 不补跑」三条都在。
- `cargo test -p qcode-core` 全绿（`parse_target` 的既有单测跟着搬家，断言不改）。

---

## 3. 步骤 3：CLI 参数面

**落点** `crates/qcode/src/cli.rs`：

```rust
/// Manage scheduled tasks (ADR-0090).
Schedule(ScheduleArgs),
```

```rust
#[derive(Debug, Args)]
pub(crate) struct ScheduleArgs {
    #[command(subcommand)]
    pub(crate) command: ScheduleCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ScheduleCommand {
    /// List scheduled tasks.
    List {
        /// Include paused and ended tasks.
        #[arg(long)]
        all: bool,
        /// Machine-readable output. The JSON shape is the stable contract.
        #[arg(long)]
        json: bool,
    },
    /// Create a scheduled task.
    Create {
        #[arg(long)]
        title: String,
        /// The message delivered when it fires (sent verbatim as a user message).
        #[arg(long)]
        prompt: String,
        /// cron <expr> | once YYYY-MM-DD HH:MM | every <secs> | daily HH:MM |
        /// weekly <days> HH:MM | monthly <day> HH:MM
        #[arg(long)]
        rule: String,
        /// new_once (default) | new | session:<id>
        #[arg(long)]
        target: Option<String>,
        /// Working directory for the delivered turn. Defaults to the current
        /// directory — stored absolute, since the driver may run elsewhere.
        #[arg(long, value_name = "PATH")]
        cwd: Option<PathBuf>,
        /// queue (default) | skip
        #[arg(long)]
        overlap: Option<String>,
        /// Approval timeout for the delivered turn; omit to wait for a human.
        #[arg(long, value_name = "SECS")]
        approval_timeout_secs: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a scheduled task by id.
    Delete { id: String },
}
```

注意：

- **子命令名用 `create` / `delete`，不是 `add` / `remove`**：`qc mcp` 用 `add` / `remove`
  （那是往配置里追加条目），这里是新建一个带 id 的实体，`create` / `delete` 与工具名
  `schedule_create` 一致。
- `--title` / `--prompt` / `--rule` 必填（clap 层面），不设默认值。`target` /
  `overlap` 用 `Option<String>` 而不是 `ValueEnum`：解析函数已经存在
  （`parse_target` / `Overlap::parse`），在命令实现里解析可以复用工具侧同一条错误文案，
  而不是让 clap 造第二份措辞。
- 参数名与工具参数逐字一致（`title` / `prompt` / `rule` / `target` / `overlap` / `cwd`），
  两个界面之间迁移不需要二次翻译。

**分派**（`crates/qcode/src/main.rs`）：

```rust
Some(Command::Schedule(args)) => cmd_schedule(args, data_dir),
```

放在 `Sessions` 之后、`Skills` 之前（与 `--help` 里的顺序一致，clap 按声明序）。同步函数，
不是 `async` —— `Scheduler` 的方法全是同步的，没有理由为它进 tokio。同时 `command_name`
加 `Some(Command::Schedule(_)) => "schedule"`。

---

## 4. 步骤 4：命令实现

**新文件** `crates/qcode/src/commands/schedule.rs`，在 `mod.rs` 里 `mod schedule;` +
`pub(crate) use schedule::cmd_schedule;`。

**签名**：

```rust
pub(crate) fn cmd_schedule(
    args: ScheduleArgs,
    data_dir: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>>
```

**公共骨架**：

```rust
// 与 `qc sessions` 一样用 hint：`list` 不该为了「可能用得上」就建出 data dir。
let root = qcode_kernel::data_dir_hint(data_dir);
let store = SessionStore::new(root);
let scheduler = Scheduler::new(store.clone());
let driver = DriverStatus::read(&store, qcode_core::schedule::now_local());
```

### 4.1 `list`

- 过滤规则**只作用于文本输出**：

  ```rust
  let mut tasks = scheduler.list();
  // ADR-0090 §1：默认只列 active；`--json` 永远全量（脚本要能看全）。
  if !all && !json {
      tasks.retain(|task| task.status == TaskStatus::Active);
  }
  ```

  `--all --json` 与 `--json` 输出相同——`--json` 已经是全量，`--all` 在它旁边是多余的，
  但**不要为这个组合报错**（`conflicts_with` 会让人以为「两个都不能给」，而实际含义是
  「全量，本来就全量」）。
- 文本输出顺序照 `schedule_list` 工具：**第一行驱动状态**（`describe()`），然后 `共 N 个`，
  然后每行一条。空时输出「当前没有定时任务。」并退出 0（不建 store）。
- 每行的格式复用工具 `row()` 的信息，但**不 import** 它（§10 不共享视图）：`- [active] 标题
  （id: …）\n  规则：… · 目标：… · 下次触发：…`。若 `cwd` 是 `Some`，追加 ` · 目录：…`。
- 输出走 `write_report`（`crates/qcode/src/commands/sessions.rs:12-36` 那个把
  `BrokenPipe` 当正常的写法）——`qc schedule list | head -3` 是正常动作。**把它从
  `sessions.rs` 挪到 `output.rs` 成为共享 helper**，两处都用。

### 4.2 `create`

按顺序：

1. `let rule = parse_rule(&args.rule)` → 失败时**打印错误并退出非 0**，不写盘。
2. `let target = Target::parse(args.target.as_deref().unwrap_or(""))` → `None` 时同上。

   `parse_target` 现在是 `qcode-tools` 的私有自由函数（`schedule.rs:68-81`），而
   `Overlap::parse`（`qcode-core/src/schedule.rs:361`）与 `TaskStatus::parse`
   （`:1014`）已经是 core 里的固有方法。**照那条既有形状加 `Target::parse`**
   （放在 `impl Target` 里、紧挨 `Target::describe`，`:978-994`），把 `qcode-tools`
   的自由函数删掉改成调用它——不留两份解析。这与 §0.2 的 `driver_footer` 上移是同一类
   动作，可以放在同一步做。
3. `overlap` 同理用 `Overlap::parse`。
4. `cwd`——**ADR §2 的核心，别照直觉写**：

   ```rust
   // 缺省 workspace 规则（ADR-0090 §2）：CLI 用 pwd，但文件系统根不是可用的缺省值
   // ——它会成为那条 task 的 primary root，而沙箱拒绝 `/` 作为 root。
   // 定时任务把这个失败从「当场报错」推迟成「几天后后台失败」，所以必须在这里挡住。
   let cwd = match args.cwd {
       Some(path) => {
           let absolute = std::path::absolute(&path)?;
           if !absolute.is_dir() {
               return Err(format!("--cwd {} is not a directory", absolute.display()).into());
           }
           absolute
       }
       None => {
           let here = std::env::current_dir()?;
           let (workspace, fell_back) = qcode_core::choose_workspace(here.clone(), qcode_core::real_home());
           if fell_back {
               // 说出来，不要静默换目录（ADR §2 的回显原则）。
               // 注意：这条 note 走 stderr，`--json` 的 stdout 不能被它污染。
               eprintln!(
                   "note: {} is a filesystem root; the task will run in {}",
                   here.display(),
                   workspace.display()
               );
           }
           workspace
       }
   };
   ```

   `choose_workspace` 从 `qcode/src/commands/gui.rs:64-77` **搬到 `qcode-core`**，签名
   `fn choose_workspace(cwd: PathBuf, home: Option<PathBuf>) -> (PathBuf, bool)`——
   **第二个返回值是「回落了没有」**，CLI 要用它决定是否打那行 note（GUI 用它写 log）。
   搬家时保持它的两个既有取舍：根目录回落 `~`；`~` 也拿不到时用原 cwd 并 `warn`
   （有个目录胜过没有）。

   显式给的路径不仅要绝对化，还要**存在性检查**：不存在就报错退出，不静默存一个注定
   失败的 cwd。
5. `scheduler.create(NewTask { … }, now_local())` → `Err(ScheduleError::Invalid(msg))` 时
   打印 msg、退出非 0。
6. 成功后打印：任务 id、规则、目标、**实际 cwd**、下次触发时刻，然后 `driver_footer`。

`create` 必须**建 store**：它要写盘，`save_schedule_tasks` 自己会 `create_dir_all`，
但为了错误信息可读，先 `SessionStore::ensure_root()`（若无此函数，用
`std::fs::create_dir_all(&root)` 并带上下文报错）。

### 4.3 `delete`

```rust
match scheduler.delete(&args.id) {
    Ok(()) => println!("已删除定时任务 {}", args.id),
    Err(ScheduleError::NotFound(id)) => { /* 打印 + 退出非 0 */ }
    Err(error) => { /* 打印 + 退出非 0 */ }
}
```

**不加 `--yes`**（ADR §7）：CLI 不假设有 tty。

### 4.4 退出码

沿用仓库惯例：成功 `ExitCode::SUCCESS`，任何失败 `ExitCode::from(1)`。**不要**引入新的
特殊退出码（`75` 是预算中断，属于 `run` 专有）。

---

## 5. 步骤 5：测试

### 5.1 契约测试（新文件 `crates/qcode/tests/schedule_cli.rs`）

照 `schedule_registration.rs` 的写法（`Command::new(env!("CARGO_BIN_EXE_qc"))` +
`--data-dir` 临时目录），至少六条：

| 用例 | 断言 |
|---|---|
| `list_on_an_empty_data_dir` | 输出含「当前没有定时任务」，退出 0，**data dir 未被创建**（`assert!(!dir.join("schedule").exists())`） |
| `create_then_list_round_trip` | `create` 退出 0 且打印 id；`list` 里出现该标题；`tasks.json` 里一条 |
| `create_stores_absolute_cwd` | 在两个不同 cwd 各建一次（用 `Command::current_dir`），两条 `cwd` 都是绝对路径且不同（**ADR 验证 10**） |
| `create_from_a_filesystem_root_uses_home` | `Command::current_dir("/")` 建一条，`tasks.json` 里的 `cwd` 是**家目录**而不是 `/`，且 stdout/stderr 里说明换过（**ADR 验证 9**） |
| `create_rejects_a_bad_rule` | `--rule "daily 25:00"` 退出非 0、stderr 可读，**`tasks.json` 不存在或为空**（ADR 验证 7） |
| `create_json_has_driver_and_task` | `create --json` 的 stdout 是合法 JSON，含 `driver.state` 与 `task.id` |
| `list_json_shape_is_stable` | `list --json` 是对象、`tasks` 是数组、每条含 `id` 与 `next_fire_at`、`driver.state` 三态之一 |
| `delete_removes_only_that_task` | 建两条、删一条，`list` 剩一条；`delete` 不存在的 id 退出非 0 且两条都在（ADR 验证 8） |
| `list_reports_the_driver_state` | 全新 data dir → 第一行「未知」；写入一条新鲜心跳（直接 `store.set_runtime_status`）→ 「在跑」（ADR 验证 1） |

`create_from_a_filesystem_root_uses_home` 是本批**最有价值的一条**：它盯的是「同一个缺省值
在延迟执行里变成静默失败」那个转移。写它之前先确认它能在今天失败——把 §2 的根检查去掉，
它应当读到 `cwd: "/"`。

### 5.2 跨进程（补进 `crates/qcode-core/tests/schedule_lock.rs`）

加一条 `cli_and_another_writer_do_not_lose_each_other`：子进程用 CLI 建，父进程用
`Scheduler::create` 建，最终两条都在。若启子进程不方便（那条测试的二进制造型是为
`qcode-core` 设计的），退一步：父进程与「子进程 = 测试二进制自身」各建一条，**CLI 走的是
同一个 `Scheduler::create`，锁的覆盖已由既有的两条用例证明**——那就把这条降级为注释说明，
不必强加。

**判定标准**：新增测试必须至少能抓住一次真实的错。写完先故意把 `cwd` 兜底改回 `None`、
把 `--all` 的 retain 去掉、把 `--json` 外层结构改成数组，各自看对应测试是否变红。红不了
的测试不写。

### 5.3 单元测试（`qcode-tools`）

`the_launch_marker_never_reaches_the_shell` 那种风格：步骤 1 的兜底在
`qcode-tools/src/schedule.rs` 的 `mod tests` 里加一条——`ToolContext::for_test` + 自己的
`Scheduler`（内存 store 没有，就用 `tempdir`），断言落盘 `cwd == Some(abs_path)`。

---

## 6. 步骤 6：收尾

1. **README 三端对照表**（`README.md:147`）：`定时任务 schedule_*` 行的说明改成
   「✅ 登记（`qc schedule create` 或对话）+ 驱动仅 GUI」。同一段的「四类差异」叙述里
   若有「只能通过工具登记」的说法，一并改。
2. **README 快速开始**：加两行 `qc schedule list` / `qc schedule create` 示例。
3. **ADR-0090 状态**：`**未实现**` → `**已实现**（日期）`，并按 SCHEDULE-REGISTRATION.md
   的做法补「实现注记」一节，写偏离（尤其 §0.1 的改判）。
4. **CHANGELOG**：`0.12.0`（新子命令是特性；若同时改坏了什么才算 breaking）。条目里
   必须写清「建了 ≠ 会跑」。
5. **`qc --help` 快照类测试**：`bare_invocation.rs` 断言的是 `Usage: qc` 与
   「Start the interactive terminal UI」，新子命令不破坏它；但若仓库里有命令清单快照，
   要更新（`grep -rn "Commands:" crates/*/tests/`）。

---

## 7. 不做（范围边界）

- `edit` / `pause` / `resume`：ADR-0090 §1 明写。`--json` 拿到 id 后用 GUI 或手改
  `tasks.json` 过渡。
- daemon：ADR-0088 §6 已否决，本规格不重新打开。
- 自然语言解析规则（「每天八点」→ `daily 08:00`）：那是 model 的活。
- `--force` / 软删 / 回收站：`Scheduler::delete` 的 `NotFound` 已够用。
- 在 `list` 里探测 GUI 进程或锁：只认 `runtime_status` 心跳。
