//! Read-modify-write a config file without losing comments or unknown keys.
//!
//! The settings UI and the first-run wizard edit the same document. Values are
//! written through `toml_edit`, validated by loading the temporary file, then
//! atomically renamed into place. Existing files get a `.bak` sibling.

use std::io::Write;
use std::path::{Path, PathBuf};

use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::config::ConfigError;

/// A config document loaded from disk, or an empty one when the file is new.
#[derive(Debug)]
pub struct ConfigEdit {
    path: PathBuf,
    document: DocumentMut,
    existed: bool,
}

impl ConfigEdit {
    /// Start with an empty document, keeping the path for save/backup.
    ///
    /// Used by the repair flow when the existing file cannot be parsed: saving
    /// still backs it up to `.bak` before replacing it.
    pub fn empty(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let existed = path.exists();
        Self {
            path,
            document: DocumentMut::new(),
            existed,
        }
    }

    /// Open `path`. A missing file yields an empty document with `existed =
    /// false`; the caller decides which defaults/template to add.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self {
                path,
                document: DocumentMut::new(),
                existed: false,
            });
        }
        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let document = text.parse::<DocumentMut>().map_err(|error| {
            ConfigError::InvalidDocument(format!("{}: {error}", path.display()))
        })?;
        Ok(Self {
            path,
            document,
            existed: true,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn existed(&self) -> bool {
        self.existed
    }

    pub fn document(&self) -> &DocumentMut {
        &self.document
    }

    pub fn document_mut(&mut self) -> &mut DocumentMut {
        &mut self.document
    }

    pub fn text(&self) -> String {
        self.document.to_string()
    }

    /// Insert or replace a scalar at `path` (all but the last segment are
    /// tables).
    pub fn set_value(&mut self, path: &[&str], value: Value) -> Result<(), ConfigError> {
        let (parents, key) = split_path(path)?;
        let table = table_mut(self.document.as_table_mut(), parents)?;
        table.insert(key, Item::Value(value));
        Ok(())
    }

    pub fn set_str(&mut self, path: &[&str], value: impl Into<String>) -> Result<(), ConfigError> {
        self.set_value(path, Value::from(value.into()))
    }

    /// Set a string, or remove the key when `value` is `None`.
    pub fn set_optional_str(
        &mut self,
        path: &[&str],
        value: Option<impl Into<String>>,
    ) -> Result<(), ConfigError> {
        match value {
            Some(value) => self.set_str(path, value),
            None => {
                self.remove(path);
                Ok(())
            }
        }
    }

    pub fn set_bool(&mut self, path: &[&str], value: bool) -> Result<(), ConfigError> {
        self.set_value(path, Value::from(value))
    }

    pub fn set_integer(&mut self, path: &[&str], value: i64) -> Result<(), ConfigError> {
        self.set_value(path, Value::from(value))
    }

    pub fn set_float(&mut self, path: &[&str], value: f64) -> Result<(), ConfigError> {
        self.set_value(path, Value::from(value))
    }

    pub fn set_string_array(
        &mut self,
        path: &[&str],
        values: impl IntoIterator<Item = String>,
    ) -> Result<(), ConfigError> {
        let array = values.into_iter().map(Value::from).collect::<Array>();
        self.set_value(path, Value::Array(array))
    }

    /// Replace an array-of-tables section (approval rules, MCP servers).
    pub fn set_array_of_tables(
        &mut self,
        path: &[&str],
        tables: Vec<Table>,
    ) -> Result<(), ConfigError> {
        let (parents, key) = split_path(path)?;
        let table = table_mut(self.document.as_table_mut(), parents)?;
        let mut array = ArrayOfTables::new();
        for table in tables {
            array.push(table);
        }
        table.insert(key, Item::ArrayOfTables(array));
        Ok(())
    }

    /// Return the mutable table at `path`, creating it when missing.
    pub fn table_mut(&mut self, path: &[&str]) -> Result<&mut Table, ConfigError> {
        table_mut(self.document.as_table_mut(), path)
    }

    /// Remove `path`; a missing path is a no-op.
    pub fn remove(&mut self, path: &[&str]) {
        let Ok((parents, key)) = split_path(path) else {
            return;
        };
        if let Some(table) = table_opt(self.document.as_table_mut(), parents) {
            table.remove(key);
        }
    }

    /// Validate and atomically persist the document.
    ///
    /// The temporary file is parsed by [`config::load`] before the rename, so a
    /// form bug cannot leave an unusable config behind.
    pub fn save(&self) -> Result<(), ConfigError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Read {
            path: parent.to_path_buf(),
            source,
        })?;

        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml");
        let temp = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        // Name the failing step: "cannot read" was reported for a create that
        // was denied, which sent the search in the wrong direction.
        // Report the directory, not the temp file: the failure is "this process
        // may not create files here", and the temp name made it look like a read
        // of a file that never existed.
        let mut file = options.open(&temp).map_err(|source| ConfigError::Read {
            path: parent.to_path_buf(),
            source: std::io::Error::new(
                source.kind(),
                format!(
                    "cannot create a temporary file ({source}); if Windows Defender \
                         Controlled Folder Access is on, allow qc.exe or point QCODE_CONFIG \
                         at a directory you own"
                ),
            ),
        })?;

        let text = self.document.to_string();
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|source| ConfigError::Read {
                path: temp.clone(),
                source,
            })?;
        drop(file);

        // Validate the text we just wrote instead of reading the temp file
        // back: on Windows a shell that runs qc inside a confinement layer can
        // create the file but not read it, which made every save fail with
        // "cannot read config ...tmp-<pid>: access denied".
        if let Err(error) = toml::from_str::<crate::config::Config>(&text) {
            let _ = std::fs::remove_file(&temp);
            return Err(ConfigError::Read {
                path: temp,
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()),
            });
        }

        if self.path.exists() {
            let backup = parent.join(format!("{file_name}.bak"));
            std::fs::copy(&self.path, &backup).map_err(|source| ConfigError::Read {
                path: backup,
                source,
            })?;
            #[cfg(windows)]
            {
                std::fs::remove_file(&self.path).map_err(|source| ConfigError::Read {
                    path: self.path.clone(),
                    source,
                })?;
            }
        }

        std::fs::rename(&temp, &self.path).map_err(|source| ConfigError::Read {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }
}

fn split_path<'a>(path: &'a [&'a str]) -> Result<(&'a [&'a str], &'a str), ConfigError> {
    let (key, parents) = path
        .split_last()
        .ok_or_else(|| ConfigError::InvalidDocument("empty config path".to_string()))?;
    Ok((parents, key))
}

fn table_mut<'a>(root: &'a mut Table, path: &[&str]) -> Result<&'a mut Table, ConfigError> {
    let mut table = root;
    for key in path {
        if !table.contains_key(key) {
            table.insert(key, Item::Table(Table::new()));
        }
        let item = table
            .get_mut(key)
            .ok_or_else(|| ConfigError::InvalidDocument(format!("missing table {key}")))?;
        table = item.as_table_mut().ok_or_else(|| {
            ConfigError::InvalidDocument(format!("'{key}' is not a table in the config"))
        })?;
    }
    Ok(table)
}

fn table_opt<'a>(root: &'a mut Table, path: &[&str]) -> Option<&'a mut Table> {
    let mut table = root;
    for key in path {
        table = table.get_mut(key)?.as_table_mut()?;
    }
    Some(table)
}

impl ConfigEdit {
    /// Write a complete [`Config`] into the document.
    ///
    /// Scalar sections are updated key by key, so comments around them survive.
    /// Dynamic maps (providers, pricing) and arrays (approval rules, MCP
    /// servers) are replaced as a unit; comments inside those tables are not
    /// preserved. Unknown sections are never touched.
    pub fn apply_config(&mut self, config: &crate::config::Config) -> Result<(), ConfigError> {
        self.set_integer(&["version"], i64::from(config.version))?;
        self.set_optional_str(&["default_provider"], config.default_provider.clone())?;

        self.set_integer(
            &["context", "window_tokens"],
            config.context.window_tokens as i64,
        )?;
        self.set_integer(&["agent", "max_turns"], i64::from(config.agent.max_turns))?;
        self.set_optional_float(&["agent", "max_cost_usd"], config.agent.max_cost_usd)?;
        self.set_bool(&["agent", "snapshot_shell"], config.agent.snapshot_shell)?;

        self.set_str(&["sandbox", "mode"], sandbox_mode(&config.sandbox.mode))?;
        self.set_bool(&["sandbox", "network"], config.sandbox.network)?;
        self.set_string_array(
            &["sandbox", "extra_writable"],
            config
                .sandbox
                .extra_writable
                .iter()
                .map(|path| path.display().to_string()),
        )?;

        self.set_str(&["network", "mode"], network_mode(&config.network.mode))?;
        self.set_bool(&["network", "enabled"], config.network.enabled)?;
        self.set_string_array(&["network", "allow"], config.network.allow.clone())?;
        self.set_bool(&["network", "allow_private"], config.network.allow_private)?;
        self.set_str(
            &["network", "unknown"],
            unknown_host(&config.network.unknown),
        )?;
        self.set_bool(&["network", "audit"], config.network.audit)?;

        self.set_bool(&["skills", "enabled"], config.skills.enabled)?;
        self.set_string_array(
            &["skills", "roots"],
            config
                .skills
                .roots
                .iter()
                .map(|path| path.display().to_string()),
        )?;

        self.set_bool(&["search", "enabled"], config.search.enabled)?;
        self.set_str(&["search", "backend"], config.search.backend.name())?;
        self.set_str(
            &["search", "api_key_env"],
            config.search.api_key_env.clone(),
        )?;
        self.set_optional_argv(
            &["search", "api_key_command"],
            config.search.api_key_command.clone(),
        )?;
        self.set_integer(
            &["search", "max_results"],
            i64::from(config.search.max_results),
        )?;
        self.set_optional_str(&["search", "base_url"], config.search.base_url.clone())?;

        self.set_bool(&["update", "enabled"], config.update.enabled)?;
        self.set_bool(&["update", "check"], config.update.check)?;
        self.set_integer(&["update", "check_days"], config.update.check_days as i64)?;
        self.set_str(&["update", "repo"], config.update.repo.clone())?;
        self.set_optional_str(&["update", "base_url"], config.update.base_url.clone())?;

        let mut providers = Table::new();
        for (name, provider) in &config.providers {
            let mut table = Table::new();
            set_optional(&mut table, "base_url", provider.base_url.clone());
            set_optional(&mut table, "api_key_env", provider.api_key_env.clone());
            if let Some(argv) = &provider.api_key_command {
                table.insert("api_key_command", Item::Value(argv_value(argv)));
            }
            set_optional(&mut table, "default_model", provider.default_model.clone());
            table.insert(
                "stream_usage",
                Item::Value(Value::from(provider.stream_usage)),
            );
            providers.insert(name, Item::Table(table));
        }
        self.replace_table(&["providers"], providers)?;

        let mut pricing = Table::new();
        for (model, price) in &config.pricing {
            let mut table = Table::new();
            table.insert(
                "input_usd_per_mtok",
                Item::Value(Value::from(price.input_usd_per_mtok)),
            );
            table.insert(
                "output_usd_per_mtok",
                Item::Value(Value::from(price.output_usd_per_mtok)),
            );
            table.insert(
                "cache_read_usd_per_mtok",
                Item::Value(Value::from(price.cache_read_usd_per_mtok)),
            );
            table.insert(
                "cache_write_usd_per_mtok",
                Item::Value(Value::from(price.cache_write_usd_per_mtok)),
            );
            pricing.insert(model, Item::Table(table));
        }
        self.replace_table(&["pricing"], pricing)?;

        let mut rules = Vec::new();
        for rule in &config.approval.rules {
            let mut table = Table::new();
            table.insert("id", Item::Value(Value::from(rule.id.clone())));
            set_optional(&mut table, "tool", rule.tool.clone());
            set_optional(&mut table, "command_prefix", rule.command_prefix.clone());
            set_optional(&mut table, "path_glob", rule.path_glob.clone());
            table.insert("action", Item::Value(Value::from(action(&rule.action))));
            if let Some(scope) = rule.scope {
                table.insert("scope", Item::Value(Value::from(scope_name(scope))));
            }
            rules.push(table);
        }
        self.set_array_of_tables(&["approval", "rules"], rules)?;

        let mut servers = Vec::new();
        for server in &config.mcp.servers {
            let mut table = Table::new();
            table.insert("name", Item::Value(Value::from(server.name.clone())));
            table.insert("command", Item::Value(Value::from(server.command.clone())));
            table.insert("args", Item::Value(argv_value(&server.args)));
            if !server.env.is_empty() {
                let mut env = Table::new();
                for (key, value) in &server.env {
                    env.insert(key, Item::Value(Value::from(value.clone())));
                }
                table.insert("env", Item::Table(env));
            }
            table.insert("enabled", Item::Value(Value::from(server.enabled)));
            table.insert(
                "timeout_secs",
                Item::Value(Value::from(server.timeout_secs as i64)),
            );
            servers.push(table);
        }
        self.set_array_of_tables(&["mcp", "servers"], servers)?;
        Ok(())
    }

    fn set_optional_float(&mut self, path: &[&str], value: Option<f64>) -> Result<(), ConfigError> {
        match value {
            Some(value) => self.set_float(path, value),
            None => {
                self.remove(path);
                Ok(())
            }
        }
    }

    fn set_optional_argv(
        &mut self,
        path: &[&str],
        value: Option<Vec<String>>,
    ) -> Result<(), ConfigError> {
        match value {
            Some(argv) => self.set_value(path, argv_value(&argv)),
            None => {
                self.remove(path);
                Ok(())
            }
        }
    }

    fn replace_table(&mut self, path: &[&str], table: Table) -> Result<(), ConfigError> {
        let (parents, key) = split_path(path)?;
        let parent = table_mut(self.document.as_table_mut(), parents)?;
        parent.insert(key, Item::Table(table));
        Ok(())
    }
}

fn set_optional(table: &mut Table, key: &str, value: Option<String>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        table.insert(key, Item::Value(Value::from(value)));
    }
}

fn argv_value(argv: &[String]) -> Value {
    Value::Array(argv.iter().cloned().map(Value::from).collect())
}

fn action(action: &qcode_core::policy::Action) -> &'static str {
    match action {
        qcode_core::policy::Action::Allow => "allow",
        qcode_core::policy::Action::Ask => "ask",
        qcode_core::policy::Action::Deny => "deny",
    }
}

fn scope_name(scope: qcode_core::event::Scope) -> &'static str {
    match scope {
        qcode_core::event::Scope::Session => "session",
        qcode_core::event::Scope::Project => "project",
        qcode_core::event::Scope::User => "user",
    }
}

fn sandbox_mode(mode: &qcode_tools::sandbox::SandboxMode) -> &'static str {
    mode.label()
}

fn network_mode(mode: &crate::config::NetworkMode) -> &'static str {
    match mode {
        crate::config::NetworkMode::Off => "off",
        crate::config::NetworkMode::Proxy => "proxy",
        crate::config::NetworkMode::Strict => "strict",
    }
}

fn unknown_host(policy: &crate::config::UnknownHost) -> &'static str {
    match policy {
        crate::config::UnknownHost::Ask => "ask",
        crate::config::UnknownHost::Deny => "deny",
        crate::config::UnknownHost::Allow => "allow",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn missing_file_is_created_with_defaults_and_private_mode() {
        let dir = temp();
        let path = dir.path().join("nested/config.toml");
        let mut edit = ConfigEdit::open(&path).expect("open");
        assert!(!edit.existed());
        edit.set_str(&["default_provider"], "deepseek")
            .expect("set");
        edit.set_str(
            &["providers", "deepseek", "default_model"],
            "deepseek-flash",
        )
        .expect("provider");
        edit.save().expect("save");

        let loaded = crate::config::load(&path).expect("valid config");
        assert_eq!(loaded.default_provider.as_deref(), Some("deepseek"));
        assert!(!edit.existed());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn save_preserves_comments_and_unknown_keys() {
        let dir = temp();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# keep me\nversion = 1\ndefault_provider = \"deepseek\"\n\n[unknown]\nkeep = true\n",
        )
        .expect("seed");

        let mut edit = ConfigEdit::open(&path).expect("open");
        edit.set_str(&["default_provider"], "openai").expect("set");
        edit.set_bool(&["agent", "snapshot_shell"], false)
            .expect("bool");
        edit.save().expect("save");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# keep me"), "{text}");
        assert!(text.contains("[unknown]"), "{text}");
        assert!(text.contains("keep = true"), "{text}");
        assert!(text.contains("default_provider = \"openai\""), "{text}");
        assert!(text.contains("snapshot_shell = false"), "{text}");
        let backup = std::fs::read_to_string(dir.path().join("config.toml.bak")).expect("backup");
        assert!(backup.contains("default_provider = \"deepseek\""));
    }

    #[test]
    fn invalid_document_is_not_written() {
        let dir = temp();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 1\ndefault_provider = \"deepseek\"\n").expect("seed");
        let mut edit = ConfigEdit::open(&path).expect("open");
        edit.set_integer(&["default_provider"], 3).expect("set");
        let error = edit.save().expect_err("must reject");
        assert!(!error.to_string().is_empty());
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("\"deepseek\""), "{text}");
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn apply_config_round_trips_all_sections() {
        use crate::config::{NetworkMode, ProviderConfig, UnknownHost};
        use qcode_core::cost::Pricing;
        use qcode_core::event::Scope;
        use qcode_core::policy::{Action, Rule};
        use qcode_mcp::McpServerConfig;
        use qcode_tools::sandbox::SandboxMode;
        use qcode_tools::web_search::SearchBackend;
        use std::collections::BTreeMap;

        let mut config = crate::config::Config::default();
        config.default_provider = Some("deepseek".to_string());
        config.providers.insert(
            "deepseek".to_string(),
            ProviderConfig {
                base_url: Some("https://api.deepseek.com/v1".to_string()),
                api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
                api_key_command: Some(vec!["echo".to_string(), "secret".to_string()]),
                default_model: Some("deepseek-flash".to_string()),
                stream_usage: false,
                extra_body: Default::default(),
            },
        );
        config.context.window_tokens = 64_000;
        config.agent.max_turns = 12;
        config.agent.max_cost_usd = Some(0.5);
        config.agent.snapshot_shell = false;
        config.sandbox.mode = SandboxMode::SandboxExec;
        config.sandbox.network = false;
        config.sandbox.extra_writable = vec![std::path::PathBuf::from("/tmp/extra")];
        config.network.mode = NetworkMode::Strict;
        config.network.allow = vec!["api.example.com:443".to_string()];
        config.network.allow_private = true;
        config.network.unknown = UnknownHost::Deny;
        config.network.audit = false;
        config.skills.enabled = false;
        config.skills.roots = vec![std::path::PathBuf::from("/skills")];
        config.search.enabled = true;
        config.search.backend = SearchBackend::Tavily;
        config.search.api_key_env = "SEARCH_KEY".to_string();
        config.search.api_key_command = Some(vec!["echo".to_string(), "search".to_string()]);
        config.search.max_results = 9;
        config.search.base_url = Some("https://search.example.com".to_string());
        config.update.enabled = false;
        config.update.check = false;
        config.update.check_days = 3;
        config.update.repo = "example/qcode".to_string();
        config.update.base_url = Some("https://updates.example.com".to_string());
        config.pricing.insert(
            "deepseek-flash".to_string(),
            Pricing {
                input_usd_per_mtok: 0.1,
                output_usd_per_mtok: 0.2,
                cache_read_usd_per_mtok: 0.01,
                cache_write_usd_per_mtok: 0.02,
            },
        );
        config.approval.rules.push(Rule {
            id: "allow-test".to_string(),
            tool: Some("run_shell".to_string()),
            command_prefix: Some("cargo test".to_string()),
            path_glob: Some("src/**".to_string()),
            action: Action::Allow,
            scope: Some(Scope::Project),
        });
        config.mcp.servers.push(McpServerConfig {
            name: "demo".to_string(),
            command: "demo-server".to_string(),
            args: vec!["--stdio".to_string()],
            env: BTreeMap::from([("TOKEN".to_string(), "x".to_string())]),
            enabled: false,
            timeout_secs: 7,
        });

        let dir = temp();
        let path = dir.path().join("config.toml");
        let mut edit = ConfigEdit::open(&path).expect("open");
        edit.apply_config(&config).expect("apply");
        edit.save().expect("save");
        let loaded = crate::config::load(&path).expect("valid");

        assert_eq!(loaded.default_provider.as_deref(), Some("deepseek"));
        assert_eq!(loaded.providers.len(), 1);
        let provider = &loaded.providers["deepseek"];
        assert_eq!(provider.default_model.as_deref(), Some("deepseek-flash"));
        assert!(!provider.stream_usage);
        assert_eq!(loaded.context.window_tokens, 64_000);
        assert_eq!(loaded.agent.max_turns, 12);
        assert_eq!(loaded.agent.max_cost_usd, Some(0.5));
        assert!(!loaded.agent.snapshot_shell);
        assert_eq!(loaded.sandbox.mode, SandboxMode::SandboxExec);
        assert_eq!(loaded.sandbox.extra_writable.len(), 1);
        assert_eq!(loaded.network.mode, NetworkMode::Strict);
        assert_eq!(loaded.network.allow, ["api.example.com:443"]);
        assert!(loaded.network.allow_private);
        assert_eq!(loaded.network.unknown, UnknownHost::Deny);
        assert!(!loaded.network.audit);
        assert!(!loaded.skills.enabled);
        assert_eq!(loaded.skills.roots.len(), 1);
        assert_eq!(loaded.search.backend, SearchBackend::Tavily);
        assert_eq!(
            loaded.search.api_key_command.as_deref(),
            Some(&["echo".to_string(), "search".to_string()][..])
        );
        assert_eq!(loaded.search.max_results, 9);
        assert_eq!(loaded.update.repo, "example/qcode");
        assert_eq!(loaded.pricing.len(), 1);
        assert_eq!(loaded.approval.rules.len(), 1);
        assert_eq!(loaded.approval.rules[0].action, Action::Allow);
        assert_eq!(loaded.approval.rules[0].scope, Some(Scope::Project));
        assert_eq!(loaded.mcp.servers.len(), 1);
        assert_eq!(loaded.mcp.servers[0].timeout_secs, 7);
        assert!(!loaded.mcp.servers[0].enabled);
    }

    #[test]
    fn arrays_and_removals_round_trip() {
        let dir = temp();
        let path = dir.path().join("config.toml");
        let mut edit = ConfigEdit::open(&path).expect("open");
        edit.set_str(&["default_provider"], "deepseek")
            .expect("provider");
        edit.set_string_array(
            &["network", "allow"],
            ["api.example.com".to_string(), "*.internal.test".to_string()],
        )
        .expect("allow");
        edit.set_bool(&["agent", "snapshot_shell"], true)
            .expect("bool");

        let mut rule = Table::new();
        rule.insert("id", Item::Value(Value::from("allow-shell")));
        rule.insert("tool", Item::Value(Value::from("run_shell")));
        rule.insert("action", Item::Value(Value::from("allow")));
        edit.set_array_of_tables(&["approval", "rules"], vec![rule])
            .expect("rules");
        edit.save().expect("save");

        let mut edit = ConfigEdit::open(&path).expect("reopen");
        edit.set_optional_str(&["default_provider"], None::<String>)
            .expect("remove");
        edit.remove(&["agent", "snapshot_shell"]);
        edit.save().expect("save again");
        let loaded = crate::config::load(&path).expect("valid");
        assert!(loaded.default_provider.is_none());
        assert!(loaded.agent.snapshot_shell);
        assert_eq!(loaded.network.allow.len(), 2);
        assert_eq!(loaded.approval.rules.len(), 1);
    }
}
