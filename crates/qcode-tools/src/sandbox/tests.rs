#[cfg(all(test, target_os = "macos"))]
mod tests {
    use crate::sandbox::macos::*;
    use crate::sandbox::*;

    fn workspace() -> PathBuf {
        PathBuf::from("/tmp/qcode-sandbox-tests/workspace")
    }

    #[test]
    fn profile_denies_by_default_and_scopes_writes() {
        let sandbox = SandboxExec::new(&workspace(), SandboxNetwork::Deny).expect("profile");
        let profile = sandbox.profile();
        assert!(profile.starts_with("(version 1)\n(deny default)"));
        assert!(profile.contains("(subpath \"/tmp/qcode-sandbox-tests/workspace\")"));
        assert!(!profile.contains("(allow network*)"));
        assert!(!profile.contains("(allow default)"));
    }

    #[test]
    fn multi_root_profile_scopes_writes_to_rw_roots() {
        let rw = PathBuf::from("/tmp/qcode-sandbox-tests/code");
        let ro = PathBuf::from("/tmp/qcode-sandbox-tests/docs");
        let profile = profile_for_roots(
            &[
                Root {
                    id: "code".into(),
                    label: "code".into(),
                    path: rw.clone(),
                    access: qcode_core::task::RootAccess::ReadWrite,
                    primary: true,
                },
                Root {
                    id: "docs".into(),
                    label: "docs".into(),
                    path: ro.clone(),
                    access: qcode_core::task::RootAccess::ReadOnly,
                    primary: false,
                },
            ],
            Some(Path::new("/tmp/qcode-sandbox-tests/tmp")),
            SandboxNetwork::Deny,
            &[],
        )
        .expect("profile");
        let write = profile
            .split("(allow file-write*")
            .nth(1)
            .expect("write block");
        assert!(write.contains("(subpath \"/tmp/qcode-sandbox-tests/code\")"));
        assert!(write.contains("(subpath \"/tmp/qcode-sandbox-tests/tmp\")"));
        assert!(!write.contains("(subpath \"/tmp/qcode-sandbox-tests/docs\")"));
        let read = profile
            .split("(allow file-read*")
            .nth(1)
            .expect("read block");
        assert!(read.contains("(subpath \"/tmp/qcode-sandbox-tests/docs\")"));
    }

    #[test]
    fn network_is_opt_in() {
        let sandbox = SandboxExec::new(&workspace(), SandboxNetwork::Allow).expect("profile");
        assert!(sandbox.profile().contains("(allow network*)"));
        assert!(sandbox.describe().contains("network allowed"));
    }

    #[test]
    fn hard_mode_allows_only_the_proxy_port() {
        let sandbox = SandboxExec::new(&workspace(), SandboxNetwork::ProxyOnly { port: 43123 })
            .expect("profile");
        let profile = sandbox.profile();
        assert!(profile.contains("(allow network-outbound (remote tcp \"localhost:43123\"))"));
        assert!(!profile.contains("(allow network*)"));
        assert!(sandbox.describe().contains("proxy-only on localhost:43123"));
    }

    #[test]
    fn hard_mode_requires_an_os_sandbox() {
        let error = build(
            SandboxMode::None,
            &workspace(),
            SandboxNetwork::ProxyOnly { port: 43123 },
            &[],
        )
        .expect_err("rejected");
        assert!(
            error
                .to_string()
                .contains("only implemented for macOS sandbox-exec")
        );
    }

    #[test]
    fn command_wraps_the_program() {
        let sandbox = SandboxExec::new(&workspace(), SandboxNetwork::Deny).expect("profile");
        let command = sandbox
            .command(
                OsStr::new("sh"),
                &[OsString::from("-c"), OsString::from("echo hi")],
                &workspace(),
            )
            .expect("command");

        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(command.get_program(), SandboxExec::EXEC);
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], sandbox.profile());
        assert_eq!(&args[2..], ["sh", "-c", "echo hi"]);
    }

    #[test]
    fn rejects_paths_that_break_the_profile() {
        let error = SandboxExec::new(Path::new("/tmp/has\"quote"), SandboxNetwork::Deny)
            .expect_err("rejected");
        assert!(error.to_string().contains("cannot represent"), "{error}");
    }

    #[test]
    fn no_sandbox_runs_the_command() {
        let mut command = NoSandbox
            .command(
                OsStr::new("sh"),
                &[OsString::from("-c"), OsString::from("printf hi")],
                Path::new("/tmp"),
            )
            .expect("command");
        let output = command.output().expect("run");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hi");
    }

    /// These two tests execute `sandbox-exec` for real, so they only prove
    /// anything outside a nested sandbox. Inside one (CI containers, the DSH
    /// runner) the preflight fails with EPERM and the test returns early.
    fn sandbox_or_skip(workspace: &Path) -> Option<SandboxExec> {
        let sandbox = SandboxExec::new(workspace, SandboxNetwork::Deny).expect("profile");
        match sandbox.check() {
            Ok(()) => Some(sandbox),
            Err(err) => {
                eprintln!("skipping: sandbox-exec unavailable here: {err}");
                None
            }
        }
    }

    /// Runtime check for the hard-mode profile: the proxy port is reachable,
    /// another live loopback port is not. Skipped when nested sandbox-exec is
    /// unavailable (CI containers, the DSH runner).
    #[test]
    fn hard_mode_denies_other_loopback_ports() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let proxy_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("proxy listener");
        let other_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("other listener");
        let proxy_port = proxy_listener.local_addr().expect("proxy addr").port();
        let other_port = other_listener.local_addr().expect("other addr").port();

        let sandbox = SandboxExec::new(
            workspace.path(),
            SandboxNetwork::ProxyOnly { port: proxy_port },
        )
        .expect("profile");
        if let Err(err) = sandbox.check() {
            let detail = err.to_string();
            // Only nested sandboxes are expected to refuse sandbox-exec. Any
            // other preflight failure (for example an invalid profile) is a
            // real failure, not a skip.
            if detail.contains("Operation not permitted") || detail.contains("EPERM") {
                eprintln!("skipping: nested sandbox-exec is not permitted here: {err}");
                return;
            }
            panic!("sandbox-exec preflight failed: {err}");
        }

        let connect = |port: u16| {
            sandbox
                .command(
                    OsStr::new("/usr/bin/nc"),
                    &[
                        OsString::from("-z"),
                        OsString::from("127.0.0.1"),
                        OsString::from(port.to_string()),
                    ],
                    workspace.path(),
                )
                .expect("command")
                .output()
                .expect("run nc")
        };

        let allowed = connect(proxy_port);
        assert!(
            allowed.status.success(),
            "the proxy port must stay reachable: {allowed:?}"
        );
        let denied = connect(other_port);
        assert!(
            !denied.status.success(),
            "a non-proxy loopback port must be denied: {denied:?}"
        );
    }

    #[test]
    fn allows_writes_inside_the_workspace() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let Some(sandbox) = sandbox_or_skip(workspace.path()) else {
            return;
        };

        let mut command = sandbox
            .command(
                OsStr::new("sh"),
                &[
                    OsString::from("-c"),
                    OsString::from("printf hi > inside.txt && cat inside.txt"),
                ],
                workspace.path(),
            )
            .expect("command");
        let output = command.output().expect("run");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hi");
        assert!(workspace.path().join("inside.txt").is_file());
    }

    #[test]
    fn denies_writes_outside_the_workspace() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let Some(sandbox) = sandbox_or_skip(workspace.path()) else {
            return;
        };
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            eprintln!("skipping: HOME is not set");
            return;
        };
        let outside = home.join(format!("qcode-sandbox-escape-{}", std::process::id()));

        let mut command = sandbox
            .command(
                OsStr::new("sh"),
                &[
                    OsString::from("-c"),
                    OsString::from(format!("printf x > '{}'", outside.display())),
                ],
                workspace.path(),
            )
            .expect("command");
        let output = command.output().expect("run");
        let _ = std::fs::remove_file(&outside);

        // The command may well succeed: bwrap creates the parent directories of
        // its binds in the sandbox's own root (a tmpfs), so a write next to
        // `~/.cargo` lands there instead of the host. What must never happen is
        // the host file appearing.
        let _ = &output;
        assert!(!outside.exists(), "sandbox created {}", outside.display());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod bwrap_tests {
    use crate::sandbox::linux::*;
    use crate::sandbox::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn workspace() -> PathBuf {
        PathBuf::from("/home/qcode-bwrap-tests/workspace")
    }

    fn cwd() -> PathBuf {
        workspace().join("sub")
    }

    fn rw_roots() -> Vec<BwrapRoot> {
        vec![BwrapRoot {
            path: workspace(),
            writable: true,
        }]
    }

    #[test]
    fn binds_workspace_read_write_and_chdirs() {
        let args = strings(&bwrap_args(&rw_roots(), &cwd(), false, &[]));
        assert!(
            args.windows(3).any(|window| window
                == [
                    "--bind",
                    "/home/qcode-bwrap-tests/workspace",
                    "/home/qcode-bwrap-tests/workspace"
                ]),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--chdir", "/home/qcode-bwrap-tests/workspace/sub"]),
            "{args:?}"
        );
        assert!(args.contains(&"--tmpfs".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
    }

    #[test]
    fn workspace_under_tmp_skips_the_tmpfs_mount() {
        let workspace = PathBuf::from("/tmp/ws");
        let roots = vec![BwrapRoot {
            path: workspace.clone(),
            writable: true,
        }];
        let args = strings(&bwrap_args(&roots, &workspace, false, &[]));
        assert!(!args.contains(&"--tmpfs".to_string()), "{args:?}");
        assert!(
            args.windows(2)
                .any(|window| window == ["--setenv", "TMPDIR"])
        );
    }

    #[test]
    fn network_is_denied_unless_opted_in() {
        let denied = strings(&bwrap_args(&rw_roots(), &cwd(), false, &[]));
        assert!(denied.contains(&"--unshare-net".to_string()));

        let allowed = strings(&bwrap_args(&rw_roots(), &cwd(), true, &[]));
        assert!(!allowed.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn a_read_only_root_is_mounted_read_only() {
        let w = workspace();
        let roots = vec![
            BwrapRoot {
                path: w.clone(),
                writable: true,
            },
            BwrapRoot {
                path: PathBuf::from("/home/qcode-bwrap-tests/docs"),
                writable: false,
            },
        ];
        let args = strings(&bwrap_args(&roots, &cwd(), false, &[]));
        assert!(
            args.windows(3)
                .any(|window| window[0] == "--bind" && window[1] == w.to_string_lossy()),
            "{args:?}"
        );
        assert!(
            args.windows(3)
                .any(|window| window[0] == "--ro-bind"
                    && window[1] == "/home/qcode-bwrap-tests/docs"),
            "{args:?}"
        );
    }
    #[test]
    fn extra_writable_paths_are_bound_once() {
        let cargo = PathBuf::from("/tmp/qcode-bwrap-tests/cargo");
        let args = strings(&bwrap_args(
            &rw_roots(),
            &cwd(),
            false,
            std::slice::from_ref(&cargo),
        ));
        let binds = args
            .windows(3)
            .filter(|window| window[0] == "--bind" && window[1] == cargo.to_string_lossy())
            .count();
        assert_eq!(binds, 1, "{args:?}");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use crate::sandbox::linux::*;
    use crate::sandbox::*;

    fn bwrap_or_skip(workspace: &Path) -> Option<Bwrap> {
        let sandbox = Bwrap::new(workspace, false).expect("sandbox");
        match sandbox.check() {
            Ok(()) => Some(sandbox),
            Err(err) => {
                eprintln!("skipping: bwrap unavailable here: {err}");
                None
            }
        }
    }

    #[test]
    fn a_read_only_root_rejects_writes_next_to_the_writable_one() {
        let rw = tempfile::tempdir().expect("rw dir");
        let ro = tempfile::tempdir().expect("ro dir");
        let Some(seed) = bwrap_or_skip(rw.path()) else {
            return;
        };

        let roots = vec![
            Root {
                id: "rw".to_string(),
                label: "app".to_string(),
                path: rw.path().to_path_buf(),
                access: qcode_core::task::RootAccess::ReadWrite,
                primary: true,
            },
            Root {
                id: "ro".to_string(),
                label: "docs".to_string(),
                path: ro.path().to_path_buf(),
                access: qcode_core::task::RootAccess::ReadOnly,
                primary: false,
            },
        ];
        let sandbox = seed.with_roots(&roots, None).expect("multi-root sandbox");

        let write = |cwd: &Path, target: &Path| {
            let mut command = sandbox
                .command(
                    OsStr::new("/bin/sh"),
                    &[
                        OsString::from("-c"),
                        OsString::from(format!("printf x > '{}'", target.display())),
                    ],
                    cwd,
                )
                .expect("command");
            command.output().expect("run")
        };

        // The writable root accepts a file, on the host.
        let inside = rw.path().join("inside.txt");
        write(rw.path(), &inside);
        assert!(inside.is_file(), "read-write root refused a write");

        // The read-only root is mounted read-only, so the write cannot land.
        let denied = ro.path().join("denied.txt");
        let output = write(rw.path(), &denied);
        assert!(!output.status.success(), "read-only root accepted a write");
        assert!(!denied.exists(), "read-only root has a file in it");
    }
    #[test]
    fn allows_writes_inside_the_workspace() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let Some(sandbox) = bwrap_or_skip(workspace.path()) else {
            return;
        };

        let mut command = sandbox
            .command(
                OsStr::new("/bin/sh"),
                &[
                    OsString::from("-c"),
                    OsString::from("printf hi > inside.txt && cat inside.txt"),
                ],
                workspace.path(),
            )
            .expect("command");
        let output = command.output().expect("run");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hi");
        assert!(workspace.path().join("inside.txt").is_file());
    }

    #[test]
    fn denies_writes_outside_the_workspace() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let Some(sandbox) = bwrap_or_skip(workspace.path()) else {
            return;
        };
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            eprintln!("skipping: HOME is not set");
            return;
        };
        let outside = home.join(format!("qcode-bwrap-escape-{}", std::process::id()));

        let mut command = sandbox
            .command(
                OsStr::new("/bin/sh"),
                &[
                    OsString::from("-c"),
                    OsString::from(format!("printf x > '{}'", outside.display())),
                ],
                workspace.path(),
            )
            .expect("command");
        let output = command.output().expect("run");
        let _ = std::fs::remove_file(&outside);

        // The command may well succeed: bwrap creates the parent directories of
        // its binds in the sandbox's own root (a tmpfs), so a write next to
        // `~/.cargo` lands there instead of the host. What must never happen is
        // the host file appearing.
        let _ = &output;
        assert!(!outside.exists(), "sandbox created {}", outside.display());
    }
}

#[cfg(test)]
mod landlock_tests {
    use crate::sandbox::linux::*;

    #[test]
    fn handled_access_grows_with_abi() {
        let v1 = landlock_handled(1);
        assert_eq!(v1, (1 << 13) - 1);
        assert_eq!(landlock_handled(2), v1 | (1 << 13));
        assert_eq!(landlock_handled(3), v1 | (1 << 13) | (1 << 14));
        assert_eq!(landlock_handled(5), v1 | (1 << 13) | (1 << 14) | (1 << 15));
        assert_eq!(landlock_handled(9), landlock_handled(5));
    }

    /// Rights the kernel refuses on a non-directory node: asking for one of
    /// these on a device made every rule fail with EINVAL, and since a failed
    /// preflight is fatal the binary would not start on Linux.
    const DIR_ONLY_RIGHTS: u64 = (1 << 3)   // READ_DIR
        | (1 << 4)                          // REMOVE_DIR
        | (1 << 5)                          // REMOVE_FILE
        | (1 << 6)                          // MAKE_CHAR
        | (1 << 7)                          // MAKE_DIR
        | (1 << 8)                          // MAKE_REG
        | (1 << 9)                          // MAKE_SOCK
        | (1 << 10)                         // MAKE_FIFO
        | (1 << 11)                         // MAKE_BLOCK
        | (1 << 12)                         // MAKE_SYM
        | (1 << 13); // REFER

    #[test]
    fn file_rights_drop_every_directory_only_bit() {
        for abi in 1..=5 {
            let rights = landlock_file_rights(landlock_handled(abi));
            assert_eq!(
                rights & DIR_ONLY_RIGHTS,
                0,
                "ABI {abi} kept a directory-only right for a non-directory node"
            );
            // The bits a device actually needs must survive.
            assert_eq!(rights & (1 << 1), 1 << 1, "ABI {abi}: WRITE_FILE missing");
            assert_eq!(rights & (1 << 2), 1 << 2, "ABI {abi}: READ_FILE missing");
        }
    }

    #[test]
    fn file_rights_respect_what_the_abi_knows() {
        // TRUNCATE and IOCTL_DEV only exist from ABI 3 / 5 on; requesting them
        // on an older kernel would fail the same way.
        assert_eq!(landlock_file_rights(landlock_handled(1)) & (1 << 14), 0);
        assert_eq!(
            landlock_file_rights(landlock_handled(3)) & (1 << 14),
            1 << 14
        );
        assert_eq!(landlock_file_rights(landlock_handled(3)) & (1 << 15), 0);
        assert_eq!(
            landlock_file_rights(landlock_handled(5)) & (1 << 15),
            1 << 15
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod landlock_runtime {
    use crate::sandbox::linux::*;
    use crate::sandbox::*;

    fn landlock_or_skip(workspace: &Path) -> Option<Landlock> {
        // A kernel with no Landlock at all is an environment we may skip on.
        // Anything else is not: a ruleset that fails to build on a kernel that
        // *does* support Landlock is a bug in this module. This distinction
        // matters — the device-rule EINVAL was silently swallowed here, which
        // kept "qc cannot start on Linux" green for days.
        if let Err(err) = landlock_abi() {
            eprintln!("skipping: landlock unavailable here: {err}");
            return None;
        }
        let sandbox = Landlock::new(workspace).expect("build the landlock ruleset");
        match sandbox.check() {
            Ok(()) => Some(sandbox),
            Err(err) => {
                // The preflight runs a real child under `restrict_self`, which
                // a nested container or seccomp profile can refuse even though
                // the ruleset itself was accepted.
                eprintln!("skipping: landlock preflight failed here: {err}");
                None
            }
        }
    }

    #[test]
    fn allows_writes_inside_the_workspace() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let Some(sandbox) = landlock_or_skip(workspace.path()) else {
            return;
        };

        let mut command = sandbox
            .command(
                OsStr::new("/bin/sh"),
                &[
                    OsString::from("-c"),
                    OsString::from("printf hi > inside.txt && cat inside.txt"),
                ],
                workspace.path(),
            )
            .expect("command");
        let output = command.output().expect("run");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hi");
        assert!(workspace.path().join("inside.txt").is_file());
    }

    #[test]
    fn denies_writes_outside_the_workspace() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let Some(sandbox) = landlock_or_skip(workspace.path()) else {
            return;
        };
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            eprintln!("skipping: HOME is not set");
            return;
        };
        let outside = home.join(format!("qcode-landlock-escape-{}", std::process::id()));

        let mut command = sandbox
            .command(
                OsStr::new("/bin/sh"),
                &[
                    OsString::from("-c"),
                    OsString::from(format!("printf x > '{}'", outside.display())),
                ],
                workspace.path(),
            )
            .expect("command");
        let output = command.output().expect("run");
        let _ = std::fs::remove_file(&outside);

        // The command may well succeed: bwrap creates the parent directories of
        // its binds in the sandbox's own root (a tmpfs), so a write next to
        // `~/.cargo` lands there instead of the host. What must never happen is
        // the host file appearing.
        let _ = &output;
        assert!(!outside.exists(), "sandbox created {}", outside.display());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_profile_tests {
    use crate::sandbox::macos::*;
    use crate::sandbox::*;

    #[test]
    fn the_profile_writes_extra_roots() {
        let root = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let roots = vec![Root {
            id: "r1".to_string(),
            label: "app".to_string(),
            path: root.path().to_path_buf(),
            access: qcode_core::task::RootAccess::ReadWrite,
            primary: true,
        }];
        let profile = profile_for_roots(
            &roots,
            None,
            SandboxNetwork::Deny,
            &[extra.path().to_path_buf()],
        )
        .expect("profile");
        let write_section = profile
            .split("(allow file-write*")
            .nth(1)
            .expect("write section");
        assert!(
            write_section.contains(&extra.path().display().to_string()),
            "extra writable root missing from the profile: {profile}"
        );
    }
}
