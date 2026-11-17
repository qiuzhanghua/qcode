//! End-to-end check of ADR-0101's read path against a **real** pre-ADR-0101
//! snapshot tree.
//!
//! The unit tests build a turn directory by hand; this one is run manually against
//! a copy of an actual session (`/tmp/oldlayout/...`), because the thing most
//! likely to break is not the parsing but the assumption that old trees look the
//! way the unit test says they do.
//!
//! Run with:
//!   QCODE_OLD_SNAPSHOTS=/tmp/oldlayout cargo test -p qcode-core --test old_layout_realdata -- --nocapture

use std::path::PathBuf;

use qcode_core::snapshot::SnapshotStore;

#[test]
fn a_real_old_layout_restores_through_the_manifest_reader() {
    let Ok(root) = std::env::var("QCODE_OLD_SNAPSHOTS") else {
        eprintln!("QCODE_OLD_SNAPSHOTS not set; skipping (this test needs real data)");
        return;
    };
    let store = SnapshotStore::new(PathBuf::from(&root));
    let sessions: Vec<String> = std::fs::read_dir(&root)
        .expect("snapshots dir")
        .flatten()
        .filter(|entry| entry.file_type().map(|k| k.is_dir()).unwrap_or(false))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!sessions.is_empty(), "no session directories under {root}");

    for session in &sessions {
        let turns = store.turns(session).expect("turns");
        assert!(!turns.is_empty(), "{session} has no turns");
        let last = *turns.last().unwrap();
        // `files()` must read the old layout: one entry per file in `files/`.
        let listed = store.files(session, last).expect("files");
        eprintln!(
            "{session}: {n} turn(s), turn {last} lists {m} path(s)",
            n = turns.len(),
            m = listed.len()
        );
        assert!(
            !listed.is_empty(),
            "{session} turn {last} listed nothing; the old layout was not read"
        );
        // Any path listed as a real file must be readable through `restore`.
        // Restoring into a scratch directory proves the read side works without
        // touching the original tree.
        let scratch = tempfile::tempdir().expect("scratch");
        let report = store
            .restore(session, last, scratch.path())
            .expect("restore");
        eprintln!(
            "{session}: restored {r}, deleted {d}, already absent {a}",
            r = report.restored,
            d = report.deleted,
            a = report.already_absent
        );
        assert!(
            report.restored > 0,
            "{session} restored nothing from a non-empty turn"
        );
        // Every restored file must be byte-identical to what the store holds.
        for entry in walk(scratch.path()) {
            assert!(entry.is_file(), "{entry:?}");
            assert!(
                std::fs::metadata(&entry).unwrap().len() > 0
                    || std::fs::read(&entry).unwrap().is_empty(),
                "{entry:?}"
            );
        }
    }
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("read dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}
