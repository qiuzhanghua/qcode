use super::load::{read_document, write_document};
use super::*;

/// Append one `[[mcp.servers]]` entry to the config file.
pub fn mcp_add(
    path: &Path,
    name: &str,
    command: &str,
    args: &[String],
    enabled: bool,
    timeout_secs: Option<u64>,
) -> Result<(), ConfigError> {
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
    server.insert(
        "command".to_string(),
        toml::Value::String(command.to_string()),
    );
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
