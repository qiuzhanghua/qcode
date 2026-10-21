//! Read-modify-write a config file without losing comments or unknown keys.
//!
//! The settings UI and the first-run wizard edit the same document. Values are
//! written through `toml_edit`, validated by loading the temporary file, then
//! atomically renamed into place. Existing files get a `.bak` sibling.

use std::io::Write;
use std::path::{Path, PathBuf};

use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::config::{self, ConfigError};

/// A config document loaded from disk, or an empty one when the file is new.
#[derive(Debug)]
pub struct ConfigEdit {
    path: PathBuf,
    document: DocumentMut,
    existed: bool,
}

impl ConfigEdit {
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
        let mut file = options.open(&temp).map_err(|source| ConfigError::Read {
            path: temp.clone(),
            source,
        })?;
        file.write_all(self.document.to_string().as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|source| ConfigError::Read {
                path: temp.clone(),
                source,
            })?;
        drop(file);

        if let Err(error) = config::load(&temp) {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
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

        let loaded = config::load(&path).expect("valid config");
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
        let loaded = config::load(&path).expect("valid");
        assert!(loaded.default_provider.is_none());
        assert!(loaded.agent.snapshot_shell);
        assert_eq!(loaded.network.allow.len(), 2);
        assert_eq!(loaded.approval.rules.len(), 1);
    }
}
