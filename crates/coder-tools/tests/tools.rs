use std::path::Path;

use coder_core::tool::{Tool, ToolContext};
use coder_tools::fs::{EditFile, ListDir, ReadFile, WriteFile};
use coder_tools::shell::RunShell;
use serde_json::json;

fn context(dir: &Path) -> ToolContext {
    ToolContext {
        workspace: dir.to_path_buf(),
    }
}

#[tokio::test]
async fn write_read_edit_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());

    WriteFile
        .execute(
            &ctx,
            json!({"path": "src/main.rs", "content": "fn main() {}\n"}),
        )
        .await
        .unwrap();

    let read = ReadFile
        .execute(&ctx, json!({"path": "src/main.rs"}))
        .await
        .unwrap();
    assert!(read.output.contains("1| fn main()"));

    EditFile
        .execute(
            &ctx,
            json!({
                "path": "src/main.rs",
                "old_string": "fn main()",
                "new_string": "fn run()"
            }),
        )
        .await
        .unwrap();

    let read = ReadFile
        .execute(&ctx, json!({"path": "src/main.rs"}))
        .await
        .unwrap();
    assert!(read.output.contains("fn run()"));
    assert!(!read.output.contains("fn main()"));
}

#[tokio::test]
async fn edit_requires_unique_match() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    WriteFile
        .execute(&ctx, json!({"path": "a.txt", "content": "x\nx\n"}))
        .await
        .unwrap();

    let output = EditFile
        .execute(
            &ctx,
            json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
        )
        .await
        .unwrap();
    assert_eq!(output.status, coder_core::event::ToolStatus::Error);
    assert!(output.output.contains("appears 2 times"));
}

#[tokio::test]
async fn rejects_parent_directory_escape() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());

    let error = WriteFile
        .execute(&ctx, json!({"path": "../escape.txt", "content": "nope"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("escapes"));
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlink_escape() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();

    let ctx = context(dir.path());
    let error = WriteFile
        .execute(&ctx, json!({"path": "link/evil.txt", "content": "nope"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("escapes"));
}

#[tokio::test]
async fn list_dir_skips_build_directories() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    std::fs::create_dir_all(dir.path().join("target")).unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn x() {}\n").unwrap();

    let output = ListDir.execute(&ctx, json!({"path": "."})).await.unwrap();
    assert!(output.output.contains("src"));
    assert!(!output.output.contains("target"));
    assert!(!output.output.contains(".git"));
}

#[tokio::test]
async fn shell_reports_exit_code_and_scrubs_env() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());

    // SAFETY: tests use a unique variable name; no other test reads it.
    unsafe { std::env::set_var("CODER_TEST_SECRET", "sk-should-not-leak") };
    let output = RunShell
        .execute(
            &ctx,
            json!({"command": "printf '%s' \"${CODER_TEST_SECRET:-missing}\""}),
        )
        .await
        .unwrap();
    unsafe { std::env::remove_var("CODER_TEST_SECRET") };

    assert!(output.output.contains("missing"));
    assert!(!output.output.contains("sk-should-not-leak"));
    assert_eq!(output.metadata.exit_code, Some(0));

    let failed = RunShell
        .execute(&ctx, json!({"command": "exit 3"}))
        .await
        .unwrap();
    assert_eq!(failed.metadata.exit_code, Some(3));
}
