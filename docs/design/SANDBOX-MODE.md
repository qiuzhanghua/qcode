# 命令执行写范围阶梯：沙箱兜底 + 只问提权（run_shell 审批重构）

> 状态：设计草案（尚未定 ADR 编号）。
> 关联：ADR-0064（shell 环境采样）、`docs/design/ASK-USER.md`（ask_user 提问工具）、
> 既有 `ApprovalClass` / `Policy` / `Approver` 链路。
> 起因：`run_shell` 的 `approval()` 不看命令内容一律返回 `ApprovalClass::Execute`，
> 默认策略对 Execute 一律 `Ask`（`crates/qcode-core/src/policy.rs` 默认分支）——于是
> `ls`、`git status` 这类只读命令也每次弹权限框。审批成了命令执行的**唯一闸门**，
> 没有任何"危险度分层"；定时任务因此也会每次卡审批。

## 1. 问题

### 1.1 简单命令也弹框，根因是没有写范围分层

- `RunShell::approval`（`crates/qcode-tools/src/shell.rs:197`）对任何命令返回
  `ApprovalClass::Execute`；默认策略 `Execute → Ask`。
- 点「允许」只放行这一次；点「总是允许」按**工具名**记住整个会话（
  `qcode-ui-bridge` 的 `BridgeApprover::session_allowed`），此后所有 run_shell
  都不再问——粒度是"整个工具"，不是"写范围"。
- 后果一：只读命令与 `rm -rf` 的询问成本相同，弹框失去信息量，用户开始无脑点
  「总是允许」，等于把闸门拆了。
- 后果二：定时任务（无人值守）每次都要等授权；现在靠 `approval_timeout`
  （`approver_with_deadline`，到期自动 Deny）兜底，代价是任务要么被拒要么挂起。

### 1.2 沙箱存在但只承担"后端"角色

`[sandbox] mode`（`crates/qcode-tools/src/sandbox/mod.rs`）是 **OS 后端选择器**
（none / sandbox-exec / bwrap / landlock / windows-acl），不是**写范围阶梯**。
`Sandbox::command` 只负责把命令包进某个后端；后端选 `none` 时 `run_shell` 完全
不设防，审批成为唯一边界——所以"危险度"与"写范围"两个概念目前是混在一起的。

## 2. 决定

### 2.1 写范围阶梯 `WriteScope`：与沙箱后端正交

新增 `WriteScope`（会话级，`read_only / workspace_write / full_access`）：

| 级别 | 含义 | 典型命令 | 默认策略 |
|---|---|---|---|
| `read_only` | 工作区只读、无网络 | `ls` `git status` `grep` `cargo check --no-run` | 直接执行 |
| `workspace_write` | 只写工作区（roots 内） | `cargo build` `git commit` `npm install` | 直接执行 |
| `full_access` | 工作区外写入、网络直连、系统级 | `rm -rf /x` `sudo` `curl \| sh` | 提权审批 |

- 每个 OS 沙箱后端把每个级别**映射或拒绝**（fail-closed）：`sandbox-exec` /
  `bwrap` / `landlock` / `windows-acl` 都能表达 `read_only` 与 `workspace_write`；
  `mode = none` 只能表达 `full_access`。
- 严格加宽阶梯（仿 dsh `WIDER_MODES`）：`read_only → [workspace_write, full_access]`，
  `workspace_write → [full_access]`。请求不能加宽到不连续级别；重复当前级别免审批。

### 2.2 会话级模式：从事件投影，默认来自配置

- 会话模式是 session 状态，由事件投影（同 ask_user 的问题投影思路），不单独落库。
- 新增事件 `SandboxModeChanged { scope }`（写入 `EventKind`，schema v9 → v10 的 additive 变更），
  replay / 跨重启恢复后模式仍在。
- 默认级别来自配置 `[sandbox] write_scope`，缺省 `workspace_write`；`mode = none`
  时强制 `full_access`（行为回到现状：每次都问）——**迁移透明**，现有用户配置不改。

### 2.3 run_shell 执行路径：默认不弹，只问提权

- 新增可选工具参数：`sandbox_permissions`（`read_only / workspace_write / full_access`）
  与 `justification`（提权理由，必填），仿 dsh 的 `sandbox_permissions + justification`。
- **无提权参数**：按会话当前模式执行，**不弹审批**（`read_only` / `workspace_write`
  直接跑；会话已是 `full_access` 也不弹——会话级授权已给出）。
- **有提权参数且宽于当前模式**：进入审批。`ApprovalRequest` 同时携带
  `current_scope` 与 `requested_scope`，UI 展示"从 workspace_write 提升到
  full_access + justification"。
  - 允许（一次性）：本次调用按请求级别执行；
  - 允许并记住（`AllowSession` 语义改为"提升会话模式到该级别"）：写
    `SandboxModeChanged`，此后会话内该级别免审；
  - 拒绝：工具返回错误 + 提权提示标记（模型下次可换策略）。
- 审批分类改造：`run_shell.approval()` 看参数——
  - 无 `sandbox_permissions` → 新类 `ApprovalClass::Shell`，默认策略 `Allow`；
  - 有提权参数 → 新类 `ApprovalClass::Escalation`，默认策略 `Ask`。
  默认策略不再对 Execute 一刀切 `Ask`；配置 `[[rules]]` 仍可 override（如
  `command_prefix = "git push"` → ask/deny 保持现状能力）。

### 2.4 模型可见策略

系统提示注入两段固定文案（仿 dsh `approval:policy` / `sandbox:policy`）：

- 当前会话写范围 + 各级别含义 + "需要更高权限时用 `sandbox_permissions` +
  `justification` 申请，不要直接尝试越界命令"；
- 被拒后 `justification` 不被采纳时，提示"换一种不越界的方式，或重新申请"。
  模型能看到策略是降低弹框率的另一半：能预判哪些命令会触发审批，从而主动降级。

### 2.5 定时任务（无人值守）

- 定时任务目标会话创建时钉入写范围预设：默认 `workspace_write`（无需审批）。
- 配置 `[schedules.<name>]` 可声明 `write_scope`（覆盖默认）；需要 `full_access`
  的定时任务在配置里显式声明，无人应答时 fail-closed（拒绝并记录），不等待。
- 保留 `approval_timeout` 兜底（现有 `approver_with_deadline` 行为不变）。
- 与 `qc run --yes`（AutoApprove）语义对齐：`--yes` 仍全部放行；无头管道输入
  保持 AutoDeny。

### 2.6 与 ask_user 的关系

审批（权限，引擎驱动）与 `ask_user`（信息，模型驱动）两条通道保持独立，互不
抢占；`docs/design/ASK-USER.md` 的通道划分不变。

## 3. 关键设计决策

| 决策 | 理由 |
|---|---|
| 写范围阶梯与沙箱后端正交 | 危险度由"能写哪"表达，不是逐命令分析；dsh 同款思路 |
| 无提权参数一律放行 | 普通命令靠沙箱限制，不再把弹框当唯一闸门 |
| 提权才进审批，且审批可"提升并记住会话" | 把「总是允许」从"按工具"细化为"按写范围"，信息量不丢失 |
| 会话模式从事件投影 | 满足"模型可见 ⟺ 日志可重建"；跨重启恢复 |
| 系统提示注入策略 | 模型能预判哪些命令触发审批，主动降级或申请 |
| 定时任务默认 workspace_write + fail-closed | 无人值守不卡审批；`approval_timeout` 仅兜底 |
| `mode = none` 强制 full_access | 迁移透明：没有沙箱后端的用户行为与现状完全一致 |

## 4. 迁移与兼容

- 现有 `[[rules]]`（tool / command_prefix / path_glob）继续生效，优先级高于默认策略。
- `[sandbox] mode = none`（现状默认）→ 写范围强制 `full_access` → 每次审批，
  现有行为零变化；用户配置了沙箱后端后，体验自动变为"普通命令免审"。
- 现有 `SandboxMode`（后端选择器）保留；新增的 `WriteScope` 是叠加在其上的会话
  语义层。
- 事件 schema v9 → v10：新增 `SandboxModeChanged`，additive，旧日志可回放。

## 5. 已知限制

- 无 OS 沙箱后端（`mode = none`）时阶梯退化为 full_access，等于现状——本设计
  的价值在配置了沙箱后才能真正兑现。
- `read_only` 对"只读命令"的判定仍靠模型自觉（工具不解析命令内容）；误用只会在
  命令实际越界时被沙箱拦下，而不是事前弹框。
- 跨平台的 workspace_write 语义由各后端保证，能力不等（landlock 只限文件系统、
  不挡网络；windows-acl 只做写限制）——`full_access` 才承诺"完全无沙箱"。
- deny 语义仍走 `[[rules]]`，阶梯不替代 deny-list（与 dsh 一致：不做 argv 匹配）。
