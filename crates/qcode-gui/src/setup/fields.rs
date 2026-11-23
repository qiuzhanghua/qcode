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
    // 保留软换行（长内容自动折行），但把续行缩进改为 None：编辑器默认
    // WrappingIndent::Same 会让前导空格把续行顶格区域占成「不可输入」的空白，
    // 看着像丢了内容。列表型输入（一行一条）用顶格续行最符合直觉。
    let input = cx.new(|cx| {
        TextareaState::new(window, cx)
            .placeholder(placeholder.to_string())
            .wrapping_indent(WrappingIndent::None)
    });
    input.update(cx, |input, cx| input.set_value(value, window, cx));
    input
}

/// Whether a table column holds a newline-joined list (one item per line) and
/// therefore needs a multi-line editor instead of a single-line [`InputState`].
pub(crate) fn is_lines_cell(key: &str) -> bool {
    matches!(
        key,
        "api_key_command"
            | "extra_body"
            | "args"
            | "env"
            | "read_paths"
            | "writable_paths"
            | "credentials"
    )
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
        ["sandbox", "network"] => config
            .sandbox
            .network_mode_with(&config.network)
            .label()
            .to_string(),
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
        ["sandbox", "extra_read"] => config
            .sandbox
            .extra_read
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        ["sandbox", "allow_pty"] => config.sandbox.allow_pty.to_string(),
        ["update", "enabled"] => config.update.enabled.to_string(),
        ["update", "check"] => config.update.check.to_string(),
        ["update", "check_days"] => config.update.check_days.to_string(),
        ["update", "repo"] => config.update.repo.clone(),
        ["update", "base_url"] => config.update.base_url.clone().unwrap_or_default(),
        ["terminal", "shell"] => config.terminal.shell.clone(),
        ["shell", "env_sampling"] => match config.shell.env_sampling {
            qcode_tools::shell_env::EnvSampling::Auto => "auto".to_string(),
            qcode_tools::shell_env::EnvSampling::Off => "off".to_string(),
        },
        ["shell", "env_command"] => config.shell.env_command.clone(),
        ["shell", "env_timeout_ms"] => config.shell.env_timeout_ms.to_string(),
        ["shell", "env_ttl_secs"] => config.shell.env_ttl_secs.to_string(),
        ["approval", "allow"] => config.approval.allow.join("\n"),
        ["approval", "deny"] => config.approval.deny.join("\n"),
        ["shell", "env_include"] => config.shell.env_include.join("\n"),
        ["shell", "env_exclude"] => config.shell.env_exclude.join("\n"),
        ["shell", "env_sample_sandboxed"] => config.shell.env_sample_sandboxed.to_string(),
        ["shell", "audit"] => match config.shell.audit {
            qcode_tools::shell_env::ShellAudit::Names => "names".to_string(),
            qcode_tools::shell_env::ShellAudit::Off => "off".to_string(),
        },
        ["terminal", "start_cwd"] => config.terminal.start_cwd.clone(),
        ["terminal", "font"] => config.terminal.font.clone(),
        ["terminal", "max_scrollback"] => config.terminal.max_scrollback.to_string(),
        ["terminal", "max_terminals"] => config.terminal.max_terminals.to_string(),
        // 这两条原来都落到 `_` 分支，于是下拉框永远显示 `options[0]`
        // （`queue` / `auto`）——即使配置写的是 `steer` / `zh-CN`。写回是对的、
        // 显示是错的，最坏的情况是「看一眼以为没生效，再点一下」。
        ["ui", "busy_delivery"] => match config.ui.busy_delivery {
            qcode_kernel::config::BusyDelivery::Queue => "queue".to_string(),
            qcode_kernel::config::BusyDelivery::Steer => "steer".to_string(),
        },
        ["ui", "language"] => config
            .ui
            .language
            .clone()
            .unwrap_or_else(|| "auto".to_string()),
        _ => options.first().unwrap_or(&"").to_string(),
    }
}

pub(crate) fn scalar_bool(config: &Config, path: &[&str]) -> bool {
    match path {
        ["agent", "snapshot_shell"] => config.agent.snapshot_shell,
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
        ["context", "window_tokens"] => apply_num(
            value,
            "context.window_tokens",
            &mut config.context.window_tokens,
        )?,
        ["agent", "max_turns"] => apply_num(value, "agent.max_turns", &mut config.agent.max_turns)?,
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
        ["terminal", "font"] => config.terminal.font = value.to_string(),
        ["shell", "env_sampling"] => {
            config.shell.env_sampling = match value {
                "off" => qcode_tools::shell_env::EnvSampling::Off,
                _ => qcode_tools::shell_env::EnvSampling::Auto,
            };
        }
        ["shell", "env_command"] => config.shell.env_command = value.to_string(),
        ["shell", "env_timeout_ms"] => apply_num(
            value,
            "shell.env_timeout_ms",
            &mut config.shell.env_timeout_ms,
        )?,
        ["shell", "env_ttl_secs"] => {
            apply_num(value, "shell.env_ttl_secs", &mut config.shell.env_ttl_secs)?
        }
        ["approval", "allow"] => config.approval.allow = lines_from(value),
        ["approval", "deny"] => config.approval.deny = lines_from(value),
        ["shell", "env_include"] => config.shell.env_include = lines_from(value),
        ["shell", "env_exclude"] => config.shell.env_exclude = lines_from(value),
        ["shell", "env_sample_sandboxed"] => config.shell.env_sample_sandboxed = value == "true",
        ["shell", "audit"] => {
            config.shell.audit = match value {
                "off" => qcode_tools::shell_env::ShellAudit::Off,
                _ => qcode_tools::shell_env::ShellAudit::Names,
            };
        }
        ["terminal", "max_scrollback"] => apply_num(
            value,
            "terminal.max_scrollback",
            &mut config.terminal.max_scrollback,
        )?,
        ["terminal", "max_terminals"] => apply_num(
            value,
            "terminal.max_terminals",
            &mut config.terminal.max_terminals,
        )?,
        ["ui", "busy_delivery"] => {
            config.ui.busy_delivery = match value {
                "steer" => qcode_kernel::config::BusyDelivery::Steer,
                _ => qcode_kernel::config::BusyDelivery::Queue,
            };
        }
        ["ui", "language"] => {
            // `auto` 写成 `None`：两者含义相同，但 `None` 让文件里干脆不出现这一行，
            // 与「没设置过」完全一致，也少一条要维护的零信息。
            config.ui.language = match value {
                "auto" | "" => None,
                other => Some(other.to_string()),
            };
        }
        ["sandbox", "mode"] => config.sandbox.mode = parse_sandbox(value)?,
        ["sandbox", "network"] => config.sandbox.network = Some(parse_sandbox_network(value)?),
        ["sandbox", "extra_writable"] => {
            config.sandbox.extra_writable = lines_from(value)
                .into_iter()
                .map(std::path::PathBuf::from)
                .collect();
        }
        ["sandbox", "extra_read"] => {
            config.sandbox.extra_read = lines_from(value)
                .into_iter()
                .map(std::path::PathBuf::from)
                .collect();
        }
        ["sandbox", "allow_pty"] => config.sandbox.allow_pty = value == "true",
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
            apply_num(value, "search.max_results", &mut config.search.max_results)?;
        }
        ["search", "base_url"] => {
            config.search.base_url = (!value.trim().is_empty()).then(|| value.trim().to_string());
        }
        ["update", "enabled"] => config.update.enabled = value == "true",
        ["update", "check"] => config.update.check = value == "true",
        ["update", "check_days"] => {
            apply_num(value, "update.check_days", &mut config.update.check_days)?
        }
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

/// 让 `[default-model].model` 跟着它所指的那个 provider 的 `default_model` 走。
///
/// 读配置时 `[default-model].model` **优先于** `providers.*.default_model`
/// （见 `config::resolve`，provider 名匹配时前者先被选中）。于是设置页里把
/// 默认 provider 的「Default model」改成别的值会变成一条假消息：保存成功、
/// 表单显示新值、下一次对话请求的还是旧模型。两条路只能有一个真话，这里让
/// provider 那一行当唯一入口，保存时把它同步进 `[default-model]`。
///
/// 只在行里有值时才同步。行被清空表示「交给目录默认」——此时
/// `[default-model].model` 是用户从模型菜单单独钉的，一次无关的表格自动保存
/// 不该顺手把它抹掉。
pub(crate) fn sync_default_model(
    default: &mut config::DefaultModelConfig,
    name: &str,
    model: Option<&str>,
) -> bool {
    let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) else {
        return false;
    };
    if default.provider.as_deref() != Some(name) || default.model.as_deref() == Some(model) {
        return false;
    }
    default.model = Some(model.to_string());
    true
}

/// 某一行的 `default_model` 是否被 `[default-model].model` 压住，返回压着它
/// 的那个模型名。
///
/// 只有「这一行就是默认 provider」时才可能被压：`config::resolve` 里
/// `[default-model].model` 只在 provider 名匹配时生效。
pub(crate) fn overridden_default_model<'a>(
    default: &'a config::DefaultModelConfig,
    name: &str,
    own: &str,
) -> Option<&'a str> {
    let global = default.model.as_deref()?;
    if default.provider.as_deref() != Some(name.trim()) {
        return None;
    }
    let own = own.trim();
    (!own.is_empty() && own != global).then_some(global)
}

/// The `images` override to write for one provider row (ADR-0071).
///
/// The switch has two states and the config has three (`Some(true)`,
/// `Some(false)`, and "inherit the catalog"). Writing `Some(false)` for a route
/// whose catalog says true would be right at that moment and wrong at the next
/// catalog update, and every save would rewrite the file; so the row only
/// becomes an override when the switch disagrees with what the route already
/// inherits. A route the catalog does not ship inherits "no images", which is
/// why the default is `false` and not `None`.
pub(crate) fn images_override(name: &str, shown: bool) -> Option<bool> {
    let inherited = qcode_kernel::config::catalog_entry(name.trim()).is_some_and(|e| e.images);
    (shown != inherited).then_some(shown)
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

/// 数值字段的容错应用：空值保留当前值（未配置的字段用默认，auto-save 不
/// 应因它失败），非空才解析；非法非空仍报错。
///
/// 历史教训：`scalar_string` 漏了 `terminal.max_scrollback` 等分支时播种为
/// 空串，`parse("")` 直接失败，导致**任何**表格保存（auto-save / 保存）都被
/// 拦下——新模型添加后写不进 config 就是这个原因。
fn apply_num<T: std::str::FromStr>(value: &str, label: &str, slot: &mut T) -> Result<(), String> {
    let value = value.trim();
    if !value.is_empty() {
        *slot = parse(value, label)?;
    }
    Ok(())
}

pub(crate) fn parse<T: std::str::FromStr>(value: &str, label: &str) -> Result<T, String> {
    value
        .trim()
        .parse()
        .map_err(|_| qcode_i18n::trp("gui.err.invalid_value", &[label.to_string()]))
}

pub(crate) fn parse_f64(
    row: &RowState,
    key: &'static str,
    cx: &Context<SetupView>,
) -> Result<f64, String> {
    parse(&cell(row, key, cx), key)
}

pub(crate) fn parse_sandbox_network(value: &str) -> Result<SandboxNetworkMode, String> {
    match value {
        "off" => Ok(SandboxNetworkMode::Off),
        "proxy" => Ok(SandboxNetworkMode::Proxy),
        "strict" => Ok(SandboxNetworkMode::Strict),
        "open" => Ok(SandboxNetworkMode::Open),
        other => Err(qcode_i18n::trp(
            "gui.err.unknown_network_mode",
            &[other.to_string()],
        )),
    }
}

pub(crate) fn parse_sandbox(value: &str) -> Result<SandboxMode, String> {
    match value {
        "none" => Ok(SandboxMode::None),
        "sandbox-exec" => Ok(SandboxMode::SandboxExec),
        "bwrap" => Ok(SandboxMode::Bwrap),
        "landlock" => Ok(SandboxMode::Landlock),
        "windows-acl" => Ok(SandboxMode::WindowsAcl),
        other => Err(qcode_i18n::trp(
            "gui.err.unknown_sandbox_mode",
            &[other.to_string()],
        )),
    }
}

pub(crate) fn parse_network(value: &str) -> Result<NetworkMode, String> {
    match value {
        "off" => Ok(NetworkMode::Off),
        "proxy" => Ok(NetworkMode::Proxy),
        "strict" => Ok(NetworkMode::Strict),
        other => Err(qcode_i18n::trp(
            "gui.err.unknown_network_mode",
            &[other.to_string()],
        )),
    }
}

pub(crate) fn parse_unknown(value: &str) -> Result<UnknownHost, String> {
    match value {
        "ask" => Ok(UnknownHost::Ask),
        "deny" => Ok(UnknownHost::Deny),
        "allow" => Ok(UnknownHost::Allow),
        other => Err(qcode_i18n::trp(
            "gui.err.unknown_network_policy",
            &[other.to_string()],
        )),
    }
}

pub(crate) fn parse_search(value: &str) -> Result<SearchBackend, String> {
    match value {
        "brave" => Ok(SearchBackend::Brave),
        "tavily" => Ok(SearchBackend::Tavily),
        "searxng" => Ok(SearchBackend::Searxng),
        other => Err(qcode_i18n::trp(
            "gui.err.unknown_search_backend",
            &[other.to_string()],
        )),
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

/// The MCP sandbox column. `inherit` (and anything unrecognised) becomes
/// `None`, which is what keeps a server following `[mcp] sandbox`: an explicit
/// `confine` written on every save would override a later global switch.
pub(crate) fn parse_sandbox_mode(value: Option<&str>) -> Option<qcode_mcp::McpSandboxMode> {
    value.and_then(qcode_mcp::McpSandboxMode::from_label)
}

/// The `credentials` column: one preset name per line. Names the loader does
/// not know are dropped with a note in the status line rather than written —
/// a config the settings screen produced must be a config that loads.
pub(crate) fn credential_presets(
    lines: &[String],
) -> (Vec<qcode_mcp::CredentialPreset>, Vec<String>) {
    let mut presets = Vec::new();
    let mut unknown = Vec::new();
    for line in lines {
        match qcode_mcp::CredentialPreset::from_label(line) {
            Some(preset) => {
                if !presets.contains(&preset) {
                    presets.push(preset);
                }
            }
            None => unknown.push(line.clone()),
        }
    }
    (presets, unknown)
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
        qcode_i18n::trp(
            "gui.err.keychain_write",
            &[credentials::macos::describe(status)],
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
        .map_err(|error| qcode_i18n::trp("gui.err.secret_tool", &[error.to_string()]))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| qcode_i18n::tr("gui.err.secret_tool_stdin").to_string())?
        .write_all(key.as_bytes())
        .map_err(|error| error.to_string())?;
    if !child.wait().map_err(|error| error.to_string())?.success() {
        return Err(qcode_i18n::tr("gui.err.secret_tool_store").to_string());
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
        .map_err(|error| qcode_i18n::trp("gui.err.powershell", &[error.to_string()]))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| qcode_i18n::tr("gui.err.powershell_stdin").to_string())?
        .write_all(key.as_bytes())
        .map_err(|error| error.to_string())?;
    if !child.wait().map_err(|error| error.to_string())?.success() {
        return Err(qcode_i18n::tr("gui.err.cred_write").to_string());
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
    Err(qcode_i18n::tr("gui.err.keychain_unimplemented").to_string())
}

/// The `api_key_command` argv that reads a stored key for `provider`, when one
/// exists — or `None`.
///
/// A provider's config section can be deleted while its key stays in the key
/// store (deleting a row does not remove the credential). Re-adding it from
/// the picker writes a fresh preset with only `api_key_env`, which orphans the
/// stored key: the profile reappears but cannot use it. The picker restores
/// the read command so the re-added provider works immediately. The argv
/// matches [`keychain_store`]'s, so the round-trip is the same command.
pub(crate) fn keychain_exists(provider: &str) -> Option<Vec<String>> {
    #[cfg(target_os = "macos")]
    {
        if credentials::macos::find("qcode", provider).is_ok() {
            return Some(vec![
                "security".to_string(),
                "find-generic-password".to_string(),
                "-w".to_string(),
                "-s".to_string(),
                "qcode".to_string(),
                "-a".to_string(),
                provider.to_string(),
            ]);
        }
        // Tail expression, not `return None;`: the latter trips `needless_return`
        // on whichever platform's block this is — and only on that one, since the
        // other `cfg` arms are not compiled here. That is how the Linux and
        // Windows arms went unnoticed until CI ran them.
        None
    }
    #[cfg(target_os = "linux")]
    {
        let output = std::process::Command::new("secret-tool")
            .args(["lookup", "service", "qcode", "user", provider])
            .output()
            .ok()?;
        if !output.stdout.is_empty() {
            return Some(vec![
                "secret-tool".to_string(),
                "lookup".to_string(),
                "service".to_string(),
                "qcode".to_string(),
                "user".to_string(),
                provider.to_string(),
            ]);
        }
        None
    }
    #[cfg(target_os = "windows")]
    {
        let script = credentials::windows_credential_read_script(&format!("qcode/{provider}"));
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .ok()?;
        if !String::from_utf8_lossy(&output.stdout).trim().is_empty() {
            return Some(vec![
                "powershell".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                script,
            ]);
        }
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
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
        .map_err(|error| qcode_i18n::trp("gui.err.open_editor", &[error.to_string()]))
}
