//! `qc update` against a local release server: the real command, no network.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn asset_name(version: &str) -> String {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    format!("qc-{version}-{}{suffix}", platform())
}

/// Answer `expected` requests, then stop. The route closure sees the port so
/// it can write self-referential URLs. Returns the port.
fn serve<F>(expected: usize, mut route: F) -> u16
where
    F: FnMut(&str, u16) -> Option<Vec<u8>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for _ in 0..expected {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            let (status, body) = match route(&path, port) {
                Some(body) => ("200 OK", body),
                None => ("404 Not Found", Vec::new()),
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    port
}

fn release_body(tag: &str, port: u16) -> Vec<u8> {
    let name = asset_name(tag.trim_start_matches('v'));
    format!(
        r#"{{"tag_name":"{tag}","assets":[{{"name":"{name}","browser_download_url":"http://127.0.0.1:{port}/{name}"}},{{"name":"SHA256SUMS","browser_download_url":"http://127.0.0.1:{port}/SHA256SUMS"}}]}}"#
    )
    .into_bytes()
}

fn config_with(port: u16) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "version = 1\n[update]\nrepo = \"example/qcode\"\nbase_url = \"http://127.0.0.1:{port}\"\n"
        ),
    )
    .expect("write config");
    (dir, path)
}

fn qc(config: &Path, data_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_qc"))
        .arg("--config")
        .arg(config)
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .output()
        .expect("run qc")
}

fn backups_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(".old-"))
        })
        .collect()
}

const PAYLOAD: &[u8] = b"new-binary-bytes";
const PAYLOAD_SHA: &str = "eefa43766774cd8198f24981cba72a46fced8e334d2a3257193fecccf6485228";

#[test]
fn check_reports_a_newer_release_and_a_matching_one() {
    let port = serve(1, |path, port| {
        (path == "/repos/example/qcode/releases/latest").then(|| release_body("v99.0.0", port))
    });
    let (dir, config) = config_with(port);
    let output = qc(&config, &dir.path().join("data"), &["update", "--check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("update available"), "{stdout}");
    assert!(stdout.contains("99.0.0"), "{stdout}");

    let port = serve(1, |path, port| {
        (path == "/repos/example/qcode/releases/latest").then(|| release_body("v0.0.1", port))
    });
    let (dir, config) = config_with(port);
    let output = qc(&config, &dir.path().join("data"), &["update", "--check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("up to date"), "{stdout}");
}

#[test]
fn an_unreachable_channel_fails_with_the_http_error() {
    // Nothing listens here; the command must report the transport failure
    // rather than pretending there is no update.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);

    let (dir, config) = config_with(port);
    let output = qc(&config, &dir.path().join("data"), &["update", "--check"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("update check failed"), "{stderr}");
}

#[test]
fn a_verified_download_replaces_the_target_and_keeps_the_old_binary() {
    let name = asset_name("99.0.0");
    let sums = format!("{PAYLOAD_SHA}  {name}\n");
    let asset_path = format!("/{name}");
    let port = serve(3, move |path, port| match path {
        "/repos/example/qcode/releases/latest" => Some(release_body("v99.0.0", port)),
        "/SHA256SUMS" => Some(sums.clone().into_bytes()),
        other if other == asset_path => Some(PAYLOAD.to_vec()),
        _ => None,
    });

    let (dir, config) = config_with(port);
    let target = dir.path().join("qc-installed.exe");
    std::fs::write(&target, b"old-binary").expect("seed");
    let output = qc(
        &config,
        &dir.path().join("data"),
        &[
            "update",
            "--yes",
            "--install-to",
            target.to_str().expect("path"),
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert_eq!(std::fs::read(&target).unwrap(), PAYLOAD);

    let backups = backups_in(dir.path());
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(std::fs::read(&backups[0]).unwrap(), b"old-binary");
}

#[test]
fn a_checksum_mismatch_leaves_the_target_alone() {
    let name = asset_name("99.0.0");
    // Same length, wrong digest: the download must be rejected.
    let wrong = "0".repeat(64);
    let sums = format!("{wrong}  {name}\n");
    let asset_path = format!("/{name}");
    let port = serve(3, move |path, port| match path {
        "/repos/example/qcode/releases/latest" => Some(release_body("v99.0.0", port)),
        "/SHA256SUMS" => Some(sums.clone().into_bytes()),
        other if other == asset_path => Some(PAYLOAD.to_vec()),
        _ => None,
    });

    let (dir, config) = config_with(port);
    let target = dir.path().join("qc-installed.exe");
    std::fs::write(&target, b"old-binary").expect("seed");
    let output = qc(
        &config,
        &dir.path().join("data"),
        &[
            "update",
            "--yes",
            "--install-to",
            target.to_str().expect("path"),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("checksum mismatch"), "{stderr}");
    assert_eq!(std::fs::read(&target).unwrap(), b"old-binary");
}
