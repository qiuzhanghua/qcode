# qcode 设置页与模型接入架构重构设计（V2）

> 状态：草稿，待评审
> 参考：deepseek-harness (dsh) 的模型选择与后台路由架构
> 范围：设置页 UI 重组 + Provider 路由/适配器注册表 + 模型选择两级化 + 凭证分离

---

## 1. 背景与目标

当前 qcode 的设置页由 `SetupView`（11 个页面）承载，配置落盘到 `~/.config/qcode/config.toml`。
随着功能增长（MCP、Skills、Sandbox、Network、Search、Pricing……），设置页出现了两类问题：

1. **UI 层**：字段声明是声明式的（`FIELDS`/`TABLES`），但 apply 路径是硬编码 `match`，新增一个配置项要改多处（schema、渲染、apply、known_keys 约束）。
2. **模型层**：Provider 身份只是 `providers` map 的 key，内置 provider 用 `builtin()` 硬编码 match，适配器用 `build_provider` 按名字 match 分发——没有"路由 + 注册表"的概念，协议（OpenAI 兼容 / Anthropic）与供应商身份耦合，新增一个供应商要动核心代码。

本设计借鉴 dsh 的三块架构，在 qcode（单二进制 Rust 应用）里落地：

- **设置命名空间分层**：每个功能域拥有自己的配置 section（schema + 默认值 + 用户层），写入按叶子 diff，未改动的键与注释原样保留。
- **Provider 路由 + 适配器注册表**：一个 provider 路由键（route）决定协议、端点、模型目录；一个适配器可服务多个路由；设置页由"可配置 provider 目录"驱动。
- **模型选择两级化**：会话级（随会话记录持久化，恢复会话时自动还原）与默认级（`default-model` 命名空间落盘，会话内切模型同步更新），二者职责分离但相互联动。

---

## 2. 现状盘点与痛点

### 2.1 设置页 UI（`crates/qcode-gui/src`）

- 入口：`setup/mod.rs` 的 `run_settings` / `run_setup`，与首次运行向导共用 `SetupView`；应用内经 `Shell::open_settings`（`shell/view.rs`）打开独立窗口。
- 页面：`forms.rs` 的 `Page` 枚举共 11 页（Welcome、Provider、ModelKey、Agent、NetworkSandbox、Search、Mcp、Skills、ApprovalPricing、UpdateAppearance、Review），顶部 ●/○ 横向导航。
- 字段：`FIELDS`（28 个标量）+ `TABLES`（providers、pricing、approval.rules、mcp.servers 四张表）+ `GUI_FIELDS`（主题/字号/密度/发送模式）。

**痛点**：

| # | 问题 | 位置 |
|---|---|---|
| P1 | apply 路径硬编码：`scalar_string/scalar_bool/apply_scalar` 按 `["provider","field"]` 字面路径逐一 match | `setup/fields.rs` |
| P2 | 渲染用 `path.join(".")` 作 key 查 BTreeMap，字段与渲染逻辑耦合 | `setup/render.rs` |
| P3 | Provider 表特例：内嵌 `__key` 密文输入 + Keychain "Store key"、硬编码预设按钮 `["deepseek","openai","anthropic","minimax","ollama"]`、MCP env 提示 | `setup/render.rs` |
| P4 | `apply_tables` 一个大 match 从行状态重建四张表；`stale_model_default` 是"行改名后残留别家模型"的补丁 | `setup/save.rs`、`setup/fields.rs` |
| P5 | `extra_body` 无法在表单编辑；无快捷键配置页 | `setup/save.rs` |
| P6 | `known_keys()` 要求每个键必须有表单控件（测试约束），导致配置结构与 UI 强绑定 | `config/load.rs`、`tests/schema.rs` |

### 2.2 模型 / Provider 配置（`crates/qcode-kernel`）

- `Config.providers: BTreeMap<String, ProviderConfig>`；`ProviderConfig` = `base_url / api_key_env / api_key_command / default_model / stream_usage / extra_body`（`config/mod.rs`）。
- 内置 provider 硬编码：`builtin()`（`config/resolve.rs`）内联 openai / deepseek / ollama / minimax / anthropic|claude 的端点与 key env。
- 适配器分发硬编码：`build_provider`（`lib.rs`）按 `resolved.name` match：mock* → `MockProvider`，anthropic|claude → `AnthropicProvider`，其余全部落到 `OpenAiCompatProvider`。

**痛点**：

| # | 问题 |
|---|---|
| P7 | 供应商身份 = map key，无"路由"抽象；协议与供应商名耦合（"新供应商必须改代码或恰好是 OpenAI 兼容"） |
| P8 | 内置供应商/端点/默认模型是代码常量，设置页预设按钮与代码重复维护两份清单 |
| P9 | 模型选择混乱：`providers.*.default_model` 落盘 + 顶栏下拉取"所有 provider 的 default_model 并集" + 运行时 `Engine.model` 内存态（`UiCommand::SetModel`，不落盘）——"当前用哪个模型"与"默认模型"职责不清 |
| P10 | 密钥与 provider 配置同表（`api_key_env` 只是环境变量名，另有 Keychain 存储），凭证状态没有统一视图 |

### 2.3 持久化与数据流

- 主配置 TOML：`~/.config/qcode/config.toml` + 项目级 `.qcode/config.toml` 叠加（`config/load.rs`）。
- 写：`SetupView::save` → `ConfigEdit::apply_config`（toml_edit 保留注释）→ `ConfigEdit::save`（临时文件 + .bak 原子替换）。
- 回灌：设置窗口只写文件；主窗口轮询 `config_mtime` → `rebuild_hook` → 重建后端（`shell/mod.rs`）。
- GUI 偏好独立 JSON `<data-dir>/gui.json`（`gui_prefs.rs`）。

---

## 3. 参考架构：dsh 的关键设计（本设计的来源）

dsh（deepseek-harness，TS monorepo）三块设计：

**① 设置命名空间（settings section）**
- 每个功能插件调用 `installSection(ctx, ns, schema, base, {setSource, onChange})` 注册自己的命名空间。
- 值分层：**默认值 → composition base → 用户层**，运行时通过 `setSource` 拿到实时快照。
- 文件持久化：`settings.yaml` 一个文档承载所有命名空间，写入是**注释保留的叶子级 diff**（只 set/delete 变更键），原子写 + 跨进程锁。

**② Provider 路由 + 适配器注册表**
- 请求体只带 `{provider, model}`；`LlmRuntime.registerAdapter(routes, adapter)` 按路由键分发到适配器。
- 一个通用适配器（pi-ai）可服务所有 pi-ai 路由；专用插件（llm-deepseek）服务单一官方路由。
- `listConfigurableProviders()` 输出目录（route + displayName + settings 地址 + 是否已配置），**设置页完全由目录驱动**。
- 协议抽象：可命名的 wire 协议表（openai-completions / openai-responses / anthropic-messages）；内置目录供应商自带协议实现与模型目录。

**③ 模型选择两级化**
- 会话级：`ModelSelection {provider, model, reasoningEffort}`。`installModelSelection` 在 prompt 组装/请求两个事件点做快照，保证并发切换不劈裂请求；同时 `selectForNextRequest` 会把选择 **append 为会话记录的持久化投影事件**（`model/selection`，`lastUsed`/`pending` fold）——恢复会话时自动还原，无需用户重选。
- 默认级：`agent-default-model` 命名空间 `{provider, model, reasoningEffort}`；`selectModel` 在安装会话选择时**同步调用 `saveSelection` 写入默认值**（写失败仅告警不阻断），因此新会话也用最新选择。

**④ 凭证分离**
- 配置里只存 credential 引用（`apiKeyEnv`），明文密钥在独立 credentials 存储（`.credentials.yaml`，0600），请求时 `credentials.resolve(ref)` 取回。

---

## 4. 目标架构（dsh 模式在 qcode 的落地）

### 4.1 总体分层

```
┌─────────────────────────────────────────────────────────────┐
│  GUI 层 (qcode-gui)                                          │
│  SettingsView（分组页面）  顶栏模型下拉  状态条(status)        │
└──────────────┬──────────────────────────────────────────────┘
               │ 写（叶子 diff）                │ 读（快照）
┌──────────────▼──────────────────────────────────────────────┐
│  配置层 (qcode-kernel::config)                               │
│  config.toml ── 命名空间 section ── section schema 校验       │
│  默认值 → 项目层 → 用户层                                     │
└──────────────┬──────────────────────────────────────────────┘
               │ resolve
┌──────────────▼──────────────────────────────────────────────┐
│  运行时 (qcode-kernel)                                       │
│  ProviderRegistry（路由→适配器）  ← build_provider 的替代      │
│  Engine.model = 会话级 ModelSelection                         │
│  default-model 命名空间 = 默认模型                             │
└──────────────┬──────────────────────────────────────────────┘
               │ {provider, model} → provider.stream()
┌──────────────▼──────────────────────────────────────────────┐
│  协议层 (qcode-providers)                                    │
│  OpenAiCompat / Anthropic / Mock —— 由协议字段选择，与供应商解耦 │
└─────────────────────────────────────────────────────────────┘
```

### 4.2 配置命名空间与分层（解决 P1/P2/P4/P6）

**概念**：`Config` 的每个顶层 section 对应一个"命名空间"，由**所属模块**拥有——模块声明 schema（默认值、校验、可编辑字段），而不是 GUI 表单反向决定配置结构。

| 现状 | 目标 |
|---|---|
| 表单 schema（FIELDS/TABLES）即配置结构 | 配置 schema（每 section 的字段定义 + 默认值）为唯一事实源，表单从 schema 生成 |
| `apply_scalar` 硬编码 match 路径 | 通用 setter：`(section, path, value) → 校验 → 叶子写入` |
| `apply_tables` 一个 match 重建四张表 | 每张表由 owner 模块提供 `row → section` 的映射函数 |
| `known_keys()` 要求键必有表单控件 | `known_keys()` 改为校验"键必须属于某 section 的 schema"（键↔控件强绑定解除） |

**分层**（沿用现有 TOML 机制，不引入新文件格式）：

```text
默认值（代码内置 defaults） → 项目层 .qcode/config.toml → 用户层 ~/.config/qcode/config.toml
```

读取顺序不变（`merge_project` 已有），新增的是"每个 section 声明自己的默认值与校验"，把散落的 `serde(default)` 集中为 section schema。

**写入**：`ConfigEdit` 增强为**按 section 叶子 diff**——只 set 变更的键，其余键与注释原样保留（沿用 toml_edit + 原子写）。这与 dsh 的 `settings.yaml` 叶子 diff 一致，但 qcode 不换 YAML。

**运行时快照**：为每个 section 提供 `set_source` 式回调——模块持有"当前有效值"的读取闭包，配置变更后主窗口无需全量重建，各模块先自行更新快照，再按需触发增量重建（见 4.7）。

### 4.3 Provider 路由 + 适配器注册表（解决 P7/P8）

**核心类型**：

```rust
// 路由 = 供应商身份；一个请求只携带 {provider, model}
pub struct ModelRoute { pub provider: String }   // 即现有 provider map key，语义升级为"路由"

// 适配器：一个适配器可服务多个路由
pub trait ProviderAdapter {
    fn routes(&self) -> &[String];
    fn stream(&self, route: &str, model: &str, req: StreamRequest) -> ...;
}

// 注册表（替代 build_provider 的 match）
pub struct ProviderRegistry {
    adapters: Vec<Box<dyn ProviderAdapter>>,        // 按路由键索引
    catalog: ProviderCatalog,                       // 内置供应商声明式目录
}
```

**内置目录**（把 `builtin()` match 改为声明式数据表，解决 P8）：

```rust
pub struct CatalogEntry {
    pub route: &'static str,          // "deepseek"
    pub display_name: &'static str,   // "DeepSeek"
    pub protocol: Protocol,           // OpenAi | Anthropic | Mock
    pub base_url: &'static str,
    pub api_key_env: Option<&'static str>,
    pub default_model: Option<&'static str>,
    pub models: &'static [&'static str],   // 目录模型清单
    pub reasoning_split: bool,             // MiniMax 等需要
}
```

内置条目：openai / deepseek / ollama / minimax / anthropic / claude（别名）。**新增供应商 = 加一行目录数据 +（若协议已支持）零代码**；设置页预设按钮改为从目录读取（单一事实源）。

**路由解析**（替代 `resolve.rs` 的 `builtin()` match）：

```text
请求路由名  --lookup--> 用户配置 providers.<route>（覆盖项）--> 目录 catalog[route]（缺省项）
  → ProviderSpec { route, protocol, base_url, auth, models }
  → 注册表按 protocol 构造适配器并缓存
```

**协议与供应商解耦**（解决 P7）：`ProviderConfig` 新增可选 `protocol = "openai" | "anthropic" | "mock"` 字段（`serde(default)` 向后兼容）。无 `protocol` 时由目录条目决定；显式声明时覆盖——即"一条手工声明路由，指定协议 + 端点 + 模型清单"，对应 dsh 的"手工声明路由（平台型）"。**火山方舟 Ark 这类 OpenAI 兼容平台 = 加一条 `protocol="openai"` + base_url 的路由，不需要新代码**。

**手工声明路由的完整字段集**（补齐自定义模型 API 的全部能力，对应 dsh `PiAiProviderProfile`）：

```toml
# 示例：火山方舟 Ark 手工路由
[providers.ark]
protocol = "openai"               # 必需（目录无此路由时）：openai | anthropic | mock
base_url = "https://ark.cn-beijing.volces.com/api/v3"   # 端点
api_key_env = "ARK_API_KEY"       # 凭证引用（或 api_key_command）
models = ["doubao-seed-1-6", "..."]  # 手填清单，或端点探测后采纳
headers = { "x-custom-header" = "value" }   # 可选：部署级自定义请求头
extra = { "extra_body" = ... }    # 可选：透传给请求体的附加字段（升级自 extra_body，M4 可编辑）
```

- `headers` 为**新增字段**（现状 `ProviderConfig` 无）：dsh 手工路由支持部署级请求头（网关鉴权、自定义头），qcode 补齐以覆盖无法仅靠 Bearer key 认证的网关。
- `api_key_env` / `api_key_command` 保留现有语义（§4.5 凭证分离只做 UI 收敛）。

**可配置 Provider 目录**（驱动设置页）：

```rust
pub struct ConfigurableProvider {
    pub route: String,
    pub display_name: String,
    pub protocol: Protocol,
    pub configured: bool,      // 用户层/项目层是否已有该路由配置
    pub credential_state: CredentialState,  // KeyEnv 已存 / 缺失 / 无需 key
}
// qcode-kernel 提供 list_configurable_providers() -> Vec<ConfigurableProvider>
```

**模块归属**（明确落地位置）：

| 职责 | 模块 | 说明 |
|---|---|---|
| `Provider` trait（端口） | `qcode-core::provider`（不变） | 现状位置合理 |
| 协议实现（OpenAI 兼容 / Anthropic / Mock / SSE） | `qcode-providers` crate（不变） | **已独立**，只依赖 `qcode-core` 的 trait，不碰 Config |
| `ProviderCatalog`（内置目录数据表 + 路由解析） | `qcode-kernel::config::catalog`（新增） | 替代 `resolve.rs::builtin()` 的硬编码 match |
| `ProviderRegistry`（路由→适配器注册表） | `qcode-kernel::provider_registry`（新增） | 替代 `lib.rs::build_provider` 的名字 match；依赖 config + qcode-providers |

依赖方向：`qcode-core(端口) ← qcode-providers(协议实现) ← qcode-kernel(目录+注册表+Engine)`，不新建 crate、不产生循环依赖。

**模型清单来源**（目录静态 vs 端点探测，借鉴 dsh `discovery.ts`）：

| 路由类型 | 模型清单来源 | 是否联网 | 元数据 |
|---|---|---|---|
| 内置目录路由（openai/deepseek/ollama/minimax/anthropic） | `ProviderCatalog` 静态表（编译期内置） | 否 | 完整：能力 / 上下文窗口 / 输出上限 / 默认模型 |
| 手工声明路由（火山方舟、网关、自托管） | 用户手填 `models` 或端点探测 | 是（可选） | 探测返回：`id` + 可选的 name / context_window / max_tokens |

- **内置目录不联网**：模型清单在构建期生成、随二进制分发，权威且带完整元数据——对应 dsh 的 `models.generated.js`（打包分发、`catalogModels()` 查内存静态表）。
- **端点探测仅对非目录路由开放**：对 `{baseURL}/models` 发 `GET`（Bearer 认证），只支持 `openai` 协议（OpenAI 兼容的 listing 形状；Anthropic 协议报告"不可探测"，回退手填）。对应 dsh 的 `LISTABLE_PROTOCOLS`。
- **探测结果不落盘**：探测是"候选元数据"供用户采纳（勾选/确认后才写入 `providers.<route>.models`）；**配置文件才是决定路由提供哪些模型的唯一事实源**——对应 dsh 的"nothing here is stored, settings.yaml remains the only thing that decides what a route serves"。
- 顶栏下拉的模型清单 = 配置的 `models` ∪ 目录模型（目录路由）∪ 用户采纳过的探测结果。

### 4.4 模型选择两级化（解决 P9）

**会话级（随会话记录持久化）**：

```rust
pub struct ModelSelection { pub provider: String, pub model: String }
// Engine.model 从 String 升级为 ModelSelection（保留 provider 维度）
// UiCommand::SetModel { provider, model } —— 切换后写会话记录，同 dsh 的 selectModel
```

- 切换动作 = 更新 `Engine.model`（内存快照，供本次 Turn 使用）**并写入会话记录**（qcode 已有 `SessionStore`，见 `qcode-core/src/store.rs`）——恢复该会话时自动还原，**关闭再打开无需用户重选**。
- 现状顶栏下拉是"所有 provider 的 default_model 并集"→ 改为**按当前路由列出可用模型**（用户配置的 models 清单 ∪ 目录模型 ∪ 端点探测），并显示所属 provider。
- 状态条展示 `App.status.model/provider`（已有结构，语义不变）。

**顶栏模型选择交互**（借鉴 dsh `ui-model-selection` 的 ModelSelect）：

- 现状是扁平下拉（各 provider 的 default_model 并集），目标改为**供应商分组的模型菜单**：一个下拉/弹层，第一层是"按 provider 分组的模型列表"（分组头 = 供应商，组内 = 模型），第二层（可选）是模型的 effort 级别。
- **选中即提交完整选择 `{provider, model}`**（不是"先选供应商、再选模型"的两步向导）——分组只是展示组织方式，提交的是一个三元组/二元组。
- 切换生效在下一个 Turn 开始处（会话级选择更新 + 写会话记录）；正在运行的 Turn 保持已快照的选择（对应 dsh 的 prompt-assembly 边界快照）。
- 触发按钮显示"当前模型 + 所属供应商"；当选择与当前路由清单对不上时降级为"选择模型"提示。

**默认级（落盘，`default-model` 命名空间）**：

```toml
# config.toml v3（示意）
[default-model]          # 新命名空间
provider = "deepseek"
model = "deepseek-flash"
```

- 替代现状"`default_provider` + 各 provider 的 `default_model`"的职责。
- **联动规则（沿用 dsh）**：会话内切模型时，同步把选择写入 `default-model` 命名空间（写失败仅告警不阻断）——即"切即默认"，新建会话用最新选择。若希望保留"显式设默认"的语义，可增加一个开关（见 §7 取舍）。
- 迁移期：读取时若 `default-model` 缺失，回退到旧的 `default_provider` + `providers.*.default_model`；写入新值后落 `default-model`。`stale_model_default` 补丁（P4）随迁移完成删除。

**请求链路**：`run_turn` 取 `Engine.model`（会话级，缺省回退 `default-model`）→ 携带 `{provider, model}` → 注册表路由 → 适配器 stream。

**同路由切模型的机制**（供应商不变、只换模型）：

- 执行路径**保持现状**：`ModelSelection {provider, model}` 同路由只改 `model`；`Provider::stream` 收到的是 `CompletionRequest.model` 字段（[qcode-core/provider.rs](crates/qcode-core/src/provider.rs)），协议实现把 model 原样透传给上游 API——协议、端点、凭证、provider 实例全部复用。
- **新增模型级元数据**（借鉴 dsh 的 catalog models）：每个模型有自己的 `models 清单条目`——能力（reasoning / images）、上下文窗口、max output、价格。切换模型时按 model 重新解析这些元数据，而非 provider 级 `capabilities()`。
- **切换时校验**：`model` 必须属于该路由的模型清单（目录 ∪ 用户配置 `models`）；未声明的模型拒绝切换或回退（对应 dsh `resolveCallConfig` 的选择时校验）。
- 顶栏下拉 = **当前路由的模型清单**（同一 provider 下列出多个 model，选择后只改 `model`）。

### 4.5 凭证分离（解决 P10）

- `ProviderConfig.api_key_env / api_key_command` 保留（引用，不存明文），但设置页从"Provider 表内嵌 `__key` 密文"改为**独立的凭证视图**：

| 列 | 内容 |
|---|---|
| 路由 | deepseek / openai / … |
| 凭证引用 | `DEEPSEEK_API_KEY`（api_key_env） |
| 状态 | 已存（Keychain）/ 缺失 / 无需 key（ollama 本地、mock） |
| 操作 | 输入并存入 Keychain / 清除 |

- Keychain "Store key" 逻辑保留（现状可用），只做 UI 收敛 + 状态聚合（`describe_credentials()` 一次列出全部路由的凭证状态，对应 dsh 的 credentials describe）。

### 4.6 设置页 UI 重组（解决 P1-P6 的 UI 面）

**页面分组**（11 页 → 5 组，横向导航保留，欢迎页移入向导）：

| 分组 | 页面 | 变更 |
|---|---|---|
| 模型 | Provider 目录 | **重构**：路由卡片列表（显示名 / 协议徽标 / 端点 / 凭证状态 / 默认模型下拉 / 停用删除） |
| 模型 | 添加 Provider | 新增：选协议 → 填 base_url / key env / 自定义 headers / 模型清单（含"探测模型"按钮，可选） |
| 模型 | 凭证 | 新增：4.5 的凭证状态表 |
| 运行时 | Agent / Sandbox / Network / Search / MCP / Skills | 保持，字段由各自 section schema 驱动 |
| 外观 | 主题 / 字号 / 密度 / 发送模式 | 保持（仍走 `gui.json`，不并入 config.toml） |
| 其他 | 审批定价 / 更新 / 关于(Review) | 保持 |

**表单渲染**：`render_field/render_table` 从 `path.join(".")` 查 BTreeMap 改为按 section schema 生成控件；`FieldKind` 扩充 `Protocol`、`KeyEnv`（带凭证状态徽标）、`ModelSelect`（按路由列出模型）等类型。

**约束回归**：`tests/schema.rs` 从"每个键必须有控件"改为"每个 section 的键必须属于其 schema，且每个 schema 字段必须能渲染"——**配置结构与 UI 松绑，反过来由 UI 适配 schema**。

### 4.7 数据流与热更新

**写路径**（不变的部分继续用，增强叶子 diff）：

```
SettingsView(表单状态)
  → build_config（按 section schema 生成 section 值）
  → ConfigEdit::apply_section(ns, section)（叶子 diff + toml_edit 保留注释 + 原子写）
  → config.toml
```

**回灌**（优化全量重建）：

- 现状：mtime 轮询 → `rebuild_hook` → 全量重建后端。
- 目标：mtime 轮询 → 通知各 section owner（`set_source` 回调）→ 模型类变更（provider/模型/凭证）走 `ProviderRegistry` 重建路由与适配器；Agent/Sandbox 等变更走各自模块增量更新；**仅当无法定位影响面时才全量重建**。
- 会话级模型切换走 `UiCommand::SetModel`：更新 `Engine.model`（内存，供当前 Turn 用）**并写会话记录**（随会话持久化）；只有"联动默认模型"时才顺带写 `default-model` 命名空间（叶子 diff）。

---

## 5. 迁移与兼容

| 项目 | 策略 |
|---|---|
| config.toml v2 → v3 | 新字段（`protocol`、`display_name`、`models`、`[default-model]`）全部 `serde(default)`，旧文件直接可读 |
| 默认模型 | 读时回退链：`[default-model]` → `default_provider` + `providers.*.default_model`；写时落新命名空间 |
| `builtin()` match → catalog 表 | 行为等价重构，逐条对照测试（openai/deepseek/ollama/minimax/anthropic/claude 端点与 key env 不变） |
| `build_provider` match → 注册表 | 构造结果不变（Mock / Anthropic / OpenAiCompat），路由表驱动方式变化 |
| GUI 偏好 | 继续独立 `gui.json`，不并入 config.toml |
| `stale_model_default` | 迁移完成后删除 |
| `extra_body` | 升级为 `[providers.<route>.extra]` 字典（可选），并允许表单编辑（阶段 M4） |

## 6. 落地步骤（阶段拆分）

| 阶段 | 内容 | 验证 |
|---|---|---|
| **M0 内部重构** | `ProviderCatalog`（目录数据表）+ `ProviderRegistry`（路由→适配器）+ 协议字段解析；行为与现有 config 格式完全不变 | 现有 resolve/build_provider 测试全绿；新增 catalog 对照测试 |
| **M1 模型选择两级化** | `default-model` 命名空间 + 会话级 `ModelSelection`（写会话记录）；顶栏下拉按路由列出模型；读取回退链；"切即默认"联动 | 恢复会话自动还原模型；设默认只 patch 两个键；旧文件迁移测试 |
| **M2 设置页 UI 重构** | 页面分组 + Provider 目录卡片 + 凭证页 + schema 驱动渲染；`apply_scalar/apply_tables` 硬编码 match 移除 | schema.rs 测试改写；手工回归 11 页 |
| **M3 凭证视图** | `describe_credentials()` 聚合 + 凭证页（Keychain 保留） | 各路由凭证状态显示正确 |
| **M4 可选扩展** | `extra` 字典 / 自定义 `headers` 表单可编辑、协议探测模型、快捷键配置页 | 手工验证 |

## 7. 明确不做 / 取舍

- **不引入插件系统**：dsh 的 `installSection` 是动态插件注册，qcode 是单二进制，落地为"编译期模块各自拥有 section schema"，不做热加载插件。
- **不换配置格式**：沿用 TOML + toml_edit 注释保留，不做 YAML/JSON 迁移（dsh 的 yaml 叶子 diff 我们已有等价能力）。
- **不做跨进程/远端设置**：qcode GUI 与 kernel 同进程，不需要 dsh 的 settings-controller RPC 层。
- **会话级模型选择随会话记录持久化**：qcode 已有 `SessionStore` 会话落盘，模型选择作为会话记录的一个字段/事件写入；持久化入口有两个——会话记录（会话级）+ `default-model` 命名空间（全局默认），不做第三处落盘。
- **不做运行时热插拔 provider**：Provider 变更仍走配置写盘 + 模块级重建；单文件原子写保证不丢配置。
- **"切即默认" vs "显式设默认"**：默认沿用 dsh 的"会话内切模型即联动默认"（简单、心智一致）；如需保留旧行为（只有点"设为默认"才改全局默认），在设置页加一个开关 `[ui] sync_default_model = false`（默认 true）。

## 8. 关键决策摘要

1. 配置结构由"各模块 section schema"定义，表单从 schema 生成——UI 与配置结构解耦（替代 FIELDS 反向约束）。
2. Provider 从"map key + 名字 match"升级为"路由 + 注册表 + 目录"——新增供应商 = 目录一行 +（必要时）一条手工路由。
3. 协议从"按供应商名推断"改为"显式 protocol 字段"——平台型（火山方舟、任意 OpenAI 兼容网关）零代码接入。
4. 模型选择两级化：会话级（写会话记录，恢复会话自动还原）+ 默认级命名空间落盘（叶子 diff）；会话内切模型联动更新默认（同 dsh）。
5. 凭证视图独立于 Provider 表，配置只存引用。
6. 设置页按"模型 / 运行时 / 外观 / 其他"分组，Provider 目录页为模型组核心。
