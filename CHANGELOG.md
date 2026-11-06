# Changelog

## 0.7.3 — 未发布

- **Provider 目录对齐 pi-ai**：内置目录按 deepseek-harness 的 pi-ai 模型清单更新
  ——openai 默认 `gpt-5.4`（补齐 gpt-5.4 / 5.3 / 5.2 / 5.1 / 5 / 4.1 / 4o 与 o 系列共
  20 个模型），deepseek 默认改为 `deepseek-v4-flash`（新增 `deepseek-v4-pro` 与
  `deepseek-v4-flash-vision-exp`），minimax 增加 `MiniMax-M2.7` 与
  `MiniMax-M2.7-highspeed`，anthropic 补齐 `claude-sonnet-4-6`、`claude-sonnet-5`、
  `claude-opus-4-5`~`claude-opus-5`、`claude-haiku-4-5`、`claude-fable-5`。用户在
  配置里显式写下的旧模型名仍原样生效。
- **Provider 设置**：模型选择菜单改为单层列表（供应商作灰色小字组标题，模型直接列在
  其下）；保存时校验至少配置一个模型（无需密钥的供应商豁免），失败阻止落盘并在设置
  页提示；保存 API 密钥时先把未存入的密钥写进钥匙串，再触发 `api_key_command` 与
  `config` 落盘，凭证绿点立即生效；`api_key_command`、`extra_body`、`args`、`env`、
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
  Settings；设置按钮改为齿轮图标 +「设置」；任务操作与添加目录按钮固定纯白背景、
  悬浮不再变色；点击任务行后操作按钮保持可见；任务菜单支持点击外部自动关闭；目录
  列表与移除确认去掉 `@` 前缀；状态栏移除「空闲」标识。
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
- **内部**：fmt / clippy 清零，含 clippy 1.99 新报的 `values()` / `contains()` 两处
  告警与两处 Windows 专属告警。
- **文档**：新增 `docs/design/GUI-FONT-CROSS-OS.md`，记录 `.SystemUIFont` 在 Linux
  启动时的 WARN 成因与各平台默认字体；Linux 已实测，macOS / Windows 待补。

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
