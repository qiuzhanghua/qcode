//! Search tools: glob and grep.
//!
//! Both walk with the `ignore` crate, so `.gitignore`, hidden files and the
//! usual build directories are respected without the model having to reason
//! about them.

use std::path::Path;

use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use qcode_core::event::{ToolMetadata, ToolStatus, Usage};
use qcode_core::provider::ToolSpec;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::path;

const MAX_GLOB_RESULTS: usize = 1000;
const MAX_GREP_MATCHES: usize = 200;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
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

fn compile_glob(pattern: &str) -> Result<GlobMatcher, ToolError> {
    Glob::new(pattern)
        .map(|glob| glob.compile_matcher())
        .map_err(|err| ToolError::Invalid(format!("invalid glob: {err}")))
}

/// Patterns without a slash are matched against the basename too, so `*.rs`
/// behaves the way people expect instead of only matching the workspace root.
fn matches_glob(matcher: &GlobMatcher, relative: &Path) -> bool {
    if matcher.is_match(relative) {
        return true;
    }
    relative
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matcher.is_match(name))
}

// ---------------------------------------------------------------------------
// glob
// ---------------------------------------------------------------------------

pub struct GlobTool;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn spec(&self) -> ToolSpec {
        spec(
            "glob",
            "Find files by glob pattern inside the workspace. Respects .gitignore \
             and skips hidden/build directories. Results are sorted.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "e.g. **/*.rs"},
                    "path": {"type": "string", "description": "Directory to search, defaults to the workspace root"}
                },
                "required": ["pattern"]
            }),
        )
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Read
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: GlobArgs = parse(args)?;
        let matcher = compile_glob(&args.pattern)?;
        let root = path::resolve(&ctx.workspace, args.path.as_deref().unwrap_or("."))?;

        let mut matches = Vec::new();
        for entry in WalkBuilder::new(&root).require_git(false).build() {
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&ctx.workspace)
                .unwrap_or(entry.path());
            if matches_glob(&matcher, relative) {
                matches.push(relative.display().to_string());
                if matches.len() >= MAX_GLOB_RESULTS {
                    break;
                }
            }
        }
        matches.sort();

        let joined = matches.join("\n");
        let bytes = joined.len() as u64;
        let (output, truncated) = crate::truncate(&joined, OUTPUT_LIMIT);
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
            full_output: truncated.then(|| joined.clone()),
        })
    }
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

pub struct GrepTool;

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    context: Option<usize>,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn spec(&self) -> ToolSpec {
        spec(
            "grep",
            "Search file contents with a regular expression. Respects .gitignore, \
             skips binary files, and returns at most 200 matching lines.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Rust regex syntax"},
                    "path": {"type": "string", "description": "Directory to search, defaults to the workspace root"},
                    "glob": {"type": "string", "description": "Only search files matching this glob, e.g. **/*.rs"},
                    "context": {"type": "integer", "minimum": 0, "maximum": 5, "description": "Context lines around each match"}
                },
                "required": ["pattern"]
            }),
        )
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Read
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: GrepArgs = parse(args)?;
        let regex = Regex::new(&args.pattern)
            .map_err(|err| ToolError::Invalid(format!("invalid regex: {err}")))?;
        let glob_filter = args.glob.as_deref().map(compile_glob).transpose()?;
        let context = args.context.unwrap_or(0).min(5);
        let root = path::resolve(&ctx.workspace, args.path.as_deref().unwrap_or("."))?;

        // (path, line number, text, is_match)
        let mut results: Vec<(String, usize, String, bool)> = Vec::new();
        let mut matched = 0;

        'walk: for entry in WalkBuilder::new(&root).require_git(false).build() {
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }

            let path = entry.path();
            let relative = path.strip_prefix(&ctx.workspace).unwrap_or(path);
            if glob_filter
                .as_ref()
                .is_some_and(|matcher| !matches_glob(matcher, relative))
            {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.len() > MAX_FILE_BYTES {
                continue;
            }

            let bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            if bytes.iter().take(8192).any(|byte| *byte == 0) {
                continue; // binary
            }

            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            let display = relative.display().to_string();

            for (index, line) in lines.iter().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                matched += 1;

                let start = index.saturating_sub(context);
                let end = (index + context + 1).min(lines.len());
                for (context_index, context_line) in lines.iter().enumerate().take(end).skip(start)
                {
                    results.push((
                        display.clone(),
                        context_index + 1,
                        context_line.to_string(),
                        context_index == index,
                    ));
                }

                if matched >= MAX_GREP_MATCHES {
                    break 'walk;
                }
            }
        }

        // Deterministic order; at equal line numbers prefer the actual match.
        results.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(b.3.cmp(&a.3)));
        results.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

        if results.is_empty() {
            return Ok(ToolOutput::ok("no matches"));
        }

        let mut output = String::new();
        for (display, line, text, is_match) in results {
            let separator = if is_match { ':' } else { '-' };
            output.push_str(&format!(
                "{display}{separator}{line}{separator}{}\n",
                text.trim_end()
            ));
        }

        let bytes = output.len() as u64;
        let raw = output;
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
