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
    /// 回调地址（`qcode://` 自定义 scheme，需在系统注册；见设计文档 §4.3）。
    pub redirect_uri: &'static str,
}

pub const TJ_AUTH: TjAuthConfig = TjAuthConfig {
    auth_url: "https://auth.taiji.com.cn/realms/taiji/protocol/openid-connect/auth",
    token_url: "https://auth.taiji.com.cn/realms/taiji/protocol/openid-connect/token",
    userinfo_url: "https://auth.taiji.com.cn/realms/taiji/resource/openid-connect/userinfo",
    jwk_set_url: "https://auth.taiji.com.cn/realms/taiji/protocol/openid-connect/certs",
    client_id: "qcode",
    client_name: "qcode",
    redirect_uri: "qcode://oauth2/callback",
};

/// 太极 route 判定与默认模型请求地址（需求 4c）。
pub const TJ_ROUTE: &str = "tj";
pub const TJ_BASE_URL: &str = "http://192.168.69.126:6573/v1/chat/completions";

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

/// 当前等待用户粘贴的授权 code 的通道（`submit_auth_code` 往这里写，
/// `open_browser_and_wait` 阻塞等它）。同一时刻只有一个登录流程在等待。
static PENDING_CODE_TX: std::sync::OnceLock<
    std::sync::Mutex<Option<std::sync::mpsc::Sender<String>>>,
> = std::sync::OnceLock::new();

fn pending_code_tx() -> &'static std::sync::Mutex<Option<std::sync::mpsc::Sender<String>>> {
    PENDING_CODE_TX.get_or_init(|| std::sync::Mutex::new(None))
}

/// 设置页「粘贴 code」确认：把用户从回调 URL 复制出的 code 交给正在等待的
/// 登录流程（没有等待者时静默丢弃）。
pub fn submit_auth_code(code: String) {
    if let Ok(mut guard) = pending_code_tx().lock()
        && let Some(tx) = guard.take()
    {
        let _ = tx.send(code);
    }
}

/// 默认授权回调实现（GUI 启动时注册为全局回调）：打开系统浏览器访问登录页，
/// 阻塞等待用户在设置页粘贴回调 URL 里的 code（最长 5 分钟，超时返回 None）。
///
/// 当前 GPUI 的 `register_url_scheme` 在 Windows 未实现（vendor/gpui-pre-windows
/// platform.rs#L1139），`qcode://` 回调无法自动接收，所以第一版用「手动粘贴 code」
/// 兜底；将来 scheme 注册可用后可直接换成自动回调。
pub fn open_browser_and_wait(auth_url: String) -> Option<String> {
    open_browser(&auth_url);
    let (tx, rx) = std::sync::mpsc::channel();
    if let Ok(mut guard) = pending_code_tx().lock() {
        *guard = Some(tx);
    }
    rx.recv_timeout(std::time::Duration::from_secs(300)).ok()
}

/// 用系统默认浏览器打开 URL（macOS `open`，Linux `xdg-open`，Windows `start`）。
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .arg("/c")
            .arg("start")
            .arg("")
            .arg(url)
            .spawn();
    }
}

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

/// 走一次完整的授权码流程（PKCE S256）并把 token 写入 `store_path`
/// （设置页「登录」按钮）。阻塞调用（内部起 current-thread tokio runtime），
/// GUI 应在 worker 线程执行；已登录时幂等。
pub fn login(
    store_path: &std::path::Path,
    prompt: qcode_providers::AuthPrompt,
) -> Result<(), String> {
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
    let provider = qcode_providers::TjProvider::new(config).map_err(|error| error.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start login runtime: {error}"))?;
    runtime
        .block_on(provider.login())
        .map_err(|error| error.to_string())
}
