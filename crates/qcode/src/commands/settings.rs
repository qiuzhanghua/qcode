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

/// `--credential` values. The table lives in `qcode-mcp` so the CLI, the GUI's
/// picker and the config loader cannot drift; only the error message is ours.
fn parse_credential(name: &str) -> Result<qcode_mcp::CredentialPreset, String> {
    qcode_mcp::CredentialPreset::from_label(name).ok_or_else(|| {
        let choices = qcode_mcp::CredentialPreset::ALL
            .iter()
            .map(|preset| preset.label())
            .collect::<Vec<_>>()
            .join(", ");
        format!("unknown credential preset {name:?}; use one of: {choices}")
    })
}

fn parse_sandbox(mode: &str) -> Result<qcode_mcp::McpSandboxMode, String> {
    qcode_mcp::McpSandboxMode::from_label(mode)
        .ok_or_else(|| format!("unknown sandbox mode {mode:?}; use \"confine\" or \"off\""))
}

/// Split `--header "Name: value"` into a pair.
///
/// The credential and line-break checks live in `McpConfig::validate`, which
/// runs when `mcp_add` validates the document it wrote — one implementation, so
/// `qc mcp add` cannot accept a header that loading the file would reject.
fn parse_header(raw: &str) -> Result<(String, String), String> {
    let Some((name, value)) = raw.split_once(':') else {
        return Err(format!(
            "header {raw:?} is not `Name: value`; use e.g. --header 'Accept: application/json'"
        ));
    };
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() {
        return Err(format!("header {raw:?} has no name"));
    }
    Ok((name.to_string(), value.to_string()))
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
            transport,
            args,
            disabled,
            timeout_secs,
            read_paths,
            writable_paths,
            credentials,
            sandbox,
        } => {
            let credentials = credentials
                .iter()
                .map(|name| parse_credential(name))
                .collect::<Result<Vec<_>, _>>()?;
            let sandbox = sandbox.as_deref().map(parse_sandbox).transpose()?;
            let headers = transport
                .headers
                .iter()
                .map(|header| parse_header(header))
                .collect::<Result<Vec<_>, _>>()?;
            let path = config::edit_path(config_path)?;
            config::mcp_add(
                &path,
                config::NewMcpServer {
                    name: &name,
                    command: transport.command.as_deref(),
                    url: transport.url.as_deref(),
                    bearer_token_env_var: transport.bearer_token_env.as_deref(),
                    headers: &headers,
                    args: &args,
                    enabled: !disabled,
                    timeout_secs,
                    read_paths: &read_paths,
                    writable_paths: &writable_paths,
                    credentials: &credentials,
                    sandbox,
                },
            )?;
            println!(
                "added MCP server '{name}' ({}) to {}",
                if transport.url.is_some() {
                    "streamable-http"
                } else {
                    "stdio"
                },
                path.display()
            );
        }
        McpCommand::Remove { name } => {
            let path = config::edit_path(config_path)?;
            config::mcp_remove(&path, &name)?;
            println!("removed MCP server '{name}' from {}", path.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}
