//! `qc schedule list|create|delete` (ADR-0090).
//!
//! The thin end of ADR-0088's argument: registering a scheduled task needs only
//! a store, so this file reads and writes the same `Scheduler` the tools and the
//! GUI use, and owns nothing of its own. What it *does* own is the honesty
//! requirement — a task that will never fire because no driver is running must
//! say so on the spot, because whoever typed the command may never have opened
//! the GUI at all.
//!
//! Deliberately **not** sharing the tools' rendering: `qcode-tools` is the tool
//! implementation crate and the CLI only consumes the state (`qcode-core`). The
//! driver footer *is* shared (§0.2) because it is a three-part claim about
//! behaviour, not formatting.

use super::*;
use qcode_core::schedule::{Target, TaskStatus, UNATTENDED_APPROVAL_TIMEOUT};
use qcode_core::scheduler::{
    DriverStatus, NewTask, ScheduleError, Scheduler, approval_footer, driver_footer,
};
use std::fmt::Write as _;

pub(crate) fn cmd_schedule(
    args: ScheduleArgs,
    data_dir: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    // The hint, not `resolve_data_dir`: `list` on a machine that has never run
    // qc must report "nothing here" rather than create a data directory to hold
    // that fact (ADR-0090 §8). `create` makes one because it has to.
    let store = SessionStore::new(qcode_kernel::data_dir_hint(data_dir));
    let scheduler = Scheduler::new(store.clone());

    match args.command {
        ScheduleCommand::List { all, json } => cmd_list(&store, &scheduler, all, json),
        ScheduleCommand::Create {
            title,
            prompt,
            rule,
            target,
            cwd,
            overlap,
            approval_timeout_secs,
            json,
        } => cmd_create(
            &store,
            &scheduler,
            NewTaskArgs {
                title,
                prompt,
                rule,
                target,
                cwd,
                overlap,
                approval_timeout_secs,
            },
            json,
        ),
        ScheduleCommand::Delete { id } => cmd_delete(&store, &scheduler, &id),
    }
}

/// 驱动状态（ADR-0091）。
///
/// 门都在 `DriverStatus::read` 里面了：探锁看 `locks/` 在不在，读心跳看
/// `index.sqlite` 在不在，两者都**不建目录**。这里只负责转达。
fn driver_now(store: &SessionStore) -> DriverStatus {
    DriverStatus::read(store, qcode_core::schedule::now_local())
}

fn cmd_list(
    store: &SessionStore,
    scheduler: &Scheduler,
    all: bool,
    json: bool,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let driver = driver_now(store);
    let mut tasks = scheduler.list();

    if json {
        // `--json` is always exhaustive: a script that has to pass `--all` to see
        // a paused task is a script that will silently miss it.
        let report = serde_json::json!({
            "driver": driver_json(driver),
            "tasks": tasks,
        });
        println!("{report}");
        return Ok(ExitCode::SUCCESS);
    }

    if !all {
        tasks.retain(|task| task.status == TaskStatus::Active);
    }

    let mut report = String::new();
    // Driver state first: it is the answer to "will any of this actually run",
    // and it has to be readable without scrolling (same order as the tool).
    let _ = writeln!(
        report,
        "{}",
        qcode_i18n::trp("core.driver.footer", &[driver.describe().as_ref()])
    );
    if tasks.is_empty() {
        if all {
            report.push_str(&format!(
                "{}\n",
                qcode_i18n::tr("core.schedule.list_empty_all")
            ));
        } else {
            report.push_str(&format!(
                "{}\n",
                qcode_i18n::tr("core.schedule.list_empty_active_only")
            ));
        }
        write_report(&report)?;
        return Ok(ExitCode::SUCCESS);
    }
    let _ = writeln!(
        report,
        "{}",
        qcode_i18n::trp("core.schedule.list_header", &[tasks.len().to_string()])
    );
    for task in &tasks {
        // 与 `schedule_list` 工具共用同一条模板：同一个任务在 CLI 与工具结果里
        // 长相不一致，是最容易被漏掉的那类不一致。
        let _ = write!(
            report,
            "{}",
            qcode_i18n::trp(
                "core.schedule.row",
                &[
                    task.status.as_str(),
                    task.title.as_str(),
                    task.id.as_str(),
                    task.rule.describe().as_str(),
                    task.target.describe().as_str(),
                    task.next_fire_at
                        .map_or_else(|| "-".to_string(), |at| at.to_string())
                        .as_str(),
                ],
            )
        );
        if let Some(cwd) = &task.cwd {
            let _ = write!(
                report,
                "{}",
                qcode_i18n::trp("core.schedule.row_dir", &[cwd.as_str()])
            );
        }
        report.push('\n');
    }
    write_report(&report)?;
    Ok(ExitCode::SUCCESS)
}

/// The `create` half, gathered so the signature above stays readable.
struct NewTaskArgs {
    title: String,
    prompt: String,
    rule: String,
    target: Option<String>,
    cwd: Option<PathBuf>,
    overlap: Option<String>,
    approval_timeout_secs: Option<u64>,
}

fn cmd_create(
    store: &SessionStore,
    scheduler: &Scheduler,
    args: NewTaskArgs,
    json: bool,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    // Every parse happens before anything touches the disk, so a bad rule leaves
    // no half-created task behind (ADR-0090 验证 7).
    let rule = qcode_core::schedule::parse_rule(&args.rule)
        .map_err(|error| -> Box<dyn std::error::Error> { error.to_string().into() })?;
    let target = Target::parse(args.target.as_deref().unwrap_or("")).ok_or_else(|| {
        qcode_i18n::trp(
            "err.cli.schedule_target.invalid",
            &[format!("{:?}", args.target.unwrap_or_default()).as_str()],
        )
    })?;
    let overlap = match args.overlap.as_deref() {
        None | Some("") => qcode_core::schedule::Overlap::Queue,
        Some(text) => qcode_core::schedule::Overlap::parse(text).ok_or_else(|| {
            qcode_i18n::trp(
                "err.cli.schedule.overlap_invalid",
                &[format!("{text:?}").as_str()],
            )
        })?,
    };
    let cwd = resolve_cwd(args.cwd)?;

    let task = scheduler
        .create(
            NewTask {
                title: args.title,
                prompt: args.prompt,
                rule,
                target,
                overlap,
                approval_timeout: args
                    .approval_timeout_secs
                    .map(std::time::Duration::from_secs),
                cwd: Some(cwd.display().to_string()),
            },
            qcode_core::schedule::now_local(),
        )
        .map_err(|error| -> Box<dyn std::error::Error> { error.to_string().into() })?;

    let driver = driver_now(store);
    if json {
        let report = serde_json::json!({
            "driver": driver_json(driver),
            // ADR-0092 §7：`timeout_secs` 是任务声明的，`unattended_default_secs`
            // 是无人值守执行时会补上的值（只在没声明时非 null）。两个都给出，
            // 脚本才不用去猜「null 是永远等，还是会补」。
            "approval": {
                "timeout_secs": task.approval_timeout.map(|timeout| timeout.as_secs()),
                "unattended_default_secs": task
                    .approval_timeout
                    .is_none()
                    .then_some(UNATTENDED_APPROVAL_TIMEOUT.as_secs()),
            },
            "task": task,
        });
        println!("{report}");
        return Ok(ExitCode::SUCCESS);
    }

    let mut report = String::new();
    let _ = writeln!(
        report,
        "{}",
        qcode_i18n::trp(
            "cli.schedule.created",
            &[task.title.as_str(), task.id.as_str()]
        )
    );
    let _ = writeln!(
        report,
        "{}",
        qcode_i18n::trp("core.schedule.field.rule", &[task.rule.describe().as_str()])
    );
    let _ = writeln!(
        report,
        "{}",
        qcode_i18n::trp(
            "core.schedule.field.target",
            &[task.target.describe().as_str()]
        )
    );
    // Echo the directory: the default is "wherever you were standing", and a
    // task that fires somewhere unexpected is the failure this line prevents.
    let _ = writeln!(
        report,
        "{}",
        qcode_i18n::trp("cli.schedule.cwd", &[cwd.display().to_string()])
    );
    let _ = writeln!(
        report,
        "{}",
        qcode_i18n::trp(
            "core.schedule.field.next_fire",
            &[task
                .next_fire_at
                .map_or_else(|| "-".to_string(), |at| at.to_string())
                .as_str()]
        )
    );
    // ADR-0092 §7：缺省值不能静默补。`None` 在有人值守与无人值守下是两种
    // 结果，所以这条必须一直印出来，而不是只在设了值时才提一句。
    let _ = writeln!(report, "{}", approval_footer(task.approval_timeout));
    let _ = writeln!(report);
    report.push_str(&driver_footer(driver));
    write_report(&report)?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_delete(
    store: &SessionStore,
    scheduler: &Scheduler,
    id: &str,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match scheduler.delete(id) {
        Ok(()) => {}
        Err(ScheduleError::NotFound(id)) => {
            // Non-zero: a script deleting an id that is not there meant to do
            // something that did not happen.
            eprintln!(
                "{}",
                qcode_i18n::trp("core.schedule.not_found_by_id", &[id.as_str()])
            );
            return Ok(ExitCode::from(1));
        }
        Err(error) => {
            eprintln!(
                "{}",
                qcode_i18n::trp("cli.schedule.delete_failed", &[error.to_string()])
            );
            return Ok(ExitCode::from(1));
        }
    }
    let mut report = format!("{}\n", qcode_i18n::trp("cli.schedule.deleted", &[id]));
    report.push_str(&driver_footer(driver_now(store)));
    write_report(&report)?;
    Ok(ExitCode::SUCCESS)
}

/// Where the delivered turn will run (ADR-0090 §2).
///
/// The default is the current directory, **except** that a filesystem root is not
/// a usable answer: it would become the task's primary sandbox root, and the
/// sandbox refuses `/`. Today that is a loud failure (`qc run` from `/` dies at
/// setup); a scheduled task would defer it to a background turn days later.
/// Same value, same rule — one of them just fails where nobody is looking.
fn resolve_cwd(requested: Option<PathBuf>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = requested {
        let absolute = std::path::absolute(&path)?;
        if !absolute.is_dir() {
            return Err(qcode_i18n::trp(
                "cli.schedule.cwd_not_a_dir",
                &[absolute.display().to_string()],
            )
            .into());
        }
        return Ok(absolute);
    }
    let here = std::env::current_dir()?;
    let (workspace, fell_back) =
        qcode_core::choose_workspace(here.clone(), qcode_core::real_home());
    if fell_back {
        // Say it on stderr so `--json` on stdout stays parseable.
        eprintln!(
            "{}",
            qcode_i18n::trp(
                "cli.schedule.fs_root_note",
                &[here.display().to_string(), workspace.display().to_string()]
            )
        );
    }
    Ok(workspace)
}

/// `--json` 里的驱动状态（ADR-0090 §6 的稳定面，四态见 ADR-0091 §2）。
///
/// `state` 四值：`running`（在跑且醒着）/ `held`（在跑但心跳过期）/ `gone`
/// （来过又走了）/ `unknown`（从没来过）。多出 `held` 是刻意的：把它并进
/// `running` 会掩盖「可能已卡住」，并进 `gone` 则会谎称「不补跑」。
fn driver_json(driver: DriverStatus) -> serde_json::Value {
    let state = match driver {
        DriverStatus::Running { .. } => "running",
        DriverStatus::Held { .. } => "held",
        DriverStatus::Gone { .. } => "gone",
        DriverStatus::Unknown => "unknown",
    };
    let at = match driver {
        DriverStatus::Running { at } | DriverStatus::Held { at: Some(at) } => Some(at.to_string()),
        DriverStatus::Held { at: None } | DriverStatus::Gone { .. } | DriverStatus::Unknown => None,
    };
    serde_json::json!({ "state": state, "at": at })
}
