//! End-to-end check of the Ctrl-C path: a real `qc` process talking to a
//! server that accepts the connection and then never answers. The turn must
//! end as `cancelled`, not hang, and the process must exit with 130.

use std::io::Read;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn wait_for_exit(child: &mut Child, limit: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            panic!("qc did not exit within {limit:?} after SIGINT");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn ctrl_c_cancels_an_inflight_request() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let (accepted_tx, accepted_rx) = mpsc::channel();

    // Hold the connection open without writing a byte: the provider is now
    // blocked on the response, which is exactly when Ctrl-C has to work.
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buffer = [0u8; 4096];
        let _ = stream.read(&mut buffer);
        let _ = accepted_tx.send(());
        std::thread::sleep(Duration::from_secs(30));
    });

    let dir = tempfile::tempdir().expect("temp dir");
    let config = dir.path().join("config.toml");
    // Keyless local server: no API key is needed for this fake endpoint.
    std::fs::write(&config, "[providers.openai]\napi_key_env = \"\"\n").expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args([
            "run",
            "hi",
            "--provider",
            "openai",
            "--model",
            "test-model",
            "--base-url",
            &format!("http://{addr}/v1"),
            "--json",
            "--config",
            config.to_str().expect("utf-8 config path"),
            "--data-dir",
            dir.path().join("data").to_str().expect("utf-8 data path"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn qc");

    accepted_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("qc never opened a connection");

    let signal = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(signal.success(), "kill -INT failed");

    let status = wait_for_exit(&mut child, Duration::from_secs(10));
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout piped")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr piped")
        .read_to_string(&mut stderr)
        .expect("read stderr");

    assert_eq!(
        status.code(),
        Some(130),
        "unexpected exit; stderr:\n{stderr}"
    );
    assert!(
        stdout.contains(r#""type":"turn_ended""#),
        "no turn_ended event in:\n{stdout}"
    );
    assert!(
        stdout.contains(r#""status":"cancelled""#),
        "turn was not marked cancelled in:\n{stdout}"
    );
    assert!(
        stderr.contains("[cancelled]"),
        "missing cancellation notice in:\n{stderr}"
    );
}
