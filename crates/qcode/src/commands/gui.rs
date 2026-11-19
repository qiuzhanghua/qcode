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
/// 图形启动的 cwd 是 launchd 给的 `/`（ADR-0085），所以这条规则是**唯一**一道——
/// `.app` 里那个负责 `cd $HOME` 的启动器已经在 ADR-0086 里删掉了（一份二进制，
/// 没有脚本可改）。实测确认这条路仍然成立：`open dist/QCode.app` 之后进程 cwd 是
/// `/`，这里退回 `/Users/q`。
///
/// 覆盖口是 `QCODE_APP_WORKSPACE`：从前它由启动器的 `cd` 兑现，现在由这里兑现，
/// 于是三端统一（Linux 的 `.desktop`、Windows 的 `.lnk` 同样能用它指定工作区），
/// 而且能从登录 shell 的 profile 里 export——环境补救正好跑在这之前。
///
/// 终端启动的 cwd 不动：那是用户真站在哪儿。
fn gui_workspace() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(over) = std::env::var_os("QCODE_APP_WORKSPACE").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(over);
        // 指了个不存在的目录就当没设：宁可退回默认规则，也不要因为一个坏路径
        // 拒绝开窗——那种失败用户完全看不出原因。
        match path.canonicalize() {
            Ok(path) if path.is_dir() => {
                tracing::info!(
                    workspace = %path.display(),
                    "gui: QCODE_APP_WORKSPACE overrides the workspace"
                );
                return Ok(path);
            }
            _ => tracing::warn!(
                value = %path.display(),
                "gui: QCODE_APP_WORKSPACE is not an existing directory; ignoring it"
            ),
        }
    }
    let cwd = std::env::current_dir()?.canonicalize()?;
    let home = qcode_kernel::config::home_dir().map(|home| home.canonicalize().unwrap_or(home));
    let (workspace, fell_back) = qcode_core::choose_workspace(cwd, home);
    if fell_back {
        tracing::warn!(
            workspace = %workspace.display(),
            "gui: launched with a filesystem root as cwd; using the home directory as the workspace"
        );
    }
    Ok(workspace)
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
        // `data_dir` 下面会被 move 进 `assemble`；rebuild hook 构造还要用它，
        // 所以先留一份。
        let rebuild_data_dir = data_dir.clone();
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
            // ADR-0087: same two strings `qc run` and the TUI inject, so the
            // model sees `AGENTS.md` and the skills index here too.
            let skills_prompt = kernel.skills.prompt_section();
            let agents_md = qcode_core::agents::load(&workspace).ok().flatten();
            let engine = qcode_gui::Engine {
                store: kernel.store,
                provider: kernel.provider.into(),
                provider_name: kernel.resolved.name.clone(),
                sandbox: kernel.sandbox.clone(),
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
                skills_prompt,
                agents_md,
                shell_env: kernel.shell_env.clone(),
                schedules: qcode_kernel::config::resolve_schedules(&kernel.config.schedules).0,
                // `[schedules]` 的**键**（含解析失败的）：reconcile 用它判断
                // 「还声明着吗」，见 `Engine::schedule_declared`。
                schedule_declared: kernel.config.schedules.keys().cloned().collect(),
                schedule_errors: qcode_kernel::config::resolve_schedules(&kernel.config.schedules)
                    .1
                    .into_iter()
                    .map(|error| (error.name().to_string(), error.to_string()))
                    .collect(),
                session_title: kernel.config.session_title.to_title_config().ok().flatten(),
                schedule_tick: None,
            };
            // 重建出来的 backend 必须还能再重建：不挂 hook 的话，第一次保存
            // 设置换进来的 backend 是「一次性」的，之后的配置变化全部被
            // `rebuild_hook().is_none()` 拒绝——状态栏、模型等跟着第一次
            // rebuild 之后就不再更新。挂同一个 hook（参数一致），无限次可用。
            let rebuild = gui_rebuild_hook(
                config_path.clone(),
                workspace.clone(),
                rebuild_data_dir,
                approval_policy.clone(),
            );
            Ok(
                Box::new(qcode_gui::LocalBackend::new(engine).with_rebuild(rebuild))
                    as Box<dyn qcode_gui::Backend>,
            )
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
    qcode_gui::on_quit(Box::new(|| qcode_core::lifecycle::finish(0)));

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
    let single = match qcode_core::single_instance::acquire(&gui_data_dir) {
        Ok(qcode_core::single_instance::Outcome::Primary(primary)) => Some(primary),
        // Another instance raised its own window: that *is* what the user asked
        // for by launching us. Exit 0 — a script running `qc gui` must not see
        // a failure just because the GUI was already up.
        Ok(qcode_core::single_instance::Outcome::HandedOver) => {
            // ADR-0082 §2 / ADR-0083 §5: even a process that exits immediately
            // goes through the log funnel. Writing "start" without "end" is what
            // makes the *next* launch report a false crash.
            qcode_core::lifecycle::exit_now(0, qcode_core::lifecycle::Reason::HandedOver)
        }
        // The mutex itself is broken (NFS, permissions). Not a reason to keep
        // the user out of the GUI — the GUI is how they debug things.
        Ok(qcode_core::single_instance::Outcome::Disabled(why)) => {
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
    /// Linux 是**另一种形式的同一个洞**：`.desktop` 由桌面会话 exec，给的是会话环境，
    /// 而不是用户交互式 shell 的环境。`~/.zshrc` 里的 `export DEEPSEEK_API_KEY=…`
    /// 因此不在进程里——见下面 `restore_launch_environment` 的注释。
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
    /// **这里原本写着「非 macOS 平台空转：Linux 的 `.desktop` 与 Windows 的 `.lnk`
    /// 本来就把环境/工作目录带进来了，没有这个洞」——那句在 Linux 上是错的**
    /// （2026-11-16 修，ADR-0094）。桌面会话启动的进程拿到的是**会话环境**——
    /// `systemd --user` / gnome-shell 从登录时的那份环境派生——而 `~/.zshrc` 里的
    /// `export DEEPSEEK_API_KEY=…` 只存在于**交互式 shell**。于是同一台机器、同一个二进制：
    ///
    /// * 终端里跑 `./target/release/qc --launched-from-gui` → 继承 export → 有 key；
    /// * 点图标 → 没有 key → `config::resolve` 判定该 provider 没有凭证，模型消失。
    ///
    /// 症状是「从菜单起就没有模型配置」，看起来像配置没读到，实际是环境不同。ADR-0086
    /// §3 的验证表里 macOS 那一行是 `env -i` 实测过的，Linux 与 Windows 只记了「没有
    /// 真机跑过」——这个洞就是那句备注。
    ///
    /// 所以 Linux 与 macOS 一样要补。Windows 仍空转：`.lnk` 同样只有登录会话环境，
    /// 逻辑上有同一个洞，但多数人把 key 设成「用户环境变量」（那样会被继承），而本仓
    /// 没有 Windows 真机可验证 pwsh 采样——留着当已知缺口，比照 macOS 的样子声称修好
    /// 更诚实。
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn restore_launch_environment(config_path: Option<&std::path::Path>) {
        let settings = config_path
            .and_then(|path| qcode_kernel::config::load(path).ok())
            .map(|loaded| loaded.shell)
            .unwrap_or_default();
        if settings.env_sampling == qcode_tools::shell_env::EnvSampling::Off {
            tracing::info!("gui: env sampling is off; graphical launch keeps the session baseline");
            return;
        }
        let cache = qcode_tools::shell_env::ShellEnvCache::new(settings);
        let Some(sampled) = cache.current_with(None) else {
            // 采不到不是致命错误：界面照旧能开，只是上下文相关的功能会缺东西。
            // 说清楚缺什么，比让它悄悄半残好。
            tracing::warn!(
                "gui: could not sample the login shell; PATH and env-based API keys stay at the session baseline"
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

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn restore_launch_environment(_config_path: Option<&std::path::Path>) {}

    // 顺序有讲究：**先**补环境，**再**算 workspace。
    //
    // 环境补救要在读配置之前（配置解析本身会查 `env` 里的 API key，而图形启动时
    // 那些变量一个都不在，ADR-0086 §3），也要在 workspace 之前——`QCODE_APP_WORKSPACE`
    // 是可以从登录 shell 里 export 的，而那正是我们才刚补进来的东西（ADR-0085 §4）。
    //
    // 这里用的 config 路径还没被 workspace 锚定：相对的 `--config` 可能解析不到，
    // 那就退回默认采样设置——采不到的后果只是少补几个变量，不是失败。
    let env_config = config_path
        .clone()
        .or_else(|| config::discover(None).ok().map(|found| found.path));
    restore_launch_environment(env_config.as_deref());

    let workspace = gui_workspace()?;
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
    // `/skills` and `/<skill-name>` read this list (ADR-0087). The TUI has
    // always set it; the GUI did not, so both commands would have reported an
    // unknown command.
    app.skills = skill_views(&kernel.skills);
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
        sandbox: kernel.sandbox.clone(),
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
        // ADR-0087: same two strings `qc run` and the TUI inject.
        skills_prompt: kernel.skills.prompt_section(),
        agents_md: qcode_core::agents::load(&workspace)?,
        shell_env: kernel.shell_env.clone(),
        schedules: qcode_kernel::config::resolve_schedules(&kernel.config.schedules).0,
        schedule_declared: kernel.config.schedules.keys().cloned().collect(),
        schedule_errors: qcode_kernel::config::resolve_schedules(&kernel.config.schedules)
            .1
            .into_iter()
            .map(|error| (error.name().to_string(), error.to_string()))
            .collect(),
        session_title: kernel.config.session_title.to_title_config().ok().flatten(),
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

    let agents_md = qcode_core::agents::load(&workspace)?;
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
        config_path: kernel.config_path.clone(),
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

    /// `QCODE_APP_WORKSPACE` 是**进程全局**的，而测试默认并行。
    ///
    /// 下面两条测试都改它，于是「A 设好 → B 覆盖 → A 读」这个交错会真的发生：
    /// A 读到自己设的目录之外的值，`gui_workspace` 正确地忽略坏值、退回默认规则，
    /// A 却断言「应该解析成我设的那个目录」——红了。**红的不是产品代码，是测试**。
    ///
    /// 2026-11-17 的 0.15.0 CI 上就这么红过一次（`test (ubuntu-latest)` 61 passed
    /// 1 failed）：本机 28 核、单跑这两条从不失败，CI 的 2 核 runner 上 62 条测试
    /// 挤在一起才撞上。所以这不是「偶发」，是「只在小机器上显形」。
    ///
    /// 用锁而不是把两条合成一条：它们验的是两件不同的事（好值生效 / 坏值被忽略），
    /// 合起来会让失败信息说不清是哪一件坏了。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 拿锁并穿透中毒：一条测试 panic 之后，其余的不该连带报「poisoned」。
    fn hold_env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `restore_launch_environment` 必须在 Linux 上也有实现体（ADR-0094）。
    ///
    /// 这个函数原本是 `#[cfg(target_os = "macos")]`，注释还写着「Linux 的 `.desktop`
    /// 本来就把环境带进来了，没有这个洞」。真机结果相反：菜单启动拿到的是**会话**
    /// 环境，`~/.zshrc` 的 `export` 不在里面，于是 `api_key_env` 型凭证解析不到，
    /// 模型整个消失——而同一个二进制从终端起一切正常。
    ///
    /// 「空转」在功能上是隐形的：`restore_launch_environment(..) {}` 编译得过、测试
    /// 全绿、只在用户的桌面上失败。所以用一条对着源码的检查把它钉住，和
    /// `boot.rs::the_linux_installer_writes_the_same_marker` 是同一个办法。
    ///
    /// **第一版写成了 `source.contains(r#"#[cfg(any(…))]"#)`，那条是自证**：断言里的
    /// 字面量本身就在同一个文件里，于是它永远能找到「自己」，把两处 `cfg` 一起改回
    /// macOS-only 依然绿。教训是这类检查必须**锚定位置**——只看某个函数定义之前的那
    /// 一个属性，而不是全文件里有没有这个串。
    #[test]
    fn the_launch_environment_is_restored_on_linux_too() {
        /// 取 `marker` 之前最近的一个 `#[cfg(..)]`，即真正作用于那个条目的属性。
        fn cfg_above(source: &str, marker: &str) -> String {
            let at = source.find(marker).unwrap_or_else(|| panic!("no {marker}"));
            let head = &source[..at];
            let start = head
                .rfind("#[cfg(")
                .expect("a cfg attribute above the item");
            head[start..].trim().to_string()
        }

        let source = include_str!("gui.rs");
        assert_eq!(
            cfg_above(source, "fn restore_launch_environment(config_path"),
            r#"#[cfg(any(target_os = "macos", target_os = "linux"))]"#,
            "restore_launch_environment must be compiled for linux as well; a macOS-only \
             cfg silently drops rc-file API keys on a menu launch"
        );
        // 另一条臂必须是**精确的补集**：写成 `not(macos)` 会让 Linux 同时命中两个
        // 定义，写成 `not(macos, linux)` 少一个平台就漏一个平台。
        assert_eq!(
            cfg_above(source, "fn restore_launch_environment(_config_path"),
            r#"#[cfg(not(any(target_os = "macos", target_os = "linux")))]"#,
            "the no-op arm must be the exact complement of the real one"
        );
    }

    /// `QCODE_APP_WORKSPACE` 是覆盖口，从前由 `.app` 启动器的 `cd` 兑现，
    /// 启动器删掉后（ADR-0086）由 `gui_workspace` 兑现（ADR-0085 §4）。
    ///
    /// 真机验过（`open dist/QCode.app`）：
    ///   不设 → cwd 是 `/` → 退到 `/Users/q`，日志说 workspace=/Users/q
    ///   设了 → 直接用，不再退
    #[test]
    fn the_workspace_override_wins_over_the_root_fallback() {
        let _env = hold_env_lock();
        let dir = std::env::temp_dir().join(format!("qc-ws-override-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let expected = dir.canonicalize().expect("canonical temp dir");
        // SAFETY: `hold_env_lock` 保证同进程内没有别的测试同时动这个变量名。
        // （它不是「全局无数据竞争」的证明——`set_var` 影响的是整个进程，而其它
        // 测试仍可能读 `std::env`——但对这个变量而言，写这一侧是串行的。）
        unsafe { std::env::set_var("QCODE_APP_WORKSPACE", &dir) };
        let workspace = gui_workspace();
        unsafe { std::env::remove_var("QCODE_APP_WORKSPACE") };
        let _ = std::fs::remove_dir_all(&dir);
        // macOS 的 /tmp 是 /private/tmp 的符号链接，两边都先 canonicalize 再比；
        // 而且要在删目录**之前**比完（`choose_workspace` 返回的已经是规范路径）。
        assert_eq!(workspace.expect("override should resolve"), expected);
    }

    /// 指了个不存在的目录就当没设：宁可退回默认规则，也不要因为一个坏路径拒绝开窗。
    #[test]
    fn a_broken_override_is_ignored_instead_of_failing() {
        let _env = hold_env_lock();
        // SAFETY: 同 `the_workspace_override_wins_over_the_root_fallback`——这把锁
        // 把「谁在写这个变量」串行化，交错不再可能。
        unsafe { std::env::set_var("QCODE_APP_WORKSPACE", "/nonexistent/qc-nowhere") };
        let workspace = gui_workspace();
        unsafe { std::env::remove_var("QCODE_APP_WORKSPACE") };
        let workspace = workspace.expect("a broken override must not stop the window");
        assert_ne!(workspace, PathBuf::from("/nonexistent/qc-nowhere"));
        assert!(workspace.is_dir());
    }
}
