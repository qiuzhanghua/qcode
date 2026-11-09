//! Unified diffs for the tools that write.
//!
//! The transcript shows what an edit actually changed, so the write tools hand
//! their before/after text here and the resulting [`ToolDiff`] rides along on
//! the [`ToolOutput`](qcode_core::tool::ToolOutput). A read-only tool never
//! touches this module.

use qcode_core::event::{DiffFileStat, DiffKind, ToolDiff};
use similar::{ChangeTag, TextDiff};

/// Lines of context around each change, the same radius `git diff` uses.
const CONTEXT: usize = 3;

/// Beyond this the patch is dropped and only the counts survive: a pathological
/// write must not double the size of a session log.
const MAX_PATCH_BYTES: usize = 64 * 1024;

/// One file's change: `before` is `None` for a new file, `after` for a deleted
/// one. A rewrite that produced identical text yields nothing to show, so it
/// reports `None` rather than an empty diff.
pub(crate) fn file(path: &str, before: Option<&str>, after: Option<&str>) -> Option<ToolDiff> {
    let kind = match (before, after) {
        (None, _) => DiffKind::Created,
        (_, None) => DiffKind::Deleted,
        _ => DiffKind::Modified,
    };
    let old = before.unwrap_or_default();
    let new = after.unwrap_or_default();

    let text = TextDiff::from_lines(old, new);
    let mut additions = 0_u32;
    let mut deletions = 0_u32;
    for change in text.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => additions += 1,
            ChangeTag::Delete => deletions += 1,
            ChangeTag::Equal => {}
        }
    }

    if additions == 0 && deletions == 0 {
        return None;
    }

    // `a/` and `b/` prefixes with `/dev/null` on the missing side: what
    // `git diff` writes, and what the diff viewer expects to read back.
    let old_header = if before.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".to_string()
    };
    let new_header = if after.is_some() {
        format!("b/{path}")
    } else {
        "/dev/null".to_string()
    };
    let patch = text
        .unified_diff()
        .context_radius(CONTEXT)
        .header(&old_header, &new_header)
        .to_string();

    let mut truncated = false;
    let patch = if patch.len() > MAX_PATCH_BYTES {
        truncated = true;
        String::new()
    } else {
        patch
    };

    Some(ToolDiff {
        patch,
        files: vec![DiffFileStat {
            path: path.to_string(),
            kind,
            additions,
            deletions,
            bytes: new.len() as u64,
        }],
        truncated,
    })
}

/// Joins per-file diffs into the one `ToolDiff` a multi-file call reports.
pub(crate) fn combine(mut diffs: Vec<ToolDiff>) -> Option<ToolDiff> {
    if diffs.is_empty() {
        return None;
    }
    if diffs.len() == 1 {
        return diffs.pop();
    }
    let mut patch = String::new();
    let mut files = Vec::new();
    let mut truncated = false;
    for diff in diffs {
        truncated |= diff.truncated;
        files.extend(diff.files);
        if !diff.patch.is_empty() {
            if !patch.is_empty() {
                patch.push('\n');
            }
            patch.push_str(&diff.patch);
        }
    }
    if patch.len() > MAX_PATCH_BYTES {
        truncated = true;
        patch.clear();
    }
    Some(ToolDiff {
        patch,
        files,
        truncated,
    })
}

/// The largest file we are willing to read just to show a diff.
pub(crate) const MAX_SOURCE_BYTES: u64 = 256 * 1024;

/// Reads `path` as text when it is small enough and valid UTF-8; anything else
/// yields `None`, and the call simply has no diff.
pub(crate) fn read_source(path: &std::path::Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modified_file_gets_real_line_numbers_and_context() {
        let before = "one\ntwo\nthree\nfour\nfive\n";
        let after = "one\ntwo\nTHREE\nfour\nfive\n";
        let diff = file("src/lib.rs", Some(before), Some(after)).expect("a diff");
        assert_eq!(diff.files.len(), 1);
        let stat = &diff.files[0];
        assert_eq!(stat.kind, DiffKind::Modified);
        assert_eq!((stat.additions, stat.deletions), (1, 1));
        assert_eq!(stat.bytes, after.len() as u64);
        assert!(
            diff.patch
                .starts_with("--- a/src/lib.rs\n+++ b/src/lib.rs\n")
        );
        // The hunk header carries the real line numbers.
        assert!(diff.patch.contains("@@ -1,5 +1,5 @@"), "{}", diff.patch);
        assert!(diff.patch.contains("-three\n"));
        assert!(diff.patch.contains("+THREE\n"));
        assert!(!diff.truncated);
    }

    #[test]
    fn created_file_is_all_additions_against_dev_null() {
        let diff = file("notes.md", None, Some("a\nb\n")).expect("a diff");
        assert_eq!(diff.files[0].kind, DiffKind::Created);
        assert_eq!((diff.files[0].additions, diff.files[0].deletions), (2, 0));
        assert!(diff.patch.starts_with("--- /dev/null\n+++ b/notes.md\n"));
    }

    #[test]
    fn deleted_file_is_all_deletions() {
        let diff = file("notes.md", Some("a\nb\n"), None).expect("a diff");
        assert_eq!(diff.files[0].kind, DiffKind::Deleted);
        assert_eq!((diff.files[0].additions, diff.files[0].deletions), (0, 2));
        assert!(diff.patch.contains("+++ /dev/null"));
    }

    #[test]
    fn an_oversized_patch_keeps_the_counts_and_drops_the_text() {
        let before = String::new();
        let after: String = (0..20_000).map(|index| format!("line {index}\n")).collect();
        assert!(after.len() < MAX_SOURCE_BYTES as usize);
        let diff = file("big.txt", Some(&before), Some(&after)).expect("a diff");
        assert!(diff.truncated);
        assert!(diff.patch.is_empty());
        assert_eq!(diff.files[0].additions, 20_000);
    }

    #[test]
    fn a_multi_file_patch_concatenates_and_keeps_every_stat() {
        let one = file("a.txt", Some("x\n"), Some("y\n")).expect("a diff");
        let two = file("b.txt", None, Some("z\n")).expect("a diff");
        let combined = combine(vec![one, two]).unwrap();
        assert_eq!(combined.files.len(), 2);
        assert_eq!(combined.files[0].path, "a.txt");
        assert_eq!(combined.files[1].kind, DiffKind::Created);
        assert!(combined.patch.contains("--- a/a.txt"));
        assert!(combined.patch.contains("--- /dev/null"));
        assert!(!combined.truncated);
    }

    #[test]
    fn an_empty_change_list_has_no_diff() {
        assert!(combine(Vec::new()).is_none());
    }

    #[test]
    fn a_rewrite_that_changes_nothing_has_no_diff() {
        assert!(file("notes.md", Some("same\n"), Some("same\n")).is_none());
    }
}
