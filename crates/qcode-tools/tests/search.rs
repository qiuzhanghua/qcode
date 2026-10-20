/// The tools print paths with the platform separator, so expectations do too.
fn p(path: &str) -> String {
    path.replace('/', std::path::MAIN_SEPARATOR_STR)
}

use qcode_core::tool::{Tool, ToolContext};
use qcode_tools::search::{GlobTool, GrepTool};
use serde_json::json;

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("target")).unwrap();
    std::fs::create_dir_all(dir.path().join("ignored")).unwrap();
    std::fs::write(dir.path().join(".gitignore"), "target/\nignored/\n").unwrap();
    std::fs::write(
        dir.path().join("src/main.rs"),
        "fn main() {\n    let needle = 1;\n}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn helper() {}\n").unwrap();
    std::fs::write(dir.path().join("target/generated.rs"), "needle\n").unwrap();
    std::fs::write(dir.path().join("ignored/skip.rs"), "needle\n").unwrap();
    std::fs::write(dir.path().join("src/blob.bin"), b"needle\0binary").unwrap();
    dir
}

fn context(dir: &tempfile::TempDir) -> ToolContext {
    ToolContext::new(dir.path())
}

#[tokio::test]
async fn glob_respects_gitignore_and_build_dirs() {
    let dir = workspace();
    let ctx = context(&dir);

    let output = GlobTool
        .execute(&ctx, json!({"pattern": "**/*.rs"}))
        .await
        .unwrap();

    assert!(output.output.contains(&p("src/main.rs")));
    assert!(output.output.contains(&p("src/lib.rs")));
    assert!(!output.output.contains(&p("target/generated.rs")));
    assert!(!output.output.contains(&p("ignored/skip.rs")));
}

#[tokio::test]
async fn glob_without_slash_matches_basenames() {
    let dir = workspace();
    let ctx = context(&dir);

    let output = GlobTool
        .execute(&ctx, json!({"pattern": "*.rs"}))
        .await
        .unwrap();
    assert!(output.output.contains(&p("src/main.rs")));
}

#[tokio::test]
async fn grep_reports_path_and_line() {
    let dir = workspace();
    let ctx = context(&dir);

    let output = GrepTool
        .execute(&ctx, json!({"pattern": "needle"}))
        .await
        .unwrap();

    assert!(output.output.contains(&p("src/main.rs:2:")));
    assert!(!output.output.contains(&p("target/generated.rs")));
    assert!(!output.output.contains(&p("ignored/skip.rs")));
    assert!(!output.output.contains(&p("blob.bin")));
}

#[tokio::test]
async fn grep_supports_context_lines() {
    let dir = workspace();
    let ctx = context(&dir);

    let output = GrepTool
        .execute(&ctx, json!({"pattern": "needle", "context": 1}))
        .await
        .unwrap();

    assert!(output.output.contains(&p("src/main.rs:2:")));
    assert!(output.output.contains(&p("src/main.rs-1-fn main()")));
    assert!(output.output.contains(&p("src/main.rs-3-}")));
}

#[tokio::test]
async fn grep_filters_by_glob() {
    let dir = workspace();
    let ctx = context(&dir);

    let output = GrepTool
        .execute(&ctx, json!({"pattern": "needle", "glob": "**/main.rs"}))
        .await
        .unwrap();

    assert!(output.output.contains(&p("src/main.rs:2:")));
    assert!(!output.output.contains(&p("lib.rs")));
}

#[tokio::test]
async fn grep_reports_no_matches() {
    let dir = workspace();
    let ctx = context(&dir);

    let output = GrepTool
        .execute(&ctx, json!({"pattern": "definitely-not-here"}))
        .await
        .unwrap();
    assert_eq!(output.output, "no matches");
}
