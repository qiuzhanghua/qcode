//! Filesystem tools: read_file, write_file, edit_file, list_dir.

use std::path::Path;

use async_trait::async_trait;
use qcode_core::event::{ToolMetadata, ToolStatus, Usage};
use qcode_core::provider::ToolSpec;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::path::{self, Access};

const OUTPUT_LIMIT: usize = 32 * 1024;

fn spec(name: &str, description: &str, parameters: Value) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
    }
}

fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|err| ToolError::Invalid(err.to_string()))
}

fn atomic_write(path: &Path, contents: &str) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temp = path.with_file_name(format!(".{file_name}.qcode-tmp-{}", std::process::id()));
    std::fs::write(&temp, contents)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

pub struct ReadFile;

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn spec(&self) -> ToolSpec {
        spec(
            "read_file",
            "Read a UTF-8 text file inside the workspace. Returns numbered lines. \
             Use offset/limit for large files.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative or absolute path"},
                    "offset": {"type": "integer", "minimum": 1, "description": "First line (1-based)"},
                    "limit": {"type": "integer", "minimum": 1, "description": "Maximum lines"}
                },
                "required": ["path"]
            }),
        )
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Read
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: ReadArgs = parse(args)?;
        let resolved = path::resolve_ctx(ctx, &args.path, Access::Read)?;
        let bytes = std::fs::read(&resolved.path)?;

        if bytes.iter().take(8192).any(|byte| *byte == 0) {
            return Ok(ToolOutput::error("binary file; refusing to read as text"));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| ToolError::Invalid("file is not valid UTF-8".to_string()))?;

        let lines: Vec<&str> = text.lines().collect();
        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(2000).min(5000);
        let start = (offset - 1).min(lines.len());
        let end = (start + limit).min(lines.len());

        let mut output = String::new();
        for (index, line) in lines[start..end].iter().enumerate() {
            output.push_str(&format!("{:>6}| {}\n", start + index + 1, line));
        }
        let raw = output;
        let (output, truncated) = crate::truncate(&raw, OUTPUT_LIMIT);

        Ok(ToolOutput {
            status: ToolStatus::Ok,
            output,
            metadata: ToolMetadata {
                truncated,
                bytes: text.len() as u64,
                ..ToolMetadata::default()
            },
            usage: Usage::default(),
            cost_usd: 0.0,
            full_output: truncated.then(|| raw.clone()),
        })
    }
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

pub struct WriteFile;

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn spec(&self) -> ToolSpec {
        spec(
            "write_file",
            "Create or overwrite a UTF-8 text file inside the workspace. \
             Parent directories are created automatically.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        )
    }

    fn approval(&self, ctx: &ToolContext, args: &Value) -> ApprovalClass {
        let path = args
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if let Ok(resolved) = path::resolve_ctx(ctx, path, Access::Read)
            && resolved.root.access == qcode_core::task::RootAccess::ReadWrite
        {
            ApprovalClass::WriteInside
        } else {
            ApprovalClass::WriteOutside
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: WriteArgs = parse(args)?;
        let resolved = path::resolve_ctx(ctx, &args.path, Access::Write)?;
        atomic_write(&resolved.path, &args.content)?;
        Ok(ToolOutput::ok(format!(
            "wrote {} bytes to {}",
            args.content.len(),
            resolved.display
        )))
    }
}

// ---------------------------------------------------------------------------
// edit_file
// ---------------------------------------------------------------------------

pub struct EditFile;

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn spec(&self) -> ToolSpec {
        spec(
            "edit_file",
            "Replace an exact string in a file. old_string must be unique unless \
             replace_all is true.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean", "default": false}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        )
    }

    fn approval(&self, ctx: &ToolContext, args: &Value) -> ApprovalClass {
        let path = args
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if let Ok(resolved) = path::resolve_ctx(ctx, path, Access::Read)
            && resolved.root.access == qcode_core::task::RootAccess::ReadWrite
        {
            ApprovalClass::WriteInside
        } else {
            ApprovalClass::WriteOutside
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: EditArgs = parse(args)?;
        if args.old_string.is_empty() {
            return Err(ToolError::Invalid("old_string is empty".to_string()));
        }
        if args.old_string == args.new_string {
            return Err(ToolError::Invalid(
                "old_string and new_string are identical".to_string(),
            ));
        }

        let resolved = path::resolve_ctx(ctx, &args.path, Access::Write)?;
        let path = &resolved.path;
        let original = std::fs::read_to_string(path)?;
        let matches = original.matches(&args.old_string).count();

        if matches == 0 {
            return Ok(ToolOutput::error(format!(
                "old_string not found in {}",
                resolved.display
            )));
        }
        if matches > 1 && !args.replace_all {
            return Ok(ToolOutput::error(format!(
                "old_string appears {matches} times in {}; add context to make it unique \
                 or pass replace_all",
                resolved.display
            )));
        }

        let updated = if args.replace_all {
            original.replace(&args.old_string, &args.new_string)
        } else {
            original.replacen(&args.old_string, &args.new_string, 1)
        };
        atomic_write(path, &updated)?;

        Ok(ToolOutput::ok(format!(
            "replaced {matches} occurrence(s) in {}",
            resolved.display
        )))
    }
}

// ---------------------------------------------------------------------------
// list_dir
// ---------------------------------------------------------------------------

pub struct ListDir;

#[derive(Deserialize)]
struct ListArgs {
    path: Option<String>,
}

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn spec(&self) -> ToolSpec {
        spec(
            "list_dir",
            "List entries in a directory inside the workspace. Skips .git, target \
             and node_modules.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "default": "."}
                }
            }),
        )
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Read
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: ListArgs = parse(args)?;
        let resolved = path::resolve_ctx(ctx, args.path.as_deref().unwrap_or("."), Access::Read)?;
        let path = &resolved.path;

        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(name.as_str(), ".git" | "target" | "node_modules") {
                continue;
            }
            let metadata = entry.metadata()?;
            let kind = if metadata.is_dir() { "dir " } else { "file" };
            entries.push(format!("{kind} {:>10}  {name}", metadata.len()));
        }
        entries.sort();

        let raw = entries.join("\n");
        let bytes = raw.len() as u64;
        let (output, truncated) = crate::truncate(&raw, OUTPUT_LIMIT);
        Ok(ToolOutput {
            status: ToolStatus::Ok,
            output,
            metadata: ToolMetadata {
                truncated,
                bytes,
                ..ToolMetadata::default()
            },
            usage: Usage::default(),
            cost_usd: 0.0,
            full_output: truncated.then(|| raw.clone()),
        })
    }
}
