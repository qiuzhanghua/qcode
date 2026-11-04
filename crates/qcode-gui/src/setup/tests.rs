use super::{extra_body_from_lines, stale_model_default};

#[test]
fn switching_provider_drops_the_mock_default() {
    assert_eq!(stale_model_default("deepseek", Some("mock-1".into())), None);
    assert_eq!(
        stale_model_default("mock", Some("mock-1".into())),
        Some("mock-1".into())
    );
    assert_eq!(
        stale_model_default("deepseek", Some("deepseek-flash".into())),
        Some("deepseek-flash".into())
    );
}

#[test]
fn extra_body_cell_parses_json_and_falls_back_to_strings() {
    let body = extra_body_from_lines(&[
        "reasoning_split = true".to_string(),
        "foo = bar".to_string(),
        "quoted = \"a string\"".to_string(),
        "= no-key".to_string(),
    ]);
    assert_eq!(body.get("reasoning_split"), Some(&serde_json::json!(true)));
    assert_eq!(body.get("foo"), Some(&serde_json::json!("bar")));
    assert_eq!(body.get("quoted"), Some(&serde_json::json!("a string")));
    assert!(!body.contains_key(""), "a line without a key is dropped");
    assert!(extra_body_from_lines(&[]).is_empty());
}

/// Windows credential store round trip: `keychain_store` writes a generic
/// credential via cmdkey and returns a one-line PowerShell read command; that
/// command must print the stored secret back. Uses a per-process target so
/// parallel test runs cannot collide, and cleans up afterwards.
#[test]
#[cfg(target_os = "windows")]
fn windows_keychain_store_round_trips_via_cmdkey_and_powershell() {
    use super::keychain_store;
    let provider = format!("test-{}", std::process::id());
    let secret = "sk-roundtrip-123";
    let argv = keychain_store(&provider, secret).expect("store");
    assert_eq!(argv.first().map(String::as_str), Some("powershell"));
    assert_eq!(argv.get(1).map(String::as_str), Some("-NoProfile"));
    assert_eq!(argv.get(2).map(String::as_str), Some("-Command"));
    // The script is stored verbatim in api_key_command (one argv per line), so
    // it must survive the newline split: no embedded newlines.
    assert!(
        !argv[3].contains('\n'),
        "the read script must be a single line"
    );
    let output = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .expect("run the read command");
    assert!(output.status.success(), "read command failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(secret), "stdout: {stdout:?}");
    let _ = std::process::Command::new("cmdkey")
        .args(["/delete:", &format!("qcode/{provider}")])
        .status();
}
