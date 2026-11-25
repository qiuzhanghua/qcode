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

/// loopback 回调端口（固定端口；须与太极 Keycloak 的 Valid Redirect URIs
/// 白名单里配置的地址一致，见设计文档 PROVIDER-PACKAGES-TJ-AUTH §4.3）。
pub const LOOPBACK_PORT: u16 = 43782;

/// 从 URL 里取一个 query 参数（`state` / `code`）。
fn query_param(url: &str, key: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// 默认授权回调实现（GUI 启动时注册为全局回调，见设计文档 §4.3）：
///
/// 1. 起一个**临时** loopback HTTP server（`127.0.0.1:<LOOPBACK_PORT>`，仅本机
///    可连），浏览器授权后 302 到该地址，qcode 收到回调、校验 `state`、提取
///    `code`，并向浏览器返回「登录成功」页面——listener 随本次登录结束而关闭；
/// 2. 同时保留 `submit_auth_code` 手动注入通道作为兜底（listener 不可用等场景）；
/// 3. 最长 5 分钟，超时返回 `None`。
pub fn open_browser_and_wait(auth_url: String) -> Option<String> {
    let expected_state = query_param(&auth_url, "state");
    let listener = std::net::TcpListener::bind(("127.0.0.1", LOOPBACK_PORT))
        .ok()
        .inspect(|listener| {
            // accept 轮询用非阻塞；accept 返回的连接自身是阻塞的，read 会等到
            // 浏览器发完请求为止。
            listener.set_nonblocking(true).ok();
        });
    let (tx, rx) = std::sync::mpsc::channel();
    if let Ok(mut guard) = pending_code_tx().lock() {
        *guard = Some(tx);
    }
    open_browser(&auth_url);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        // 手动注入通道（兜底）：GUI 或未来流程把 code 直接交给等待者。
        if let Ok(code) = rx.try_recv() {
            return Some(code);
        }
        // loopback 回调：浏览器授权后 302 到这里。
        if let Some(listener) = &listener
            && let Ok((mut stream, _)) = listener.accept()
        {
            match receive_callback(&mut stream, expected_state.as_deref()) {
                Some(code) => {
                    let _ = write_success_page(&mut stream);
                    return Some(code);
                }
                None => {
                    // 不是本次登录的回调（state 不符或格式不对）：告知浏览器并继续等。
                    let _ = write_error_page(&mut stream);
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// 从一次 HTTP 回调连接里解析出 `code`，并校验 `state` 与本次授权请求一致。
///
/// 请求形如 `GET /oauth2/callback?code=…&state=… HTTP/1.1`。state 不符或没有
/// code 返回 `None`（丢弃该连接，继续等真正属于本次登录的回调）。
fn receive_callback(
    stream: &mut std::net::TcpStream,
    expected_state: Option<&str>,
) -> Option<String> {
    let head = read_http_request_head(stream).ok()?;
    let target = head.lines().next()?.split_whitespace().nth(1)?;
    let url = format!("http://127.0.0.1:{LOOPBACK_PORT}{target}");
    let parsed = reqwest::Url::parse(&url).ok()?;
    let code = query_param(parsed.as_str(), "code")?;
    if code.is_empty() {
        return None;
    }
    match expected_state {
        // 本次授权请求带了 state：回调必须原样带回来（防旧链接/伪造触发）。
        Some(state) if query_param(parsed.as_str(), "state").as_deref() != Some(state) => None,
        _ => Some(code),
    }
}

/// 读到 HTTP 请求头（`\r\n\r\n` 为止）。浏览器回调只发一个 GET，头部量很小。
fn read_http_request_head(stream: &mut std::net::TcpStream) -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = [0u8; 4096];
    let mut data = Vec::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                if data.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// 回调页的骨架 + 一行标题。只有标题走 `tr()`：看这个页面的人就是选界面
/// 语言的那个人（ADR-0105），而布局是 HTML，不翻。
fn callback_page(headline: &str) -> String {
    format!(
        "<html><body style=\"font-family:sans-serif;text-align:center;margin-top:4rem\"><h2>{headline}</h2></body></html>"
    )
}

/// 两个回调页只差状态行与标题，正文与关闭连接的方式共用。
fn write_callback_page(
    stream: &mut std::net::TcpStream,
    status: &str,
    headline: &str,
) -> std::io::Result<()> {
    let body = callback_page(headline);
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    use std::io::Write;
    stream.write_all(response.as_bytes())
}

/// 向浏览器返回「登录成功」页。
fn write_success_page(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
    write_callback_page(
        stream,
        "200 OK",
        &qcode_i18n::tr("provider.tj.callback_success"),
    )
}

/// 回调校验失败时告知浏览器（继续等待真正属于本次登录的回调）。
fn write_error_page(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
    write_callback_page(
        stream,
        "400 Bad Request",
        &qcode_i18n::tr("provider.tj.callback_rejected"),
    )
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
        use std::os::windows::process::CommandExt;
        // 必须把 URL 用引号包起来再交给 cmd：授权 URL 的查询串含 `&`，cmd 会
        // 把它当成命令分隔符，导致 client_id/redirect_uri 等被当作命令执行。
        let _ = std::process::Command::new("cmd")
            .arg("/c")
            .raw_arg(format!("start \"\" \"{url}\""))
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
    use std::io::{Read, Write};

    fn callback_url(code: &str, state: &str) -> Vec<u8> {
        format!(
            "GET /oauth2/callback?code={code}&state={state} HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n"
        )
        .into_bytes()
    }

    /// 造一对已连接的 TcpStream（浏览器端 + 回调接收端）。
    fn socket_pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let browser = std::net::TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (browser, server)
    }

    #[test]
    fn query_param_extracts_state_and_code() {
        let url = "https://auth.taiji.com.cn/realms/taiji/protocol/openid-connect/auth?response_type=code&client_id=qcode&state=abc123&code_challenge=x";
        assert_eq!(query_param(url, "state").as_deref(), Some("abc123"));
        assert_eq!(query_param(url, "code"), None);
        assert_eq!(query_param("not a url", "state"), None);
    }

    #[test]
    fn loopback_callback_extracts_code_and_checks_state() {
        let (mut browser, mut server) = socket_pair();
        browser.write_all(&callback_url("c0de", "s1")).unwrap();
        assert_eq!(
            receive_callback(&mut server, Some("s1")).as_deref(),
            Some("c0de")
        );
    }

    #[test]
    fn loopback_callback_rejects_a_wrong_state() {
        // 旧链接/伪造回调带了错误的 state：必须丢弃，不当作本次登录的结果。
        let (mut browser, mut server) = socket_pair();
        browser.write_all(&callback_url("c0de", "wrong")).unwrap();
        assert_eq!(receive_callback(&mut server, Some("s1")), None);
    }

    #[test]
    fn loopback_callback_tolerates_absent_state_when_not_expected() {
        // 授权请求没带 state（旧版本）时，回调只要带 code 就收。
        let (mut browser, mut server) = socket_pair();
        browser.write_all(&callback_url("c0de", "s1")).unwrap();
        assert_eq!(receive_callback(&mut server, None).as_deref(), Some("c0de"));
    }

    #[test]
    fn success_page_is_a_valid_http_response() {
        // 页面标题跟着界面语言（`provider.tj.callback_success`），断言中文原文。
        qcode_i18n::pin_for_tests();
        let (mut browser, mut server) = socket_pair();
        write_success_page(&mut server).unwrap();
        // 真实现里 accept 出来的 stream 在写完就掉落，浏览器才读到 EOF；
        // 不 drop 的话 `read_to_string` 会一直等下去。
        drop(server);
        let mut response = String::new();
        browser.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("登录成功"));
    }

    #[test]
    fn rejection_page_is_a_400_with_its_own_headline() {
        qcode_i18n::pin_for_tests();
        let (mut browser, mut server) = socket_pair();
        write_error_page(&mut server).unwrap();
        drop(server);
        let mut response = String::new();
        browser.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(response.contains("回调校验失败"));
    }

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
