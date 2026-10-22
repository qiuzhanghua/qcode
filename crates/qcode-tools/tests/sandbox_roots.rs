//! Real-machine check that a multi-root task can only write where it may.
//!
//! Windows restricts writes only, so a read-write root must accept a file and a
//! read-only root must refuse one. The sandbox is the ACL backend, which needs
//! to derive a restricted token: when that is unavailable in the current
//! environment the test reports a skip instead of pretending to pass.

#![cfg(windows)]

use std::path::Path;
use std::process::Stdio;

use qcode_core::task::{Root, RootAccess};
use qcode_tools::sandbox::{Sandbox, SandboxMode, SandboxNetwork, build};

fn acl_sandbox(root: &Path) -> Option<std::sync::Arc<dyn Sandbox>> {
    let sandbox = build(SandboxMode::WindowsAcl, root, SandboxNetwork::Deny, &[]).ok()?;
    sandbox.check().ok()?;
    Some(sandbox)
}

fn run(sandbox: &dyn Sandbox, cwd: &Path, script: &str) -> Option<bool> {
    let mut command = sandbox
        .command(
            std::ffi::OsStr::new("cmd.exe"),
            &[
                std::ffi::OsString::from("/c"),
                std::ffi::OsString::from(script),
            ],
            cwd,
        )
        .ok()?;
    let output = command.stdin(Stdio::null()).output().ok()?;
    Some(output.status.success())
}

#[test]
fn a_read_only_root_refuses_writes_while_a_read_write_root_accepts_them() {
    let rw = tempfile::tempdir().expect("rw dir");
    let ro = tempfile::tempdir().expect("ro dir");

    let Some(base) = acl_sandbox(rw.path()) else {
        eprintln!("skip: the ACL sandbox is unavailable in this environment");
        return;
    };

    let roots = vec![
        Root {
            id: "rw".to_string(),
            label: "app".to_string(),
            path: rw.path().to_path_buf(),
            access: RootAccess::ReadWrite,
            primary: true,
        },
        Root {
            id: "ro".to_string(),
            label: "docs".to_string(),
            path: ro.path().to_path_buf(),
            access: RootAccess::ReadOnly,
            primary: false,
        },
    ];
    let Some(sandbox) = base.with_roots(&roots, None).ok() else {
        eprintln!("skip: with_roots is not supported by this backend");
        return;
    };

    // Writing inside the read-write root must work.
    let wrote_rw = run(sandbox.as_ref(), rw.path(), "echo ok> inside.txt");
    assert_eq!(
        wrote_rw,
        Some(true),
        "a write inside the read-write root was refused"
    );
    assert!(rw.path().join("inside.txt").exists());

    // Writing inside the read-only root must be refused by the token.
    let wrote_ro = run(sandbox.as_ref(), ro.path(), "echo ok> denied.txt");
    assert_eq!(wrote_ro, Some(false), "the read-only root accepted a write");
    assert!(
        !ro.path().join("denied.txt").exists(),
        "the read-only root has a file in it"
    );
}
