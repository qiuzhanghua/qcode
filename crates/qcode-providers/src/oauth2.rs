//! 可配置 OAuth2 统一认证 Provider。
//!
//! 与太极统一认证（`tj.rs`，端点/客户端硬编码 + 固定 PKCE）不同：本 Provider
//! 的端点、客户端标识、登录模式全部来自用户配置（存于 config.toml 的
//! `providers.<route>.oauth2`）。支持两种 OAuth2 登录流程：
//!
//! - 授权码（`AuthorizationCode`，confidential client）：授权 URL 不带
//!   `code_challenge`，token 端点用 `code + client_secret` 换 token；
//! - PKCE（`Pkce`，public client）：授权 URL 带 S256 `code_challenge`，token
//!   端点用 `code + code_verifier` 换 token（复用 `tj` 的 PKCE 工具）。
//!
//! 请求模型网关时把 access token 放进 `Authorization: Bearer`（复用
//! `OpenAiCompatProvider` 的 `api_key` 通道），401 时先 refresh、失败再走
//! 授权流程，重试闭环在本 Provider 内部。token 生命周期与存储（`oauth2_auth.json`）
//! 复用 tj 的 `TjTokenStore` / `TjTokens`。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use qcode_core::provider::{Capabilities, CompletionRequest, EventStream, Provider, ProviderError};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::tj::{
    AuthPrompt, TjTokenStore, TjTokens, decode_jwt_exp, decode_jwt_username, pkce_pair,
    random_state,
};

/// OAuth2 登录模式：授权码（confidential，带 client_secret）或 PKCE（public，
/// 无 secret）。`Serialize` 供 config 写盘复用（kernel 侧 `OAuth2AuthConfig`
/// 直接搬运该值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuth2Mode {
    #[default]
    AuthorizationCode,
    Pkce,
}

impl OAuth2Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AuthorizationCode => "authorization_code",
            Self::Pkce => "pkce",
        }
    }
}

/// 可配置的 OAuth2 统一认证端点与客户端标识（值由 kernel 侧 config 搬运而来）。
#[derive(Debug, Clone)]
pub struct OAuth2AuthSettings {
    pub mode: OAuth2Mode,
    pub client_id: String,
    /// 授权码模式（confidential client）的客户端密钥；PKCE 模式为 None。
    pub client_secret: Option<String>,
    pub auth_url: String,
    pub token_url: String,
    pub scope: Option<String>,
    /// 回调地址；缺省用 loopback（`127.0.0.1:43782/oauth2/callback`，
    /// 与太极的监听端口一致，见 kernel 侧 loopback 模块）。
    pub redirect_uri: String,
}

/// 可配置 OAuth2 的 OAuth 客户端：授权码（带/不带 PKCE）/ refresh 两段 HTTP。
#[derive(Debug, Clone)]
pub struct OAuth2AuthClient {
    client: reqwest::Client,
    settings: OAuth2AuthSettings,
}

impl OAuth2AuthClient {
    pub fn new(
        settings: OAuth2AuthSettings,
        proxy_url: Option<&str>,
    ) -> Result<Self, ProviderError> {
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

    pub fn settings(&self) -> &OAuth2AuthSettings {
        &self.settings
    }

    /// 授权码换 token：PKCE 模式带 `code_verifier`（无 secret）；授权码模式
    /// 带 `client_secret`（有 secret 时）。
    pub async fn exchange_code(
        &self,
        code: &str,
        code_verifier: Option<&str>,
        client_secret: Option<&str>,
    ) -> Result<TjTokens, String> {
        let params = exchange_params(&self.settings, code, code_verifier, client_secret);
        let params: Vec<(&str, &str)> = params
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        self.post_token(&params).await
    }

    /// refresh_token 换新 token（两种模式同构：不带 secret / verifier）。
    pub async fn refresh(&self, refresh_token: &str) -> Result<TjTokens, String> {
        let params = refresh_params(&self.settings, refresh_token);
        let params: Vec<(&str, &str)> = params
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        self.post_token(&params).await
    }

    async fn post_token(&self, params: &[(&str, &str)]) -> Result<TjTokens, String> {
        let response = self
            .client
            .post(&self.settings.token_url)
            .form(&params)
            .send()
            .await
            .map_err(|error| {
                qcode_i18n::trp("provider.oauth2.token_request", &[error.to_string()])
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(qcode_i18n::trp(
                "provider.oauth2.token_status",
                &[
                    self.settings.token_url.clone(),
                    status.to_string(),
                    body.clone(),
                ],
            ));
        }
        let value: serde_json::Value = serde_json::from_str(&body).map_err(|error| {
            qcode_i18n::trp("provider.oauth2.token_not_json", &[error.to_string()])
        })?;
        let access_token = value
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                qcode_i18n::trp("provider.oauth2.token_no_access_token", &[body.as_str()])
            })?
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
        // token 响应可能不带 `username` 字段；用户名从 id_token 载荷兜底解出。
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

/// OAuth2Provider 的构造配置。
#[derive(Clone)]
pub struct OAuth2ProviderConfig {
    pub name: String,
    /// LLM 网关地址（用户配置，`GET {base}/models` / `POST {base}/chat/completions`）。
    pub base_url: String,
    pub settings: OAuth2AuthSettings,
    /// `oauth2_auth.json` 的路径（与 config.toml 同目录）。
    pub store_path: PathBuf,
    /// 授权回调；None = 无交互能力（CLI / 引擎侧任务运行时）。
    pub prompt: Option<AuthPrompt>,
    pub proxy_url: Option<String>,
    pub stream_usage: bool,
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    pub headers: BTreeMap<String, String>,
    pub images: bool,
}

// `AuthPrompt`（`Arc<dyn Fn …>`）不实现 Debug，手写跳过。
impl std::fmt::Debug for OAuth2ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2ProviderConfig")
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

/// 可配置 OAuth2 统一认证 Provider：token 生命周期（ensure / refresh / 401
/// 重试）在内部闭环。
#[derive(Clone)]
pub struct OAuth2Provider {
    config: OAuth2ProviderConfig,
    auth: OAuth2AuthClient,
    store: TjTokenStore,
}

impl std::fmt::Debug for OAuth2Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2Provider")
            .field("config", &self.config)
            .field("auth", &self.auth)
            .field("store", &self.store)
            .finish()
    }
}

impl OAuth2Provider {
    pub fn new(config: OAuth2ProviderConfig) -> Result<Self, ProviderError> {
        let auth = OAuth2AuthClient::new(config.settings.clone(), config.proxy_url.as_deref())?;
        let store = TjTokenStore::new(config.store_path.clone());
        Ok(Self {
            config,
            auth,
            store,
        })
    }

    /// 触发一次完整的登录流程（授权码 / PKCE，按配置的 mode）并把 token 写入
    /// `oauth2_auth.json`。已登录时幂等：有有效 token 直接返回。
    pub async fn login(&self) -> Result<(), ProviderError> {
        self.ensure_token().await.map(|_| ())
    }

    /// 先无条件刷新（有 `refresh_token` 才试），失败再走完整登录流程。与
    /// [`Self::login`] 的区别同 tj：`login` 在本地 token 未过期时短路，401 时
    /// 点了等于没点，这里必须真的换一次。
    pub async fn refresh_or_login(&self) -> Result<(), ProviderError> {
        let stored = self.store.load().unwrap_or_default();
        if !stored.refresh_token.is_empty()
            && let Ok(fresh) = self.auth.refresh(&stored.refresh_token).await
        {
            let _ = self.store.save(&fresh);
            return Ok(());
        }
        self.ensure_token().await.map(|_| ())
    }

    /// 确保有一个可用的 access token：有效则直接用；无/过期先试 refresh，再
    /// 失败走登录流程（按配置的 mode）。
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
        // 登录流程：PKCE 模式生成 verifier/challenge；授权码模式不带 challenge。
        let (verifier, challenge) = match self.config.settings.mode {
            OAuth2Mode::Pkce => {
                let (verifier, challenge) = pkce_pair();
                (Some(verifier), Some(challenge))
            }
            OAuth2Mode::AuthorizationCode => (None, None),
        };
        let state = random_state();
        let auth_url = authorize_url(&self.config.settings, challenge.as_deref(), &state)?;
        let code = match &self.config.prompt {
            Some(prompt) => prompt(auth_url.to_string()).ok_or_else(|| {
                ProviderError::Auth(qcode_i18n::tr("provider.oauth2.login_incomplete").to_string())
            })?,
            None => {
                return Err(ProviderError::Auth(
                    qcode_i18n::tr("provider.oauth2.not_signed_in").to_string(),
                ));
            }
        };
        let tokens = self
            .auth
            .exchange_code(
                &code,
                verifier.as_deref(),
                self.config.settings.client_secret.as_deref(),
            )
            .await
            .map_err(ProviderError::Auth)?;
        let _ = self.store.save(&tokens);
        Ok(tokens.access_token)
    }

    /// 401 后的 token 恢复：先 refresh，失败再走登录流程。
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
impl Provider for OAuth2Provider {
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
            // 把当前 token 当作 api_key 复用 OpenAI 兼容的请求构造与 SSE 解析
            // （`Authorization: Bearer <token>`）；401 在响应头阶段出现，流一旦
            // 建立就不会中途 401，重试闭环只需处理 Err(Auth)。
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

/// 授权页 URL。`challenge` 为 `Some` 时补 S256 参数（只有 PKCE 模式会）。
///
/// 抽成自由函数是为了让「PKCE 带 challenge、授权码模式不带」这条**安全相关**的
/// 差别能被单测钉住——它原先内联在 `ensure_token` 里，而那条路径必须先起浏览器
/// 才能走到，测不了。
fn authorize_url(
    settings: &OAuth2AuthSettings,
    challenge: Option<&str>,
    state: &str,
) -> Result<reqwest::Url, ProviderError> {
    // scope 缺省补 `openid`：统一认证普遍按 OIDC 子集实现，不带 scope 会被拒。
    let scope = settings.scope.as_deref().unwrap_or("openid");
    let mut params = vec![
        ("response_type", "code"),
        ("client_id", settings.client_id.as_str()),
        ("redirect_uri", settings.redirect_uri.as_str()),
        ("scope", scope),
        ("state", state),
    ];
    if let Some(challenge) = challenge {
        params.push(("code_challenge_method", "S256"));
        params.push(("code_challenge", challenge));
    }
    reqwest::Url::parse_with_params(&settings.auth_url, &params).map_err(|error| {
        ProviderError::Auth(qcode_i18n::trp(
            "provider.oauth2.auth_url",
            &[error.to_string()],
        ))
    })
}

/// code 换 token 的表单参数。
///
/// 两种模式的差别就在这里：PKCE 带 `code_verifier`、**不带** `client_secret`；
/// 授权码模式反过来。写错的方向是「public client 把 secret 发出去」或
/// 「confidential client 不带 secret 被拒」，两种都值得一条测试。
fn exchange_params(
    settings: &OAuth2AuthSettings,
    code: &str,
    code_verifier: Option<&str>,
    client_secret: Option<&str>,
) -> Vec<(String, String)> {
    let mut params = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("client_id".to_string(), settings.client_id.clone()),
        ("code".to_string(), code.to_string()),
        ("redirect_uri".to_string(), settings.redirect_uri.clone()),
    ];
    if let Some(verifier) = code_verifier {
        params.push(("code_verifier".to_string(), verifier.to_string()));
    }
    if let Some(secret) = client_secret {
        params.push(("client_secret".to_string(), secret.to_string()));
    }
    params
}

/// refresh 的表单参数。两种模式同构，且**都不带** `client_secret`
/// （PKCE 没有可带的；授权码模式的 refresh 端点按 RFC 6749 §6 也只认 client
/// 认证，而我们是 public 客户端路径的复用，实测两种网关都不需要）。
fn refresh_params(settings: &OAuth2AuthSettings, refresh_token: &str) -> Vec<(String, String)> {
    vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("client_id".to_string(), settings.client_id.clone()),
        ("refresh_token".to_string(), refresh_token.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(mode: OAuth2Mode) -> OAuth2AuthSettings {
        OAuth2AuthSettings {
            mode,
            client_id: "cli".to_string(),
            client_secret: match mode {
                OAuth2Mode::AuthorizationCode => Some("sec".to_string()),
                OAuth2Mode::Pkce => None,
            },
            auth_url: "https://auth.example.com/oauth2/auth".to_string(),
            token_url: "https://auth.example.com/oauth2/token".to_string(),
            scope: None,
            redirect_uri: "http://127.0.0.1:43782/oauth2/callback".to_string(),
        }
    }

    fn query(url: &reqwest::Url) -> BTreeMap<String, String> {
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    #[test]
    fn pkce_mode_builds_auth_url_with_challenge() {
        let url = authorize_url(&settings(OAuth2Mode::Pkce), Some("ch"), "st").expect("url");
        let params = query(&url);
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(params.get("client_id").map(String::as_str), Some("cli"));
        assert_eq!(params.get("state").map(String::as_str), Some("st"));
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(params.get("code_challenge").map(String::as_str), Some("ch"));
        // 缺省 scope 是 openid。
        assert_eq!(params.get("scope").map(String::as_str), Some("openid"));
    }

    #[test]
    fn authorization_code_mode_has_no_verifier_path() {
        // 授权码模式不带 challenge 的任何一个参数；带上会被认证服务器按 PKCE
        // 处理，而 confidential client 又没有 verifier，换 token 必然失败。
        let url = authorize_url(&settings(OAuth2Mode::AuthorizationCode), None, "st").expect("url");
        let params = query(&url);
        assert!(!params.contains_key("code_challenge"));
        assert!(!params.contains_key("code_challenge_method"));
    }

    #[test]
    fn a_configured_scope_replaces_the_openid_default() {
        let mut settings = settings(OAuth2Mode::Pkce);
        settings.scope = Some("openid profile email".to_string());
        let url = authorize_url(&settings, None, "st").expect("url");
        assert_eq!(
            query(&url).get("scope").map(String::as_str),
            Some("openid profile email")
        );
    }

    #[test]
    fn pkce_exchange_sends_a_verifier_and_never_a_secret() {
        let settings = settings(OAuth2Mode::Pkce);
        let params = exchange_params(&settings, "c", Some("v"), None);
        let keys: Vec<&str> = params.iter().map(|(key, _)| key.as_str()).collect();
        assert!(keys.contains(&"code_verifier"));
        // public client 把 secret 发出去是泄露，不只是错误。
        assert!(!keys.contains(&"client_secret"));
    }

    #[test]
    fn authorization_code_exchange_sends_the_secret() {
        let settings = settings(OAuth2Mode::AuthorizationCode);
        let params = exchange_params(&settings, "c", None, Some("sec"));
        let keys: Vec<&str> = params.iter().map(|(key, _)| key.as_str()).collect();
        assert!(keys.contains(&"client_secret"));
        assert!(!keys.contains(&"code_verifier"));
    }

    #[test]
    fn refresh_never_carries_a_secret_or_verifier() {
        let params = refresh_params(&settings(OAuth2Mode::AuthorizationCode), "r");
        let keys: Vec<&str> = params.iter().map(|(key, _)| key.as_str()).collect();
        assert!(!keys.contains(&"client_secret"));
        assert!(!keys.contains(&"code_verifier"));
        assert!(keys.contains(&"refresh_token"));
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
        let empty = TjTokens::default();
        assert!(!empty.is_valid(now));
    }

    #[test]
    fn jwt_exp_is_read_without_signature_verification() {
        let token = "eyJhbGciOiJub25lIn0.eyJleHAiOjE3MDAwMDAwMDB9.";
        assert_eq!(decode_jwt_exp(token), Some(1700000000));
        assert_eq!(decode_jwt_exp("not-a-jwt"), None);
    }

    #[test]
    fn token_store_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("qcode-oauth2-test-{}", std::process::id()));
        let store = TjTokenStore::new(dir.join("oauth2_auth.json"));
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
}
