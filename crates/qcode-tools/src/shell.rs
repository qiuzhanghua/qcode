//! run_shell: the only tool that executes arbitrary code.
//!
//! The environment is scrubbed down to a small allow-list so API keys in the
//! parent process cannot be read by `env`. Approval is `Execute` by default.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use qcode_core::event::{ToolMetadata, ToolStatus};
use qcode_core::provider::ToolSpec;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::path;

const OUTPUT_LIMIT: usize = 32 * 1024;
const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "LANG",
    "LC_ALL",
    "TERM",
    "TMPDIR",
    // Build configuration, not secrets. Without these the agent's cargo uses
    // the wrong registry cache (and may fight the sandbox).
    "CARGO_HOME",
    "RUSTUP_HOME",
    "CARGO_TARGET_DIR",
];

pub struct RunShell;

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    cwd: Option<String>,
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for RunShell {
    fn name(&self) -> &str {
        "run_shell"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_shell".to_string(),
            description: "Run a shell command inside the workspace. Use it for builds, \
                          tests, git and package managers; use the dedicated tools for \
                          reading and editing files."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string", "description": "Working directory, defaults to the workspace root"},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600}
                },
                "required": ["command"]
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Execute
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: ShellArgs =
            serde_json::from_value(args).map_err(|err| ToolError::Invalid(err.to_string()))?;

        if args.command.trim().is_empty() {
            return Err(ToolError::Invalid("command is empty".to_string()));
        }

        let cwd = match args.cwd.as_deref() {
            Some(cwd) if !cwd.is_empty() => path::resolve(&ctx.workspace, cwd)?,
            _ => ctx.workspace.clone(),
        };
        let timeout_secs = args.timeout_secs.unwrap_or(120).clamp(1, 600);

        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(&args.command)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        command.env_clear();
        for key in ENV_ALLOWLIST {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }

        let child = command.spawn()?;
        let output =
            match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
                .await
            {
                Ok(result) => result?,
                Err(_) => {
                    return Ok(ToolOutput {
                        status: ToolStatus::Error,
                        output: format!("command timed out after {timeout_secs}s"),
                        metadata: ToolMetadata::default(),
                    });
                }
            };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut combined = String::new();
        combined.push_str(&stdout);
        if !stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&stderr);
        }

        let exit_code = output.status.code();
        let mut text = format!(
            "exit code: {}\n",
            exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string())
        );
        text.push_str(&combined);

        let (text, truncated) = crate::truncate(&text, OUTPUT_LIMIT);
        Ok(ToolOutput {
            status: ToolStatus::Ok,
            output: text,
            metadata: ToolMetadata {
                truncated,
                bytes: combined.len() as u64,
                exit_code,
                ..ToolMetadata::default()
            },
        })
    }
}
