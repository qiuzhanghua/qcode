use super::*;

#[test]
fn identical_rounds_are_caught_but_different_ones_reset() {
    let mut guard = ProgressGuard::new();
    assert_eq!(guard.observe("a"), None);
    assert_eq!(guard.observe("a"), None);
    assert_eq!(guard.observe("a"), None);
    assert_eq!(guard.observe("a"), None);
    assert_eq!(guard.observe("a"), None);
    assert_eq!(guard.observe("a"), None);
    assert_eq!(guard.observe("a"), None);
    assert_eq!(guard.observe("a"), None);
    assert_eq!(guard.observe("a"), None);
    assert_eq!(
        guard.observe("a"),
        Some(10),
        "the tenth identical round stops"
    );

    let mut guard = ProgressGuard::new();
    assert_eq!(guard.observe("a"), None);
    assert_eq!(guard.observe("b"), None, "a different call is progress");
    assert_eq!(guard.observe("b"), None);
    assert_eq!(guard.observe("b"), None);
    assert_eq!(guard.observe("b"), None);
    assert_eq!(guard.observe("b"), None);
    assert_eq!(guard.observe("b"), None);
    assert_eq!(guard.observe("b"), None);
    assert_eq!(guard.observe("b"), None);
    assert_eq!(guard.observe("b"), None);
    assert_eq!(guard.observe("b"), None);
    assert_eq!(guard.observe("b"), Some(10));
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
    assert_eq!(system_prompt(None, None, &[], &[]), SYSTEM_PROMPT);
    assert_eq!(system_prompt(Some("   "), None, &[], &[]), SYSTEM_PROMPT);

    let with_rules = system_prompt(Some("  use tabs  "), None, &[], &[]);
    assert!(with_rules.starts_with(SYSTEM_PROMPT));
    assert!(with_rules.contains("# AGENTS.md"));
    assert!(with_rules.ends_with("use tabs"));

    let with_skills = system_prompt(
        Some("use tabs"),
        Some("# Skills\n\n- `x` (user): y"),
        &[],
        &[],
    );
    assert!(with_skills.contains("# AGENTS.md"));
    assert!(with_skills.ends_with("- `x` (user): y"));
}

/// ADR-0068 §3.1: 8 KiB across all servers, truncation noted, and the note is
/// charged to the same budget so the returned string never exceeds it.
#[test]
fn mcp_instructions_are_capped_at_the_budget() {
    let big = crate::tool::ServerPrompt {
        server: "big".to_string(),
        text: "x".repeat(40_000),
    };
    let rendered = mcp_prompt(&[big]);
    assert!(
        rendered.len() <= MCP_INSTRUCTIONS_BUDGET,
        "{} bytes exceeds the {} budget",
        rendered.len(),
        MCP_INSTRUCTIONS_BUDGET
    );
    assert!(rendered.contains("# MCP: big"));
    assert!(rendered.contains("truncated"));

    // Under the budget: verbatim, no note.
    let small = crate::tool::ServerPrompt {
        server: "small".to_string(),
        text: "Use `echo` first.".to_string(),
    };
    let rendered = mcp_prompt(&[small]);
    assert!(rendered.contains("Use `echo` first."));
    assert!(!rendered.contains("truncated"));
}

/// The budget is shared, and a server that does not fit is skipped rather than
/// half-rendered: an empty heading would cost tokens and say nothing.
#[test]
fn the_budget_is_shared_across_servers() {
    let prompts: Vec<crate::tool::ServerPrompt> = (0..4)
        .map(|index| crate::tool::ServerPrompt {
            server: format!("s{index}"),
            // Each takes a quarter of the budget minus what the heading eats.
            text: "y".repeat(2_000),
        })
        .collect();
    let rendered = mcp_prompt(&prompts);
    assert!(
        rendered.len() <= MCP_INSTRUCTIONS_BUDGET,
        "{}",
        rendered.len()
    );
    assert!(rendered.contains("# MCP: s0"), "the first always fits");
    // Only whole sections: every heading that appears is followed by text.
    for index in 0..4 {
        if let Some(rest) = rendered.split(&format!("# MCP: s{index}")).nth(1) {
            assert!(
                !rest.trim().is_empty(),
                "s{index} got a heading with nothing under it"
            );
        }
    }
}

/// A multi-byte character split in half would be a corrupt prompt.
#[test]
fn truncation_lands_on_a_character_boundary() {
    let prompt = crate::tool::ServerPrompt {
        server: "wide".to_string(),
        // Three bytes per char, so a naive byte cut lands mid-character.
        text: "工".repeat(MCP_INSTRUCTIONS_BUDGET),
    };
    let rendered = mcp_prompt(&[prompt]);
    assert!(rendered.len() <= MCP_INSTRUCTIONS_BUDGET);
    // `String` is already valid UTF-8 by construction; what this pins down is
    // that the cut did not silently drop the whole section.
    assert!(rendered.contains("# MCP: wide"));
    assert!(rendered.contains('工'));
    assert!(rendered.contains("truncated"));
}

/// Empty and whitespace-only instructions produce nothing at all.
#[test]
fn blank_instructions_produce_no_section() {
    let prompts = vec![
        crate::tool::ServerPrompt {
            server: "empty".to_string(),
            text: String::new(),
        },
        crate::tool::ServerPrompt {
            server: "blank".to_string(),
            text: "   \n\t ".to_string(),
        },
    ];
    assert_eq!(mcp_prompt(&prompts), "");
}

/// The section sits after the skills index and before the todo list: project
/// rules first, third-party text next, live session state last.
#[test]
fn mcp_sections_sit_between_skills_and_todos() {
    let todos = vec![TodoItem {
        content: "write tests".to_string(),
        status: crate::todo::TodoStatus::InProgress,
    }];
    let prompt = system_prompt(
        Some("use tabs"),
        Some("# Skills\n\n- `x` (user): y"),
        &[crate::tool::ServerPrompt {
            server: "stub".to_string(),
            text: "Handshake prose.".to_string(),
        }],
        &todos,
    );
    let skills_at = prompt.find("# Skills").expect("skills");
    let mcp_at = prompt.find("# MCP: stub").expect("mcp");
    let todo_at = prompt.find("# Todo").expect("todo");
    assert!(skills_at < mcp_at, "{prompt}");
    assert!(mcp_at < todo_at, "{prompt}");
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
        &[],
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
