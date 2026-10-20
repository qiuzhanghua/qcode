//! `apply_patch`: apply a unified diff inside the workspace.
//!
//! The whole patch is parsed and every file is computed before anything is
//! written. Writes are staged next to their targets and committed with
//! rename+backup, so a validation error never leaves a half-applied patch.
//! Paths are workspace-relative; absolute paths, `..` and renames are rejected.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use async_trait::async_trait;
use qcode_core::event::ToolMetadata;
use qcode_core::provider::ToolSpec;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::path;

const MAX_PATCH_BYTES: usize = 256 * 1024;
const MAX_FILES: usize = 20;
const DEV_NULL: &str = "/dev/null";

pub struct ApplyPatch;

#[derive(Deserialize)]
struct PatchArgs {
    diff: String,
}

#[async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".to_string(),
            description: "Apply a unified diff to files inside the workspace. The whole patch is \
                          validated first; if any hunk or path fails, nothing is written. Absolute \
                          paths, `..` and renames are rejected."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "diff": {
                        "type": "string",
                        "description": "Unified diff, e.g. `--- a/src/x.rs` / `+++ b/src/x.rs`"
                    }
                },
                "required": ["diff"]
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::BulkWrite
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: PatchArgs =
            serde_json::from_value(args).map_err(|error| ToolError::Invalid(error.to_string()))?;
        if args.diff.trim().is_empty() {
            return Err(ToolError::Invalid("diff is empty".to_string()));
        }
        if args.diff.len() > MAX_PATCH_BYTES {
            return Err(ToolError::Invalid(format!(
                "diff is {} bytes; the limit is {MAX_PATCH_BYTES}",
                args.diff.len()
            )));
        }

        let patches = parse_patch(&args.diff)?;
        if patches.len() > MAX_FILES {
            return Err(ToolError::Invalid(format!(
                "patch touches {} files; the limit is {MAX_FILES}",
                patches.len()
            )));
        }

        let mut seen = BTreeSet::new();
        let mut ops = Vec::with_capacity(patches.len());
        for patch in &patches {
            let op = plan_file(ctx, patch)?;
            if !seen.insert(op.target.clone()) {
                return Err(ToolError::Invalid(format!(
                    "patch touches the same file twice: {}",
                    op.target.display()
                )));
            }
            ops.push(op);
        }

        let summary = commit(ops)?;
        Ok(ToolOutput::ok(summary).with_metadata(ToolMetadata {
            bytes: args.diff.len() as u64,
            ..ToolMetadata::default()
        }))
    }
}

#[derive(Debug)]
struct FilePatch {
    old_path: String,
    new_path: String,
    hunks: Vec<Hunk>,
}

impl FilePatch {
    fn is_create(&self) -> bool {
        self.old_path == DEV_NULL
    }

    fn is_delete(&self) -> bool {
        self.new_path == DEV_NULL
    }
}

#[derive(Debug)]
struct Hunk {
    old_start: usize,
    old_len: usize,
    new_len: usize,
    lines: Vec<PatchLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Context,
    Add,
    Remove,
}

#[derive(Debug)]
struct PatchLine {
    kind: LineKind,
    text: String,
    no_newline: bool,
}

fn parse_patch(text: &str) -> Result<Vec<FilePatch>, ToolError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0;
    let mut patches = Vec::new();

    while index < lines.len() {
        let Some(old_raw) = lines[index].strip_prefix("--- ") else {
            index += 1;
            continue;
        };
        let old_path = parse_path(old_raw)?;
        index += 1;

        let Some(new_raw) = lines.get(index).and_then(|line| line.strip_prefix("+++ ")) else {
            return Err(ToolError::Invalid(format!(
                "missing `+++` line after `--- {}`",
                old_path
            )));
        };
        let new_path = parse_path(new_raw)?;
        index += 1;

        let mut hunks = Vec::new();
        while index < lines.len() {
            if lines[index].starts_with("--- ")
                || lines[index].starts_with("diff --git ")
                || lines[index].starts_with("index ")
                || lines[index].starts_with("new file mode ")
                || lines[index].starts_with("deleted file mode ")
                || lines[index].starts_with("similarity index ")
            {
                break;
            }
            if lines[index].trim().is_empty() {
                index += 1;
                continue;
            }
            let Some((old_start, old_len, new_len)) = parse_hunk_header(lines[index])? else {
                return Err(ToolError::Invalid(format!(
                    "expected a hunk header, got: {}",
                    lines[index]
                )));
            };
            index += 1;

            let mut hunk_lines: Vec<PatchLine> = Vec::new();
            let mut old_seen = 0usize;
            let mut new_seen = 0usize;
            while index < lines.len() {
                let line = lines[index];
                if line.starts_with("@@ ")
                    || line.starts_with("diff --git ")
                    || line.starts_with("index ")
                    || line.starts_with("new file mode ")
                    || line.starts_with("deleted file mode ")
                    || line.starts_with("similarity index ")
                {
                    break;
                }
                // `--- ` is also a valid removed line (prefix `-` + `-- ...`).
                // Only treat it as the next file header once this hunk's
                // counts are satisfied.
                if line.starts_with("--- ") && old_seen >= old_len && new_seen >= new_len {
                    break;
                }
                if line.starts_with("\\ No newline at end of file") {
                    let Some(last) = hunk_lines.last_mut() else {
                        return Err(ToolError::Invalid(
                            "`\\ No newline` without a preceding line".to_string(),
                        ));
                    };
                    last.no_newline = true;
                    index += 1;
                    continue;
                }
                if line.is_empty() {
                    hunk_lines.push(PatchLine {
                        kind: LineKind::Context,
                        text: String::new(),
                        no_newline: false,
                    });
                    old_seen += 1;
                    new_seen += 1;
                    index += 1;
                    continue;
                }
                let (kind, text) = match line.as_bytes()[0] {
                    b' ' => (LineKind::Context, &line[1..]),
                    b'+' => (LineKind::Add, &line[1..]),
                    b'-' => (LineKind::Remove, &line[1..]),
                    _ => {
                        return Err(ToolError::Invalid(format!("unexpected hunk line: {line}")));
                    }
                };
                match kind {
                    LineKind::Context => {
                        old_seen += 1;
                        new_seen += 1;
                    }
                    LineKind::Add => new_seen += 1,
                    LineKind::Remove => old_seen += 1,
                }
                hunk_lines.push(PatchLine {
                    kind,
                    text: text.to_string(),
                    no_newline: false,
                });
                index += 1;
            }

            if old_seen != old_len || new_seen != new_len {
                return Err(ToolError::Invalid(format!(
                    "hunk at line {} has {old_seen}/{new_seen} lines, header says {old_len}/{new_len}",
                    old_start + 1
                )));
            }

            hunks.push(Hunk {
                old_start,
                old_len,
                new_len,
                lines: hunk_lines,
            });
        }

        if hunks.is_empty() {
            return Err(ToolError::Invalid(format!(
                "no hunks for file {}",
                if new_path == DEV_NULL {
                    &old_path
                } else {
                    &new_path
                }
            )));
        }
        patches.push(FilePatch {
            old_path,
            new_path,
            hunks,
        });
    }

    if patches.is_empty() {
        return Err(ToolError::Invalid(
            "no file patches found; expected a unified diff".to_string(),
        ));
    }
    Ok(patches)
}

fn parse_path(raw: &str) -> Result<String, ToolError> {
    let path = raw.split('\t').next().unwrap_or("").trim();
    if path.is_empty() {
        return Err(ToolError::Invalid("empty path in diff header".to_string()));
    }
    Ok(path.to_string())
}

fn parse_hunk_header(line: &str) -> Result<Option<(usize, usize, usize)>, ToolError> {
    static HUNK: OnceLock<Regex> = OnceLock::new();
    let regex = HUNK.get_or_init(|| {
        Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").expect("valid hunk regex")
    });
    let Some(captures) = regex.captures(line) else {
        return Ok(None);
    };

    let number = |index: usize, default: usize| -> Result<usize, ToolError> {
        match captures.get(index) {
            Some(value) => value
                .as_str()
                .parse()
                .map_err(|_| ToolError::Invalid(format!("invalid hunk header: {line}"))),
            None => Ok(default),
        }
    };

    Ok(Some((number(1, 0)?, number(2, 1)?, number(4, 1)?)))
}

/// One planned change. `content = None` means delete.
struct PlannedOp {
    target: PathBuf,
    content: Option<String>,
    existed: bool,
}

fn plan_file(ctx: &ToolContext, patch: &FilePatch) -> Result<PlannedOp, ToolError> {
    let target_name = if patch.is_create() {
        if patch.is_delete() {
            return Err(ToolError::Invalid(
                "a patch cannot be both create and delete".to_string(),
            ));
        }
        Some(&patch.new_path)
    } else {
        Some(&patch.old_path)
    };
    let Some(raw_target) = target_name else {
        return Err(ToolError::Invalid("missing file path".to_string()));
    };
    if raw_target == DEV_NULL {
        return Err(ToolError::Invalid("missing file path".to_string()));
    }

    let trimmed = raw_target
        .strip_prefix("a/")
        .or_else(|| raw_target.strip_prefix("b/"))
        .unwrap_or(raw_target);
    validate_relative(trimmed)?;

    if !patch.is_create() && !patch.is_delete() {
        let old = patch
            .old_path
            .strip_prefix("a/")
            .or_else(|| patch.old_path.strip_prefix("b/"))
            .unwrap_or(&patch.old_path);
        let new = patch
            .new_path
            .strip_prefix("a/")
            .or_else(|| patch.new_path.strip_prefix("b/"))
            .unwrap_or(&patch.new_path);
        if old != new {
            return Err(ToolError::Invalid(format!(
                "renames are not supported: {old} -> {new}"
            )));
        }
    }

    let target = path::resolve(&ctx.workspace, trimmed)?;
    let metadata = std::fs::symlink_metadata(&target).ok();
    let existed = metadata.is_some();

    match (patch.is_create(), patch.is_delete()) {
        (true, _) if existed => {
            return Err(ToolError::Invalid(format!(
                "cannot create {}: it already exists",
                target.display()
            )));
        }
        (_, true) if !existed => {
            return Err(ToolError::Invalid(format!(
                "cannot delete {}: it does not exist",
                target.display()
            )));
        }
        _ => {}
    }

    let original = if existed {
        std::fs::read_to_string(&target)?
    } else {
        String::new()
    };
    let updated = apply_hunks(&original, &patch.hunks, &target)?;

    if patch.is_delete() {
        if !updated.trim().is_empty() {
            return Err(ToolError::Invalid(format!(
                "delete patch for {} leaves content behind",
                target.display()
            )));
        }
        return Ok(PlannedOp {
            target,
            content: None,
            existed,
        });
    }

    Ok(PlannedOp {
        target,
        content: Some(updated),
        existed,
    })
}

fn validate_relative(raw: &str) -> Result<(), ToolError> {
    let path = Path::new(raw);
    // `has_root` also catches a Windows path like `/etc/passwd`, which is not
    // `is_absolute` there but would still escape the workspace by resolving
    // against the drive root.
    if path.is_absolute() || path.has_root() {
        return Err(ToolError::Invalid(format!(
            "absolute paths are not allowed: {raw}"
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ToolError::Invalid(format!(
            "`..` is not allowed in patch paths: {raw}"
        )));
    }
    if path.components().next().is_none() {
        return Err(ToolError::Invalid("empty path in patch".to_string()));
    }
    Ok(())
}

fn apply_hunks(original: &str, hunks: &[Hunk], target: &Path) -> Result<String, ToolError> {
    let (input, mut trailing_newline) = split_lines(original);
    let mut output = Vec::new();
    let mut consumed = 0;

    for (hunk_index, hunk) in hunks.iter().enumerate() {
        let start = if hunk.old_len == 0 {
            hunk.old_start
        } else {
            hunk.old_start.saturating_sub(1)
        };
        if start < consumed {
            return Err(ToolError::Invalid(format!(
                "{}: hunk {} overlaps the previous hunk",
                target.display(),
                hunk_index + 1
            )));
        }
        if start > input.len() {
            return Err(ToolError::Invalid(format!(
                "{}: hunk {} starts after the end of the file",
                target.display(),
                hunk_index + 1
            )));
        }

        output.extend(input[consumed..start].iter().cloned());
        let mut cursor = start;
        let mut old_seen = 0;
        let mut new_seen = 0;

        for line in &hunk.lines {
            match line.kind {
                LineKind::Context => {
                    if input.get(cursor).map(String::as_str) != Some(line.text.as_str()) {
                        return Err(context_error(
                            target,
                            hunk_index,
                            cursor,
                            &line.text,
                            input.get(cursor),
                        ));
                    }
                    output.push(line.text.clone());
                    cursor += 1;
                    old_seen += 1;
                    new_seen += 1;
                }
                LineKind::Remove => {
                    if input.get(cursor).map(String::as_str) != Some(line.text.as_str()) {
                        return Err(context_error(
                            target,
                            hunk_index,
                            cursor,
                            &line.text,
                            input.get(cursor),
                        ));
                    }
                    cursor += 1;
                    old_seen += 1;
                }
                LineKind::Add => {
                    output.push(line.text.clone());
                    new_seen += 1;
                }
            }
        }

        if old_seen != hunk.old_len || new_seen != hunk.new_len {
            return Err(ToolError::Invalid(format!(
                "{}: hunk {} counts do not match its header",
                target.display(),
                hunk_index + 1
            )));
        }
        consumed = cursor;
    }

    output.extend(input[consumed..].iter().cloned());

    // A `\ No newline` marker on the last line produced by the last hunk
    // controls only end-of-file changes.
    if consumed == input.len()
        && let Some(last_hunk) = hunks.last()
        && let Some(line) = last_hunk
            .lines
            .iter()
            .rev()
            .find(|line| line.kind != LineKind::Remove)
    {
        match line.kind {
            LineKind::Add => trailing_newline = !line.no_newline,
            LineKind::Context if line.no_newline => trailing_newline = false,
            _ => {}
        }
    }

    Ok(join_lines(&output, trailing_newline))
}

fn context_error(
    target: &Path,
    hunk_index: usize,
    line: usize,
    expected: &str,
    actual: Option<&String>,
) -> ToolError {
    ToolError::Invalid(format!(
        "{}: hunk {} does not match at line {}: expected {:?}, found {:?}",
        target.display(),
        hunk_index + 1,
        line + 1,
        expected,
        actual.map(String::as_str)
    ))
}

fn split_lines(text: &str) -> (Vec<String>, bool) {
    if text.is_empty() {
        return (Vec::new(), true);
    }
    let trailing = text.ends_with('\n');
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    if trailing {
        lines.pop();
    }
    (lines, trailing)
}

fn join_lines(lines: &[String], trailing: bool) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut text = lines.join("\n");
    if trailing {
        text.push('\n');
    }
    text
}

/// Stage every change, then commit with backups. On failure, restore what was
/// already moved so the patch stays all-or-nothing.
fn commit(ops: Vec<PlannedOp>) -> Result<String, ToolError> {
    let mut staged: Vec<StagedOp> = Vec::with_capacity(ops.len());
    let pid = std::process::id();

    for (index, op) in ops.into_iter().enumerate() {
        let temp = op.target.with_file_name(format!(
            ".{}.qcode-patch-{pid}-{index}",
            op.target
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("file")
        ));
        if let Some(content) = &op.content {
            if let Some(parent) = op.target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&temp, content)?;
        }
        staged.push(StagedOp {
            target: op.target,
            temp: op.content.as_ref().map(|_| temp),
            backup: None,
            existed: op.existed,
            delete: op.content.is_none(),
        });
    }

    let result = commit_staged(&mut staged);
    if result.is_err() {
        rollback(&mut staged);
    }
    result?;

    let mut summary = String::from("applied patch:\n");
    for op in &staged {
        let action = if !op.existed {
            "added"
        } else if op.delete {
            "deleted"
        } else {
            "modified"
        };
        summary.push_str(&format!("- {action} {}\n", op.target.display()));
    }
    Ok(summary)
}

struct StagedOp {
    target: PathBuf,
    temp: Option<PathBuf>,
    backup: Option<PathBuf>,
    existed: bool,
    delete: bool,
}

fn commit_staged(staged: &mut [StagedOp]) -> Result<(), ToolError> {
    let pid = std::process::id();
    for (index, op) in staged.iter_mut().enumerate() {
        if op.existed {
            let backup = op.target.with_file_name(format!(
                ".{}.qcode-backup-{pid}-{index}",
                op.target
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("file")
            ));
            std::fs::rename(&op.target, &backup)?;
            op.backup = Some(backup);
        }
        if let Some(temp) = &op.temp {
            std::fs::rename(temp, &op.target)?;
            op.temp = None;
        }
    }

    for op in staged.iter() {
        if let Some(backup) = &op.backup {
            let _ = std::fs::remove_file(backup);
        }
    }
    Ok(())
}

fn rollback(staged: &mut [StagedOp]) {
    for op in staged.iter_mut().rev() {
        if let Some(temp) = &op.temp {
            let _ = std::fs::remove_file(temp);
        } else if !op.existed {
            // A create whose temp was already renamed into place.
            let _ = std::fs::remove_file(&op.target);
        }
        if let Some(backup) = &op.backup {
            // The new content may have been renamed into place; then restore.
            let _ = std::fs::remove_file(&op.target);
            let _ = std::fs::rename(backup, &op.target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_defaults_to_bulk_write() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ApplyPatch.approval(&ToolContext::new(dir.path()), &json!({})),
            ApprovalClass::BulkWrite
        );
    }

    async fn run(workspace: &Path, diff: &str) -> Result<ToolOutput, ToolError> {
        ApplyPatch
            .execute(&ToolContext::new(workspace), json!({ "diff": diff }))
            .await
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[tokio::test]
    async fn modifies_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

        let output = run(
            dir.path(),
            "--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n",
        )
        .await
        .unwrap();

        assert!(output.output.contains("modified"));
        assert_eq!(read(&file), "one\nTWO\nthree\n");
    }

    #[tokio::test]
    async fn creates_and_deletes_files() {
        let dir = tempfile::tempdir().unwrap();

        let output = run(
            dir.path(),
            "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+hello\n+world\n",
        )
        .await
        .unwrap();
        assert!(output.output.contains("added"));
        assert_eq!(read(&dir.path().join("new.txt")), "hello\nworld\n");

        let output = run(
            dir.path(),
            "--- a/new.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-hello\n-world\n",
        )
        .await
        .unwrap();
        assert!(output.output.contains("deleted"));
        assert!(!dir.path().join("new.txt").exists());
    }

    #[tokio::test]
    async fn a_bad_hunk_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        std::fs::write(&first, "one\n").unwrap();
        std::fs::write(&second, "other\n").unwrap();

        let error = run(
            dir.path(),
            "--- a/first.txt\n+++ b/first.txt\n@@ -1,1 +1,1 @@\n-one\n+ONE\n\
             --- a/second.txt\n+++ b/second.txt\n@@ -1,1 +1,1 @@\n-missing\n+M\n",
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("does not match"));
        assert_eq!(read(&first), "one\n");
        assert_eq!(read(&second), "other\n");
    }

    #[tokio::test]
    async fn rejects_absolute_and_parent_paths() {
        let dir = tempfile::tempdir().unwrap();

        let absolute = run(
            dir.path(),
            "--- /etc/passwd\n+++ /etc/passwd\n@@ -1,1 +1,1 @@\n-a\n+b\n",
        )
        .await
        .unwrap_err();
        assert!(absolute.to_string().contains("absolute paths"));

        let parent = run(
            dir.path(),
            "--- a/../escape.txt\n+++ b/../escape.txt\n@@ -1,1 +1,1 @@\n-a\n+b\n",
        )
        .await
        .unwrap_err();
        assert!(parent.to_string().contains(".."));
    }

    #[tokio::test]
    async fn rejects_renames_and_missing_targets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.txt"), "same\n").unwrap();

        let rename = run(
            dir.path(),
            "--- a/old.txt\n+++ b/new.txt\n@@ -1,1 +1,1 @@\n-same\n+different\n",
        )
        .await
        .unwrap_err();
        assert!(rename.to_string().contains("renames are not supported"));

        let create_existing = run(
            dir.path(),
            "--- /dev/null\n+++ b/old.txt\n@@ -0,0 +1,1 @@\n+x\n",
        )
        .await
        .unwrap_err();
        assert!(create_existing.to_string().contains("already exists"));

        let delete_missing = run(
            dir.path(),
            "--- a/missing.txt\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-x\n",
        )
        .await
        .unwrap_err();
        assert!(delete_missing.to_string().contains("does not exist"));
    }

    #[tokio::test]
    async fn handles_missing_final_newline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("no-newline.txt");
        std::fs::write(&file, "old").unwrap();

        run(
            dir.path(),
            "--- a/no-newline.txt\n+++ b/no-newline.txt\n@@ -1,1 +1,1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n",
        )
        .await
        .unwrap();

        assert_eq!(read(&file), "new");
    }

    #[tokio::test]
    async fn skips_git_preamble_between_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();

        let output = run(
            dir.path(),
            "diff --git a/a.txt b/a.txt\nindex 111..222 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,1 +1,1 @@\n-a\n+A\n\
             diff --git a/b.txt b/b.txt\nindex 333..444 100644\n--- a/b.txt\n+++ b/b.txt\n@@ -1,1 +1,1 @@\n-b\n+B\n",
        )
        .await
        .unwrap();

        assert!(output.output.contains("modified"));
        assert_eq!(read(&dir.path().join("a.txt")), "A\n");
        assert_eq!(read(&dir.path().join("b.txt")), "B\n");
    }

    #[tokio::test]
    async fn removed_lines_may_start_with_dashes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("dashes.txt");
        std::fs::write(&file, "keep\n-- removed line\ntail\n").unwrap();

        run(
            dir.path(),
            "--- a/dashes.txt\n+++ b/dashes.txt\n@@ -1,3 +1,2 @@\n keep\n--- removed line\n tail\n",
        )
        .await
        .unwrap();

        assert_eq!(read(&file), "keep\ntail\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_a_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "secret\n").unwrap();
        symlink(&secret, dir.path().join("link.txt")).unwrap();

        let error = run(
            dir.path(),
            "--- a/link.txt\n+++ b/link.txt\n@@ -1,1 +1,1 @@\n-secret\n+changed\n",
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("escapes the workspace"));
        assert_eq!(read(&secret), "secret\n");
    }
}
