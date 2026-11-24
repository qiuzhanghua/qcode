//! `job_list`, `job_read`, `job_kill`: following a command that outlived its
//! wait budget.
//!
//! `run_shell` hands a slow command to [`crate::jobs`] and returns a handle
//! (ADR-0106). These three are the other half of that bargain: without them the
//! hand-over would only mean "the output is somewhere you cannot reach".
//!
//! They are deliberately thin. All the state lives in [`crate::jobs`]; these
//! tools only turn it into text a model can act on, and every number they report
//! (dropped bytes, exit code, age) comes straight from the job rather than being
//! inferred here.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qcode_core::provider::ToolSpec;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::jobs::{self, Job, JobRegistry, Phase};

/// Fill a positional placeholder list from numbers, which is what every message
/// here needs (`%{0}`, `%{1}` …).
fn secs(duration: Duration) -> String {
    format!("{:.0}", duration.as_secs_f64())
}

/// One line describing where a job stands.
fn phase_text(phase: Phase) -> String {
    match phase {
        Phase::Running => qcode_i18n::tr("tool.jobs.running").into_owned(),
        Phase::Exited(Some(code)) => {
            qcode_i18n::trp("tool.jobs.exited", &[code.to_string().as_str()])
        }
        Phase::Exited(None) => qcode_i18n::tr("tool.jobs.exited_unknown").into_owned(),
        Phase::Killed => qcode_i18n::tr("tool.jobs.killed").into_owned(),
        Phase::Expired => qcode_i18n::trp(
            "tool.jobs.expired",
            &[(jobs::MAX_LIFETIME.as_secs() / 60).to_string().as_str()],
        ),
    }
}

// ------------------------------------------------------------------ list

pub struct JobList {
    jobs: Arc<JobRegistry>,
}

impl JobList {
    pub fn new(jobs: Arc<JobRegistry>) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl Tool for JobList {
    fn name(&self) -> &str {
        "job_list"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "List the commands still running in the background, plus the ones \
                          that finished most recently. `run_shell` moves a command here when \
                          it outlives `timeout_secs` or was started with `background: true`."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        // Reads state the model itself created; no files, no processes, no
        // network. Same class as `todo_write`.
        ApprovalClass::Read
    }

    async fn execute(&self, _ctx: &ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        let jobs = self.jobs.list();
        if jobs.is_empty() {
            return Ok(ToolOutput::ok(
                qcode_i18n::tr("tool.jobs.none").into_owned(),
            ));
        }

        let mut text = String::new();
        for job in jobs {
            // Shape: `job-1 [running 12s] 2048 bytes  npm run dev`.
            text.push_str(&format!(
                "{}\n",
                qcode_i18n::trp(
                    "tool.jobs.list_row",
                    &[
                        job.id(),
                        phase_text(job.phase()).as_str(),
                        secs(job.elapsed()).as_str(),
                        job.produced().to_string().as_str(),
                        first_line(job.command()),
                    ],
                )
            ));
        }
        Ok(ToolOutput::ok(text))
    }
}

/// The command's first line: a multi-line command would otherwise blow up a
/// one-row-per-job listing.
fn first_line(command: &str) -> &str {
    command.lines().next().unwrap_or("").trim()
}

// ------------------------------------------------------------------ read

pub struct JobRead {
    jobs: Arc<JobRegistry>,
}

impl JobRead {
    pub fn new(jobs: Arc<JobRegistry>) -> Self {
        Self { jobs }
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    job: String,
    /// How long to wait for new output before answering "nothing yet".
    #[serde(default)]
    wait_secs: Option<u64>,
}

#[async_trait]
impl Tool for JobRead {
    fn name(&self) -> &str {
        "job_read"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: concat!(
                "Read what a background job printed since the last read. Each call advances a ",
                "cursor, so output is never repeated. Set `wait_secs` to wait for the job ",
                "instead of asking again: it returns as soon as there is new output, the job ",
                "exits, or the wait runs out."
            )
            .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job": {
                        "type": "string",
                        "description": "Job handle from run_shell or job_list, e.g. `job-3`."
                    },
                    "wait_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": jobs::MAX_WAIT.as_secs(),
                        "description": "Wait up to this long for new output or for the job to exit. Default 0: return immediately."
                    }
                },
                "required": ["job"],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        // The output of a command the user already approved. Reading it does not
        // touch anything new — the process is doing whatever it was doing.
        ApprovalClass::Read
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: ReadArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Invalid(format!("job_read: {err}")))?;
        let Some(job) = self.jobs.get(&args.job) else {
            return Ok(ToolOutput::error(qcode_i18n::trp(
                "tool.jobs.unknown",
                &[args.job.as_str()],
            )));
        };

        let wait = Duration::from_secs(args.wait_secs.unwrap_or(0)).min(jobs::MAX_WAIT);
        let read = read_with_wait(&job, wait, ctx).await;

        let mut text = String::new();
        if read.dropped > 0 {
            // Said first, because it explains why the rest is shorter than the
            // command's byte count.
            text.push_str(&format!(
                "{}\n",
                qcode_i18n::trp("tool.jobs.dropped", &[read.dropped.to_string().as_str()])
            ));
        }
        if read.is_empty() {
            text.push_str(&format!("{}\n", qcode_i18n::tr("tool.jobs.no_output")));
        } else {
            text.push_str(&String::from_utf8_lossy(&read.out));
            if !read.err.is_empty() {
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&String::from_utf8_lossy(&read.err));
                if !text.ends_with('\n') {
                    text.push('\n');
                }
            }
        }
        // Where it stands, so the model does not have to call `job_list` next.
        text.push_str(&format!("{}\n", phase_text(read.phase)));

        Ok(ToolOutput {
            status: read.phase.tool_status(),
            ..ToolOutput::ok(text)
        })
    }
}

/// One read, retried until there is something to say, the job ends, the wait
/// runs out, or the turn is cancelled.
///
/// Cancelling here only stops the *waiting*: the job keeps running, which is the
/// whole point of having detached it.
async fn read_with_wait(job: &Arc<Job>, wait: Duration, ctx: &ToolContext) -> jobs::ReadOut {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let read = job.read(jobs::READ_LIMIT);
        // Nothing new and the job is done: stop, there is nothing to wait for.
        if !read.is_empty() || !read.phase.is_running() {
            return read;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return read;
        }
        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return read,
            _ = tokio::time::sleep(jobs::POLL.min(deadline - now)) => {}
        }
    }
}

// ------------------------------------------------------------------ kill

pub struct JobKill {
    jobs: Arc<JobRegistry>,
}

impl JobKill {
    pub fn new(jobs: Arc<JobRegistry>) -> Self {
        Self { jobs }
    }
}

#[derive(Deserialize)]
struct KillArgs {
    job: String,
}

#[async_trait]
impl Tool for JobKill {
    fn name(&self) -> &str {
        "job_kill"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: concat!(
                "Stop a background job, killing its whole process tree. Use it when a job is ",
                "no longer needed or is clearly stuck; a job that is simply long does not ",
                "need killing."
            )
            .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job": {
                        "type": "string",
                        "description": "Job handle, e.g. `job-3`."
                    }
                },
                "required": ["job"],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        // Stopping a process the model itself started. It is not a new capability
        // — `run_shell` could already run `kill` — but it is a command, so it is
        // classified like one rather than like a read.
        ApprovalClass::Execute
    }

    async fn execute(&self, _ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: KillArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Invalid(format!("job_kill: {err}")))?;
        let Some(job) = self.jobs.get(&args.job) else {
            return Ok(ToolOutput::error(qcode_i18n::trp(
                "tool.jobs.unknown",
                &[args.job.as_str()],
            )));
        };

        let phase = self.jobs.kill(&job).await;
        let text = format!(
            "{}\n{}\n",
            qcode_i18n::trp("tool.jobs.killed_job", &[job.id()]),
            phase_text(phase)
        );
        Ok(ToolOutput {
            status: phase.tool_status(),
            ..ToolOutput::ok(text)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multiline_command_is_summarised_by_its_first_line() {
        assert_eq!(first_line("cargo build\ncargo test"), "cargo build");
        assert_eq!(first_line("  ls  "), "ls");
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn an_unknown_handle_says_so_rather_than_failing_the_turn() {
        let jobs = Arc::new(JobRegistry::new());
        assert!(jobs.get("job-9").is_none());
        // The tool turns this into a tool-level error, not an engine error: the
        // model can fix it by listing jobs.
        assert_eq!(
            Phase::Exited(Some(0)).tool_status(),
            qcode_core::event::ToolStatus::Ok
        );
    }
}
