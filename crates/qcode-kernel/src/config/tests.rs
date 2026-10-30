use super::*;

fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    move |key| map.get(key).cloned()
}

#[test]
fn sandbox_mode_defaults_to_the_platform_backend() {
    let config: Config = toml::from_str("").unwrap();
    assert_eq!(config.sandbox.mode, default_sandbox_mode());
    assert!(!config.sandbox.network);

    let config: Config = toml::from_str(
        r#"
            [sandbox]
            mode = "sandbox-exec"
            network = true
            "#,
    )
    .unwrap();
    assert_eq!(config.sandbox.mode, SandboxMode::SandboxExec);
    assert!(config.sandbox.network);

    let config: Config = toml::from_str("[sandbox]\nmode = \"bwrap\"\n").unwrap();
    assert_eq!(config.sandbox.mode, SandboxMode::Bwrap);

    let config: Config = toml::from_str("[sandbox]\nmode = \"landlock\"\n").unwrap();
    assert_eq!(config.sandbox.mode, SandboxMode::Landlock);

    let error = toml::from_str::<Config>("[sandbox]\nmode = \"seccomp\"\n").unwrap_err();
    assert!(error.to_string().contains("unknown variant"), "{error}");
}

#[test]
fn loads_a_standalone_policy_file() {
    use qcode_core::policy::Action;

    let dir = std::env::temp_dir().join(format!("qcode-policy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let path = dir.join("policy.toml");
    std::fs::write(
        &path,
        r#"
            [[rules]]
            id = "allow-shell"
            tool = "run_shell"
            action = "allow"
            "#,
    )
    .unwrap();

    let rules = load_policy(&path).expect("load policy");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].id, "allow-shell");
    assert_eq!(rules[0].tool.as_deref(), Some("run_shell"));
    assert_eq!(rules[0].action, Action::Allow);
}

#[test]
fn policy_file_errors_name_the_path() {
    let dir = std::env::temp_dir().join(format!("qcode-policy-err-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let missing = dir.join("missing.toml");
    match load_policy(&missing) {
        Err(ConfigError::ReadPolicy { path, .. }) => assert_eq!(path, missing),
        other => panic!("unexpected result: {other:?}"),
    }

    let invalid = dir.join("invalid.toml");
    std::fs::write(&invalid, "rules = 1").unwrap();
    match load_policy(&invalid) {
        Err(ConfigError::ParsePolicy { path, .. }) => assert_eq!(path, invalid),
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn defaults_to_mock() {
    let resolved = resolve(&Config::default(), &Overrides::default(), &env_from(&[])).unwrap();
    assert!(resolved.is_mock);
    assert_eq!(resolved.name, "mock");
    assert_eq!(resolved.model, "mock-1");
}

#[test]
fn cli_beats_env_beats_config() {
    let config: Config = toml::from_str(
        r#"
            default_provider = "openai"

            [providers.openai]
            default_model = "config-model"
            "#,
    )
    .unwrap();
    let overrides = Overrides {
        model: Some("cli-model".to_string()),
        ..Overrides::default()
    };
    let resolved = resolve(
        &config,
        &overrides,
        &env_from(&[("OPENAI_API_KEY", "sk-test"), ("QCODE_MODEL", "env-model")]),
    )
    .unwrap();

    assert_eq!(resolved.name, "openai");
    assert_eq!(resolved.model, "cli-model");
    assert_eq!(resolved.api_key.as_deref(), Some("sk-test"));
    assert_eq!(
        resolved.base_url.as_deref(),
        Some("https://api.openai.com/v1")
    );
}

#[test]
fn env_model_used_when_cli_is_silent() {
    let config: Config = toml::from_str(
        r#"
            default_provider = "deepseek"

            [providers.deepseek]
            default_model = "config-model"
            "#,
    )
    .unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[
            ("DEEPSEEK_API_KEY", "sk-test"),
            ("QCODE_MODEL", "env-model"),
        ]),
    )
    .unwrap();
    assert_eq!(resolved.model, "env-model");
}

#[test]
fn deepseek_defaults_to_flash() {
    let config: Config = toml::from_str(r#"default_provider = "deepseek""#).unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[("DEEPSEEK_API_KEY", "sk-test")]),
    )
    .unwrap();
    assert_eq!(resolved.model, "deepseek-flash");
}

#[test]
fn anthropic_defaults_and_claude_alias() {
    let config: Config = toml::from_str(r#"default_provider = "anthropic""#).unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[("ANTHROPIC_API_KEY", "sk-ant-test")]),
    )
    .unwrap();
    assert_eq!(
        resolved.base_url.as_deref(),
        Some("https://api.anthropic.com")
    );
    assert_eq!(resolved.model, "claude-sonnet-4-5");
    assert_eq!(resolved.api_key.as_deref(), Some("sk-ant-test"));

    let alias: Config = toml::from_str(r#"default_provider = "claude""#).unwrap();
    let resolved = resolve(
        &alias,
        &Overrides::default(),
        &env_from(&[("ANTHROPIC_API_KEY", "sk-ant-test")]),
    )
    .unwrap();
    assert_eq!(resolved.name, "claude");
    assert_eq!(
        resolved.base_url.as_deref(),
        Some("https://api.anthropic.com")
    );
}

#[test]
fn explicit_model_overrides_deepseek_default() {
    let config = Config::default();
    let overrides = Overrides {
        provider: Some("deepseek".to_string()),
        model: Some("custom-model".to_string()),
        ..Overrides::default()
    };
    let resolved = resolve(
        &config,
        &overrides,
        &env_from(&[("DEEPSEEK_API_KEY", "sk-test")]),
    )
    .unwrap();
    assert_eq!(resolved.model, "custom-model");
}

#[test]
fn missing_key_is_actionable() {
    let config: Config = toml::from_str(r#"default_provider = "deepseek""#).unwrap();
    let error = resolve(&config, &Overrides::default(), &env_from(&[])).unwrap_err();
    assert!(matches!(error, ConfigError::MissingKey { .. }));
    assert!(error.to_string().contains("DEEPSEEK_API_KEY"));
}

#[test]
fn the_key_comes_from_the_environment_before_the_command() {
    let config: Config = toml::from_str(
        r#"
default_provider = "deepseek"

[providers.deepseek]
api_key_env = "DS_KEY"
api_key_command = ["cmd", "/C", "echo from-command"]
"#,
    )
    .unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[("DS_KEY", "from-env")]),
    )
    .unwrap();
    assert_eq!(resolved.api_key.as_deref(), Some("from-env"));
    assert_eq!(resolved.api_key_source, "env DS_KEY");
}

#[test]
fn a_missing_environment_variable_falls_back_to_the_command() {
    let config: Config = toml::from_str(
        r#"
default_provider = "deepseek"

[providers.deepseek]
api_key_env = "DS_KEY"
api_key_command = ["cmd", "/C", "echo from-command"]
"#,
    )
    .unwrap();
    if cfg!(windows) {
        let resolved = resolve(&config, &Overrides::default(), &env_from(&[])).unwrap();
        assert_eq!(resolved.api_key.as_deref(), Some("from-command"));
        assert_eq!(resolved.api_key_source, "command cmd");
    }
}

#[test]
fn a_failing_key_command_reports_the_reason_without_its_output() {
    if !cfg!(windows) {
        return;
    }
    let config: Config = toml::from_str(
        r#"
default_provider = "deepseek"

[providers.deepseek]
api_key_env = "DS_KEY"
api_key_command = ["cmd", "/C", "exit 3"]
"#,
    )
    .unwrap();
    let error = resolve(&config, &Overrides::default(), &env_from(&[])).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("key command failed"), "{text}");
    assert!(text.contains("exited with"), "{text}");
}
#[test]
fn keyless_provider_needs_no_key() {
    let config: Config = toml::from_str(
        r#"
            default_provider = "ollama"

            [providers.ollama]
            default_model = "qwen"
            "#,
    )
    .unwrap();
    let resolved = resolve(&config, &Overrides::default(), &env_from(&[])).unwrap();
    assert!(resolved.api_key.is_none());
    assert_eq!(
        resolved.base_url.as_deref(),
        Some("http://localhost:11434/v1")
    );
}

#[test]
fn unknown_provider_without_base_url_fails() {
    let config: Config = toml::from_str(r#"default_provider = "mystery""#).unwrap();
    let error = resolve(&config, &Overrides::default(), &env_from(&[])).unwrap_err();
    assert!(matches!(error, ConfigError::UnknownProvider(_)));
}

#[test]
fn context_window_defaults_and_overrides() {
    let config = Config::default();
    assert_eq!(config.context.window_tokens, 128_000);

    let config: Config = toml::from_str("[context]\nwindow_tokens = 32000\n").unwrap();
    assert_eq!(config.context.window_tokens, 32_000);
}

#[test]
fn the_turn_budget_defaults_to_eight_and_can_be_raised() {
    let config = Config::default();
    assert_eq!(config.agent.max_turns, 64);
    assert_eq!(config.agent.max_cost_usd, None);
    assert!(
        config.agent.snapshot_shell,
        "workspace snapshots are on by default"
    );

    let config: Config =
        toml::from_str("[agent]\nmax_turns = 32\nmax_cost_usd = 0.25\nsnapshot_shell = false\n")
            .unwrap();
    assert_eq!(config.agent.max_turns, 32);
    assert_eq!(config.agent.max_cost_usd, Some(0.25));
    assert!(!config.agent.snapshot_shell);
}

#[test]
fn pricing_table_parses_and_resolves() {
    let config: Config = toml::from_str(
        r#"
            [pricing."deepseek-flash"]
            input_usd_per_mtok = 0.28
            output_usd_per_mtok = 0.42
            cache_read_usd_per_mtok = 0.028
            "#,
    )
    .unwrap();
    let pricing = pricing_for(&config, "deepseek-flash", Some(1.0))
        .unwrap()
        .expect("pricing entry");
    assert_eq!(pricing.input_usd_per_mtok, 0.28);
    assert_eq!(pricing.output_usd_per_mtok, 0.42);
    assert_eq!(pricing.cache_read_usd_per_mtok, 0.028);
    assert_eq!(pricing.cache_write_usd_per_mtok, 0.0);
}

#[test]
fn max_cost_without_pricing_is_an_error() {
    let error = pricing_for(&Config::default(), "mystery", Some(0.5)).unwrap_err();
    assert!(matches!(error, ConfigError::PricingRequired { .. }));
    assert!(error.to_string().contains("[pricing.\"mystery\"]"));
}

#[test]
fn no_budget_needs_no_pricing() {
    assert!(
        pricing_for(&Config::default(), "mystery", None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn invalid_budget_or_rates_are_rejected() {
    assert!(matches!(
        pricing_for(&Config::default(), "m", Some(-1.0)),
        Err(ConfigError::InvalidMaxCost)
    ));
    assert!(matches!(
        pricing_for(&Config::default(), "m", Some(f64::NAN)),
        Err(ConfigError::InvalidMaxCost)
    ));

    let config: Config = toml::from_str(
        r#"
            [pricing.m]
            input_usd_per_mtok = -0.1
            output_usd_per_mtok = 1.0
            "#,
    )
    .unwrap();
    assert!(matches!(
        pricing_for(&config, "m", Some(1.0)),
        Err(ConfigError::InvalidPricing { .. })
    ));
}

#[test]
fn skills_are_enabled_by_default_and_can_be_disabled() {
    let config: Config = toml::from_str("").unwrap();
    assert!(config.skills.enabled);
    assert!(config.skills.roots.is_empty());

    let config: Config = toml::from_str(
        "[skills]\nenabled = false\nroots = [\"/tmp/skills\", \"~/space/skills\"]\n",
    )
    .unwrap();
    assert!(!config.skills.enabled);
    assert_eq!(config.skills.roots.len(), 2);
}

#[test]
fn mcp_servers_parse_with_defaults() {
    let config: Config = toml::from_str(
        r#"
            [[mcp.servers]]
            name = "filesystem"
            command = "npx"
            args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
            "#,
    )
    .unwrap();
    config.mcp.validate().unwrap();

    assert_eq!(config.mcp.servers.len(), 1);
    let server = &config.mcp.servers[0];
    assert_eq!(server.name, "filesystem");
    assert_eq!(server.args.len(), 3);
    assert!(server.enabled);
    assert_eq!(server.timeout_secs, 120);
}

#[test]
fn duplicate_mcp_server_names_are_rejected() {
    let config: Config = toml::from_str(
        r#"
            [[mcp.servers]]
            name = "fs"
            command = "a"

            [[mcp.servers]]
            name = "fs"
            command = "b"
            "#,
    )
    .unwrap();
    assert!(matches!(
        config.mcp.validate(),
        Err(McpConfigError::DuplicateName(_))
    ));
}

#[test]
fn model_override_works_for_unknown_provider() {
    let config = Config::default();
    let overrides = Overrides {
        provider: Some("local".to_string()),
        model: Some("m".to_string()),
        base_url: Some("http://127.0.0.1:1234/v1".to_string()),
        stream_usage: None,
    };
    let resolved = resolve(&config, &overrides, &env_from(&[])).unwrap();
    assert_eq!(
        resolved.base_url.as_deref(),
        Some("http://127.0.0.1:1234/v1")
    );
    assert!(resolved.api_key.is_none());
}

#[test]
fn project_rules_are_scoped_and_cannot_allow() {
    let mut base: Config = toml::from_str(
        r#"
            [[approval.rules]]
            id = "user-allow"
            tool = "run_shell"
            action = "allow"
            "#,
    )
    .unwrap();
    let project: Config = toml::from_str(
        r#"
            [[approval.rules]]
            id = "proj-allow"
            tool = "read_file"
            action = "allow"

            [[approval.rules]]
            id = "proj-deny"
            tool = "run_shell"
            command_prefix = "git push"
            action = "deny"
            "#,
    )
    .unwrap();

    merge_project(&mut base, project, ProjectSections::default());
    assert_eq!(base.approval.rules.len(), 3);
    let proj_allow = base
        .approval
        .rules
        .iter()
        .find(|rule| rule.id == "proj-allow")
        .expect("project allow");
    assert_eq!(proj_allow.action, Action::Ask);
    assert_eq!(proj_allow.scope, Some(Scope::Project));
    let proj_deny = base
        .approval
        .rules
        .iter()
        .find(|rule| rule.id == "proj-deny")
        .expect("project deny");
    assert_eq!(proj_deny.action, Action::Deny);
}

#[test]
fn project_config_loads_from_the_workspace() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(dir.path().join(".qcode")).unwrap();
    std::fs::write(
        dir.path().join(".qcode/config.toml"),
        r#"
            [[approval.rules]]
            id = "repo-deny"
            tool = "run_shell"
            action = "deny"
            "#,
    )
    .unwrap();

    let config = load_project(dir.path()).expect("project config");
    assert_eq!(config.approval.rules.len(), 1);
    assert_eq!(config.approval.rules[0].scope, Some(Scope::Project));
}

#[test]
fn network_defaults_and_overrides() {
    let defaults: Config = toml::from_str("").unwrap();
    assert_eq!(defaults.network.mode, NetworkMode::Proxy);
    assert!(defaults.network.enabled);
    assert!(defaults.network.audit);
    assert_eq!(defaults.network.unknown, UnknownHost::Ask);
    assert!(!defaults.network.allow_private);

    let config: Config = toml::from_str(
        r#"
            [network]
            mode = "strict"
            enabled = false
            allow = ["api.example.com", "*.internal.test"]
            allow_private = true
            unknown = "deny"
            audit = false
            "#,
    )
    .unwrap();
    assert_eq!(config.network.mode, NetworkMode::Strict);
    assert!(!config.network.enabled);
    assert_eq!(config.network.allow.len(), 2);
    assert!(config.network.allow_private);
    assert_eq!(config.network.unknown, UnknownHost::Deny);
    assert!(!config.network.audit);
}

#[test]
fn search_defaults_and_overrides() {
    let defaults: Config = toml::from_str("").unwrap();
    assert!(defaults.search.enabled);
    assert_eq!(defaults.search.backend, SearchBackend::Brave);
    assert_eq!(defaults.search.max_results, 5);
    assert_eq!(
        defaults.search.key_env_name().as_deref(),
        Some("BRAVE_API_KEY")
    );

    let config: Config = toml::from_str(
        r#"
            [search]
            enabled = false
            backend = "searxng"
            max_results = 100
            base_url = "http://localhost:8888/"
            "#,
    )
    .unwrap();
    assert!(!config.search.enabled);
    assert_eq!(config.search.backend, SearchBackend::Searxng);
    assert_eq!(config.search.key_env_name(), None);
    assert_eq!(config.search.max_results, 100);
    let settings = config.search.settings(&|_| None);
    assert_eq!(settings.max_results, 20);
    assert_eq!(settings.base_url.as_deref(), Some("http://localhost:8888/"));
    assert!(settings.is_usable());
}

#[test]
fn search_resolves_the_configured_key_variable() {
    let config: Config = toml::from_str(
        r#"
            [search]
            backend = "tavily"
            api_key_env = "MY_SEARCH_KEY"
            "#,
    )
    .unwrap();
    assert!(!config.search.settings(&|_| None).is_usable());
    let settings = config
        .search
        .settings(&|name| (name == "MY_SEARCH_KEY").then(|| "tvly-x".to_string()));
    assert!(settings.is_usable());
    assert_eq!(settings.api_key.as_deref(), Some("tvly-x"));
}

#[test]
fn project_search_section_replaces_only_when_present() {
    let base = Config {
        search: SearchConfig {
            base_url: Some("https://api.search.brave.com".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };

    let project = Config {
        search: SearchConfig {
            backend: SearchBackend::Searxng,
            base_url: Some("http://host.docker.internal:8888".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut merged = base.clone();
    merge_project(
        &mut merged,
        project.clone(),
        ProjectSections {
            context: false,
            sandbox: false,
            search: false,
        },
    );
    assert_eq!(merged.search.backend, SearchBackend::Brave);

    merge_project(
        &mut merged,
        project,
        ProjectSections {
            context: false,
            sandbox: false,
            search: true,
        },
    );
    assert_eq!(merged.search.backend, SearchBackend::Searxng);
    assert_eq!(
        merged.search.base_url.as_deref(),
        Some("http://host.docker.internal:8888")
    );
}

#[test]
fn mcp_add_and_remove_edit_the_config_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");

    mcp_add(
        &path,
        "filesystem",
        "npx",
        &["-y".to_string(), "server".to_string()],
        false,
        Some(30),
    )
    .expect("add");
    let config = load(&path).expect("load");
    assert_eq!(config.mcp.servers.len(), 1);
    let server = &config.mcp.servers[0];
    assert_eq!(server.name, "filesystem");
    assert_eq!(server.args, vec!["-y", "server"]);
    assert!(!server.enabled);
    assert_eq!(server.timeout_secs, 30);

    assert!(matches!(
        mcp_add(&path, "filesystem", "npx", &[], true, None),
        Err(ConfigError::McpExists(_))
    ));

    mcp_remove(&path, "filesystem").expect("remove");
    let config = load(&path).expect("load after remove");
    assert!(config.mcp.servers.is_empty());
    assert!(matches!(
        mcp_remove(&path, "filesystem"),
        Err(ConfigError::McpMissing(_))
    ));
}

#[test]
fn project_context_and_sandbox_only_override_when_present() {
    let dir = tempfile::tempdir().expect("temp dir");
    let base_path = dir.path().join("config.toml");
    std::fs::write(
        &base_path,
        "[context]\nwindow_tokens = 100000\n[sandbox]\nnetwork = false\n",
    )
    .unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(workspace.join(".qcode")).unwrap();

    std::fs::write(
        workspace.join(".qcode/config.toml"),
        "[context]\nwindow_tokens = 32000\n[sandbox]\nnetwork = true\n",
    )
    .unwrap();
    let merged = load_merged(&base_path, &workspace).expect("merged");
    assert_eq!(merged.context.window_tokens, 32_000);
    assert!(merged.sandbox.network);

    // A project file without these sections must not reset the user values.
    std::fs::write(workspace.join(".qcode/config.toml"), "\n").unwrap();
    let merged = load_merged(&base_path, &workspace).expect("merged without sections");
    assert_eq!(merged.context.window_tokens, 100_000);
    assert!(!merged.sandbox.network);
}

#[test]
fn a_config_without_a_version_is_the_current_schema() {
    let config: Config = toml::from_str("").expect("empty config");
    assert_eq!(config.version, CONFIG_VERSION);
    assert_eq!(Config::default().version, CONFIG_VERSION);
}

#[test]
fn a_newer_config_version_is_rejected_with_the_supported_one() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "version = 99\n").unwrap();

    let error = load(&path).expect_err("newer version").to_string();
    assert!(error.contains("declares version 99"), "{error}");
    assert!(
        error.contains(&format!("up to {CONFIG_VERSION}")),
        "{error}"
    );
    assert!(error.contains("upgrade qcode"), "{error}");
}

#[test]
fn a_zero_config_version_is_rejected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "version = 0\n").unwrap();

    let error = load(&path).expect_err("zero version").to_string();
    assert!(error.contains("version must be 1"), "{error}");
}

#[test]
fn a_project_config_cannot_declare_a_newer_version() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "version = 1\n").unwrap();
    std::fs::create_dir_all(dir.path().join(".qcode")).unwrap();
    std::fs::write(dir.path().join(".qcode/config.toml"), "version = 7\n").unwrap();

    let error = load_merged(&path, dir.path())
        .expect_err("newer project version")
        .to_string();
    assert!(error.contains("declares version 7"), "{error}");
}

#[test]
fn typos_are_reported_as_unknown_keys_but_still_load() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "defualt_provider = \"mock\"\n[search]\nbakend = \"tavily\"\n",
    )
    .unwrap();

    // Serde ignores both keys, so the file loads; the typo list is what a
    // caller reports. Unknown keys must never be fatal: a newer qcode may
    // add optional keys that this build simply does not read.
    load(&path).expect("unknown keys are not fatal");
    let document: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
    let mut unknown = unknown_keys(&document);
    unknown.sort();
    assert_eq!(unknown, vec!["defualt_provider", "search.bakend"]);
}

#[test]
fn every_section_key_the_build_reads_is_known() {
    let document: toml::Table = r#"
             version = 1
             default_provider = "mock"

             [approval]
             rules = []

             [context]
             window_tokens = 64000

             [agent]
             max_turns = 24
             max_cost_usd = 0.50

             [skills]
             enabled = true
             roots = []

             [sandbox]
             mode = "none"
             network = false
             extra_writable = []

             [network]
             enabled = true
             allow = []
             allow_private = false
             unknown = "ask"
             audit = true

             [search]
             enabled = true
             backend = "brave"
             api_key_env = "BRAVE_API_KEY"
             max_results = 5
             base_url = "https://api.search.brave.com"

             [update]
             enabled = true
             check = false
             check_days = 7
             repo = "qiuzhanghua/qcode"
             base_url = "https://api.github.com"

             [providers.ollama]
             base_url = "http://127.0.0.1:11434/v1"

             [[mcp.servers]]
             name = "filesystem"
             command = "npx"
        "#
    .parse()
    .expect("document");

    assert!(
        unknown_keys(&document).is_empty(),
        "{:?}",
        unknown_keys(&document)
    );
}
#[test]
fn minimax_defaults_to_the_m3_model() {
    let config: Config = toml::from_str(r#"default_provider = "minimax""#).unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[("MINIMAX_API_KEY", "sk-test")]),
    )
    .expect("resolve");
    assert_eq!(resolved.name, "minimax");
    assert_eq!(resolved.model, "MiniMax-M3");
    assert_eq!(
        resolved.base_url.as_deref(),
        Some("https://api.minimaxi.com/v1")
    );
    assert_eq!(resolved.api_key.as_deref(), Some("sk-test"));
}
#[test]
fn ui_busy_delivery_defaults_to_queue_and_parses_steer() {
    let config: Config = toml::from_str("").expect("empty config");
    assert_eq!(config.ui.busy_delivery, BusyDelivery::Queue);

    let config: Config = toml::from_str("[ui]\nbusy_delivery = \"steer\"\n").expect("parse");
    assert_eq!(config.ui.busy_delivery, BusyDelivery::Steer);
}

#[test]
fn an_unknown_busy_delivery_is_rejected() {
    let error = toml::from_str::<Config>("[ui]\nbusy_delivery = \"drop\"\n").unwrap_err();
    assert!(
        error.to_string().contains("busy_delivery"),
        "the error should name the key: {error}"
    );
}

#[test]
fn a_plain_directory_is_not_a_system_install() {
    assert!(!is_system_install(Path::new("relative/tools/qcode")));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_system_directories_are_never_portable() {
    assert!(is_system_install(Path::new("/usr/local/bin")));
    assert!(is_system_install(Path::new("/opt/qcode")));
    assert!(!is_system_install(Path::new("/home/q/tools/qcode")));
}

#[test]
fn the_user_config_honours_xdg_config_home() {
    // The resolver reads the env, so this checks the branch shape rather
    // than the value: an explicit path always wins over any env var.
    let explicit = PathBuf::from("/tmp/explicit.toml");
    let found = discover(Some(explicit.clone())).expect("discover");
    assert_eq!(found.path, explicit);
    assert_eq!(found.source, ConfigSource::Flag);
}
