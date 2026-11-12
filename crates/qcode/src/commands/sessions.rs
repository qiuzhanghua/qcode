use super::*;

/// Write a whole report, treating a closed pipe as "the reader has seen enough".
///
/// `qc sessions list | head -40` is an ordinary thing to type, and `println!`
/// panics inside the stdlib when the pipe closes first — ADR-0080 fixed exactly
/// this for `qc logs --follow`. Building the report first also means a command
/// that fails halfway does not leave half a listing in the pipe.
fn write_report(buffer: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = std::io::stdout().lock();
    match write_all_report(&mut out, buffer) {
        Ok(()) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// The part of [`write_report`] that decides what a closed pipe means.
///
/// Split out with an injectable writer so the decision can be tested without a
/// real `| head`: whether the pipe is already closed when we write is a race
/// that a test cannot rely on.
fn write_all_report(out: &mut dyn std::io::Write, buffer: &str) -> std::io::Result<()> {
    match out.write_all(buffer.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        // The reader went away (`| head`). Not an error.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error),
    }
}

/// The name the sidebar shows for a session.
///
/// An explicit rename wins over the first user message, and the last rename wins
/// over earlier ones, so the whole log decides — that is what the sidebar
/// renders (`Backend::title` only knows the fallback). `None` when the session
/// never got a name at all.
fn task_title(store: &SessionStore, id: &str) -> Option<String> {
    match store.load(id) {
        Ok(events) => task_title_from_events(&events),
        // A log that cannot be read is a naming problem, not a listing problem.
        Err(error) => {
            tracing::debug!(%error, session = %id, "cannot read session while resolving a title");
            None
        }
    }
}

fn task_title_from_events(events: &[qcode_core::event::Event]) -> Option<String> {
    use qcode_core::event::EventKind;

    let mut renamed: Option<String> = None;
    let mut first_message: Option<String> = None;
    for event in events {
        match &event.kind {
            EventKind::TaskRenamed(event) => {
                let title = event.title.trim();
                if !title.is_empty() {
                    renamed = Some(title.to_string());
                }
            }
            EventKind::UserMessage(message) if first_message.is_none() => {
                let text = message.text.trim().replace('\n', " ");
                if !text.is_empty() {
                    first_message = Some(text.chars().take(48).collect());
                }
            }
            _ => {}
        }
    }
    renamed.or(first_message)
}

pub(crate) fn cmd_sessions(
    args: SessionsArgs,
    data_dir: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let store = SessionStore::new(qcode_kernel::resolve_data_dir(data_dir));
    let mut report = String::new();

    match args.command {
        SessionsCommand::List { titles } => {
            use std::fmt::Write as _;
            let sessions = store.list()?;
            if sessions.is_empty() {
                report.push_str("no sessions\n");
            }
            for session in sessions {
                let _ = write!(
                    report,
                    "{}  turns={}  cost=${:.4}  model={}  cwd={}",
                    session.id,
                    session.turns,
                    session.cost_usd,
                    session.started.model,
                    session.started.cwd
                );
                if titles && let Some(title) = task_title(&store, &session.id) {
                    let _ = write!(report, "  title={title:?}");
                }
                report.push('\n');
                // Roots decide what a task can touch, and a missing one is the
                // difference between "works" and "every file tool fails".
                match store.task(&session.id) {
                    Ok(task) => {
                        for root in &task.roots {
                            let access = match root.access {
                                qcode_core::task::RootAccess::ReadOnly => "ro",
                                qcode_core::task::RootAccess::ReadWrite => "rw",
                            };
                            let state = match root.state() {
                                qcode_core::task::RootState::Ready => "",
                                qcode_core::task::RootState::Missing => "  MISSING",
                            };
                            let _ = writeln!(
                                report,
                                "    @{} {} {}{}",
                                root.label,
                                access,
                                root.path.display(),
                                state
                            );
                        }
                        if task.roots.is_empty() {
                            report.push_str("    (no roots)\n");
                        }
                    }
                    Err(_) => report.push_str("    (no task record)\n"),
                }
            }
        }
        SessionsCommand::Show { id } => {
            // Write as we go: a 20 MB session's pretty JSON is far too big to
            // buffer, and the point of `| head` is to not read all of it.
            use std::fmt::Write as _;
            for event in store.load(&id)? {
                let _ = writeln!(report, "{}", serde_json::to_string_pretty(&event)?);
                if report.len() >= REPORT_FLUSH_AT {
                    write_report(&report)?;
                    report.clear();
                }
            }
        }
        SessionsCommand::Fork { id, at } => {
            use std::fmt::Write as _;
            let forked = store.fork(&id, at)?;
            let _ = writeln!(report, "{forked}");
        }
        SessionsCommand::Gc {
            older_than_days,
            yes,
        } => {
            use std::fmt::Write as _;
            let ids = store.gc(older_than_days, yes)?;
            if ids.is_empty() {
                report.push_str("nothing to clean\n");
            }
            let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(store.root());
            let mut freed = 0u64;
            for id in &ids {
                let size = snapshots.size(id).unwrap_or(0);
                freed += size;
                let _ = writeln!(
                    report,
                    "{} {id} (snapshots: {:.1} MB)",
                    if yes { "removed" } else { "would remove" },
                    size as f64 / (1024.0 * 1024.0)
                );
            }
            if yes {
                for id in &ids {
                    if let Err(error) = snapshots.remove_session(id) {
                        let _ = writeln!(report, "cannot remove snapshots for {id}: {error}");
                    }
                }
                // Snapshot trees whose session is gone (deleted by hand, or by an
                // older qc that did not clean up) are orphans.
                let keep: Vec<String> = store.list()?.into_iter().map(|s| s.id).collect();
                match snapshots.sweep(&keep) {
                    Ok(removed) if !removed.is_empty() => {
                        let _ =
                            writeln!(report, "removed {} orphan snapshot tree(s)", removed.len());
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = writeln!(report, "cannot sweep snapshots: {error}");
                    }
                }
                let _ = writeln!(report, "freed {:.1} MB", freed as f64 / (1024.0 * 1024.0));
            } else {
                report.push_str("dry run; pass --yes to delete\n");
            }
        }
        SessionsCommand::Delete {
            ids,
            except,
            keep_title,
            yes,
        } => {
            use std::fmt::Write as _;
            let all = store.list()?;
            let doomed: Vec<String> = if keep_title.is_empty() {
                // Every named id must exist. With `--except` a mistyped id would
                // otherwise mean "keep nothing", i.e. delete the session the user
                // meant to protect — the exact failure this subcommand exists to
                // avoid.
                if let Some(missing) = ids.iter().find(|id| !all.iter().any(|s| &&s.id == id)) {
                    return Err(format!("no such session: {missing}").into());
                }
                if except {
                    all.iter()
                        .filter(|session| !ids.contains(&session.id))
                        .map(|session| session.id.clone())
                        .collect()
                } else {
                    ids.clone()
                }
            } else {
                if !ids.is_empty() || except {
                    return Err("give ids, or --keep-title, not both".into());
                }
                // Picking by the name the sidebar shows, because that is what the
                // user can actually read off the screen. A title that resolves to
                // nothing (typo, stale name) or to several sessions would, under
                // "delete every other one", silently drop a task that was meant
                // to be kept — so both abort.
                let mut keep: Vec<String> = Vec::with_capacity(keep_title.len());
                for wanted in &keep_title {
                    let wanted = wanted.trim();
                    let matches: Vec<&str> = all
                        .iter()
                        .filter(|session| {
                            task_title(&store, &session.id).as_deref() == Some(wanted)
                        })
                        .map(|session| session.id.as_str())
                        .collect();
                    match matches.as_slice() {
                        [] => {
                            return Err(format!(
                                "no session titled {wanted:?}; \
                                 `qc sessions list --titles` shows the names"
                            )
                            .into());
                        }
                        [only] => keep.push((*only).to_string()),
                        many => {
                            return Err(format!(
                                "{} sessions are titled {wanted:?}; pass the id instead",
                                many.len()
                            )
                            .into());
                        }
                    }
                }
                all.iter()
                    .filter(|session| !keep.contains(&session.id))
                    .map(|session| session.id.clone())
                    .collect()
            };
            if doomed.is_empty() {
                report.push_str("nothing to delete\n");
            }
            // Show what a session was, not just its id: the whole point is to
            // recognise a test run before it goes.
            for session in all.iter().filter(|s| doomed.contains(&s.id)) {
                let _ = write!(
                    report,
                    "{}  turns={}  model={}  cwd={}",
                    session.id, session.turns, session.started.model, session.started.cwd
                );
                if let Some(title) = task_title(&store, &session.id) {
                    let _ = write!(report, "  title={title:?}");
                }
                report.push('\n');
            }
            if !yes {
                let _ = writeln!(
                    report,
                    "{} session(s) would be deleted; pass --yes to delete",
                    doomed.len()
                );
            } else {
                let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(store.root());
                let mut removed = 0usize;
                for id in &doomed {
                    store.delete_session(id)?;
                    // `delete_session` drops the log, the index row and any
                    // task targeting the session; pre-images live in a separate
                    // tree (`gc` cleans that one the same way).
                    if let Err(error) = snapshots.remove_session(id) {
                        let _ = writeln!(report, "cannot remove snapshots for {id}: {error}");
                    }
                    removed += 1;
                }
                let _ = writeln!(report, "deleted {removed} session(s)");
            }
        }
        SessionsCommand::Checkpoints { id } => {
            use std::fmt::Write as _;
            let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(store.root());
            let turns = snapshots.turns(&id)?;
            if turns.is_empty() {
                let _ = writeln!(report, "no checkpoints for {id}");
            }
            for turn in turns {
                let files = snapshots.files(&id, turn)?;
                let _ = writeln!(report, "turn {turn}: {} file(s)", files.len());
                for file in files {
                    let _ = writeln!(report, "  {file}");
                }
            }
        }
        SessionsCommand::Rollback { id, to, yes } => {
            use std::fmt::Write as _;
            // The workspace being restored is the one the session recorded, not
            // the current directory: rolling back from another repo would
            // otherwise write files into the wrong tree.
            let session = store
                .list()?
                .into_iter()
                .find(|session| session.id == id)
                .ok_or_else(|| format!("no such session: {id}"))?;
            let workspace = PathBuf::from(&session.started.cwd);
            let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(store.root());
            let turns = snapshots.turns(&id)?;
            let affected: Vec<u32> = turns.into_iter().filter(|turn| *turn >= to).collect();
            if affected.is_empty() {
                write_report(&format!(
                    "nothing recorded at or after turn {to}; nothing to do\n"
                ))?;
                return Ok(ExitCode::SUCCESS);
            }
            let _ = writeln!(
                report,
                "would restore {} turn(s) into {}",
                affected.len(),
                workspace.display()
            );
            if !yes {
                report.push_str("dry run; pass --yes to restore\n");
                write_report(&report)?;
                return Ok(ExitCode::SUCCESS);
            }
            let report_done = snapshots.restore(&id, to, &workspace)?;
            let _ = writeln!(
                report,
                "restored {} file(s), removed {} created file(s) (turns {:?})",
                report_done.restored, report_done.deleted, report_done.turns
            );
            for refused in &report_done.refused {
                let _ = writeln!(report, "skipped {refused}");
            }
        }
    }

    write_report(&report)?;
    Ok(ExitCode::SUCCESS)
}

/// Flush `show`'s output in chunks rather than buffering one enormous string.
const REPORT_FLUSH_AT: usize = 1 << 16;

/// Store an API key in the platform key store and tell the user what to paste.
///
/// The key arrives on stdin so it never shows up in the process list, and the
/// printed snippet uses `api_key_command` (ADR-0039) instead of a plaintext
/// environment variable.
pub(crate) fn store_key(
    provider: &str,
    service: &str,
    account: Option<&str>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    use std::io::Read;

    let account = account.unwrap_or(provider);
    let mut key = String::new();
    std::io::stdin().read_to_string(&mut key)?;
    let key = key.trim();
    if key.is_empty() {
        return Err("no key on stdin; try: echo <key> | qc config key set".into());
    }

    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-a",
                account,
                "-s",
                service,
                "-w",
                key,
            ])
            .status()?;
        if !status.success() {
            return Err("`security add-generic-password` failed".into());
        }
        println!(
            "stored in the keychain; add to your config:\n\
             api_key_command = [\"security\", \"find-generic-password\", \"-w\", \"-s\", \"{service}\", \"-a\", \"{account}\"]"
        );
    }

    #[cfg(target_os = "linux")]
    {
        use std::io::Write as _;
        let mut child = std::process::Command::new("secret-tool")
            .args([
                "store", "--label", service, "service", service, "user", account,
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(key.as_bytes())?;
        if !child.wait()?.success() {
            return Err("`secret-tool store` failed".into());
        }
        println!(
            "stored in libsecret; add to your config:\n\
             api_key_command = [\"secret-tool\", \"lookup\", \"service\", \"{service}\", \"user\", \"{account}\"]"
        );
    }

    #[cfg(target_os = "windows")]
    {
        // DPAPI keeps it user-scoped and encrypted; HKCU would be plaintext.
        let data_dir = qcode_kernel::resolve_data_dir(None);
        std::fs::create_dir_all(&data_dir)?;
        let path = data_dir.join(format!("{service}-{account}.cred"));
        let script = format!(
            "$s = [Console]::In.ReadLine(); \
             $s | ConvertTo-SecureString -AsPlainText -Force | ConvertFrom-SecureString | Set-Content -NoNewline '{}'",
            path.display()
        );
        use std::io::Write as _;
        // `pwsh` is what most machines running qc will have on PATH; powershell
        // is the fallback for a clean Windows install.
        let shell = if std::process::Command::new("pwsh")
            .arg("-NoProfile")
            .arg("-Command")
            .arg("exit 0")
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
        {
            "pwsh"
        } else {
            "powershell"
        };
        let mut child = std::process::Command::new(shell)
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(format!("{key}\n").as_bytes())?;
        if !child.wait()?.success() {
            return Err("storing the key with DPAPI failed".into());
        }
        println!(
            "stored (DPAPI, user-scoped) in {}; add to your config:\n\
             api_key_command = [\"{shell}\", \"-NoProfile\", \"-Command\", \"$s = Get-Content '{}' | ConvertTo-SecureString; [Runtime.InteropServices.Marshal]::PtrToStringAuto([Runtime.InteropServices.Marshal]::SecureStringToBSTR($s))\"]",
            path.display(),
            path.display()
        );
    }

    let _ = provider;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::{task_title_from_events, write_all_report};
    use qcode_core::event::Event;
    use std::io::{self, Write};

    /// Build an event the way the log stores it, so the tests exercise the same
    /// shape a real session file has instead of a hand-built struct that could
    /// drift from the schema.
    fn event(seq: u64, json_kind: &str) -> Event {
        let line = format!(
            r#"{{"v":1,"seq":{seq},"ts":"2026-11-12T08:00:00.000000Z","session":"s","turn":1,{json_kind}}}"#
        );
        serde_json::from_str(&line).expect("event")
    }

    fn user(seq: u64, text: &str) -> Event {
        event(
            seq,
            &format!(
                r#""type":"user_message","data":{{"text":{}}}"#,
                json_str(text)
            ),
        )
    }

    fn renamed(seq: u64, title: &str) -> Event {
        event(
            seq,
            &format!(
                r#""type":"task_renamed","data":{{"title":{}}}"#,
                json_str(title)
            ),
        )
    }

    fn json_str(value: &str) -> String {
        serde_json::to_string(value).expect("string")
    }

    /// The sidebar shows an explicit name when there is one, and the first user
    /// message otherwise — the same fallback `Backend::title` uses.
    #[test]
    fn an_explicit_title_wins_over_the_first_message() {
        let events = [user(1, "帮我看看这个 bug"), renamed(2, "日常工作")];
        assert_eq!(task_title_from_events(&events).as_deref(), Some("日常工作"));
    }

    #[test]
    fn an_unnamed_session_falls_back_to_its_first_message() {
        let events = [user(1, "帮我看看这个 bug"), user(2, "another")];
        assert_eq!(
            task_title_from_events(&events).as_deref(),
            Some("帮我看看这个 bug")
        );
    }

    /// A rename appends; the sidebar shows the last one, so the last one must
    /// decide. Taking the first would bring an old name back to life.
    #[test]
    fn the_last_rename_wins() {
        let events = [
            renamed(1, "old name"),
            renamed(2, "中学数理化"),
            user(3, "message"),
        ];
        assert_eq!(
            task_title_from_events(&events).as_deref(),
            Some("中学数理化")
        );
    }

    /// A session with no message and no rename has no name. Inventing one would
    /// let `--keep-title` match the wrong session.
    #[test]
    fn a_session_without_a_name_has_no_title() {
        let events = [event(
            1,
            r#""type":"assistant_message","data":{"blocks":[],"provider_raw":null}"#,
        )];
        assert_eq!(task_title_from_events(&events), None);
    }

    /// A writer that always fails the same way, standing in for stdout after the
    /// reader of a pipe has gone away.
    struct Failing(io::ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// `qc sessions list | head -40` must not panic. Before this, `println!`
    /// died inside the stdlib on the closed pipe.
    #[test]
    fn a_closed_pipe_is_not_an_error() {
        let mut closed = Failing(io::ErrorKind::BrokenPipe);
        assert!(write_all_report(&mut closed, "some report\n").is_ok());
    }

    /// Every other write failure is still a failure: a full disk must not look
    /// like a successful listing.
    #[test]
    fn other_write_failures_are_reported() {
        let mut disk = Failing(io::ErrorKind::StorageFull);
        let error = write_all_report(&mut disk, "some report\n").expect_err("must fail");
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    }

    #[test]
    fn a_normal_write_succeeds() {
        let mut buffer: Vec<u8> = Vec::new();
        write_all_report(&mut buffer, "hello\n").expect("write");
        assert_eq!(buffer, b"hello\n");
    }
}
