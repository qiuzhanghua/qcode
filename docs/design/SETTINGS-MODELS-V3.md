# 模型设置页改版设计文档

> 项目：qcode（GPUI 桌面应用）
> 涉及模块：`crates/qcode-gui/src/setup`（设置窗口 Models 页）
> 版本：v1（需求评审用）
> 状态：待评审

---

## 1. 背景与目标

当前 Models 页（设置窗口 → 模型）展示的是**全部可配置服务商目录**（内置 catalog + 用户手写 route），
每个服务商一张卡片，模型编辑需要点「编辑」进弹窗。用户反馈两条改版诉求：

1. **列表只展示"已配置了模型"的 Provider 和模型**，没有配置模型的不展示；一个 Provider 对应一个**可展开的模型列表**。
2. 新增一个**「添加模型」按钮**：点击后弹出 Provider 大列表选择框（该选择框**只做选择**），点击某个 Provider 后弹出**具体的配置页**。

目标形态：贴近主流 IDE 的"已添加模型列表 + 添加模型向导"交互。

---

## 2. 现状分析

### 2.1 数据模型（config.toml）

- `providers.<route>` 为一个 `ProviderConfig`（[config/mod.rs](file:///d:/workspace/qcode/crates/qcode-kernel/src/config/mod.rs#L945-L983)）：
  `base_url`、`api_key_env`、`api_key_command`、`default_model`、`protocol`、`images`、`stream_usage`、`extra_body`、`headers`、**`models`**。
- `models` 语义（U0）：**空列表 = 使用 catalog 内置模型目录**；非空 = 覆盖 catalog（[注释原文](file:///d:/workspace/qcode/crates/qcode-kernel/src/config/mod.rs#L978-L982)）。
- `ModelEntry`（[config/mod.rs](file:///d:/workspace/qcode/crates/qcode-kernel/src/config/mod.rs#L1055-L1064)）：`id`、`name`、`context_window`、`max_tokens`。
- 内置 catalog（[catalog.rs](file:///d:/workspace/qcode/crates/qcode-kernel/src/config/catalog.rs#L73-L170)）：openai → deepseek → ollama → minimax → anthropic，每项带 `protocol / base_url / api_key_env / default_model / models / images`。
- `ProviderRegistry::list_configurable_providers`（[provider_registry.rs](file:///d:/workspace/qcode/crates/qcode-kernel/src/provider_registry.rs#L93-L117)）：catalog 目录（presentation order）+ config 中 catalog 未收录的手写 route。

### 2.2 当前 Models 页 UI

入口 `render_models_providers`（[models.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/models.rs#L489-L622)）：

```
┌ 模型设置页 ───────────────────────────────┐
│  [当前默认：provider / model ▾]  ← 可点开默认模型选择器
│  ┌ Provider 卡片（目录所有行，无论是否配置模型）──┐
│  │ ● OpenAI (openai) · OpenAI 兼容 · 凭证已配置  │
│  │  [设为默认]  [编辑]                          │
│  └───────────────────────────────────────────┘
│  ...（每张卡片同构）
│  [＋ 添加自定义提供方]   ← 声明卡片（route/id/地址/key env/协议/模型）
└────────────────────────────────────────────┘
```

- 卡片头：凭证圆点（Configured 实心 / Missing 空心 / Keyless 无）、显示名、`protocol · 凭证状态`、"当前默认"徽标、「设为默认」（[models.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/models.rs#L511-L599)）。
- 「编辑」按钮：已配置 route 直接 `open_editor`；未配置 route 先 `add_provider_preset`（写入 catalog 预设行，[tables.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/tables.rs#L334-L378)）再 `open_editor`。
- 编辑弹窗 `render_provider_dialog`（[models.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/models.rs#L628-L741)）：API 密钥（钥匙串）→ 「展开更多配置」→ 模型目录 `render_models_editor`（[models.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/models.rs#L972-L1103)）。
- 模型目录编辑器：badge（已自定义/使用默认）、「恢复默认模型」、「获取可用模型」（probe 候选选择框）、模型行（ID/显示名/上下文窗口/max_tokens + 删除 ×）、「＋ 添加模型」。

### 2.3 相关状态字段（SetupView，[mod.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/mod.rs#L82-L181)）

| 字段 | 用途 |
|---|---|
| `editing_provider: Option<String>` | 当前打开编辑弹窗的 route，同时只开一个 |
| `editor_expanded: bool` | 编辑弹窗「更多配置」折叠态 |
| `declaring: bool` + `custom_*` | 自定义提供方声明卡片草稿 |
| `probe_candidates/picked/query/search` | 探测模型候选选择框 |
| `reachability/reach_inflight` | 无密钥 provider 端点可达性（后台探测）——**改版后移除**（见 3.3） |
| `default_model_input` | 默认模型手输框 |
| `saved_notice/provider_error/provider_notice` | 提示文案 |

相关动作：`open_editor/close_editor`、`open_custom_card/close_custom_card`、`create_custom_provider`、`add_provider_preset`、`make_default/pin_default`、`add_model/remove_model/reset_models`、`probe_models`、`store_provider_key`、`save/auto_save`。

---

## 3. 需求拆解与设计

### 3.1 需求 1：只展示"已配置模型"的 Provider，可展开模型列表

#### 3.1.1 "已配置了模型"的判定口径

新增判定函数（建议放 `models.rs` 或 `qcode-kernel` 的 registry 侧，供 UI 单方使用即可）：

```rust
/// 某 route 是否算「已配置模型」：profile 存在，且 models 非空或 default_model 已设置。
fn configured_models(route: &str, config: &Config) -> bool {
    let Some(profile) = config.providers.get(route) else {
        return false; // 没进 config.providers = 没配置，不展示
    };
    !profile.models.is_empty() || profile.default_model.is_some()
}
```

- 判定**严格口径**：必须是 `config.providers` 中存在的 profile，且 `models` 非空**或** `default_model` 非空。
- catalog 内置模型列表（如 openai 的 gpt 系列）**不视为已配置**——用户没有为它做过任何选择，符合"没有配置模型的不展示"。
- 自定义 route（手写声明）天然满足（创建时即带模型或默认模型）。

#### 3.1.2 生效模型列表（展示数据源）

```rust
/// 某 route 生效的模型列表：profile.models 覆盖 catalog，否则回退 catalog 内置。
fn effective_models(route: &str, config: &Config) -> Vec<ModelEntry> {
    match config.providers.get(route) {
        Some(profile) if !profile.models.is_empty() => profile.models.clone(),
        _ => qcode_kernel::config::catalog_models(route),
    }
}
```

> 注：`effective_models` 服务于已展示 provider 的列表渲染；回退分支实际只会命中"default_model 已设但 models 空"的 provider（此时展示 catalog 内置列表）。

#### 3.1.3 页面结构（重构 `render_models_providers`）

```
┌ 模型设置页 ─────────────────────────────────────┐
│  [当前默认：provider / model ▾]                  │
│  ┌ Provider 卡片（仅 configured_models 为真的）──┐ │
│  │ ▸ ● OpenAI (openai) · 凭证已配置   [编辑]     │ │   ← 头可点击展开/收起
│  └─────────────────────────────────────────────┘ │
│  ┌ Provider 卡片（展开态）──────────────────────┐ │
│  │ ▾ ● DeepSeek (deepseek) · 凭证已配置  [编辑]  │ │
│  │  ┌ 模型列表 ──────────────────────────────┐  │ │
│  │  │ deepseek-flash — DeepSeek-V4.1-Flash   │  │ │
│  │  │   上下文 128K · 最大输出 8K             │  │ │
│  │  │ deepseek-v4-pro — ...                  │  │ │
│  │  │ [＋ 在此 Provider 添加模型]             │  │ │
│  │  └────────────────────────────────────────┘  │ │
│  └─────────────────────────────────────────────┘ │
│  [＋ 添加模型]                                    │ ← 需求 2 入口
└──────────────────────────────────────────────────┘
```

设计要点：

- **过滤**：`directory`（`list_configurable_providers` 结果）先按 `configured_models(route)` 过滤，再渲染卡片。顺序沿用 catalog presentation order + 自定义 route 追加。
- **展开状态**：新增 `expanded_routes: BTreeSet<String>`（纯 UI 状态，不落盘）。卡片头整行可点击切换；展开时渲染 `effective_models`。
- **模型行**：只读展示（id + name + `上下文 X · 最大输出 Y` 小字），不提供行内编辑；编辑/增删模型统一进配置页（避免弹窗与行内双入口）。
- **卡片头**：保留凭证圆点、显示名、protocol·凭证状态、"当前默认"徽标、「设为默认」（可用且非默认）、「编辑」（进配置页）。展开/收起 caret（▸/▾）放在右侧。
- **空态**：过滤后无卡片时显示提示 +「添加模型」按钮（引导到需求 2 流程）。
- **不受过滤影响的部分**：顶部「当前默认」行（`render_default_model_line`）基于 `default_model` 配置，独立于列表过滤，保持不变——即使默认 provider 未"配置模型"也应能查看与修改。

### 3.2 需求 2：「添加模型」→ Provider 大列表选择框 → 配置页

#### 3.2.1 交互流程

```
[＋ 添加模型] 按钮
   │
   ▼
Provider 大列表选择框（modal，只选择，不配置）
   │  行内容：凭证圆点 + 显示名 (route) + protocol · 凭证状态
   │  底部固定行：「自定义提供方…」（入口）
   │
   ▼ 点击某个 Provider
   │
   ├─ catalog 内置 route：
   │     已配置 → close_picker + open_editor(route)   （编辑已有配置）
   │     未配置 → close_picker + add_provider_preset(route) + open_editor(route)
   │             （写入 catalog 预设行后再进配置页，沿用现有逻辑）
   ├─ 已声明的自定义 route（catalog 外）：
   │     close_picker + open_editor(route)
   └─ 「自定义提供方…」：
         close_picker + open_custom_card(声明卡片，现有流程)
```

- **选择框只做选择**：框内无任何表单、无编辑动作；点击行即关闭并进入下一步。
- **配置页**：**完全复用**现有 `render_provider_dialog`（编辑弹窗）+ `render_models_editor`（模型目录），不新建配置 UI——API 密钥、协议、端点、模型增删、探测、设为默认、删除全部沿用。
- **未配置 route 的首次进入**：复用 `add_provider_preset`（[tables.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/tables.rs#L334-L378)），catalog 预设行自动带 base_url / key env / protocol / 内置模型列表。

#### 3.2.2 状态结构

新增字段（`SetupView`）：

```rust
/// Provider 选择框（需求 2）是否打开。
adding_provider: bool,
/// 选择框内 Provider 行搜索框（可选，Provider 多时启用）。
provider_picker_search: Option<Entity<InputState>>,
```

复用：`editing_provider`、`editor_expanded`、`provider_error/notice`、`saved_notice`、`custom_*`（声明卡片）。

#### 3.2.3 新增动作

```rust
fn open_provider_picker(&mut self, window, cx)   // 打开选择框
fn close_provider_picker(&mut self, cx)          // 关闭选择框
fn pick_provider(&mut self, route: &str, window, cx)
//   关闭选择框 →（未配置则 add_provider_preset）→ open_editor(route)
```

#### 3.2.4 「添加自定义提供方」入口去留

建议：将现有「＋ 添加自定义提供方」按钮**并入**选择框底部固定行（「自定义提供方…」），页面上只保留一个主按钮「＋ 添加模型」。
若希望保留旧入口（wizard 流程等），也可两按钮并存——实现上互不影响，文档默认采用"并入"方案。

### 3.3 清理：移除自动可达性探测（主界面 + 设置界面）

#### 3.3.1 动机

改版后 provider 必须**显式配置模型**（`configured_models` 判定）才会出现在设置页列表，主界面模型选择器与之对齐后同样只列"已配置模型"的 provider。端点是否真的可用，由配置页的「获取可用模型」（`probe_models`，`GET {base}/models`）**显式验证**——真实 API 探测，比现有 TCP 短连接探测更准确。因此两处自动探测不再有使用场景：

- 设置页渲染路径对无密钥 provider（如 ollama）的自动端点探测（`ensure_reachability`）；
- 主界面构建模型选择器时对 keyless 端点的同步/缓存探测（`keyless_endpoint_reachable`）。

#### 3.3.2 设置界面删除清单（`crates/qcode-gui/src/setup`）

| 位置 | 内容 |
|---|---|
| [mod.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/mod.rs#L164-L173) `SetupView` 字段 | `reachability`、`reach_checked_at`、`reach_inflight`、`reach_tx`、`reach_rx` |
| [mod.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/mod.rs#L386-L390) `SetupView::new` 初始化 | 上述字段的初始值 |
| [models.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/models.rs#L47-L57) | `provider_usable` 中 keyless 可达性分支（或整个函数，若其唯一定义点删除） |
| [models.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/models.rs#L61-L120) | `ensure_reachability` |
| [models.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/models.rs#L122-L165) | `spawn_reach_drain` |
| [models.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/models.rs#L492) | `render_models_providers` 开头的 `self.ensure_reachability(cx)` 调用 |

> 若以上探测线程相关 import（`std::sync::mpsc` 通道、探测线程 spawn）仅此处使用，一并清理。

#### 3.3.3 主界面删除清单（`crates/qcode-gui/src/lib.rs`）

| 内容 | 位置 |
|---|---|
| `provider_models` 中 keyless 探测分支（"无需密钥 provider 必须端点可达才列入"） | [lib.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/lib.rs#L419-L435) |
| `keyless_endpoint_reachable` | [lib.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/lib.rs#L506-L522) |
| `endpoint_reachable` | [lib.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/lib.rs#L489-L502) |
| `endpoint_of` | [lib.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/lib.rs#L462-L486) |
| 相关 import（`std::net::TcpStream`、`ToSocketAddrs` 等，若仅此处使用） | lib.rs |

主界面 `provider_models` 简化为：遍历 `config.providers`，只取满足 `configured_models(route)` 的 provider，列出 `effective_models(route)`（排序 / 去重逻辑保留）；不再需要 `list_configurable_providers` 的可访问性过滤与 TCP 探测。

#### 3.3.4 替代行为

- "ollama 是否好用"由用户**显式触发**：进入配置页点「获取可用模型」→ 失败则在配置页内提示（现有 `probe_result` 错误文案路径），不再在列表渲染 / 选择器构建时自动探测。
- 列表与选择器**不再因端点不可达而隐藏**已配置模型的 provider：配置了模型的 ollama 始终可见；真正不可用时由任务运行期错误提示兜底。

---

## 4. UI 元素树（GPUI）

```text
render_models_providers
├─ render_default_model_line()                     // 不变
├─ saved_notice（如有）
├─ [for route in filtered_routes]                  // configured_models 过滤
│   └─ ProviderCard（div.border.rounded.p_3）
│       ├─ head（div 整行 clickable）
│       │   ├─ caret（▸/▾）
│       │   ├─ credential dot（Configured/Missing/Keyless）
│       │   ├─ display_name (route)
│       │   ├─ caption（protocol · credential_text）
│       │   ├─ "当前默认" badge（是默认时）
│       │   ├─ [设为默认]（可用且非默认）
│       │   └─ [编辑] → open_editor
│       └─ when expanded_routes.contains(route)
│           └─ ModelList
│               ├─ [for m in effective_models(route)]
│               │   └─ ModelRow（id · name · 上下文/最大输出 caption，只读）
│               └─ [＋ 在此 Provider 添加模型] → add_model
├─ [＋ 添加模型] → open_provider_picker
└─ when adding_provider
    └─ ProviderPicker（modal overlay，同 render_custom_card 的 overlay 模式）
        ├─ 标题「选择 Provider」
        ├─ [for p in list_configurable_providers()]
        │   └─ ProviderRow（只选择，on_click → pick_provider）
        │       ├─ credential dot + display_name (route)
        │       └─ caption（protocol · credential_text）
        └─ 「自定义提供方…」→ pick_custom → open_custom_card
```

---

## 5. 保存与数据流

- **模型/Provider 的增删改**：全部走现有 `tables` 状态 + `auto_save(AutoSaveKind::Tables)`（[save.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/save.rs#L373)），**无需新增保存逻辑**。
- **列表可见性由配置驱动**：provider 配置了模型 → 出现在列表；从配置页删除模型目录/删除 provider → 卡片自动消失，无需额外同步。
- **展开状态**（`expanded_routes`）与选择框开关（`adding_provider`）是纯 UI 状态，不落盘、不进 config.toml。
- **默认模型**：不受过滤影响；`pin_default_fields` 仍同时写 `[default-model]` 与 `providers.<route>.default_model`（[models.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/models.rs#L238-L248)）。注意：为某 provider `pin_default` 后，若它原本没有模型目录，`default_model` 会让它满足"已配置模型"判定而出现在列表。

---

## 6. 边界情况

| 场景 | 处理 |
|---|---|
| 无任何已配置模型的 Provider | 空态提示 +「添加模型」按钮 |
| 默认 Provider 未"配置模型" | 顶部默认行不受过滤影响，仍可查看/修改 |
| `pin_default` 后 provider 出现 | 判定含 `default_model`，符合预期 |
| ollama（keyless） | 配置了模型才展示；未配置时只能通过「添加模型」进入；端点不可达**不隐藏**，由配置页「获取可用模型」显式验证（见 3.3） |
| 选择框点击"已配置"Provider | 直接 `open_editor`，不重复写预设行 |
| 选择框点击"未配置"catalog route | `add_provider_preset` 写入预设行 → 进配置页 |
| 自定义 route（catalog 外） | 选择框列出（来自 `list_configurable_providers` 尾部），点击进 `open_editor` |
| wizard 模式 | `render_provider_dialog` 现有行为是 wizard 下隐藏；选择框保持一致，可后续按需放开 |
| 编辑弹窗内删除 Provider | 删除后 `configured_models` 为假，卡片自动消失；`delete_target` 流程不变 |

---

## 7. 实现步骤（建议顺序）

1. **状态字段**：`SetupView` 增加 `expanded_routes: BTreeSet<String>`、`adding_provider: bool`、`provider_picker_search`（可选）。
2. **判定与数据源函数**：新增 `configured_models(route, config)` 与 `effective_models(route, config)`（`models.rs`）。
3. **重构列表**：`render_models_providers` 按 `configured_models` 过滤 + 卡片头展开/收起 + 只读模型列表（含空态）。
4. **选择框**：新增 `render_provider_picker`（modal overlay）+ `open/close_provider_picker` + `pick_provider`。
5. **接线**：「＋ 添加模型」→ 选择框 → 配置页；「自定义提供方」入口并入选择框底部。
6. **清理自动探测**：按 3.3 删除设置界面 `ensure_reachability` / `reachability` 等字段与函数、主界面 `keyless_endpoint_reachable` / `endpoint_reachable` / `endpoint_of`，`provider_models` 改用 `configured_models` / `effective_models`。
7. **测试**：`setup/tests.rs` 增补（过滤判定、pick 流程、展开切换），手工验收 walkthrough。

---

## 8. 相关文件索引

| 文件 | 职责 |
|---|---|
| [models.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/models.rs) | Models 页渲染、编辑弹窗、默认模型、自定义卡片（主要改动点） |
| [mod.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/mod.rs) | `SetupView` 状态字段、订阅 |
| [tables.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/tables.rs) | `add_provider_preset`、probe、模型行增删（复用） |
| [save.rs](file:///d:/workspace/qcode/crates/qcode-gui/src/setup/save.rs) | 表格保存 / auto-save（复用） |
| [provider_registry.rs](file:///d:/workspace/qcode/crates/qcode-kernel/src/provider_registry.rs) | `list_configurable_providers`（选择框与列表的数据源） |
| [catalog.rs](file:///d:/workspace/qcode/crates/qcode-kernel/src/config/catalog.rs) | 内置服务商目录、`catalog_models` |
| [config/mod.rs](file:///d:/workspace/qcode/crates/qcode-kernel/src/config/mod.rs) | `ProviderConfig`、`ModelEntry`（`models`/`default_model` 判定依据） |

---

## 9. 待确认问题

1. **"已配置模型"判定口径**：采用"profile 存在且 `models` 非空或 `default_model` 非空"的严格口径；是否接受"仅 `default_model` 已设"也算？或者用户希望 catalog 内置模型也算"已配置"（即 profile 存在即展示）？
2. **「添加自定义提供方」入口**：并入选择框底部（默认方案）还是保留页面独立按钮？
3. **模型行展示形式**：只读行 +「编辑」进配置页（默认）；还是需要行内快速删除/默认标记？
