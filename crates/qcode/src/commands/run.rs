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
        file,
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
            return Err(qcode_i18n::tr("err.cli.run.continue_needs_resume")
                .into_owned()
                .into());
        }
        if raise_turns.is_some() && max_turns.is_some()
            || raise_cost.is_some() && max_cost.is_some()
        {
            return Err(qcode_i18n::tr("err.cli.run.raise_duplicate_limit")
                .into_owned()
                .into());
        }
        if raise_persist && raise_turns.is_none() && raise_cost.is_none() {
            return Err(qcode_i18n::tr("err.cli.run.raise_persist_needs_either")
                .into_owned()
                .into());
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
        return Err(qcode_i18n::tr("err.cli.run.prompt_empty")
            .into_owned()
            .into());
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

    let mut attachments = read_images(&image)?
        .into_iter()
        .map(qcode_core::engine::AttachmentSource::Inline)
        .collect::<Vec<_>>();
    if !file.is_empty() {
        // Files are stored, never inlined: the model gets a read-only path
        // (ADR-0061 §7).
        let attachments_store = qcode_attachments::AttachmentStore::open(kernel.store.root())?;
        for path in &file {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    qcode_i18n::trp(
                        "err.cli.run.not_a_file_name",
                        &[path.display().to_string().as_str()],
                    )
                })?;
            let reader = std::fs::File::open(path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            let stored = attachments_store.put_file(name, reader)?;
            attachments.push(qcode_core::engine::AttachmentSource::Stored(
                qcode_attachments::AttachmentRef::File(stored),
            ));
        }
    }

    // `assemble` already installed the key for every log line; this is the handle
    // for the strings that bypass the logger — an error returned to `main`, which
    // prints it with `eprintln!` (ADR-0078).
    let redactor = qcode_kernel::redact::current();
    tracing::info!(
        provider = %resolved.name,
        model = %resolved.model,
        base_url = ?resolved.base_url,
        "provider resolved"
    );

    let agents_md = qcode_core::agents::load(&workspace)?;
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
                parent: None,
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

    // A question has no sensible auto-answer: `--yes` only auto-approves tool
    // permissions. Headless runs answer interactively on the terminal; piped
    // runs get `None`, so a blocking ask_user fails and a timed one expires.
    let questioner: Option<Arc<dyn Questioner>> = if !yes && std::io::stdin().is_terminal() {
        Some(Arc::new(InteractiveQuestioner))
    } else {
        None
    };

    // Pre-images for rollback: every write the agent makes records what was
    // there first.
    let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(&data_dir)
        .with_shell(loaded.agent.snapshot_shell);

    // The child registry excludes `subagent`, so nesting stops at depth one;
    // it also drops the schedule tools and `ask_user`.
    //
    // 这两条的理由在 ADR-0088 之后变了，值得写准：定时任务已经不是「只有 GUI
    // 支持」（登记三端都有），**剔除的理由换成了「子代理不该有会话之外的持久
    // 副作用」**——它是一次可丢弃的委派，留下的 cron 条目会在父会话早已结束后
    // 继续烧钱。`ask_user` 同理属于运行时根：子代理把没答上来的问题报给父模型，
    // 而不是自己弹窗。
    let runner = qcode_kernel::subagent::build(
        &store,
        provider.clone(),
        tools.without(
            &[
                qcode_tools::schedule::SCHEDULE_TOOL_NAMES.as_slice(),
                qcode_tools::ask::ASK_USER_TOOL_NAMES.as_slice(),
            ]
            .concat(),
        ),
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
        return Err(qcode_i18n::tr("err.cli.run.timeout_too_small")
            .into_owned()
            .into());
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

    // 会话标题自动生成（会话标题自动生成设计）：回合开始前就并行启动——模型
    // 调用与回合重叠，回合结束锁释放后立即落盘，标题不用等回合跑完再生成。
    // 首条消息尚未写入日志，所以把本次输入作为 pending 来源传给生成器。
    let title_task = if let Some(title_config) = loaded.session_title.to_title_config()? {
        let title_cancel = CancellationToken::new();
        let gen_provider = provider.clone();
        let gen_store = store.clone();
        let gen_session = session_id.clone();
        let gen_prompt = prompt.clone();
        Some(tokio::spawn(async move {
            qcode_core::title::generate_title(
                gen_provider.as_ref(),
                &gen_store,
                &gen_session,
                &title_config,
                &title_cancel,
                Some(&gen_prompt),
            )
            .await
        }))
    } else {
        None
    };

    // 定时任务：`qc run` 能**登记**（任务定义是 store 里的纯数据），但不会**触发**
    // ——驱动（时钟 + 会话宿主）不在这个进程里（ADR-0088 §1）。工具结果会说明这条，
    // 所以这里注入的是「能登记 + 驱动状态」而不是干脆不给。
    let scheduler = std::sync::Arc::new(qcode_core::scheduler::Scheduler::new(store.clone()));
    let driver =
        qcode_core::scheduler::DriverStatus::read(&store, qcode_core::schedule::now_local());

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: provider.as_ref(),
            tools: &tools,
            policy: &policy,
            approver: approver.as_ref(),
            snapshots: Some(&snapshots),
            scheduler: Some(scheduler),
            // `qc run` 本身不驱动定时任务；心跳里有没有人，读出来才知道。
            driver,
            questioner,
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
            scheduled: None,
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

    // 标题生成与回合并行启动，回合结束锁已释放；这里等它落盘（生成器在
    // 回合内已完成模型调用，落盘等待只会是一瞬），进程退出前必须拿到结果。
    if let Some(task) = title_task {
        match task.await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::debug!(%error, session = %session_id, "session title generation failed")
            }
            Err(error) => tracing::debug!(%error, "session title generation task panicked"),
        }
    }

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
            println!(
                "{}",
                qcode_i18n::trp("cli.run.paused", &[pause_line(pause).as_str()])
            );
            if let Some(summary) = &pause.summary {
                println!("{summary}");
            }
            println!();
            println!("{}", qcode_i18n::tr("cli.run.continue_hint"));
        }
        if loaded.update.checks_on_startup()
            && let Some(notice) = update::notice(&loaded.update, &data_dir).await
        {
            eprintln!("[update] {notice}");
        }
        return Ok(exit_code(EXIT_PAUSED));
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
            return Ok(exit_code(124));
        }
        eprintln!("[cancelled] turn interrupted; the event log is intact");
        return Ok(exit_code(130));
    }

    if !json {
        println!(
            "{}",
            qcode_i18n::trp("cli.run.session", &[outcome.session_id.as_str()])
        );
        println!();
        println!("{}", outcome.text);
        if outcome.cost_usd > 0.0 {
            println!();
            println!(
                "{}",
                qcode_i18n::trp(
                    "cli.run.cost",
                    &[format!("{:.6}", outcome.cost_usd).as_str()]
                )
            );
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
