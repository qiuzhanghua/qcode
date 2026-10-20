# `qc acp`：在编辑器里用 qcode

`qc acp` 是 ACP（Agent Client Protocol）v1 的服务端：编辑器把 qcode 当子进程拉起，
通过 stdio 上的 JSON-RPC 驱动同一个引擎（工具、审批、会话日志、压缩都由 qcode 负责）。
决策与边界见 [DECISIONS.md](./DECISIONS.md) 的 ADR-0032。

## 1. 协议与实现范围

| 方法 | 方向 | 说明 |
|---|---|---|
| `initialize` | client → agent | 协商版本与能力；回 `protocolVersion`（客户端更高时回 1） |
| `authenticate` | client → agent | qcode 不需要认证，直接成功，`authMethods` 为空 |
| `session/new` | client → agent | 用请求里的绝对 `cwd` 建会话，回 `sessionId` |
| `session/load` | client → agent | 读该会话的 JSONL 并重放成通知 |
| `session/prompt` | client → agent | 跑一轮，流式 `session/update`，最后回 `stopReason` |
| `session/cancel` | client → agent（通知） | 取消正在跑的那一轮 |

`stopReason` 取值：`end_turn`（正常）、`cancelled`（被取消）、`max_turn_requests`
（命中 `[agent] max_turns`）；其它失败也以 `end_turn` 结束，但会先发一条
`agent_message_chunk` 文本说明错误——**失败不会是静默的**。

事件映射：`assistant_message` 的文本 → `agent_message_chunk`，思维 → `agent_thought_chunk`，
`tool_call` → `tool_call`（kind 按工具名映射），`tool_result` → `tool_call_update`
（`completed` / `failed`），审批 → `session/request_permission`。

审批选项：`allow-once`（这次）、`allow-session`（本轮内该工具不再问）、`reject-once`。
qcode 不跨轮记住选择：客户端应自己缓存 `allow_always` 的选择并在下次直接作答。

**明确不做**：`fs/*` 与 `terminal/*` 委托（工具在本机执行）、客户端传入的 `mcpServers`
（MCP 由 qcode 配置决定）、图像/音频 prompt（回 `-32602`）、`session/set_mode`、
`session/list`、逐 token 流式、ACP 会话里的 `subagent` 工具。

## 2. Zed

`settings.json`（Windows 在 `%APPDATA%\Zed\settings.json`，macOS 在
`~/.config/zed/settings.json`）：

```json
{
  "agent_servers": {
    "qcode": {
      "command": "C:\\Users\\you\\space\\research\\qcode\\target\\debug\\qc.exe",
      "args": ["acp"]
    }
  }
}
```

要点：

- `command` 必须是**绝对路径**或 PATH 上能找到的名字，Zed 不会继承你 shell 的别名；
- `args` 只写 `["acp"]`；配置、数据目录、审批策略都走 qcode 自己的配置（或环境变量
  `QCODE_CONFIG` / `QCODE_DATA_DIR`），不要在 `args` 里塞 `--config`，否则换个仓库就失效；
- 需要非默认审批策略时，在 `args` 里加 `["acp", "--approval-policy", "C:\\path\\policy.toml"]`
  （这是全局参数，对 `acp` 子命令同样生效）。

打开方式：Zed 里 `Ctrl-?` 打开 agent panel → 选择 `qcode` → 输入一句话 → 回车。

### 在 Zed 里验收的清单

按顺序做，每条都能肉眼判断：

1. **连接**：选中 qcode 后不报 "failed to start"。失败时看 Zed 的日志，或在本机直接跑
   `qc acp`（喂一行 `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}`
   应该立刻回一行 `result`）。
2. **普通回答**：发一句「用一句话自我介绍」，出现流式文本，结束后没有错误气泡。
3. **工具调用 + 权限**：发「列出当前目录的前五个文件」。应弹出权限请求（allow once /
   allow this tool for the session / reject）；选允许后出现工具执行与结果；选拒绝后
   agent 应说明被拒绝，而不是假装成功。
4. **取消**：发一个会跑一会儿的请求（例如「跑 cargo test 并解释失败原因」），中途按取消；
   agent 应以 `cancelled` 收尾，不残留进程。
5. **重开会话**：Zed 的会话列表里重新打开之前的会话，历史应被重放（`session/load`），
   并且可以继续对话。
6. **成本**：`turn_ended` 里的 usage/cost 与 `[pricing]` 一致（`qc sessions show <id>` 可核对）。

任何一步失败，带上 `RUST_LOG=debug` 重跑一次（Zed 的 agent 环境变量可在 `agent_servers`
里用 `"env": {"RUST_LOG": "debug"}` 传），再看 `<data-dir>/logs/` 下的日志。

## 3. Neovim 与其它客户端

任何 ACP 客户端都可以，配置形态基本一致：给它一个可执行文件与参数。

```lua
-- 以某个 ACP 插件为例：命令是 argv 形式，第一个元素可执行文件、其余是参数
require("your_acp_plugin").setup({
  agents = {
    qcode = { command = { "/abs/path/to/qc", "acp" } },
  },
})
```

插件之间字段名不同（`command` 可能是字符串或 argv，`args` 可能合并进 `command`），
以各自 README 为准。**共同要求只有一条**：把 `qc acp` 作为子进程启动，并且不要捕获/转发
stdout 上的非 JSON 内容——qcode 把日志写到 stderr，stdout 只有协议消息。

## 4. 兼容性测试

官方仓库提供 compatibility kit（`https://agentclientprotocol.com/libraries/testing`）。
用法是让 kit 作为客户端、把被测 agent 的命令传进去；对我们的形态就是：

```bash
# 以 kit 的 README 为准；这里只说明被测命令的形状
<compatibility-kit> --agent "qc acp"        # 或 --command /path/to/qc --args acp
```

本仓库**没有**内置这套 kit（它需要单独下载，且当前开发机上不可运行），所以 CI 里跑的是
本仓库自己的测试：

- `crates/qcode-acp`：9 个单测 + 5 个集成测试（假客户端，覆盖事件映射、权限联合、会话重放）；
- `crates/qcode/tests/acp_stdio.rs`：**启动真实 `qc acp` 进程**，走完
  `initialize → session/new → session/prompt`，并验证工具调用会先发
  `session/request_permission`、拒绝后工具确实没执行。

跑法：

```bash
cargo test -p qcode-acp
cargo test -p qcode --test acp_stdio
```

## 5. 排错

| 现象 | 先看什么 |
|---|---|
| 编辑器报 agent 起不来 | 手动跑 `qc acp` 喂一行 `initialize`；确认 `qc` 在 PATH 或给绝对路径 |
| 一直转圈没有输出 | 是否把 stdout 重定向进了日志；`RUST_LOG=debug qc acp` 的日志在 stderr |
| 每次都弹权限 | 在配置里加 `[[approval.rules]]`（按工具名或命令前缀），或客户端缓存 `allow_always` |
| 工具全部失败 | `qc doctor` 检查配置/provider；`run_shell` 在 Windows 上需要 pwsh 或 cmd 在 PATH |
| 上下文被压缩得很早 | `[context] window_tokens` 按模型实际窗口改 |
| 会话没有历史 | 数据目录被沙箱限制时 `qc` 会回落到工作区 `.qcode-data`，`qc doctor` 会显示实际路径 |
