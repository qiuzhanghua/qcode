use super::save::{CloseOutcome, decide_close};
use super::{
    AutoSaveKind, extra_body_from_lines, is_lines_cell, overridden_default_model,
    stale_model_default, sync_default_model,
};
use crate::forms::{self, FieldKind};

/// Every `FieldKind::Lines` table column must be edited with a multi-line
/// textarea; `is_lines_cell` is the classification used when seeding rows. A
/// single-line Input silently strips `\n`/`\r` (gpui-base `normalize_input`),
/// which would glue a newline-joined list into one item on the next save.
#[test]
fn lines_columns_match_field_kind_lines() {
    for table in forms::TABLES {
        for column in table.columns {
            assert_eq!(
                is_lines_cell(column.key),
                matches!(column.kind, FieldKind::Lines),
                "column {} of {} must match its editor kind",
                column.key,
                table.path.join(".")
            );
        }
    }
    // Every key the classifier accepts must be a real Lines column.
    let lines_keys = forms::TABLES
        .iter()
        .flat_map(|table| table.columns)
        .filter(|column| matches!(column.kind, FieldKind::Lines))
        .map(|column| column.key)
        .collect::<Vec<_>>();
    for key in ["api_key_command", "extra_body", "args", "env"] {
        assert!(lines_keys.contains(&key), "{key} must be a Lines column");
    }
}

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
fn sync_default_model_moves_the_global_default_with_the_provider_row() {
    let mut default = qcode_kernel::config::DefaultModelConfig {
        provider: Some("deepseek".into()),
        model: Some("deepseek-flash".into()),
    };
    assert!(sync_default_model(
        &mut default,
        "deepseek",
        Some("DeepSeek-V4.1-Flash")
    ));
    assert_eq!(default.model.as_deref(), Some("DeepSeek-V4.1-Flash"));

    // 不是默认 provider 的行不碰全局默认。
    assert!(!sync_default_model(
        &mut default,
        "minimax",
        Some("MiniMax-M3")
    ));
    assert_eq!(default.model.as_deref(), Some("DeepSeek-V4.1-Flash"));

    // 行被清空（交给目录默认）时保留全局默认：那是从模型菜单单钉的值，
    // 一次无关的表格自动保存不该把它抹掉。
    assert!(!sync_default_model(&mut default, "deepseek", None));
    assert!(!sync_default_model(&mut default, "deepseek", Some("  ")));
    assert_eq!(default.model.as_deref(), Some("DeepSeek-V4.1-Flash"));

    // 没变就不报成功，省掉一次多余的写盘。
    assert!(!sync_default_model(
        &mut default,
        "deepseek",
        Some("DeepSeek-V4.1-Flash")
    ));
}

/// 用户报的那个 bug：在设置页把默认 provider 的「Default model」改成
/// `DeepSeek-V4.1-Flash`，保存成功、表单显示新值，跑任务还是旧模型。
///
/// 原因是读配置时 `[default-model].model` 压着 `providers.*.default_model`。
/// 这条用例走的是真实的 `resolve`，所以它验的不是「同步函数被调用过」，
/// 而是「保存之后真正会用的模型变了」——前者可以全绿而用户仍然看到旧模型。
#[test]
fn a_saved_provider_model_is_the_one_resolve_returns() {
    use qcode_kernel::config::{Overrides, ProviderConfig, resolve};

    let mut config = qcode_kernel::config::Config {
        default_model: qcode_kernel::config::DefaultModelConfig {
            provider: Some("deepseek".into()),
            model: Some("deepseek-flash".into()),
        },
        ..qcode_kernel::config::Config::default()
    };
    config.providers.insert(
        "deepseek".into(),
        ProviderConfig {
            base_url: Some("https://api.deepseek.com/v1".into()),
            api_key_env: Some("DEEPSEEK_API_KEY".into()),
            default_model: Some("deepseek-flash".into()),
            ..ProviderConfig::default()
        },
    );
    let env = |key: &str| (key == "DEEPSEEK_API_KEY").then(|| "sk-test".to_string());
    assert_eq!(
        resolve(&config, &Overrides::default(), &env)
            .expect("resolves")
            .model,
        "deepseek-flash"
    );

    // What the form's save path does: the row now says the new model, and the
    // tables are applied (which is where the sync lives).
    config
        .providers
        .get_mut("deepseek")
        .expect("row")
        .default_model = Some("DeepSeek-V4.1-Flash".into());
    let row_model = config.providers["deepseek"].default_model.clone();
    assert!(sync_default_model(
        &mut config.default_model,
        "deepseek",
        row_model.as_deref()
    ));

    assert_eq!(
        resolve(&config, &Overrides::default(), &env)
            .expect("resolves")
            .model,
        "DeepSeek-V4.1-Flash"
    );
}

#[test]
fn the_shadowed_model_is_the_one_the_global_default_names() {
    use qcode_kernel::config::DefaultModelConfig;
    let default = DefaultModelConfig {
        provider: Some("deepseek".into()),
        model: Some("deepseek-flash".into()),
    };
    // The user's actual state: the row says one thing, `[default-model]` another.
    assert_eq!(
        overridden_default_model(&default, "deepseek", "DeepSeek-V4.1-Flash"),
        Some("deepseek-flash")
    );
    // Same value on both sides: nothing is being shadowed.
    assert_eq!(
        overridden_default_model(&default, "deepseek", "deepseek-flash"),
        None
    );
    // Another provider's row is never shadowed by it (resolve only applies the
    // global model to the provider it names).
    assert_eq!(
        overridden_default_model(&default, "minimax", "MiniMax-M3"),
        None
    );
    // An empty row means "inherit the catalog", which is not an override.
    assert_eq!(overridden_default_model(&default, "deepseek", ""), None);
}

#[test]
fn the_default_picker_lists_each_providers_models_and_hides_mock() {
    use qcode_kernel::config::{Config, ModelEntry, ProviderConfig};
    let mut config = Config::default();
    config.providers.insert(
        "deepseek".into(),
        ProviderConfig {
            default_model: Some("deepseek-flash".into()),
            ..ProviderConfig::default()
        },
    );
    config.providers.insert(
        "minimax".into(),
        ProviderConfig {
            models: vec![ModelEntry {
                id: "MiniMax-M3".into(),
                ..ModelEntry::default()
            }],
            ..ProviderConfig::default()
        },
    );
    config
        .providers
        .insert("mock".into(), ProviderConfig::default());

    let groups = super::models::default_model_candidates(&config);
    let names: Vec<&str> = groups.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec!["deepseek", "minimax"],
        "mock is not a real route"
    );
    // The row's own default model is offered even when the catalog is empty …
    assert_eq!(groups[0].1, vec!["deepseek-flash".to_string()]);
    // … and an explicit model list is used as-is.
    assert_eq!(groups[1].1, vec!["MiniMax-M3".to_string()]);
}

/// 设置页写表格时会重写整份配置，`[default-model]` 也在里面；但它另有写者
/// （主窗口的模型菜单）。没改过它就用磁盘现值，改过就用草稿——两条都要，
/// 少一条就有一类「改完又被改回去」。
#[test]
fn a_table_save_keeps_the_model_the_menu_just_picked() {
    use super::save::adopt_disk_default_model;
    use qcode_kernel::config::{Config, DefaultModelConfig};

    let snapshot = DefaultModelConfig {
        provider: Some("deepseek".into()),
        model: Some("deepseek-flash".into()),
    };
    let disk = Config {
        default_model: DefaultModelConfig {
            provider: Some("deepseek".into()),
            model: Some("DeepSeek-V4.1-Flash".into()),
        },
        ..Config::default()
    };

    // Untouched by this save → the menu's choice survives.
    let mut draft = Config {
        default_model: snapshot.clone(),
        ..Config::default()
    };
    adopt_disk_default_model(&mut draft, &snapshot, Some(&disk));
    assert_eq!(
        draft.default_model.model.as_deref(),
        Some("DeepSeek-V4.1-Flash")
    );

    // This save moved it (the row was edited) → the draft wins.
    let mut draft = Config {
        default_model: DefaultModelConfig {
            provider: Some("deepseek".into()),
            model: Some("another-model".into()),
        },
        ..Config::default()
    };
    adopt_disk_default_model(&mut draft, &snapshot, Some(&disk));
    assert_eq!(draft.default_model.model.as_deref(), Some("another-model"));

    // No readable file yet (first save): keep the draft.
    let mut draft = Config {
        default_model: snapshot.clone(),
        ..Config::default()
    };
    adopt_disk_default_model(&mut draft, &snapshot, None);
    assert_eq!(draft.default_model.model.as_deref(), Some("deepseek-flash"));
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
/// credential through `CredWriteW` (the secret arrives on stdin) and returns a
/// one-line PowerShell read command; that command must print the stored secret
/// back. Uses a per-process target so parallel test runs cannot collide, and
/// cleans up afterwards.
#[test]
#[cfg(target_os = "windows")]
fn windows_keychain_store_round_trips_via_credwrite_and_powershell() {
    use super::credentials::windows_credential_delete_script;
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
    assert!(
        !argv.iter().any(|arg| arg.contains(secret)),
        "the secret must never reach argv"
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
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &windows_credential_delete_script(&format!("qcode/{provider}")),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The generated PowerShell must stay on one line and must not let a provider
/// name close the string it is embedded in.
#[test]
#[cfg(any(target_os = "windows", test))]
fn windows_credential_scripts_quote_provider_names() {
    use super::credentials::{
        ps_single_quoted, windows_credential_delete_script, windows_credential_read_script,
        windows_credential_write_script,
    };
    assert_eq!(ps_single_quoted("deepseek"), "'deepseek'");
    assert_eq!(ps_single_quoted("o'brien"), "'o''brien'");
    let write = windows_credential_write_script("qcode/o'brien", "o'brien");
    assert!(!write.contains('\n'), "the write script must be one line");
    assert!(write.contains("'qcode/o''brien'"), "{write}");
    assert!(
        write.contains("[Console]::In.ReadToEnd()"),
        "the secret must come from stdin: {write}"
    );
    let read = windows_credential_read_script("qcode/deepseek");
    assert!(!read.contains('\n'), "the read script must be one line");
    assert!(read.ends_with("[QCred]::Get('qcode/deepseek')"), "{read}");
    let delete = windows_credential_delete_script("qcode/deepseek");
    assert!(!delete.contains('\n'), "the delete script must be one line");
    assert!(
        delete.contains("[QCred]::Del('qcode/deepseek')"),
        "{delete}"
    );
}

/// macOS round trip: the key reaches the keychain without passing through
/// `argv`, and the command stored in `api_key_command` reads it back.
#[test]
#[cfg(target_os = "macos")]
fn macos_keychain_store_keeps_the_secret_out_of_argv() {
    use super::credentials::macos;
    use super::keychain_store;

    if !macos::available() {
        eprintln!("skipping: this session has no default keychain");
        return;
    }
    let provider = format!("test-{}", std::process::id());
    let secret = "sk-roundtrip-123";
    let argv = match keychain_store(&provider, secret) {
        Ok(argv) => argv,
        Err(error) if is_keychain_unavailable(&error) => {
            eprintln!("skipping: keychain unavailable ({error})");
            return;
        }
        Err(error) => panic!("store failed: {error}"),
    };
    assert!(
        !argv.iter().any(|arg| arg.contains(secret)),
        "the secret must never reach argv: {argv:?}"
    );
    // Saving the same provider twice goes through the duplicate-item branch.
    keychain_store(&provider, secret).expect("rewrite the existing item");
    let output = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .expect("run the read command");
    assert!(output.status.success(), "read command failed");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), secret);
    macos::delete("qcode", &provider).expect("delete the probe item");
}

#[cfg(target_os = "macos")]
fn is_keychain_unavailable(error: &str) -> bool {
    error.contains("errSecInteractionNotAllowed") || error.contains("errSecNoSuchKeychain")
}

/// Closing the window must not drop the edit the debounce is still holding.
///
/// The bug this pins down: the timer woke up, saw `closing`, and returned without
/// writing, so anything typed in the last ~250 ms was lost — by the X button and by a
/// window close alike, on a screen that says 自动保存 · 修改即时写入.
#[test]
fn closing_flushes_the_pending_auto_save_instead_of_dropping_it() {
    let mut flushed = Vec::new();
    let outcome = decide_close(
        false,
        Some(AutoSaveKind::Scalar(&["providers", "key"])),
        false,
        |kind| {
            flushed.push(kind);
            Ok(())
        },
    );
    assert_eq!(outcome, CloseOutcome::Close);
    assert_eq!(
        flushed,
        vec![AutoSaveKind::Scalar(&["providers", "key"])],
        "the pending edit was never written"
    );
}

#[test]
fn closing_with_nothing_pending_writes_nothing() {
    let mut flushed = false;
    let outcome = decide_close(false, None, false, |_| {
        flushed = true;
        Ok(())
    });
    assert_eq!(outcome, CloseOutcome::Close);
    assert!(
        !flushed,
        "nothing was pending, so nothing should be written"
    );
}

#[test]
fn a_failed_write_keeps_the_window_open_once_and_then_lets_it_go() {
    let failing = |_: AutoSaveKind| Err("config.toml 没能写入：磁盘满".to_string());
    let pending = Some(AutoSaveKind::Tables);

    // First attempt: stay open so the message is read.
    let first = decide_close(false, pending, false, failing);
    assert_eq!(
        first,
        CloseOutcome::StayOpen("config.toml 没能写入：磁盘满".to_string()),
        "a failed write must not lose the edit silently"
    );

    // Second attempt: the user has seen the message and asked again. A read-only
    // config file must not trap someone in the settings window.
    let second = decide_close(false, pending, true, failing);
    assert_eq!(second, CloseOutcome::CloseDespiteFailure);
}

/// The wizard has an explicit save form and never auto saves, so closing it is its
/// Cancel — no flush, and no "did you save?" interruption.
#[test]
fn the_wizard_closes_without_touching_auto_save() {
    let mut flushed = false;
    let outcome = decide_close(true, Some(AutoSaveKind::Tables), false, |_| {
        flushed = true;
        Ok(())
    });
    assert_eq!(outcome, CloseOutcome::Close);
    assert!(!flushed);
}
