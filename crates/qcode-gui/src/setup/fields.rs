use super::*;

pub(crate) fn input(
    window: &mut Window,
    cx: &mut Context<SetupView>,
    value: String,
    placeholder: &str,
) -> Entity<InputState> {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_string()));
    input.update(cx, |input, cx| input.set_value(value, window, cx));
    input
}

/// A multi-line text input for newline-joined lists (one item per line).
///
/// A single-line [`InputState`] would silently strip `\n`/`\r` on insert
/// (gpui-base `normalize_input`) and glue the items together, corrupting the
/// value on the next save.
pub(crate) fn textarea(
    window: &mut Window,
    cx: &mut Context<SetupView>,
    value: String,
    placeholder: &str,
) -> Entity<TextareaState> {
    let input = cx.new(|cx| TextareaState::new(window, cx).placeholder(placeholder.to_string()));
    input.update(cx, |input, cx| input.set_value(value, window, cx));
    input
}

/// Whether a table column holds a newline-joined list (one item per line) and
/// therefore needs a multi-line editor instead of a single-line [`InputState`].
pub(crate) fn is_lines_cell(key: &str) -> bool {
    matches!(key, "api_key_command" | "extra_body" | "args" | "env")
}

pub(crate) fn secret_input(
    window: &mut Window,
    cx: &mut Context<SetupView>,
    placeholder: &str,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .masked(true)
            .placeholder(placeholder.to_string())
    })
}

pub(crate) fn scalar_string(config: &Config, path: &[&str], options: &[&str]) -> String {
    match path {
        ["context", "window_tokens"] => config.context.window_tokens.to_string(),
        ["agent", "max_turns"] => config.agent.max_turns.to_string(),
        ["agent", "max_cost_usd"] => config
            .agent
            .max_cost_usd
            .map(|value| value.to_string())
            .unwrap_or_default(),
        ["sandbox", "mode"] => config.sandbox.mode.label().to_string(),
        ["network", "mode"] => network_mode(&config.network.mode).to_string(),
        ["network", "allow"] => config.network.allow.join("\n"),
        ["network", "unknown"] => unknown_host(&config.network.unknown).to_string(),
        ["search", "backend"] => config.search.backend.name().to_string(),
        ["search", "api_key_env"] => config.search.api_key_env.clone(),
        ["search", "api_key_command"] => config
            .search
            .api_key_command
            .as_ref()
            .map(|argv| argv.join("\n"))
            .unwrap_or_default(),
        ["search", "max_results"] => config.search.max_results.to_string(),
        ["search", "base_url"] => config.search.base_url.clone().unwrap_or_default(),
        ["skills", "roots"] => config
            .skills
            .roots
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        ["sandbox", "extra_writable"] => config
            .sandbox
            .extra_writable
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        ["update", "enabled"] => config.update.enabled.to_string(),
        ["update", "check"] => config.update.check.to_string(),
        ["update", "check_days"] => config.update.check_days.to_string(),
        ["update", "repo"] => config.update.repo.clone(),
        ["update", "base_url"] => config.update.base_url.clone().unwrap_or_default(),
        _ => options.first().unwrap_or(&"").to_string(),
    }
}

pub(crate) fn scalar_bool(config: &Config, path: &[&str]) -> bool {
    match path {
        ["agent", "snapshot_shell"] => config.agent.snapshot_shell,
        ["sandbox", "network"] => config.sandbox.network,
        ["network", "enabled"] => config.network.enabled,
        ["network", "allow_private"] => config.network.allow_private,
        ["network", "audit"] => config.network.audit,
        ["search", "enabled"] => config.search.enabled,
        ["skills", "enabled"] => config.skills.enabled,
        ["update", "enabled"] => config.update.enabled,
        ["update", "check"] => config.update.check,
        _ => false,
    }
}

pub(crate) fn apply_scalar(config: &mut Config, path: &[&str], value: &str) -> Result<(), String> {
    match path {
        ["context", "window_tokens"] => {
            config.context.window_tokens = parse(value, "context.window_tokens")?;
        }
        ["agent", "max_turns"] => config.agent.max_turns = parse(value, "agent.max_turns")?,
        ["agent", "max_cost_usd"] => {
            config.agent.max_cost_usd = if value.trim().is_empty() {
                None
            } else {
                Some(parse(value, "agent.max_cost_usd")?)
            };
        }
        ["agent", "snapshot_shell"] => config.agent.snapshot_shell = value == "true",
        ["terminal", "shell"] => config.terminal.shell = value.to_string(),
        ["terminal", "start_cwd"] => config.terminal.start_cwd = value.to_string(),
        ["terminal", "max_scrollback"] => {
            config.terminal.max_scrollback = parse(value, "terminal.max_scrollback")?
        }
        ["terminal", "max_terminals"] => {
            config.terminal.max_terminals = parse(value, "terminal.max_terminals")?
        }
        ["ui", "busy_delivery"] => {
            config.ui.busy_delivery = match value {
                "steer" => qcode_kernel::config::BusyDelivery::Steer,
                _ => qcode_kernel::config::BusyDelivery::Queue,
            };
        }
        ["sandbox", "mode"] => config.sandbox.mode = parse_sandbox(value)?,
        ["sandbox", "network"] => config.sandbox.network = value == "true",
        ["sandbox", "extra_writable"] => {
            config.sandbox.extra_writable = lines_from(value)
                .into_iter()
                .map(std::path::PathBuf::from)
                .collect();
        }
        ["network", "mode"] => config.network.mode = parse_network(value)?,
        ["network", "enabled"] => config.network.enabled = value == "true",
        ["network", "allow"] => config.network.allow = lines_from(value),
        ["network", "allow_private"] => config.network.allow_private = value == "true",
        ["network", "unknown"] => config.network.unknown = parse_unknown(value)?,
        ["network", "audit"] => config.network.audit = value == "true",
        ["skills", "enabled"] => config.skills.enabled = value == "true",
        ["skills", "roots"] => {
            config.skills.roots = lines_from(value)
                .into_iter()
                .map(std::path::PathBuf::from)
                .collect()
        }
        ["search", "enabled"] => config.search.enabled = value == "true",
        ["search", "backend"] => config.search.backend = parse_search(value)?,
        ["search", "api_key_env"] => config.search.api_key_env = value.to_string(),
        ["search", "api_key_command"] => {
            let argv = lines_from(value);
            config.search.api_key_command = (!argv.is_empty()).then_some(argv);
        }
        ["search", "max_results"] => {
            config.search.max_results = parse(value, "search.max_results")?;
        }
        ["search", "base_url"] => {
            config.search.base_url = (!value.trim().is_empty()).then(|| value.trim().to_string());
        }
        ["update", "enabled"] => config.update.enabled = value == "true",
        ["update", "check"] => config.update.check = value == "true",
        ["update", "check_days"] => config.update.check_days = parse(value, "update.check_days")?,
        ["update", "repo"] => config.update.repo = value.to_string(),
        ["update", "base_url"] => {
            config.update.base_url = (!value.trim().is_empty()).then(|| value.trim().to_string());
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn cell(row: &RowState, key: &'static str, cx: &Context<SetupView>) -> String {
    if let Some(input) = row.cells.get(key) {
        return input.read(cx).value().to_string();
    }
    if let Some(textarea) = row.textareas.get(key) {
        return textarea.read(cx).value().to_string();
    }
    String::new()
}

/// Current text of a scalar field, whether it lives in a single-line input or
/// in a multi-line textarea (`FieldKind::Lines`).
pub(crate) fn scalar_value(view: &SetupView, key: &str, cx: &Context<SetupView>) -> String {
    if let Some(input) = view.inputs.get(key) {
        return input.read(cx).value().to_string();
    }
    if let Some(textarea) = view.textareas.get(key) {
        return textarea.read(cx).value().to_string();
    }
    String::new()
}

/// Drop a model that belongs to a different provider.
///
/// The form starts each row from a preset and lets the name be edited; a renamed
/// row keeps the old model ("deepseek-flash") and would otherwise save that
/// under, say, minimax. Dropping it lets the provider's own default apply.
pub(crate) fn stale_model_default(name: &str, model: Option<String>) -> Option<String> {
    let model = model?;
    let stale = (name != "mock" && model == "mock-1")
        || qcode_kernel::config::builtin_models()
            .into_iter()
            .any(|(other, default)| other != name && default == model);
    (!stale).then_some(model)
}

pub(crate) fn optional(
    row: &RowState,
    key: &'static str,
    cx: &Context<SetupView>,
) -> Option<String> {
    let value = cell(row, key, cx);
    (!value.trim().is_empty()).then(|| value.trim().to_string())
}

pub(crate) fn lines(row: &RowState, key: &'static str, cx: &Context<SetupView>) -> Vec<String> {
    lines_from(&cell(row, key, cx))
}

pub(crate) fn lines_from(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse a providers-table `Extra body` cell: one `key = value` per line,
/// split on the FIRST `=`. The value is JSON when it parses; otherwise it is
/// kept as a plain string so an edit never fails the save. Empty → empty map.
pub(crate) fn extra_body_from_lines(lines: &[String]) -> BTreeMap<String, serde_json::Value> {
    lines
        .iter()
        .filter_map(|line| {
            line.split_once('=').map(|(key, value)| {
                let key = key.trim();
                let value = value.trim();
                let parsed = serde_json::from_str(value)
                    .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));
                (key.to_string(), parsed)
            })
        })
        .filter(|(key, _)| !key.is_empty())
        .collect()
}

pub(crate) fn parse<T: std::str::FromStr>(value: &str, label: &str) -> Result<T, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("{label}: invalid value"))
}

pub(crate) fn parse_f64(
    row: &RowState,
    key: &'static str,
    cx: &Context<SetupView>,
) -> Result<f64, String> {
    parse(&cell(row, key, cx), key)
}

pub(crate) fn parse_sandbox(value: &str) -> Result<SandboxMode, String> {
    match value {
        "none" => Ok(SandboxMode::None),
        "sandbox-exec" => Ok(SandboxMode::SandboxExec),
        "bwrap" => Ok(SandboxMode::Bwrap),
        "landlock" => Ok(SandboxMode::Landlock),
        "windows-acl" => Ok(SandboxMode::WindowsAcl),
        other => Err(format!("unknown sandbox mode {other}")),
    }
}

pub(crate) fn parse_network(value: &str) -> Result<NetworkMode, String> {
    match value {
        "off" => Ok(NetworkMode::Off),
        "proxy" => Ok(NetworkMode::Proxy),
        "strict" => Ok(NetworkMode::Strict),
        other => Err(format!("unknown network mode {other}")),
    }
}

pub(crate) fn parse_unknown(value: &str) -> Result<UnknownHost, String> {
    match value {
        "ask" => Ok(UnknownHost::Ask),
        "deny" => Ok(UnknownHost::Deny),
        "allow" => Ok(UnknownHost::Allow),
        other => Err(format!("unknown network policy {other}")),
    }
}

pub(crate) fn parse_search(value: &str) -> Result<SearchBackend, String> {
    match value {
        "brave" => Ok(SearchBackend::Brave),
        "tavily" => Ok(SearchBackend::Tavily),
        "searxng" => Ok(SearchBackend::Searxng),
        other => Err(format!("unknown search backend {other}")),
    }
}

pub(crate) fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Allow => "allow",
        Action::Ask => "ask",
        Action::Deny => "deny",
    }
}

pub(crate) fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Session => "session",
        Scope::Project => "project",
        Scope::User => "user",
    }
}

pub(crate) fn parse_action(value: Option<&str>) -> Action {
    match value {
        Some("allow") => Action::Allow,
        Some("deny") => Action::Deny,
        _ => Action::Ask,
    }
}

pub(crate) fn parse_scope(value: Option<&str>) -> Scope {
    match value {
        Some("project") => Scope::Project,
        Some("user") => Scope::User,
        _ => Scope::Session,
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn keychain_store(provider: &str, key: &str) -> Result<Vec<String>, String> {
    // Written through the Security framework rather than `security
    // add-generic-password -w <key>`, which would put the secret in `argv`.
    credentials::macos::store("qcode", provider, key).map_err(|status| {
        format!(
            "keychain write failed ({})",
            credentials::macos::describe(status)
        )
    })?;
    Ok(vec![
        "security".to_string(),
        "find-generic-password".to_string(),
        "-w".to_string(),
        "-s".to_string(),
        "qcode".to_string(),
        "-a".to_string(),
        provider.to_string(),
    ])
}

#[cfg(target_os = "linux")]
pub(crate) fn keychain_store(provider: &str, key: &str) -> Result<Vec<String>, String> {
    use std::io::Write as _;

    let mut child = std::process::Command::new("secret-tool")
        .args([
            "store", "--label", "qcode", "service", "qcode", "user", provider,
        ])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("secret-tool: {error}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "secret-tool stdin unavailable".to_string())?
        .write_all(key.as_bytes())
        .map_err(|error| error.to_string())?;
    if !child.wait().map_err(|error| error.to_string())?.success() {
        return Err("secret-tool store failed".to_string());
    }
    Ok(vec![
        "secret-tool".to_string(),
        "lookup".to_string(),
        "service".to_string(),
        "qcode".to_string(),
        "user".to_string(),
        provider.to_string(),
    ])
}

#[cfg(target_os = "windows")]
pub(crate) fn keychain_store(provider: &str, key: &str) -> Result<Vec<String>, String> {
    use std::io::Write as _;

    // A generic credential (CRED_TYPE_GENERIC) keeps its secret in the blob,
    // so CredRead can hand it back; cmdkey's Domain Password form does not.
    // The secret goes in over stdin because `/pass:<key>` would be visible to
    // every process of this user through WMI's command line.
    let target = format!("qcode/{provider}");
    let mut child = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &credentials::windows_credential_write_script(&target, provider),
        ])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("powershell: {error}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "powershell stdin unavailable".to_string())?
        .write_all(key.as_bytes())
        .map_err(|error| error.to_string())?;
    if !child.wait().map_err(|error| error.to_string())?.success() {
        return Err("CredWrite failed".to_string());
    }
    Ok(vec![
        "powershell".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        credentials::windows_credential_read_script(&target),
    ])
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub(crate) fn keychain_store(_provider: &str, _key: &str) -> Result<Vec<String>, String> {
    Err("keychain setup is not implemented on this platform yet".to_string())
}

pub(crate) fn gui_string(prefs: &GuiPreferences, path: &[&str], options: &[&str]) -> String {
    match path {
        ["gui", "theme"] => prefs.theme.clone(),
        ["gui", "font_size"] => prefs.font_size.to_string(),
        ["gui", "code_font"] => prefs.code_font.clone(),
        ["gui", "density"] => prefs.density.clone(),
        ["gui", "send_mode"] => prefs.send_mode.clone(),
        _ => options.first().unwrap_or(&"").to_string(),
    }
}

pub(crate) fn network_mode(mode: &NetworkMode) -> &'static str {
    match mode {
        NetworkMode::Off => "off",
        NetworkMode::Proxy => "proxy",
        NetworkMode::Strict => "strict",
    }
}

pub(crate) fn unknown_host(policy: &UnknownHost) -> &'static str {
    match policy {
        UnknownHost::Ask => "ask",
        UnknownHost::Deny => "deny",
        UnknownHost::Allow => "allow",
    }
}

pub(crate) fn open_in_editor(path: &std::path::Path) -> Result<(), String> {
    let mut command = if let Ok(editor) = std::env::var("EDITOR") {
        let mut command = std::process::Command::new(editor);
        command.arg(path);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("open");
        command.arg("-e").arg(path);
        command
    } else {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot open editor: {error}"))
}
