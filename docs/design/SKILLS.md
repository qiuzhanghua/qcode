# Skills

> 状态：已实现（2026-10-19）。格式遵循公开的
> [Agent Skills specification](https://agentskills.io/specification)；本文件只记录
> qcode 的取舍与边界。

## 1. 是什么

Skill 是一个目录，至少包含 `SKILL.md`（YAML frontmatter + Markdown 正文），可选带
`scripts/`、`references/`、`assets/` 等附件。它是一包**按需加载的指令**，不是权限：
skill 里让模型跑的命令仍然走正常的策略与审批。

## 2. 发现与优先级

| 来源 | 目录 |
|---|---|
| global | `~/.agents/skills/<name>/SKILL.md`（`npx skills -g` 的通用位置） |
| global | `~/.config/agents/skills/<name>/SKILL.md`（universal agent 位置） |
| user | `~/.config/qcode/skills/<name>/SKILL.md` |
| config | `[skills] roots = [...]` 里的目录，按列出顺序 |
| project | `<workspace>/.agents/skills/<name>/SKILL.md` |
| project | `<workspace>/.qcode/skills/<name>/SKILL.md`（qcode 专属，最高） |

- 优先级 global → user → config → project(.agents) → project(.qcode)，后者覆盖前者；
  `roots` 支持 `~/`，相对路径按 workspace 解析。
- 同名时后加载的覆盖先加载的；其余按名字排序。
- 目录名必须与 frontmatter 的 `name` 一致；不合法、缺字段、超限、symlink 的 skill 会
  `warn` 并跳过，不影响 qc 启动。
- 没有 `HOME` 时只丢 global / user 根，config / project 根照常工作。
- `[skills] enabled = false` 完全跳过发现；`qc skills list` 可打印发现结果与被检查的根。

## 3. Frontmatter 子集

qcode 只解释两个字段：

- `name`：1–64 字符，只允许 `a-z`、`0-9`、`-`；不能以 `-` 开头/结尾，不能有 `--`，
  且必须与目录名一致。
- `description`：1–1024 字符，进入 system prompt。

值支持单行、单/双引号、`|` 字面块与 `>` 折叠块；`metadata`、`license`、`compatibility`
等字段被忽略。`allowed-tools` **刻意不支持**：skill 不能放宽审批。

## 4. 渐进披露

1. **启动**：只读 frontmatter，把 `name` + `description` 组进 system prompt 的
   `# Skills` 段（上限 16KiB，超出截断并注明）。
2. **激活**：模型调用 `skill` 工具，返回 `# Skill: <name>`、来源目录、完整正文和附件清单。
3. **附件**：模型用 `skill(name=..., file="references/X.md")` 读取单个文件；也可在得到
   绝对路径后用 `run_shell` 调 `scripts/` 里的脚本（仍按 shell 审批）。

这样，不使用 skill 的 turn 只付一次索引的 token；正文与附件不进入上下文。

### TUI 快速检查

- `/skills`：列出 `name [source] files=N` 和 description；没有发现时提示去跑
  `qc skills list` 看根目录。
- `/skills <name>`：直接显示正文、来源目录与附件清单，不经过模型、不写事件日志。
- 命令在 turn 运行期间也可用；修改 skill 或 config 后仍需重启 qc 才会重新发现。
- 以 `/` 开头的输入一律先当 UI 命令处理；要让模型看到以 `/` 开头的文本，目前需要改写。

## 5. 工具契约

`skill(name, file?)`，`ApprovalClass::Read`（只读本地文件，默认 allow）。

- 未知 skill / 未知附件 → 工具错误，附可用名称。
- `file` 必须是 skill 根下的相对路径；绝对路径、`..`、symlink 逃逸、二进制文件被拒绝。
- 输出与内置工具一样做 32KiB 截断，并带 `[skill:<name>]` 来源标记。

## 6. 限制

| 项 | 上限 |
|---|---|
| `SKILL.md` 大小 | 64KiB |
| description | 1024 字符 |
| system prompt 索引 | 16KiB |
| 列出的附件数 | 200 |
| 单个附件读取 | 256KiB（输出截断到 32KiB） |
| 附件扫描深度 | 8 层 |

## 7. 安全

- skill 目录与 `SKILL.md` 都拒绝 symlink；附件读取先 canonicalize 再校验前缀。
- skill 内容是**不可信指令**：不改变审批、不扩大 workspace、不引入 `allowed-tools`。
- 用户级 skill 在 workspace 之外，只有 `skill` 工具能读；`read_file` 仍被限制在 workspace。
- 不做 imports、远程下载、自动激活或 slash command；这些留给后续版本。

## 8. 配置

```toml
[skills]
enabled = true   # 默认 true
roots = ["~/space/skills/skills"]   # 额外目录，可选
```

## 9. 测试

- 发现/优先级/跳过非法项、frontmatter（引号、`|`、`>`、未知字段）、名字与目录不匹配、
  超限、symlink 目录与 `SKILL.md`。
- `skill` 工具：正文+附件清单、读取附件、路径逃逸、绝对路径、symlink、二进制、未知 skill。
- 索引预算截断；引擎端到端（索引 → `skill` 调用 → tool_result）。
