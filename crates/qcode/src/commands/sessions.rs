use super::*;

pub(crate) fn cmd_sessions(
    args: SessionsArgs,
    data_dir: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let store = SessionStore::new(qcode_kernel::resolve_data_dir(data_dir));

    match args.command {
        SessionsCommand::List => {
            let sessions = store.list()?;
            if sessions.is_empty() {
                println!("no sessions");
                return Ok(ExitCode::SUCCESS);
            }
            for session in sessions {
                println!(
                    "{}  turns={}  cost=${:.4}  model={}  cwd={}",
                    session.id,
                    session.turns,
                    session.cost_usd,
                    session.started.model,
                    session.started.cwd
                );
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
                            println!(
                                "    @{} {} {}{}",
                                root.label,
                                access,
                                root.path.display(),
                                state
                            );
                        }
                        if task.roots.is_empty() {
                            println!("    (no roots)");
                        }
                    }
                    Err(_) => println!("    (no task record)"),
                }
            }
        }
        SessionsCommand::Show { id } => {
            for event in store.load(&id)? {
                println!("{}", serde_json::to_string_pretty(&event)?);
            }
        }
        SessionsCommand::Fork { id, at } => {
            let forked = store.fork(&id, at)?;
            println!("{forked}");
        }
        SessionsCommand::Gc {
            older_than_days,
            yes,
        } => {
            let ids = store.gc(older_than_days, yes)?;
            if ids.is_empty() {
                println!("nothing to clean");
            }
            let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(store.root());
            let mut freed = 0u64;
            for id in &ids {
                let size = snapshots.size(id).unwrap_or(0);
                freed += size;
                println!(
                    "{} {id} (snapshots: {:.1} MB)",
                    if yes { "removed" } else { "would remove" },
                    size as f64 / (1024.0 * 1024.0)
                );
            }
            if yes {
                for id in &ids {
                    if let Err(error) = snapshots.remove_session(id) {
                        println!("cannot remove snapshots for {id}: {error}");
                    }
                }
                // Snapshot trees whose session is gone (deleted by hand, or by an
                // older qc that did not clean up) are orphans.
                let keep: Vec<String> = store.list()?.into_iter().map(|s| s.id).collect();
                match snapshots.sweep(&keep) {
                    Ok(removed) if !removed.is_empty() => {
                        println!("removed {} orphan snapshot tree(s)", removed.len());
                    }
                    Ok(_) => {}
                    Err(error) => println!("cannot sweep snapshots: {error}"),
                }
                println!("freed {:.1} MB", freed as f64 / (1024.0 * 1024.0));
            } else {
                println!("dry run; pass --yes to delete");
            }
        }
        SessionsCommand::Checkpoints { id } => {
            let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(store.root());
            let turns = snapshots.turns(&id)?;
            if turns.is_empty() {
                println!("no checkpoints for {id}");
                return Ok(ExitCode::SUCCESS);
            }
            for turn in turns {
                let files = snapshots.files(&id, turn)?;
                println!("turn {turn}: {} file(s)", files.len());
                for file in files {
                    println!("  {file}");
                }
            }
        }
        SessionsCommand::Rollback { id, to, yes } => {
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
                println!("nothing recorded at or after turn {to}; nothing to do");
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "would restore {} turn(s) into {}",
                affected.len(),
                workspace.display()
            );
            if !yes {
                println!("dry run; pass --yes to restore");
                return Ok(ExitCode::SUCCESS);
            }
            let report = snapshots.restore(&id, to, &workspace)?;
            println!(
                "restored {} file(s), removed {} created file(s) (turns {:?})",
                report.restored, report.deleted, report.turns
            );
            for refused in &report.refused {
                println!("skipped {refused}");
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

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
