use crate::cli::*;
use crate::output::*;
use crate::{agents, doctor, logging, subagent, tui, update};
use qcode_core::approval::{Approver, AutoApprove, AutoDeny, InteractiveApprover};
use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::SessionStarted;
use qcode_core::provider::Provider;
use qcode_core::store::SessionStore;
use qcode_core::tool::Tool;
use qcode_kernel::config;
use qcode_tools::skills::SkillSet;
use qcode_tools::subagent::SubagentTool;
use qcode_tui::SkillView;
use std::io::IsTerminal;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

mod gui;
mod misc;
mod run;
mod sessions;
mod settings;

pub(crate) use gui::{cmd_gui, cmd_tui};
#[cfg(windows)]
pub(crate) use misc::cmd_sandbox;
pub(crate) use misc::{cmd_acp, cmd_doctor, cmd_skills, cmd_update};
pub(crate) use run::cmd_run;
pub(crate) use sessions::{cmd_sessions, store_key};
pub(crate) use settings::{cmd_config, cmd_mcp};

/// Read `--image` files into base64 attachments.
///
/// Validating here keeps the engine's error path about capabilities only; a
/// missing or oversized file is a user error, not a model error.
pub(crate) fn read_images(
    paths: &[PathBuf],
) -> Result<Vec<qcode_core::engine::Attachment>, Box<dyn std::error::Error>> {
    const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
    let mut attachments = Vec::new();
    for path in paths {
        let metadata = std::fs::metadata(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if metadata.len() > MAX_IMAGE_BYTES {
            return Err(format!("{} is larger than 8 MB", path.display()).into());
        }
        let mime = match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("webp") => "image/webp",
            Some("gif") => "image/gif",
            _ => {
                return Err(format!(
                    "{}: only PNG, JPEG, WebP and GIF are supported",
                    path.display()
                )
                .into());
            }
        };
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        use base64::Engine as _;
        let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        attachments.push(qcode_core::engine::Attachment {
            mime_type: mime.to_string(),
            data_base64,
        });
    }
    Ok(attachments)
}

pub(crate) fn activated_prompt(
    skills: &SkillSet,
    name: &str,
    task: String,
) -> Result<String, Box<dyn std::error::Error>> {
    let Some(skill) = skills.get(name) else {
        let available: Vec<&str> = skills
            .iter()
            .take(20)
            .map(|skill| skill.name.as_str())
            .collect();
        return Err(format!(
            "unknown skill '{name}'; available: {}",
            available.join(", ")
        )
        .into());
    };
    let task = if task.trim().is_empty() {
        "Follow the skill instructions above now.".to_string()
    } else {
        task
    };
    Ok(format!(
        "[skill:{}]\n{}\n\n{task}",
        skill.name,
        skill.body.trim()
    ))
}

pub(crate) fn skill_views(skills: &SkillSet) -> Vec<SkillView> {
    skills
        .iter()
        .map(|skill| SkillView {
            name: skill.name.clone(),
            source: skill.source.label().to_string(),
            description: skill.description.clone(),
            dir: skill.dir.display().to_string(),
            body: skill.body.clone(),
            files: skill.files.clone(),
            files_truncated: skill.files_truncated,
        })
        .collect()
}
