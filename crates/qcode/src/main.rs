mod agents;
mod doctor;
mod logging;
#[cfg(windows)]
mod sandbox_probe;
mod subagent;
mod tui;
mod update;

use std::process::ExitCode;

use clap::Parser;
use qcode_kernel::config;

mod cli;
mod commands;
mod output;

use cli::{Cli, Command};
use commands::*;

#[tokio::main]
async fn main() -> ExitCode {
    logging::init();

    let Cli {
        data_dir,
        config,
        approval_policy,
        command,
    } = Cli::parse();

    let result = match command {
        Some(Command::Run(args)) => cmd_run(args, data_dir, config, approval_policy).await,
        Some(Command::Resume(args)) => {
            cmd_tui(data_dir, config, approval_policy, Some(args.id)).await
        }
        Some(Command::Sessions(args)) => cmd_sessions(args, data_dir),
        Some(Command::Skills(args)) => cmd_skills(args, config),
        Some(Command::Config(args)) => cmd_config(args, config),
        Some(Command::Mcp(args)) => cmd_mcp(args, config),
        Some(Command::Doctor) => cmd_doctor(data_dir, config).await,
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
