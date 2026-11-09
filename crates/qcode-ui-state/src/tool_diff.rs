//! Diffs for transcripts that were written before schema v5.
//!
//! v5 made every write tool record the change it made, but a session recorded
//! by an older build has only the call arguments. These helpers rebuild a
//! best-effort diff from them so an old transcript still shows *that* a file
//! changed, and — for `apply_patch`, whose arguments carry a whole unified
//! patch — exactly what changed.

use qcode_core::event::{DiffFileStat, DiffKind, ToolDiff};
use serde_json::Value;

/// Same cap the writing tools use: a long patch is dropped rather than stored
/// twice.
const MAX_PATCH_BYTES: usize = 64 * 1024;

pub(crate) fn fallback(name: &str, arguments: &Value) -> Option<ToolDiff> {
    match name {
        "apply_patch" => patch_from_arguments(arguments),
        "write_file" => created_from_arguments(arguments),
        "edit_file" => edit_from_arguments(arguments),
        _ => None,
    }
}

fn patch_from_arguments(arguments: &Value) -> Option<ToolDiff> {
    let patch = arguments.get("diff")?.as_str()?.to_string();
    let files = stats_from_patch(&patch);
    if files.is_empty() {
        return None;
    }
    let truncated = patch.len() > MAX_PATCH_BYTES;
    Some(ToolDiff {
        patch: if truncated { String::new() } else { patch },
        files,
        truncated,
    })
}

fn created_from_arguments(arguments: &Value) -> Option<ToolDiff> {
    let path = arguments.get("path")?.as_str()?;
    let content = arguments.get("content")?.as_str()?;
    let mut patch = format!("--- /dev/null\n+++ b/{path}\n");
    let mut lines = 0_u32;
    for line in content.lines() {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
        lines += 1;
    }
    let truncated = patch.len() > MAX_PATCH_BYTES;
    Some(ToolDiff {
        patch: if truncated { String::new() } else { patch },
        files: vec![DiffFileStat {
            path: path.to_string(),
            kind: DiffKind::Created,
            additions: lines,
            deletions: 0,
            bytes: content.len() as u64,
        }],
        truncated,
    })
}

/// `edit_file` only knows the two fragments, not the file around them, so the
/// counts are right but there is nothing honest to render as a patch.
fn edit_from_arguments(arguments: &Value) -> Option<ToolDiff> {
    let path = arguments.get("path")?.as_str()?;
    let old = arguments.get("old_string").and_then(|value| value.as_str());
    let new = arguments.get("new_string").and_then(|value| value.as_str());
    if old.is_none() && new.is_none() {
        return None;
    }
    let additions = new.map(|text| text.lines().count() as u32).unwrap_or(0);
    let deletions = old.map(|text| text.lines().count() as u32).unwrap_or(0);
    Some(ToolDiff {
        patch: String::new(),
        files: vec![DiffFileStat {
            path: path.to_string(),
            kind: DiffKind::Modified,
            additions,
            deletions,
            bytes: 0,
        }],
        truncated: true,
    })
}

/// Walks a unified diff and collects the per-file counts the row chips show.
fn stats_from_patch(patch: &str) -> Vec<DiffFileStat> {
    let mut files: Vec<DiffFileStat> = Vec::new();
    let mut current: Option<(String, DiffKind)> = None;
    let mut additions = 0_u32;
    let mut deletions = 0_u32;

    let flush = |current: &mut Option<(String, DiffKind)>,
                 additions: &mut u32,
                 deletions: &mut u32,
                 files: &mut Vec<DiffFileStat>| {
        if let Some((path, kind)) = current.take() {
            files.push(DiffFileStat {
                path,
                kind,
                additions: *additions,
                deletions: *deletions,
                bytes: 0,
            });
        }
        *additions = 0;
        *deletions = 0;
    };

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            // A new file header closes the previous one.
            flush(&mut current, &mut additions, &mut deletions, &mut files);
            current = Some((strip_prefix(rest), DiffKind::Modified));
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let path = strip_prefix(rest);
            let kind = match current.as_ref().map(|(old, _)| old.as_str()) {
                Some("/dev/null") => DiffKind::Created,
                Some(_) if path == "/dev/null" => DiffKind::Deleted,
                _ => DiffKind::Modified,
            };
            let name = if path == "/dev/null" {
                current
                    .as_ref()
                    .map(|(old, _)| old.clone())
                    .unwrap_or_default()
            } else {
                path
            };
            current = Some((name, kind));
        } else if let Some(rest) = line.strip_prefix('+') {
            if !rest.starts_with("++") {
                additions += 1;
            }
        } else if let Some(rest) = line.strip_prefix('-')
            && !rest.starts_with("--")
        {
            deletions += 1;
        }
    }
    flush(&mut current, &mut additions, &mut deletions, &mut files);
    files
}

fn strip_prefix(line: &str) -> String {
    let name = line.split('\t').next().unwrap_or(line).trim();
    name.strip_prefix("a/")
        .or_else(|| name.strip_prefix("b/"))
        .unwrap_or(name)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_old_apply_patch_keeps_its_patch_and_per_file_counts() {
        let patch = "--- a/src/one.rs\n+++ b/src/one.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n context\n--- /dev/null\n+++ b/two.rs\n@@ -0,0 +1 @@\n+hello\n";
        let diff = fallback("apply_patch", &json!({ "diff": patch })).unwrap();
        assert_eq!(diff.patch, patch);
        assert_eq!(diff.files.len(), 2);
        assert_eq!(diff.files[0].path, "src/one.rs");
        assert_eq!(diff.files[0].kind, DiffKind::Modified);
        assert_eq!((diff.files[0].additions, diff.files[0].deletions), (1, 1));
        assert_eq!(diff.files[1].path, "two.rs");
        assert_eq!(diff.files[1].kind, DiffKind::Created);
        assert_eq!(diff.files[1].additions, 1);
        assert!(!diff.truncated);
    }

    #[test]
    fn an_old_write_file_is_all_green() {
        let diff = fallback(
            "write_file",
            &json!({"path": "notes.md", "content": "a\nb\n"}),
        )
        .unwrap();
        assert_eq!(diff.files[0].kind, DiffKind::Created);
        assert_eq!(diff.files[0].additions, 2);
        assert_eq!(diff.files[0].bytes, 4);
        assert!(diff.patch.contains("+b\n"));
    }

    #[test]
    fn an_old_edit_file_only_knows_the_two_fragments() {
        let diff = fallback(
            "edit_file",
            &json!({"path": "a.rs", "old_string": "one\ntwo", "new_string": "one\n2\nthree"}),
        )
        .unwrap();
        assert_eq!(diff.files[0].path, "a.rs");
        assert_eq!((diff.files[0].additions, diff.files[0].deletions), (3, 2));
        assert!(diff.patch.is_empty());
        assert!(diff.truncated);
    }

    #[test]
    fn a_read_only_call_has_no_diff() {
        assert!(fallback("read_file", &json!({"path": "a.rs"})).is_none());
    }
}
