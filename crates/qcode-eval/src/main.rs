//! Unattended evaluation runner for qcode.
//!
//! Each task is a directory under `eval/tasks/<id>/`:
//!
//! - `task.toml`: prompt, limits and check commands;
//! - `planted/` (optional): files copied over the fixture to create the broken
//!   starting state;
//! - `hidden/` (optional): tests copied in after the agent finishes.
//!
//! The runner copies `eval/fixtures/workspace` to a temporary directory, applies
//! the planted overlay, runs `qc run` headless, restores tests/scripts/AGENTS.md
//! to their snapshot state, injects the hidden tests, then runs the checks.
//! Results and event logs land in `eval/runs/<timestamp>/`.
//!
//! `--agent-cmd` replaces the agent invocation with a shell command. It receives
//! `QC_EVAL_WORKSPACE` and `QC_EVAL_PROMPT` in the environment and is intended
//! for testing the harness itself.

use std::env;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

type Result<T> = std::result::Result<T, String>;

const USAGE: &str = "\
usage: qc-eval [options]

  --list                 print discovered tasks and exit
  --task ID              run one task (default: all)
  --eval-dir DIR         eval root (default: <repo>/eval)
  --runs DIR             run output directory (default: <eval-dir>/runs)
  --qc PATH              qc binary (default: <repo>/target/debug/qc)
  --agent-cmd CMD        replace the agent with `bash -c CMD` (harness tests)
  --keep                 keep temporary workspaces even when a task passes
  -h, --help             print this help
";

/// Snapshot-relative paths that are always restored before checking. Tests,
/// scripts and AGENTS.md are inputs, not part of the agent's answer.
const RESTORED: &[&str] = &[
    "AGENTS.md",
    "scripts",
    "core/tests",
    "store/tests",
    "cli/tests",
];

#[derive(Debug, Clone, Deserialize)]
struct Task {
    id: String,
    prompt: String,
    #[serde(default = "default_snapshot")]
    snapshot: String,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    #[serde(default = "default_turns")]
    max_turns: u32,
    #[serde(default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    checks: Vec<Check>,
}

#[derive(Debug, Clone, Deserialize)]
struct Check {
    cmd: String,
    #[serde(default)]
    expect_exit: i32,
}

fn default_snapshot() -> String {
    "v1".to_string()
}

fn default_timeout() -> u64 {
    600
}

fn default_turns() -> u32 {
    40
}

#[derive(Debug, Clone)]
struct TaskEntry {
    dir: PathBuf,
    task: Task,
}

#[derive(Debug, Default, Clone)]
struct Metrics {
    turns: u32,
    tool_calls: u32,
    approvals: u32,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
    wall_ms: u64,
    events_bytes: u64,
}

#[derive(Debug)]
struct CheckOutcome {
    cmd: String,
    expect_exit: i32,
    exit: Option<i32>,
    timed_out: bool,
    passed: bool,
    log: PathBuf,
}

#[derive(Debug)]
struct TaskResult {
    id: String,
    snapshot: String,
    passed: bool,
    checks: Vec<CheckOutcome>,
    metrics: Metrics,
    workspace: PathBuf,
    error: Option<String>,
}

struct Options {
    eval_dir: PathBuf,
    runs_dir: PathBuf,
    qc: PathBuf,
    agent_cmd: Option<String>,
    only: Option<String>,
    keep: bool,
    list: bool,
}

struct Outcome {
    status: Option<ExitStatus>,
    timed_out: bool,
}

impl Outcome {
    fn exit_code(&self) -> Option<i32> {
        self.status.and_then(|status| status.code())
    }
}

fn main() {
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("qc-eval: {message}");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    };

    if let Err(message) = run(options) {
        eprintln!("qc-eval: {message}");
        std::process::exit(1);
    }
}

fn run(options: Options) -> Result<()> {
    let fixture = options.eval_dir.join("fixtures/workspace");
    if !fixture.is_dir() {
        return Err(format!("fixture not found: {}", fixture.display()));
    }

    let mut tasks = load_tasks(&options.eval_dir.join("tasks"))?;
    if let Some(only) = &options.only {
        tasks.retain(|entry| &entry.task.id == only);
        if tasks.is_empty() {
            return Err(format!("unknown task `{only}`"));
        }
    }
    if tasks.is_empty() {
        return Err("no tasks found".to_string());
    }

    if options.list {
        for entry in &tasks {
            let tools = if entry.task.allowed_tools.is_empty() {
                "all".to_string()
            } else {
                entry.task.allowed_tools.join(",")
            };
            println!(
                "{}  snapshot={}  turns<={}  tools={tools}",
                entry.task.id, entry.task.snapshot, entry.task.max_turns
            );
            println!("    {}", entry.task.prompt.trim().replace('\n', " "));
        }
        return Ok(());
    }

    let run_dir = options.runs_dir.join(run_stamp());
    fs::create_dir_all(&run_dir)
        .map_err(|err| format!("cannot create {}: {err}", run_dir.display()))?;

    if options.agent_cmd.is_none() && !options.qc.is_file() {
        return Err(format!(
            "qc binary not found at {}; run `cargo build -p qcode` or pass --qc",
            options.qc.display()
        ));
    }

    println!(
        "qc-eval: {} task(s), logs in {}",
        tasks.len(),
        run_dir.display()
    );

    let mut results = Vec::new();
    for entry in &tasks {
        println!("[run ] {}", entry.task.id);
        let result = run_task(entry, &fixture, &options, &run_dir);
        match result {
            Ok(result) => {
                print_result(&result);
                results.push(result);
            }
            Err(message) => {
                println!("[ERR ] {}: {message}", entry.task.id);
                results.push(TaskResult {
                    id: entry.task.id.clone(),
                    snapshot: entry.task.snapshot.clone(),
                    passed: false,
                    checks: Vec::new(),
                    metrics: Metrics::default(),
                    workspace: PathBuf::new(),
                    error: Some(message),
                });
            }
        }
    }

    write_summary(&run_dir, &options, &results)?;
    let passed = results.iter().filter(|result| result.passed).count();
    println!(
        "\n{passed}/{} passed · summary: {}",
        results.len(),
        run_dir.join("summary.md").display()
    );

    if passed == results.len() {
        Ok(())
    } else {
        Err(format!(
            "{}/{} tasks failed",
            results.len() - passed,
            results.len()
        ))
    }
}

fn load_tasks(tasks_dir: &Path) -> Result<Vec<TaskEntry>> {
    let entries = fs::read_dir(tasks_dir)
        .map_err(|err| format!("cannot read {}: {err}", tasks_dir.display()))?;

    let mut tasks = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("cannot read {}: {err}", tasks_dir.display()))?;
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path().join("task.toml");
        if !path.is_file() {
            continue;
        }
        let text = fs::read_to_string(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        let task: Task =
            toml::from_str(&text).map_err(|err| format!("invalid {}: {err}", path.display()))?;
        if task.checks.is_empty() {
            return Err(format!("{} has no checks", path.display()));
        }
        tasks.push(TaskEntry {
            dir: entry.path(),
            task,
        });
    }
    tasks.sort_by(|left, right| left.task.id.cmp(&right.task.id));
    Ok(tasks)
}

fn run_task(
    entry: &TaskEntry,
    fixture: &Path,
    options: &Options,
    run_dir: &Path,
) -> Result<TaskResult> {
    let task = &entry.task;
    let workspace = env::temp_dir().join(format!("qc-eval-{}-{}", task.id, std::process::id()));
    remove_path(&workspace)?;
    fs::create_dir_all(&workspace)
        .map_err(|err| format!("cannot create {}: {err}", workspace.display()))?;

    let log_dir = run_dir.join(&task.id);
    fs::create_dir_all(&log_dir)
        .map_err(|err| format!("cannot create {}: {err}", log_dir.display()))?;

    prepare_workspace(entry, fixture, &workspace)?;

    let events = log_dir.join("events.jsonl");
    let agent_stderr = log_dir.join("agent.stderr");
    let start = Instant::now();
    let agent = run_agent(task, &workspace, options, &events, &agent_stderr)?;
    let wall = start.elapsed();

    let mut error = None;
    if agent.timed_out {
        error = Some(format!("agent timed out after {}s", task.timeout_secs));
    } else if agent.exit_code() != Some(0) {
        error = Some(format!(
            "agent exited with {:?}",
            agent
                .exit_code()
                .map_or_else(|| "<signal>".to_string(), |code| code.to_string())
        ));
    }

    restore_and_inject(entry, fixture, &workspace)?;

    let mut checks = Vec::new();
    let mut passed = error.is_none();
    for (index, check) in task.checks.iter().enumerate() {
        let log = log_dir.join(format!("check-{}.log", index + 1));
        let outcome = run_check(&workspace, check, &log)?;
        passed &= outcome.passed;
        checks.push(outcome);
        if !passed {
            break;
        }
    }

    let metrics = parse_metrics(&events, wall);

    if passed && !options.keep {
        let _ = fs::remove_dir_all(&workspace);
    }

    Ok(TaskResult {
        id: task.id.clone(),
        snapshot: task.snapshot.clone(),
        passed,
        checks,
        metrics,
        workspace,
        error,
    })
}

fn prepare_workspace(entry: &TaskEntry, fixture: &Path, workspace: &Path) -> Result<()> {
    copy_tree(fixture, workspace)?;
    let planted = entry.dir.join("planted");
    if planted.is_dir() {
        copy_tree(&planted, workspace)?;
    }
    Ok(())
}

/// Restore the snapshot versions of tests/scripts/AGENTS.md, then overlay the
/// task's hidden tests.
fn restore_and_inject(entry: &TaskEntry, fixture: &Path, workspace: &Path) -> Result<()> {
    for relative in RESTORED {
        let source = fixture.join(relative);
        let target = workspace.join(relative);
        remove_path(&target)?;
        if source.exists() {
            copy_path(&source, &target)?;
        }
    }

    let hidden = entry.dir.join("hidden");
    if hidden.is_dir() {
        copy_tree(&hidden, workspace)?;
    }
    Ok(())
}

fn run_agent(
    task: &Task,
    workspace: &Path,
    options: &Options,
    events: &Path,
    stderr: &Path,
) -> Result<Outcome> {
    let cargo_home = workspace.join(".cargo-home");
    fs::create_dir_all(&cargo_home)
        .map_err(|err| format!("cannot create {}: {err}", cargo_home.display()))?;

    let mut command = if let Some(agent) = &options.agent_cmd {
        let mut command = Command::new("bash");
        command
            .arg("-c")
            .arg(agent)
            .env("QC_EVAL_WORKSPACE", workspace)
            .env("QC_EVAL_PROMPT", task.prompt.trim());
        command
    } else {
        let mut command = Command::new(&options.qc);
        command
            .arg("run")
            .arg(task.prompt.trim())
            .arg("--cwd")
            .arg(workspace)
            .arg("--json")
            .arg("--yes")
            .arg("--max-turns")
            .arg(task.max_turns.to_string())
            .arg("--timeout")
            .arg(task.timeout_secs.to_string())
            .arg("--data-dir")
            .arg(workspace.join(".qc-data"));
        command
    };

    command
        .current_dir(workspace)
        .env("CARGO_HOME", &cargo_home)
        .env("CARGO_TERM_COLOR", "never")
        .env_remove("QCODE_PROVIDER")
        .env_remove("QCODE_MODEL")
        .env_remove("QCODE_BASE_URL")
        .env_remove("QCODE_DATA_DIR")
        .env_remove("RUST_LOG");

    run_with_timeout(
        command,
        Duration::from_secs(task.timeout_secs),
        events,
        stderr,
    )
}

fn run_check(workspace: &Path, check: &Check, log: &Path) -> Result<CheckOutcome> {
    let cargo_home = workspace.join(".cargo-home");
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(&check.cmd)
        .current_dir(workspace)
        .env("CARGO_HOME", cargo_home)
        .env("CARGO_TERM_COLOR", "never")
        .env_remove("RUST_LOG");

    let output = run_with_timeout(
        command,
        Duration::from_secs(300),
        log,
        &log.with_extension("err"),
    )?;
    let exit = output.exit_code();
    let passed = !output.timed_out && exit == Some(check.expect_exit);

    Ok(CheckOutcome {
        cmd: check.cmd.clone(),
        expect_exit: check.expect_exit,
        exit,
        timed_out: output.timed_out,
        passed,
        log: log.to_path_buf(),
    })
}

fn run_with_timeout(
    mut command: Command,
    timeout: Duration,
    stdout: &Path,
    stderr: &Path,
) -> Result<Outcome> {
    let out = fs::File::create(stdout)
        .map_err(|err| format!("cannot create {}: {err}", stdout.display()))?;
    let err = fs::File::create(stderr)
        .map_err(|err| format!("cannot create {}: {err}", stderr.display()))?;

    let mut child = command
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .map_err(|error| format!("cannot start agent: {error}"))?;

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(Outcome {
                    status: Some(status),
                    timed_out: false,
                });
            }
            Ok(None) => {}
            Err(error) => return Err(format!("cannot wait for agent: {error}")),
        }

        if start.elapsed() > timeout {
            let _ = child.kill();
            let status = child.wait().ok();
            return Ok(Outcome {
                status,
                timed_out: true,
            });
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn parse_metrics(events: &Path, wall: Duration) -> Metrics {
    let mut metrics = Metrics {
        wall_ms: wall.as_millis().min(u128::from(u64::MAX)) as u64,
        ..Metrics::default()
    };

    let Ok(text) = fs::read_to_string(events) else {
        return metrics;
    };
    metrics.events_bytes = text.len() as u64;

    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value["type"].as_str() {
            Some("turn_started") => metrics.turns += 1,
            Some("tool_call") => metrics.tool_calls += 1,
            Some("approval_requested") => metrics.approvals += 1,
            Some("assistant_message") => {
                let usage = &value["data"]["usage"];
                metrics.input_tokens += usage["input_tokens"].as_u64().unwrap_or(0);
                metrics.output_tokens += usage["output_tokens"].as_u64().unwrap_or(0);
            }
            Some("turn_ended") => {
                metrics.cost_usd += value["data"]["cost_usd"].as_f64().unwrap_or(0.0);
            }
            _ => {}
        }
    }

    metrics
}

fn print_result(result: &TaskResult) {
    let label = if result.passed { "PASS" } else { "FAIL" };
    let metrics = &result.metrics;
    println!(
        "[{label}] {}  turns={} tools={} approvals={} tokens={}/{} cost=${:.4} wall={:.1}s",
        result.id,
        metrics.turns,
        metrics.tool_calls,
        metrics.approvals,
        metrics.input_tokens,
        metrics.output_tokens,
        metrics.cost_usd,
        metrics.wall_ms as f64 / 1000.0,
    );

    if let Some(error) = &result.error {
        println!("       {error}");
    }
    for check in &result.checks {
        if !check.passed {
            if check.timed_out {
                println!("       check timed out: {}", check.cmd);
            } else {
                println!(
                    "       check failed (exit {:?}, expected {}): {}",
                    check.exit, check.expect_exit, check.cmd
                );
            }
            println!("       log: {}", check.log.display());
        }
    }
    if !result.passed && !result.workspace.as_os_str().is_empty() {
        println!("       workspace kept: {}", result.workspace.display());
    }
}

fn write_summary(run_dir: &Path, options: &Options, results: &[TaskResult]) -> Result<()> {
    let mut text = String::new();
    let _ = writeln!(text, "# qc-eval run {}\n", run_stamp());
    let _ = writeln!(text, "- qc: `{}`", options.qc.display());
    let _ = writeln!(text, "- eval dir: `{}`", options.eval_dir.display());
    let _ = writeln!(text, "- tasks: {}\n", results.len());

    let _ = writeln!(
        text,
        "| task | result | snapshot | turns | tools | approvals | input | output | cost | wall |"
    );
    let _ = writeln!(text, "|---|---|---|---|---|---|---|---|---|---|");
    for result in results {
        let metrics = &result.metrics;
        let label = if result.passed { "pass" } else { "fail" };
        let _ = writeln!(
            text,
            "| {} | {} | {} | {} | {} | {} | {} | {} | ${:.4} | {:.1}s |",
            result.id,
            label,
            result.snapshot,
            metrics.turns,
            metrics.tool_calls,
            metrics.approvals,
            metrics.input_tokens,
            metrics.output_tokens,
            metrics.cost_usd,
            metrics.wall_ms as f64 / 1000.0,
        );
    }

    for result in results {
        if let Some(error) = &result.error {
            let _ = writeln!(text, "\n## {}\n\n- {error}", result.id);
        }
        if let Some(failure) = result.checks.iter().find(|check| !check.passed) {
            let _ = writeln!(
                text,
                "\n## {}\n\n- check: `{}` (exit {:?}, expected {})\n- log: `{}`",
                result.id,
                failure.cmd,
                failure.exit,
                failure.expect_exit,
                failure.log.display()
            );
        }
    }

    let path = run_dir.join("summary.md");
    fs::write(&path, text).map_err(|err| format!("cannot write {}: {err}", path.display()))
}

fn copy_path(source: &Path, target: &Path) -> Result<()> {
    if source.is_dir() {
        fs::create_dir_all(target)
            .map_err(|err| format!("cannot create {}: {err}", target.display()))?;
        copy_tree(source, target)
    } else {
        fs::copy(source, target)
            .map(|_| ())
            .map_err(|err| format!("cannot copy {}: {err}", source.display()))
    }
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target)
        .map_err(|err| format!("cannot create {}: {err}", target.display()))?;

    let entries =
        fs::read_dir(source).map_err(|err| format!("cannot read {}: {err}", source.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("cannot read {}: {err}", source.display()))?;
        let name = entry.file_name();
        if skip(&name) {
            continue;
        }
        let path = entry.path();
        let destination = target.join(&name);
        copy_path(&path, &destination)?;
    }
    Ok(())
}

/// Build products and local agent state never belong in a snapshot.
fn skip(name: &OsStr) -> bool {
    name == "target" || name == ".git" || name == ".qc-data" || name == ".cargo-home"
}

fn remove_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)
            .map_err(|err| format!("cannot remove {}: {err}", path.display())),
        Ok(_) => {
            fs::remove_file(path).map_err(|err| format!("cannot remove {}: {err}", path.display()))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("cannot inspect {}: {err}", path.display())),
    }
}

fn run_stamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("{seconds}-{}", std::process::id())
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn parse_args() -> Result<Options> {
    let root = repo_root();
    let mut options = Options {
        eval_dir: root.join("eval"),
        runs_dir: root.join("eval/runs"),
        qc: root.join("target/debug/qc"),
        agent_cmd: None,
        only: None,
        keep: false,
        list: false,
    };

    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        let mut next = |flag: &str| args.next().ok_or_else(|| format!("{flag} needs a value"));
        match argument.as_str() {
            "--list" => options.list = true,
            "--keep" => options.keep = true,
            "--eval-dir" => options.eval_dir = PathBuf::from(next("--eval-dir")?),
            "--runs" => options.runs_dir = PathBuf::from(next("--runs")?),
            "--qc" => options.qc = PathBuf::from(next("--qc")?),
            "--task" => options.only = Some(next("--task")?),
            "--agent-cmd" => options.agent_cmd = Some(next("--agent-cmd")?),
            "--help" | "-h" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }

    if !options.runs_dir.is_absolute() {
        options.runs_dir = root.join(&options.runs_dir);
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("qc-eval-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn parses_a_task_and_defaults() {
        let task: Task = toml::from_str(
            r#"
            id = "01-demo"
            prompt = "fix it"
            [[checks]]
            cmd = "cargo test"
            "#,
        )
        .expect("parse task");

        assert_eq!(task.id, "01-demo");
        assert_eq!(task.snapshot, "v1");
        assert_eq!(task.timeout_secs, 600);
        assert_eq!(task.max_turns, 40);
        assert_eq!(task.checks.len(), 1);
        assert_eq!(task.checks[0].expect_exit, 0);
    }

    #[test]
    fn metrics_count_engine_events() {
        let dir = temp_dir("metrics");
        fs::create_dir_all(&dir).unwrap();
        let events = dir.join("events.jsonl");
        let lines = [
            r#"{"type":"turn_started","data":{"input":"hi"}}"#,
            r#"{"type":"tool_call","data":{"name":"read_file"}}"#,
            r#"{"type":"approval_requested","data":{}}"#,
            r#"{"type":"assistant_message","data":{"usage":{"input_tokens":10,"output_tokens":4}}}"#,
            r#"{"type":"turn_ended","data":{"cost_usd":0.25}}"#,
            r#"not json"#,
        ];
        fs::write(&events, lines.join("\n")).unwrap();

        let metrics = parse_metrics(&events, Duration::from_millis(1500));
        assert_eq!(metrics.turns, 1);
        assert_eq!(metrics.tool_calls, 1);
        assert_eq!(metrics.approvals, 1);
        assert_eq!(metrics.input_tokens, 10);
        assert_eq!(metrics.output_tokens, 4);
        assert!((metrics.cost_usd - 0.25).abs() < f64::EPSILON);
        assert_eq!(metrics.wall_ms, 1500);
    }

    #[test]
    fn copy_tree_skips_build_directories_and_overlays_files() {
        let source = temp_dir("copy-src");
        let target = temp_dir("copy-dst");
        fs::create_dir_all(source.join("crate/src")).unwrap();
        fs::create_dir_all(source.join("target")).unwrap();
        fs::write(source.join("crate/src/lib.rs"), "base").unwrap();
        fs::write(source.join("target/junk"), "junk").unwrap();

        copy_tree(&source, &target).unwrap();
        assert!(target.join("crate/src/lib.rs").is_file());
        assert!(!target.join("target").exists());

        let overlay = temp_dir("copy-overlay");
        fs::create_dir_all(overlay.join("crate/src")).unwrap();
        fs::write(overlay.join("crate/src/lib.rs"), "planted").unwrap();
        copy_tree(&overlay, &target).unwrap();

        assert_eq!(
            fs::read_to_string(target.join("crate/src/lib.rs")).unwrap(),
            "planted"
        );
    }
}
