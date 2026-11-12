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

    /// Write a list of paths, keeping the spelling the file already uses.
    ///
    /// [`Config`] holds **expanded** paths — `~/x` becomes `/home/q/x` at load
    /// time (ADR-0066 §2 修订注记 §2) — so writing `path.display()` back would
    /// freeze `writable_paths = ["~/Documents"]` into the absolute form the
    /// first time any setting is saved. One GUI save and the portability the
    /// tilde bought is gone, silently, in a file the user is not looking at.
    ///
    /// So: keep every existing literal whose expansion still equals the value
    /// being written, and only rewrite the ones that do not. "Did it change" is
    /// answered by expansion rather than by string equality, because the two
    /// spellings are the same path and the file's version is the better one.
    ///
    /// Matching is by value, not by index: reorder the list in the UI and the
    /// survivors keep their spelling. A literal is consumed when matched, so
    /// duplicates (`["~/a", "/home/q/a"]`) each keep their own form.
    fn set_path_array(&mut self, path: &[&str], values: &[PathBuf]) -> Result<(), ConfigError> {
        let available = self.existing_path_literals(path);
        self.set_string_array(path, spell_paths(values, available))
    }

    /// The string entries of the array at `path`, if it is one.
    ///
    /// Anything else — missing, a scalar, an array of non-strings — yields an
    /// empty list, so [`spell_paths`] falls back to writing the absolute form.
    /// That is the pre-fix behaviour, which is the right fallback: it never
    /// loses a path.
    fn existing_path_literals(&self, path: &[&str]) -> Vec<String> {
        let mut item = self.document.as_item();
        for key in path {
            let Some(next) = item.get(key) else {
                return Vec::new();
            };
            item = next;
        }
        array_strings(item)
    }

    /// The same literals, per `[[mcp.servers]]` name and key.
    ///
    /// `set_array_of_tables` replaces the whole array, so the server tables are
    /// rebuilt from `Config` and their `read_paths` / `writable_paths` would
    /// otherwise come back expanded. Read them out *before* that happens.
    fn existing_server_path_literals(
        &self,
        key: &str,
    ) -> std::collections::HashMap<String, Vec<String>> {
        let mut found = std::collections::HashMap::new();
        let Some(servers) = self
            .document
            .get("mcp")
            .and_then(|mcp| mcp.get("servers"))
            .and_then(|servers| servers.as_array_of_tables())
        else {
            return found;
        };
        for table in servers {
            let Some(name) = table.get("name").and_then(|name| name.as_str()) else {
                continue;
            };
            if let Some(item) = table.get(key) {
                found.insert(name.to_string(), array_strings(item));
            }
        }
        found
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
        // Some Windows setups let a process rewrite `config.toml` but deny
        // creating a new file beside it (third-party AV, a folder policy). The
        // atomic path is preferred; a direct write with a `.bak` first is better
        // than refusing to save.
        let mut file = match options.open(&temp) {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::PermissionDenied => {
                // The backup is also a new file, so it can be refused for the
                // same reason; a save without a backup beats no save at all.
                let backup = parent.join(format!("{file_name}.bak"));
                if self.path.exists()
                    && let Err(error) = std::fs::copy(&self.path, &backup)
                    && error.kind() != std::io::ErrorKind::PermissionDenied
                {
                    return Err(ConfigError::Read {
                        path: backup,
                        source: error,
                    });
                }
                let text = self.document.to_string();
                return std::fs::write(&self.path, text.as_bytes()).map_err(|source| {
                    ConfigError::Read {
                        path: self.path.clone(),
                        source,
                    }
                });
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: parent.to_path_buf(),
                    source: std::io::Error::new(
                        source.kind(),
                        format!("cannot create a temporary file ({source})"),
                    ),
                });
            }
        };

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

/// Respell `values` (absolute) using the literals `available` already holds.
///
/// A literal is kept when expanding it gives back the value being written, so
/// `~/Documents` survives a save as `~/Documents` instead of becoming
/// `/home/q/Documents`. Literals are consumed as they match, so a list holding
/// both spellings of the same path keeps both.
///
/// Expansion rather than string equality is the test because the two spellings
/// are the same path and the file's version is the better one: it is the one
/// that works on the next machine.
fn spell_paths(values: &[PathBuf], mut available: Vec<String>) -> Vec<String> {
    let home = crate::config::home_dir();
    values
        .iter()
        .map(|value| {
            let matched = available.iter().position(|literal| {
                crate::config::expand_tilde(Path::new(literal), home.as_deref()) == *value
            });
            match matched {
                Some(index) => available.remove(index),
                None => value.display().to_string(),
            }
        })
        .collect()
}

/// The string entries of an array item, or nothing when it is not one.
fn array_strings(item: &Item) -> Vec<String> {
    item.as_array()
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

impl ConfigEdit {
    /// Write a complete [`Config`] into the document.
    ///
    /// Scalar sections are updated key by key, so comments around them survive.
    /// Dynamic maps (providers, pricing) and arrays (approval rules, MCP
    /// servers) are replaced as a unit; comments inside those tables are not
    /// preserved. Unknown sections are never touched.
    pub fn apply_config(&mut self, config: &crate::config::Config) -> Result<(), ConfigError> {
        // ADR-0065: a file keeps the schema it was written with; only
        // `qc config migrate` upgrades it. Writing `schema` here would silently
        // jump a v1 file to v2 the first time a setting is saved.
        if self.document.get("schema").is_some() {
            self.set_integer(&["schema"], i64::from(config.schema))?;
        } else if self.document.get("version").is_some() {
            // Keep the spelling the file already uses; `version` is an alias.
            self.set_integer(&["version"], i64::from(config.schema))?;
        } else {
            // A new file speaks the current schema.
            self.set_integer(&["schema"], i64::from(config.schema))?;
        }
        self.set_optional_str(&["default_provider"], config.default_provider.clone())?;
        self.set_optional_str(
            &["default-model", "provider"],
            config.default_model.provider.clone(),
        )?;
        self.set_optional_str(
            &["default-model", "model"],
            config.default_model.model.clone(),
        )?;

        self.set_integer(
            &["context", "window_tokens"],
            config.context.window_tokens as i64,
        )?;
        self.set_integer(&["agent", "max_turns"], i64::from(config.agent.max_turns))?;
        self.set_optional_float(&["agent", "max_cost_usd"], config.agent.max_cost_usd)?;
        self.set_bool(&["agent", "snapshot_shell"], config.agent.snapshot_shell)?;
        self.set_str(
            &["ui", "busy_delivery"],
            busy_delivery_str(config.ui.busy_delivery),
        )?;

        self.set_str(&["sandbox", "mode"], sandbox_mode(&config.sandbox.mode))?;
        self.set_str(
            &["sandbox", "network"],
            config.sandbox.network_mode_with(&config.network).label(),
        )?;
        self.set_path_array(
            &["sandbox", "extra_writable"],
            &config.sandbox.extra_writable,
        )?;
        self.set_path_array(&["sandbox", "extra_read"], &config.sandbox.extra_read)?;

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
        self.set_path_array(&["skills", "roots"], &config.skills.roots)?;

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
            // `protocol` was never written here, so a route switched to the
            // Anthropic endpoint in the settings table came back as OpenAI after
            // the next save (the table is rebuilt, and an omitted key is the
            // same as "inherit the catalog").
            set_optional(
                &mut table,
                "protocol",
                provider
                    .protocol
                    .map(|protocol| protocol.as_str().to_string()),
            );
            set_optional_bool(&mut table, "images", provider.images);
            table.insert(
                "stream_usage",
                Item::Value(Value::from(provider.stream_usage)),
            );
            if !provider.models.is_empty() {
                // A bare id is all most files need; ADR-0065 lets that be a
                // string array, and writing `[[providers.x.models]]` tables for
                // one id is exactly the noise the schema change removes.
                let bare = provider.models.iter().all(|model| {
                    model.name.is_none()
                        && model.context_window.is_none()
                        && model.max_tokens.is_none()
                });
                if bare {
                    let mut ids = toml_edit::Array::new();
                    for model in &provider.models {
                        ids.push(model.id.clone());
                    }
                    table.insert("models", Item::Value(Value::Array(ids)));
                } else {
                    let mut models = ArrayOfTables::new();
                    for model in &provider.models {
                        let mut row = Table::new();
                        row.insert("id", Item::Value(Value::from(model.id.clone())));
                        set_optional(&mut row, "name", model.name.clone());
                        if let Some(context) = model.context_window {
                            row.insert("context_window", Item::Value(Value::from(context as i64)));
                        }
                        if let Some(max) = model.max_tokens {
                            row.insert("max_tokens", Item::Value(Value::from(max as i64)));
                        }
                        models.push(row);
                    }
                    table.insert("models", Item::ArrayOfTables(models));
                }
            }
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
        self.set_string_array(
            &["approval", "allow"],
            config.approval.allow.iter().cloned(),
        )?;
        self.set_string_array(&["approval", "deny"], config.approval.deny.iter().cloned())?;

        let mut servers = Vec::new();
        // Read the spellings out before the array is replaced below.
        let existing_read_paths = self.existing_server_path_literals("read_paths");
        let existing_writable_paths = self.existing_server_path_literals("writable_paths");
        for server in &config.mcp.servers {
            let mut table = Table::new();
            table.insert("name", Item::Value(Value::from(server.name.clone())));
            // Exactly one of these is set (ADR-0067 §1); writing the one that is
            // set and nothing otherwise is what keeps `apply_config` from
            // turning a remote server into a broken stdio one on save.
            set_optional(&mut table, "command", server.command.clone());
            set_optional(&mut table, "url", server.url.clone());
            set_optional(
                &mut table,
                "bearer_token_env_var",
                server.bearer_token_env_var.clone(),
            );
            if !server.headers.is_empty() {
                let mut headers = Table::new();
                for (name, value) in &server.headers {
                    headers.insert(name, Item::Value(Value::from(value.clone())));
                }
                table.insert("headers", Item::Table(headers));
            }
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
            // The ADR-0066 grant fields. `set_array_of_tables` replaces the whole
            // array, so anything left out here is dropped from the file on save —
            // including fields the user wrote by hand. The spellings are the
            // file's, not `Config`'s (see `spell_paths`).
            for (key, paths, existing) in [
                ("read_paths", &server.read_paths, &existing_read_paths),
                (
                    "writable_paths",
                    &server.writable_paths,
                    &existing_writable_paths,
                ),
            ] {
                if paths.is_empty() {
                    continue;
                }
                let available = existing.get(&server.name).cloned().unwrap_or_default();
                let spelled = spell_paths(paths, available);
                table.insert(
                    key,
                    Item::Value(Value::Array(spelled.into_iter().map(Value::from).collect())),
                );
            }
            if !server.credentials.is_empty() {
                table.insert(
                    "credentials",
                    Item::Value(Value::Array(
                        server
                            .credentials
                            .iter()
                            .map(|preset| Value::from(preset.label().to_string()))
                            .collect(),
                    )),
                );
            }
            if let Some(mode) = server.sandbox {
                table.insert("sandbox", Item::Value(Value::from(mode.label())));
            }
            servers.push(table);
        }
        // The crate-wide MCP sandbox default. Written only when it differs from
        // the default, so an untouched config does not grow a `[mcp]` shell —
        // same rule as `prune_empty_sections` (ADR-0065 决定 8).
        if config.mcp.sandbox == qcode_mcp::McpSandboxMode::default() {
            self.remove(&["mcp", "sandbox"]);
        } else {
            self.set_str(&["mcp", "sandbox"], config.mcp.sandbox.label())?;
        }
        self.set_array_of_tables(&["mcp", "servers"], servers)?;

        self.prune_empty_sections();
        Ok(())
    }

    /// Drop sections that ended up empty (ADR-0065 决定 8).
    ///
    /// A saved file should say what differs from the defaults. `set_*` writes
    /// unconditionally, so a config with no providers or no pricing would grow
    /// `[providers]` / `[pricing]` shells that mean nothing.
    fn prune_empty_sections(&mut self) {
        let empty_table = |document: &toml_edit::DocumentMut, name: &str| {
            document
                .get(name)
                .map(|item| match item {
                    Item::Table(table) => table.is_empty(),
                    _ => false,
                })
                .unwrap_or(false)
        };
        for section in ["providers", "pricing"] {
            if empty_table(&self.document, section) {
                self.document.remove(section);
            }
        }

        let approval = self
            .document
            .get("approval")
            .and_then(|item| item.as_table_like());
        if let Some(table) = approval {
            let rules_empty = table
                .get("rules")
                .and_then(|item| item.as_array_of_tables())
                .map(|rules| rules.is_empty())
                .unwrap_or(true);
            let list_empty = |name: &str| {
                table
                    .get(name)
                    .and_then(|item| item.as_array())
                    .map(|array| array.is_empty())
                    .unwrap_or(true)
            };
            if rules_empty && list_empty("allow") && list_empty("deny") {
                self.document.remove("approval");
            }
        }
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

/// Persist the global default model (`[default-model]`) — the GUI calls this
/// on "切即默认" (switching the session model also makes it the default).
///
/// A `None` value removes the key; a missing `[default-model]` table is
/// created on demand. The save goes through [`ConfigEdit::save`], so the
/// result is validated and written atomically.
pub fn set_default_model(
    path: &Path,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<(), ConfigError> {
    let mut edit = ConfigEdit::open(path)?;
    edit.set_optional_str(&["default-model", "provider"], provider)?;
    edit.set_optional_str(&["default-model", "model"], model)?;
    edit.save()
}

/// Write ONE scalar key into `path`, preserving comments and writing
/// atomically (`.bak`, validation — exactly like [`ConfigEdit::save`]).
///
/// `None` removes the key (an "unset" optional). The value string is typed by
/// inspection: `true`/`false` → bool, an integer literal → integer, a float
/// literal → float, anything else → a string. The array keys
/// (`sandbox.extra_writable`, `sandbox.extra_read`, `network.allow`, `skills.roots`,
/// `search.api_key_command`) take one entry per line. This mirrors what
/// [`ConfigEdit::apply_config`] writes for the same keys, so a settings auto
/// save never changes the file's shape.
pub fn set_scalar(path: &Path, keys: &[&str], value: Option<&str>) -> Result<(), ConfigError> {
    let mut edit = ConfigEdit::open(path)?;
    match value {
        Some(value) => set_scalar_value(&mut edit, keys, value)?,
        None => edit.remove(keys),
    }
    edit.save()
}

fn set_scalar_value(edit: &mut ConfigEdit, keys: &[&str], value: &str) -> Result<(), ConfigError> {
    match keys {
        ["sandbox", "extra_writable"]
        | ["sandbox", "extra_read"]
        | ["network", "allow"]
        | ["skills", "roots"]
        | ["search", "api_key_command"] => {
            let values = value
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            edit.set_string_array(keys, values)
        }
        _ => match value.trim() {
            "true" => edit.set_bool(keys, true),
            "false" => edit.set_bool(keys, false),
            _ => {
                if let Ok(integer) = value.trim().parse::<i64>() {
                    edit.set_integer(keys, integer)
                } else if let Ok(float) = value.trim().parse::<f64>() {
                    edit.set_float(keys, float)
                } else {
                    edit.set_str(keys, value.to_string())
                }
            }
        },
    }
}

fn set_optional(table: &mut Table, key: &str, value: Option<String>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        table.insert(key, Item::Value(Value::from(value)));
    }
}

/// A tri-state bool: `None` leaves the key out entirely, so an unset override
/// keeps inheriting from the catalog instead of being frozen into the file.
fn set_optional_bool(table: &mut Table, key: &str, value: Option<bool>) {
    if let Some(value) = value {
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

fn busy_delivery_str(delivery: crate::config::BusyDelivery) -> &'static str {
    match delivery {
        crate::config::BusyDelivery::Queue => "queue",
        crate::config::BusyDelivery::Steer => "steer",
    }
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

    /// The bug this fixes: saving any setting from the GUI used to freeze `~/x`
    /// into an absolute path, because `Config` holds expanded paths.
    ///
    /// One save and the file no longer means the same thing on another machine —
    /// and the user never asked for that edit, cannot see it happen, and only
    /// finds out when they move the config to their laptop.
    #[test]
    fn a_mcp_tilde_path_survives_a_save_that_did_not_touch_it() {
        let home = crate::config::home_dir().expect("a home directory to expand ~");
        let dir = temp();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
schema = 2

[[mcp.servers]]
name = "wordcraft"
command = "wordcraft-cli"
writable_paths = ["~/Documents"]
read_paths = ["~/cot/lib/wordcraft"]
"#,
        )
        .expect("write");

        // Load the way a run does: `~` becomes absolute in memory. That step is
        // what makes writing `Config` back a lossy operation.
        let mut config = crate::config::load(&path).expect("load");
        assert_eq!(
            config.mcp.servers[0].writable_paths,
            vec![home.join("Documents")],
            "the loader is what expands the path"
        );

        // Then save it back with one unrelated setting changed, which is what
        // the settings page does.
        config.agent.max_turns = 7;
        let mut edit = ConfigEdit::open(&path).expect("open");
        edit.apply_config(&config).expect("apply");
        edit.save().expect("save");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains(r#"writable_paths = ["~/Documents"]"#),
            "the tilde was frozen into an absolute path:\n{text}"
        );
        assert!(
            text.contains(r#"read_paths = ["~/cot/lib/wordcraft"]"#),
            "the tilde was frozen into an absolute path:\n{text}"
        );
        // ... and the setting that did change still changed.
        assert_eq!(
            crate::config::load(&path).expect("reload").agent.max_turns,
            7
        );
    }

    /// The other code path: `[sandbox]` and `[skills] roots` go through
    /// `set_path_array` rather than the rebuilt MCP tables.
    #[test]
    fn every_tilde_path_list_keeps_its_spelling() {
        let dir = temp();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
schema = 2

[sandbox]
extra_read = ["~/cot"]
extra_writable = ["~/Documents/output"]

[skills]
roots = ["~/skills"]
"#,
        )
        .expect("write");

        let mut config = crate::config::load(&path).expect("load");
        config.context.window_tokens = 99_000; // unrelated
        let mut edit = ConfigEdit::open(&path).expect("open");
        edit.apply_config(&config).expect("apply");
        edit.save().expect("save");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains(r#"extra_read = ["~/cot"]"#), "{text}");
        assert!(
            text.contains(r#"extra_writable = ["~/Documents/output"]"#),
            "{text}"
        );
        assert!(text.contains(r#"roots = ["~/skills"]"#), "{text}");
    }

    /// A path that *did* change is written in full: the fix must not degenerate
    /// into "never rewrite a path".
    #[test]
    fn a_tilde_path_that_changed_is_rewritten() {
        let home = crate::config::home_dir().expect("home");
        let dir = temp();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "schema = 2\n\n[[mcp.servers]]\nname = \"w\"\ncommand = \"x\"\n\
             writable_paths = [\"~/Documents\"]\n",
        )
        .expect("write");

        let mut config = crate::config::load(&path).expect("load");
        config.mcp.servers[0].writable_paths = vec![home.join("Projects")];
        let mut edit = ConfigEdit::open(&path).expect("open");
        edit.apply_config(&config).expect("apply");
        edit.save().expect("save");

        let text = std::fs::read_to_string(&path).expect("read");
        let projects = home.join("Projects");
        assert!(
            text.contains(&format!("writable_paths = [\"{}\"]", projects.display())),
            "a changed path must be written, not dropped:\n{text}"
        );
        assert!(!text.contains("~/Documents"), "{text}");
    }

    /// Reordering keeps each survivor's own spelling: the match is by value, so
    /// moving an entry in the UI does not respell it.
    #[test]
    fn reordered_paths_keep_their_own_spellings() {
        let home = crate::config::home_dir().expect("home");
        let spelled = spell_paths(
            &[home.join("b"), home.join("a")],
            vec!["~/a".to_string(), "~/b".to_string()],
        );
        assert_eq!(spelled, vec!["~/b", "~/a"]);
    }

    /// An array we cannot spell from — unknown entries, or fewer of them — must
    /// not lose a path: unmatched values fall back to the absolute form.
    #[test]
    fn unmatched_values_fall_back_to_the_absolute_form() {
        let home = crate::config::home_dir().expect("home");
        let only_new = home.join("only-new");
        let spelled = spell_paths(
            std::slice::from_ref(&only_new),
            vec!["~/unrelated".to_string()],
        );
        assert_eq!(spelled, vec![only_new.display().to_string()]);
    }

    #[test]
    fn a_default_config_writes_no_empty_sections() {
        let dir = temp();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# keep me\n").expect("write");

        let mut edit = ConfigEdit::open(&path).expect("open");
        let config = crate::config::Config::default();
        edit.apply_config(&config).expect("apply");
        edit.save().expect("save");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("[providers]"), "{text}");
        assert!(!text.contains("[pricing]"), "{text}");
        assert!(!text.contains("[approval]"), "{text}");
        assert!(text.contains("# keep me"), "{text}");
    }

    #[test]
    fn bare_model_ids_are_written_as_a_string_array() {
        let dir = temp();
        let path = dir.path().join("config.toml");
        let mut edit = ConfigEdit::open(&path).expect("open");
        let mut config = crate::config::Config::default();
        config.providers.insert(
            "p".to_string(),
            crate::config::ProviderConfig {
                base_url: Some("https://example.test/v1".to_string()),
                models: vec![crate::config::ModelEntry {
                    id: "m".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        edit.apply_config(&config).expect("apply");
        edit.save().expect("save");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains(r#"models = ["m"]"#), "{text}");
        assert!(!text.contains("[[providers.p.models]]"), "{text}");

        let loaded: crate::config::Config = toml::from_str(&text).expect("reload");
        assert_eq!(loaded.providers["p"].models[0].id, "m");
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
                // Both are tri-states that the table used to drop: `protocol`
                // was never written at all, so a route pointed at the Anthropic
                // endpoint came back as OpenAI after the next settings save.
                protocol: Some(crate::config::Protocol::Anthropic),
                images: Some(false),
                stream_usage: false,
                extra_body: Default::default(),
                headers: Default::default(),
                models: vec![crate::config::ModelEntry {
                    id: "deepseek-flash".to_string(),
                    name: Some("DeepSeek Flash".to_string()),
                    context_window: Some(131_072),
                    max_tokens: Some(8_192),
                }],
            },
        );
        config.context.window_tokens = 64_000;
        config.agent.max_turns = 12;
        config.agent.max_cost_usd = Some(0.5);
        config.agent.snapshot_shell = false;
        config.sandbox.mode = SandboxMode::SandboxExec;
        config.sandbox.network = Some(crate::config::SandboxNetworkMode::Proxy);
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
        config.mcp.sandbox = qcode_mcp::McpSandboxMode::Off;
        config.mcp.servers.push(McpServerConfig {
            name: "demo".to_string(),
            command: Some("demo-server".to_string()),
            args: vec!["--stdio".to_string()],
            env: BTreeMap::from([("TOKEN".to_string(), "x".to_string())]),
            enabled: false,
            timeout_secs: 7,
            ..Default::default()
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
        assert_eq!(provider.protocol, Some(crate::config::Protocol::Anthropic));
        assert_eq!(provider.images, Some(false));
        assert!(!provider.stream_usage);
        assert_eq!(provider.models.len(), 1);
        assert_eq!(provider.models[0].id, "deepseek-flash");
        assert_eq!(provider.models[0].name.as_deref(), Some("DeepSeek Flash"));
        assert_eq!(provider.models[0].context_window, Some(131_072));
        assert_eq!(provider.models[0].max_tokens, Some(8_192));
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
        // The crate-wide default and the per-server override, both of which are
        // easy to drop on save because they are not part of the servers array.
        assert_eq!(loaded.mcp.sandbox, qcode_mcp::McpSandboxMode::Off);
        assert_eq!(loaded.mcp.servers.len(), 1);
        assert_eq!(loaded.mcp.servers[0].timeout_secs, 7);
        assert!(!loaded.mcp.servers[0].enabled);
    }

    #[test]
    fn set_scalar_round_trips_and_removes() {
        let dir = temp();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# keep me\nversion = 1\n[agent]\nsnapshot_shell = true\n",
        )
        .expect("seed");

        set_scalar(&path, &["default_provider"], Some("deepseek")).expect("string");
        set_scalar(&path, &["agent", "max_turns"], Some("12")).expect("integer");
        set_scalar(&path, &["agent", "max_cost_usd"], Some("0.5")).expect("float");
        set_scalar(&path, &["agent", "snapshot_shell"], Some("false")).expect("bool");
        set_scalar(
            &path,
            &["network", "allow"],
            Some("api.example.com:443\n*.internal.test"),
        )
        .expect("array");

        let loaded = crate::config::load(&path).expect("valid");
        assert_eq!(loaded.default_provider.as_deref(), Some("deepseek"));
        assert_eq!(loaded.agent.max_turns, 12);
        assert_eq!(loaded.agent.max_cost_usd, Some(0.5));
        assert!(!loaded.agent.snapshot_shell);
        assert_eq!(
            loaded.network.allow,
            [
                "api.example.com:443".to_string(),
                "*.internal.test".to_string()
            ]
        );
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# keep me"), "{text}");

        set_scalar(&path, &["default_provider"], None).expect("remove");
        let loaded = crate::config::load(&path).expect("valid");
        assert!(loaded.default_provider.is_none());
        assert_eq!(loaded.agent.max_turns, 12, "other keys survive");
        let backup = std::fs::read_to_string(dir.path().join("config.toml.bak")).expect("backup");
        assert!(backup.contains("deepseek"), "{backup}");
    }

    /// The MCP page's new "sandbox default" control (ADR-0067 P1) writes a
    /// single scalar under a section that `apply_config` does not rebuild, so
    /// it takes the generic `set_scalar` path. Round trip it, and check that a
    /// value the enum does not accept is refused by the save's validation
    /// rather than written and silently ignored at load time.
    #[test]
    fn the_mcp_sandbox_default_round_trips_and_rejects_garbage() {
        let dir = temp();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 1\n").expect("seed");

        set_scalar(&path, &["mcp", "sandbox"], Some("off")).expect("write");
        let loaded = crate::config::load(&path).expect("valid");
        assert_eq!(loaded.mcp.sandbox, qcode_mcp::McpSandboxMode::Off);

        // The default is `confine`; writing it back is a no-op in meaning but
        // must still be accepted.
        set_scalar(&path, &["mcp", "sandbox"], Some("confine")).expect("write default");
        let loaded = crate::config::load(&path).expect("valid");
        assert_eq!(loaded.mcp.sandbox, qcode_mcp::McpSandboxMode::Confine);

        assert!(
            set_scalar(&path, &["mcp", "sandbox"], Some("maybe")).is_err(),
            "an unknown mode must fail the save, not land in the file"
        );
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("maybe"), "{text}");
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

    #[test]
    fn set_default_model_round_trips() {
        let dir = temp();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_provider = \"deepseek\"\n").expect("seed");

        set_default_model(&path, Some("ark"), Some("m1")).expect("set");
        let loaded = crate::config::load(&path).expect("valid");
        assert_eq!(loaded.default_model.provider.as_deref(), Some("ark"));
        assert_eq!(loaded.default_model.model.as_deref(), Some("m1"));
        assert_eq!(loaded.default_provider.as_deref(), Some("deepseek"));

        set_default_model(&path, None, None).expect("clear");
        let loaded = crate::config::load(&path).expect("valid");
        assert!(loaded.default_model.provider.is_none());
        assert!(loaded.default_model.model.is_none());

        // A fresh file (no `[default-model]`) is created on demand.
        let fresh = dir.path().join("fresh.toml");
        set_default_model(&fresh, Some("ollama"), Some("qwen")).expect("fresh");
        let loaded = crate::config::load(&fresh).expect("valid");
        assert_eq!(loaded.default_model.provider.as_deref(), Some("ollama"));
        assert_eq!(loaded.default_model.model.as_deref(), Some("qwen"));
    }
}

/// What `qc config migrate` changed (ADR-0065).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// The schema found in the file before migrating.
    pub from: u32,
    /// Whether anything was written.
    pub changed: bool,
    /// `[providers.*].default_model` keys folded into `[default-model]`.
    pub folded_defaults: usize,
    /// `[[providers.*.models]]` tables collapsed to `models = ["id"]`.
    pub collapsed_models: usize,
    /// `[network] enabled`/`mode` folded into `[sandbox] network`.
    pub folded_network: bool,
    /// Where the pre-migration copy went, when a file was rewritten.
    pub backup: Option<PathBuf>,
}

/// Rewrite a v1 file into the v2 shape (ADR-0065).
///
/// The mechanical part only: fold `[providers.*].default_model` into
/// `[default-model]`, collapse model tables that carry nothing but an id, and
/// switch `version` to `schema`. Comments outside those tables survive; the
/// file is backed up to `.bak` first, and running this twice changes nothing.
pub fn migrate_user_config(path: &Path) -> Result<MigrationReport, ConfigError> {
    let mut edit = ConfigEdit::open(path)?;
    if !edit.existed() {
        return Ok(MigrationReport::default());
    }
    let from = edit
        .document()
        .get("schema")
        .or_else(|| edit.document().get("version"))
        .and_then(|item| item.as_integer())
        .unwrap_or(1) as u32;
    if from >= crate::config::CONFIG_VERSION {
        return Ok(MigrationReport {
            from,
            ..MigrationReport::default()
        });
    }

    let mut folded_defaults = 0usize;
    let mut collapsed_models = 0usize;
    let mut folded_network = false;

    // 1. `[providers.X].default_model` → `[default-model]` when that is still
    //    empty. A provider that loses its key keeps working: the resolver falls
    //    back to the catalog's default model.
    let providers: Vec<String> = edit
        .document()
        .get("providers")
        .and_then(|item| item.as_table_like())
        .map(|table| table.iter().map(|(name, _)| name.to_string()).collect())
        .unwrap_or_default();
    let already_set = edit
        .document()
        .get("default-model")
        .and_then(|item| item.as_table_like())
        .and_then(|table| table.get("model"))
        .and_then(|item| item.as_str())
        .map(|model| !model.is_empty())
        .unwrap_or(false);
    for (index, provider) in providers.iter().enumerate() {
        let Some(table) = edit
            .document_mut()
            .get_mut("providers")
            .and_then(|item| item.as_table_like_mut())
            .and_then(|table| table.get_mut(provider))
            .and_then(|item| item.as_table_like_mut())
        else {
            continue;
        };
        let Some(default) = table.remove("default_model") else {
            continue;
        };
        folded_defaults += 1;
        if !already_set
            && index == 0
            && let Some(model) = default.as_str()
        {
            let _ = edit.set_str(&["default-model", "model"], model);
            let _ = edit.set_str(&["default-model", "provider"], provider.clone());
        }
    }

    // 2. A model table that only says `id` is the shorthand we would have
    //    written; anything with metadata stays a table. Both encodings matter:
    //    `models = [{ id = "x" }]` is an array of inline tables, while
    //    `[[providers.x.models]]` is an array of tables.
    for provider in &providers {
        let Some(item) = edit
            .document_mut()
            .get_mut("providers")
            .and_then(|item| item.as_table_like_mut())
            .and_then(|table| table.get_mut(provider))
            .and_then(|item| item.as_table_like_mut())
            .and_then(|table| table.get_mut("models"))
        else {
            continue;
        };
        let mut ids: Option<Vec<String>> = None;
        if let Some(array) = item.as_array() {
            let mut found = Vec::new();
            let mut ok = true;
            for entry in array.iter() {
                let Some(table) = entry.as_inline_table() else {
                    ok = false;
                    break;
                };
                let Some(id) = table.get("id").and_then(|value| value.as_str()) else {
                    ok = false;
                    break;
                };
                if table.len() != 1 {
                    ok = false;
                    break;
                }
                found.push(id.to_string());
            }
            if ok {
                ids = Some(found);
            }
        } else if let Some(array) = item.as_array_of_tables() {
            let mut found = Vec::new();
            let mut ok = true;
            for table in array.iter() {
                let Some(id) = table.get("id").and_then(|value| value.as_str()) else {
                    ok = false;
                    break;
                };
                if table.len() != 1 {
                    ok = false;
                    break;
                }
                found.push(id.to_string());
            }
            if ok {
                ids = Some(found);
            }
        }
        let Some(ids) = ids else { continue };
        let count = ids.len();
        let mut list = toml_edit::Array::new();
        for id in ids {
            list.push(id);
        }
        let _ = edit.set_value(&["providers", provider, "models"], Value::Array(list));
        collapsed_models += count;
    }

    // 3. `[network] enabled/mode` → `[sandbox] network` (ADR-0065). The rest of
    //    the section (allow list, unknown policy, audit) stays where it is: the
    //    knob answers "may it reach the network", not "which hosts".
    let legacy_enabled = edit
        .document()
        .get("network")
        .and_then(|item| item.as_table_like())
        .and_then(|table| table.get("enabled"))
        .and_then(|item| item.as_bool());
    let legacy_mode = edit
        .document()
        .get("network")
        .and_then(|item| item.as_table_like())
        .and_then(|table| table.get("mode"))
        .and_then(|item| item.as_str())
        .map(str::to_string);
    // A legacy `network = true|false` is a bool, not a mode name; only a string
    // counts as "already migrated".
    let knob = edit
        .document()
        .get("sandbox")
        .and_then(|item| item.as_table_like())
        .and_then(|table| table.get("network"));
    let legacy_flag = knob.and_then(|item| item.as_bool());
    let has_knob = knob.is_some_and(|item| item.as_str().is_some());
    if !has_knob && (legacy_enabled.is_some() || legacy_mode.is_some()) {
        let mode = match (legacy_mode.as_deref(), legacy_enabled, legacy_flag) {
            (_, _, Some(true)) => "open",
            (Some("strict"), _, _) => "strict",
            (Some("off"), _, _) => "off",
            (_, Some(false), _) => "off",
            _ => "proxy",
        };
        let _ = edit.set_str(&["sandbox", "network"], mode);
        edit.remove(&["network", "mode"]);
        edit.remove(&["network", "enabled"]);
        folded_network = true;
    }

    // 4. The version key itself. A file that says `version` keeps the key and
    //    gets the new number; renaming it would drop the comment attached to it,
    //    and the alias makes both spellings read the same. Files that already
    //    say `schema` (or said nothing) get `schema`.
    if let Some(value) = edit
        .document_mut()
        .get_mut("version")
        .and_then(|item| item.as_value_mut())
    {
        // In place: `insert` would replace the key and take its comment with it.
        *value = Value::from(i64::from(crate::config::CONFIG_VERSION));
    } else {
        edit.set_integer(&["schema"], i64::from(crate::config::CONFIG_VERSION))?;
    }
    edit.save()?;

    Ok(MigrationReport {
        from,
        changed: true,
        folded_defaults,
        collapsed_models,
        folded_network,
        backup: Some(backup_path(path)),
    })
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".bak");
    PathBuf::from(name)
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use std::io::Write;

    fn v1_file(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("config.toml");
        let mut file = std::fs::File::create(&path).expect("create");
        write!(
            file,
            r#"# my qcode config
version = 1

[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"
default_model = "deepseek-flash"
models = [{{ id = "deepseek-flash" }}, {{ id = "deepseek-v4-pro" }}]

[providers.gw]
base_url = "http://localhost:8000/v1"
models = [{{ id = "m", name = "M" }}]
"#
        )
        .expect("write");
        path
    }

    #[test]
    fn migrate_folds_defaults_and_collapses_bare_model_tables() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = v1_file(&dir);

        let report = migrate_user_config(&path).expect("migrate");
        assert!(report.changed);
        assert_eq!(report.from, 1);
        assert_eq!(report.folded_defaults, 1);
        assert_eq!(report.collapsed_models, 2);
        assert!(report.backup.expect("backup path").exists());

        let text = std::fs::read_to_string(&path).expect("read");
        // A file that used `version` keeps the spelling, with the new number.
        assert!(
            text.contains("version = 2") || text.contains("schema = 2"),
            "{text}"
        );
        assert!(!text.contains("version = 1"), "{text}");
        assert!(
            !text.contains(
                "[providers.deepseek]\napi_key_env = \"DEEPSEEK_API_KEY\"\ndefault_model"
            ),
            "{text}"
        );
        assert!(
            text.contains(r#"models = ["deepseek-flash", "deepseek-v4-pro"]"#),
            "{text}"
        );
        // A table with metadata keeps its shape.
        assert!(text.contains(r#"{ id = "m", name = "M" }"#), "{text}");
        // The comment outside the rewritten tables survives.
        assert!(text.contains("# my qcode config"), "{text}");

        // The result is a file this build loads without complaints.
        let loaded: crate::config::Config = toml::from_str(&text).expect("reload");
        assert_eq!(loaded.schema, crate::config::CONFIG_VERSION);
        assert_eq!(
            loaded.default_model.model.as_deref(),
            Some("deepseek-flash")
        );
    }

    #[test]
    fn migrate_folds_the_network_switch_and_keeps_the_host_list() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"version = 1

[sandbox]
mode = "sandbox-exec"
network = false

[network]
enabled = false
allow = ["api.deepseek.com"]
audit = true
"#,
        )
        .expect("write");

        let report = migrate_user_config(&path).expect("migrate");
        assert!(report.folded_network);

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains(r#"network = "off""#), "{text}");
        assert!(!text.contains("enabled = false"), "{text}");
        // The host list is not part of the switch and stays put.
        assert!(text.contains(r#"allow = ["api.deepseek.com"]"#), "{text}");
        assert!(text.contains("audit = true"), "{text}");

        let loaded: crate::config::Config = toml::from_str(&text).expect("reload");
        assert_eq!(
            loaded.sandbox_network_mode(),
            crate::config::SandboxNetworkMode::Off
        );
    }

    #[test]
    fn migrating_twice_changes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = v1_file(&dir);
        migrate_user_config(&path).expect("first");
        let after_first = std::fs::read_to_string(&path).expect("read");

        let report = migrate_user_config(&path).expect("second");
        assert!(!report.changed);
        assert_eq!(report.from, crate::config::CONFIG_VERSION);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), after_first);
    }
}
