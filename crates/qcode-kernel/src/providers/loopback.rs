//! loopback OAuth 回调的共享实现（太极与可配置 OAuth2 统一认证共用）。
//!
//! 登录时在本机起一个临时 HTTP server 接收授权回调：浏览器授权后 302 到
//! `http://127.0.0.1:<port>/oauth2/callback`，qcode 收到回调、校验 `state`、
//! 提取 `code`，并向浏览器返回「登录成功」页面——listener 随本次登录结束而
//! 关闭。端口默认 [`LOOPBACK_PORT`]，授权 URL 的 `redirect_uri` 参数可覆盖
//! （不同认证服务器白名单里的回调地址可能不同）。

/// loopback 回调端口（默认；须与认证服务器 Valid Redirect URIs 白名单里
/// 配置的地址一致）。
pub const LOOPBACK_PORT: u16 = 43782;

/// 从 URL 里取一个 query 参数（`state` / `code` / `redirect_uri`）。
pub(crate) fn query_param(url: &str, key: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// 当前等待用户粘贴的授权 code 的通道（`submit_auth_code` 往这里写，
/// `open_browser_and_wait_with` 阻塞等它）。同一时刻只有一个登录流程在等待。
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

/// 回调页文案的两条 key（`{prefix}.callback_success` / `{prefix}.callback_rejected`）。
pub(crate) struct CallbackKeys {
    pub success: &'static str,
    pub rejected: &'static str,
}

/// 默认授权回调实现：起 loopback HTTP server、开浏览器、等待 code 回来。
///
/// 1. 起一个**临时** loopback HTTP server（监听端口取自授权 URL 的
///    `redirect_uri` 参数，失败回退 [`LOOPBACK_PORT`]），浏览器授权后 302 到
///    该地址，qcode 收到回调、校验 `state`、提取 `code`，并向浏览器返回
///    「登录成功」页面——listener 随本次登录结束而关闭；
/// 2. 同时保留 `submit_auth_code` 手动注入通道作为兜底；
/// 3. 最长 5 分钟，超时返回 `None`。
pub(crate) fn open_browser_and_wait_with(
    auth_url: String,
    callback_keys: CallbackKeys,
) -> Option<String> {
    let expected_state = query_param(&auth_url, "state");
    let port = callback_port(&auth_url);
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
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
            match receive_callback(&mut stream, port, expected_state.as_deref()) {
                Some(code) => {
                    let _ = write_success_page(&mut stream, callback_keys.success);
                    return Some(code);
                }
                None => {
                    // 不是本次登录的回调（state 不符或格式不对）：告知浏览器并继续等。
                    let _ = write_error_page(&mut stream, callback_keys.rejected);
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// 从授权 URL 的 `redirect_uri` 参数解析出回调端口；失败回退 [`LOOPBACK_PORT`]。
fn callback_port(auth_url: &str) -> u16 {
    query_param(auth_url, "redirect_uri")
        .and_then(|uri| reqwest::Url::parse(&uri).ok())
        .and_then(|url| url.port())
        .unwrap_or(LOOPBACK_PORT)
}

/// 从一次 HTTP 回调连接里解析出 `code`，并校验 `state` 与本次授权请求一致。
///
/// 请求形如 `GET /oauth2/callback?code=…&state=… HTTP/1.1`。state 不符或没有
/// code 返回 `None`（丢弃该连接，继续等真正属于本次登录的回调）。
pub(crate) fn receive_callback(
    stream: &mut std::net::TcpStream,
    port: u16,
    expected_state: Option<&str>,
) -> Option<String> {
    let head = read_http_request_head(stream).ok()?;
    let target = head.lines().next()?.split_whitespace().nth(1)?;
    let url = format!("http://127.0.0.1:{port}{target}");
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
pub(crate) fn write_success_page(
    stream: &mut std::net::TcpStream,
    key: &'static str,
) -> std::io::Result<()> {
    write_callback_page(stream, "200 OK", &qcode_i18n::tr(key))
}

/// 回调校验失败时告知浏览器（继续等待真正属于本次登录的回调）。
pub(crate) fn write_error_page(
    stream: &mut std::net::TcpStream,
    key: &'static str,
) -> std::io::Result<()> {
    write_callback_page(stream, "400 Bad Request", &qcode_i18n::tr(key))
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
        let url = "https://auth.example.com/oauth2/auth?response_type=code&client_id=qcode&state=abc123&code_challenge=x";
        assert_eq!(query_param(url, "state").as_deref(), Some("abc123"));
        assert_eq!(query_param(url, "code"), None);
        assert_eq!(query_param("not a url", "state"), None);
    }

    #[test]
    fn callback_port_reads_redirect_uri_and_falls_back() {
        let url = "https://auth.example.com/oauth2/auth?redirect_uri=http%3A%2F%2F127.0.0.1%3A45000%2Foauth2%2Fcallback&state=s";
        // URL-encoded redirect_uri 解析出自定义端口。
        assert_eq!(callback_port(url), 45000);
        // 没有 redirect_uri 参数 → 默认端口。
        assert_eq!(
            callback_port("https://auth.example.com/oauth2/auth"),
            LOOPBACK_PORT
        );
        // redirect_uri 没有端口 → 默认端口（reqwest Url::port 对 80/443 有特判，
        // 其它无端口 URL 返回 None，正好落回默认）。
        assert_eq!(
            callback_port(
                "https://auth.example.com/oauth2/auth?redirect_uri=http%3A%2F%2Flocalhost%2Fcallback"
            ),
            LOOPBACK_PORT
        );
    }

    #[test]
    fn loopback_callback_extracts_code_and_checks_state() {
        let (mut browser, mut server) = socket_pair();
        browser.write_all(&callback_url("c0de", "s1")).unwrap();
        assert_eq!(
            receive_callback(&mut server, LOOPBACK_PORT, Some("s1")).as_deref(),
            Some("c0de")
        );
    }

    #[test]
    fn loopback_callback_rejects_a_wrong_state() {
        let (mut browser, mut server) = socket_pair();
        browser.write_all(&callback_url("c0de", "wrong")).unwrap();
        assert_eq!(
            receive_callback(&mut server, LOOPBACK_PORT, Some("s1")),
            None
        );
    }

    #[test]
    fn loopback_callback_tolerates_absent_state_when_not_expected() {
        let (mut browser, mut server) = socket_pair();
        browser.write_all(&callback_url("c0de", "s1")).unwrap();
        assert_eq!(
            receive_callback(&mut server, LOOPBACK_PORT, None).as_deref(),
            Some("c0de")
        );
    }

    #[test]
    fn success_page_is_a_valid_http_response() {
        qcode_i18n::pin_for_tests();
        let (mut browser, mut server) = socket_pair();
        write_success_page(&mut server, "provider.tj.callback_success").unwrap();
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
        write_error_page(&mut server, "provider.tj.callback_rejected").unwrap();
        drop(server);
        let mut response = String::new();
        browser.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(response.contains("回调校验失败"));
    }
}
