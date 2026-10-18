# qcode — 评测方案（EVAL）

> 决策背景见 [DECISIONS.md](./DECISIONS.md) §4.8。目标是让“改 prompt / 换模型 / 改工具”
> 都有可比较的数字，而不是凭感觉。

## 1. 原则

- **离线**：fixture 零外部依赖，构建与测试不联网。
- **确定性**：每个任务从固定快照开始，环境变量清洗，超时固定。
- **客观**：判据只有命令退出码与隐藏测试，不做主观评分。
- **防作弊**：隐藏测试在 agent 结束后才注入；测试/脚本在检查前恢复原样。
- **快**：单任务 `cargo test` < 5 秒，整套 < 2 分钟。

## 2. Fixture

```text
eval/fixtures/workspace/
  Cargo.toml                 # [workspace] members = ["core", "store", "cli"]
  core/src/lib.rs            # 纯逻辑：行切分、区间、UTF-8 边界
  store/src/lib.rs           # 键值存储：原子写、路径校验
  cli/src/main.rs            # 参数解析、输出
  cli/tests/integration.rs   # 可见的集成测试
  scripts/ci.sh              # fmt + clippy -D warnings + test
  AGENTS.md                  # 项目约定（测试命令、风格、禁止事项）
```

只用 std，不引入任何 crate。三个 crate 的目的是制造真实的跨文件导航，
而不是把逻辑塞进一个文件。

## 3. 任务定义

```toml
id = "01-fix-bug"
prompt = "..."
snapshot = "v1"              # fixture 快照标签
timeout_secs = 600
max_turns = 40
allowed_tools = ["read_file", "edit_file", "run_shell"]

[[checks]]
cmd = "cargo test -q"
expect_exit = 0
```

- `snapshot`：fixture 在该标签下的 git 状态；运行时复制一份，不污染源。
- `allowed_tools`：限制工具面，便于定位问题（不是安全边界）。
- `checks`：按顺序执行，任一失败即任务失败。

## 4. 五条任务

| # | 任务 | 植入的问题 | 判据 |
|---|---|---|---|
| 01 | 修 bug | `core::truncate` 在 UTF-8 边界上 off-by-one，可见测试失败 | `cargo test` 全绿 + 隐藏边界测试 |
| 02 | 加功能 | CLI 缺 `--format json` | 隐藏测试校验 JSON schema；原测试无回归 |
| 03 | 重构 | `core` 与 `store` 各有一份重复的规范化逻辑 | 全绿 + `clippy -D warnings` + 结构检查脚本（重复函数消失、调用点更新） |
| 04 | 跨文件 API 改动 | `store::Store` trait 缺 `delete`，调用方未更新 | `cargo build` + 全绿 + 隐藏测试验证 `delete` 语义 |
| 05 | 修 CI | `scripts/ci.sh` 因 planted fmt/clippy 问题失败 | `scripts/ci.sh` 退出 0 |

设计意图：01 考定位，02 考规格理解，03 考不破坏行为，04 考全局一致性，
05 考工程工具链。五条覆盖“读 → 改 → 验证”的完整闭环。

## 5. Runner 协议

1. 校验 fixture 干净（`git status --porcelain` 为空）。
2. 复制 snapshot 到临时目录；在副本里初始化 git。
3. 清洗环境变量（同 SPEC §12 的 shell 清洗规则）。
4. 记录起始指标；以 headless 模式运行 agent：
   `qc run "<prompt>" --json --approval-policy eval/policy.toml --max-turns N --timeout N`。
5. agent 结束后：恢复 `tests/`、`scripts/`、`AGENTS.md` 到快照状态；
   注入隐藏测试；执行 checks。
6. 记录指标与事件日志路径；删除临时目录（失败时保留并打印路径）。

`eval/policy.toml` 固定一条策略：workspace 内读写 allow，shell allow，
workspace 外 deny——保证评测可无人值守运行。

## 6. 指标

| 指标 | 用途 |
|---|---|
| pass@1（五条任务的通过数） | 主指标 |
| turns / tool calls | 效率 |
| input / output tokens、cost_usd | 成本 |
| wall time | 体验 |
| approval 次数 | 交互负担 |
| 事件日志大小 | 存储开销 |

每次跑完输出一张表，附上 commit、模型、prompt 版本、rig 版本。
结果落在 `eval/runs/<timestamp>/summary.md`，不进 git（或只保留汇总）。

## 7. 阶段

- **P1**：本方案，M2 起并行准备，M4 必须可用。
- **P2**：真实仓库——`qrlib`（1k LOC、有测试）或自有中等仓库；任务改为真实 issue。
- **P3**：self-hosting——用 qcode 修 qcode 的 bug、加小功能；这是最终验收。

## 8. 不做什么

- 不做主观打分（“代码优雅”之类）。
- 不做大规模 SWE-bench 式评测（成本高、与自用目标无关）。
- 不把 eval 结果当营销数字——它是回归工具，不是 benchmark 成绩。
