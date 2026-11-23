use super::*;

use std::collections::HashMap;

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
                    "\n{}",
                    qcode_i18n::trp(
                        "cli.cfg.show_outdated",
                        &[loaded.schema.to_string().as_str()]
                    )
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
                    "{}",
                    qcode_i18n::trp(
                        "cli.cfg.migrate_current",
                        &[
                            path.display().to_string().as_str(),
                            report.from.to_string().as_str()
                        ]
                    )
                );
            } else {
                println!(
                    "{}",
                    qcode_i18n::trp(
                        "cli.cfg.migrate_done",
                        &[
                            path.display().to_string().as_str(),
                            report.from.to_string().as_str(),
                            qcode_kernel::config::CONFIG_VERSION.to_string().as_str()
                        ]
                    )
                );
                if report.folded_defaults > 0 {
                    println!(
                        "{}",
                        qcode_i18n::trp(
                            "cli.cfg.migrate_folded_defaults",
                            &[report.folded_defaults.to_string().as_str()]
                        )
                    );
                }
                if report.folded_network {
                    println!("{}", qcode_i18n::tr("cli.cfg.migrate_folded_network"));
                }
                if report.collapsed_models > 0 {
                    println!(
                        "{}",
                        qcode_i18n::trp(
                            "cli.cfg.migrate_collapsed_models",
                            &[report.collapsed_models.to_string().as_str()]
                        )
                    );
                }
                if let Some(backup) = &report.backup {
                    println!(
                        "{}",
                        qcode_i18n::trp(
                            "cli.cfg.migrate_backup",
                            &[backup.display().to_string().as_str()]
                        )
                    );
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
                return Err(qcode_i18n::trp(
                    "err.cli.settings.editor_exited",
                    &[status.to_string().as_str()],
                )
                .into());
            }
            match config::load(&path) {
                Ok(_) => println!(
                    "{}",
                    qcode_i18n::trp("cli.cfg.edit_ok", &[path.display().to_string().as_str()])
                ),
                Err(err) => eprintln!(
                    "{}",
                    qcode_i18n::trp("cli.cfg.edit_bad", &[err.to_string().as_str()])
                ),
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
        qcode_i18n::trp(
            "err.cli.template.unknown_preset",
            &[format!("{name:?}").as_str(), choices.as_str()],
        )
    })
}

fn parse_sandbox(mode: &str) -> Result<qcode_mcp::McpSandboxMode, String> {
    qcode_mcp::McpSandboxMode::from_label(mode).ok_or_else(|| {
        qcode_i18n::trp(
            "err.cli.settings.sandbox_mode_invalid",
            &[format!("{mode:?}").as_str()],
        )
    })
}

/// Split `--header "Name: value"` into a pair.
///
/// The credential and line-break checks live in `McpConfig::validate`, which
/// runs when `mcp_add` validates the document it wrote — one implementation, so
/// `qc mcp add` cannot accept a header that loading the file would reject.
fn parse_header(raw: &str) -> Result<(String, String), String> {
    let Some((name, value)) = raw.split_once(':') else {
        return Err(qcode_i18n::trp(
            "err.cli.settings.header_not_pair",
            &[format!("{raw:?}").as_str()],
        ));
    };
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() {
        return Err(qcode_i18n::trp(
            "err.cli.settings.header_no_name",
            &[format!("{raw:?}").as_str()],
        ));
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
                "{}",
                qcode_i18n::trp(
                    "cli.cfg.mcp_added",
                    &[
                        name.as_str(),
                        if transport.url.is_some() {
                            "streamable-http"
                        } else {
                            "stdio"
                        },
                        path.display().to_string().as_str()
                    ]
                )
            );
        }
        McpCommand::Remove { name } => {
            let path = config::edit_path(config_path)?;
            config::mcp_remove(&path, &name)?;
            println!(
                "{}",
                qcode_i18n::trp(
                    "cli.cfg.mcp_removed",
                    &[name.as_str(), path.display().to_string().as_str()]
                )
            );
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
/// Two behaviours, one per kind. Security templates write a whole file and
/// refuse to touch an existing one unless `--force` (which prints a diff
/// first): `[sandbox]` / `[network]` are a safety boundary, and changing them
/// has to be a visible, deliberate act. Provider templates never rewrite a
/// whole file; an existing config gets one `[providers.*]` appended after a
/// diff and a confirmation (§2) — `--force` has no seat there, the
/// confirmation is unconditional.
fn config_init(
    template_arg: &str,
    force: bool,
    config_path: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    use qcode_kernel::template::Kind;

    let (kind, id) = parse_template_arg(template_arg)?;
    let template = qcode_kernel::template::find(kind, &id)?;
    // Answer the prompts before rendering: text/path answers are substituted
    // into the document, secrets are never written — only hinted at (§4).
    let answers = collect_prompt_answers(&template)?;
    let mut document = template.substitute(&answers)?;
    let path = config::edit_path(config_path)?;

    // A provider template on an existing file takes the append path: one
    // `[providers.*]` added after a printed diff and a y/N prompt. Appending
    // is already non-destructive, so `--force` does not apply here.
    if path.exists() && kind == Kind::Llm {
        return provider_append(&template, &document, &path, confirm_append);
    }

    if !template.applies_here() {
        if !force {
            return Err(qcode_i18n::trp(
                "err.cli.template.wrong_os",
                &[
                    template.os.join(", ").as_str(),
                    qcode_kernel::template::CURRENT_OS,
                ],
            )
            .into());
        }
        // §验证3: `--force` writes, and the file says it does not apply here —
        // so if assembly later fails it does so with the reason visible, not as
        // a bare `Unsupported` out of nowhere.
        document = format!(
            "{}\n\n{document}",
            qcode_i18n::trp(
                "cli.config.unsupported_template_notice",
                &[
                    template.os.join(", "),
                    qcode_kernel::template::CURRENT_OS.to_string(),
                ],
            ),
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

    if path.exists() {
        if !force {
            return Err(qcode_i18n::trp(
                "err.cli.template.already_exists",
                &[path.display().to_string().as_str()],
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
            "{}",
            qcode_i18n::trp(
                "cli.cfg.init_wrote",
                &[
                    path.display().to_string().as_str(),
                    format!("{}/{}", kind.as_str(), id).as_str()
                ]
            )
        ),
        Err(error) => eprintln!(
            "{}",
            qcode_i18n::trp(
                "cli.cfg.init_not_loadable",
                &[
                    path.display().to_string().as_str(),
                    error.to_string().as_str()
                ]
            )
        ),
    }
    print_key_hints(&template);
    Ok(ExitCode::SUCCESS)
}

/// Ask the text/path prompts of a template and return the answers keyed by
/// prompt key.
///
/// Secret prompts are never asked for (ADR-0104 §4): the generated file only
/// carries the environment variable name, and [`print_key_hints`] says where
/// the value should live. On a non-TTY the remaining prompts cannot be
/// answered, so the command refuses rather than hanging on a silent read.
fn collect_prompt_answers(
    template: &qcode_kernel::template::Template,
) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    use qcode_kernel::template::PromptKind;
    use std::io::IsTerminal;

    let asks: Vec<&qcode_kernel::template::Prompt> = template
        .prompts
        .iter()
        .filter(|prompt| prompt.kind != PromptKind::Secret)
        .collect();
    if asks.is_empty() {
        return Ok(HashMap::new());
    }
    if !std::io::stdin().is_terminal() {
        let keys: Vec<&str> = asks.iter().map(|prompt| prompt.key.as_str()).collect();
        return Err(qcode_i18n::trp(
            "err.cli.template.needs_terminal",
            &[keys.join(", ").as_str()],
        )
        .into());
    }
    let mut answers = HashMap::new();
    for prompt in asks {
        use std::io::Write as _;
        eprint!("{}: ", prompt.label);
        std::io::stderr().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let value = line.trim();
        if value.is_empty() {
            return Err(
                qcode_i18n::trp("err.cli.settings.empty_answer", &[prompt.key.as_str()]).into(),
            );
        }
        answers.insert(prompt.key.clone(), value.to_string());
    }
    Ok(answers)
}

/// Append one `[providers.*]` from an llm template to an existing config file
/// (ADR-0104 §2) — the only operation a provider template may perform on an
/// existing file.
///
/// The section is appended at text level: re-serialising the whole document
/// would strip every comment in the user's config, and comments are half of
/// what a template is for. The combined file must parse before the diff is
/// even shown, and a route that already exists is refused — TOML forbids two
/// `[providers.X]` tables, and rewriting the user's route behind a diff would
/// be exactly the silent boundary change this command exists to avoid.
fn provider_append(
    template: &qcode_kernel::template::Template,
    document: &str,
    path: &std::path::Path,
    confirm: impl FnOnce() -> Result<bool, Box<dyn std::error::Error>>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let id = &template.id;
    let existing = std::fs::read_to_string(path)?;
    let loaded = config::load(path)?;
    if loaded.providers.contains_key(id) {
        return Err(qcode_i18n::trp(
            "err.cli.template.provider_exists",
            &[path.display().to_string().as_str(), id.as_str()],
        )
        .into());
    }
    let section = format!(
        "# Added by `qc config init --template {}/{id}`.\n{document}",
        template.kind.as_str(),
    );
    // Validate the combined file the same way `template.config()` validates the
    // template alone. An append touches only `[providers.*]`, so a user's
    // sandbox/network pair is untouched by construction; the whole must still
    // parse and pass the config-wide checks (see [`combine_config`]).
    let combined =
        combine_config(&existing, &section).map_err(|error| format!("{error}; not writing"))?;

    println!("--- {} (existing)", path.display());
    println!("+++ template {}/{}", template.kind.as_str(), id);
    for line in line_diff(&existing, &combined) {
        println!("{line}");
    }
    // The decision is injected so the decline path is testable without a
    // terminal; production passes [`confirm_append`], which refuses to even
    // ask on a non-TTY.
    if !confirm()? {
        println!(
            "{}",
            qcode_i18n::trp(
                "cli.cfg.append_cancelled",
                &[path.display().to_string().as_str()]
            )
        );
        return Ok(ExitCode::SUCCESS);
    }
    std::fs::write(path, &combined)?;
    println!(
        "{}",
        qcode_i18n::trp(
            "cli.cfg.append_done",
            &[id.as_str(), path.display().to_string().as_str()]
        )
    );
    print_key_hints(template);
    Ok(ExitCode::SUCCESS)
}

/// Append `section` to `existing` at text level and check the result still
/// loads.
///
/// Text-level, not parsed-merge: re-serialising the whole document would strip
/// every comment in the user's config, and comments are half of what a template
/// is for. The parse-and-validate step is what keeps an append from producing
/// a file that stops loading — a config that broke is worse than no config.
fn combine_config(existing: &str, section: &str) -> Result<String, String> {
    let mut combined = existing.to_string();
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push('\n');
    combined.push_str(section);
    let check: qcode_kernel::config::Config = toml::from_str(&combined)
        .map_err(|error| format!("the appended config does not parse: {error}"))?;
    check
        .sandbox
        .validate()
        .map_err(|error| error.to_string())?;
    check.mcp.validate().map_err(|error| error.to_string())?;
    Ok(combined)
}

/// y/N confirmation on stderr, so the question survives a `> out.log` run
/// (same reasoning as `qc update`'s prompt, ADR-0078). Declining is not an
/// error — it just leaves the file alone.
fn confirm_append() -> Result<bool, Box<dyn std::error::Error>> {
    use std::io::{IsTerminal, Write as _};
    if !std::io::stdin().is_terminal() {
        return Err(qcode_i18n::tr("err.cli.settings.append_needs_terminal")
            .into_owned()
            .into());
    }
    eprint!("append? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Print where each secret prompt's key should come from. The generated file
/// only carries the environment variable name; this is the step that tells the
/// user where to put the value (ADR-0104 §4).
fn print_key_hints(template: &qcode_kernel::template::Template) {
    use qcode_kernel::template::PromptKind;
    for prompt in template
        .prompts
        .iter()
        .filter(|prompt| prompt.kind == PromptKind::Secret)
    {
        let env = prompt.env.as_deref().unwrap_or("(none)");
        println!("{}", qcode_i18n::trp("cli.cfg.key_hint", &[env]));
    }
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
            other => {
                return Err(qcode_i18n::trp(
                    "err.cli.template.unknown_kind",
                    &[format!("{other:?}").as_str()],
                ));
            }
        };
        Some((kind, id))
    } else {
        None
    };
    if let Some((kind, id)) = prefixed {
        if id.is_empty() {
            return Err(qcode_i18n::trp(
                "err.cli.settings.template_name_empty",
                &[format!("{arg:?}").as_str()],
            ));
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
                return Err(qcode_i18n::trp(
                    "err.cli.template.ambiguous_kind",
                    &[format!("{arg:?}").as_str(), arg],
                ));
            }
            found = Some(template.kind);
        }
    }
    match found {
        Some(kind) => Ok((kind, arg.to_string())),
        None => Err(qcode_i18n::trp(
            "err.cli.template.no_such_template",
            &[format!("{arg:?}").as_str()],
        )),
    }
}

/// `qc template list` (ADR-0104 §2): every embedded template, what it is, and
/// whether it applies to this platform.
///
/// Source is `built-in` for now; user templates are a later decision, but the
/// label is printed so the column exists when they arrive.
pub(crate) fn cmd_template(args: TemplateArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
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
                    qcode_i18n::tr("cli.template.applies").into_owned()
                } else {
                    qcode_i18n::tr("cli.template.not_for_platform").into_owned()
                };
                println!(
                    "{}",
                    qcode_i18n::trp(
                        "cli.template.line",
                        &[
                            format!("{:<10}", template.kind.as_str()).as_str(),
                            format!("{:<14}", template.id).as_str(),
                            template.title.as_str(),
                            os.as_str(),
                            applies.as_str(),
                        ]
                    )
                );
                println!("    {}", template.summary);
            }
            println!(
                "\n{}",
                qcode_i18n::trp(
                    "cli.template.count",
                    &[templates.len().to_string().as_str()]
                )
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The `llm/deepseek` template's rendered document, prompts answered
    /// (it only asks for a secret, which is never written, so no answers are
    /// needed).
    fn deepseek_section() -> String {
        let template =
            qcode_kernel::template::find(qcode_kernel::template::Kind::Llm, "deepseek").unwrap();
        template.substitute(&HashMap::new()).unwrap()
    }

    #[test]
    fn combine_config_appends_and_loads() {
        let existing = "schema = 2\n[sandbox]\nmode = \"none\"\n";
        let section = format!("# Added by test.\n{}\n", deepseek_section());
        let combined = combine_config(existing, &section).unwrap();
        let parsed: qcode_kernel::config::Config = toml::from_str(&combined).unwrap();
        assert!(parsed.providers.contains_key("deepseek"));
        // The original content is preserved verbatim: an append must not
        // re-serialize or rewrite anything the user wrote.
        assert!(combined.starts_with(existing), "{combined}");
    }

    #[test]
    fn combine_config_refuses_a_duplicate_route() {
        // TOML forbids two `[providers.deepseek]` tables, so the same route in
        // both halves must fail the combined parse — the append path's guard
        // against silently rewriting a route the user already configured.
        let existing = "schema = 2\n[providers.deepseek]\napi_key_env = \"X\"\n";
        let error = combine_config(existing, &deepseek_section()).unwrap_err();
        assert!(error.contains("does not parse"), "{error}");
    }

    #[test]
    fn combine_config_refuses_a_broken_existing() {
        let existing = "schema = 2\n[broken\n";
        let error = combine_config(existing, &deepseek_section()).unwrap_err();
        assert!(error.contains("does not parse"), "{error}");
    }

    #[test]
    fn provider_append_refuses_an_existing_route() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "schema = 2\n[providers.deepseek]\napi_key_env = \"X\"\n",
        )
        .unwrap();
        let template =
            qcode_kernel::template::find(qcode_kernel::template::Kind::Llm, "deepseek").unwrap();
        let error =
            provider_append(&template, &deepseek_section(), &path, || Ok(true)).unwrap_err();
        assert!(
            format!("{error}").contains("already has a [providers.deepseek]"),
            "{error}"
        );
    }

    #[test]
    fn provider_append_decline_leaves_the_file_unchanged() {
        // The confirmation decision is injected, so this works without a
        // terminal (a real run passes `confirm_append`, which refuses to even
        // ask on a non-TTY).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "schema = 2\n[sandbox]\nmode = \"none\"\n";
        std::fs::write(&path, original).unwrap();
        let template =
            qcode_kernel::template::find(qcode_kernel::template::Kind::Llm, "deepseek").unwrap();
        let result = provider_append(&template, &deepseek_section(), &path, || Ok(false));
        assert!(result.is_ok(), "declining is not an error: {result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
