use super::*;

pub(crate) async fn cmd_run(
    args: RunArgs,
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    approval_policy: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let RunArgs {
        prompt,
        skill,
        json,
        include_raw,
        provider,
        model,
        base_url,
        no_stream_usage,
        max_turns,
        image,
        yes,
        cwd,
        resume,
        max_cost,
        timeout,
    } = args;

    let prompt = match prompt {
        Some(prompt) => prompt,
        None => {
            let mut buffer = String::new();
            std::io::stdin().read_to_string(&mut buffer)?;
            buffer
        }
    };
    if prompt.trim().is_empty() {
        return Err("prompt is empty".into());
    }

    let workspace = match cwd {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let workspace = workspace.canonicalize()?;
    let kernel = qcode_kernel::assemble(
        config_path,
        &workspace,
        data_dir,
        approval_policy.as_deref(),
        config::Overrides {
            provider,
            model,
            base_url,
            stream_usage: no_stream_usage.then_some(false),
        },
    )
    .await?;
    let loaded = &kernel.config;
    // The flag wins, then the config, then the built-in default: a skill run
    // needs far more turns than a single question does.
    let max_turns = max_turns.unwrap_or(loaded.agent.max_turns);
    let max_cost = max_cost.or(loaded.agent.max_cost_usd);
    let resolved = &kernel.resolved;
    let pricing = config::pricing_for(loaded, &resolved.model, max_cost)?;

    let attachments = read_images(&image)?;

    let redactor = logging::Redactor::new(resolved.api_key.clone());
    tracing::info!(
        provider = %resolved.name,
        model = %resolved.model,
        base_url = ?resolved.base_url,
        "provider resolved"
    );

    let agents_md = agents::load(&workspace)?;
    if agents_md.is_some() {
        tracing::info!("loaded AGENTS.md from the workspace root");
    }
    let skills = kernel.skills.clone();
    let skills_prompt = skills.prompt_section();
    let prompt = match skill {
        Some(name) => activated_prompt(&skills, &name, prompt)?,
        None => prompt,
    };
    let cwd = workspace.display().to_string();
    let data_dir = kernel.store.root().to_path_buf();
    let policy = kernel.policy;
    let store = kernel.store;
    let mut tools = kernel.tools;
    let network = kernel.network;
    let provider: Arc<dyn Provider> = kernel.provider.into();

    let (session_id, initial_events) = match resume {
        Some(id) => {
            // Validate before the engine writes anything; a typo should not
            // create a new session file.
            store.load(&id)?;
            tracing::info!(session = %id, "session resumed");
            (id, Vec::new())
        }
        None => {
            let created = store.create_task(&SessionStarted {
                model: resolved.model.clone(),
                provider: resolved.name.clone(),
                cwd,
                version: env!("CARGO_PKG_VERSION").to_string(),
            })?;
            tracing::info!(session = %created.id, "session created");
            (created.id, vec![created.started, created.configured])
        }
    };
    if json {
        for event in &initial_events {
            print_event(event, include_raw);
        }
    }

    let approver: Arc<dyn Approver> = if yes {
        Arc::new(AutoApprove)
    } else if std::io::stdin().is_terminal() {
        Arc::new(InteractiveApprover)
    } else {
        Arc::new(AutoDeny)
    };

    // Pre-images for rollback: every write the agent makes records what was
    // there first.
    let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(&data_dir)
        .with_shell(loaded.agent.snapshot_shell);

    // The child registry excludes `subagent`, so nesting stops at depth one.
    let runner = subagent::build(
        &store,
        provider.clone(),
        tools.clone(),
        &policy,
        approver.clone(),
        &workspace,
        &resolved.model,
        Some(loaded.context.window_tokens),
        pricing,
        network.clone(),
        Some(snapshots.clone()),
        None,
    );
    tools.extend([Arc::new(SubagentTool::new(runner)) as Arc<dyn Tool>]);

    if timeout == Some(0) {
        return Err("--timeout must be at least 1 second".into());
    }

    let cancel = CancellationToken::new();
    install_ctrl_c(cancel.clone());
    let timed_out = Arc::new(AtomicBool::new(false));
    if let Some(seconds) = timeout {
        install_timeout(
            cancel.clone(),
            Duration::from_secs(seconds),
            timed_out.clone(),
        );
    }

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: provider.as_ref(),
            tools: &tools,
            policy: &policy,
            approver: approver.as_ref(),
            snapshots: Some(&snapshots),
        },
        RunOptions {
            session_id: &session_id,
            model: &resolved.model,
            input: prompt,
            cwd: workspace.clone(),
            max_turns,
            cancel: cancel.clone(),
            pricing,
            max_cost_usd: max_cost,
            on_delta: None,
            context_window: Some(loaded.context.window_tokens),
            agents_md,
            skills: skills_prompt,
            network: network.clone(),
            attachments,
            inbox: None,
        },
        |event| {
            if json {
                print_event(event, include_raw);
            } else {
                progress(event);
            }
        },
    )
    .await
    .map_err(|err| redactor.redact(&err.to_string()))?;

    if outcome.cancelled {
        if !json && !outcome.text.is_empty() {
            println!("{}", outcome.text);
        }
        if timed_out.load(Ordering::SeqCst) {
            eprintln!(
                "[timeout] turn exceeded {}s; cancelled",
                timeout.unwrap_or_default()
            );
            return Ok(ExitCode::from(124));
        }
        eprintln!("[cancelled] turn interrupted; the event log is intact");
        return Ok(ExitCode::from(130));
    }

    if !json {
        println!("session: {}", outcome.session_id);
        println!();
        println!("{}", outcome.text);
        if outcome.cost_usd > 0.0 {
            println!();
            println!("cost: ${:.6} (estimated)", outcome.cost_usd);
        }
    }

    // A stale, cached version check only ever prints: `qc run` must not fail
    // because GitHub is unreachable, and the JSONL stream stays on stdout.
    if loaded.update.checks_on_startup()
        && let Some(notice) = update::notice(&loaded.update, &data_dir).await
    {
        eprintln!("[update] {notice}");
    }

    Ok(ExitCode::SUCCESS)
}
