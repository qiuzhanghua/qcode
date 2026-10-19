use std::path::Path;
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

use qcode_core::event::ToolStatus;
use qcode_core::tool::{Tool, ToolContext};
use qcode_net::{Egress, EgressOptions};
use qcode_tools::fetch::WebFetch;
use qcode_tools::fs::{EditFile, ListDir, ReadFile, WriteFile};
use qcode_tools::shell::RunShell;
use serde_json::json;
#[cfg(unix)]
use tokio_util::sync::CancellationToken;

fn context(dir: &Path) -> ToolContext {
    ToolContext::new(dir.to_path_buf())
}

#[tokio::test]
async fn write_read_edit_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());

    WriteFile
        .execute(
            &ctx,
            json!({"path": "src/main.rs", "content": "fn main() {}\n"}),
        )
        .await
        .unwrap();

    let read = ReadFile
        .execute(&ctx, json!({"path": "src/main.rs"}))
        .await
        .unwrap();
    assert!(read.output.contains("1| fn main()"));

    EditFile
        .execute(
            &ctx,
            json!({
                "path": "src/main.rs",
                "old_string": "fn main()",
                "new_string": "fn run()"
            }),
        )
        .await
        .unwrap();

    let read = ReadFile
        .execute(&ctx, json!({"path": "src/main.rs"}))
        .await
        .unwrap();
    assert!(read.output.contains("fn run()"));
    assert!(!read.output.contains("fn main()"));
}

#[tokio::test]
async fn edit_requires_unique_match() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    WriteFile
        .execute(&ctx, json!({"path": "a.txt", "content": "x\nx\n"}))
        .await
        .unwrap();

    let output = EditFile
        .execute(
            &ctx,
            json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
        )
        .await
        .unwrap();
    assert_eq!(output.status, qcode_core::event::ToolStatus::Error);
    assert!(output.output.contains("appears 2 times"));
}

#[tokio::test]
async fn rejects_parent_directory_escape() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());

    let error = WriteFile
        .execute(&ctx, json!({"path": "../escape.txt", "content": "nope"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("escapes"));
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlink_escape() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();

    let ctx = context(dir.path());
    let error = WriteFile
        .execute(&ctx, json!({"path": "link/evil.txt", "content": "nope"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("escapes"));
}

#[tokio::test]
async fn list_dir_skips_build_directories() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    std::fs::create_dir_all(dir.path().join("target")).unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn x() {}\n").unwrap();

    let output = ListDir.execute(&ctx, json!({"path": "."})).await.unwrap();
    assert!(output.output.contains("src"));
    assert!(!output.output.contains("target"));
    assert!(!output.output.contains(".git"));
}

#[tokio::test]
async fn shell_reports_exit_code_and_scrubs_env() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());

    // SAFETY: tests use a unique variable name; no other test reads it.
    unsafe { std::env::set_var("QCODE_TEST_SECRET", "sk-should-not-leak") };
    let output = RunShell::default()
        .execute(
            &ctx,
            json!({"command": "printf '%s' \"${QCODE_TEST_SECRET:-missing}\""}),
        )
        .await
        .unwrap();
    unsafe { std::env::remove_var("QCODE_TEST_SECRET") };

    assert!(output.output.contains("missing"));
    assert!(!output.output.contains("sk-should-not-leak"));
    assert_eq!(output.metadata.exit_code, Some(0));

    if let Ok(cargo_home) = std::env::var("CARGO_HOME") {
        let output = RunShell::default()
            .execute(
                &ctx,
                json!({"command": "printf '%s' \"${CARGO_HOME:-missing}\""}),
            )
            .await
            .unwrap();
        assert!(output.output.contains(&cargo_home));
    }

    let failed = RunShell::default()
        .execute(&ctx, json!({"command": "exit 3"}))
        .await
        .unwrap();
    assert_eq!(failed.metadata.exit_code, Some(3));
}

#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only performs the permission/existence check.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Wait for the background child's pid to appear, then cancel the run. Returns
/// the pid so the test can assert that it is gone.
#[cfg(unix)]
async fn cancel_once_child_is_running(pid_file: &Path, cancel: &CancellationToken) -> Option<i32> {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(pid_file)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            cancel.cancel();
            return Some(pid);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

#[cfg(unix)]
async fn wait_until_gone(pid: i32) -> bool {
    for _ in 0..150 {
        if !process_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_kills_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let ctx = context(dir.path()).with_cancel(cancel.clone());
    let pid_file = dir.path().join("child.pid");

    let shell = RunShell::default();
    let run = shell.execute(
        &ctx,
        json!({"command": "sleep 30 & echo $! > child.pid; wait"}),
    );
    let (output, pid) = tokio::join!(run, cancel_once_child_is_running(&pid_file, &cancel));

    let output = output.unwrap();
    assert_eq!(output.status, ToolStatus::Cancelled);
    assert!(output.output.contains("cancelled by user"));

    let pid = pid.expect("background child never wrote its pid");
    assert!(
        wait_until_gone(pid).await,
        "grandchild {pid} survived cancellation"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_kills_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let pid_file = dir.path().join("child.pid");

    // The pid file is written before the timeout fires; the tool call itself
    // is what has to wait one second.
    let output = RunShell::default()
        .execute(
            &ctx,
            json!({
                "command": "sleep 30 & echo $! > child.pid; wait",
                "timeout_secs": 1
            }),
        )
        .await
        .unwrap();

    assert_eq!(output.status, ToolStatus::Error);
    assert!(output.output.contains("timed out after 1s"));

    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        wait_until_gone(pid).await,
        "grandchild {pid} survived the timeout"
    );
}

#[tokio::test]
async fn web_fetch_goes_through_the_egress_proxy() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello",
            )
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
    let ctx = context(Path::new(".")).with_network(Arc::new(egress));

    let output = WebFetch
        .execute(&ctx, json!({"url": format!("http://127.0.0.1:{port}/")}))
        .await
        .expect("fetch");
    assert_eq!(output.status, ToolStatus::Ok);
    assert!(output.output.contains("hello"), "{}", output.output);
}

#[cfg(unix)]
#[tokio::test]
async fn shell_receives_the_proxy_environment() {
    let egress = Egress::start(EgressOptions::default())
        .await
        .expect("egress");
    let proxy_url = egress.proxy_url().to_string();
    let ctx = context(Path::new(".")).with_network(Arc::new(egress));

    let output = RunShell::default()
        .execute(
            &ctx,
            json!({"command": "printf '%s' \"$HTTPS_PROXY\"", "timeout_secs": 10}),
        )
        .await
        .expect("shell");
    assert_eq!(output.status, ToolStatus::Ok, "{}", output.output);
    assert!(output.output.contains(&proxy_url), "{}", output.output);
}
