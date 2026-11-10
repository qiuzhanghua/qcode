# Changelog

## 未发布

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
