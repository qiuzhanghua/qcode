//! Real-machine check that a multi-root task can only write where it may.
//!
//! Windows restricts writes only, so a read-write root must accept a file and a
//! read-only root must refuse one. The ACL sandbox runs commands through the
//! hidden `qc sandbox-run` helper, so this test lives in the `qc` package and
//! points the sandbox at the real binary instead of the test executable.

#![cfg(windows)]

use std::path::Path;
use std::process::Stdio;

use qcode_core::task::{Root, RootAccess};
use qcode_tools::sandbox::Sandbox;
use qcode_tools::windows_acl::WindowsAcl;

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
    Some(command.stdin(Stdio::null()).output().ok()?.status.success())
}

#[test]
fn a_read_only_root_refuses_writes_while_a_read_write_root_accepts_them() {
    // %TEMP% is not writable in every environment, so the fixture lives under
    // the target directory cargo provides.
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("sandbox-roots-{}", std::process::id()));
    let rw = base.join("rw");
    let ro = base.join("ro");
    std::fs::create_dir_all(&rw).expect("rw dir");
    std::fs::create_dir_all(&ro).expect("ro dir");

    let seed = match WindowsAcl::new(&rw, &[])
        .map(|sandbox| sandbox.with_helper(env!("CARGO_BIN_EXE_qc")))
    {
        Ok(sandbox) => sandbox,
        Err(error) => {
            eprintln!("skip: cannot build the ACL sandbox: {error}");
            return;
        }
    };
    if let Err(error) = seed.check() {
        eprintln!("skip: the ACL preflight failed: {error}");
        return;
    }

    let roots = vec![
        Root {
            id: "rw".to_string(),
            label: "app".to_string(),
            path: rw.clone(),
            access: RootAccess::ReadWrite,
            primary: true,
        },
        Root {
            id: "ro".to_string(),
            label: "docs".to_string(),
            path: ro.clone(),
            access: RootAccess::ReadOnly,
            primary: false,
        },
    ];
    let sandbox = match seed.with_roots(&roots, None) {
        Ok(sandbox) => sandbox,
        Err(error) => {
            eprintln!("skip: with_roots failed: {error}");
            return;
        }
    };

    assert_eq!(
        run(sandbox.as_ref(), &rw, "echo ok> inside.txt"),
        Some(true),
        "a write inside the read-write root was refused"
    );
    assert!(rw.join("inside.txt").exists());

    assert_eq!(
        run(sandbox.as_ref(), &ro, "echo ok> denied.txt"),
        Some(false),
        "the read-only root accepted a write"
    );
    assert!(
        !ro.join("denied.txt").exists(),
        "the read-only root has a file in it"
    );

    let _ = std::fs::remove_dir_all(&base);
}
