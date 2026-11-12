//! Filesystem tools: read_file, write_file, edit_file, list_dir.

use std::path::Path;

use async_trait::async_trait;
use qcode_core::event::{ToolMetadata, ToolStatus, Usage};
use qcode_core::provider::ToolSpec;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolImage, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::path::{self, Access};

const OUTPUT_LIMIT: usize = 32 * 1024;

/// Run a synchronous filesystem operation off the runtime thread.
///
/// The GUI runs the engine on a `new_current_thread` runtime, so a blocking
/// `std::fs` call does not just cost a frame: while one task reads a large
/// file, everything else on that thread — streaming output, cancellation, a
/// second session's turn — is stopped. ADR-0058 §8: keep the calls, move them
/// to the blocking pool instead of growing the runtime's thread count.
async fn blocking<T, F>(work: F) -> Result<T, ToolError>
where
    F: FnOnce() -> Result<T, ToolError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        // The join only fails when the closure panicked; surface it as an IO
        // error so the tool row records a failure instead of hanging the turn.
        Err(error) => Err(ToolError::Io(std::io::Error::other(format!(
            "blocking task failed: {error}"
        )))),
    }
}

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

/// Detect an image by its magic bytes (ADR-0081).
///
/// Not by extension: a screenshot saved as `.dat` is still a PNG, and a `.png`
/// holding text is still text. The store accepts exactly these four formats, so
/// recognising a fifth here would only move the failure one layer down.
fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

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
            "Read a file inside the workspace. Text files return numbered lines (use \
             offset/limit for large ones). A PNG/JPEG/WebP/GIF comes back as the picture \
             itself, which a model that accepts images can look at.",
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
        let target = resolved.path.clone();
        let bytes = blocking(move || std::fs::read(target).map_err(ToolError::from)).await?;

        // A picture is a legitimate thing to read (ADR-0081): hand the model
        // the image instead of refusing it as "binary". Detected by magic
        // bytes, not by extension — a screenshot saved as `.dat` is still a
        // PNG, and a `.png` that is really text is still text.
        if let Some(mime) = image_mime(&bytes) {
            let size = bytes.len();
            let label = resolved.path.display().to_string();
            let mut output = ToolOutput::ok(format!(
                "{label}: {mime}, {size} bytes — returned as an image, not as text."
            ));
            output.images.push(ToolImage {
                mime_type: mime.to_string(),
                data: bytes,
                label,
            });
            return Ok(output);
        }

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
            images: Vec::new(),
            diff: None,
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
        // Read the old text before the write so the transcript can show what
        // changed; a missing or unreadable file simply means "no before".
        let target = resolved.path.clone();
        let before_path = target.clone();
        let before = blocking(move || Ok(crate::diff::read_source(&before_path))).await?;
        let diff = crate::diff::file(
            &path::diff_path(&resolved),
            before.as_deref(),
            Some(&args.content),
        );
        let contents = args.content;
        let written = contents.len();
        blocking(move || atomic_write(&target, &contents)).await?;
        Ok(
            ToolOutput::ok(format!("wrote {written} bytes to {}", resolved.display))
                .with_diff(diff),
        )
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
        let read_path = path.clone();
        let original =
            blocking(move || std::fs::read_to_string(read_path).map_err(ToolError::from)).await?;
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
        let diff = crate::diff::file(&path::diff_path(&resolved), Some(&original), Some(&updated));
        let write_path = path.clone();
        blocking(move || atomic_write(&write_path, &updated)).await?;
        Ok(ToolOutput::ok(format!(
            "replaced {matches} occurrence(s) in {}",
            resolved.display
        ))
        .with_diff(diff))
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
        let target = resolved.path.clone();

        let entries = blocking(move || -> Result<Vec<String>, ToolError> {
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(&target)? {
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
            Ok(entries)
        })
        .await?;

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
            images: Vec::new(),
            diff: None,
        })
    }
}
