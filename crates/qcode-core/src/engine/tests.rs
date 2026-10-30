use super::*;

#[test]
fn identical_rounds_are_caught_but_different_ones_reset() {
    let mut guard = ProgressGuard::new();
    assert_eq!(guard.observe("a"), None);
    assert_eq!(guard.observe("a"), None);
    assert_eq!(
        guard.observe("a"),
        Some(3),
        "the third identical round stops"
    );

    let mut guard = ProgressGuard::new();
    assert_eq!(guard.observe("a"), None);
    assert_eq!(guard.observe("b"), None, "a different call is progress");
    assert_eq!(guard.observe("b"), None);
    assert_eq!(guard.observe("b"), Some(3));
}

#[test]
fn a_signature_tracks_the_tool_and_the_arguments() {
    let call = |name: &str, args: &str| ToolCallBlock {
        call_id: "c1".to_string(),
        name: name.to_string(),
        arguments: serde_json::from_str(args).expect("json"),
    };
    let shell_ls = call_signature(&[call("run_shell", r#"{"command":"ls"}"#)]);
    assert_eq!(
        shell_ls,
        call_signature(&[call("run_shell", r#"{"command":"ls"}"#)])
    );
    assert_ne!(
        shell_ls,
        call_signature(&[call("run_shell", r#"{"command":"ls -l"}"#)])
    );
    assert_ne!(
        shell_ls,
        call_signature(&[call("list_dir", r#"{"command":"ls"}"#)])
    );
}

#[test]
fn roots_prompt_lists_aliases_and_access() {
    use crate::task::{Root, RootAccess};

    assert!(roots_prompt(&[]).contains("No roots are configured"));
    let prompt = roots_prompt(&[
        Root {
            id: "a".into(),
            label: "code".into(),
            path: "/src".into(),
            access: RootAccess::ReadWrite,
            primary: true,
        },
        Root {
            id: "b".into(),
            label: "docs".into(),
            path: "/docs".into(),
            access: RootAccess::ReadOnly,
            primary: false,
        },
    ]);
    assert!(prompt.contains("@code"));
    assert!(prompt.contains("read-write"));
    assert!(prompt.contains("@docs"));
    assert!(prompt.contains("read-only"));
}

#[test]
fn agents_md_and_skills_are_appended_to_the_system_prompt() {
    assert_eq!(system_prompt(None, None, &[]), SYSTEM_PROMPT);
    assert_eq!(system_prompt(Some("   "), None, &[]), SYSTEM_PROMPT);

    let with_rules = system_prompt(Some("  use tabs  "), None, &[]);
    assert!(with_rules.starts_with(SYSTEM_PROMPT));
    assert!(with_rules.contains("# AGENTS.md"));
    assert!(with_rules.ends_with("use tabs"));

    let with_skills = system_prompt(Some("use tabs"), Some("# Skills\n\n- `x` (user): y"), &[]);
    assert!(with_skills.contains("# AGENTS.md"));
    assert!(with_skills.ends_with("- `x` (user): y"));
}

#[test]
fn todo_section_is_appended_after_skills() {
    let todos = vec![TodoItem {
        content: "write tests".to_string(),
        status: crate::todo::TodoStatus::InProgress,
    }];
    let prompt = system_prompt(
        Some("use tabs"),
        Some("# Skills\n\n- `x` (user): y"),
        &todos,
    );
    assert!(prompt.contains("# Skills"));
    assert!(prompt.contains("# Todo"));
    assert!(prompt.contains("[~] write tests"));
    assert!(prompt.ends_with("[~] write tests"));
}

#[test]
fn the_roots_prompt_marks_a_missing_root() {
    use crate::task::{Root, RootAccess};
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("gone");
    std::fs::create_dir_all(gone.as_path()).unwrap();
    let roots = vec![Root {
        id: "r1".to_string(),
        label: "docs".to_string(),
        path: gone.clone(),
        access: RootAccess::ReadWrite,
        primary: true,
    }];
    let before = roots_prompt(&roots);
    assert!(!before.contains("UNAVAILABLE"), "{before}");
    std::fs::remove_dir_all(&gone).unwrap();
    let after = roots_prompt(&roots);
    assert!(after.contains("UNAVAILABLE"), "{after}");
    assert!(after.contains("relink"), "{after}");
}
