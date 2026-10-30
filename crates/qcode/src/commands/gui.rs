use super::*;

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
