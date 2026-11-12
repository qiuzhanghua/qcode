#[cfg(all(test, target_os = "macos"))]
mod macos_sandbox_tests {
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
            OutsideRoots::BuildCaches,
            &[],
        )
        .expect("profile");
        let write = block(&profile, "(allow file-write*");
        assert!(write.contains("(subpath \"/tmp/qcode-sandbox-tests/code\")"));
        assert!(write.contains("(subpath \"/tmp/qcode-sandbox-tests/tmp\")"));
        assert!(!write.contains("(subpath \"/tmp/qcode-sandbox-tests/docs\")"));
        let read = block(&profile, "(allow file-read*");
        assert!(read.contains("(subpath \"/tmp/qcode-sandbox-tests/docs\")"));
    }

    /// One profile block: from its header line to the closing `)` line.
    fn block<'a>(profile: &'a str, header: &str) -> &'a str {
        let start = profile.find(header).expect("block header");
        let rest = &profile[start..];
        let end = rest.find("\n)\n").expect("block end") + 3;
        &rest[..end]
    }

    /// The read-only roots have to be *denied*, not merely left out of the
    /// allow list: `/tmp` and `$TMPDIR` are writable for every task, so a
    /// read-only root below either one would keep its parent's permission.
    /// Seatbelt applies the last matching rule, so the deny must come after the
    /// allow block.
    #[test]
    fn read_only_roots_are_denied_after_the_writable_ones() {
        let rw = PathBuf::from("/tmp/qcode-sandbox-tests/code");
        let ro = PathBuf::from("/tmp/qcode-sandbox-tests/no-such-docs");
        let profile = profile_for_roots(
            &[
                Root {
                    id: "code".into(),
                    label: "code".into(),
                    path: rw,
                    access: qcode_core::task::RootAccess::ReadWrite,
                    primary: true,
                },
                Root {
                    id: "docs".into(),
                    label: "docs".into(),
                    path: ro,
                    access: qcode_core::task::RootAccess::ReadOnly,
                    primary: false,
                },
            ],
            None,
            SandboxNetwork::Deny,
            &[],
            OutsideRoots::BuildCaches,
            &[],
        )
        .expect("profile");

        let allow = profile.find("(allow file-write*").expect("allow block");
        let deny = profile
            .find("(deny file-write* (subpath \"/tmp/qcode-sandbox-tests/no-such-docs\"))")
            .expect("deny rule for the read-only root");
        assert!(
            deny > allow,
            "the deny rule must follow the allow block:\n{profile}"
        );
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
            SandboxPaths::default(),
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

    fn root(path: &Path, access: qcode_core::task::RootAccess, primary: bool) -> Root {
        Root {
            id: path.display().to_string(),
            label: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "root".to_string()),
            path: path.to_path_buf(),
            access,
            primary,
        }
    }

    /// Build a multi-root sandbox for a real run, or `None` when this process is
    /// itself sandboxed (CI containers, the DSH runner) and `sandbox-exec`
    /// cannot be nested. Any other preflight failure is a real failure.
    fn multi_root_or_skip(
        seed_dir: &Path,
        roots: &[Root],
        temp: &Path,
    ) -> Option<Arc<dyn Sandbox>> {
        let seed = SandboxExec::new(seed_dir, SandboxNetwork::Deny).expect("profile");
        if let Err(error) = seed.check() {
            let detail = error.to_string();
            assert!(
                detail.contains("Operation not permitted") || detail.contains("EPERM"),
                "sandbox-exec preflight failed: {detail}"
            );
            eprintln!("skipping: nested sandbox-exec is not permitted here: {detail}");
            return None;
        }
        Some(
            seed.with_roots(roots, Some(temp), OutsideRoots::BuildCaches)
                .expect("multi-root sandbox"),
        )
    }

    fn run(sandbox: &dyn Sandbox, cwd: &Path, script: &str) -> std::process::Output {
        sandbox
            .command(
                OsStr::new("/bin/sh"),
                &[OsString::from("-c"), OsString::from(script)],
                cwd,
            )
            .expect("command")
            .output()
            .expect("run")
    }

    /// Real run of the root matrix: every root is readable, only read-write
    /// roots and the turn temp accept writes.
    ///
    /// The fixture lives under `$TMPDIR`, which the profile keeps writable for
    /// every task — the read-only half of this test therefore also proves that
    /// the `(deny file-write* ...)` rule wins over the parent's allowance.
    #[test]
    fn a_read_only_root_refuses_writes_inside_a_writable_temp_tree() {
        let base = tempfile::tempdir().expect("base");
        let base = base.path().canonicalize().expect("canonical base");
        let rw = base.join("code");
        let ro = base.join("docs");
        let temp = base.join("turn-tmp");
        for dir in [&rw, &ro, &temp] {
            std::fs::create_dir_all(dir).expect("fixture dir");
        }
        std::fs::write(ro.join("note.txt"), "readable").expect("fixture file");

        let roots = vec![
            root(&rw, qcode_core::task::RootAccess::ReadWrite, true),
            root(&ro, qcode_core::task::RootAccess::ReadOnly, false),
        ];
        let Some(sandbox) = multi_root_or_skip(&rw, &roots, &temp) else {
            return;
        };

        // Reads are allowed from both roots, read-only included.
        let read = run(
            sandbox.as_ref(),
            &rw,
            &format!("cat '{}'", ro.join("note.txt").display()),
        );
        assert!(read.status.success(), "{read:?}");
        assert_eq!(String::from_utf8_lossy(&read.stdout), "readable");

        // The read-write root accepts a file, on the host.
        let written = run(sandbox.as_ref(), &rw, "printf hi > new.txt");
        assert!(written.status.success(), "{written:?}");
        assert!(rw.join("new.txt").is_file());

        // The read-only root refuses one even though its parent is writable.
        let denied = run(
            sandbox.as_ref(),
            &rw,
            &format!("printf x > '{}'", ro.join("denied.txt").display()),
        );
        assert!(
            !denied.status.success(),
            "read-only root accepted a write: {denied:?}"
        );
        assert!(!ro.join("denied.txt").exists());

        // The turn temp is writable and works as a cwd of its own.
        let scratch = run(sandbox.as_ref(), &temp, "printf hi > scratch.txt");
        assert!(scratch.status.success(), "{scratch:?}");
        assert!(temp.join("scratch.txt").is_file());
    }

    /// A no-root task keeps the turn temp and loses every root: the profile must
    /// not reach any user file, which is why the fixture lives under `$HOME`
    /// (a `$TMPDIR` fixture would be readable by design).
    #[test]
    fn a_no_root_task_sees_only_its_temp() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            eprintln!("skipping: HOME is not set");
            return;
        };
        let base = home.join(format!(".qcode-no-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let private = base.join("private");
        let temp = base.join("turn-tmp");
        for dir in [&private, &temp] {
            std::fs::create_dir_all(dir).expect("fixture dir");
        }
        std::fs::write(private.join("secret.txt"), "secret").expect("fixture file");

        let sandbox = multi_root_or_skip(&temp, &[], &temp);
        if let Some(sandbox) = sandbox {
            let read = run(
                sandbox.as_ref(),
                &temp,
                &format!("cat '{}'", private.join("secret.txt").display()),
            );
            assert!(
                !read.status.success(),
                "a no-root task read a user file: {read:?}"
            );

            let write = run(
                sandbox.as_ref(),
                &temp,
                &format!("printf x > '{}'", private.join("denied.txt").display()),
            );
            assert!(!write.status.success(), "a no-root task wrote a user file");
            assert!(!private.join("denied.txt").exists());

            let scratch = run(sandbox.as_ref(), &temp, "printf hi > scratch.txt");
            assert!(scratch.status.success(), "{scratch:?}");
            assert!(temp.join("scratch.txt").is_file());
        }

        let _ = std::fs::remove_dir_all(&base);
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
        let args = strings(&bwrap_args(
            &rw_roots(),
            &cwd(),
            false,
            &[],
            OutsideRoots::BuildCaches,
            &[],
            &[],
        ));
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
        let args = strings(&bwrap_args(
            &roots,
            &workspace,
            false,
            &[],
            OutsideRoots::BuildCaches,
            &[],
            &[],
        ));
        assert!(!args.contains(&"--tmpfs".to_string()), "{args:?}");
        assert!(
            args.windows(2)
                .any(|window| window == ["--setenv", "TMPDIR"])
        );
    }

    #[test]
    fn network_is_denied_unless_opted_in() {
        let denied = strings(&bwrap_args(
            &rw_roots(),
            &cwd(),
            false,
            &[],
            OutsideRoots::BuildCaches,
            &[],
            &[],
        ));
        assert!(denied.contains(&"--unshare-net".to_string()));

        let allowed = strings(&bwrap_args(
            &rw_roots(),
            &cwd(),
            true,
            &[],
            OutsideRoots::BuildCaches,
            &[],
            &[],
        ));
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
        let args = strings(&bwrap_args(
            &roots,
            &cwd(),
            false,
            &[],
            OutsideRoots::BuildCaches,
            &[],
            &[],
        ));
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
            OutsideRoots::BuildCaches,
            &[],
            &[],
        ));
        let binds = args
            .windows(3)
            .filter(|window| window[0] == "--bind" && window[1] == cargo.to_string_lossy())
            .count();
        assert_eq!(binds, 1, "{args:?}");
    }

    /// `[sandbox] extra_writable` becomes an rw bind — and does so even when
    /// the build-cache grant is off, because the two are different questions:
    /// "what qcode adds on its own" versus "what the user asked for".
    ///
    /// The MCP-server shape is the one that matters here: those sandboxes run
    /// with `OutsideRoots::Nothing`, and dropping the list there would have
    /// reproduced the bug on the bwrap backend while Landlock was fixed.
    #[test]
    fn extra_writable_adds_rw_binds_even_without_build_caches() {
        let docs = PathBuf::from("/home/qcode-bwrap-tests/Documents");
        let args = strings(&bwrap_args(
            &rw_roots(),
            &cwd(),
            false,
            &[],
            OutsideRoots::Nothing,
            &[],
            std::slice::from_ref(&docs),
        ));
        assert!(
            args.windows(3)
                .any(|window| window[0] == "--bind" && window[1] == docs.to_string_lossy()),
            "extra_writable was not bound read-write: {args:?}"
        );
        assert!(
            !args
                .windows(3)
                .any(|window| window[0] == "--ro-bind" && window[1] == docs.to_string_lossy()),
            "extra_writable was bound read-only: {args:?}"
        );
    }

    /// An `extra_writable` path that is already a task root is not bound twice:
    /// bwrap errors out on a duplicate destination.
    #[test]
    fn extra_writable_skips_a_path_that_is_already_a_root() {
        let workspace = workspace();
        let args = strings(&bwrap_args(
            &rw_roots(),
            &cwd(),
            false,
            &[],
            OutsideRoots::Nothing,
            &[],
            std::slice::from_ref(&workspace),
        ));
        let binds = args
            .windows(3)
            .filter(|window| window[0] == "--bind" && window[1] == workspace.to_string_lossy())
            .count();
        assert_eq!(binds, 1, "{args:?}");
    }

    /// A workspace under an `extra_writable` path must not get a private tmpfs
    /// mounted over it, or the grant is invisible from inside.
    #[test]
    fn extra_writable_under_tmp_suppresses_the_tmpfs() {
        let workspace = PathBuf::from("/home/qcode-bwrap-tests/workspace");
        let roots = vec![BwrapRoot {
            path: workspace.clone(),
            writable: true,
        }];
        let args = strings(&bwrap_args(
            &roots,
            &workspace,
            false,
            &[],
            OutsideRoots::Nothing,
            &[],
            &[PathBuf::from("/tmp/qcode-bwrap-tests/Documents")],
        ));
        assert!(!args.contains(&"--tmpfs".to_string()), "{args:?}");
    }

    /// `[sandbox] extra_read` becomes read-only binds, and a root that is
    /// already bound is not bound twice.
    #[test]
    fn extra_read_adds_read_only_binds() {
        let prefix = PathBuf::from("/opt/toolchain");
        let args = strings(&bwrap_args(
            &rw_roots(),
            &cwd(),
            false,
            &[],
            OutsideRoots::BuildCaches,
            std::slice::from_ref(&prefix),
            &[],
        ));
        let binds = args
            .windows(3)
            .filter(|window| window[0] == "--ro-bind" && window[1] == prefix.to_string_lossy())
            .count();
        assert_eq!(binds, 1, "{args:?}");
        assert!(
            !args
                .windows(3)
                .any(|w| w[0] == "--bind" && w[1] == prefix.to_string_lossy()),
            "a read root must never be bound read-write: {args:?}"
        );

        // A root the task already has needs no extra bind.
        let workspace = rw_roots()[0].path.clone();
        let args = strings(&bwrap_args(
            &rw_roots(),
            &cwd(),
            false,
            &[],
            OutsideRoots::BuildCaches,
            std::slice::from_ref(&workspace),
            &[],
        ));
        let duplicate = args
            .windows(3)
            .filter(|w| w[0] == "--ro-bind" && w[1] == workspace.to_string_lossy())
            .count();
        assert_eq!(duplicate, 0, "{args:?}");
    }

    /// `OutsideRoots::Nothing`: no build-cache binds, and `TMPDIR` is left as
    /// qcode set it (its private HOME for an MCP server, ADR-0068 §3).
    ///
    /// The private `--tmpfs /tmp` stays: it is not the host's `/tmp`, so a
    /// server that ignores `TMPDIR` writes into a directory that disappears
    /// with the process. Only the *host* path is what the fix is about.
    #[test]
    fn outside_roots_nothing_binds_no_build_caches() {
        let cargo = PathBuf::from("/tmp/qcode-bwrap-tests/cargo");
        let args = strings(&bwrap_args(
            &rw_roots(),
            &cwd(),
            false,
            std::slice::from_ref(&cargo),
            OutsideRoots::Nothing,
            &[],
            &[],
        ));
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "--bind" && w[1] == cargo.to_string_lossy()),
            "CARGO_HOME was bound read-write: {args:?}"
        );
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "--setenv" && w[1] == "TMPDIR"),
            "TMPDIR was forced to /tmp, which is not the server's private HOME: {args:?}"
        );
        // The isolation itself survives.
        assert!(
            args.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == "/tmp"),
            "the private tmpfs must stay: {args:?}"
        );
    }

    /// And the same call with build caches does bind them, so the assertion
    /// above is a difference and not an artefact of the fixture.
    #[test]
    fn outside_roots_build_caches_binds_them() {
        let cargo = PathBuf::from("/tmp/qcode-bwrap-tests/cargo");
        let args = strings(&bwrap_args(
            &rw_roots(),
            &cwd(),
            false,
            std::slice::from_ref(&cargo),
            OutsideRoots::BuildCaches,
            &[],
            &[],
        ));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--bind" && w[1] == cargo.to_string_lossy()),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--setenv" && w[1] == "TMPDIR"),
            "{args:?}"
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use crate::sandbox::linux::*;
    use crate::sandbox::*;

    /// `extra_writable` is validated by both Linux backends, not only landlock.
    ///
    /// bwrap would otherwise surface a missing path as its own mount error and a
    /// `/` grant as a working bind, so the two backends would disagree about the
    /// same config while each looked self-consistent.
    #[test]
    fn both_linux_backends_refuse_a_dangerous_or_missing_extra_writable() {
        let workspace = tempfile::tempdir().expect("workspace");
        let missing = workspace.path().join("no-such-dir");

        let bwrap_missing =
            Bwrap::new(workspace.path(), false, &[], std::slice::from_ref(&missing))
                .expect_err("bwrap must reject a missing extra_writable root");
        assert!(bwrap_missing.to_string().contains("extra_writable"));

        let bwrap_root = Bwrap::new(workspace.path(), false, &[], &[PathBuf::from("/")])
            .expect_err("bwrap must reject / writable");
        assert!(bwrap_root.to_string().contains("filesystem root"));

        if landlock_abi().is_err() {
            eprintln!("skipping the landlock half: landlock unavailable here");
            return;
        }
        let landlock_missing = Landlock::new(workspace.path(), &[], std::slice::from_ref(&missing))
            .expect_err("landlock must reject a missing extra_writable root");
        assert!(landlock_missing.to_string().contains("extra_writable"));
    }

    fn bwrap_or_skip(workspace: &Path) -> Option<Bwrap> {
        let sandbox = Bwrap::new(workspace, false, &[], &[]).expect("sandbox");
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
        let sandbox = seed
            .with_roots(&roots, None, OutsideRoots::BuildCaches)
            .expect("multi-root sandbox");

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
        let sandbox = Landlock::new(workspace, &[], &[]).expect("build the landlock ruleset");
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

    /// `[sandbox] extra_read`: a subtree outside the fixed system allowlist
    /// becomes readable **and executable**, which is the whole point — the
    /// toolchain on the machine this was written for lives under `$HOME`.
    ///
    /// Both halves run under `OutsideRoots::Nothing` on purpose. `tempfile`
    /// puts the fixture under `/tmp`, and `BuildCaches` grants `/tmp` writable
    /// unconditionally — comparing "bare" against "bare + extra_read" under
    /// `BuildCaches` would have passed for the wrong reason (the same trap that
    /// made an earlier test route around `/tmp` instead of asserting on it).
    /// Under `Nothing`, `/tmp` is not granted at all, so `extra_read` is the
    /// only difference between the two sandboxes.
    #[test]
    fn extra_read_grants_a_subtree_outside_the_system_allowlist() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let tool = tempfile::tempdir().expect("tool prefix");
        std::fs::write(tool.path().join("hello.sh"), "#!/bin/sh\necho ok\n").expect("script");
        let script = tool.path().join("hello.sh");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        if landlock_abi().is_err() {
            eprintln!("skipping: landlock unavailable here");
            return;
        }

        let roots = vec![Root {
            id: "rw".to_string(),
            label: "app".to_string(),
            path: workspace.path().to_path_buf(),
            access: qcode_core::task::RootAccess::ReadWrite,
            primary: true,
        }];
        // Driven through `with_roots`, because that is the call `run_shell`
        // makes every turn: a fix that only worked on the seed sandbox would
        // never reach a single tool.
        let build = |extra_read: &[PathBuf]| {
            let seed = Landlock::new(workspace.path(), extra_read, &[]).expect("ruleset");
            seed.with_roots(&roots, None, OutsideRoots::Nothing)
                .expect("multi-root sandbox")
        };
        let read = |sandbox: &dyn Sandbox| {
            let mut command = sandbox
                .command(
                    OsStr::new("/bin/sh"),
                    &[
                        OsString::from("-c"),
                        OsString::from(format!("cat '{}' >/dev/null", script.display())),
                    ],
                    workspace.path(),
                )
                .expect("command");
            command.output().expect("run").status.success()
        };
        let run_script = |sandbox: &dyn Sandbox| -> Option<bool> {
            let mut command = sandbox
                .command(OsStr::new(&script), &[], workspace.path())
                .expect("command");
            match command.output() {
                Ok(output) => Some(output.status.success()),
                // An `execve` Landlock denies fails *in the parent*: the child
                // never starts, so there is no exit status to inspect and
                // `output()` returns EACCES. `None` means "denied at spawn",
                // which is a denial like any other — the previous version
                // `.expect("run")`ed here and so panicked on every denial it
                // was written to observe.
                Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => None,
                Err(err) => panic!("cannot even attempt {}: {err}", script.display()),
            }
        };
        // Both shapes of denial count as denied.
        let denied = |sandbox: &dyn Sandbox| !matches!(run_script(sandbox), Some(true));

        // Without it: unreachable. This is the failure that motivated the field.
        let bare = build(&[]);
        if let Err(err) = bare.check() {
            eprintln!("skipping: landlock preflight failed here: {err}");
            return;
        }
        assert!(
            !read(bare.as_ref()),
            "the fixture must be unreadable without extra_read, or this proves nothing"
        );
        assert!(
            denied(bare.as_ref()),
            "executing outside the allowlist must be denied too"
        );

        // With it: read works, and so does execute — a prefix whose binaries
        // cannot run is the situation this field exists to end.
        let granted = build(&[tool.path().to_path_buf()]);
        assert!(
            read(granted.as_ref()),
            "extra_read did not grant {}",
            tool.path().display()
        );
        assert!(
            run_script(granted.as_ref()) == Some(true),
            "extra_read must include execute, or a toolchain under it cannot run: {:?}",
            run_script(granted.as_ref())
        );
        assert!(
            granted.describe().contains("extra read"),
            "doctor must be able to say what was granted: {}",
            granted.describe()
        );
        // And it stayed read-only: the subtree must not become writable.
        let write = granted
            .command(
                OsStr::new("/bin/sh"),
                &[
                    OsString::from("-c"),
                    OsString::from(format!("printf x > '{}/w.txt'", tool.path().display())),
                ],
                workspace.path(),
            )
            .expect("command")
            .output()
            .expect("run");
        assert!(
            !write.status.success(),
            "extra_read granted write access: {write:?}"
        );
    }

    /// `[sandbox] extra_writable`: a subtree outside the task roots becomes
    /// writable — the Landlock half of what `sandbox-exec` and `windows-acl`
    /// already did.
    ///
    /// Before this, `Landlock::new` took no write list at all and `mod.rs` threw
    /// it away with `let _ = extra_writable`, so `extra_writable =
    /// ["~/Documents"]` produced a read-only directory and no error. Measured
    /// with `touch ~/Documents/x`: `Permission denied`, while `cat` on a file in
    /// the same directory worked — which is what a user reads as "my config is
    /// being ignored", because it was.
    ///
    /// Three things have to hold, and only the first is the feature: the listed
    /// subtree is writable, an unlisted sibling is not, and the grant reaches a
    /// sandbox built by `with_roots` rather than only the seed.
    #[test]
    fn extra_write_grants_a_writable_subtree_outside_the_roots() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let listed = tempfile::tempdir().expect("listed dir");
        let sibling = tempfile::tempdir().expect("unlisted dir");
        if landlock_abi().is_err() {
            eprintln!("skipping: landlock unavailable here");
            return;
        }

        let roots = vec![Root {
            id: "rw".to_string(),
            label: "app".to_string(),
            path: workspace.path().to_path_buf(),
            access: qcode_core::task::RootAccess::ReadWrite,
            primary: true,
        }];
        // Through `with_roots`, which is the call `run_shell` makes every turn.
        let build = |extra_write: &[PathBuf]| {
            let seed = Landlock::new(workspace.path(), &[], extra_write).expect("ruleset");
            seed.with_roots(&roots, None, OutsideRoots::Nothing)
                .expect("multi-root sandbox")
        };
        // `create` writes a new file and then renames it, so it needs MAKE_REG,
        // WRITE_FILE and REFER together — the mask a hand-picked "just
        // WRITE_FILE" would have failed.
        let create = |sandbox: &dyn Sandbox, dir: &Path| {
            let target = dir.join("created.txt");
            let done = sandbox
                .command(
                    OsStr::new("/bin/sh"),
                    &[
                        OsString::from("-c"),
                        OsString::from(format!(
                            "printf x > '{d}/tmp' && mv '{d}/tmp' '{d}/created.txt'",
                            d = dir.display()
                        )),
                    ],
                    workspace.path(),
                )
                .expect("command")
                .output()
                .expect("run")
                .status
                .success();
            let wrote = target.exists();
            let _ = std::fs::remove_file(&target);
            (done, wrote)
        };

        // Without it: denied. Asserted first, or the test proves nothing.
        let bare = build(&[]);
        if let Err(err) = bare.check() {
            eprintln!("skipping: landlock preflight failed here: {err}");
            return;
        }
        let (ok, wrote) = create(bare.as_ref(), listed.path());
        assert!(
            !wrote,
            "the fixture dir must be unwritable without extra_write"
        );
        assert!(!ok, "the write must fail, not merely be dropped");

        // With it: allowed, and reported.
        let granted = build(&[listed.path().to_path_buf()]);
        let (ok, wrote) = create(granted.as_ref(), listed.path());
        assert!(
            wrote && ok,
            "extra_write did not grant {}: {ok} {wrote}",
            listed.path().display()
        );
        assert!(
            granted.describe().contains("extra write"),
            "doctor must be able to say what was granted: {}",
            granted.describe()
        );

        // The grant is the listed subtree only — a sibling under the same
        // parent stays denied, which is the difference between this and
        // "grant the parent".
        let (ok, wrote) = create(granted.as_ref(), sibling.path());
        assert!(
            !wrote && !ok,
            "extra_write leaked to an unlisted sibling: {ok} {wrote}"
        );
    }

    /// A missing `extra_writable` root is named at startup, like `extra_read`.
    ///
    /// This one matters more than its read counterpart: a silently skipped
    /// write root is indistinguishable from the setting being ignored, which is
    /// the exact symptom this field spent a release having.
    #[test]
    fn extra_write_rejects_a_missing_root() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        if landlock_abi().is_err() {
            eprintln!("skipping: landlock unavailable here");
            return;
        }
        let missing = workspace.path().join("no-such-dir");
        let error = Landlock::new(workspace.path(), &[], std::slice::from_ref(&missing))
            .expect_err("a missing extra_writable root must be rejected");
        assert!(
            error.to_string().contains("extra_writable"),
            "the message must name the field: {error}"
        );
        assert!(
            error.to_string().contains("no-such-dir"),
            "the message must name the path: {error}"
        );
    }

    /// The dangerous shapes are refused at the backend too, not only by config
    /// validation — `Sandbox::build` is public and a rule once installed cannot
    /// be withdrawn.
    ///
    /// `/` is the probe because it needs no home lookup to be obvious; the
    /// home-shaped cases are covered where `real_home` can be injected, in
    /// `qcode_core::paths`.
    #[test]
    fn extra_write_rejects_the_filesystem_root() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        if landlock_abi().is_err() {
            eprintln!("skipping: landlock unavailable here");
            return;
        }
        let error = Landlock::new(workspace.path(), &[], &[PathBuf::from("/")])
            .expect_err("granting / writable must be rejected");
        let text = error.to_string();
        assert!(text.contains("extra_writable"), "{text}");
        assert!(text.contains("filesystem root"), "{text}");
    }

    /// A read root that does not exist is a startup error that names itself,
    /// rather than a rule Landlock silently skips.
    #[test]
    fn extra_read_rejects_a_missing_root() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        if landlock_abi().is_err() {
            eprintln!("skipping: landlock unavailable here");
            return;
        }
        let missing = workspace.path().join("no-such-prefix");
        let error = Landlock::new(workspace.path(), std::slice::from_ref(&missing), &[])
            .expect_err("a missing extra_read root must be rejected");
        assert!(
            error.to_string().contains("extra_read"),
            "the message must name the field: {error}"
        );
        assert!(
            error.to_string().contains("no-such-prefix"),
            "the message must name the path: {error}"
        );
    }

    /// The write set is the roots, and the two shapes must actually differ on a
    /// real kernel — otherwise `OutsideRoots` is decoration.
    ///
    /// `/tmp` is the probe because it is where the bug was: the backend granted
    /// it unconditionally, so every MCP server could write there (measured on
    /// wordcraft-cli, which happily wrote a .docx into `/tmp/...`). `run_shell`
    /// keeps it; an MCP server must not.
    #[test]
    fn the_host_temp_dir_is_writable_only_when_build_caches_are_granted() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let Some(seed) = landlock_or_skip(workspace.path()) else {
            return;
        };
        let roots = vec![Root {
            id: "rw".to_string(),
            label: "app".to_string(),
            path: workspace.path().to_path_buf(),
            access: qcode_core::task::RootAccess::ReadWrite,
            primary: true,
        }];
        let target =
            std::env::temp_dir().join(format!("qcode-outside-roots-{}", std::process::id()));
        let _ = std::fs::remove_file(&target);
        let write = |sandbox: &dyn Sandbox| {
            let mut command = sandbox
                .command(
                    OsStr::new("/bin/sh"),
                    &[
                        OsString::from("-c"),
                        OsString::from(format!("printf x > '{}'", target.display())),
                    ],
                    workspace.path(),
                )
                .expect("command");
            let output = command.output().expect("run");
            let wrote = target.exists();
            let _ = std::fs::remove_file(&target);
            (output.status.success(), wrote)
        };

        // `run_shell`: the host temp dir comes with the build caches.
        let shell = seed
            .with_roots(&roots, None, OutsideRoots::BuildCaches)
            .expect("build caches sandbox");
        let (ok, wrote) = write(shell.as_ref());
        assert!(ok && wrote, "run_shell must keep writing to /tmp");

        // An MCP server: exactly the roots.
        let server = seed
            .with_roots(&roots, None, OutsideRoots::Nothing)
            .expect("roots-only sandbox");
        let (ok, wrote) = write(server.as_ref());
        assert!(!wrote, "an MCP server wrote to the host /tmp");
        assert!(!ok, "the write must fail, not merely be dropped");
    }

    /// The build caches are the other half of the same allowance, and the one
    /// with teeth: `~/.cargo/config.toml` gets to name a `rustc-wrapper`.
    #[test]
    fn the_build_caches_are_writable_only_when_granted() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let cache = tempfile::tempdir().expect("cache dir");
        let Some(seed) = landlock_or_skip(workspace.path()) else {
            return;
        };
        let roots = vec![Root {
            id: "rw".to_string(),
            label: "app".to_string(),
            path: workspace.path().to_path_buf(),
            access: qcode_core::task::RootAccess::ReadWrite,
            primary: true,
        }];
        let target = cache.path().join("config.toml");
        let write = |sandbox: &dyn Sandbox| {
            let mut command = sandbox
                .command(
                    OsStr::new("/bin/sh"),
                    &[
                        OsString::from("-c"),
                        OsString::from(format!("printf x > '{}'", target.display())),
                    ],
                    workspace.path(),
                )
                .expect("command");
            let output = command.output().expect("run");
            let wrote = target.exists();
            let _ = std::fs::remove_file(&target);
            (output.status.success(), wrote)
        };

        // `writable_paths()` reads `CARGO_HOME` out of the *process* env, so the
        // only way to point at a temp dir is to set it here. SAFETY: this test
        // binary has no other user of the variable, and the guard restores it.
        let previous = std::env::var_os("CARGO_HOME");
        // SAFETY: single-threaded use inside this test (see the comment above).
        unsafe { std::env::set_var("CARGO_HOME", cache.path()) };

        let shell = seed
            .with_roots(&roots, None, OutsideRoots::BuildCaches)
            .expect("build caches sandbox");
        let (ok, wrote) = write(shell.as_ref());
        let server = seed
            .with_roots(&roots, None, OutsideRoots::Nothing)
            .expect("roots-only sandbox");
        let (server_ok, server_wrote) = write(server.as_ref());

        match previous {
            // SAFETY: as above.
            Some(value) => unsafe { std::env::set_var("CARGO_HOME", value) },
            None => {
                // SAFETY: as above.
                unsafe { std::env::remove_var("CARGO_HOME") }
            }
        }

        assert!(ok && wrote, "run_shell must keep writing to CARGO_HOME");
        assert!(
            !server_wrote,
            "an MCP server wrote into CARGO_HOME — its config.toml can name a rustc-wrapper"
        );
        assert!(!server_ok, "the write must fail, not merely be dropped");
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

    /// The macOS half of the same rule: no host scratch directories and no
    /// cache writes when the caller granted nothing. Read access to them stays
    /// (a binary may live under `CARGO_HOME`), which is why this asserts on the
    /// write block specifically.
    #[test]
    fn outside_roots_nothing_drops_the_scratch_and_cache_writes() {
        let roots = vec![Root {
            id: "r1".to_string(),
            label: "app".to_string(),
            path: PathBuf::from("/Users/test/project"),
            access: qcode_core::task::RootAccess::ReadWrite,
            primary: true,
        }];
        // The whole `(allow file-write* ...)` sexp, not just its first entry:
        // `)\n` would match the closing paren of the first `(subpath ...)`.
        let write_block = |profile: &str| -> String {
            let start = profile.find("(allow file-write*").expect("write block");
            let rest = &profile[start..];
            let end = rest.find("\n)\n").expect("write block end");
            rest[..end].to_string()
        };

        let shell = profile_for_roots(
            &roots,
            None,
            SandboxNetwork::Deny,
            &[],
            OutsideRoots::BuildCaches,
            &[],
        )
        .expect("profile");
        let shell_write = write_block(&shell);
        assert!(shell_write.contains("/private/tmp"), "{shell_write}");
        assert!(shell_write.contains("/Users/test/project"), "{shell_write}");

        let server = profile_for_roots(
            &roots,
            None,
            SandboxNetwork::Deny,
            &[],
            OutsideRoots::Nothing,
            &[],
        )
        .expect("profile");
        let server_write = write_block(&server);
        assert!(
            server_write.contains("/Users/test/project"),
            "{server_write}"
        );
        for outside in ["/private/tmp", "/tmp", "/var/folders"] {
            assert!(
                !server_write.contains(outside),
                "{outside} is writable for an MCP server: {server_write}"
            );
        }
        if let Some(cargo) = std::env::var_os("CARGO_HOME") {
            let cargo = cargo.to_string_lossy();
            assert!(
                !server_write.contains(cargo.as_ref()),
                "CARGO_HOME is writable for an MCP server: {server_write}"
            );
        }
    }

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
            OutsideRoots::BuildCaches,
            &[],
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
