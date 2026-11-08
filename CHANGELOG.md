# Changelog

## 未发布

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
