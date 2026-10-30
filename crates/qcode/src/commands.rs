use crate::cli::*;
use crate::output::*;
#[cfg(windows)]
use crate::sandbox_probe;
use crate::{agents, doctor, logging, subagent, tui, update};
use std::io::IsTerminal;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use qcode_core::approval::{Approver, AutoApprove, AutoDeny, InteractiveApprover};
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::SessionStarted;
use qcode_core::provider::Provider;
use qcode_core::store::SessionStore;
use qcode_core::tool::Tool;
use qcode_kernel::config;
use qcode_tools::skills::SkillSet;
use qcode_tools::subagent::SubagentTool;
use qcode_tui::SkillView;
use tokio_util::sync::CancellationToken;

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

/// Build the closure that turns the current config into a fresh backend.
///
/// The shell calls it when the config file changes, so saving Settings swaps the
/// engine in place instead of re-execing the process (no window flash).
#[cfg(feature = "gui")]
pub(crate) fn gui_rebuild_hook(
    config_path: PathBuf,
    workspace: PathBuf,
    data_dir: PathBuf,
    approval_policy: Option<PathBuf>,
) -> qcode_gui::RebuildFn {
    std::sync::Arc::new(move || {
        let config_path = config_path.clone();
        let workspace = workspace.clone();
        let data_dir = data_dir.clone();
        let approval_policy = approval_policy.clone();
        Box::pin(async move {
            // No nested runtime here: the caller awaits this on the UI executor.
            let kernel = qcode_kernel::assemble(
                Some(config_path),
                &workspace,
                Some(data_dir),
                approval_policy.as_deref(),
                config::Overrides::default(),
            )
            .await
            .map_err(|error| error.to_string())?;
            let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(kernel.store.root())
                .with_shell(kernel.config.agent.snapshot_shell);
            let engine = qcode_gui::Engine {
                store: kernel.store,
                provider: kernel.provider.into(),
                provider_name: kernel.resolved.name.clone(),
                tools: kernel.tools,
                policy: kernel.policy,
                workspace: workspace.clone(),
                model: kernel.resolved.model.clone(),
                pricing: config::pricing_for(&kernel.config, &kernel.resolved.model, None)
                    .ok()
                    .flatten(),
                pricing_table: Default::default(),
                max_turns: kernel.config.agent.max_turns,
                max_cost_usd: kernel.config.agent.max_cost_usd,
                context_window: Some(kernel.config.context.window_tokens),
                network: kernel.network,
                snapshots: Some(snapshots),
            };
            Ok(Box::new(qcode_gui::LocalBackend::new(engine)) as Box<dyn qcode_gui::Backend>)
        })
    })
}

/// Open the desktop window (ADR-0037).
///
/// The window itself lives in the `qcode-gui` binary for now; this subcommand
/// assembles the kernel first so the GUI can be handed the same pieces as the
/// other frontends.
/// The lean build keeps the subcommand so the error explains what to download
/// instead of letting clap call it an unknown command.
#[cfg(not(feature = "gui"))]
pub(crate) async fn cmd_gui(
    _data_dir: Option<PathBuf>,
    _config_path: Option<PathBuf>,
    _approval_policy: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    eprintln!(
        "this build has no GUI support; rebuild with `cargo build -p qcode --features gui`, 
        or download the desktop build (qcode-desktop)"
    );
    Ok(ExitCode::from(2))
}

#[cfg(feature = "gui")]
pub(crate) async fn cmd_gui(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    approval_policy: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let workspace = std::env::current_dir()?.canonicalize()?;
    // Workspace switching re-execs qc with current_dir set to the new
    // workspace; relative --config/--data-dir values must be anchored first.
    let config_path = config_path.map(|path| {
        if path.is_relative() {
            workspace.join(path)
        } else {
            path
        }
    });
    let data_dir = data_dir.map(|path| {
        if path.is_relative() {
            workspace.join(path)
        } else {
            path
        }
    });
    let approval_policy = approval_policy.map(|path| {
        if path.is_relative() {
            workspace.join(path)
        } else {
            path
        }
    });
    let found = config::discover(config_path.clone())?;
    let setup_path = found.path.clone();
    eprintln!(
        "gui: config={} (source: {})",
        setup_path.display(),
        found.source.label()
    );
    if let Some(dir) = setup_path.parent()
        && !config::dir_accepts_new_files(dir)
    {
        eprintln!(
            " WARN config directory {} denies new files: {}",
            dir.display(),
            config::CONFIG_DIR_HINT
        );
    }
    // A missing file is the clearest signal: `load` treats it as an empty
    // document, and `resolve` can still succeed when the shell provides the key
    // through env vars, which would silently skip the unconfigured path.
    eprintln!("gui: config={}", setup_path.display());
    let exists = setup_path.exists();
    let resolved = config::load(&setup_path).ok().and_then(|loaded| {
        config::resolve(&loaded, &config::Overrides::default(), &|key| {
            std::env::var(key).ok()
        })
        .ok()
    });
    match &resolved {
        Some(resolved) => eprintln!(
            "gui: config exists={exists} provider={} model={} key={}",
            resolved.name, resolved.model, resolved.api_key_source
        ),
        None => eprintln!("gui: config exists={exists}, but it does not resolve"),
    }
    // A missing file is the clearest signal. A file that resolves but has no
    // provider entry still counts as configured when the shell provides the
    // values (env keys), which is a supported setup.
    let needs_setup = resolved.is_none();
    if needs_setup {
        eprintln!("gui: no usable config, opening the window without a kernel");
        // No config yet: open the window anyway. Settings stays reachable from
        // the sidebar, and the backend refuses prompts with an explanation
        // instead of the old behaviour of trapping the user in the wizard.
        let setup_data_dir = qcode_kernel::resolve_data_dir(data_dir.clone());
        let store = qcode_core::store::SessionStore::new(&setup_data_dir);
        let rebuild = gui_rebuild_hook(
            setup_path.clone(),
            workspace.clone(),
            setup_data_dir.clone(),
            approval_policy.clone(),
        );
        let backend =
            qcode_gui::UnconfiguredBackend::new(workspace.clone(), store).with_rebuild(rebuild);
        let mut app = qcode_tui::App::new("unconfigured", "-");
        app.push_notice(
            "not configured yet: open Settings, fill in the provider and key, then restart qc gui",
        );
        qcode_gui::run(app, Box::new(backend), setup_path, setup_data_dir);
        return Ok(ExitCode::SUCCESS);
    }
    let gui_data_dir = qcode_kernel::resolve_data_dir(data_dir.clone());
    let kernel = qcode_kernel::assemble(
        config_path,
        &workspace,
        data_dir,
        approval_policy.as_deref(),
        config::Overrides::default(),
    )
    .await?;
    println!(
        "gui: provider={} model={} sandbox={} tools={} skills={}",
        kernel.resolved.name,
        kernel.resolved.model,
        kernel.sandbox,
        kernel.tools.len(),
        kernel.skills.len()
    );
    // Seed the projection with what the kernel resolved, then hand the window
    // the same App the TUI would render.
    let mut app = qcode_tui::App::new(kernel.resolved.name.clone(), kernel.resolved.model.clone());
    app.sandbox = kernel.sandbox.clone();
    app.context_window = Some(kernel.config.context.window_tokens);
    app.push_notice(format!(
        "kernel: sandbox={} tools={} skills={}",
        kernel.sandbox,
        kernel.tools.len(),
        kernel.skills.len()
    ));
    let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(kernel.store.root())
        .with_shell(kernel.config.agent.snapshot_shell);
    let engine = qcode_gui::Engine {
        store: kernel.store,
        provider: kernel.provider.into(),
        provider_name: kernel.resolved.name.clone(),
        tools: kernel.tools,
        policy: kernel.policy,
        workspace: workspace.clone(),
        model: kernel.resolved.model.clone(),
        pricing: config::pricing_for(
            &kernel.config,
            &kernel.resolved.model,
            kernel.config.agent.max_cost_usd,
        )?,
        pricing_table: kernel.config.pricing.clone(),
        max_turns: kernel.config.agent.max_turns,
        max_cost_usd: kernel.config.agent.max_cost_usd,
        context_window: Some(kernel.config.context.window_tokens),
        network: kernel.network,
        snapshots: Some(snapshots),
    };
    let backend = qcode_gui::LocalBackend::new(engine).with_rebuild(gui_rebuild_hook(
        setup_path.clone(),
        workspace.clone(),
        gui_data_dir.clone(),
        approval_policy.clone(),
    ));
    qcode_gui::run(app, Box::new(backend), setup_path, gui_data_dir);
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn cmd_tui(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    approval_policy: Option<PathBuf>,
    resume: Option<String>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("TUI needs a terminal; use `qc run \"...\"` for headless mode".into());
    }

    let workspace = std::env::current_dir()?.canonicalize()?;
    let kernel = qcode_kernel::assemble(
        config_path,
        &workspace,
        data_dir,
        approval_policy.as_deref(),
        config::Overrides::default(),
    )
    .await?;
    let loaded = &kernel.config;
    let resolved = &kernel.resolved;
    let pricing = config::pricing_for(loaded, &resolved.model, None)?;

    let redactor = logging::Redactor::new(resolved.api_key.clone());
    tracing::info!(
        provider = %resolved.name,
        model = %resolved.model,
        "starting TUI"
    );

    let agents_md = agents::load(&workspace)?;
    let skills = kernel.skills.clone();
    let skills_prompt = skills.prompt_section();
    let skill_views = skill_views(&skills);
    let policy = kernel.policy;
    let store = kernel.store;
    let tools = kernel.tools;
    let network = kernel.network;
    let provider: Arc<dyn Provider> = kernel.provider.into();

    tui::run(tui::TuiOptions {
        store: &store,
        provider,
        tools: &tools,
        policy: &policy,
        workspace,
        provider_name: &resolved.name,
        model: &resolved.model,
        pricing,
        max_cost_usd: loaded.agent.max_cost_usd,
        max_turns: loaded.agent.max_turns,
        snapshot_shell: loaded.agent.snapshot_shell,
        context_window: Some(loaded.context.window_tokens),
        agents_md,
        skills_prompt,
        skills: skill_views,
        sandbox: kernel.sandbox.clone(),
        resume,
        network,
        busy_delivery: loaded.ui.busy_delivery,
    })
    .await
    .map_err(|err| redactor.redact(&err.to_string()))?;

    Ok(ExitCode::SUCCESS)
}

pub(crate) fn cmd_sessions(
    args: SessionsArgs,
    data_dir: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let store = SessionStore::new(qcode_kernel::resolve_data_dir(data_dir));

    match args.command {
        SessionsCommand::List => {
            let sessions = store.list()?;
            if sessions.is_empty() {
                println!("no sessions");
                return Ok(ExitCode::SUCCESS);
            }
            for session in sessions {
                println!(
                    "{}  turns={}  cost=${:.4}  model={}  cwd={}",
                    session.id,
                    session.turns,
                    session.cost_usd,
                    session.started.model,
                    session.started.cwd
                );
                // Roots decide what a task can touch, and a missing one is the
                // difference between "works" and "every file tool fails".
                match store.task(&session.id) {
                    Ok(task) => {
                        for root in &task.roots {
                            let access = match root.access {
                                qcode_core::task::RootAccess::ReadOnly => "ro",
                                qcode_core::task::RootAccess::ReadWrite => "rw",
                            };
                            let state = match root.state() {
                                qcode_core::task::RootState::Ready => "",
                                qcode_core::task::RootState::Missing => "  MISSING",
                            };
                            println!(
                                "    @{} {} {}{}",
                                root.label,
                                access,
                                root.path.display(),
                                state
                            );
                        }
                        if task.roots.is_empty() {
                            println!("    (no roots)");
                        }
                    }
                    Err(_) => println!("    (no task record)"),
                }
            }
        }
        SessionsCommand::Show { id } => {
            for event in store.load(&id)? {
                println!("{}", serde_json::to_string_pretty(&event)?);
            }
        }
        SessionsCommand::Fork { id, at } => {
            let forked = store.fork(&id, at)?;
            println!("{forked}");
        }
        SessionsCommand::Gc {
            older_than_days,
            yes,
        } => {
            let ids = store.gc(older_than_days, yes)?;
            if ids.is_empty() {
                println!("nothing to clean");
            }
            let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(store.root());
            let mut freed = 0u64;
            for id in &ids {
                let size = snapshots.size(id).unwrap_or(0);
                freed += size;
                println!(
                    "{} {id} (snapshots: {:.1} MB)",
                    if yes { "removed" } else { "would remove" },
                    size as f64 / (1024.0 * 1024.0)
                );
            }
            if yes {
                for id in &ids {
                    if let Err(error) = snapshots.remove_session(id) {
                        println!("cannot remove snapshots for {id}: {error}");
                    }
                }
                // Snapshot trees whose session is gone (deleted by hand, or by an
                // older qc that did not clean up) are orphans.
                let keep: Vec<String> = store.list()?.into_iter().map(|s| s.id).collect();
                match snapshots.sweep(&keep) {
                    Ok(removed) if !removed.is_empty() => {
                        println!("removed {} orphan snapshot tree(s)", removed.len());
                    }
                    Ok(_) => {}
                    Err(error) => println!("cannot sweep snapshots: {error}"),
                }
                println!("freed {:.1} MB", freed as f64 / (1024.0 * 1024.0));
            } else {
                println!("dry run; pass --yes to delete");
            }
        }
        SessionsCommand::Checkpoints { id } => {
            let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(store.root());
            let turns = snapshots.turns(&id)?;
            if turns.is_empty() {
                println!("no checkpoints for {id}");
                return Ok(ExitCode::SUCCESS);
            }
            for turn in turns {
                let files = snapshots.files(&id, turn)?;
                println!("turn {turn}: {} file(s)", files.len());
                for file in files {
                    println!("  {file}");
                }
            }
        }
        SessionsCommand::Rollback { id, to, yes } => {
            // The workspace being restored is the one the session recorded, not
            // the current directory: rolling back from another repo would
            // otherwise write files into the wrong tree.
            let session = store
                .list()?
                .into_iter()
                .find(|session| session.id == id)
                .ok_or_else(|| format!("no such session: {id}"))?;
            let workspace = PathBuf::from(&session.started.cwd);
            let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(store.root());
            let turns = snapshots.turns(&id)?;
            let affected: Vec<u32> = turns.into_iter().filter(|turn| *turn >= to).collect();
            if affected.is_empty() {
                println!("nothing recorded at or after turn {to}; nothing to do");
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "would restore {} turn(s) into {}",
                affected.len(),
                workspace.display()
            );
            if !yes {
                println!("dry run; pass --yes to restore");
                return Ok(ExitCode::SUCCESS);
            }
            let report = snapshots.restore(&id, to, &workspace)?;
            println!(
                "restored {} file(s), removed {} created file(s) (turns {:?})",
                report.restored, report.deleted, report.turns
            );
            for refused in &report.refused {
                println!("skipped {refused}");
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// Store an API key in the platform key store and tell the user what to paste.
///
/// The key arrives on stdin so it never shows up in the process list, and the
/// printed snippet uses `api_key_command` (ADR-0039) instead of a plaintext
/// environment variable.
pub(crate) fn store_key(
    provider: &str,
    service: &str,
    account: Option<&str>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    use std::io::Read;

    let account = account.unwrap_or(provider);
    let mut key = String::new();
    std::io::stdin().read_to_string(&mut key)?;
    let key = key.trim();
    if key.is_empty() {
        return Err("no key on stdin; try: echo <key> | qc config key set".into());
    }

    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-a",
                account,
                "-s",
                service,
                "-w",
                key,
            ])
            .status()?;
        if !status.success() {
            return Err("`security add-generic-password` failed".into());
        }
        println!(
            "stored in the keychain; add to your config:\n\
             api_key_command = [\"security\", \"find-generic-password\", \"-w\", \"-s\", \"{service}\", \"-a\", \"{account}\"]"
        );
    }

    #[cfg(target_os = "linux")]
    {
        use std::io::Write as _;
        let mut child = std::process::Command::new("secret-tool")
            .args([
                "store", "--label", service, "service", service, "user", account,
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(key.as_bytes())?;
        if !child.wait()?.success() {
            return Err("`secret-tool store` failed".into());
        }
        println!(
            "stored in libsecret; add to your config:\n\
             api_key_command = [\"secret-tool\", \"lookup\", \"service\", \"{service}\", \"user\", \"{account}\"]"
        );
    }

    #[cfg(target_os = "windows")]
    {
        // DPAPI keeps it user-scoped and encrypted; HKCU would be plaintext.
        let data_dir = qcode_kernel::resolve_data_dir(None);
        std::fs::create_dir_all(&data_dir)?;
        let path = data_dir.join(format!("{service}-{account}.cred"));
        let script = format!(
            "$s = [Console]::In.ReadLine(); \
             $s | ConvertTo-SecureString -AsPlainText -Force | ConvertFrom-SecureString | Set-Content -NoNewline '{}'",
            path.display()
        );
        use std::io::Write as _;
        // `pwsh` is what most machines running qc will have on PATH; powershell
        // is the fallback for a clean Windows install.
        let shell = if std::process::Command::new("pwsh")
            .arg("-NoProfile")
            .arg("-Command")
            .arg("exit 0")
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
        {
            "pwsh"
        } else {
            "powershell"
        };
        let mut child = std::process::Command::new(shell)
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(format!("{key}\n").as_bytes())?;
        if !child.wait()?.success() {
            return Err("storing the key with DPAPI failed".into());
        }
        println!(
            "stored (DPAPI, user-scoped) in {}; add to your config:\n\
             api_key_command = [\"{shell}\", \"-NoProfile\", \"-Command\", \"$s = Get-Content '{}' | ConvertTo-SecureString; [Runtime.InteropServices.Marshal]::PtrToStringAuto([Runtime.InteropServices.Marshal]::SecureStringToBSTR($s))\"]",
            path.display(),
            path.display()
        );
    }

    let _ = provider;
    Ok(ExitCode::SUCCESS)
}

pub(crate) fn cmd_config(
    args: ConfigArgs,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match args.command {
        ConfigCommand::Show => {
            let workspace = std::env::current_dir()?.canonicalize()?;
            let loaded = qcode_kernel::load_config(config_path, &workspace)?;
            print_config(&loaded);
        }
        ConfigCommand::Key { command } => match command {
            KeyCommand::Set {
                provider,
                service,
                account,
            } => return store_key(&provider, &service, account.as_deref()),
        },
        ConfigCommand::Edit => {
            let path = config::edit_path(config_path)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if !path.exists() {
                std::fs::write(&path, "# qcode config\nversion = 1\n")?;
            }
            let editor = std::env::var("EDITOR")
                .or_else(|_| std::env::var("VISUAL"))
                .unwrap_or_else(|_| {
                    if cfg!(windows) {
                        "notepad".to_string()
                    } else {
                        "vi".to_string()
                    }
                });
            let mut parts = editor.split_whitespace();
            let program = parts.next().unwrap_or("vi");
            let status = std::process::Command::new(program)
                .args(parts)
                .arg(&path)
                .status()?;
            if !status.success() {
                return Err(format!("editor exited with {status}").into());
            }
            match config::load(&path) {
                Ok(_) => println!("config ok: {}", path.display()),
                Err(err) => eprintln!("warning: {err}"),
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) fn cmd_mcp(
    args: McpArgs,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let workspace = std::env::current_dir()?.canonicalize()?;
    match args.command {
        McpCommand::List => {
            let loaded = qcode_kernel::load_config(config_path, &workspace)?;
            print_mcp(&loaded.mcp);
        }
        McpCommand::Add {
            name,
            command,
            args,
            disabled,
            timeout_secs,
        } => {
            let path = config::edit_path(config_path)?;
            config::mcp_add(&path, &name, &command, &args, !disabled, timeout_secs)?;
            println!("added MCP server '{name}' to {}", path.display());
        }
        McpCommand::Remove { name } => {
            let path = config::edit_path(config_path)?;
            config::mcp_remove(&path, &name)?;
            println!("removed MCP server '{name}' from {}", path.display());
        }
    }
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

/// Read `--image` files into base64 attachments.
///
/// Validating here keeps the engine's error path about capabilities only; a
/// missing or oversized file is a user error, not a model error.
pub(crate) fn read_images(
    paths: &[PathBuf],
) -> Result<Vec<qcode_core::engine::Attachment>, Box<dyn std::error::Error>> {
    const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
    let mut attachments = Vec::new();
    for path in paths {
        let metadata = std::fs::metadata(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if metadata.len() > MAX_IMAGE_BYTES {
            return Err(format!("{} is larger than 8 MB", path.display()).into());
        }
        let mime = match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("webp") => "image/webp",
            Some("gif") => "image/gif",
            _ => {
                return Err(format!(
                    "{}: only PNG, JPEG, WebP and GIF are supported",
                    path.display()
                )
                .into());
            }
        };
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        use base64::Engine as _;
        let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        attachments.push(qcode_core::engine::Attachment {
            mime_type: mime.to_string(),
            data_base64,
        });
    }
    Ok(attachments)
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

pub(crate) fn activated_prompt(
    skills: &SkillSet,
    name: &str,
    task: String,
) -> Result<String, Box<dyn std::error::Error>> {
    let Some(skill) = skills.get(name) else {
        let available: Vec<&str> = skills
            .iter()
            .take(20)
            .map(|skill| skill.name.as_str())
            .collect();
        return Err(format!(
            "unknown skill '{name}'; available: {}",
            available.join(", ")
        )
        .into());
    };
    let task = if task.trim().is_empty() {
        "Follow the skill instructions above now.".to_string()
    } else {
        task
    };
    Ok(format!(
        "[skill:{}]\n{}\n\n{task}",
        skill.name,
        skill.body.trim()
    ))
}

pub(crate) fn skill_views(skills: &SkillSet) -> Vec<SkillView> {
    skills
        .iter()
        .map(|skill| SkillView {
            name: skill.name.clone(),
            source: skill.source.label().to_string(),
            description: skill.description.clone(),
            dir: skill.dir.display().to_string(),
            body: skill.body.clone(),
            files: skill.files.clone(),
            files_truncated: skill.files_truncated,
        })
        .collect()
}
