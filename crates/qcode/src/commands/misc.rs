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
            println!("granted windows-acl write access to {}", root.display());
        }
        SandboxCommand::Revoke { .. } => {
            let (root, removed) =
                qcode_tools::windows_acl::revoke_path(&path).map_err(std::io::Error::other)?;
            println!("revoked {removed} ACE(s) under {}", root.display());
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
    let agents_md = agents::load(&workspace)?;
    let skills_prompt = kernel.skills.prompt_section();

    tracing::info!(
        provider = %kernel.resolved.name,
        model = %kernel.resolved.model,
        "acp server starting"
    );
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
        skills: skills_prompt,
        network: kernel.network,
    };
    qcode_acp::stdio::run(Arc::new(deps)).await?;
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn cmd_doctor(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let healthy = doctor::run(data_dir, config_path).await?;
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
        println!("skills are disabled ([skills] enabled = false)");
        return Ok(ExitCode::SUCCESS);
    }

    let roots = qcode_kernel::skill_roots(&loaded.skills, &workspace);
    let skills = SkillSet::discover(&roots);

    if skills.is_empty() {
        println!("no skills discovered; checked:");
        for (source, path) in &roots {
            println!("  [{}] {}", source.label(), path.display());
        }
        return Ok(ExitCode::SUCCESS);
    }

    for skill in skills.iter() {
        println!(
            "{}  [{}]  files={}",
            skill.name,
            skill.source.label(),
            skill.files.len()
        );
        println!("  {}", skill.dir.display());
        println!("  {}", skill.description.replace('\n', " "));
    }
    println!("\n{} skill(s)", skills.len());
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
        println!("environment sampling is off ([shell] env_sampling = \"off\")");
        println!("run_shell only sees the allow list plus the egress proxy variables");
        return Ok(ExitCode::SUCCESS);
    }

    let cache = qcode_tools::shell_env::ShellEnvCache::new(settings);
    let started = std::time::Instant::now();
    let sample = cache.current();
    let report = cache.last_report().unwrap_or_default();
    let Some(sample) = sample else {
        println!("sampling failed after {}ms", started.elapsed().as_millis());
        if let Some(error) = report.error {
            println!("  {error}");
        }
        return Ok(ExitCode::FAILURE);
    };

    let full = args.full && std::io::stdout().is_terminal();
    if args.full && !full {
        eprintln!("--full needs a terminal; printing names only");
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
        "shell: {}   variables: {}   sampled in {}ms{}",
        sample.program().display(),
        sample.len(),
        report.duration_ms,
        if report.cached { "  (cached)" } else { "" }
    );
    if let Some(error) = &report.error {
        println!("  last attempt failed: {error}");
    }
    if !report.added.is_empty() {
        println!(
            "  new since the previous sample: {}",
            report.added.join(", ")
        );
    }
    if !report.removed.is_empty() {
        println!(
            "  gone since the previous sample: {}",
            report.removed.join(", ")
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
