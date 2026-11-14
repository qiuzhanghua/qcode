use super::*;

/// Where the GUI's workspace comes from — and why it is not just `current_dir()`.
///
/// `cfg`-gated with the rest of the window: the lean build has no GUI to place.
///
/// A GUI launched from Finder / Dock gets `cwd = /`. Measured on this machine
/// with a probe bundle that printed `getcwd()`:
///
/// ```text
/// terminal exec  -> cwd=/Users/q/space/opentj/qcode   (the shell's cwd)
/// open -a (Finder)-> cwd=/                            (LaunchServices)
/// ```
///
/// `/` is not a workspace, and `workspace` is not cosmetic: it is the primary
/// root for MCP confinement (`kernel::mcp_roots`, `primary: path == workspace`),
/// the sandbox write scope, and the place `.agents/skills` is looked up. With
/// `/` as the workspace, `workspace_write` silently means "write anywhere on the
/// disk" — a sandbox downgrade that no UI would show.
///
/// The `.app` launcher (`scripts/build-macos-app.sh`) already `cd`s to `$HOME`
/// before exec, so the double-click path was covered. This is the same rule
/// applied where it cannot be bypassed: any launcher on any platform that
/// hands us a filesystem root gets the home directory instead. A *terminal*
/// launch is untouched — there the cwd is a real choice.
fn gui_workspace() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let home = qcode_kernel::config::home_dir().map(|home| home.canonicalize().unwrap_or(home));
    let (workspace, fell_back) = choose_workspace(cwd, home);
    if fell_back {
        tracing::warn!(
            workspace = %workspace.display(),
            "gui: launched with a filesystem root as cwd; using the home directory as the workspace"
        );
    }
    Ok(workspace)
}

/// The decision itself, split out because [`gui_workspace`] reads the real
/// `current_dir()` and would therefore be untestable. Returns
/// `(workspace, used_the_home_fallback)`.
fn choose_workspace(cwd: PathBuf, home: Option<PathBuf>) -> (PathBuf, bool) {
    // A root has no parent: `/` on Unix, `C:\` on Windows. One rule for both,
    // rather than a per-platform list of "bad" directories to keep in sync
    // (and deliberately not a guess at what Windows shortcut defaults are).
    if cwd.parent().is_some() {
        return (cwd, false);
    }
    match home {
        Some(home) => (home, true),
        // No home to fall back to: the cwd we have beats failing to open a
        // window at all.
        None => (cwd, false),
    }
}

/// Build the closure that turns the current config into a fresh backend.
///
/// The shell calls it when the config file changes, so saving Settings swaps the
/// engine in place instead of re-execing the process (no window flash).
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
                Some(config_path.clone()),
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
                proxy_url: kernel
                    .egress
                    .as_ref()
                    .map(|egress| egress.proxy_url().to_string()),
                tools: kernel.tools,
                policy: kernel.policy,
                workspace: workspace.clone(),
                config_path: config_path.clone(),
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
                shell_env: kernel.shell_env.clone(),
                schedules: qcode_kernel::config::resolve_schedules(&kernel.config.schedules).0,
                schedule_errors: qcode_kernel::config::resolve_schedules(&kernel.config.schedules)
                    .1
                    .into_iter()
                    .map(|error| (error.name().to_string(), error.to_string()))
                    .collect(),
                schedule_tick: None,
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
pub(crate) async fn cmd_gui(
    data_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    approval_policy: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    // The end-of-session log line normally comes from `main`'s one funnel, which
    // works because `Platform::run` returns on Linux and Windows. On macOS it does
    // not: quitting goes through `-[NSApplication terminate:]`, AppKit runs the
    // quit observers and then calls `exit()` itself, so `main` never resumes and
    // the funnel never runs. The result was a `qc session starting` line with no
    // matching end, and the next launch warning that the previous session "was
    // killed, crashed, or the machine went down" — on every clean quit.
    //
    // Registered unconditionally rather than `cfg(target_os = "macos")`: the
    // callback is idempotent (`lifecycle::end_once` writes once and ignores the
    // rest), so on Linux and Windows it just writes the same line a moment earlier.
    // Gating it would make the two platforms diverge in a way only the log sees.
    qcode_gui::on_quit(Box::new(|| crate::lifecycle::finish(0)));

    // ADR-0083 §3: the lock is taken at the very top of `cmd_gui`, and `main`
    // never touches it. That placement is what keeps `qc run` / `qc acp` /
    // `qc logs` working while a GUI is open — none of them pass through here.
    //
    // The data dir is resolved first because it *is* the lock's scope
    // (ADR-0083 §2): two GUIs on two data dirs write two unrelated stores, so
    // treating them as one instance would be wrong. Resolving it once, here,
    // also keeps the setup branch and the normal branch on the same directory
    // instead of resolving twice.
    let gui_data_dir = qcode_kernel::resolve_data_dir(data_dir.clone());
    let single = match qcode_gui::single_instance::acquire(&gui_data_dir) {
        Ok(qcode_gui::SingleInstanceOutcome::Primary(primary)) => Some(primary),
        // Another instance raised its own window: that *is* what the user asked
        // for by launching us. Exit 0 — a script running `qc gui` must not see
        // a failure just because the GUI was already up.
        Ok(qcode_gui::SingleInstanceOutcome::HandedOver) => {
            // ADR-0082 §2 / ADR-0083 §5: even a process that exits immediately
            // goes through the log funnel. Writing "start" without "end" is what
            // makes the *next* launch report a false crash.
            crate::lifecycle::exit_now(0, crate::lifecycle::Reason::HandedOver)
        }
        // The mutex itself is broken (NFS, permissions). Not a reason to keep
        // the user out of the GUI — the GUI is how they debug things.
        Ok(qcode_gui::SingleInstanceOutcome::Disabled(why)) => {
            tracing::warn!(why = %why, "gui: single-instance check disabled");
            None
        }
        Err(error) => return Err(error.into()),
    };

    /// 图形启动时把进程环境补回来（ADR-0086 §3）。
    ///
    /// macOS 上经 LaunchServices 启动（双击 / Dock）**拿不到环境**：实测只给 5 个
    /// 变量——`__CFBundleIdentifier`、`__CF_USER_TEXT_ENCODING`、`XPC_FLAGS`、
    /// `XPC_SERVICE_NAME`、`SSH_AUTH_SOCK`。**没有 `PATH`，没有 `TERM`，什么都没有。**
    ///
    /// 这与 gpm-go 那次 C1 实测一致（`open` 起来的进程只有 launchd 基线，
    /// `open --env K=V` 四种写法全不生效）。
    ///
    /// 对 qcode 是实打实的损失，三处都在用：`qc doctor` 的 MCP 可执行文件查找
    /// （`doctor.rs` 已注明「Dock 启动时是 launchd 的最小 PATH」）、`config::resolve`
    /// 的 `env` API key 来源、`COT_HOME` / `TDP_HOME` 工具链注入。
    ///
    /// 解法不新造：复用 ADR-0064 那套「问用户的登录 shell 要环境」
    /// （`{shell} -lic 'env -0'`，带缓存与脱敏）。这里只做一件事——在采样**之前**
    /// 把缺的变量补进本进程，于是后续所有读 `std::env` 的代码（含采样本身）都看得到。
    ///
    /// 只补**当前缺失**的变量：显式传进来的 `--config` 之类不在这里，而用户真在
    /// 环境里设过的值（终端启动时通常全都有）不会被文件里的值盖掉。
    ///
    /// 非 macOS 平台空转：Linux 的 `.desktop` 与 Windows 的 `.lnk` 本来就把环境/
    /// 工作目录带进来了，没有这个洞。
    #[cfg(target_os = "macos")]
    fn restore_launch_environment(
        config_path: Option<&std::path::Path>,
        workspace: &std::path::Path,
    ) {
        let settings = config_path
            .and_then(|path| qcode_kernel::config::load(path).ok())
            .map(|loaded| loaded.shell)
            .unwrap_or_default();
        if settings.env_sampling == qcode_tools::shell_env::EnvSampling::Off {
            tracing::info!("gui: env sampling is off; graphical launch keeps the launchd baseline");
            return;
        }
        let cache = qcode_tools::shell_env::ShellEnvCache::new(settings);
        let Some(sampled) = cache.current_with(None) else {
            // 采不到不是致命错误：界面照旧能开，只是上下文相关的功能会缺东西。
            // 说清楚缺什么，比让它悄悄半残好。
            tracing::warn!(
                workspace = %workspace.display(),
                "gui: could not sample the login shell; PATH and env-based API keys stay at the launchd baseline"
            );
            return;
        };
        let mut added = 0usize;
        for (name, value) in sampled.vars() {
            if std::env::var_os(name).is_none() {
                // SAFETY: 单线程在这里从不成立，但 `set_var` 的 `unsafe` 是 Rust 2024
                // 对「改环境会影响别的线程」的提示。这个调用发生在窗口、引擎、任何
                // 工作线程存在**之前**（`run` 还没被调用），是能保证的唯一位置。
                unsafe { std::env::set_var(name, value) };
                added += 1;
            }
        }
        tracing::info!(
            added,
            sampled = sampled.len(),
            shell = %sampled.program().display(),
            "gui: restored environment from the login shell for a graphical launch"
        );
    }

    #[cfg(not(target_os = "macos"))]
    fn restore_launch_environment(
        _config_path: Option<&std::path::Path>,
        _workspace: &std::path::Path,
    ) {
    }

    let workspace = gui_workspace()?;
    // 环境补救要在**读配置之前**做：配置解析本身会查 `env` 里的 API key，而图形
    // 启动时那些变量一个都不在（ADR-0086 §3）。用还没被 workspace 锚定的
    // config 路径采样设置——采不到也无妨，那里会退回默认设置。
    // 没给 `--config` 时交给 `discover` 去找——那才是这次真正会读的文件。
    let env_config = config_path
        .clone()
        .or_else(|| config::discover(None).ok().map(|found| found.path));
    restore_launch_environment(env_config.as_deref(), &workspace);
    // Relative --config/--data-dir values are anchored at the workspace so they
    // mean the same thing whether or not the process was started from a shell.
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
    // Startup diagnostics, and the only record of them: a `.desktop` launch has no
    // terminal to print to, so these go to `logs/qc.log` (ADR-0077). The path used
    // to be printed twice — once with its source, once bare — which told the reader
    // nothing the second time.
    tracing::info!(
        config = %setup_path.display(),
        source = found.source.label(),
        "gui: config discovered"
    );
    if let Some(dir) = setup_path.parent()
        && !config::dir_accepts_new_files(dir)
    {
        // Was a hand-rolled `" WARN ..."` string, which `RUST_LOG` could not turn
        // off and which no `grep` for a level name would find.
        tracing::warn!(
            dir = %dir.display(),
            hint = config::CONFIG_DIR_HINT,
            "config directory denies new files"
        );
    }
    // A missing file is the clearest signal: `load` treats it as an empty
    // document, and `resolve` can still succeed when the shell provides the key
    // through env vars, which would silently skip the unconfigured path.
    let exists = setup_path.exists();
    let resolved = config::load(&setup_path).ok().and_then(|loaded| {
        config::resolve(&loaded, &config::Overrides::default(), &|key| {
            std::env::var(key).ok()
        })
        .ok()
    });
    match &resolved {
        Some(resolved) => tracing::info!(
            exists,
            provider = %resolved.name,
            model = %resolved.model,
            key = %resolved.api_key_source,
            "gui: config resolved"
        ),
        None => tracing::info!(exists, "gui: config exists but does not resolve"),
    }
    // A missing file is the clearest signal. A file that resolves but has no
    // provider entry still counts as configured when the shell provides the
    // values (env keys), which is a supported setup.
    let needs_setup = resolved.is_none();
    if needs_setup {
        tracing::info!("gui: no usable config, opening the window without a kernel");
        // No config yet: open the window anyway. Settings stays reachable from
        // the sidebar, and the backend refuses prompts with an explanation
        // instead of the old behaviour of trapping the user in the wizard.
        let setup_data_dir = gui_data_dir.clone();
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
        qcode_gui::run(app, Box::new(backend), setup_path, setup_data_dir, single);
        return Ok(ExitCode::SUCCESS);
    }
    let kernel = qcode_kernel::assemble(
        config_path,
        &workspace,
        data_dir,
        approval_policy.as_deref(),
        config::Overrides::default(),
    )
    .await?;
    // Startup diagnostics, and the only record of them: a `.desktop` launch has
    // no terminal to print to, so this goes to `logs/qc.log` (ADR-0077) like the
    // `gui: config discovered` line above. It used to be a `println!`, which put
    // narration on the stdout channel that ADR-0079 reserves for products — and
    // nothing consumes stdout on this path.
    tracing::info!(
        provider = %kernel.resolved.name,
        model = %kernel.resolved.model,
        sandbox = %kernel.sandbox,
        tools = kernel.tools.len(),
        skills = kernel.skills.len(),
        "gui: kernel resolved"
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
        proxy_url: kernel
            .egress
            .as_ref()
            .map(|egress| egress.proxy_url().to_string()),
        tools: kernel.tools,
        policy: kernel.policy,
        workspace: workspace.clone(),
        config_path: setup_path.clone(),
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
        shell_env: kernel.shell_env.clone(),
        schedules: qcode_kernel::config::resolve_schedules(&kernel.config.schedules).0,
        schedule_errors: qcode_kernel::config::resolve_schedules(&kernel.config.schedules)
            .1
            .into_iter()
            .map(|error| (error.name().to_string(), error.to_string()))
            .collect(),
        schedule_tick: None,
    };
    let backend = qcode_gui::LocalBackend::new(engine).with_rebuild(gui_rebuild_hook(
        setup_path.clone(),
        workspace.clone(),
        gui_data_dir.clone(),
        approval_policy.clone(),
    ));
    qcode_gui::run(app, Box::new(backend), setup_path, gui_data_dir, single);
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

    // `assemble` already installed the key for every log line; this is the handle
    // for the strings that bypass the logger — an error returned to `main`, which
    // prints it with `eprintln!` (ADR-0078).
    let redactor = qcode_kernel::redact::current();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_directory_is_kept_as_the_workspace() {
        let cwd = PathBuf::from("/Users/someone/project");
        let (workspace, fell_back) =
            choose_workspace(cwd.clone(), Some(PathBuf::from("/Users/someone")));
        assert_eq!(workspace, cwd, "终端启动的 cwd 是用户的选择，不许改写");
        assert!(!fell_back);
    }

    #[test]
    fn the_filesystem_root_falls_back_to_home() {
        // 实测：Finder / Dock 启动给的就是这个（`open -a` 打出来的 cwd 是 `/`）。
        let (workspace, fell_back) =
            choose_workspace(PathBuf::from("/"), Some(PathBuf::from("/Users/someone")));
        assert_eq!(workspace, PathBuf::from("/Users/someone"));
        assert!(fell_back, "应当报告回退过，好让日志里留下一条");
    }

    #[test]
    fn a_root_without_a_home_is_still_usable() {
        // 没有家目录可退时，宁可拿 `/` 开窗，也不要因为「路径不理想」拒绝启动。
        let (workspace, fell_back) = choose_workspace(PathBuf::from("/"), None);
        assert_eq!(workspace, PathBuf::from("/"));
        assert!(!fell_back);
    }
}
