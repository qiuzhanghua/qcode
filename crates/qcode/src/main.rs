mod agents;
mod doctor;
mod logging;
#[cfg(windows)]
mod sandbox_probe;
mod subagent;
mod tui;
mod update;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use qcode_kernel::config;

mod cli;
mod commands;
mod output;

use cli::{Cli, Command};
use commands::*;

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
    run()
}

#[tokio::main]
async fn run() -> ExitCode {
    let Cli {
        data_dir,
        config,
        approval_policy,
        command,
    } = Cli::parse();

    // Logging comes up before any command runs, because the first thing worth
    // recording is often why the command could not (ADR-0077). It comes up after
    // `parse` because the `[log]` level lives in the config file that names.
    //
    // Probing the config for its level can itself log; those lines are dropped
    // here because no subscriber exists yet. The command loads the same file again
    // and repeats any warning with the logger running, so nothing is lost overall.
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let log_level = qcode_kernel::load_config(config.clone(), &workspace)
        .ok()
        .map(|loaded| loaded.log.level);
    logging::init(log_level.as_deref());

    // The file lives in the data directory, so it has to wait for that path. The
    // hint, not `resolve_data_dir`: that one probes and falls back, and a command
    // that never touches the store (`qc skills list`) should not create one just
    // because it might have logged.
    //
    // `qc logs` is exempt, and not for tidiness: attaching a file creates today's
    // `qc.<date>.log` and the `qc.log` symlink, so the reader would manufacture the
    // very file it is about to report as absent. Its own diagnostics are not worth
    // that — a command whose job is to show a file should not write to it.
    if !matches!(command, Some(Command::Logs(_))) {
        logging::attach_file(&qcode_kernel::data_dir_hint(data_dir.clone()));
    }

    let result = match command {
        Some(Command::Run(args)) => cmd_run(args, data_dir, config, approval_policy).await,
        Some(Command::Resume(args)) => {
            cmd_tui(data_dir, config, approval_policy, Some(args.id)).await
        }
        Some(Command::Sessions(args)) => cmd_sessions(args, data_dir),
        Some(Command::Skills(args)) => cmd_skills(args, config),
        Some(Command::Logs(args)) => cmd_logs(args, data_dir),
        Some(Command::Config(args)) => cmd_config(args, config),
        Some(Command::Mcp(args)) => cmd_mcp(args, config),
        Some(Command::Doctor(args)) => cmd_doctor(data_dir, config, args.probe).await,
        Some(Command::ShellEnv(args)) => cmd_shell_env(args, config),
        Some(Command::Update(args)) => cmd_update(args, data_dir, config).await,
        Some(Command::Acp) => cmd_acp(data_dir, config, approval_policy).await,
        Some(Command::Gui) => cmd_gui(data_dir, config, approval_policy).await,
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
        None => cmd_tui(data_dir, config, approval_policy, None).await,
    };

    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
