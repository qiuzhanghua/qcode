//! Local egress proxy for qcode.
//!
//! The proxy speaks HTTP CONNECT and enforces a host allowlist. It is the
//! application-level boundary described in ADR-0023: qcode-owned HTTP goes
//! through it and fails closed; shell/MCP can only be asked to use it via
//! environment variables.
//!
//! Threat model: prevent accidental data exfiltration and SSRF by the model,
//! not a hostile local process. Raw sockets and `--noproxy` can bypass this.

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use base64::Engine;
use qcode_core::Network;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

const MAX_HEADER_BYTES: usize = 16 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum NetError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid proxy URL: {0}")]
    InvalidProxy(reqwest::Error),
}

/// What to do with a host that is not on the allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownPolicy {
    /// Fail closed. The tool layer may ask the user first and then call
    /// `allow_host` for the session.
    Deny,
    /// Allow any public host (still subject to the private-IP check).
    Allow,
}

#[derive(Debug, Clone)]
pub struct EgressOptions {
    /// Host patterns: `api.openai.com`, `*.example.com`, or `host:port`.
    pub allow: Vec<String>,
    /// Allow loopback/private/link-local destinations (local Ollama, tests).
    pub allow_private: bool,
    pub unknown: UnknownPolicy,
    pub audit_path: Option<PathBuf>,
}

impl Default for EgressOptions {
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            allow_private: false,
            unknown: UnknownPolicy::Deny,
            audit_path: None,
        }
    }
}

/// A running per-process proxy. Drop stops it.
pub struct Egress {
    inner: Arc<Inner>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

struct Inner {
    token: String,
    proxy_url: String,
    rules: Vec<AllowRule>,
    session_allow: RwLock<HashSet<String>>,
    allow_private: bool,
    unknown: UnknownPolicy,
    audit_path: Option<PathBuf>,
    audit_lock: Mutex<()>,
}

impl std::fmt::Debug for Egress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Egress")
            .field("port", &self.port())
            .field("allow_private", &self.inner.allow_private)
            .finish_non_exhaustive()
    }
}

impl Egress {
    pub async fn start(options: EgressOptions) -> Result<Self, NetError> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let token = Ulid::new().to_string();
        let proxy_url = format!("http://qcode:{token}@127.0.0.1:{port}");
        let rules = options
            .allow
            .iter()
            .filter_map(|rule| AllowRule::parse(rule))
            .collect();
        let inner = Arc::new(Inner {
            token,
            proxy_url,
            rules,
            session_allow: RwLock::new(HashSet::new()),
            allow_private: options.allow_private,
            unknown: options.unknown,
            audit_path: options.audit_path,
            audit_lock: Mutex::new(()),
        });
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(serve(listener, inner.clone(), shutdown.clone()));
        Ok(Self {
            inner,
            shutdown,
            task,
        })
    }

    pub fn proxy_url(&self) -> &str {
        &self.inner.proxy_url
    }

    pub fn allow_host(&self, host: &str) {
        if let Ok(mut allow) = self.inner.session_allow.write() {
            allow.insert(normalize_host(host));
        }
    }

    pub fn has_private_access(&self) -> bool {
        self.inner.allow_private
    }

    /// Parse the loopback port; useful for tests and diagnostics.
    pub fn port(&self) -> u16 {
        self.inner
            .proxy_url
            .rsplit('@')
            .next()
            .and_then(|authority| authority.rsplit(':').next())
            .and_then(|port| port.parse().ok())
            .unwrap_or(0)
    }
}

impl Drop for Egress {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.task.abort();
    }
}

impl Network for Egress {
    fn client(&self) -> reqwest::Client {
        let proxy = reqwest::Proxy::all(&self.inner.proxy_url)
            .unwrap_or_else(|err| panic!("session proxy URL is invalid: {err}"));
        reqwest::Client::builder()
            .proxy(proxy)
            .no_proxy()
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::limited(10))
            .user_agent(concat!("qcode/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("build HTTP client")
    }

    fn proxy_url(&self) -> String {
        self.inner.proxy_url.clone()
    }

    fn allow_host(&self, host: &str) {
        Egress::allow_host(self, host);
    }

    fn env(&self) -> Vec<(String, String)> {
        let url = &self.inner.proxy_url;
        vec![
            ("HTTP_PROXY".to_string(), url.clone()),
            ("HTTPS_PROXY".to_string(), url.clone()),
            ("ALL_PROXY".to_string(), url.clone()),
            (
                "NO_PROXY".to_string(),
                "localhost,127.0.0.1,::1".to_string(),
            ),
        ]
    }
}

async fn serve(listener: TcpListener, inner: Arc<Inner>, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        let inner = inner.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_connection(stream, inner).await {
                                tracing::debug!(%err, "egress connection failed");
                            }
                        });
                    }
                    Err(err) => {
                        tracing::warn!(%err, "egress accept failed");
                        break;
                    }
                }
            }
        }
    }
}

async fn handle_connection(mut client: TcpStream, inner: Arc<Inner>) -> Result<(), NetError> {
    let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_headers(&mut client));
    let (head, rest) = match handshake.await {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => return Err(err),
        Err(_) => {
            respond(&mut client, 408, "request timeout").await?;
            return Ok(());
        }
    };

    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let request = lines.next().unwrap_or_default();
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    let authorization = header_value(lines.clone(), "proxy-authorization");
    if !authorized(&inner.token, authorization) {
        audit(&inner, target, 0, "denied", "bad-token");
        respond(&mut client, 407, "proxy authentication required").await?;
        return Ok(());
    }

    if method == "CONNECT" {
        let Some((host, port)) = parse_authority(target) else {
            audit(&inner, target, 0, "denied", "invalid-authority");
            respond(&mut client, 400, "invalid authority").await?;
            return Ok(());
        };
        let upstream = match open_upstream(&inner, &host, port).await {
            Ok(upstream) => upstream,
            Err(failure) => {
                audit(&inner, &host, port, "denied", failure.reason);
                respond(&mut client, failure.status, failure.reason).await?;
                return Ok(());
            }
        };
        audit(&inner, &host, port, "allowed", "connect");
        let mut upstream = upstream;
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        // Bytes already read past the header (rare, but legal for eager clients).
        if !rest.is_empty() {
            upstream.write_all(&rest).await?;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        return Ok(());
    }

    // Plain HTTP proxy requests use absolute-form (`GET http://host/...`).
    let Ok(url) = reqwest::Url::parse(target) else {
        audit(&inner, target, 0, "denied", "invalid-url");
        respond(&mut client, 400, "invalid absolute URL").await?;
        return Ok(());
    };
    if url.scheme() != "http" {
        audit(&inner, target, 0, "denied", "unsupported-scheme");
        respond(&mut client, 405, "only http:// and CONNECT are supported").await?;
        return Ok(());
    }
    let Some(host) = url.host_str() else {
        audit(&inner, target, 0, "denied", "missing-host");
        respond(&mut client, 400, "missing host").await?;
        return Ok(());
    };
    let port = url.port_or_known_default().unwrap_or(80);

    let upstream = match open_upstream(&inner, host, port).await {
        Ok(upstream) => upstream,
        Err(failure) => {
            audit(&inner, host, port, "denied", failure.reason);
            respond(&mut client, failure.status, failure.reason).await?;
            return Ok(());
        }
    };
    let Some(rewritten) = rewrite_http_head(&text, &url) else {
        audit(&inner, host, port, "denied", "invalid-request");
        respond(&mut client, 400, "invalid request").await?;
        return Ok(());
    };
    audit(&inner, host, port, "allowed", "http");
    let mut upstream = upstream;
    upstream.write_all(&rewritten).await?;
    if !rest.is_empty() {
        upstream.write_all(&rest).await?;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

struct UpstreamFailure {
    status: u16,
    reason: &'static str,
}

async fn open_upstream(inner: &Inner, host: &str, port: u16) -> Result<TcpStream, UpstreamFailure> {
    if !inner.allowed(host, port) {
        return Err(UpstreamFailure {
            status: 403,
            reason: "not-allowlisted",
        });
    }

    let addresses: Vec<_> = match tokio::net::lookup_host((host, port)).await {
        Ok(addresses) => addresses.collect(),
        Err(_) => {
            return Err(UpstreamFailure {
                status: 502,
                reason: "dns-failure",
            });
        }
    };
    let mut destination = None;
    for address in addresses {
        if !inner.allow_private && blocked_ip(address.ip()) {
            return Err(UpstreamFailure {
                status: 403,
                reason: "private-address",
            });
        }
        if destination.is_none() {
            destination = Some(address);
        }
    }
    let Some(destination) = destination else {
        return Err(UpstreamFailure {
            status: 502,
            reason: "no-address",
        });
    };

    TcpStream::connect(destination)
        .await
        .map_err(|_| UpstreamFailure {
            status: 502,
            reason: "connect-failed",
        })
}

/// Turn an absolute-form request into origin-form for the upstream server.
fn rewrite_http_head(head: &str, url: &reqwest::Url) -> Option<Vec<u8>> {
    let mut lines = head.split("\r\n");
    let request = lines.next()?;
    let mut parts = request.split_whitespace();
    let method = parts.next()?;
    let _target = parts.next()?;
    let version = parts.next()?;

    let mut path = url.path().to_string();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }

    let mut output = format!("{method} {path} {version}\r\n");
    for line in lines {
        let Some((name, _)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        output.push_str(line);
        output.push_str("\r\n");
    }
    output.push_str("Connection: close\r\n\r\n");
    Some(output.into_bytes())
}

async fn read_headers(stream: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>), NetError> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(position) = find_headers_end(&buffer) {
            let head = buffer[..position].to_vec();
            let rest = buffer[position + 4..].to_vec();
            return Ok((head, rest));
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(NetError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "proxy headers too large",
            )));
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(NetError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "client closed during proxy handshake",
            )));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn find_headers_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_authority(authority: &str) -> Option<(String, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, rest) = rest.split_once(']')?;
        let port = rest.strip_prefix(':')?.parse().ok()?;
        return Some((host.to_string(), port));
    }
    let (host, port) = authority.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port.parse().ok()?))
}

fn header_value<'a>(lines: impl Iterator<Item = &'a str>, wanted: &str) -> Option<&'a str> {
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case(wanted) {
            return Some(value.trim());
        }
    }
    None
}

fn authorized(token: &str, header: Option<&str>) -> bool {
    let Some(header) = header else {
        return false;
    };
    let Some(encoded) = header.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return false;
    };
    decoded == format!("qcode:{token}")
}

fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_end_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

#[derive(Debug)]
struct AllowRule {
    pattern: String,
    port: Option<u16>,
}

impl AllowRule {
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let (host, port) = match raw.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|ch| ch.is_ascii_digit()) => {
                (host, Some(port.parse().ok()?))
            }
            _ => (raw, None),
        };
        Some(Self {
            pattern: normalize_host(host),
            port,
        })
    }

    fn matches(&self, host: &str, port: u16) -> bool {
        if let Some(expected) = self.port {
            if expected != port {
                return false;
            }
        } else if port != 443 && port != 80 {
            return false;
        }

        if let Some(suffix) = self.pattern.strip_prefix("*.") {
            host.len() > suffix.len() + 1
                && host.ends_with(suffix)
                && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
        } else {
            host == self.pattern
        }
    }
}

impl Inner {
    fn allowed(&self, host: &str, port: u16) -> bool {
        let host = normalize_host(host);
        if self.rules.iter().any(|rule| rule.matches(&host, port)) {
            return true;
        }
        if self
            .session_allow
            .read()
            .map(|allow| allow.contains(&host))
            .unwrap_or(false)
        {
            return true;
        }
        self.unknown == UnknownPolicy::Allow
    }

    fn audit(&self, host: &str, port: u16, result: &str, reason: &str) {
        let Some(path) = &self.audit_path else {
            return;
        };
        let Ok(line) = serde_json::to_string(&serde_json::json!({
            "ts": time::OffsetDateTime::now_utc().to_string(),
            "host": host,
            "port": port,
            "result": result,
            "reason": reason,
        })) else {
            return;
        };
        let _guard = self.audit_lock.lock();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

fn audit(inner: &Inner, host: &str, port: u16, result: &str, reason: &str) {
    inner.audit(host, port, result, reason);
}

async fn respond(stream: &mut TcpStream, status: u16, message: &str) -> Result<(), NetError> {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let body = format!("{message}\n");
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

fn blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_unspecified()
                || ip.is_multicast()
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn token_from(url: &str) -> String {
        let rest = url.strip_prefix("http://").expect("http prefix");
        let (userinfo, _) = rest.rsplit_once('@').expect("userinfo");
        userinfo
            .strip_prefix("qcode:")
            .expect("qcode user")
            .to_string()
    }

    async fn connect(egress: &Egress, host: &str, port: u16) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", egress.port()))
            .await
            .expect("proxy");
        let token = token_from(egress.proxy_url());
        let auth = base64::engine::general_purpose::STANDARD.encode(format!("qcode:{token}"));
        let request = format!(
            "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Authorization: Basic {auth}\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        stream
    }

    async fn read_head(stream: &mut TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }

    #[tokio::test]
    async fn tunnels_allowlisted_hosts_and_blocks_others() {
        let echo = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = echo.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 1024];
                    while let Ok(read) = socket.read(&mut buffer).await {
                        if read == 0 || socket.write_all(&buffer[..read]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        let egress = Egress::start(EgressOptions {
            allow: vec![format!("127.0.0.1:{echo_port}")],
            allow_private: true,
            ..EgressOptions::default()
        })
        .await
        .expect("egress");

        let mut stream = connect(&egress, "127.0.0.1", echo_port).await;
        let head = read_head(&mut stream).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        stream.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");

        let mut blocked = connect(&egress, "example.com", 443).await;
        let head = read_head(&mut blocked).await;
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    }

    #[tokio::test]
    async fn rejects_missing_token_and_private_addresses() {
        let echo = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        drop(echo);

        let egress = Egress::start(EgressOptions {
            allow: vec![format!("127.0.0.1:{echo_port}")],
            allow_private: false,
            ..EgressOptions::default()
        })
        .await
        .expect("egress");

        // No Proxy-Authorization.
        let mut stream = TcpStream::connect(("127.0.0.1", egress.port()))
            .await
            .unwrap();
        stream
            .write_all(format!("CONNECT 127.0.0.1:{echo_port} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        assert!(read_head(&mut stream).await.starts_with("HTTP/1.1 407"));

        // Correct token, but loopback is private and allow_private is false.
        let mut stream = connect(&egress, "127.0.0.1", echo_port).await;
        assert!(read_head(&mut stream).await.starts_with("HTTP/1.1 403"));
    }

    #[test]
    fn allow_rules_support_wildcards_and_ports() {
        let wildcard = AllowRule::parse("*.example.com").expect("rule");
        assert!(wildcard.matches("api.example.com", 443));
        assert!(!wildcard.matches("example.com", 443));
        assert!(!wildcard.matches("api.example.com", 8443));

        let port = AllowRule::parse("localhost:11434").expect("rule");
        assert!(port.matches("localhost", 11434));
        assert!(!port.matches("localhost", 443));
    }

    #[tokio::test]
    async fn audit_log_records_allow_and_deny() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("network.jsonl");
        let egress = Egress::start(EgressOptions {
            allow: vec!["example.com".to_string()],
            audit_path: Some(audit.clone()),
            ..EgressOptions::default()
        })
        .await
        .expect("egress");

        let mut blocked = connect(&egress, "blocked.example.org", 443).await;
        assert!(read_head(&mut blocked).await.starts_with("HTTP/1.1 403"));

        let log = std::fs::read_to_string(&audit).expect("audit");
        assert!(log.contains("not-allowlisted"), "{log}");
        assert!(log.contains("blocked.example.org"), "{log}");
    }

    #[tokio::test]
    async fn plain_http_requests_are_proxied() {
        let server = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = server.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = server.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let text = String::from_utf8_lossy(&request);
            assert!(text.starts_with("GET /x?"), "{text}");
            assert!(!text.to_ascii_lowercase().contains("proxy-authorization"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
        });

        let egress = Egress::start(EgressOptions {
            allow: vec![format!("127.0.0.1:{port}")],
            allow_private: true,
            ..EgressOptions::default()
        })
        .await
        .expect("egress");

        let response = egress
            .client()
            .get(format!("http://127.0.0.1:{port}/x?q=1"))
            .send()
            .await
            .expect("request");
        assert!(response.status().is_success());
        assert_eq!(response.text().await.unwrap(), "ok");
    }
}
