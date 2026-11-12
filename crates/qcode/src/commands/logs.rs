use super::*;

/// `qc logs`: say where the log is, then show its tail.
///
/// The path comes first and unconditionally, because that is what a bug report
/// needs ("attach this file") and what a user cannot guess — the file lives in the
/// data directory, which is not somewhere they browse. Tailing is the convenience
/// on top; `--path-only` turns it off for scripts that just want the name.
pub(crate) fn cmd_logs(
    args: LogsArgs,
    data_dir: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let data_dir = qcode_kernel::resolve_data_dir(data_dir);
    let dir = logging::log_dir(&data_dir);
    let current = logging::current_log(&data_dir);

    println!("{}", current.display());

    if args.path_only {
        return Ok(ExitCode::SUCCESS);
    }

    let files = logging::log_files(&data_dir);
    if files.is_empty() {
        // Not an error: file logging may never have run in this data directory,
        // and a first `qc logs` before the first turn is a normal thing to do.
        println!("no log files in {}", dir.display());
        return Ok(ExitCode::SUCCESS);
    }

    // Naming the older files is the point of rotating them: a bug that showed up
    // last week is in a different file than today's.
    if files.len() > 1 {
        println!("(older files in {}: {})", dir.display(), files.len() - 1);
    }

    let text = std::fs::read_to_string(&current)
        .map_err(|error| format!("cannot read {}: {error}", current.display()))?;
    let skip = text.lines().count().saturating_sub(args.lines);
    for line in text.lines().skip(skip) {
        println!("{line}");
    }

    Ok(ExitCode::SUCCESS)
}
