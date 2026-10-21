# 10 万事件会话性能基线

> v2.2 性能验收。基线日期：2026-10-21。实现：`crates/qcode-core/tests/session_perf.rs`、
> `crates/qcode-ui-state/tests/replay_perf.rs`，入口 `make perf`。

## 怎么跑

```bash
make perf
```

两个测试都标了 `#[ignore]`，普通 `cargo test` 不会跑；`make perf` 只跑这两个，并把
耗时打印出来。测试会生成约 16.6 MiB 的临时 JSONL 会话，结束后自动删除。

## Fixture

- 100,001 个事件 = `session_started` + 25,000 轮 × 4 个事件
  （`turn_started` / `user_message` / `assistant_message` / `turn_ended`）。
- 投影后 50,000 个 transcript block。
- 每个 turn 的文本固定为 `question N` / `answer N`，不依赖网络。

## 基线环境

| 项 | 值 |
|---|---|
| 机器 | Apple M1 Max |
| 系统 | macOS 27.0.1 (26A434) |
| 工具链 | rustc 1.99.1 / cargo 1.99.1 |
| profile | debug（`cargo test` 默认） |
| 采集时间 | 2026-10-21 |

## 结果

| 指标 | 基线 | 测试上限 | 说明 |
|---|---:|---:|---|
| `SessionStore::load` | **570 ms** | 5 s | 读取 16.6 MiB、逐行 JSON 解析、seq 校验 |
| `SessionStore::list`（首次，重建索引） | **579 ms** | 15 s | 索引缺失或过期时扫描日志 |
| `SessionStore::list`（索引命中） | **210 µs** | 1 s | `qc sessions list` 的日常路径 |
| `App::apply` 回放（TUI/GUI 打开成本） | **4.5 ms** | 5 s | 只做投影，不含终端绘制/窗口初始化 |

`qc sessions list` 使用索引命中路径；进程启动、参数解析和打印不在这张表里。TUI 的实际
绘制由 ratatui 虚拟化，占比远小于投影；这里测的是 resume/open 的主要 CPU 成本。

## 解读

- 首次 `list` 的 579 ms 是**一次性**的：索引文件不存在或会话数与索引不一致时才扫描
  重建；之后是 210 µs。
- `load` 是 resume 的主要成本；100k 事件 570 ms 仍远低于交互可感知的 1–2 s 预算。
- 回放 4.5 ms 说明 100k 事件本身不是 TUI 打开的瓶颈；如果实际 resume 慢，先查
  `load`、终端绘制和模型请求，而不是投影。
- 上限是数量级保护，不是精确 SLA；CI 不跑这两个测试，避免机器差异造成抖动。

## 回归规则

1. 在**同一台机器**、同一 profile 下重跑；与上表相比超过 2× 需要查因。
2. `SessionStore::list` 索引命中超过 1 s，或 `load` 超过 5 s，视为回归。
3. 改动 `SessionStore`、事件 schema 或 `qcode-ui-state` 投影后，至少跑一次 `make perf`。
4. 如果新增索引/缓存策略，同时记录 `EXPLAIN QUERY PLAN`，避免把全表扫描带回来。

## 未覆盖

- release profile 的数字（通常更低）。
- 真实终端/window 的绘制、字体与滚动帧率。
- 100k 事件下的内存峰值。
- `qc sessions list` 的完整进程 wall time（可后续用 `/usr/bin/time` 补）。
