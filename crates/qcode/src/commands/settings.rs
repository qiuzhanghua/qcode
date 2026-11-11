use super::*;

pub(crate) fn cmd_config(
    args: ConfigArgs,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match args.command {
        ConfigCommand::Show => {
            let workspace = std::env::current_dir()?.canonicalize()?;
            let loaded = qcode_kernel::load_config(config_path, &workspace)?;
            print_config(&loaded);
            if loaded.schema < qcode_kernel::config::CONFIG_VERSION {
                println!(
                    "\nthis file is schema {}; `qc config migrate` can drop duplicated defaults \
                     (backup kept as .bak)",
                    loaded.schema
                );
            }
        }
        ConfigCommand::Key { command } => match command {
            KeyCommand::Set {
                provider,
                service,
                account,
            } => return store_key(&provider, &service, account.as_deref()),
        },
        ConfigCommand::Migrate => {
            let path = config::edit_path(config_path)?;
            let report = qcode_kernel::config_edit::migrate_user_config(&path)?;
            if !report.changed {
                println!(
                    "{}: already schema {} (nothing to do)",
                    path.display(),
                    report.from
                );
            } else {
                println!(
                    "{}: schema {} → {}",
                    path.display(),
                    report.from,
                    qcode_kernel::config::CONFIG_VERSION
                );
                if report.folded_defaults > 0 {
                    println!(
                        "  folded {} provider default_model key(s) into [default-model]",
                        report.folded_defaults
                    );
                }
                if report.folded_network {
                    println!("  folded [network] enabled/mode into [sandbox] network");
                }
                if report.collapsed_models > 0 {
                    println!(
                        "  collapsed {} model table(s) into `models = [\"id\", …]`",
                        report.collapsed_models
                    );
                }
                if let Some(backup) = &report.backup {
                    println!("  backup: {}", backup.display());
                }
            }
        }
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
