# Skills over MCP — 开发计划（ADR-0108）

> 依据：[ADR-0108](./DECISIONS.md#adr-0108skills-走-mcp远程索引按服务器-opt-in推翻-adr-0004-的不做远程安装2026-11-26)
> 与 [SKILLS.md](./SKILLS.md)。本文只排**还没做**的事，P0 已完成的部分见 §0。
> 状态截至 2026-11-28。

## 0. 现在在哪

P0 已落地（提交 `7590f6c feat(skills): MCP 服务器贡献 skill 索引（ADR-0108 P0）`），
`cargo test --workspace`（143 个 target）与 i18n 门禁全绿。已经能用的：

| 能力 | 位置 |
|---|---|
| 读 `extensions` 判 `supports_skills`，只对声明了的 server 发 `skills/list` | `qcode-mcp/src/lib.rs` |
| 连接一次同时喂工具表与 skill 索引（索引先于工具表） | `qcode-kernel/src/lib.rs` `connect_mcp` |
| `SkillSource::Mcp(<server>)` + 模型可见的 `mcp:<server>`；同名落 `Lookup::Ambiguous` | `qcode-tools/src/skills.rs` |
| 正文 `skills/get` → `resources/read` → size + SHA-256 + frontmatter 逐字段比对 | 同上 |
| 512 resource / 16 MiB 在**索引时**就拒；`dynamic` 拒载；未列文件不发请求 | 同上 |
| 配置双闸门 `[skills] mcp` / `[[mcp.servers]] skills` | `qcode-kernel/src/config/` |
| `qc doctor --probe` 报「声明了但没索引」 | `qcode/src/doctor.rs` |
| 测试 | `qcode-kernel/tests/mcp_skills.rs`(5)、`mcp_connect_once.rs`(2)、skills 单测 40 条 |

### P0 还差两个口子（ADR §6 第 1、3 项）——**已补，2026-11-28**

- **MCP 服务器表格没有 `skills` 列**。`setup/save.rs:1026` 目前靠「保存时沿用旧值」避免
  丢配置，注释里也写着「ADR-0108 §6 adds one in M8」——用户在界面上**改不了**。
- **`qc skills list` 没有那句提示**。ADR §7 的代价是「开了开关却看不到，会以为没生效」，
  对价就是末尾一行「另有 N 个服务器配置为提供 skills，见 `qc doctor`」。

## 1. 任务清单

### A. P0 收尾（小，先做）

| 编号 | 任务 | 改动点 | 验收 | 规模 |
|---|---|---|---|---|
| **A1** ✅ | MCP 服务器表格加 `skills` 列 | `gui/src/forms.rs` 追加列与选项表、`setup/tables.rs` 播种、`setup/save.rs` 三态映射、i18n `gui.setup.spec.mcp.servers.col.skills`（双语）。**没有用 `FieldKind::Bool`**：`Option<bool>` 需要「inherit/on/off」三态，跟 `sandbox` 列同一个形状 | `cargo test -p qcode-gui --test spec_i18n`（4 条）+ `setup::tests`（新增三态映射测试）；golden 不受影响——它只覆盖 CLI `--help` | S（~50 行 + 1 键） |
| **A2** ✅ | `qc skills list` 末尾提示 | `qcode/src/commands/misc.rs` `cmd_skills`：统计「配置为提供 skills 的服务器数」，非零时打一行；i18n `cli.skills.mcp_hint`（双语） | `crates/qcode/tests/skills_list.rs::points_at_servers_that_would_contribute_skills` | S（~20 行 + 1 键） |

### B. 独立决策：统一执行闸门（阻断 SEP-2640 合规声明）

ADR-0108「推翻三」把这件事从 skills 里摘出来了：`run_shell` 是
`ApprovalClass::Shell` → **默认 `Allow`**（`qcode-core/src/policy.rs:431`，还有一条测试
断言它不弹窗），`web_fetch` 的 `Network` 同理（`policy.rs:429`，与 ADR-0005 §2 的
「默认 ask」已经分叉）。在统一闸门存在之前，qcode **不能**声称满足
`host-exec-gate-while-acting` 与 `host-no-implicit-local-execution`。

| 编号 | 任务 | 产出 | 验收 |
|---|---|---|---|
| **B1** ✅ | 执行面的安全模型 | ADR-0109（Accepted）：**不加逐次审批**；边界由 MCP 沙箱 + grants、宿主沙箱、网络 allowlist、skill 摘要绑定负责；无人值守照常运行 | 已写入 `docs/design/DECISIONS.md`；ADR-0108「推翻三」的措辞已按它修订 | — |
| **B2** ✅ | 把边界写进文档与诊断 | README 的 Skills 一节、`SKILLS.md` §7、`qc doctor` 的 `sandbox_none_boundary` 都改成「边界 = MCP 沙箱 + grants + 宿主沙箱 + 网络 allowlist」，并点明 `[sandbox] mode = "none"` 时宿主侧没有边界 | i18n 门禁 12/12；`doctor_mcp` 的 25 条通过（第 26 条是本机沙箱环境的已知用例）；不改 `policy.rs` | S |

> 顺序上 B 不阻塞 C/D：只有「对外宣称合规」这一条依赖它。

### C. P1：`list_changed` 热更新

ADR §2 记了成本：`SkillSet` 现在是不可变 `Arc`，而 `prompt_section()` 有**七处调用、
五个文件**，结果都被缓存成 `String`（`qcd/src/lib.rs:184,216`、
`qcode/src/commands/run.rs:152`、`gui.rs:94,481,541`、`misc.rs:72`）。热更新要穿透这六处，并且碰一个会话假设：**压缩动的是
工具结果，不是 system prompt**——索引中途变长，前半段与会话后半段的上下文会不一致。

| 编号 | 任务 | 做法 | 验收 | 规模 |
|---|---|---|---|---|
| **C1** ✅ | `SkillIndex` 句柄 | `Arc<SkillSet>` → `Arc<SkillIndex>`：内部 `RwLock<SkillSet>` + `AtomicU64 revision`；新增 `section()` / `replace_served()` / owned 的 `resolve()`；`prompt_section()` 保留给 C2 之前的调用点 | 单测：替换只动目标服务器、revision 递增、替换同样受 limits 约束 | M（~260 行） |
| **C2** ✅ | 前端不再缓存渲染好的字符串 | `qcd`、`qc run`、TUI、GUI、ACP 五处都改成**每回合从索引读**（`SkillIndex::prompt_section()`）。比「存 (revision, String)」更简单：一次小字符串构造，永远最新 | 五处编译通过 + 全量测试；TTL 测试断言 prompt 里出现新 skill | M（~90 行，7 个文件） |
| **C3** ✅ | **改为 TTL 驱动**（`list_changed` 在 SEP-2640 与 rmcp 3.9.0 里都不存在，见 ADR §2 勘误） | `qcode-mcp` 的 `list_skills` 返回 `ServerSkillListing`（含 `ttl_ms`）；kernel 的 `spawn_skill_refresh` 按 TTL 重列该 server 并 `replace_served`，失败保留旧条目 | stub 支持 `MCP_STUB_SKILL_TTL_MS` / `MCP_STUB_SKILL_ROTATE`；`mcp_skills.rs::a_servers_ttl_refresh_reaches_the_index` | M（~150 行 + 1 个测试） |
| **C4** ✅ | 会话一致性验证 | `qcode-providers/tests/skill_index_turns.rs`：两轮真实 `run_turn`，中间 `replace_served` 加一个 skill；用 `MockProvider.requests()` 断言第 1 轮 system prompt 没有它、第 2 轮有 | `cargo test -p qcode-providers --test skill_index_turns` | S |

### D. P1：第三方内容的来源徽章（要动 schema）

ADR §5/§6 第 4 项：P0 明确接受「这条防护对人不可见」。要可见就得让
`ToolEntry` 带上来源，而那是**会话日志 schema**（今天 `EVENT_SCHEMA_VERSION = 12`，
`qcode-core/src/event.rs:48`）。

| 编号 | 任务 | 做法 | 验收 | 规模 |
|---|---|---|---|---|
| **D1** | schema v13：工具结果带来源 | `ToolEntry` 增 `source: Option<String>`（`skill` 的 `mcp:<server>` 或本地来源），旧事件读成 `None` | schema 兼容测试（v12 事件仍可读）、`event_schema.rs` 更新 | M（~120 行） |
| **D2** | 渲染 + 文案 | TUI/GUI 的工具卡把来源显示出来（`render/transcript.rs`），i18n 2 条 | 渲染快照/断言；golden 重录 | S（~60 行） |
| **D3** | 回放与旧会话 | 旧事件没有来源时不显示徽章、不伪装成本地 | 回放测试 | S |

### E. P2（记录，不做）

| 项 | 为什么不做 |
|---|---|
| `directory_read`（`capabilities.rs:269`、`DIRECTORY_MIME_TYPE`） | 本地 skill 的脚本今天也不执行（ADR-0004 §3），远程的不该比本地更宽松 |
| 捆绑脚本 / `allowed-tools` 生效 | 同上；且 `host-ignore-allowed-tools` 要求继续忽略 |
| 磁盘缓存 | 绕开 `host-cache-*` 整组要求；内存里按 entry digest 验已经够用 |

### F. 观察项（不排期，验收依赖它）

| 项 | 内容 | 怎么盯 |
|---|---|---|
| F1 | 有没有**已发布**的 server 在服务 skills | `github-mcp-server#3046` 是否合并并发布；`skills.extensions.modelcontextprotocol.io` 的 stable 目录是否回来 |
| F2 | 扩展 Beta → stable | 同一规格页与 SEP 列表 |
| F3 | 用 conformance 的客户端场景当回归 | `conformance#330` 的 `capabilities` / `no-prefetch` / `verification` 三个场景，逐条对到 `mcp_skills.rs` 的断言上 |

## 2. 顺序、依赖与红线

```
A1, A2 ✅ ──► B1（决策）──► B2（仅当 B1 选 b/c）
   └──► C1 ✅ ──► C2 ✅ ──► C3 ✅（改 TTL）──► C4
                 └──► D1 ──► D2 ──► D3
```

- **A 先做**：两个小口子把 P0 真正闭合；A1 的 GUI spec 变更最好赶在 C/D 之前，
  免得与后面动 schema 的提交混在一起。
- **C 与 D 互不依赖**，可以并行；但 D 动 schema，建议放在 C 之后，避免 schema 与索引
  语义在同一轮里一起变。
- **B 只挡「合规声明」**，不挡功能。若近期不打算对外宣称 SEP-2640，B1 可以只写 ADR、
  不改代码。
- 红线（沿用 ADR，任何一项破掉都要回到 ADR 重新决策）：
  1. 不加 skills 专属审批闸门；授权只发生在 allowlist 一个地方。
  2. 默认配置下 prompt 与改动前逐字节一致（`mcp = false` 时不发 `skills/list`）。
  3. 不落磁盘缓存；跨服务器读不做。
  4. 不声称 SEP-2640 host 合规，直到 B 落地。

## 3. 每个任务的验收门禁

- `cargo fmt --all --check`、`clippy --workspace --all-targets -D warnings`、全量测试
  （本机跳过的 4 条理由见 `docs/DEVELOPMENT.md`）。
- i18n：`cargo test -p qcode-i18n --test gates`（12 条）+ 双语 golden 88 项。
- 新增用户可见文案一律双语，工具名/参数名/`mcp:<server>` 不翻。
- A1 会改 GUI spec（由 `crates/qcode-gui/tests/spec_i18n.rs` 盯）；D2 才需要重录 golden。
