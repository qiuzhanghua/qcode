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
        ConfigCommand::Init { template, force } => {
            return config_init(&template, force, config_path);
        }
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

/// `qc config init --template <id>` (ADR-0104 §2): materialise a template into a
/// real `config.toml`.
///
/// The template is scaffolding, not an overlay (§1): it is read once, writes an
/// ordinary config file, and never participates in loading afterwards. So the
/// file this command produces is exactly what the user owns from then on — and
/// upgrading qcode cannot change it behind their back.
///
/// Security templates refuse to overwrite an existing file unless `--force`
/// (which prints a diff first): `[sandbox]` / `[network]` are a safety boundary,
/// and changing them has to be a visible, deliberate act. The provider-append
/// case of §2 waits for the llm templates.
fn config_init(
    template_arg: &str,
    force: bool,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let (kind, id) = parse_template_arg(template_arg)?;
    let template = qcode_kernel::template::find(kind, &id)?;

    let mut document = template.document()?;
    if !template.applies_here() {
        if !force {
            return Err(format!(
                "this template belongs to {}; refusing to write it on {} \
                 (use --force to write anyway)",
                template.os.join(", "),
                qcode_kernel::template::CURRENT_OS,
            )
            .into());
        }
        // §验证3: `--force` writes, and the file says it does not apply here —
        // so if assembly later fails it does so with the reason visible, not as
        // a bare `Unsupported` out of nowhere.
        document = format!(
            "# 本平台不可用：这份模板声明属于 {}，当前平台是 {}。\n\
             # 文件已用 --force 写出；本机的沙箱后端可能无法装配。\n\n{document}",
            template.os.join(", "),
            qcode_kernel::template::CURRENT_OS,
        );
    }
    // The generated file carries the schema version it was written with, so a
    // future `qc config migrate` can tell how old it is (ADR-0104 §后果).
    document = format!(
        "# Generated by `qc config init --template {}/{id}`.\n\
         # Schema {schema}: run `qc config migrate` when a newer version is out.\n\
         schema = {schema}\n\n{document}",
        kind.as_str(),
        schema = qcode_kernel::config::CONFIG_VERSION,
    );

    let path = config::edit_path(config_path)?;
    if path.exists() {
        if !force {
            return Err(format!(
                "{} already exists; refusing to overwrite \
                 (use --force to print a diff first)",
                path.display(),
            )
            .into());
        }
        let existing = std::fs::read_to_string(&path)?;
        println!("--- {} (existing)", path.display());
        println!("+++ template {}/{id}", kind.as_str());
        for line in line_diff(&existing, &document) {
            println!("{line}");
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &document)?;
    match config::load(&path) {
        Ok(_) => println!(
            "wrote {} from template {}/{id}",
            path.display(),
            kind.as_str()
        ),
        Err(error) => eprintln!(
            "warning: wrote {} but it does not load: {error}",
            path.display()
        ),
    }
    Ok(ExitCode::SUCCESS)
}

/// Parse the `--template` argument: a kind-prefixed id (`security-landlock`,
/// `llm/deepseek`) or a bare id when it names exactly one template.
fn parse_template_arg(arg: &str) -> Result<(qcode_kernel::template::Kind, String), String> {
    use qcode_kernel::template::Kind;
    let prefixed = if let Some(id) = arg.strip_prefix("security-") {
        Some((Kind::Security, id))
    } else if let Some(id) = arg.strip_prefix("llm-") {
        Some((Kind::Llm, id))
    } else if let Some((kind, id)) = arg.split_once('/') {
        let kind = match kind {
            "security" => Kind::Security,
            "llm" => Kind::Llm,
            other => return Err(format!("unknown template kind {other:?}; use security/ or llm/")),
        };
        Some((kind, id))
    } else {
        None
    };
    if let Some((kind, id)) = prefixed {
        if id.is_empty() {
            return Err(format!("template name after the kind is empty: {arg:?}"));
        }
        return Ok((kind, id.to_string()));
    }
    // Bare id: must resolve to exactly one template.
    let mut found: Option<Kind> = None;
    for template in qcode_kernel::template::all().map_err(|error| error.to_string())? {
        if template.id == arg {
            if let Some(previous) = found
                && previous != template.kind
            {
                return Err(format!(
                    "{arg:?} exists in both security/ and llm/; spell it out, e.g. \
                     security-{arg} or llm-{arg}"
                ));
            }
            found = Some(template.kind);
        }
    }
    match found {
        Some(kind) => Ok((kind, arg.to_string())),
        None => Err(format!(
            "no template named {arg:?}; run `qc template list` to see the available ones"
        )),
    }
}

/// `qc template list` (ADR-0104 §2): every embedded template, what it is, and
/// whether it applies to this platform.
///
/// Source is `built-in` for now; user templates are a later decision, but the
/// label is printed so the column exists when they arrive.
pub(crate) fn cmd_template(
    args: TemplateArgs,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match args.command {
        TemplateCommand::List => {
            let templates = qcode_kernel::template::all()?;
            for template in &templates {
                let os = if template.os.is_empty() {
                    "all platforms".to_string()
                } else {
                    template.os.join(", ")
                };
                let applies = if template.applies_here() {
                    "applies here"
                } else {
                    "not for this platform"
                };
                println!(
                    "{:<10} {:<14} [built-in] {} ({os}; {applies})",
                    template.kind.as_str(),
                    template.id,
                    template.title,
                );
                println!("    {}", template.summary);
            }
            println!("\n{} template(s)", templates.len());
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// A minimal line diff for `--force` output: unchanged lines keep a leading
/// space, removals get `-`, additions get `+`. Good enough for a config file;
/// unified hunks would add complexity without helping the decision.
fn line_diff(old: &str, new: &str) -> Vec<String> {
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let (n, m) = (old_lines.len(), new_lines.len());
    let mut table = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if old_lines[i] == new_lines[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            out.push(format!(" {}", old_lines[i]));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            out.push(format!("-{}", old_lines[i]));
            i += 1;
        } else {
            out.push(format!("+{}", new_lines[j]));
            j += 1;
        }
    }
    while i < n {
        out.push(format!("-{}", old_lines[i]));
        i += 1;
    }
    while j < m {
        out.push(format!("+{}", new_lines[j]));
        j += 1;
    }
    out
}
