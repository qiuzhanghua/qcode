# 模型供应商「套餐模式」与「太极统一认证 Provider」设计方案

> 本文档仅为设计方案，不涉及代码改动。所有标注「占位/待确认」的硬编码值需在实施前补齐真实数据。

---

## 1. 现状与目标

### 1.1 现状（已核对源码）

- Provider 是**路由（route）中心**的：`config.toml` 中 `providers.<route>` 一个路由只有
  一份 base_url、一份 key 引用（`api_key_env` / `api_key_command`）、一份模型列表
  （`models`）与一个 `default_model`（[ProviderConfig](../../crates/qcode-kernel/src/config/mod.rs)）。
- 内置 catalog（[catalog.rs](../../crates/qcode-kernel/src/config/catalog.rs)）只有
  openai / deepseek / ollama / minimax / anthropic 五家。**火山方舟、智谱 Bigmodel、阿里云百炼均不在内置列表中**。
- API key 只存 OS 钥匙串（macOS Keychain / Linux secret-tool / Windows CredWrite），
  config.toml 只存「读回该 key」的命令（[setup/fields.rs](../../crates/qcode-gui/src/setup/fields.rs)）。
  请求时由 `resolve.rs` 执行 env 或 key 命令得到 key（[resolve.rs](../../crates/qcode-kernel/src/config/resolve.rs)）。
- LLM 请求经 `ProviderRegistry::build` 构造 OpenAI/Anthropic adapter，`Authorization: Bearer <key>`
  注入请求头（[openai_compat.rs](../../crates/qcode-providers/src/openai_compat.rs)）。
  **401/403 只映射成 `ProviderError::Auth`，没有任何 refresh / 自动重取 token 的流程**
  （[openai_compat.rs](../../crates/qcode-providers/src/openai_compat.rs)）。
- 项目现状为「qcode 不实现 OAuth 授权流程」，token 交给外部命令（`api_key_command`）每次取
  （[ADR-0104 §6，_oauth2-example.toml](../../crates/qcode-kernel/template/llm/_oauth2-example.toml)）。

### 1.2 目标

1. 为支持套餐的供应商（火山引擎、智谱 Bigmodel、阿里云、小米、Z.ai、BytePlus）增加「套餐模式」。
2. 无套餐的供应商界面上不出现套餐选择；有套餐的供应商在配置时出现套餐下拉。
3. 钥匙串存储、问题框模型列表的 provider 名字后面带套餐标识（不同套餐 = 不同 key + 不同模型）。
4. 新增「太极统一认证」Provider：OAuth 端点与 client_id 硬编码（public client + PKCE，
   无 client_secret）；无 token 走授权码流程（打开 Keycloak 登录页 → 回调 code → PKCE 换
   token）；401 先 refresh、失败再走授权码流程；token 存 `tj_auth.json`；配置页无钥匙串
   逻辑；默认地址 `http://192.168.69.126:6573/v1/chat/completions`。

---

## 2. 设计决策

| 决策点 | 结论 | 理由 |
|---|---|---|
| D1 套餐建模 | **一个套餐 = 一个 provider route（扁平化）** | 现有架构整体以 route 为中心，key/模型/默认/凭证状态全部天然按 route 隔离，改动最小、零破坏 |
| D2 套餐元数据来源 | 新增静态 `PACKAGES` 表（类似 catalog），代码内置三家供应商的套餐清单 | 套餐是厂商静态属性，不需要用户手填 |
| D3 套餐展示 | 卡片与模型菜单的组标题用「厂商名 (套餐名)」 | 直接满足「provider 名字后面带套餐标识」 |
| D4 太极认证 | 新增 `AuthMode::TjOAuth` + 专用 adapter `TjProvider`，token 生命周期在 provider 内部管理 | 401→refresh→重试 必须在请求循环内闭环，现有 engine 层不重试 Auth 错误 |
| D5 交互获取授权 | provider 运行在 worker 线程，通过注入的「授权回调」让 GUI 打开 Keycloak 登录页并取回 code | 复用现有「运行时有交互能力」的机制，不把 GUI 依赖带进 providers crate |

---

## 3. 套餐模式

### 3.1 数据模型：一个套餐 = 一个 route

**不动 `ProviderConfig` 结构**，`providers.<route>` 语义不变，每个套餐独立成路由：

```toml
# 火山引擎：按量计费（基础 route）+ Coding Plan + Agent Plan，各套餐一个 route
[providers.volcano]                # 按量计费
base_url = "https://ark.cn-beijing.volces.com/api/v3"
protocol = "openai"
api_key_env = "VOLC_ARK_API_KEY"
models = ["doubao-1.5-pro-32k", "..."]   # 按量计费全量模型，待补

[providers.volcano-coding]         # Coding Plan 套餐
base_url = "https://ark.cn-beijing.volces.com/api/v3"
protocol = "openai"
api_key_env = "VOLC_ARK_CODING_API_KEY"
models = ["..."]                   # Coding Plan 套餐模型，待补

[providers.volcano-agent]          # Agent Plan 套餐
base_url = "https://ark.cn-beijing.volces.com/api/v3"
protocol = "openai"
api_key_env = "VOLC_ARK_AGENT_API_KEY"
models = ["..."]                   # Agent Plan 套餐模型，待补
```

### 3.2 新增静态套餐元数据表（D2）

新建 `crates/qcode-kernel/src/config/packages.rs`，结构与 catalog 一致、静态内置：

```rust
pub struct PackageEntry {
    pub route: &'static str,          // providers.<route>
    pub vendor: &'static str,         // 组名：volcano / bigmodel / dashscope / xiaomi / zai / byteplus
    pub label: &'static str,          // 套餐名：按量计费 / Coding Plan / Agent Plan
    pub base_url: &'static str,
    pub protocol: Protocol,           // 六家均为 OpenAi
    pub api_key_env: &'static str,    // 该套餐的推荐环境变量名（用户可改）
    pub default_model: Option<&'static str>,
    pub models: &'static [&'static str],   // 该套餐可用的模型
}

/// 厂商 → 套餐列表；无套餐厂商（openai 等）不在此表。
pub const PACKAGES: &[PackageGroup] = &[ /* 见下方清单 */ ];
```

**套餐清单（2026-11-23 用户确认）**：

| vendor | 显示名 | 套餐 label | route | base_url | api_key_env（建议） | 状态 |
|---|---|---|---|---|---|---|
| volcano | 火山引擎 | 按量计费 | `volcano` | `https://ark.cn-beijing.volces.com/api/v3` | `VOLC_ARK_API_KEY` | 端点已填，模型待补 |
| volcano | 火山引擎 | Coding Plan | `volcano-coding` | 同上 | `VOLC_ARK_CODING_API_KEY` | 模型待补 |
| volcano | 火山引擎 | Agent Plan | `volcano-agent` | 同上 | `VOLC_ARK_AGENT_API_KEY` | 模型待补 |
| bigmodel | 智谱 Bigmodel | 按量计费 | `bigmodel` | `https://open.bigmodel.cn/api/paas/v4` | `BIGMODEL_API_KEY` | 端点已填，模型待补 |
| bigmodel | 智谱 Bigmodel | Coding Plan | `bigmodel-coding` | 同上 | `BIGMODEL_CODING_API_KEY` | 模型待补 |
| dashscope | 阿里云 | 按量计费 | `dashscope` | `https://dashscope.aliyuncs.com/compatible-mode/v1` | `DASHSCOPE_API_KEY` | 端点已填，模型待补 |
| dashscope | 阿里云 | Coding Plan | `dashscope-coding` | 同上 | `DASHSCOPE_CODING_API_KEY` | 模型待补 |
| xiaomi | 小米 | 按量计费 | `xiaomi` | 待确认 | `XIAOMI_API_KEY` | 待补 |
| xiaomi | 小米 | Coding Plan | `xiaomi-coding` | 待确认 | `XIAOMI_CODING_API_KEY` | 待补 |
| zai | Z.ai | 按量计费 | `zai` | 待确认 | `ZAI_API_KEY` | 待补 |
| zai | Z.ai | Coding Plan | `zai-coding` | 待确认 | `ZAI_CODING_API_KEY` | 待补 |
| byteplus | BytePlus | 按量计费 | `byteplus` | 待确认 | `BYTEPLUS_API_KEY` | 待补 |
| byteplus | BytePlus | Coding Plan | `byteplus-coding` | 待确认 | `BYTEPLUS_CODING_API_KEY` | 待补 |

显示名统一由 `route` 派生：`厂商显示名 (套餐名)`，例如 `火山引擎 (Coding Plan)`；
按量计费作为厂商基础 route（route 不带后缀），显示名同样带套餐标识，保持一致性。

### 3.3 配置 UI（设置页）

改动集中在 [models.rs](../../crates/qcode-gui/src/setup/models.rs)：

- **添加流程**（`open_provider_picker`，[models.rs](../../crates/qcode-gui/src/setup/models.rs)）：
  - 普通厂商（openai 等）：点选后直接进现有配置对话框——**全程无套餐下拉**。
  - 包型厂商（火山方舟等）：选中厂商后**弹出套餐下拉**（该厂商的 `PACKAGES` 条目），
    选定套餐 → 确定 route → 进入现有配置对话框（API key 钥匙串、模型编辑器、探测全部复用）。
- **Provider 卡片列表**（`render_models_providers`）：display_name 即「厂商名 (套餐名)」，
  天然带套餐标识；`configured_models`、凭证状态、设为默认等逻辑**零改动**（route 即套餐）。
- **编辑对话框**（`render_provider_dialog`，[models.rs](../../crates/qcode-gui/src/setup/models.rs)）：与现状完全一致，无需按套餐分支。
- **钥匙串**（`store_provider_key`）：account 使用 route（= `火山方舟 (基础套餐)` 对应的 route），
  套餐标识随 route 进入钥匙串条目名，无需改 [fields.rs](../../crates/qcode-gui/src/setup/fields.rs) 的存储逻辑。

### 3.4 问题框模型列表（D3）

`provider_models`（[lib.rs](../../crates/qcode-gui/src/lib.rs)）与
`render_model_menu`（[main.rs](../../crates/qcode-gui/src/render/main.rs)）组标题改用
display_name：

```
火山引擎 (按量计费)
  ├─ doubao-1.5-pro-32k
  └─ doubao-1.5-pro-256k
火山引擎 (Coding Plan)
  ├─ doubao-1.5-pro-32k
  └─ doubao-seed
火山引擎 (Agent Plan)
  └─ doubao-agent
openai
  └─ gpt-5.4
```

无套餐厂商标题不变（不带括号后缀）。

### 3.5 套餐模式改动面小结

| 位置 | 改动 |
|---|---|
| `config/packages.rs`（新） | 静态套餐表，三家供应商条目（占位） |
| `provider_registry.rs` | `list_configurable_providers` 合并套餐条目、按 PACKAGES 派生 display_name（小改） |
| `setup/models.rs` | 添加流程加套餐下拉（小改）；卡片/对话框显示名（小改） |
| `lib.rs::provider_models` | 组标题带套餐标识（小改） |
| `resolve.rs` / `openai_compat.rs` / 钥匙串 / 请求链路 | **零改动** |

> 备选方案 B：`ProviderConfig` 增加 `packages: Vec<PackageConfig>` 字段，一个 route 挂多个套餐，
> 按所选 model 反推套餐取 key。改动面大（resolve、凭证状态、钥匙串账号命名、对话框按套餐分区、
> 模型菜单分组均要改），**仅在要求「一个供应商配置项下管理多个套餐」时才考虑**。本文推荐方案 A。

---

## 4. 太极统一认证 Provider

### 4.1 硬编码 OAuth 配置

新建 `crates/qcode-kernel/src/providers/tj.rs`，值已按实测配置填入（见下方「实测记录」）：

```rust
/// 太极统一认证端点与客户端标识 —— 全部硬编码在代码中（需求 4a）。
/// 客户端为 public + PKCE S256（B 方案，实测通过），token 端点不校验
/// client_secret，因此**无需硬编码任何密钥**。
pub struct TjAuthConfig {
    pub auth_url: &'static str,     // https://auth.taiji.com.cn/realms/taiji/protocol/openid-connect/auth
    pub token_url: &'static str,    // https://auth.taiji.com.cn/realms/taiji/protocol/openid-connect/token
    pub userinfo_url: &'static str, // https://auth.taiji.com.cn/realms/taiji/resource/openid-connect/userinfo（太极自定义网关路径，非 well-known 的 /protocol/ 版本）
    pub jwk_set_url: &'static str,  // https://auth.taiji.com.cn/realms/taiji/protocol/openid-connect/certs
    pub client_id: &'static str,    // "qcode"
    pub client_name: &'static str,  // "qcode"
    pub redirect_uri: &'static str, // "qcode://oauth2/callback"（自定义 scheme，需在系统注册，见 §4.3）
    // 无 client_secret 字段。
}
pub const TJ_AUTH: TjAuthConfig = TjAuthConfig { /* 按上述实测值填入 */ };

/// 太极 route 判定与默认请求地址（需求 4c）
pub const TJ_ROUTE: &str = "tj";
pub const TJ_BASE_URL: &str = "http://192.168.69.126:6573/v1/chat/completions";
```

> **实测记录（2026-11-23，Keycloak 26.2.4，realm `taiji`，client `qcode`）**：
> - `/.well-known/openid-configuration` 可公网访问，确认标准 Keycloak；`code_challenge_methods_supported: ["plain","S256"]`。
> - 授权端点带合法 `code_challenge`（S256，43 字符 base64url）→ `200` 登录页；**PKCE 生效**。
>   （注意：code_challenge 必须是 RFC 7636 格式的 43 字符 base64url，格式非法会报
>   `invalid_request: Invalid parameter: code_challenge`，此前误判为 PKCE 未启用。）
> - token 端点**不带 secret**用假 code 请求 → `400 invalid_grant / Code not valid`（而非 `401 unauthorized_client`），
>   证明 client 已为 public，secret 认证不再要求。
> - 回调 `qcode://oauth2/callback` 已通过白名单校验；`/resource/openid-connect/userinfo` 端点有效（无 token 返回 401）。
> - 修改前该 client 为 confidential（无 secret 请求返回 `401 unauthorized_client`），已由管理员改为 public + PKCE S256。

### 4.2 token 存储：`tj_auth.json`

与 `config.toml` 同目录（`home_dir()/.config/qcode/`，见 [load.rs](../../crates/qcode-kernel/src/config/load.rs)）：

```json
{
  "access_token":  "...",
  "refresh_token": "...",
  "expires_at":    1732345678,   // 由 expires_in 或 JWT exp 计算
  "id_token":      "...",        // 可选
  "username":      "zhang.san"   // 可选，登录后展示用
}
```

写入与读取统一封装在 `TjTokenStore`（原子写：先写临时文件再 rename；类 Unix 文件权限 0600）。

### 4.3 请求流程（需求 4b）

```
发送 LLM 请求（route == "tj"）
        │
        ▼
┌─ ensure_token() ───────────────────────────────┐
│ 读 tj_auth.json                                │
│  ├─ 有 access_token 且未过期 → 直接用它发请求    │
│  └─ 无 / 过期 → 授权码流程（PKCE S256）：        │
│      1) 生成 code_verifier + code_challenge(S256)
│      2) 打开 Keycloak 登录页（系统浏览器或应用内嵌 WebView）
│         —— 用户在登录页输入用户名/密码完成登录
│      3) 回调 qcode://oauth2/callback?code=… 拿到 code
│      4) POST token_url（code + code_verifier + redirect_uri，
│         无 client_secret）→ access_token + refresh_token + expires_in
│      5) 写回 tj_auth.json                      │
└────────────────────────────────────────────────┘
        │
        ▼
   发送请求（Authorization: Bearer <access_token>）
        │
  响应 401 ? ──否──► 正常返回
        │是
        ▼
┌─ refresh_token() ──────────────────────────────┐
│ POST token_url（grant_type=refresh_token，无 secret）│
│  ├─ 成功 → 更新 tj_auth.json → 重试原请求（1 次）│
│  └─ 失败 → 回到授权码流程（再次打开登录页）→ 重试 │
└────────────────────────────────────────────────┘
```

- **token 是否有效的判断**：优先用 token 响应里的 `expires_in` 计算 `expires_at`；
  若无 `expires_in`，本地**不解签**仅解码 access_token（JWT）的 `exp` 载荷判断。
  `jwk_set_url` 预留给需要验签的场景（当前流程未强制验签）。
- **认证方式**：public client + PKCE S256（实测通过，见 §4.1 记录）——token 请求不带
  client_secret；`code_verifier` 用完即弃，不需要持久化。
- **回调 scheme 注册**：`qcode://` 自定义 scheme 需要系统注册——Windows 写注册表
  `HKCU\Software\Classes\qcode`，macOS 配 Info.plist `CFBundleURLTypes`，Linux 用
  `.desktop` 文件的 `MimeType=x-scheme-handler/qcode`。备用方案：loopback 回调
  `http://127.0.0.1:<固定端口>/oauth2/callback`（RFC 8252），需在 Keycloak 的
  Valid redirect URIs 中精确配置该地址。
- **401 重试闭环在 provider 内部**，不依赖 engine 层的 `is_retryable`
  （[stream.rs](../../crates/qcode-core/src/engine/stream.rs)，该表不含 Auth）。

### 4.4 代码落点

**qcode-providers crate（新增 `tj.rs`）—— 纯请求层，不依赖 GUI：**

```rust
pub struct TjProvider {
    inner: OpenAiCompatProvider,     // 复用 openai_compat 的请求构造/SSE 解析
    auth: TjAuthClient,              // 授权码（PKCE）/ refresh 两段 HTTP
    store: TjTokenStore,             // tj_auth.json 读写
    prompt: Option<AuthPrompt>,      // 向 GUI 要一次授权机会（D5）
}

impl Provider for TjProvider {
    async fn stream(&self, req, cancel) -> Result<EventStream, ProviderError> {
        let token = self.ensure_token(cancel).await?;        // 无/过期 → 交互授权码流程
        loop {
            let res = self.inner.send_with_token(&req, &token).await?;
            if res.status() != 401 { return parse(res); }
            // 401 → refresh 一次
            match self.auth.refresh(&self.store).await {
                Ok(new_token) => { token = new_token; continue; }   // 重试原请求
                Err(_) => {
                    token = self.ensure_token(cancel).await?;       // 重新授权码流程
                    continue;
                }
            }
        }
    }
}

/// GUI 注册的回调：触发打开 Keycloak 登录页（系统浏览器或内嵌 WebView），
/// 阻塞等待 `qcode://oauth2/callback` 回调带回的 code，返回该 code。
pub type AuthPrompt = Arc<dyn Fn() -> Option<String> + Send + Sync>;
```

**qcode-kernel：**

- `config/resolve.rs`：`Resolved` 增加 `auth_mode: AuthMode`（`ApiKey` / `TjOAuth`），
  tj route 解析为 `TjOAuth`、`api_key: None`、`base_url` 取 `TJ_BASE_URL`。
- `provider_registry.rs`：
  - `build()` 对 `auth_mode == TjOAuth` 构造 `TjProvider`（把 `AuthPrompt` 注入，默认 None = 直接报错「未登录」）；
  - `credential_state()` 对 tj route 改为**检查 `tj_auth.json` 是否存在有效 token**（Configured/Missing），不走 env/key command；
  - `configured_models` 不变（tj 仍需配置模型列表）。

**qcode-gui：**

- 配置对话框（`render_provider_dialog`）：route == `TJ_ROUTE` 时：
  - **跳过 API 密钥区块与「存入钥匙串」按钮**（需求 4c，无钥匙串逻辑）；
  - 新增「太极统一认证」状态区：显示 未登录 / 已登录（用户名），「登录」按钮 → 打开
    Keycloak 登录页（触发授权码流程）并写 `tj_auth.json`；
  - base_url 预填 `http://192.168.69.126:6573/v1/chat/completions`，其余字段与模型编辑器与其他 Provider 相同（需求 4c）。
- 注册 `AuthPrompt` 回调（打开 Keycloak 登录页并等待回调 code），注入 `ProviderRegistry::build`。
- `provider_usable()`：tj route 的可用性由 token 存在性决定，同步 `credential_state`。

### 4.5 太极改动面小结

| 位置 | 改动 |
|---|---|
| `kernel/src/providers/tj.rs`（新） | 硬编码端点/client_id、`TjAuthConfig`、`TJ_ROUTE`、`TJ_BASE_URL`（无 secret） |
| `qcode-providers/src/tj.rs`（新） | `TjTokenStore`、`TjAuthClient`（PKCE 授权码 + refresh）、`TjProvider`（ensure/refresh/401 闭环） |
| `kernel/config/resolve.rs` | `Resolved.auth_mode`（小改） |
| `kernel/provider_registry.rs` | build 分派 TjProvider、tj 凭证状态（小改） |
| `gui/setup/models.rs` | tj 对话框无钥匙串 + 登录状态区（中改） |
| `gui` shell/注册回调 | 注入 `AuthPrompt`（打开登录页等待 code）（小改） |
| `Cargo.toml` | 不新增外部依赖（复用 reqwest / serde_json / sha2） |

### 4.6 ADR 影响

[ADR-0104 §6](../../crates/qcode-kernel/template/llm/_oauth2-example.toml)
「qcode 不实现 oauth2 授权流程」需**修订**：太极统一认证作为内置例外，授权码流程在应用内实现；
其他 Provider 仍走外部命令模式，不受影响。

---

## 5. 建议实施顺序

1. `packages.rs` 静态套餐表 + `provider_registry` 目录合并/显示名 → 配置 UI 套餐下拉 + 模型菜单标识。
2. `tj.rs`（kernel）硬编码端点 + `TjTokenStore` + `TjAuthClient`（先做 refresh/ensure 的纯逻辑与单测）。
3. `Resolved.auth_mode` + `TjProvider` + `ProviderRegistry::build` 分派 + 401 重试闭环（单元/集成测试）。
4. GUI：tj 对话框（无钥匙串 + 登录区）、`AuthPrompt` 回调、`provider_usable`/凭证状态同步。
5. 补齐六家厂商各套餐的模型清单与剩余端点（小米/Z.ai/BytePlus），替换占位值。

每步均遵循项目既有约定：新增逻辑自带 `#[cfg(test)]` 测试（如 resolve、credential_state、token 刷新状态机）。

---

## 6. 风险与待确认项

| # | 事项 | 影响 | 处理 |
|---|---|---|---|
| 1 | 六家厂商的**套餐模型列表**（各套餐可用模型）；小米/Z.ai/BytePlus 的 **API 端点** | 套餐表 models/base_url 待填 | 套餐名已确认（2026-11-23）；模型清单与剩余端点实施前提供 |
| 2 | 太极授权端点请求参数格式 | ~~待确认~~ 已实测：标准 Keycloak 授权码 + PKCE S256（见 §4.1 实测记录）；登录页交互、`code_verifier` 换 token 均已验证 | — |
| 3 | ~~`client_secret` 硬编码~~（已消除） | — | 实测 B 方案通过（public + PKCE S256），qcode 不携带任何密钥；`client_id` 为公开标识，无泄露风险 |
| 4 | `tj_auth.json` 明文存 token | 本地明文凭据 | 类 Unix 0600 权限；Windows 按用户目录隔离 |
| 5 | 套餐模型列表需不重叠（保证模型→套餐映射唯一，方案 B 才依赖；方案 A 无此约束） | — | 方案 A 天然规避 |
| 6 | 无 token 时 CLI 场景（无 GUI 可开登录页） | `AuthPrompt = None` | 返回明确错误「太极统一认证未登录，请在设置页登录」 |
