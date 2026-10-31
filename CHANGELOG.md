# Changelog

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
