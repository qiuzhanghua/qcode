//! 太极统一认证（Taiji Unified Auth）Provider 的硬编码配置。
//!
//! 设计文档 PROVIDER-PACKAGES-TJ-AUTH §4：OAuth 端点与客户端标识全部硬编码在
//! 代码中（需求 4a）。客户端为 public + PKCE S256（2026-11-23 实测通过），
//! token 端点不校验 client_secret，因此**无需硬编码任何密钥**。

/// 太极统一认证端点与客户端标识（实测值，2026-11-23）。
pub struct TjAuthConfig {
    /// 授权端点（Keycloak 登录页）。
    pub auth_url: &'static str,
    /// token 端点（code / refresh_token → token）。
    pub token_url: &'static str,
    /// 用户信息端点（太极自定义网关路径，非 well-known 的 `/protocol/` 版本）。
    pub userinfo_url: &'static str,
    /// JWK 集合端点（预留给验签场景，当前流程未强制验签）。
    pub jwk_set_url: &'static str,
    /// 客户端 ID（public client，无 secret）。
    pub client_id: &'static str,
    /// 客户端名称。
    pub client_name: &'static str,
    /// 回调地址（loopback，登录时起临时 HTTP server 接收；见设计文档 §4.3）。
    pub redirect_uri: &'static str,
}

pub const TJ_AUTH: TjAuthConfig = TjAuthConfig {
    auth_url: "https://auth.taiji.com.cn/realms/taiji/protocol/openid-connect/auth",
    token_url: "https://auth.taiji.com.cn/realms/taiji/protocol/openid-connect/token",
    userinfo_url: "https://auth.taiji.com.cn/realms/taiji/resource/openid-connect/userinfo",
    jwk_set_url: "https://auth.taiji.com.cn/realms/taiji/protocol/openid-connect/certs",
    client_id: "qcode",
    client_name: "qcode",
    redirect_uri: "http://127.0.0.1:43782/oauth2/callback",
};

/// 太极 route 判定与默认模型请求地址（需求 4c）。
pub const TJ_ROUTE: &str = "tj";
pub const TJ_BASE_URL: &str = "http://192.168.69.126:6573/v1";

/// 该 route 是否是太极统一认证 Provider。
pub fn is_tj_route(route: &str) -> bool {
    route == TJ_ROUTE
}

/// 进程级授权回调（D5）：GUI 启动时注册一次，供所有 TjProvider 在需要授权时
/// 打开登录页并取回 code；CLI / 无人值守不注册，保持 None。
static GLOBAL_AUTH_PROMPT: std::sync::OnceLock<qcode_providers::AuthPrompt> =
    std::sync::OnceLock::new();

/// 注册全局授权回调（GUI 启动时调用；重复注册幂等，首个生效）。
pub fn set_global_auth_prompt(prompt: qcode_providers::AuthPrompt) {
    let _ = GLOBAL_AUTH_PROMPT.set(prompt);
}

/// 取全局授权回调；未注册时返回 None。
pub fn global_auth_prompt() -> Option<qcode_providers::AuthPrompt> {
    GLOBAL_AUTH_PROMPT.get().cloned()
}

/// 默认授权回调实现（GUI 启动时注册为全局回调，见设计文档 §4.3）：委托给
/// 共享的 loopback 实现（`providers::loopback`），回调页文案走太极的 key。
/// 授权 URL 里 `redirect_uri` 的端口会被 loopback 实现采纳（太极固定为
/// [`LOOPBACK_PORT`]），行为与旧版一致。
pub fn open_browser_and_wait(auth_url: String) -> Option<String> {
    crate::providers::loopback::open_browser_and_wait_with(
        auth_url,
        crate::providers::loopback::CallbackKeys {
            success: "provider.tj.callback_success",
            rejected: "provider.tj.callback_rejected",
        },
    )
}

/// 手动注入授权 code 的通道（设置页「粘贴 code」兜底）。实现见
/// [`crate::providers::loopback::submit_auth_code`]。
pub use crate::providers::loopback::submit_auth_code;

/// loopback 回调端口（太极 Keycloak 白名单里配置的地址，见设计文档 §4.3）。
pub use crate::providers::loopback::LOOPBACK_PORT;

/// 硬编码配置 → providers 层的 `TjAuthSettings`（qcode-providers 不依赖 kernel，
/// 值在这里搬运）。
fn tj_settings() -> qcode_providers::TjAuthSettings {
    qcode_providers::TjAuthSettings {
        auth_url: TJ_AUTH.auth_url.to_string(),
        token_url: TJ_AUTH.token_url.to_string(),
        userinfo_url: TJ_AUTH.userinfo_url.to_string(),
        jwk_set_url: TJ_AUTH.jwk_set_url.to_string(),
        client_id: TJ_AUTH.client_id.to_string(),
        client_name: TJ_AUTH.client_name.to_string(),
        redirect_uri: TJ_AUTH.redirect_uri.to_string(),
    }
}

/// 构造 TjProvider（配置硬编码 + store 路径 + 授权回调）。设置页「登录」与
/// 主界面「太极内网登录」两条入口共用同一份构造。
fn tj_provider(
    store_path: &std::path::Path,
    prompt: qcode_providers::AuthPrompt,
) -> Result<qcode_providers::TjProvider, String> {
    let config = qcode_providers::TjProviderConfig {
        name: TJ_ROUTE.to_string(),
        base_url: TJ_BASE_URL.to_string(),
        settings: tj_settings(),
        store_path: store_path.to_path_buf(),
        prompt: Some(prompt),
        proxy_url: None,
        stream_usage: true,
        extra_body: Default::default(),
        headers: Default::default(),
        images: false,
    };
    qcode_providers::TjProvider::new(config).map_err(|error| error.to_string())
}

/// 在 current-thread tokio runtime 上阻塞执行 `future`（登录是 reqwest 异步
/// HTTP，GPUI executor 不是 tokio reactor，GUI 应在 worker 线程调用）。
fn block_on_token_op<F>(future: F) -> Result<(), String>
where
    F: std::future::Future<Output = Result<(), qcode_core::provider::ProviderError>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| qcode_i18n::trp("provider.tj.login_runtime", &[error.to_string()]))?;
    runtime.block_on(future).map_err(|error| error.to_string())
}

/// 走一次完整的授权码流程（PKCE S256）并把 token 写入 `store_path`
/// （设置页「登录」按钮）。阻塞调用（内部起 current-thread tokio runtime），
/// GUI 应在 worker 线程执行；已登录时幂等。
pub fn login(
    store_path: &std::path::Path,
    prompt: qcode_providers::AuthPrompt,
) -> Result<(), String> {
    let provider = tj_provider(store_path, prompt)?;
    block_on_token_op(provider.login())
}

/// 主界面「太极内网登录」按钮：先无条件刷新 token，刷新失败再走完整授权码
/// 流程（PKCE S256）。与 `login`（本地 token 有效就幂等返回）不同：401 时
/// 本地 token 可能仍未过期，`login` 会短路，必须真的换一次。阻塞调用
/// （内部起 current-thread tokio runtime），GUI 应在 worker 线程执行。
pub fn refresh_or_login(
    store_path: &std::path::Path,
    prompt: qcode_providers::AuthPrompt,
) -> Result<(), String> {
    let provider = tj_provider(store_path, prompt)?;
    block_on_token_op(provider.refresh_or_login())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_redirect_uri_matches_the_loopback_port() {
        // 白名单地址与 listener 端口必须同源：redirect_uri 就来自这里的端口常量。
        assert!(
            TJ_AUTH
                .redirect_uri
                .starts_with(&format!("http://127.0.0.1:{LOOPBACK_PORT}/oauth2/callback"))
        );
    }
}
