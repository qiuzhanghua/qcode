use super::*;

/// `EX_TEMPFAIL`: the turn stopped on a budget, the session is intact, and
/// running the same command again continues it (ADR-0060 §2.8).
const EXIT_PAUSED: u8 = 75;

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
        continue_,
        raise_turns,
        raise_cost,
        raise_persist,
    } = args;

    // `--continue` (and the two `--raise-*` shorthands) continue a paused turn:
    // the task is already in the log, so a prompt is optional (ADR-0060 §2.8).
    let continuation = continue_ || raise_turns.is_some() || raise_cost.is_some() || raise_persist;
    if continuation {
        if resume.is_none() {
            return Err("--continue needs --resume <session> to know which turn to resume".into());
        }
        if raise_turns.is_some() && max_turns.is_some()
            || raise_cost.is_some() && max_cost.is_some()
        {
            return Err(
                "--raise-* already sets the limit for this window; drop the duplicate flag".into(),
            );
        }
        if raise_persist && raise_turns.is_none() && raise_cost.is_none() {
            return Err("--raise-persist needs --raise-turns or --raise-cost".into());
        }
    }

    let prompt = match prompt {
        Some(prompt) => prompt,
        None if continuation => String::new(),
        None => {
            let mut buffer = String::new();
            std::io::stdin().read_to_string(&mut buffer)?;
            buffer
        }
    };
    if prompt.trim().is_empty() && !continuation {
        return Err("prompt is empty".into());
    }

    let workspace = match cwd {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let workspace = workspace.canonicalize()?;
    // Kept for `--raise-persist`, which writes back into the same file the
    // kernel read.
    let config_target = config_path.clone();
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
    // A continuation window may raise either limit; the rest of the session
    // keeps the configured value.
    let max_turns = raise_turns.or(max_turns).unwrap_or(loaded.agent.max_turns);
    let max_cost = raise_cost.or(max_cost).or(loaded.agent.max_cost_usd);
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
        None if continuation && prompt.trim().is_empty() => {
            qcode_core::engine::CONTINUE_PROMPT.to_string()
        }
        None => prompt,
    };
    // `--raise-persist` writes the new limit into config.toml, so the next
    // session starts with it. A failed write is a warning, not a reason to
    // abandon a turn the user already asked for.
    if raise_persist {
        // Write back to the file `qc config edit` would touch — `--config`, then
        // `$QCODE_CONFIG`, then the user config — so a raised limit lands where
        // the next run reads it.
        let target = qcode_kernel::config::edit_path(config_target)?;
        match qcode_kernel::config_edit::ConfigEdit::open(target) {
            Ok(mut edit) => {
                if let Some(turns) = raise_turns {
                    edit.set_integer(&["agent", "max_turns"], i64::from(turns))?;
                }
                if let Some(usd) = raise_cost {
                    edit.set_float(&["agent", "max_cost_usd"], usd)?;
                }
                edit.save()?;
            }
            Err(error) => eprintln!("[config] cannot persist the raised limit: {error}"),
        }
    }
    let continuation = continuation.then(|| qcode_core::event::Continuation {
        // Filled in by the engine from the log: a stale value here cannot
        // forge a fresh budget.
        turn: 0,
        window: 1,
        raise: (raise_turns.is_some() || raise_cost.is_some()).then_some(
            qcode_core::event::RaiseLimit {
                turns: raise_turns,
                usd: raise_cost,
                persist: raise_persist,
            },
        ),
    });
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
            continuation,
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

    // A budget pause is a normal ending, not a failure: exit code 75
    // (EX_TEMPFAIL) tells a script "not done, run me again" without looking
    // like a crash (ADR-0060 §2.8).
    if let Some(pause) = &outcome.pause {
        if json {
            // The events carry the pause too, but a script that reads the
            // ndjson stream stops at the first `turn_ended`; this line is the
            // one summary of the whole hand-off (ADR-0060 §2.8).
            print_pause(&outcome.session_id, pause);
        }
        if !json && !outcome.text.is_empty() {
            println!("{}", outcome.text);
        }
        if !json {
            println!();
            println!("paused: {}", pause_line(pause));
            if let Some(summary) = &pause.summary {
                println!("{summary}");
            }
            println!();
            println!(
                "continue: `qc run --continue`, or send another message; \
                 raise [agent] max_turns / --max-cost to give it more room"
            );
        }
        if loaded.update.checks_on_startup()
            && let Some(notice) = update::notice(&loaded.update, &data_dir).await
        {
            eprintln!("[update] {notice}");
        }
        return Ok(ExitCode::from(EXIT_PAUSED));
    }

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
