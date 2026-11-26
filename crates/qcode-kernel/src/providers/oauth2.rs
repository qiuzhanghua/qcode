//! 可配置 OAuth2 统一认证 Provider 的 kernel 侧胶水。
//!
//! 与太极（`tj.rs`，端点/客户端硬编码）不同：OAuth2 的端点、客户端标识、
//! 登录模式全部来自用户配置（`providers.<route>.oauth2`），由 GUI 在设置页
//! 编辑后经 `login` / `refresh_or_login` 显式传入。token 存
//! `oauth2_auth.json`（与 config.toml 同目录）。

/// OAuth2 统一认证 route 判定（GUI 路由判定用；`resolve` 的认证模式判定走
/// `entry.oauth2.is_some()`，任何 route 都可携带 oauth2 配置）。
pub const OAUTH2_ROUTE: &str = "oauth2";

/// 该 route 是否是可配置 OAuth2 统一认证 Provider。
pub fn is_oauth2_route(route: &str) -> bool {
    route == OAUTH2_ROUTE
}

/// 进程级授权回调（GUI 启动时注册一次，供所有 OAuth2Provider 在需要授权时
/// 打开登录页并取回 code；CLI / 无人值守不注册，保持 None）。与太极的
/// `GLOBAL_AUTH_PROMPT` 相互独立（两套回调共用同一个 loopback 实现与
/// 「粘贴 code」通道）。
static GLOBAL_OAUTH2_PROMPT: std::sync::OnceLock<qcode_providers::AuthPrompt> =
    std::sync::OnceLock::new();

/// 注册全局授权回调（GUI 启动时调用；重复注册幂等，首个生效）。
pub fn set_global_oauth2_prompt(prompt: qcode_providers::AuthPrompt) {
    let _ = GLOBAL_OAUTH2_PROMPT.set(prompt);
}

/// 取全局授权回调；未注册时返回 None。
pub fn global_oauth2_prompt() -> Option<qcode_providers::AuthPrompt> {
    GLOBAL_OAUTH2_PROMPT.get().cloned()
}

/// 默认授权回调实现（GUI 启动时注册为 OAuth2 的全局回调）：委托给共享的
/// loopback 实现，回调页文案走 OAuth2 的 key；监听端口取自授权 URL 里
/// `redirect_uri` 参数（配置不同的回调地址也能工作）。
pub fn open_browser_and_wait(auth_url: String) -> Option<String> {
    crate::providers::loopback::open_browser_and_wait_with(
        auth_url,
        crate::providers::loopback::CallbackKeys {
            success: "provider.oauth2.callback_success",
            rejected: "provider.oauth2.callback_rejected",
        },
    )
}

/// 默认回调地址：与太极同端口（`127.0.0.1:43782/oauth2/callback`）。配置里
/// `redirect_uri` 缺省时用它；认证服务器白名单须包含该地址。
pub fn default_redirect_uri() -> String {
    format!(
        "http://127.0.0.1:{}/oauth2/callback",
        crate::providers::loopback::LOOPBACK_PORT
    )
}

/// kernel 侧 config 的 `OAuth2AuthConfig` → providers 层的 `OAuth2AuthSettings`
/// （依赖方向：qcode-providers 不依赖 kernel，值在这里搬运；`redirect_uri`
/// 缺省补 loopback 默认地址）。
///
/// `client_secret` 由调用方传入**已解析**的值（[`crate::config::oauth2_client_secret`]
/// 的结果），而不是从 `config` 里再读一次明文：那会绕过 env / command 两个来源，
/// 让「密钥只从环境变量取」变成一句空话。
pub fn oauth2_settings(
    config: &crate::config::OAuth2AuthConfig,
    client_secret: Option<String>,
) -> qcode_providers::OAuth2AuthSettings {
    qcode_providers::OAuth2AuthSettings {
        mode: config.mode,
        client_id: config.client_id.clone(),
        client_secret,
        auth_url: config.auth_url.clone(),
        token_url: config.token_url.clone(),
        scope: config.scope.clone(),
        redirect_uri: config
            .redirect_uri
            .clone()
            .unwrap_or_else(default_redirect_uri),
    }
}

/// 构造 OAuth2Provider（登录只需 settings/store/prompt，base_url 留空——它只
/// 在 `stream()` 访问模型网关时用，登录流程不碰）。
fn oauth2_provider(
    store_path: &std::path::Path,
    settings: qcode_providers::OAuth2AuthSettings,
    prompt: qcode_providers::AuthPrompt,
) -> Result<qcode_providers::OAuth2Provider, String> {
    let config = qcode_providers::OAuth2ProviderConfig {
        name: OAUTH2_ROUTE.to_string(),
        base_url: String::new(),
        settings,
        store_path: store_path.to_path_buf(),
        prompt: Some(prompt),
        proxy_url: None,
        stream_usage: true,
        extra_body: Default::default(),
        headers: Default::default(),
        images: false,
    };
    qcode_providers::OAuth2Provider::new(config).map_err(|error| error.to_string())
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
        .map_err(|error| qcode_i18n::trp("provider.oauth2.login_runtime", &[error.to_string()]))?;
    runtime.block_on(future).map_err(|error| error.to_string())
}

/// 走一次完整的登录流程（授权码 / PKCE，按 `settings.mode`）并把 token 写入
/// `store_path`（设置页「登录」按钮）。阻塞调用（内部起 current-thread tokio
/// runtime），GUI 应在 worker 线程执行；已登录时幂等。
pub fn login(
    store_path: &std::path::Path,
    settings: qcode_providers::OAuth2AuthSettings,
    prompt: qcode_providers::AuthPrompt,
) -> Result<(), String> {
    let provider = oauth2_provider(store_path, settings, prompt)?;
    block_on_token_op(provider.login())
}

/// 主界面「OAuth2 登录」按钮：先无条件刷新 token，刷新失败再走完整登录流程。
/// 与 `login`（本地 token 有效就幂等返回）不同：401 时本地 token 可能仍未过期，
/// 必须真的换一次。阻塞调用（内部起 current-thread tokio runtime），GUI 应在
/// worker 线程执行。
pub fn refresh_or_login(
    store_path: &std::path::Path,
    settings: qcode_providers::OAuth2AuthSettings,
    prompt: qcode_providers::AuthPrompt,
) -> Result<(), String> {
    let provider = oauth2_provider(store_path, settings, prompt)?;
    block_on_token_op(provider.refresh_or_login())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::loopback::LOOPBACK_PORT;
    use qcode_providers::OAuth2Mode;

    fn settings() -> qcode_providers::OAuth2AuthSettings {
        qcode_providers::OAuth2AuthSettings {
            mode: OAuth2Mode::AuthorizationCode,
            client_id: "cli".to_string(),
            client_secret: Some("sec".to_string()),
            auth_url: "https://auth.example.com/oauth2/auth".to_string(),
            token_url: "https://auth.example.com/oauth2/token".to_string(),
            scope: None,
            redirect_uri: "http://127.0.0.1:45000/oauth2/callback".to_string(),
        }
    }

    #[test]
    fn route_detection_matches_exact_string() {
        assert!(is_oauth2_route("oauth2"));
        assert!(!is_oauth2_route("oauth"));
        assert!(!is_oauth2_route("tj"));
    }

    #[test]
    fn settings_conversion_fills_default_redirect_uri() {
        let config = crate::config::OAuth2AuthConfig {
            mode: OAuth2Mode::Pkce,
            client_id: "cli".to_string(),
            client_secret: None,
            client_secret_env: None,
            client_secret_command: None,
            auth_url: "https://auth.example.com/oauth2/auth".to_string(),
            token_url: "https://auth.example.com/oauth2/token".to_string(),
            scope: None,
            redirect_uri: None,
        };
        let settings = oauth2_settings(&config, None);
        assert_eq!(settings.mode, OAuth2Mode::Pkce);
        assert_eq!(settings.client_id, "cli");
        assert_eq!(settings.client_secret, None);
        // 缺省补 loopback 默认地址（与 loopback 端口同源）。
        assert_eq!(
            settings.redirect_uri,
            format!("http://127.0.0.1:{}/oauth2/callback", LOOPBACK_PORT)
        );
    }

    /// 传进来的密钥必须真的落进 settings。
    ///
    /// 这条看着琐碎，但它盖的是一个**所有其它测试都发现不了**的错误：
    /// `oauth2_settings` 第二参数接的是已解析的密钥，如果有人把它写成
    /// `client_secret: None`（或从 `config.client_secret` 重新读明文，绕过
    /// env / command 两个来源），上面那条测试照样绿——它测的正是 `None`。
    /// 而运行时的表现是授权码模式换 token 被网关拒绝，看起来像端点配错了。
    #[test]
    fn settings_conversion_carries_the_resolved_secret() {
        let mut config = crate::config::OAuth2AuthConfig {
            mode: OAuth2Mode::AuthorizationCode,
            client_id: "cli".to_string(),
            client_secret_env: Some("CORP_OAUTH_SECRET".to_string()),
            auth_url: "https://auth.example.com/oauth2/auth".to_string(),
            token_url: "https://auth.example.com/oauth2/token".to_string(),
            ..Default::default()
        };
        // 明文那份**没有**：密钥只来自环境变量，所以 settings 里出现值就说明
        // 用的是传进来的那个，而不是配置里重新读的。
        config.client_secret = None;
        let settings = oauth2_settings(&config, Some("resolved-by-env".to_string()));
        assert_eq!(settings.client_secret.as_deref(), Some("resolved-by-env"));
        // 而配置里那两个来源的名字不该被带到运行期结构里：settings 只承载值。
        assert_eq!(settings.mode, OAuth2Mode::AuthorizationCode);
    }

    #[test]
    fn default_redirect_uri_matches_the_loopback_port() {
        // 白名单地址与 listener 端口必须同源：默认回调地址就来自端口常量。
        assert!(
            default_redirect_uri()
                .starts_with(&format!("http://127.0.0.1:{LOOPBACK_PORT}/oauth2/callback"))
        );
    }

    #[test]
    fn login_without_interactive_prompt_errors_cleanly() {
        // 授权回调返回 None（无交互能力/用户放弃）时登录应报错而不是挂起：
        // 这是引擎侧任务运行时（prompt=None）路径的等价行为。
        let dir =
            std::env::temp_dir().join(format!("qcode-oauth2-kernel-test-{}", std::process::id()));
        let store = dir.join("oauth2_auth.json");
        let prompt: qcode_providers::AuthPrompt = std::sync::Arc::new(|_| None);
        let result = login(&store, settings(), prompt);
        assert!(result.is_err(), "非交互回调必须报错而非挂起");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
