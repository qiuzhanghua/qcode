//! 太极统一认证（Taiji Unified Auth）Provider（PROVIDER-PACKAGES-TJ-AUTH §4）。
//!
//! public client + PKCE S256：token 端点不校验 client_secret（2026-11-23 实测）。
//! 发送请求前先确保 token——有则直接用；无/过期走授权码流程（打开 Keycloak
//! 登录页 → 回调 code → code+verifier 换 token，写入 `tj_auth.json`）。请求
//! 返回 401 时先 refresh、失败再走授权码流程，重试闭环在本 provider 内部，
//! 不依赖 engine 层的 `is_retryable`（该表不含 Auth）。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use qcode_core::provider::{Capabilities, CompletionRequest, EventStream, Provider, ProviderError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};

/// 太极统一认证的端点与客户端标识（值由 kernel 侧硬编码配置填充，无 secret）。
#[derive(Debug, Clone)]
pub struct TjAuthSettings {
    pub auth_url: String,
    pub token_url: String,
    pub userinfo_url: String,
    pub jwk_set_url: String,
    pub client_id: String,
    pub client_name: String,
    pub redirect_uri: String,
}

/// 授权回调：GUI 注册——用给定的授权 URL（已带 `code_challenge` 与 `state`）
/// 打开 Keycloak 登录页，阻塞等待 loopback 回调
/// （`http://127.0.0.1:<port>/oauth2/callback`，见 kernel 侧 §4.3）带回的 code，
/// 返回该 code。`None` = 没有交互能力（CLI / 无人值守），此时返回 `None`
/// 表示无法完成授权。
pub type AuthPrompt = Arc<dyn Fn(String) -> Option<String> + Send + Sync>;

/// `tj_auth.json` 的 token 记录。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TjTokens {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    /// Unix 秒；0 = 未知（由 access_token 的 `exp` 载荷兜底）。
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl TjTokens {
    /// `pub(crate)`：OAuth2 统一认证的 token 有效性判定复用。
    pub(crate) fn is_valid(&self, now: u64) -> bool {
        !self.access_token.is_empty() && (self.expires_at == 0 || now < self.expires_at)
    }

    /// 登录用户名：优先存盘的 `username` 字段；旧文件里只有 id_token（token
    /// 响应不带 `username` 字段时，见 `post_token`）则从 id_token 载荷解出。
    pub fn username(&self) -> Option<String> {
        self.username
            .clone()
            .or_else(|| self.id_token.as_deref().and_then(decode_jwt_username))
    }
}

/// `tj_auth.json` 读写：原子写（临时文件 + rename），类 Unix 权限 0600。
#[derive(Debug, Clone)]
pub struct TjTokenStore {
    path: PathBuf,
}

impl TjTokenStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<TjTokens, String> {
        if !self.path.exists() {
            return Ok(TjTokens::default());
        }
        let text = std::fs::read_to_string(&self.path).map_err(|error| {
            qcode_i18n::trp(
                "provider.tj.store_read",
                &[self.path.display().to_string(), error.to_string()],
            )
        })?;
        serde_json::from_str(&text).map_err(|error| {
            qcode_i18n::trp(
                "provider.tj.store_parse",
                &[self.path.display().to_string(), error.to_string()],
            )
        })
    }

    pub fn save(&self, tokens: &TjTokens) -> Result<(), String> {
        if let Some(dir) = self.path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir).map_err(|error| {
                qcode_i18n::trp(
                    "provider.tj.store_create",
                    &[dir.display().to_string(), error.to_string()],
                )
            })?;
        }
        let text = serde_json::to_string_pretty(tokens).map_err(|error| {
            qcode_i18n::trp("provider.tj.store_serialize", &[error.to_string()])
        })?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, text).map_err(|error| {
            qcode_i18n::trp(
                "provider.tj.store_write",
                &[tmp.display().to_string(), error.to_string()],
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(
                |error| {
                    qcode_i18n::trp(
                        "provider.tj.store_chmod",
                        &[tmp.display().to_string(), error.to_string()],
                    )
                },
            )?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|error| {
            qcode_i18n::trp(
                "provider.tj.store_move",
                &[self.path.display().to_string(), error.to_string()],
            )
        })
    }
}

/// 太极统一认证的 OAuth 客户端：授权码（PKCE）/ refresh 两段 HTTP。
#[derive(Debug, Clone)]
pub struct TjAuthClient {
    client: reqwest::Client,
    settings: TjAuthSettings,
}

impl TjAuthClient {
    pub fn new(settings: TjAuthSettings, proxy_url: Option<&str>) -> Result<Self, ProviderError> {
        let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(10));
        if let Some(proxy) = proxy_url {
            let proxy = reqwest::Proxy::all(proxy)
                .map_err(|error| ProviderError::Network(error.to_string()))?;
            builder = builder.proxy(proxy).no_proxy();
        }
        let client = builder
            .build()
            .map_err(|error| ProviderError::Network(error.to_string()))?;
        Ok(Self { client, settings })
    }

    pub fn settings(&self) -> &TjAuthSettings {
        &self.settings
    }

    /// 授权码 + PKCE `code_verifier` 换 token（不带 client_secret，public client）。
    pub async fn exchange_code(&self, code: &str, code_verifier: &str) -> Result<TjTokens, String> {
        let params = [
            ("grant_type", "authorization_code"),
            ("client_id", self.settings.client_id.as_str()),
            ("code", code),
            ("redirect_uri", self.settings.redirect_uri.as_str()),
            ("code_verifier", code_verifier),
        ];
        self.post_token(&params).await
    }

    /// refresh_token 换新 token。
    pub async fn refresh(&self, refresh_token: &str) -> Result<TjTokens, String> {
        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", self.settings.client_id.as_str()),
            ("refresh_token", refresh_token),
        ];
        self.post_token(&params).await
    }

    async fn post_token(&self, params: &[(&str, &str)]) -> Result<TjTokens, String> {
        let response = self
            .client
            .post(&self.settings.token_url)
            .form(&params)
            .send()
            .await
            .map_err(|error| qcode_i18n::trp("provider.tj.token_request", &[error.to_string()]))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(qcode_i18n::trp(
                "provider.tj.token_status",
                &[
                    self.settings.token_url.clone(),
                    status.to_string(),
                    body.clone(),
                ],
            ));
        }
        let value: serde_json::Value = serde_json::from_str(&body)
            .map_err(|error| qcode_i18n::trp("provider.tj.token_not_json", &[error.to_string()]))?;
        let access_token = value
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| qcode_i18n::trp("provider.tj.token_no_access_token", &[body.as_str()]))?
            .to_string();
        let refresh_token = value
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let expires_at = value
            .get("expires_in")
            .and_then(serde_json::Value::as_u64)
            .map(|seconds| now_secs().saturating_add(seconds))
            .or_else(|| decode_jwt_exp(&access_token))
            .unwrap_or(0);
        let id_token = value
            .get("id_token")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        // 太极的 token 响应不带 `username` 字段（2026-11 实测），用户名从
        // id_token 载荷兜底解出（`preferred_username`，退 `sub`）。
        let username = value
            .get("username")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| id_token.as_deref().and_then(decode_jwt_username));
        Ok(TjTokens {
            access_token,
            refresh_token,
            expires_at,
            id_token,
            username,
        })
    }
}

/// TjProvider 的构造配置。
#[derive(Clone)]
pub struct TjProviderConfig {
    pub name: String,
    /// LLM 网关地址（`http://192.168.69.126:6573/v1`，需求 4c）。
    pub base_url: String,
    pub settings: TjAuthSettings,
    /// `tj_auth.json` 的路径（与 config.toml 同目录）。
    pub store_path: PathBuf,
    /// 授权回调；None = 无交互能力（CLI）。
    pub prompt: Option<AuthPrompt>,
    pub proxy_url: Option<String>,
    pub stream_usage: bool,
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    pub headers: BTreeMap<String, String>,
    pub images: bool,
}

// `AuthPrompt`（`Arc<dyn Fn …>`）不实现 Debug，手写跳过。
impl std::fmt::Debug for TjProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TjProviderConfig")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("settings", &self.settings)
            .field("store_path", &self.store_path)
            .field("prompt", &self.prompt.as_ref().map(|_| "<prompt>"))
            .field("proxy_url", &self.proxy_url)
            .field("stream_usage", &self.stream_usage)
            .field("images", &self.images)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// 太极统一认证 Provider：token 生命周期（ensure / refresh / 401 重试）在内部闭环。
#[derive(Clone)]
pub struct TjProvider {
    config: TjProviderConfig,
    auth: TjAuthClient,
    store: TjTokenStore,
}

impl std::fmt::Debug for TjProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TjProvider")
            .field("config", &self.config)
            .field("auth", &self.auth)
            .field("store", &self.store)
            .finish()
    }
}

impl TjProvider {
    pub fn new(config: TjProviderConfig) -> Result<Self, ProviderError> {
        let auth = TjAuthClient::new(config.settings.clone(), config.proxy_url.as_deref())?;
        let store = TjTokenStore::new(config.store_path.clone());
        Ok(Self {
            config,
            auth,
            store,
        })
    }

    /// 触发一次完整的授权码流程（PKCE S256）并把 token 写入 `tj_auth.json`
    /// （设置页「登录」按钮与需要即时登录的场景）。已登录时幂等：有有效
    /// token 直接返回，不重复弹登录页。
    pub async fn login(&self) -> Result<(), ProviderError> {
        self.ensure_token().await.map(|_| ())
    }

    /// 主界面「太极内网登录」按钮用的路径：**先无条件刷新**（有
    /// `refresh_token` 才试），失败再走完整授权码流程（PKCE S256）。
    ///
    /// 与 [`Self::login`] 的区别：`login` 在本地 token 未过期时直接返回——
    /// 401 时本地 access token 可能仍显示“有效”（被吊销/被踢），那个短路
    /// 会让按钮点了等于没点，这里必须真的换一次才管用。
    pub async fn refresh_or_login(&self) -> Result<(), ProviderError> {
        let stored = self.store.load().unwrap_or_default();
        if !stored.refresh_token.is_empty()
            && let Ok(fresh) = self.auth.refresh(&stored.refresh_token).await
        {
            let _ = self.store.save(&fresh);
            return Ok(());
        }
        // 刷新失败（或无 refresh_token）→ 走与 401 重试相同的兜底：授权码流程。
        self.ensure_token().await.map(|_| ())
    }

    /// 确保有一个可用的 access token：有效则直接用；无/过期先试 refresh，再
    /// 失败走授权码流程（PKCE S256）。
    async fn ensure_token(&self) -> Result<String, ProviderError> {
        let stored = self.store.load().unwrap_or_default();
        if stored.is_valid(now_secs()) {
            return Ok(stored.access_token);
        }
        if !stored.refresh_token.is_empty()
            && let Ok(fresh) = self.auth.refresh(&stored.refresh_token).await
        {
            let _ = self.store.save(&fresh);
            return Ok(fresh.access_token);
        }
        // 授权码流程（PKCE S256）：生成 verifier/challenge，打开登录页取 code，
        // 用 code + verifier 换 token 并落盘。
        let (verifier, challenge) = pkce_pair();
        // `state` 防串号/CSRF：kernel 侧的回调处理会校验它原样带回
        // （对齐 VSCode 扩展 _pendingStates 的做法，设计文档 §4.3）。
        let state = random_state();
        let auth_url = reqwest::Url::parse_with_params(
            &self.config.settings.auth_url,
            &[
                ("response_type", "code"),
                ("client_id", self.config.settings.client_id.as_str()),
                ("redirect_uri", self.config.settings.redirect_uri.as_str()),
                ("scope", "openid"),
                ("state", state.as_str()),
                ("code_challenge_method", "S256"),
                ("code_challenge", challenge.as_str()),
            ],
        )
        .map_err(|error| {
            ProviderError::Auth(qcode_i18n::trp(
                "provider.tj.auth_url",
                &[error.to_string()],
            ))
        })?;
        let code = match &self.config.prompt {
            Some(prompt) => prompt(auth_url.to_string()).ok_or_else(|| {
                ProviderError::Auth(qcode_i18n::tr("provider.tj.login_incomplete").to_string())
            })?,
            None => {
                return Err(ProviderError::Auth(
                    qcode_i18n::tr("provider.tj.not_signed_in").to_string(),
                ));
            }
        };
        let tokens = self
            .auth
            .exchange_code(&code, &verifier)
            .await
            .map_err(ProviderError::Auth)?;
        let _ = self.store.save(&tokens);
        Ok(tokens.access_token)
    }

    /// 401 后的 token 恢复：先 refresh，失败再走授权码流程。
    async fn refresh_or_reauth(&self) -> Result<String, ProviderError> {
        let stored = self.store.load().unwrap_or_default();
        if !stored.refresh_token.is_empty()
            && let Ok(fresh) = self.auth.refresh(&stored.refresh_token).await
        {
            let _ = self.store.save(&fresh);
            return Ok(fresh.access_token);
        }
        self.ensure_token().await
    }
}

#[async_trait]
impl Provider for TjProvider {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tools: true,
            parallel_tools: true,
            streaming_tool_args: true,
            reasoning: true,
            prompt_cache: true,
            images: self.config.images,
            max_context: None,
            max_output: None,
        }
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, ProviderError> {
        let token = self.ensure_token().await?;
        let mut token = token;
        loop {
            // 把当前 token 当作 api_key 复用 OpenAI 兼容的请求构造与 SSE 解析；
            // 401 会在响应头阶段出现（openai_compat 对非成功状态返回 Err(Auth)），
            // 所以流一旦建立就不会中途 401，重试闭环只需要处理 Err(Auth)。
            let inner = OpenAiCompatProvider::new(OpenAiCompatConfig {
                name: self.config.name.clone(),
                base_url: self.config.base_url.clone(),
                api_key: Some(token.clone()),
                stream_usage: self.config.stream_usage,
                proxy_url: self.config.proxy_url.clone(),
                extra_body: self.config.extra_body.clone(),
                headers: self.config.headers.clone(),
                images: self.config.images,
            })?;
            match inner.stream(request.clone(), cancel.clone()).await {
                Err(ProviderError::Auth(_)) => match self.refresh_or_reauth().await {
                    Ok(new_token) => {
                        token = new_token;
                        continue;
                    }
                    Err(error) => return Err(error),
                },
                other => return other,
            }
        }
    }
}

/// 当前 Unix 秒。
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// PKCE S256：43 字符 verifier + 对应 challenge。`pub(crate)`：OAuth2 统一
/// 认证 Provider（`oauth2.rs`）的 PKCE 模式复用同一套生成。
pub(crate) fn pkce_pair() -> (String, String) {
    let mut bytes = [0u8; 32];
    let _ = getrandom::getrandom(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// OAuth `state` 参数：16 字节 OS 随机数 → 32 字符 hex。回调时由 kernel 侧
/// 校验它原样带回（防旧链接/伪造触发登录）。`pub(crate)`：OAuth2 复用。
pub(crate) fn random_state() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::getrandom(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 从 access_token（JWT）的载荷里解出 `exp`（Unix 秒）。本地**不解签**，仅
/// 读取过期时间；无 `exp` 时返回 None（由 `expires_in` 兜底）。
/// `pub(crate)`：OAuth2 统一认证的 token 解析复用。
pub(crate) fn decode_jwt_exp(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("exp").and_then(serde_json::Value::as_u64)
}

/// 从 id_token（JWT）载荷里取登录用户名：`preferred_username`，退 `sub`。
/// 与 `decode_jwt_exp` 一样本地不解签，只读载荷；签名由服务端校验。
/// `pub(crate)`：OAuth2 统一认证的用户名兜底复用。
pub(crate) fn decode_jwt_username(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("preferred_username")
        .or_else(|| value.get("sub"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// 太极内网模型发现的固定端点与插件凭据（与 kernel 侧 PROVIDER-PACKAGES-TJ-AUTH
/// 配套的插件接口，2026-11 实测值）。模型列表**不是**通用的
/// `GET {base}/models`：需要先用登录用户名到密钥服务换 plugin key，再用该
/// key 带 `X-App-ID` 请求模型列表。
const TJ_GET_KEY_URL: &str = "http://192.168.242.214:3344/api/get_key";
const TJ_GET_KEY_SECRET: &str = "7XTwQgNhWye3yUzOjrQeZLfVFGzpEeN52vgC6sIVWB1udoZF7";
const TJ_MODELS_URL: &str = "http://192.168.242.214:8888/v1/models";
const TJ_MODELS_APP_ID: &str = "tj_plugin_prod_001";

/// 太极内网的模型发现：先用 `username`（登录成功后写入 `tj_auth.json`）到
/// 密钥服务换 plugin key，再用该 key 带 `X-App-ID` 请求模型列表。结果与
/// 通用发现一样是候选元数据（`id` / `name` / 上下文窗口 / 输出上限），
/// 由调用方决定采纳哪些。
pub async fn discover_tj_models(
    username: &str,
) -> Result<Vec<crate::discovery::ModelCandidate>, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| qcode_i18n::trp("provider.discover.http_client", &[error.to_string()]))?;
    let key = fetch_tj_key(&client, username).await?;
    let response = client
        .get(TJ_MODELS_URL)
        .header("X-App-ID", TJ_MODELS_APP_ID)
        .bearer_auth(&key)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| {
            qcode_i18n::trp(
                "provider.discover.get_failed",
                &[TJ_MODELS_URL.to_string(), error.to_string()],
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(qcode_i18n::trp(
            "provider.discover.get_status",
            &[TJ_MODELS_URL.to_string(), status.to_string(), text],
        ));
    }
    let body = response.text().await.map_err(|error| {
        qcode_i18n::trp(
            "provider.discover.read_body",
            &[TJ_MODELS_URL.to_string(), error.to_string()],
        )
    })?;
    crate::discovery::parse_model_candidates(&body, TJ_MODELS_URL)
}

/// 用户名 → plugin key：`POST /api/get_key`，`Secret` 头 + JSON body。
async fn fetch_tj_key(client: &reqwest::Client, username: &str) -> Result<String, String> {
    let response = client
        .post(TJ_GET_KEY_URL)
        .header("Secret", TJ_GET_KEY_SECRET)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&serde_json::json!({ "username": username }))
        .send()
        .await
        .map_err(|error| qcode_i18n::trp("provider.tj.get_key_request", &[error.to_string()]))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(qcode_i18n::trp(
            "provider.tj.get_key_status",
            &[TJ_GET_KEY_URL.to_string(), status.to_string(), body.clone()],
        ));
    }
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| qcode_i18n::trp("provider.tj.get_key_not_json", &[error.to_string()]))?;
    let key = value
        .pointer("/keys/0/key")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| qcode_i18n::trp("provider.tj.get_key_no_key", &[body.as_str()]))?
        .to_string();
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_base64url() {
        let (verifier, challenge) = pkce_pair();
        assert_eq!(verifier.len(), 43);
        assert_eq!(challenge.len(), 43);
        // challenge 必须等于 SHA256(verifier) 的 base64url 编码。
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected);
    }

    #[test]
    fn state_is_a_random_32_char_hex_string() {
        let a = random_state();
        let b = random_state();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "两次随机 state 不该相同");
    }

    #[test]
    fn token_validity_considers_expiry() {
        let now = now_secs();
        let fresh = TjTokens {
            access_token: "tok".to_string(),
            expires_at: now + 60,
            ..Default::default()
        };
        assert!(fresh.is_valid(now));
        let expired = TjTokens {
            access_token: "tok".to_string(),
            expires_at: now - 1,
            ..Default::default()
        };
        assert!(!expired.is_valid(now));
        let unknown = TjTokens {
            access_token: "tok".to_string(),
            expires_at: 0,
            ..Default::default()
        };
        // expires_at = 0 表示未知，视为有效（由服务端 401 兜底）。
        assert!(unknown.is_valid(now));
        let empty = TjTokens::default();
        assert!(!empty.is_valid(now));
    }

    #[test]
    fn jwt_exp_is_read_without_signature_verification() {
        // {alg:none}.{exp:1700000000}.
        let token = "eyJhbGciOiJub25lIn0.eyJleHAiOjE3MDAwMDAwMDB9.";
        assert_eq!(decode_jwt_exp(token), Some(1700000000));
        assert_eq!(decode_jwt_exp("not-a-jwt"), None);
    }

    #[test]
    fn token_store_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("qcode-tj-test-{}", std::process::id()));
        let store = TjTokenStore::new(dir.join("tj_auth.json"));
        let tokens = TjTokens {
            access_token: "a".to_string(),
            refresh_token: "r".to_string(),
            expires_at: 123,
            username: Some("zhang.san".to_string()),
            ..Default::default()
        };
        store.save(&tokens).expect("save");
        let loaded = store.load().expect("load");
        assert_eq!(loaded.access_token, "a");
        assert_eq!(loaded.refresh_token, "r");
        assert_eq!(loaded.expires_at, 123);
        assert_eq!(loaded.username.as_deref(), Some("zhang.san"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_store_loads_empty() {
        let dir = std::env::temp_dir().join(format!("qcode-tj-missing-{}", std::process::id()));
        let store = TjTokenStore::new(dir.join("tj_auth.json"));
        let loaded = store.load().expect("load");
        assert!(loaded.access_token.is_empty());
        assert!(!loaded.is_valid(now_secs()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
