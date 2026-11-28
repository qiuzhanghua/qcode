use super::*;

#[cfg(windows)]
pub(crate) fn cmd_sandbox(args: SandboxArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let path = match &args.command {
        SandboxCommand::Grant { path } | SandboxCommand::Revoke { path } => {
            path.clone().unwrap_or_else(|| PathBuf::from("."))
        }
    };
    match args.command {
        SandboxCommand::Grant { .. } => {
            let root =
                qcode_tools::windows_acl::grant_path(&path).map_err(std::io::Error::other)?;
            println!(
                "{}",
                qcode_i18n::trp(
                    "cli.sandbox_grant.granted",
                    &[root.display().to_string().as_str()]
                )
            );
        }
        SandboxCommand::Revoke { .. } => {
            let (root, removed) =
                qcode_tools::windows_acl::revoke_path(&path).map_err(std::io::Error::other)?;
            println!(
                "{}",
                qcode_i18n::trp(
                    "cli.sandbox_grant.revoked",
                    &[
                        removed.to_string().as_str(),
                        root.display().to_string().as_str()
                    ]
                )
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn cmd_update(
    args: update::UpdateArgs,
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let data_dir = qcode_kernel::resolve_data_dir(data_dir);
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let loaded = qcode_kernel::load_config(config_path, &workspace)?;
    update::run(args, &loaded.update, &data_dir).await?;
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn cmd_acp(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    approval_policy: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let workspace = std::env::current_dir()?.canonicalize()?;
    let kernel = qcode_kernel::assemble(
        config_path,
        &workspace,
        data_dir,
        approval_policy.as_deref(),
        config::Overrides::default(),
    )
    .await?;
    let pricing = config::pricing_for(
        &kernel.config,
        &kernel.resolved.model,
        kernel.config.agent.max_cost_usd,
    )?;
    let agents_md = qcode_core::agents::load(&workspace)?;

    tracing::info!(
        provider = %kernel.resolved.name,
        model = %kernel.resolved.model,
        "acp server starting"
    );
    // 定时任务：ACP 能**登记**（任务定义是 store 里的纯数据），但编辑器里没有调度
    // 时钟——触发要 GUI 在跑。以前这里把四个工具整体剔除，等于连登记也不给
    // （ADR-0088 §1/§5）。
    let scheduler =
        std::sync::Arc::new(qcode_core::scheduler::Scheduler::new(kernel.store.clone()));
    let driver =
        qcode_core::scheduler::DriverStatus::read(&kernel.store, qcode_core::schedule::now_local());
    let deps = qcode_acp::Deps {
        store: kernel.store,
        provider: kernel.provider.into(),
        tools: kernel.tools,
        policy: kernel.policy,
        workspace,
        model: kernel.resolved.model.clone(),
        pricing,
        max_turns: kernel.config.agent.max_turns,
        snapshot_shell: kernel.config.agent.snapshot_shell,
        max_cost_usd: kernel.config.agent.max_cost_usd,
        context_window: Some(kernel.config.context.window_tokens),
        agents_md,
        skill_index: kernel.skills,
        network: kernel.network,
        scheduler: Some(scheduler),
        driver,
    };
    qcode_acp::stdio::run(Arc::new(deps)).await?;
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn cmd_doctor(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    probe: bool,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let healthy = doctor::run(data_dir, config_path, probe).await?;
    Ok(if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Built-in tools plus whatever the enabled MCP servers advertise and the
/// `skill` tool when skills were discovered. A server that fails to start only
/// removes its own tools; `connect_all` logs why.
pub(crate) fn cmd_skills(
    args: SkillsArgs,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match args.command {
        SkillsCommand::List => {}
    }

    let workspace = std::env::current_dir()?.canonicalize()?;
    let loaded = qcode_kernel::load_config(config_path, &workspace)?;
    if !loaded.skills.enabled {
        println!("{}", qcode_i18n::tr("cli.skills.disabled"));
        return Ok(ExitCode::SUCCESS);
    }

    // ADR-0108 §7: skills served over MCP cannot be listed here without
    // connecting, and this command must stay offline. Say how many servers
    // would contribute, and point at the command that does connect — otherwise
    // "I turned it on and the list is empty" has no explanation.
    let mcp_configured = loaded
        .mcp
        .servers
        .iter()
        .filter(|server| server.skills_allowed(loaded.skills.mcp))
        .count();
    let print_mcp_hint = || {
        if mcp_configured > 0 {
            println!(
                "\n{}",
                qcode_i18n::trp(
                    "cli.skills.mcp_hint",
                    &[mcp_configured.to_string().as_str()]
                )
            );
        }
    };

    let roots = qcode_kernel::skill_roots(&loaded.skills, &workspace);
    let skills = SkillSet::discover(&roots);
    // ADR-0110 §8: this lists what the model can use, so the skills turned off
    // in the settings list stay out — but the count is reported, or 「I turned
    // it off and now it is gone」 reads like discovery broke.
    let disabled = qcode_tools::skills::Disabled::from_raw(&loaded.skills.disabled);
    let disabled_count = skills.len() - skills.iter().filter(|s| disabled.allows(s)).count();

    if skills.is_empty() {
        println!("{}", qcode_i18n::tr("cli.skills.none_found"));
        for (source, path) in &roots {
            println!("  [{}] {}", source.label(), path.display());
        }
        print_mcp_hint();
        return Ok(ExitCode::SUCCESS);
    }

    for skill in skills.iter().filter(|skill| disabled.allows(skill)) {
        println!(
            "{}",
            qcode_i18n::trp(
                "cli.skills.skill_line",
                &[
                    skill.name.as_str(),
                    // `origin_label`, not `label`: `mcp:<server>` keeps a served
                    // skill from reading as a local one (ADR-0108 §3).
                    skill.source.origin_label().as_str(),
                    skill.files.len().to_string().as_str()
                ]
            )
        );
        println!("  {}", skill.dir.location());
        println!("  {}", skill.description.replace('\n', " "));
    }
    println!(
        "\n{}",
        qcode_i18n::trp(
            "cli.skills.count",
            &[skills
                .iter()
                .filter(|skill| disabled.allows(skill))
                .count()
                .to_string()
                .as_str()]
        )
    );
    if disabled_count > 0 {
        println!(
            "{}",
            qcode_i18n::trp(
                "cli.skills.disabled_count",
                &[disabled_count.to_string().as_str()]
            )
        );
    }
    print_mcp_hint();
    Ok(ExitCode::SUCCESS)
}

/// `qc shell-env`: what `run_shell` would hand to a command (ADR-0064).
///
/// Names by default: a sample is the user's whole environment, API keys
/// included, and a terminal is easy to screenshot or paste. Values need
/// `--full`, which refuses when stdout is not a tty.
pub(crate) fn cmd_shell_env(
    args: ShellEnvArgs,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let workspace = std::env::current_dir()?.canonicalize()?;
    let loaded = qcode_kernel::load_config(config_path, &workspace)?;
    let settings = loaded.shell.clone();

    if settings.env_sampling == qcode_tools::shell_env::EnvSampling::Off {
        println!("{}", qcode_i18n::tr("cli.shell_env.sampling_off"));
        println!("{}", qcode_i18n::tr("cli.shell_env.sampling_off_detail"));
        return Ok(ExitCode::SUCCESS);
    }

    let cache = qcode_tools::shell_env::ShellEnvCache::new(settings);
    let started = std::time::Instant::now();
    let sample = cache.current();
    let report = cache.last_report().unwrap_or_default();
    let Some(sample) = sample else {
        println!(
            "{}",
            qcode_i18n::trp(
                "cli.shell_env.sampling_failed",
                &[started.elapsed().as_millis().to_string().as_str()]
            )
        );
        if let Some(error) = report.error {
            println!("  {error}");
        }
        return Ok(ExitCode::FAILURE);
    };

    let full = args.full && std::io::stdout().is_terminal();
    if args.full && !full {
        eprintln!("{}", qcode_i18n::tr("cli.shell_env.full_needs_terminal"));
    }

    if args.json {
        let mut object = serde_json::Map::new();
        object.insert(
            "program".to_string(),
            serde_json::Value::String(sample.program().display().to_string()),
        );
        object.insert("count".to_string(), serde_json::Value::from(sample.len()));
        if full {
            object.insert("vars".to_string(), serde_json::to_value(sample.vars())?);
        } else {
            object.insert("names".to_string(), serde_json::to_value(sample.names())?);
        }
        println!("{}", serde_json::Value::Object(object));
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "{}",
        qcode_i18n::trp(
            "cli.shell_env.summary",
            &[
                sample.program().display().to_string().as_str(),
                sample.len().to_string().as_str(),
                report.duration_ms.to_string().as_str(),
                if report.cached {
                    qcode_i18n::tr("cli.shell_env.summary_cached").into_owned()
                } else {
                    String::new()
                }
                .as_str()
            ]
        )
    );
    if let Some(error) = &report.error {
        println!(
            "{}",
            qcode_i18n::trp("cli.shell_env.last_attempt_failed", &[error.as_str()])
        );
    }
    if !report.added.is_empty() {
        println!(
            "{}",
            qcode_i18n::trp(
                "cli.shell_env.new_since",
                &[report.added.join(", ").as_str()]
            )
        );
    }
    if !report.removed.is_empty() {
        println!(
            "{}",
            qcode_i18n::trp(
                "cli.shell_env.gone_since",
                &[report.removed.join(", ").as_str()]
            )
        );
    }
    println!();
    for name in sample.names() {
        match (full, sample.vars().get(&name)) {
            (true, Some(value)) => println!("{name}={value}"),
            _ => println!("{name}"),
        }
    }
    Ok(ExitCode::SUCCESS)
}
