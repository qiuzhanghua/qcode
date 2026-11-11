use super::load::{read_document, write_document};
use super::*;

/// One `qc mcp add` invocation. A struct because every field is a string or a
/// list of strings, and six positional arguments is where the order starts
/// being wrong silently.
#[derive(Debug, Default)]
pub struct NewMcpServer<'a> {
    pub name: &'a str,
    /// Stdio transport. Exactly one of `command` and `url` is set; which one
    /// decides the transport (ADR-0067 §1).
    pub command: Option<&'a str>,
    /// Streamable HTTP transport.
    pub url: Option<&'a str>,
    /// Name of the environment variable holding the bearer token.
    pub bearer_token_env_var: Option<&'a str>,
    /// Extra request headers, already parsed into name/value pairs.
    pub headers: &'a [(String, String)],
    pub args: &'a [String],
    pub enabled: bool,
    pub timeout_secs: Option<u64>,
    /// ADR-0066 grant extras.
    pub read_paths: &'a [PathBuf],
    pub writable_paths: &'a [PathBuf],
    pub credentials: &'a [qcode_mcp::CredentialPreset],
    pub sandbox: Option<qcode_mcp::McpSandboxMode>,
}

/// Append one `[[mcp.servers]]` entry to the config file.
pub fn mcp_add(path: &Path, new: NewMcpServer<'_>) -> Result<(), ConfigError> {
    let NewMcpServer {
        name,
        command,
        url,
        bearer_token_env_var,
        headers,
        args,
        enabled,
        timeout_secs,
        read_paths,
        writable_paths,
        credentials,
        sandbox,
    } = new;
    let mut document = read_document(path)?;
    let root = document
        .as_table_mut()
        .ok_or_else(|| ConfigError::InvalidDocument("root must be a table".to_string()))?;
    let mcp = root
        .entry("mcp")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| ConfigError::InvalidDocument("mcp must be a table".to_string()))?;
    let servers = mcp
        .entry("servers")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| ConfigError::InvalidDocument("mcp.servers must be an array".to_string()))?;

    if servers
        .iter()
        .any(|server| server.get("name").and_then(toml::Value::as_str) == Some(name))
    {
        return Err(ConfigError::McpExists(name.to_string()));
    }

    let mut server = toml::Table::new();
    server.insert("name".to_string(), toml::Value::String(name.to_string()));
    // Only the field that was given, so the file reads the way the user would
    // have written it and `validate()` sees exactly one transport.
    if let Some(command) = command {
        server.insert(
            "command".to_string(),
            toml::Value::String(command.to_string()),
        );
    }
    if let Some(url) = url {
        server.insert("url".to_string(), toml::Value::String(url.to_string()));
    }
    if let Some(var) = bearer_token_env_var {
        server.insert(
            "bearer_token_env_var".to_string(),
            toml::Value::String(var.to_string()),
        );
    }
    if !headers.is_empty() {
        let mut table = toml::Table::new();
        for (key, value) in headers {
            table.insert(key.clone(), toml::Value::String(value.clone()));
        }
        server.insert("headers".to_string(), toml::Value::Table(table));
    }
    if !args.is_empty() {
        server.insert(
            "args".to_string(),
            toml::Value::Array(
                args.iter()
                    .map(|arg| toml::Value::String(arg.clone()))
                    .collect(),
            ),
        );
    }
    if !enabled {
        server.insert("enabled".to_string(), toml::Value::Boolean(false));
    }
    if let Some(seconds) = timeout_secs {
        server.insert(
            "timeout_secs".to_string(),
            toml::Value::Integer(seconds as i64),
        );
    }
    for (key, paths) in [
        ("read_paths", read_paths),
        ("writable_paths", writable_paths),
    ] {
        if paths.is_empty() {
            continue;
        }
        server.insert(
            key.to_string(),
            toml::Value::Array(
                paths
                    .iter()
                    .map(|path| toml::Value::String(path.display().to_string()))
                    .collect(),
            ),
        );
    }
    if !credentials.is_empty() {
        server.insert(
            "credentials".to_string(),
            toml::Value::Array(
                credentials
                    .iter()
                    .map(|preset| toml::Value::String(preset.label().to_string()))
                    .collect(),
            ),
        );
    }
    if let Some(mode) = sandbox {
        server.insert(
            "sandbox".to_string(),
            toml::Value::String(mode.label().to_string()),
        );
    }
    servers.push(toml::Value::Table(server));

    validate_document(&document)?;
    write_document(path, &document)
}

/// Remove `[[mcp.servers]]` by name.
pub fn mcp_remove(path: &Path, name: &str) -> Result<(), ConfigError> {
    let mut document = read_document(path)?;
    let servers = document
        .get_mut("mcp")
        .and_then(|mcp| mcp.get_mut("servers"))
        .and_then(toml::Value::as_array_mut)
        .ok_or_else(|| ConfigError::McpMissing(name.to_string()))?;
    let before = servers.len();
    servers.retain(|server| server.get("name").and_then(toml::Value::as_str) != Some(name));
    if servers.len() == before {
        return Err(ConfigError::McpMissing(name.to_string()));
    }
    validate_document(&document)?;
    write_document(path, &document)
}
