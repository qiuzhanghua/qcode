# 资料来源与 clean-room 规则

## 允许

- 官方文档、公开演讲/博客、changelog、公开 issue 中的行为描述。
- 黑盒观察：CLI flag、退出码、JSON 事件形状、文件布局。
- 自己写的 spec 与实验记录。
- MIT / Apache-2.0 的通用库与公开论文。

## 禁止

- OpenAI Codex 源码（即使它是 Apache-2.0）。
- 任何形态的 Claude Code 源码，包括 2026 年泄漏的 npm bundle 与反编译产物。
- 第三方复刻/克隆项目。
- 第三方 agent 实现的 prompt 与内部架构（含 rig 自带的 coding agent）；
  只允许查阅公开 API 文档与用法示例。
- 把上述内容转述给实现者——“口头污染”同样算。

## 已核实的事实来源

- Codex：Apache-2.0，带 NOTICE；Linux sandbox 默认 bubblewrap。
- Claude Code：专有；Consumer ToS §3(3) 禁止反编译/逆向；
  Commercial ToS 禁止复制服务或构建竞品。
- Gemini CLI：Apache-2.0。
- dsh（deepseek-harness）：MIT，公开。
- macOS `sandbox-exec`：已标 DEPRECATED；嵌套调用 EPERM(71)。
- Landlock：路径级 FS、端口级网络；UDP 需 ABI 10。
- `birdcage`：GPL-3.0，排除。

## 商标

- 产品名、命令名、包名不得使用 Codex / Claude / Anthropic / Gemini。
- 只允许叙述性说明 “compatible with X”，不加 logo，不暗示背书。

## 贡献者声明

每次 PR 附带：

> 我确认未阅读上述禁止清单中的任何源码或反编译产物；
> 本提交的设计来自公开文档、黑盒观察或独立推导。

## 记录规则

- 每个设计结论在 ADR 中注明来源类别：`doc` / `black-box` / `derived`。
- release 前审 git 历史；依赖跑 `cargo-deny`。
