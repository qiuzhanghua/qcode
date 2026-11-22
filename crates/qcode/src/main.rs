mod boot;
mod doctor;
#[cfg(windows)]
mod sandbox_probe;
mod tui;
mod update;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches};
use qcode_kernel::config;

mod clap_i18n;
mod cli;
#[cfg(test)]
mod cli_i18n_gate;
mod commands;
mod output;

use cli::{Cli, Command};
use commands::*;

/// 解析命令行，把 clap 的帮助与报错也翻成当前语言。
///
/// **为什么不能继续用 `Cli::parse_from`**：它只会在出错时自己打印再 `exit`，
/// 我们拿不到那段文本，也就没法改写 clap 硬编码的 `Usage:` / `error:` 之类
/// （I18N.md §4.5）。
///
/// **英文路径完全走原生**：直接用 `Error::print()` / `Error::exit()`，字节与颜色
/// 与改动前逐字节相同（不变量 1）。只有非英文才渲染成纯文本再过规则表——
/// `StyledStr` 的 `Display` 明确是 colorless，所以中文用户拿到的帮助**没有颜色**。
/// 这是 §4.5 记过的取舍：保色要自己重新着色，属独立改动。
fn parse_cli(argv: Vec<String>) -> Cli {
    let localized = clap_i18n::localize(Cli::command());
    let matches = match localized.try_get_matches_from(argv) {
        Ok(matches) => matches,
        Err(error) => exit_with_clap_error(error),
    };
    match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        // 走到这里说明 derive 与命令树不一致，是编程错误而非用户输入问题。
        Err(error) => exit_with_clap_error(error),
    }
}

/// 按 clap 的约定把错误打到对的流、用对的退出码。
fn exit_with_clap_error(error: clap::Error) -> ! {
    if qcode_i18n::locale() == qcode_i18n::DEFAULT {
        // 英文：原样交给 clap，含颜色与它自己的流向判断。
        error.exit()
    }
    use std::io::Write;
    let rendered = clap_i18n::apply_labels(&error.render().to_string());
    // 用 `write_all` 而不是 `print!` / `eprint!`：这段是**一整块已经渲染好的文本**，
    // 不是提问。`stdout_channels` 那条测试把所有 `print!("字面量")` 当成提问（因为有
    // 可能是在问 y/N），而 clap 对「帮助」类错误的约定恰恰是写 stdout。
    if error.use_stderr() {
        let _ = std::io::stderr().write_all(rendered.as_bytes());
    } else {
        let _ = std::io::stdout().write_all(rendered.as_bytes());
    }
    std::process::exit(error.exit_code());
}

/// The real entry point: answer the macOS guard, then hand over to the runtime.
///
/// The guard is this same binary re-executed with a sentinel argument (see
/// `qcode_tools::procguard`), and it has to be recognised **here**, before the
/// tokio runtime and before clap: the guard is a bare loop of `kqueue` and
/// `read` on purpose, and giving it an async runtime and a thread pool would be
/// both waste and a source of threads it must not have.
fn main() -> ExitCode {
    if let Some(code) = qcode_tools::procguard::guard_main_if_requested() {
        return ExitCode::from(u8::try_from(code).unwrap_or(1));
    }
    // Opt in to guard-backed cleanup before anything can spawn a shell. On
    // Windows this is an empty call: the kernel already does this, through the
    // job object every command is given.
    qcode_tools::procguard::enable();
    // 读走启动标记并从本进程环境里删掉（ADR-0089 §5）。必须在这里：`remove_var`
    // 改的是进程级 `environ`，此刻还没有 tokio 运行时、也没有别的线程。
    // 放晚一步，`run_shell` / 内建终端 / 环境采样的子进程就会继承它，
    // 而「在 GUI 拉起的 shell 里敲 qc」就会再开一个窗口。
    let env_marked_from_gui = boot::consume_env_marker();
    run(env_marked_from_gui)
}

#[tokio::main]
async fn run(env_marked_from_gui: bool) -> ExitCode {
    // 启动形态（ADR-0086）必须在 clap 之前定下来：内部标记不是给用户看的选项，
    // 让 clap 收下它等于允许它出现在 `--help` 与补全里。
    let (argv, marked_from_gui) = boot::strip_launch_marker(std::env::args());
    let launch = boot::detect(marked_from_gui, env_marked_from_gui);

    // 界面语言必须在 clap 之前定下来：帮助与报错文案是**解析时**求值的
    // （ADR-0105 §4.3）。放在 `parse_from` 之后，`qc --help` 就永远是英文。
    //
    // 这里传 `None`：P0 只认 `QCODE_LANG` 与系统首选。读 `[ui] language` 得先
    // 解析 `--config`，而配置加载在 clap 之后，属于后一批的事（I18N.md §9）。
    qcode_i18n::init(None);

    let Cli {
        data_dir,
        config,
        approval_policy,
        command,
    } = parse_cli(argv);

    // Logging comes up before any command runs, because the first thing worth
    // recording is often why the command could not (ADR-0077). It comes up after
    // `parse` because the `[log]` level lives in the config file that names.
    //
    // Probing the config for its level can itself log; those lines are dropped
    // here because no subscriber exists yet. The command loads the same file again
    // and repeats any warning with the logger running, so nothing is lost overall.
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let loaded = qcode_kernel::load_config(config.clone(), &workspace).ok();

    // 语言的第二次机会（I18N.md §9 第 4 条）：`init` 在 clap 之前跑，那时只有
    // 环境变量与系统首选；`[ui] language` 要等配置读出来。帮助与解析报错仍然是
    // 第一次定下的语言（它们求值太早，改不了），但**命令的输出**跟随配置。
    //
    // `QCODE_LANG` 仍然压过配置（ADR-0105 §2 的优先级），所以只在环境变量缺席时
    // 才补这一枪。`auto` / 认不出的值经 `explicit` 变成 `None`，等于不改。
    if std::env::var_os(qcode_i18n::LANG_ENV).is_none()
        && let Some(locale) =
            qcode_i18n::explicit(loaded.as_ref().and_then(|it| it.ui.language.as_deref()))
    {
        qcode_i18n::set(locale);
    }

    let log_level = loaded.and_then(|loaded| loaded.log.level);
    qcode_kernel::logging::init(log_level.as_deref());

    // The file lives in the data directory, so it has to wait for that path. The
    // hint, not `resolve_data_dir`: that one probes and falls back, and a command
    // that never touches the store (`qc skills list`) should not create one just
    // because it might have logged.
    //
    // `qc logs` is exempt, and not for tidiness: attaching a file creates today's
    // `qc.<date>.log` and the `qc.log` symlink, so the reader would manufacture the
    // very file it is about to report as absent. Its own diagnostics are not worth
    // that — a command whose job is to show a file should not write to it.
    //
    // The same exemption covers the lifecycle lines: they are only worth writing
    // if they persist, and for this command nothing persists.
    let logs_command = matches!(command, Some(Command::Logs(_)));
    if !logs_command {
        let data_dir = qcode_kernel::data_dir_hint(data_dir.clone());
        qcode_kernel::logging::attach_file(&data_dir);
        // The child-group registry lives in `qcode-tools`, which `qcode-core`
        // cannot depend on; the binary that owns the groups hands the reaper
        // over so a forced exit can still kill them (ADR-0082).
        qcode_core::lifecycle::install_reaper(qcode_tools::child_groups::kill_all);
        qcode_core::lifecycle::start(command_name(&command, launch.frontend), &data_dir);
    }

    let result = match command {
        Some(Command::Tui) => cmd_tui(data_dir, config, approval_policy, None).await,
        Some(Command::Run(args)) => cmd_run(args, data_dir, config, approval_policy).await,
        Some(Command::Resume(args)) => {
            cmd_tui(data_dir, config, approval_policy, Some(args.id)).await
        }
        Some(Command::Sessions(args)) => cmd_sessions(args, data_dir),
        Some(Command::Schedule(args)) => cmd_schedule(args, data_dir),
        Some(Command::Daemon(args)) => cmd_daemon(args, data_dir),
        Some(Command::Skills(args)) => cmd_skills(args, config),
        Some(Command::Logs(args)) => cmd_logs(args, data_dir),
        Some(Command::Config(args)) => cmd_config(args, config),
        Some(Command::Template(args)) => cmd_template(args),
        Some(Command::Mcp(args)) => cmd_mcp(args, config),
        Some(Command::Doctor(args)) => cmd_doctor(data_dir, config, args.probe).await,
        Some(Command::ShellEnv(args)) => cmd_shell_env(args, config),
        Some(Command::Update(args)) => cmd_update(args, data_dir, config).await,
        Some(Command::Acp) => cmd_acp(data_dir, config, approval_policy).await,
        #[cfg(windows)]
        Some(Command::SandboxProbe(args)) => sandbox_probe::run(&args),
        #[cfg(windows)]
        Some(Command::SandboxRun(args)) => Ok(ExitCode::from(
            qcode_tools::windows_acl::run_helper(&args.args) as u8,
        )),
        #[cfg(windows)]
        Some(Command::Sandbox(args)) => cmd_sandbox(args),
        #[cfg(windows)]
        Some(Command::Whoami) => {
            for line in qcode_tools::windows_acl::diagnose() {
                println!("{line}");
            }
            Ok(ExitCode::SUCCESS)
        }
        // 没有子命令：只可能是「被 GUI 入口拉起」这一种情况还需要开窗
        // （ADR-0086 §1 的标记层），其余一律打用法（ADR-0089）。
        //
        // 这里刻意**不**再退到 TUI：`qc` 敲错、脚本里写错一个变量，都会变成
        // 一个占住终端的全屏程序，而退出它的办法（Ctrl-C 两次）不在任何人的
        // 手指记忆里。「终端存在」不等于「用户想进入终端界面」。
        None => match launch.frontend {
            boot::Frontend::Gui => cmd_gui(data_dir, config, approval_policy).await,
            boot::Frontend::Tui => {
                print_usage();
                Ok(ExitCode::SUCCESS)
            }
        },
    };

    let code = match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    };

    // One funnel, so no ordinary return can skip the end line. The panic and
    // signal paths write their own (see `lifecycle`), and the guard there keeps
    // this from doubling it.
    let exit = if code == ExitCode::SUCCESS { 0 } else { 1 };
    qcode_core::lifecycle::finish(exit);
    code
}

/// The subcommand's name, for the start line.
///
/// Deliberately a name and not `std::env::args()`: see the module docs — at this
/// point the credential redactor is not armed yet, and argv is where an
/// `--api-key` would be.
fn command_name(command: &Option<Command>, launch: boot::Frontend) -> &'static str {
    match command {
        Some(Command::Tui) => "tui",
        Some(Command::Run(_)) => "run",
        Some(Command::Resume(_)) => "resume",
        Some(Command::Sessions(_)) => "sessions",
        Some(Command::Schedule(_)) => "schedule",
        Some(Command::Daemon(_)) => "daemon",
        Some(Command::Skills(_)) => "skills",
        Some(Command::Logs(_)) => "logs",
        Some(Command::Config(_)) => "config",
        Some(Command::Template(_)) => "template",
        Some(Command::Mcp(_)) => "mcp",
        Some(Command::Doctor(_)) => "doctor",
        Some(Command::ShellEnv(_)) => "shell-env",
        Some(Command::Update(_)) => "update",
        Some(Command::Acp) => "acp",
        #[cfg(windows)]
        Some(Command::SandboxProbe(_)) => "sandbox-probe",
        #[cfg(windows)]
        Some(Command::SandboxRun(_)) => "sandbox-run",
        #[cfg(windows)]
        Some(Command::Sandbox(_)) => "sandbox",
        #[cfg(windows)]
        Some(Command::Whoami) => "whoami",
        None => launch.as_str(),
    }
}

/// Bare `qc` prints usage instead of taking over the terminal (ADR-0089).
///
/// Goes to stdout and exits 0, exactly like `qc --help`: a typo or a script that
/// expanded to nothing should leave a readable answer behind, not a full-screen
/// program whose only exit is Ctrl-C twice.
fn print_usage() {
    // `render_help` into a string rather than `print_help` into clap's own
    // handle: the text is what the integration test asserts on, and a `String`
    // is the only shape that can be checked without capturing stdout.
    use std::io::Write;
    // 与 `qc --help` 走**同一条**渲染路径（含本地化与内置标签改写），否则两者
    // 会说出不一样的文本——ADR-0089 的整个意义是「裸 `qc` 给出可读的答案」。
    let text = clap_i18n::help_text(Cli::command());
    let mut out = std::io::stdout();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}
