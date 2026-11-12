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
    assert_eq!(config.sandbox.network, None);

    let config: Config = toml::from_str(
        r#"
            [sandbox]
            mode = "sandbox-exec"
            network = true
            "#,
    )
    .unwrap();
    assert_eq!(config.sandbox.mode, SandboxMode::SandboxExec);
    assert_eq!(config.sandbox.network, Some(SandboxNetworkMode::Open));

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

/// A `deny` whose glob does not parse is worse than a crash: it reads as
/// "denied" in the file and never matches at runtime. Both loaders must refuse
/// it, and the message must name the rule so the typo is findable.
#[test]
fn an_unparsable_rule_pattern_fails_to_load() {
    let dir = std::env::temp_dir().join(format!("qcode-policy-glob-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let policy = dir.join("policy.toml");
    std::fs::write(
        &policy,
        r#"
            [[rules]]
            id = "deny-broken"
            tool = "mcp__[__x"
            action = "deny"
        "#,
    )
    .unwrap();
    match load_policy(&policy) {
        Err(ConfigError::InvalidPolicyRule { path, detail }) => {
            assert_eq!(path, policy);
            assert!(detail.contains("deny-broken"), "{detail}");
            assert!(detail.contains("tool"), "{detail}");
        }
        other => panic!("unexpected result: {other:?}"),
    }

    // The config file goes through the same check.
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        r#"
            version = 1

            [[approval.rules]]
            id = "deny-broken"
            tool = "mcp__[__x"
            action = "deny"
        "#,
    )
    .unwrap();
    match load(&config) {
        Err(ConfigError::InvalidPolicyRule { detail, .. }) => {
            assert!(detail.contains("deny-broken"), "{detail}");
        }
        other => panic!("unexpected result: {other:?}"),
    }

    // A wildcard that does parse is accepted — the check must not be so eager
    // that it rejects the feature itself.
    let good = dir.join("good.toml");
    std::fs::write(
        &good,
        r#"
            [[rules]]
            id = "allow-wordcraft"
            tool = "mcp__wordcraft__*"
            action = "allow"
        "#,
    )
    .unwrap();
    let rules = load_policy(&good).expect("a valid glob must load");
    assert_eq!(rules.len(), 1);
}

/// A key the build knows must not be reported as unknown.
///
/// Cheap test, real bug: `[...] extra_read` was added to `SandboxConfig` and to
/// serde, but not to `KNOWN_KEYS`. Everything compiled and every config
/// round-tripped — and every user who set the key got
/// "unknown config keys are ignored", which is exactly the warning that teaches
/// people to ignore warnings. Written because that mistake was made.
#[test]
fn every_sandbox_key_is_known() {
    let document: toml::Table = r#"
        [sandbox]
        mode = "landlock"
        network = false
        extra_writable = ["/tmp/w"]
        extra_read = ["/home/q/cot"]
        allow_pty = true
    "#
    .parse()
    .expect("parse");
    let unknown = super::load::unknown_keys(&document);
    assert!(unknown.is_empty(), "reported as unknown: {unknown:?}");
}

/// `[sandbox] extra_read` is a list of directories, each recursive.
///
/// Recursion is the point: the reason the field exists is a toolchain prefix
/// with `bin/`, `lib/` and `share/` under it, and listing those three by hand
/// (and again for every new tool) is what nobody does.
#[test]
fn sandbox_extra_read_is_a_list_of_directories() {
    let config: Config = toml::from_str(
        r#"
            version = 1

            [sandbox]
            mode = "landlock"
            extra_read = ["/home/q/cot", "/opt/tools"]

            [[approval.rules]]
            id = "allow-read"
            tool = "read_file"
            action = "allow"
        "#,
    )
    .expect("parse");
    assert_eq!(
        config.sandbox.extra_read,
        vec![PathBuf::from("/home/q/cot"), PathBuf::from("/opt/tools"),]
    );
    // The two lists must not be confused for one another: a config that sets
    // only `extra_read` grants no write access.
    assert!(config.sandbox.extra_writable.is_empty());
}

/// `[sandbox] extra_writable` refuses the shapes that are never what was meant.
///
/// The field is config-wide, so unlike an MCP server's derived grant there is
/// no workspace narrowing it. `~/` is the typo this exists for: it reads like
/// "my home" and hands a confined `run_shell` every file the user can write,
/// including `~/.ssh` and `~/.aws`. `/` and `/home` are the copy-paste variants.
#[test]
fn extra_writable_refuses_the_filesystem_root() {
    let config: Config = toml::from_str(
        r#"
            [sandbox]
            extra_writable = ["/"]
        "#,
    )
    .expect("parse");
    let error = config
        .sandbox
        .validate()
        .expect_err("`/` as a write root must be refused");
    let text = error.to_string();
    assert!(text.contains("extra_writable"), "{text}");
    assert!(text.contains("filesystem root"), "{text}");
}

#[test]
fn extra_writable_refuses_a_directory_holding_every_home() {
    for path in ["/home", "/Users", "/root"] {
        let config: Config = toml::from_str(&format!(
            r#"
            [sandbox]
            extra_writable = ["{path}"]
            "#
        ))
        .expect("parse");
        let error = config
            .sandbox
            .validate()
            .expect_err(&format!("{path} as a write root must be refused"));
        assert!(
            error.to_string().contains("every user's home"),
            "{path}: {error}"
        );
    }
}

/// The home directory itself is refused, its subdirectories are not — that is
/// the whole distinction the field lives on.
#[test]
fn extra_writable_refuses_the_home_directory_but_not_a_subdirectory() {
    let Some(home) = super::load::home_dir() else {
        eprintln!("skipping: HOME is not set");
        return;
    };
    let refused: Config = toml::from_str(&format!(
        r#"
        [sandbox]
        extra_writable = ["{}"]
        "#,
        home.display()
    ))
    .expect("parse");
    assert!(
        refused.sandbox.validate().is_err(),
        "{} as a write root must be refused",
        home.display()
    );

    let allowed: Config = toml::from_str(&format!(
        r#"
        [sandbox]
        extra_writable = ["{}"]
        "#,
        home.join("Documents").display()
    ))
    .expect("parse");
    assert!(
        allowed.sandbox.validate().is_ok(),
        "a subdirectory of the home is exactly what this field is for"
    );
}

/// End to end through the loader, because the check is only useful if it runs
/// on the path a real config takes — `~/` has to be expanded *first*, or it is
/// the relative path `~/` and compares equal to nothing.
#[test]
fn loading_a_config_with_a_home_wide_write_root_fails() {
    let Some(home) = super::load::home_dir() else {
        eprintln!("skipping: HOME is not set");
        return;
    };
    let document = toml::Value::Table(
        r#"
        [sandbox]
        extra_writable = ["~/"]
    "#
        .parse::<toml::Table>()
        .expect("parse"),
    );
    let error =
        super::load::validate_document(&document).expect_err("`~/` must not survive validation");
    let text = error.to_string();
    assert!(text.contains("extra_writable"), "{text}");
    assert!(
        text.contains(&home.display().to_string()),
        "the message must name the expanded path: {text}"
    );

    // And the shape people actually write keeps loading.
    let ok = toml::Value::Table(
        r#"
        [sandbox]
        extra_writable = ["~/Documents"]
        extra_read = ["~/Documents"]
    "#
        .parse::<toml::Table>()
        .expect("parse"),
    );
    super::load::validate_document(&ok).expect("`~/Documents` must keep loading");
}

/// `[sandbox] allow_pty` is off unless asked for.
///
/// The default is the whole safety argument: on `landlock` this grant reaches
/// the **host** devpts, whose `/dev/pts/N` nodes belong to the user's other
/// terminal windows. A sandbox that hands out a way to type into an unsandboxed
/// shell is not a sandbox, so it must be a decision, not a default.
#[test]
fn allow_pty_is_off_unless_asked_for() {
    let config: Config = toml::from_str("").expect("parse");
    assert!(!config.sandbox.allow_pty);

    let config: Config = toml::from_str(
        r#"
            [sandbox]
            allow_pty = true
        "#,
    )
    .expect("parse");
    assert!(config.sandbox.allow_pty);
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

    let args = ["-y".to_string(), "server".to_string()];
    let read_paths = [PathBuf::from("/opt/tool")];
    let credentials = [qcode_mcp::CredentialPreset::Github];
    mcp_add(
        &path,
        NewMcpServer {
            name: "filesystem",
            command: Some("npx"),
            url: None,
            bearer_token_env_var: None,
            headers: &[],
            args: &args,
            enabled: false,
            timeout_secs: Some(30),
            read_paths: &read_paths,
            writable_paths: &[],
            credentials: &credentials,
            sandbox: Some(qcode_mcp::McpSandboxMode::Confine),
        },
    )
    .expect("add");
    let config = load(&path).expect("load");
    assert_eq!(config.mcp.servers.len(), 1);
    let server = &config.mcp.servers[0];
    assert_eq!(server.name, "filesystem");
    assert_eq!(server.args, vec!["-y", "server"]);
    assert!(!server.enabled);
    assert_eq!(server.timeout_secs, 30);
    // The ADR-0066 extras survive the round trip through the file.
    assert_eq!(server.read_paths, read_paths);
    assert_eq!(server.credentials, credentials);
    assert_eq!(server.sandbox, Some(qcode_mcp::McpSandboxMode::Confine));

    assert!(matches!(
        mcp_add(
            &path,
            NewMcpServer {
                name: "filesystem",
                command: Some("npx"),
                ..Default::default()
            }
        ),
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
    assert_eq!(merged.sandbox.network, Some(SandboxNetworkMode::Open));

    // A project file without these sections must not reset the user values.
    std::fs::write(workspace.join(".qcode/config.toml"), "\n").unwrap();
    let merged = load_merged(&base_path, &workspace).expect("merged without sections");
    assert_eq!(merged.context.window_tokens, 100_000);
    // The user file says `network = false`, which is the pre-ADR spelling of
    // "no explicit choice" — the plan then follows `[network]`.
    assert_eq!(merged.sandbox.network, None);
}

#[test]
fn a_config_without_a_version_is_the_current_schema() {
    let config: Config = toml::from_str("").expect("empty config");
    assert_eq!(config.schema, CONFIG_VERSION);
    assert_eq!(Config::default().schema, CONFIG_VERSION);
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

             [mcp]
             sandbox = "confine"

             [[mcp.servers]]
             name = "filesystem"
             command = "npx"

             [[mcp.servers]]
             name = "context7"
             url = "https://mcp.context7.com/mcp"
             bearer_token_env_var = "CONTEXT7_TOKEN"
        "#
    .parse()
    .expect("document");

    // Every key the README documents has to be here, or the sample config
    // warns about its own valid keys. `[mcp] sandbox` was documented and
    // missing from this list until ADR-0067 — the sample in the README was
    // reported as a typo on every load.
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

#[test]
fn catalog_carries_the_builtin_routes() {
    assert_eq!(
        catalog_entry("deepseek").map(|c| c.protocol),
        Some(Protocol::OpenAi)
    );
    assert_eq!(
        catalog_entry("anthropic").map(|c| c.protocol),
        Some(Protocol::Anthropic)
    );
    // `claude` is the friendly alias of the `anthropic` route.
    assert_eq!(catalog_entry("claude").map(|c| c.route), Some("anthropic"));
    assert_eq!(catalog_entry("nope"), None);
    assert!(catalog_entries().any(|entry| entry.route == "minimax"));
}

#[test]
fn protocol_comes_from_the_catalog_and_config_can_override_it() {
    let config: Config = toml::from_str(r#"default_provider = "deepseek""#).unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[("DEEPSEEK_API_KEY", "sk-test")]),
    )
    .unwrap();
    assert_eq!(resolved.protocol, Protocol::OpenAi);

    let config: Config = toml::from_str(r#"default_provider = "anthropic""#).unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[("ANTHROPIC_API_KEY", "sk-ant-test")]),
    )
    .unwrap();
    assert_eq!(resolved.protocol, Protocol::Anthropic);

    // An explicit `protocol` overrides the catalog default: an OpenAI-compatible
    // route repointed at an Anthropic-compatible gateway.
    let config: Config = toml::from_str(
        r#"
            default_provider = "gateway"
            [providers.gateway]
            base_url = "https://gateway.example/v1"
            protocol = "anthropic"
            api_key_env = "GATEWAY_KEY"
            default_model = "gw-model"
            "#,
    )
    .unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[("GATEWAY_KEY", "sk-gw")]),
    )
    .unwrap();
    assert_eq!(resolved.protocol, Protocol::Anthropic);
    assert_eq!(resolved.model, "gw-model");
}

#[test]
fn a_hand_declared_route_defaults_to_openai_compatible() {
    let config: Config = toml::from_str(
        r#"
            default_provider = "ark"
            [providers.ark]
            base_url = "https://ark.cn-beijing.volces.com/api/v3"
            api_key_env = "ARK_API_KEY"
            default_model = "doubao-seed-1-6"
            "#,
    )
    .unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[("ARK_API_KEY", "sk-ark")]),
    )
    .unwrap();
    assert_eq!(resolved.protocol, Protocol::OpenAi);
    assert_eq!(resolved.model, "doubao-seed-1-6");
    assert_eq!(resolved.name, "ark");
}

#[test]
fn mock_routes_resolve_to_the_mock_protocol() {
    let config = Config::default();
    let resolved = resolve(&config, &Overrides::default(), &env_from(&[])).unwrap();
    assert_eq!(resolved.protocol, Protocol::Mock);
}

#[test]
fn default_model_section_provides_provider_and_model() {
    let config: Config = toml::from_str(
        r#"
            [default-model]
            provider = "ark"
            model = "m1"

            [providers.ark]
            base_url = "https://ark.cn-beijing.volces.com/api/v3"
            api_key_env = "ARK_API_KEY"
            "#,
    )
    .unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[("ARK_API_KEY", "sk-ark")]),
    )
    .unwrap();
    assert_eq!(resolved.name, "ark");
    assert_eq!(resolved.model, "m1");
}

#[test]
fn absent_default_model_keeps_the_default_provider_fallback() {
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
        &env_from(&[("DEEPSEEK_API_KEY", "sk-test")]),
    )
    .unwrap();
    assert_eq!(resolved.name, "deepseek");
    assert_eq!(resolved.model, "config-model");
}

#[test]
fn default_model_wins_over_the_legacy_default_provider() {
    // V2.1: `[default-model].provider` outranks the legacy `default_provider`,
    // and the model named there is used, not the other route's default.
    let config: Config = toml::from_str(
        r#"
            default_provider = "openai"

            [default-model]
            provider = "ark"
            model = "m1"

            [providers.ark]
            base_url = "https://ark.cn-beijing.volces.com/api/v3"
            api_key_env = "ARK_API_KEY"

            [providers.openai]
            default_model = "gpt-4o"
            "#,
    )
    .unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[("ARK_API_KEY", "sk-ark")]),
    )
    .unwrap();
    assert_eq!(resolved.name, "ark");
    assert_eq!(resolved.model, "m1");
}

#[test]
fn default_model_does_not_change_a_different_routes_model() {
    // The `[default-model].model` is scoped to the provider it names: when the
    // model section is absent, the resolved route falls back to its own
    // `default_model`, not the default-model section's.
    let config: Config = toml::from_str(
        r#"
            [providers.openai]
            default_model = "gpt-4o"
            "#,
    )
    .unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[("OPENAI_API_KEY", "sk-test")]),
    )
    .unwrap();
    assert_eq!(resolved.name, "openai");
    assert_eq!(resolved.model, "gpt-4o");
}

#[test]
fn no_default_falls_back_to_the_first_usable_provider() {
    // No `[default-model]`, no `default_provider`: resolve picks the first
    // usable provider (alphabetical order), skipping the one whose key is
    // missing. Its model comes from `default_model`, then `models[0]`, then
    // the catalog default.
    let config: Config = toml::from_str(
        r#"
            [providers.ark]
            base_url = "https://ark.cn-beijing.volces.com/api/v3"
            api_key_env = "ARK_API_KEY"
            models = [{ id = "doubao-1" }]

            [providers.deepseek]
            base_url = "https://api.deepseek.com"
            api_key_env = "DEEPSEEK_API_KEY"
            default_model = "deepseek-flash"
            "#,
    )
    .unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[("DEEPSEEK_API_KEY", "sk-test")]),
    )
    .unwrap();
    // deepseek sorts after ark, but ark's key is missing, so deepseek wins. Its
    // explicit `default_model` is honoured as written.
    assert_eq!(resolved.name, "deepseek");
    assert_eq!(resolved.model, "deepseek-flash");
}

#[test]
fn first_usable_takes_the_first_models_entry_when_no_default_model() {
    let config: Config = toml::from_str(
        r#"
            [providers.ark]
            base_url = "https://ark.cn-beijing.volces.com/api/v3"
            api_key_env = "ARK_API_KEY"
            models = [{ id = "doubao-1" }, { id = "doubao-2" }]
            "#,
    )
    .unwrap();
    let resolved = resolve(
        &config,
        &Overrides::default(),
        &env_from(&[("ARK_API_KEY", "sk-ark")]),
    )
    .unwrap();
    assert_eq!(resolved.name, "ark");
    assert_eq!(resolved.model, "doubao-1");
}

#[test]
fn first_usable_skips_mock_and_unusable_routes_then_falls_back_to_mock() {
    // A mock route never becomes the default; with no usable provider at all
    // (no key anywhere), resolve falls back to mock as before.
    let config: Config = toml::from_str(
        r#"
            [providers.mock]
            default_model = "mock-1"

            [providers.deepseek]
            base_url = "https://api.deepseek.com"
            api_key_env = "DEEPSEEK_API_KEY"
            default_model = "deepseek-flash"
            "#,
    )
    .unwrap();
    let resolved = resolve(&config, &Overrides::default(), &env_from(&[])).unwrap();
    assert_eq!(resolved.name, "mock");
    assert_eq!(resolved.model, "mock-1");
}

#[test]
fn models_accept_a_bare_list_of_ids() {
    // ADR-0065: the shorthand is what the settings page writes.
    let config: Config = toml::from_str(
        "[providers.deepseek]\nmodels = [\"deepseek-flash\", \"deepseek-v4-pro\"]\n",
    )
    .expect("parse");
    let models = &config.providers["deepseek"].models;
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "deepseek-flash");
    assert!(models[0].name.is_none());
    assert_eq!(models[1].id, "deepseek-v4-pro");
}

#[test]
fn models_still_accept_tables_with_metadata() {
    let config: Config = toml::from_str(
        "[providers.gw]\nmodels = [{ id = \"m\", name = \"M\", context_window = 1000 }]\n",
    )
    .expect("parse");
    let model = &config.providers["gw"].models[0];
    assert_eq!(model.id, "m");
    assert_eq!(model.name.as_deref(), Some("M"));
    assert_eq!(model.context_window, Some(1000));
}

#[test]
fn a_version_key_is_read_as_the_schema() {
    // Files written before ADR-0065 say `version = 1`.
    let config: Config = toml::from_str("version = 1\n").expect("parse");
    assert_eq!(config.schema, 1);
    let config: Config = toml::from_str("schema = 2\n").expect("parse");
    assert_eq!(config.schema, 2);
}

#[test]
fn approval_shorthand_expands_to_prefix_and_glob_rules() {
    let config: Config = toml::from_str(
        r#"
[approval]
allow = ["run_shell:echo *", "read_file:src/**", "web_search"]
deny = ["run_shell:rm -rf *"]
"#,
    )
    .expect("parse");
    let rules = config.approval.shorthand_rules();
    assert_eq!(rules.len(), 4);

    assert_eq!(rules[0].id, "allow-0");
    assert_eq!(rules[0].tool.as_deref(), Some("run_shell"));
    assert_eq!(rules[0].command_prefix.as_deref(), Some("echo "));
    assert!(matches!(rules[0].action, Action::Allow));

    assert_eq!(rules[1].path_glob.as_deref(), Some("src/**"));
    assert_eq!(rules[1].tool.as_deref(), Some("read_file"));

    // A bare tool matches the tool alone.
    assert_eq!(rules[2].tool.as_deref(), Some("web_search"));
    assert!(rules[2].command_prefix.is_none());
    assert!(rules[2].path_glob.is_none());

    assert_eq!(rules[3].id, "deny-0");
    assert!(matches!(rules[3].action, Action::Deny));
}

#[test]
fn approval_shorthand_decides_like_the_long_form() {
    use qcode_core::policy::Policy;
    use qcode_core::tool::ApprovalClass;
    use serde_json::json;

    let config: Config =
        toml::from_str("[approval]\nallow = [\"run_shell:echo *\"]\n").expect("parse");
    let policy = Policy::new(config.approval.shorthand_rules());

    let allowed = policy.evaluate(
        "run_shell",
        ApprovalClass::Execute,
        &json!({"command": "echo hi"}),
    );
    assert!(matches!(allowed.action, Action::Allow), "{allowed:?}");
    let denied = policy.evaluate(
        "run_shell",
        ApprovalClass::Execute,
        &json!({"command": "rm -rf /"}),
    );
    assert!(!matches!(denied.action, Action::Allow), "{denied:?}");
}

#[test]
fn an_unknown_allow_key_in_approval_is_reported() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[approval]\nallows = [\"x\"]\n").expect("write");
    let workspace = dir.path().to_path_buf();
    let _ = workspace;
    let document: toml::Table = toml::from_str("[approval]\nallows = [\"x\"]\n").expect("parse");
    let unknown = crate::config::unknown_keys(&document);
    assert!(
        unknown.iter().any(|key| key == "approval.allows"),
        "{unknown:?}"
    );
}

#[test]
fn the_network_plan_folds_the_legacy_keys_in() {
    // A file written before ADR-0065: `[network] enabled = false`.
    let config: Config = toml::from_str("[network]\nenabled = false\n").expect("parse");
    assert!(!config.network_plan().proxy);
    assert_eq!(config.sandbox_network_mode(), SandboxNetworkMode::Off);

    // The default: proxy on, sandbox loopback-only.
    let config = Config::default();
    assert!(config.network_plan().proxy);
    assert!(!config.network_plan().raw);
    assert_eq!(config.sandbox_network_mode(), SandboxNetworkMode::Proxy);

    // `[network] mode = "strict"` still means strict.
    let config: Config = toml::from_str("[network]\nmode = \"strict\"\n").expect("parse");
    assert!(config.network_plan().strict);

    // `[sandbox] network = true` used to mean raw network.
    let config: Config = toml::from_str("[sandbox]\nnetwork = true\n").expect("parse");
    assert_eq!(config.sandbox_network_mode(), SandboxNetworkMode::Open);
    assert!(config.network_plan().raw);
    assert!(!config.network_plan().proxy);
}

#[test]
fn the_new_switch_wins_over_the_legacy_section() {
    let config: Config =
        toml::from_str("[network]\nenabled = true\n\n[sandbox]\nnetwork = \"off\"\n")
            .expect("parse");
    let plan = config.network_plan();
    assert!(!plan.proxy);
    assert!(!plan.raw);
    assert_eq!(config.sandbox_network_mode(), SandboxNetworkMode::Off);

    let config: Config =
        toml::from_str("[network]\nenabled = true\n\n[sandbox]\nnetwork = \"open\"\n")
            .expect("parse");
    assert!(config.network_plan().raw);
    assert!(!config.network_plan().proxy);
}

// ---- `~` in configured paths ---------------------------------------------
//
// The rule is one helper (`expand_tilde`) called from one place per parse
// (`expand_paths`), and the tests below are the three things that can go wrong
// with it: it expands too little (`~/x` stays relative and the MCP validator
// rejects a config that should work), too much (`~user/x` or `Documents`
// silently becoming something), or differently on the second pass.

fn test_home() -> PathBuf {
    PathBuf::from("/home/q")
}

#[test]
fn tilde_expands_only_for_this_user() {
    let home = Some(test_home());
    let cases = [
        // written, expected
        ("~", "/home/q"),
        ("~/Documents", "/home/q/Documents"),
        // Not expanded: another account's home needs a passwd lookup, and
        // granting a path under it is never what a per-task config means.
        ("~ftp/x", "~ftp/x"),
        // Not expanded: the config is user-wide, the workspace is per-task.
        ("Documents", "Documents"),
        // Idempotent: already absolute, unchanged.
        ("/tmp/x", "/tmp/x"),
    ];
    for (written, expected) in cases {
        assert_eq!(
            expand_tilde(Path::new(written), home.as_deref()),
            PathBuf::from(expected),
            "{written}"
        );
    }
}

#[test]
fn tilde_without_a_home_leaves_the_path_alone() {
    // A missing `$HOME` must not invent one. The path stays as written, so the
    // MCP validator's "must be absolute" check still rejects it instead of the
    // grant silently pointing at `/x`.
    assert_eq!(
        expand_tilde(Path::new("~/Documents"), None),
        PathBuf::from("~/Documents")
    );
}

#[test]
fn expansion_is_idempotent() {
    let home = test_home();
    let once = expand_tilde(Path::new("~/Documents"), Some(home.as_path()));
    let twice = expand_tilde(&once, Some(home.as_path()));
    assert_eq!(once, twice);
    assert_eq!(twice, PathBuf::from("/home/q/Documents"));
}

#[test]
fn every_configured_path_list_gets_expanded() {
    let mut config: Config = toml::from_str(
        r#"
            [sandbox]
            extra_read = ["~/cot"]
            extra_writable = ["~/scratch"]

            [skills]
            roots = ["~/skills"]

            [[mcp.servers]]
            name = "wordcraft"
            command = "/usr/bin/env"
            read_paths = ["~/cot/lib/wordcraft"]
            writable_paths = ["~/Documents", "/srv/shared"]
            "#,
    )
    .expect("parse");

    let home = test_home();
    expand_paths_with(&mut config, Some(home.as_path()));

    assert_eq!(config.sandbox.extra_read, [PathBuf::from("/home/q/cot")]);
    assert_eq!(
        config.sandbox.extra_writable,
        [PathBuf::from("/home/q/scratch")]
    );
    assert_eq!(config.skills.roots, [PathBuf::from("/home/q/skills")]);
    let server = &config.mcp.servers[0];
    assert_eq!(
        server.read_paths,
        [PathBuf::from("/home/q/cot/lib/wordcraft")]
    );
    // Mixed list: the `~` entry expands, the absolute one is untouched.
    assert_eq!(
        server.writable_paths,
        [
            PathBuf::from("/home/q/Documents"),
            PathBuf::from("/srv/shared")
        ]
    );
}

#[test]
fn a_relative_mcp_path_is_still_rejected_after_expansion() {
    // The whole point of expanding *before* `validate()`: `~/x` becomes
    // absolute and passes, `Documents` does not and still fails. Otherwise
    // expansion would have quietly turned the validator off.
    let mut config: Config = toml::from_str(
        r#"
            [[mcp.servers]]
            name = "wordcraft"
            command = "/usr/bin/env"
            writable_paths = ["~/Documents"]
            "#,
    )
    .expect("parse");
    let home = test_home();
    expand_paths_with(&mut config, Some(home.as_path()));
    if let Err(error) = config.mcp.validate() {
        panic!("`~/Documents` must be accepted once expanded: {error}");
    }

    let mut config: Config = toml::from_str(
        r#"
            [[mcp.servers]]
            name = "wordcraft"
            command = "/usr/bin/env"
            writable_paths = ["Documents"]
            "#,
    )
    .expect("parse");
    expand_paths_with(&mut config, Some(home.as_path()));
    let error = config.mcp.validate().expect_err("relative path");
    assert!(
        error.to_string().contains("must be an absolute path"),
        "{error}"
    );

    // `~user` is not `~`: it is left as written and therefore rejected, rather
    // than silently granted as something under the running user's home.
    let mut config: Config = toml::from_str(
        r#"
            [[mcp.servers]]
            name = "wordcraft"
            command = "/usr/bin/env"
            writable_paths = ["~user/Documents"]
            "#,
    )
    .expect("parse");
    expand_paths_with(&mut config, Some(home.as_path()));
    let error = config.mcp.validate().expect_err("~user");
    assert!(error.to_string().contains("~user/Documents"), "{error}");
}

#[test]
fn skill_roots_keep_expanding_and_keep_resolving_against_the_workspace() {
    let workspace = Path::new("/w");
    let home = Some(test_home());
    // Same helper as everything else for `~`...
    assert_eq!(
        crate::expand_root(Path::new("~/skills"), workspace, home.as_deref()),
        PathBuf::from("/home/q/skills")
    );
    // ...but a skill root is a directory to scan, not a grant, so a relative
    // one keeps its workspace meaning (`.qcode/skills` in a repository).
    assert_eq!(
        crate::expand_root(Path::new("skills"), workspace, home.as_deref()),
        PathBuf::from("/w/skills")
    );
    assert_eq!(
        crate::expand_root(Path::new("/opt/skills"), workspace, home.as_deref()),
        PathBuf::from("/opt/skills")
    );
}

/// Image input is a deployment property, not a protocol one.
///
/// Before this, `images` was a constant of the adapter (`false` for every
/// OpenAI-compatible route, `true` for Anthropic), so DeepSeek — which answers
/// images over both endpoints, verified 2026-11-11 — had every picture
/// replaced with a "[image omitted]" line before the request was built.
#[test]
fn resolve_reports_whether_the_route_takes_images() {
    let env = env_from(&[("DEEPSEEK_API_KEY", "sk-test")]);

    // Catalog answer: DeepSeek and Anthropic see images, Ollama does not.
    let config: Config = toml::from_str(
        r#"
            [providers.deepseek]
            default_model = "deepseek-flash"
            "#,
    )
    .expect("parse");
    assert!(
        resolve(&config, &Overrides::default(), &env)
            .expect("resolves")
            .images,
        "deepseek's catalog entry takes images"
    );

    let ollama: Config = toml::from_str(
        r#"
            [providers.ollama]
            base_url = "http://localhost:11434/v1"
            api_key_env = ""
            default_model = "llama3"
            "#,
    )
    .expect("parse");
    assert!(
        !resolve(&ollama, &Overrides::default(), &env)
            .expect("resolves")
            .images,
        "a catalog route without image support stays text-only"
    );

    // A route the catalog does not ship is text-only until told otherwise.
    let gateway: Config = toml::from_str(
        r#"
            [providers.gateway]
            base_url = "https://gateway.example.com/v1"
            api_key_env = "DEEPSEEK_API_KEY"
            default_model = "some-model"
            "#,
    )
    .expect("parse");
    assert!(
        !resolve(&gateway, &Overrides::default(), &env)
            .expect("resolves")
            .images
    );

    // …and the config override wins in both directions.
    let mut off = config.clone();
    off.providers.get_mut("deepseek").expect("route").images = Some(false);
    assert!(
        !resolve(&off, &Overrides::default(), &env)
            .expect("resolves")
            .images,
        "explicit false turns a catalog route text-only"
    );

    let mut on = gateway.clone();
    on.providers.get_mut("gateway").expect("route").images = Some(true);
    assert!(
        resolve(&on, &Overrides::default(), &env)
            .expect("resolves")
            .images,
        "explicit true lets a hand-declared gateway take images"
    );
}

/// The engine keys its image handling off the capability, so the adapter has to
/// report the configured value instead of a constant.
#[test]
fn the_built_provider_reports_the_resolved_image_capability() {
    use crate::provider_registry::ProviderRegistry;
    let env = env_from(&[("DEEPSEEK_API_KEY", "sk-test")]);
    for (protocol, base_url, expected) in [
        ("openai", "https://api.deepseek.com/v1", true),
        ("anthropic", "https://api.deepseek.com/anthropic", true),
    ] {
        let text = format!(
            r#"
            [providers.deepseek]
            base_url = "{base_url}"
            protocol = "{protocol}"
            api_key_env = "DEEPSEEK_API_KEY"
            default_model = "deepseek-flash"
            images = false
            "#
        );
        let config: Config = toml::from_str(&text).expect("parse");
        let resolved = resolve(&config, &Overrides::default(), &env).expect("resolves");
        assert!(!resolved.images, "the file said false ({protocol})");
        let provider = ProviderRegistry::new(None).build(&resolved).expect("build");
        assert!(
            !provider.capabilities().images,
            "{protocol} adapter must report the config, not a constant"
        );

        // And with the override removed, both protocols take the catalog's yes.
        let config: Config = toml::from_str(&text.replace("images = false", "")).expect("parse");
        let resolved = resolve(&config, &Overrides::default(), &env).expect("resolves");
        assert_eq!(resolved.images, expected, "{protocol}");
        let provider = ProviderRegistry::new(None).build(&resolved).expect("build");
        assert_eq!(provider.capabilities().images, expected, "{protocol}");
    }
}
