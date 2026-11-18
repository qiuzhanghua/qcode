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

> 「限制网络」指的是**机制上的强制**，不是「由代理或审批兜底」。`landlock` 与 `windows-acl`
> 都没有任何网络机制；`[network]` 那套出口代理只给子进程设 `HTTP_PROXY` 等环境变量，无视
> 它们的程序照样出得去。详见 §7。

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
| 要**强制**挡住网络 | `bwrap`（Landlock 做不到；代理只是引流，见 §7） |
| 只想让流量走 allowlist 代理 | 任何后端都行，但必须**不写** `[sandbox] network`，否则代理可能根本不起（§7.3） |
| 要跑终端类程序（`vim`、pty 测试） | `bwrap`；只有 Landlock 可用时才开 `allow_pty` |
| 环境里没有 `bwrap`、或 userns 被禁（CI、精简容器、Ubuntu 24.04 默认） | `landlock` |
| 需要能写宿主 `/tmp` | `landlock`（bwrap 默认给私有 tmpfs） |
| 内核 < 5.13 | 只能 `bwrap`，或 `mode = "none"` 靠审批兜底 |
| 要最省事、无外部依赖 | `landlock` |

`mode = "none"` 时沙箱完全不设防，**审批是唯一边界**——这不是「弱一点的沙箱」，是「没有
沙箱」，`qc doctor` 会如实这么写。

## 7. 网络：两层配置，且容易配成「两边都失效」

这一节讲的是最容易踩的坑。**`[sandbox] network` 与 `[network]` 是两个不同的东西**，而且前者
会影响后者是否生效。

### 7.1 两层各管什么

| | `[sandbox] network` | `[network]` |
|---|---|---|
| 是什么 | **策略意图**：这次运行该不该联网 | **出口代理**的机制参数 |
| 取值 | `off` / `proxy` / `strict` / `open` | `mode` / `enabled` / `allow` / `allow_private` / `unknown` / `audit` |
| 谁能执行 | 只有 `bwrap`（`--unshare-net`）与 `sandbox-exec`（Seatbelt） | 与后端无关，纯用户态 |
| 在 `landlock` 下 | **被丢弃**（`sandbox/mod.rs` 里 `let _ = network;`） | 仍可用 |

Landlock 没有网络命名空间，也没有 socket 过滤，所以它拿不到「不许联网」这件事。给它这一项
不会报错，只会没有效果。

### 7.2 优先级：`[sandbox] network` 一旦写了就压过 `[network] mode`

`config/mod.rs` 的 `network_mode_with`：

```rust
if let Some(mode) = self.network { return mode; }   // [sandbox] network 直接返回
match network.mode { /* [network] mode 走不到 */ }
```

它算出的 `NetworkPlan { proxy, strict, raw }` 决定三件事，其中 `proxy` 是**代理要不要启动**。

### 7.3 于是 `[sandbox] network = "off"` + `mode = "landlock"` 等于三者全失效

```
network = "off"  →  NetworkPlan { proxy: false }  →  build_egress() 立刻 return Ok(None)
                 →  代理根本没起  →  allow / unknown / audit 全部无效
mode = "landlock"→  `let _ = network;`            →  OS 层也不拦
```

**结果：网络完全敞开，既没有代理也没有 OS 限制**，而配置看起来像配了 allowlist。

这个组合不会报错，`qc doctor` 的连通性检查也照样打勾——因为它走的是同一条 `build_egress`，
拿到 `None` 就直接连 provider。唯一能看出问题的迹象是 `logs/network.jsonl` 不再增长。

### 7.4 代理的具体形态

代理不是固定端口，每个会话现开（`qcode-net/src/lib.rs`）：

```rust
let listener = TcpListener::bind(("127.0.0.1", 0)).await?;   // 端口 0 = 内核分配
let token = Ulid::new().to_string();
let proxy_url = format!("http://qcode:{token}@127.0.0.1:{port}");
```

注入给 `run_shell` 子进程的四个变量（`Network::env`）：

```sh
HTTP_PROXY=http://qcode:<ulid>@127.0.0.1:<port>
HTTPS_PROXY=<同上>
ALL_PROXY=<同上>
NO_PROXY=localhost,127.0.0.1,::1
```

- token 是随机 ULID，防同机其他进程盗用这个代理；
- `[network] allow` 的条目加上 **provider 的 host 自动追加**（base_url 解析出来的
  `host:port`），所以模型端点通常不用手写；
- `unknown = "ask"` 在代理层被当成 **deny**——代理无法弹框，这是 `build_egress` 里显式映射的；
- 放行/拒绝逐条写入 `logs/network.jsonl`，`reason` 例如 `not-allowlisted`。

### 7.5 必须记住的边界：代理只是「引流」，不是强制

即使代理正常启动，**Landlock 下它也只是给子进程设了环境变量**。无视 `HTTP_PROXY` 的程序
（裸 socket、`curl --noproxy`、直连 IP、Go 的部分默认行为）照样出得去。真正能强制只有
`bwrap --unshare-net` 与 `sandbox-exec` 的 Seatbelt profile。

这也是 ADR-0012 §5「网络不限制」的实际含义：**不是「由审批代替」，而是「没有机制」。**

### 7.6 怎么配才对

| 目标 | 配法 |
|---|---|
| 要代理生效，Landlock 下够用就好 | **删掉** `[sandbox] network`，让 `[network] mode = "proxy"` 派生 |
| 要真的断网 | `mode = "bwrap"`（无 `--unshare-net` 以外的余地）；Landlock 做不到 |
| macOS 硬模式（只能连代理端口） | `[sandbox] network = "strict"` + `mode = "sandbox-exec"`，两者缺一即报错 |
| 要直连、不要代理 | `[sandbox] network = "open"` |

推荐写法（不写 `[sandbox] network`，由 `[network]` 接管）：

```toml
[sandbox]
mode = "landlock"
allow_pty = true

[network]
mode = "proxy"
enabled = true
allow = ["api.deepseek.com:443"]
audit = true
```

## 8. 本机（2026-11-18）实测

```
bwrap:   /usr/bin/bwrap        已安装，但跑不起来（见第 2 点）
内核:    Landlock 可用（ABI 支持 pty 的 IOCTL_DEV）
生效:    Landlock filesystem confinement (network not restricted,
         writes: roots + build caches + host /tmp)
配置:    [sandbox] mode = "landlock", allow_pty = true, network = "off"
         [network] mode = "proxy", enabled = true, allow = ["api.deepseek.com:443"]
```

这台机器正好是 §7.3 那个组合的实例，两点实测：

1. **`network = "off"` 让代理根本没启动，而不是「由代理代管」。** 判据是审计日志：

   ```
   logs/network.jsonl 最后写入   2026-11-17 10:06
   检查时间                      2026-11-18 08:16   （22 小时内 0 条新记录）
   ```

   同时 `qc doctor` 的 `connectivity` 仍然打勾——因为它走同一条 `build_egress`，拿到 `None`
   后直连 provider。**「能连上 provider」证明不了代理在工作。**
   （这条是复查时发现的：初稿写「实际由 `[network]` 那套代管」，与代码不符——
   `[sandbox] network` 一旦写了，`[network] mode` 根本走不到。）

2. **`bwrap` 装了也未必跑得起来。** 本机直接验证：

   ```
   $ bwrap --unshare-user --unshare-net ...
   bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted
   ```

   这正是 ADR-0011 记的 Ubuntu 24.04 默认限制 unprivileged userns。装了二进制不等于可用，
   所以 qcode 的 preflight 有意义：它在启动时就测，而不是等你第一条命令失败。
   这也意味着这台机器**想真正断网没有可用手段**——Landlock 管不了，bwrap 起不来。

## 9. 一句话总结

**bwrap 的能力是 Landlock 的超集，代价是外部依赖与 userns 权限；Landlock 是内核原生的最小
可用子集，管得住文件，管不住网络、`/tmp` 与设备。** 授权语义两者共享，切换只改变隔离强度。

网络这层要单独记一句：**`[sandbox] network` 是意图、`[network]` 是机制，前者压过后者；
在 Landlock 上前者是空转，却仍会决定代理启不启动。** 配错的表现是「什么都不报错，但什么都没
生效」，唯一的可观察迹象是 `logs/network.jsonl` 不再增长。
