# Changelog

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
