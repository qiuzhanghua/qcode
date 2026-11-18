# 沙箱后端对比：Linux 的 Landlock 与 bwrap

> 状态：说明文档（不是决策记录）。决策见 ADR-0010（sandbox 抽象）、ADR-0011（bwrap）、
> ADR-0012（Landlock）、ADR-0048（多根与路径能力）、ADR-0066（MCP 授权）。
> 实现：`crates/qcode-tools/src/sandbox/{linux,macos,mod}.rs`、`windows_acl.rs`。
> 本文只负责把「四个后端各自能表达什么、为什么不同」讲清楚，不重复 ADR 的理由。

## 1. 一句话区别

**Landlock 是内核的 LSM 强制访问控制，只做减权；bwrap 是用户态建命名空间，换掉一整个
文件系统视图。**

前者在一组「允许的动作」上做减法，进程仍然活在宿主的世界里；后者把进程放进新建的 mount /
pid / net 命名空间，重新拼一个根。所以 Landlock 拿不到的东西（私有 `/tmp`、私有 devpts、
网络切断），bwrap 都有；bwrap 要的东西（外部二进制、unprivileged userns），Landlock 都不要。

## 2. 机制对照

| | Landlock | bwrap |
|---|---|---|
| 本质 | Linux LSM（`landlock_*` syscall 444/445/446） | 用户态程序，`clone` 出新命名空间后 `mount` 重排 |
| 生效方式 | `PR_SET_NO_NEW_PRIVS` + `landlock_restrict_self()`，对**当前进程**不可逆降权 | 进程从出生起就在新命名空间里 |
| 规则怎么装 | 父进程建 ruleset，子进程在 `pre_exec` 里应用 → 只限制 `run_shell` 及其后代，qc 本身不受限 | 参数拼好后 `exec bwrap`，被沙箱的是 bwrap 的子进程 |
| 依赖 | 内核 ≥ 5.13 且 LSM 启用（`lsm=` 里含 landlock） | `bwrap` 二进制 + unprivileged user namespaces |
| 需要 root | 否 | 否（靠 `--unshare-user`） |
| 命名空间 | **无** | user / pid / ipc / uts / cgroup-try，可选 net |
| 网络 | **不管**（ADR-0012 §5） | `--unshare-net` 可切断 |
| `/tmp` | 宿主 `/tmp`（无私有 tmpfs） | 私有 tmpfs（workspace 在 `/tmp` 下时自动跳过） |
| PTY | 只能给宿主的 devpts，且有越狱风险（见 §4） | `--dev` 挂 `newinstance` 私有 devpts |
| 被拒时的表现 | 路径**存在**，操作返回 `EACCES` | 路径**不存在** |

两条容易记错的：

- **Landlock 的降权不可撤销、且对 fork 出的子进程继承。** 所以它必须装在 `pre_exec` 里，
  装早了会把 qc 自己也锁住。
- **bwrap 的错误常常发生在 preflight 而不是运行时。** 缺二进制、userns 被 AppArmor 挡住，
  都在「起一个 `/bin/true`」时就失败，`qc` 直接拒绝启动而不是静默降级。

## 3. 能力矩阵（四个后端）

| | `landlock` | `bwrap` | `sandbox-exec` (macOS) | `windows-acl` |
|---|---|---|---|---|
| 限制读 | ✅ | ✅ | ✅ | ❌（读不限制） |
| 限制写 | ✅ | ✅ | ✅ | ✅ |
| 限制网络 | ❌ | ✅ | ✅ | ❌ |
| 私有 `/tmp` | ❌ | ✅ | 部分 | ❌ |
| 私有 PTY | ❌（只能宿主 devpts） | ✅ | — | ❌ |
| 外部依赖 | 无 | `bwrap` | 系统自带 | 无 |
| 需额外系统设置 | 内核 ≥5.13 | Ubuntu 24.04 需放开 userns | 无 | 无 |
| qcode 里的自称（`qc doctor` 输出） | `Landlock filesystem confinement (network not restricted, …)` | `bubblewrap (network allowed/denied, …)` | `sandbox-exec (network denied/allowed, …)` | **`windows-acl (partial)`** |

`windows-acl` 自称 `partial` 是有意的：它只做**写**限制（受限令牌 + 常驻 ACE），读与网络
都不管，硬链接与命名管道也不在覆盖范围。它的授权是常驻的，`qc sandbox grant/revoke` 可
提前铺好或逐对象撤销。

## 4. 两处具体差异，值得单独讲

### 4.1 PTY：Landlock 的授权带着一条越狱通道

bwrap 的 `--dev` 挂的是 `newinstance,ptmxmode=0666,mode=0620` 的**新** devpts，沙箱内分配
的 `/dev/pts/N` 对宿主机任何进程都不可见，给 PTY 是免费的。

Landlock 没有 mount namespace，能给的只有**宿主的** devpts。而 `/dev/pts/N` 上你其它终端
窗口的节点属于同一个 uid——于是这个授权等于「可以往一个未受限的 shell 里输入」，是一条从
沙箱里出去的完整通道。所以：

- `[sandbox] allow_pty` 默认 `false`，是个显式开关；
- 关掉时，任何 `openpty` 都会 `EACCES`。实测本仓库 `qcode-terminal` 的集成测试 23/23 全
  挂在 `/dev/ptmx` 上——**设备节点本身是 0666，所以拒绝只可能来自 Landlock**；
- 开启时 `IOCTL_DEV` 必须一并给（终端 resize 走 `ioctl(TIOCSWINSZ)`，ABI 5+ 归它管），
  `allow_pty` 已经包含这点。

**有条件就用 bwrap**：同样是跑终端程序，bwrap 不把宿主 devpts 划进沙箱。

### 4.2 `/tmp`：Landlock 给的是宿主的

Landlock 没有「私有 tmpfs」这个技巧，授权 `/tmp` 就是授权**宿主**的 `/tmp`。这也是为什么
`OutsideRoots::Nothing`（MCP server 用）曾经出过问题：一个第三方 server 能写 `/tmp/anything`，
在装了 Rust 的机器上还能改 `~/.cargo/config.toml`（其中的 `[build] rustc-wrapper` 是下一次
`cargo build` 的任意代码执行）。

bwrap 侧则是 `--tmpfs /tmp`，进程退出即消失；但**当 workspace 本身在 `/tmp` 下时必须跳过**
——先挂 tmpfs 会把 bind 源路径盖掉。这一判断在 `bwrap_args` 里是显式的（`touches_tmp`）。

## 5. 两者的联系：一套权限语义，两个落地手段

后端之间**共享同一个「谁能读、谁能写」的模型**，不是各写一套：

- `extra_read` / `extra_writable` 的校验是共用的（`linux.rs` 里的 `validate`，注释原文：
  「Shared by `Bwrap` and `Landlock` rather than written twice: the two would drift into
  'bubblewrap refuses `/` but landlock grants it'」）。`/`、`/home`、`/Users`、`/root`、
  home 本身在**加载期**就拒绝，两个后端一起拒。
- 同一套 `Sandbox` trait：`describe()` / `command()` / `check()`。
- 同一个 `OutsideRoots` 枚举决定「除 roots 外还额外给什么」（`BuildCaches` 给
  `run_shell`，`Nothing` 给 MCP server）。
- 都做 preflight，失败即拒绝启动，不静默降级。

所以选后端**只影响隔离强度，不影响授权语义**——同一份配置换后端，能读能写的目录集合是一样的
（差异只在 §3 那几项：网络、`/tmp`、PTY）。

## 6. 怎么选

| 你的处境 | 选 |
|---|---|
| 要挡住网络 | `bwrap`（Landlock 做不到） |
| 要跑终端类程序（`vim`、pty 测试） | `bwrap`；只有 Landlock 可用时才开 `allow_pty` |
| 环境里没有 `bwrap`、或 userns 被禁（CI、精简容器、Ubuntu 24.04 默认） | `landlock` |
| 需要能写宿主 `/tmp` | `landlock`（bwrap 默认给私有 tmpfs） |
| 内核 < 5.13 | 只能 `bwrap`，或 `mode = "none"` 靠审批兜底 |
| 要最省事、无外部依赖 | `landlock` |

`mode = "none"` 时沙箱完全不设防，**审批是唯一边界**——这不是「弱一点的沙箱」，是「没有
沙箱」，`qc doctor` 会如实这么写。

## 7. 本机（2026-11-18）实测

```
bwrap:   /usr/bin/bwrap        已安装
内核:    Landlock 可用（ABI 支持 pty 的 IOCTL_DEV）
生效:    Landlock filesystem confinement (network not restricted,
         writes: roots + build caches + host /tmp)
配置:    [sandbox] mode = "landlock", allow_pty = true, network = "off"
```

两点值得注意：

1. **`network = "off"` 在 Landlock 上不生效。** 代码里是 `let _ = network;` 显式忽略。
   网络实际由 `[network] mode = "proxy"` 那套出口代管（本机 `allow = ["api.deepseek.com:443"]`）。
   **别把 `[sandbox] network` 当成网络隔离**——在 Landlock 下它没有任何作用。
2. **`bwrap` 装了也未必跑得起来。** 本机直接验证：

   ```
   $ bwrap --unshare-user --unshare-net ...
   bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted
   ```

   这正是 ADR-0011 记的 Ubuntu 24.04 默认限制 unprivileged userns。装了二进制不等于可用，
   所以 qcode 的 preflight 有意义：它在启动时就测，而不是等你第一条命令失败。

## 8. 一句话总结

**bwrap 的能力是 Landlock 的超集，代价是外部依赖与 userns 权限；Landlock 是内核原生的最小
可用子集，管得住文件，管不住网络、`/tmp` 与设备。** 授权语义两者共享，切换只改变隔离强度。
