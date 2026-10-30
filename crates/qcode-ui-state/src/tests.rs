use qcode_core::event::{
    AssistantMessage, Event, EventKind, SessionStarted, ToolCall, ToolResult, TurnStarted, Usage,
    UserMessage,
};
use serde_json::json;

use super::*;

fn event(kind: EventKind) -> Event {
    Event::new(1, "s1", 1, kind)
}

/// Send one named key, the way a frontend would: text goes in as input
/// events, everything else as an action.
fn press(app: &mut App, name: &str) -> Option<UiAction> {
    match name {
        "enter" => app.handle_action(AppAction::Submit),
        "esc" => app.handle_action(AppAction::Escape),
        "up" => app.handle_action(AppAction::Up),
        "down" => app.handle_action(AppAction::Down),
        "page-up" | "ctrl-u" => app.handle_action(AppAction::PageUp),
        "page-down" | "ctrl-d" => app.handle_action(AppAction::PageDown),
        "ctrl-c" => app.handle_action(AppAction::Interrupt),
        "ctrl-n" | "shift-enter" | "alt-enter" => app.handle_action(AppAction::Newline),
        "ctrl-end" => app.handle_action(AppAction::ScrollToTail),
        "ctrl-o" => app.handle_action(AppAction::ExpandLastTool),
        "ctrl-t" => app.handle_action(AppAction::CycleContext),
        "ctrl-b" => app.handle_action(AppAction::ToggleChildGroup),
        "ctrl-v" => app.handle_action(AppAction::PasteImage),
        "ctrl-s" => app.handle_action(AppAction::Steer),
        other => {
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => app.handle_input(InputEvent::Insert(ch)),
                _ => None,
            }
        }
    }
}

fn type_text(app: &mut App, text: &str) {
    app.handle_input(InputEvent::InsertStr(text.to_string()));
}
fn demo_skill() -> SkillView {
    SkillView {
        name: "demo".to_string(),
        source: "config".to_string(),
        description: "demo skill".to_string(),
        dir: "/tmp/demo".to_string(),
        body: "Follow the steps.".to_string(),
        files: vec!["references/x.md".to_string()],
        files_truncated: false,
    }
}

#[test]
fn slash_skills_lists_and_shows_a_skill() {
    let mut app = App::new("mock", "mock-1");
    app.skills = vec![demo_skill()];

    type_text(&mut app, "/skills");
    assert_eq!(press(&mut app, "enter"), None);
    assert!(!app.status.running);
    assert!(matches!(
        app.blocks.last(),
        Some(Block::Notice(text))
            if text.contains("demo [config]") && text.contains("demo skill")
    ));

    type_text(&mut app, "/skills demo");
    assert_eq!(press(&mut app, "enter"), None);
    assert!(matches!(
        app.blocks.last(),
        Some(Block::Skill(text))
            if text.contains("# Skill: demo")
                && text.contains("references/x.md")
                && text.contains("Follow the steps.")
    ));
}

#[test]
fn slash_skills_reports_unknown_and_empty_cases() {
    let mut app = App::new("mock", "mock-1");
    type_text(&mut app, "/skills");
    press(&mut app, "enter");
    assert!(matches!(
        app.blocks.last(),
        Some(Block::Notice(text)) if text.contains("no skills discovered")
    ));

    app.skills = vec![demo_skill()];
    type_text(&mut app, "/skills nope");
    press(&mut app, "enter");
    assert!(matches!(
        app.blocks.last(),
        Some(Block::Notice(text)) if text.contains("unknown skill 'nope'")
    ));
}

#[test]
fn unknown_slash_command_is_not_sent_to_the_model() {
    let mut app = App::new("mock", "mock-1");
    type_text(&mut app, "/nope");
    assert_eq!(press(&mut app, "enter"), None);
    assert!(!app.status.running);
    assert!(matches!(
        app.blocks.last(),
        Some(Block::Notice(text)) if text.contains("unknown command '/nope'")
    ));
}

#[test]
fn slash_skill_name_activates_it_as_a_user_message() {
    let mut app = App::new("mock", "mock-1");
    app.skills = vec![demo_skill()];

    type_text(&mut app, "/demo 盘点本机");
    let action = press(&mut app, "enter");
    let Some(UiAction::Submit(text)) = action else {
        panic!("expected a submit action");
    };
    assert!(text.starts_with("[skill:demo]\n"));
    assert!(text.contains("Follow the steps."));
    assert!(text.ends_with("盘点本机"));
    assert!(app.status.running);

    app.apply(&event(EventKind::UserMessage(UserMessage {
        starts_segment: false,
        queued_id: None,
        text,
        attachments: Vec::new(),
    })));
    assert!(matches!(
        app.blocks.last(),
        Some(Block::Skill(text)) if text == "activated skill demo"
    ));
}

#[test]
fn slash_skill_activation_is_rejected_while_running() {
    let mut app = App::new("mock", "mock-1");
    app.skills = vec![demo_skill()];
    app.status.running = true;

    type_text(&mut app, "/demo");
    assert_eq!(press(&mut app, "enter"), None);
    assert!(matches!(
        app.blocks.last(),
        Some(Block::Notice(text)) if text.contains("already running")
    ));
}

#[test]
fn enter_submits_and_clears_the_composer() {
    let mut app = App::new("mock", "mock-1");
    for character in "fix tests".chars() {
        press(&mut app, &character.to_string());
    }
    let action = press(&mut app, "enter");
    assert_eq!(action, Some(UiAction::Submit("fix tests".to_string())));
    assert!(app.composer.is_empty());
    assert!(app.blocks.is_empty());
    assert!(app.status.running);
}

#[test]
fn esc_cancels_only_while_running() {
    let mut app = App::new("mock", "mock-1");
    assert_eq!(press(&mut app, "esc"), None);

    app.apply(&event(EventKind::TurnStarted(TurnStarted {
        input: "go".to_string(),
    })));
    assert_eq!(press(&mut app, "esc"), Some(UiAction::Cancel));
}

#[test]
fn replay_of_an_unfinished_turn_ends_idle() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::TurnStarted(TurnStarted {
        input: "go".to_string(),
    })));
    assert!(app.status.running, "live TurnStarted still runs");

    // 恢复会话时日志可能停在 TurnStarted，重放结束必须回到空闲。
    app.finish_replay();
    assert!(!app.status.running);
    // 并且把那个没有 TurnEnded 的 turn 封口，避免卡片永远显示 running。
    assert!(!app.turns[0].running());
    assert_eq!(app.turns[0].status, Some(TurnStatus::Cancelled));
}

#[test]
fn ctrl_c_cancels_then_quits() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::TurnStarted(TurnStarted {
        input: "go".to_string(),
    })));

    assert_eq!(press(&mut app, "ctrl-c"), Some(UiAction::Cancel));
    assert_eq!(press(&mut app, "ctrl-c"), Some(UiAction::Quit));
}

#[test]
fn approval_prompt_answers_with_y_or_n() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::ToolCall(ToolCall {
        call_id: "c1".to_string(),
        name: "run_shell".to_string(),
        arguments: json!({"command": "cargo test"}),
    })));
    app.apply(&event(EventKind::ApprovalRequested(
        qcode_core::event::ApprovalRequested {
            call_id: "c1".to_string(),
            rule_id: None,
            reason: "process execution".to_string(),
        },
    )));

    let prompt = app.approval.clone().expect("prompt");
    assert_eq!(prompt.tool, "run_shell");
    assert_eq!(prompt.summary, "cargo test");

    let action = press(&mut app, "y");
    assert_eq!(
        action,
        Some(UiAction::Approve {
            call_id: "c1".to_string(),
            allow: true,
            session: false,
        })
    );
    assert!(app.approval.is_none());
}

#[test]
fn a_answers_allow_for_the_session() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::ToolCall(ToolCall {
        call_id: "c1".to_string(),
        name: "run_shell".to_string(),
        arguments: json!({"command": "cargo test"}),
    })));
    app.apply(&event(EventKind::ApprovalRequested(
        qcode_core::event::ApprovalRequested {
            call_id: "c1".to_string(),
            rule_id: None,
            reason: "process execution".to_string(),
        },
    )));

    assert_eq!(
        press(&mut app, "a"),
        Some(UiAction::Approve {
            call_id: "c1".to_string(),
            allow: true,
            session: true,
        })
    );
    assert!(app.approval.is_none());
}

#[test]
fn composer_supports_cjk_and_shift_enter_newlines() {
    let mut app = App::new("mock", "mock-1");
    for character in "中文".chars() {
        press(&mut app, &character.to_string());
    }
    press(&mut app, "shift-enter");
    press(&mut app, "!");
    assert_eq!(app.composer.text(), "中文\n!");

    let action = press(&mut app, "enter");
    assert_eq!(action, Some(UiAction::Submit("中文\n!".to_string())));
    assert!(app.composer.is_empty());
}

#[test]
fn ctrl_n_and_modified_enter_insert_newlines() {
    let mut app = App::new("mock", "mock-1");
    press(&mut app, "a");
    press(&mut app, "ctrl-n");
    press(&mut app, "b");
    press(&mut app, "shift-enter");
    press(&mut app, "c");
    press(&mut app, "alt-enter");
    press(&mut app, "d");
    assert_eq!(app.composer.text(), "a\nb\nc\nd");

    let action = press(&mut app, "enter");
    assert_eq!(action, Some(UiAction::Submit("a\nb\nc\nd".to_string())));
}

#[test]
fn page_keys_move_the_scroll_offset() {
    let mut app = App::new("mock", "mock-1");
    press(&mut app, "page-up");
    press(&mut app, "page-up");
    assert_eq!(app.scroll, 20);

    press(&mut app, "page-down");
    assert_eq!(app.scroll, 10);

    press(&mut app, "ctrl-u");
    assert_eq!(app.scroll, 20);
    press(&mut app, "ctrl-d");
    assert_eq!(app.scroll, 10);

    press(&mut app, "x");
    assert_eq!(app.scroll, 0);
    assert_eq!(app.composer.text(), "x");
}

#[test]
fn arrows_scroll_when_the_draft_is_a_single_line() {
    let mut app = App::new("mock", "mock-1");
    press(&mut app, "up");
    press(&mut app, "up");
    assert_eq!(app.scroll, 20);
    press(&mut app, "down");
    assert_eq!(app.scroll, 10);
}

#[test]
fn arrows_edit_the_composer_when_the_draft_has_multiple_lines() {
    let mut app = App::new("mock", "mock-1");
    type_text(&mut app, "a");
    press(&mut app, "shift-enter");
    type_text(&mut app, "b");
    assert_eq!(app.composer.line_count(), 2);

    press(&mut app, "up");
    assert_eq!(app.scroll, 0, "a multi-line draft keeps the arrow");
}

#[test]
fn slash_exit_quits_in_three_spellings() {
    for command in ["/exit", "/quit", "/q"] {
        let mut app = App::new("mock", "mock-1");
        type_text(&mut app, command);
        assert_eq!(
            press(&mut app, "enter"),
            Some(UiAction::Quit),
            "{command} should quit"
        );
    }
}

#[test]
fn slash_checkpoint_and_rollback_are_local_actions() {
    let mut app = App::new("mock", "mock-1");
    type_text(&mut app, "/checkpoint");
    assert_eq!(press(&mut app, "enter"), Some(UiAction::Checkpoints));

    type_text(&mut app, "/rollback 3");
    assert_eq!(press(&mut app, "enter"), Some(UiAction::Rollback(3)));

    // A turn number is required; the UI explains instead of guessing.
    type_text(&mut app, "/rollback");
    assert_eq!(press(&mut app, "enter"), None);
    let notices: Vec<&str> = app
        .blocks
        .iter()
        .filter_map(|block| match block {
            Block::Notice(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("/rollback <turn>")),
        "{notices:?}"
    );
}

#[test]
fn ctrl_v_asks_the_loop_to_paste_an_image() {
    let mut app = App::new("mock", "mock-1");

    assert_eq!(press(&mut app, "ctrl-v"), Some(UiAction::PasteImage));
    // The UI has no clipboard access, so the key must not touch the composer.
    assert!(app.composer.text().is_empty());
}

#[test]
fn tool_result_updates_the_matching_row() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::ToolCall(ToolCall {
        call_id: "c1".to_string(),
        name: "run_shell".to_string(),
        arguments: json!({"command": "cargo test"}),
    })));
    app.apply(&event(EventKind::ToolResult(ToolResult {
        call_id: "c1".to_string(),
        status: ToolStatus::Ok,
        output: "ok".to_string(),
        metadata: Default::default(),
    })));

    match app.blocks.last() {
        Some(Block::Tool(tool)) => {
            assert_eq!(tool.status, Some(ToolStatus::Ok));
            assert_eq!(tool.output, "ok");
            assert!(!tool.expanded);
        }
        other => panic!("unexpected block: {other:?}"),
    }

    press(&mut app, "ctrl-o");
    assert!(matches!(app.blocks.last(), Some(Block::Tool(tool)) if tool.expanded));
}

#[test]
fn todo_write_result_becomes_a_todo_block() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::ToolCall(ToolCall {
        call_id: "c1".to_string(),
        name: "todo_write".to_string(),
        arguments: json!({"items": [
            {"content": "read spec", "status": "completed"},
            {"content": "build tool", "status": "in_progress"},
            {"content": "run tests"},
        ]}),
    })));
    app.apply(&event(EventKind::ToolResult(ToolResult {
        call_id: "c1".to_string(),
        status: ToolStatus::Ok,
        output: "Todo list (1/3 done):".to_string(),
        metadata: Default::default(),
    })));

    match app.blocks.last() {
        Some(Block::Todo(items)) => {
            assert_eq!(items.len(), 3);
            assert_eq!(items[0].status, qcode_core::todo::TodoStatus::Completed);
            assert_eq!(items[2].content, "run tests");
        }
        other => panic!("unexpected block: {other:?}"),
    }
}

#[test]
fn failed_todo_write_does_not_replace_the_list() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::ToolCall(ToolCall {
        call_id: "c1".to_string(),
        name: "todo_write".to_string(),
        arguments: json!({"items": [{"content": "ok"}]}),
    })));
    app.apply(&event(EventKind::ToolResult(ToolResult {
        call_id: "c1".to_string(),
        status: ToolStatus::Ok,
        output: "Todo list (0/1 done):".to_string(),
        metadata: Default::default(),
    })));
    app.apply(&event(EventKind::ToolCall(ToolCall {
        call_id: "c2".to_string(),
        name: "todo_write".to_string(),
        arguments: json!({"items": [{"content": ""}]}),
    })));
    app.apply(&event(EventKind::ToolResult(ToolResult {
        call_id: "c2".to_string(),
        status: ToolStatus::Error,
        output: "item 1 has empty `content`".to_string(),
        metadata: Default::default(),
    })));

    assert_eq!(
        app.blocks
            .iter()
            .filter(|block| matches!(block, Block::Todo(_)))
            .count(),
        1,
        "{:?}",
        app.blocks
    );
}

#[test]
fn empty_todo_write_clears_the_list_with_a_notice() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::ToolCall(ToolCall {
        call_id: "c1".to_string(),
        name: "todo_write".to_string(),
        arguments: json!({"items": []}),
    })));
    app.apply(&event(EventKind::ToolResult(ToolResult {
        call_id: "c1".to_string(),
        status: ToolStatus::Ok,
        output: "todo list cleared".to_string(),
        metadata: Default::default(),
    })));

    assert!(matches!(
        app.blocks.last(),
        Some(Block::Notice(text)) if text.contains("cleared")
    ));
}

#[test]
fn assistant_text_and_usage_reach_the_transcript() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::AssistantMessage(AssistantMessage {
        blocks: vec![
            ContentBlock::Reasoning {
                text: "thinking".to_string(),
                signature: None,
            },
            ContentBlock::Text {
                text: "done".to_string(),
            },
        ],
        provider_raw: json!({}),
        usage: Usage::default(),
    })));
    assert_eq!(app.blocks.len(), 2);
    assert!(matches!(
        &app.blocks[0],
        Block::Reasoning(entry) if entry.text == "thinking"
    ));
    assert_eq!(app.blocks[1], Block::Assistant("done".to_string()));
    assert_eq!(app.turns.len(), 1);
    assert_eq!(app.turns[0].process, vec![0]);
    assert_eq!(app.turns[0].answer_block, Some(1));
}

#[test]
fn turns_track_input_process_answer_and_usage() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&Event::new(
        1,
        "s1",
        1,
        EventKind::TurnStarted(TurnStarted {
            input: "go".to_string(),
        }),
    ));
    app.apply(&Event::new(
        2,
        "s1",
        1,
        EventKind::UserMessage(UserMessage {
            starts_segment: false,
            queued_id: None,
            text: "go".to_string(),
            attachments: Vec::new(),
        }),
    ));
    app.apply(&Event::new(
        3,
        "s1",
        1,
        EventKind::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: json!({"path": "src/lib.rs"}),
        }),
    ));
    app.apply(&Event::new(
        4,
        "s1",
        1,
        EventKind::AssistantMessage(AssistantMessage {
            blocks: vec![
                ContentBlock::Reasoning {
                    text: "look first".to_string(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "answer".to_string(),
                },
            ],
            provider_raw: json!({}),
            usage: Usage::default(),
        }),
    ));
    app.apply(&Event::new(
        5,
        "s1",
        1,
        EventKind::TurnEnded(qcode_core::event::TurnEnded {
            status: TurnStatus::Completed,
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(3),
                ..Usage::default()
            },
            cost_usd: 0.01,
        }),
    ));

    assert_eq!(app.turns.len(), 1);
    let turn = &app.turns[0];
    assert_eq!(turn.number, 1);
    assert_eq!(turn.status, Some(TurnStatus::Completed));
    assert!(turn.input_block.is_some());
    assert!(turn.answer_block.is_some());
    assert_eq!(turn.process.len(), 2);
    assert_eq!(turn.usage.output_tokens, Some(3));
    assert_eq!(turn.cost_usd, 0.01);
}

#[test]
fn steered_follow_up_message_becomes_its_own_turn() {
    let mut app = App::new("mock", "mock-1");
    // A 开场：TurnStarted + 输入 + 一个工具调用。
    app.apply(&Event::new(
        1,
        "s1",
        1,
        EventKind::TurnStarted(TurnStarted {
            input: "a".to_string(),
        }),
    ));
    app.apply(&Event::new(
        2,
        "s1",
        1,
        EventKind::UserMessage(UserMessage {
            starts_segment: false,
            queued_id: None,
            text: "A".to_string(),
            attachments: Vec::new(),
        }),
    ));
    app.apply(&Event::new(
        3,
        "s1",
        1,
        EventKind::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: json!({"path": "x"}),
        }),
    ));
    // 排队追加的 B 被推进运行中的 A：引擎不发出新的 TurnStarted，只在同
    // 一个 turn number 下追加 UserMessage，并用 starts_segment 标记边界。
    app.apply(&Event::new(
        4,
        "s1",
        1,
        EventKind::UserMessage(UserMessage {
            starts_segment: true,
            queued_id: Some("q1".to_string()),
            text: "B".to_string(),
            attachments: Vec::new(),
        }),
    ));
    app.apply(&Event::new(
        5,
        "s1",
        1,
        EventKind::ToolCall(ToolCall {
            call_id: "c2".to_string(),
            name: "read_file".to_string(),
            arguments: json!({"path": "y"}),
        }),
    ));
    // run 结束：A 和拆出的 B 一起收口。
    app.apply(&Event::new(
        6,
        "s1",
        1,
        EventKind::TurnEnded(qcode_core::event::TurnEnded {
            status: TurnStatus::Completed,
            usage: Usage::default(),
            cost_usd: 0.0,
        }),
    ));

    assert_eq!(app.turns.len(), 2, "A 与排队追加的 B 是两个独立 turn");
    let a = &app.turns[0];
    let b = &app.turns[1];
    assert_eq!(
        app.blocks[a.input_block.expect("A has input")],
        Block::User("A".to_string())
    );
    assert_eq!(
        app.blocks[b.input_block.expect("B has input")],
        Block::User("B".to_string())
    );
    assert_eq!(a.process.len(), 1, "A 的工具属于 A");
    assert_eq!(b.process.len(), 1, "B 的工具属于 B");
    assert!(!a.running(), "A 随 TurnEnded 收口");
    assert!(!b.running(), "B 随 TurnEnded 收口");
    assert_eq!(a.status, Some(TurnStatus::Completed));
    assert_eq!(b.status, Some(TurnStatus::Completed));
}

#[test]
fn v4_plain_user_message_does_not_split_a_turn() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&Event::new(
        1,
        "s1",
        1,
        EventKind::TurnStarted(TurnStarted {
            input: "a".to_string(),
        }),
    ));
    app.apply(&Event::new(
        2,
        "s1",
        1,
        EventKind::UserMessage(UserMessage {
            starts_segment: false,
            queued_id: None,
            text: "A".to_string(),
            attachments: Vec::new(),
        }),
    ));
    // v4 without the explicit marker: a second user message stays in the
    // same turn instead of being guessed into a new one.
    app.apply(&Event::new(
        3,
        "s1",
        1,
        EventKind::UserMessage(UserMessage {
            starts_segment: false,
            queued_id: None,
            text: "B".to_string(),
            attachments: Vec::new(),
        }),
    ));

    assert_eq!(app.turns.len(), 1);
    assert_eq!(
        app.blocks[app.turns[0].input_block.expect("input")],
        Block::User("A".to_string())
    );
}

#[test]
fn v3_log_without_the_marker_falls_back_to_the_heuristic() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&Event::new(
        1,
        "s1",
        1,
        EventKind::TurnStarted(TurnStarted {
            input: "a".to_string(),
        }),
    ));
    app.apply(&Event::new(
        2,
        "s1",
        1,
        EventKind::UserMessage(UserMessage {
            starts_segment: false,
            queued_id: None,
            text: "A".to_string(),
            attachments: Vec::new(),
        }),
    ));
    let mut older = Event::new(
        3,
        "s1",
        1,
        EventKind::UserMessage(UserMessage {
            starts_segment: false,
            queued_id: None,
            text: "B".to_string(),
            attachments: Vec::new(),
        }),
    );
    older.v = 3;
    app.apply(&older);

    assert_eq!(app.turns.len(), 2, "v3 日志仍用启发式拆分");
    assert_eq!(
        app.blocks[app.turns[1].input_block.expect("B has input")],
        Block::User("B".to_string())
    );
}

#[test]
fn turns_are_closed_and_indexed_independently() {
    let mut app = App::new("mock", "mock-1");
    for (turn, input, answer) in [(1u32, "first", "one"), (2u32, "second", "two")] {
        app.apply(&Event::new(
            u64::from(turn * 10),
            "s1",
            turn,
            EventKind::TurnStarted(TurnStarted {
                input: input.to_string(),
            }),
        ));
        app.apply(&Event::new(
            u64::from(turn * 10 + 1),
            "s1",
            turn,
            EventKind::UserMessage(UserMessage {
                starts_segment: false,
                queued_id: None,
                text: input.to_string(),
                attachments: Vec::new(),
            }),
        ));
        app.apply(&Event::new(
            u64::from(turn * 10 + 2),
            "s1",
            turn,
            EventKind::AssistantMessage(AssistantMessage {
                blocks: vec![ContentBlock::Text {
                    text: answer.to_string(),
                }],
                provider_raw: json!({}),
                usage: Usage::default(),
            }),
        ));
        app.apply(&Event::new(
            u64::from(turn * 10 + 3),
            "s1",
            turn,
            EventKind::TurnEnded(qcode_core::event::TurnEnded {
                status: TurnStatus::Completed,
                usage: Usage::default(),
                cost_usd: 0.0,
            }),
        ));
    }
    assert_eq!(app.turns.len(), 2);
    assert_eq!(app.turns[0].number, 1);
    assert_eq!(app.turns[1].number, 2);
    assert!(app.turns[0].duration().is_some());
    assert!(app.turns[1].duration().is_some());
    assert_eq!(app.turns[0].input_block, Some(0));
    assert_eq!(app.turns[1].input_block, Some(2));
    assert_eq!(app.turns[0].answer_block, Some(1));
    assert_eq!(app.turns[1].answer_block, Some(3));
}

#[test]
fn streamed_reasoning_keeps_its_place_before_later_tools() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::TurnStarted(TurnStarted {
        input: "go".to_string(),
    })));
    app.apply_delta(&Delta::Reasoning("look first".to_string()));
    app.apply(&Event::new(
        2,
        "s1",
        1,
        EventKind::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: json!({"path": "src/lib.rs"}),
        }),
    ));
    app.apply(&Event::new(
        3,
        "s1",
        1,
        EventKind::AssistantMessage(AssistantMessage {
            blocks: vec![
                ContentBlock::Reasoning {
                    text: "look first".to_string(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "done".to_string(),
                },
            ],
            provider_raw: json!({}),
            usage: Usage::default(),
        }),
    ));
    assert_eq!(app.turns[0].process.len(), 2);
    assert!(matches!(
        &app.blocks[app.turns[0].process[0]],
        Block::Reasoning(entry) if entry.text == "look first"
    ));
    assert!(matches!(
        &app.blocks[app.turns[0].process[1]],
        Block::Tool(tool) if tool.call_id == "c1"
    ));
}

#[test]
fn reasoning_delta_is_kept_until_the_authoritative_message() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::TurnStarted(TurnStarted {
        input: "go".to_string(),
    })));
    app.apply_delta(&Delta::Reasoning("step ".to_string()));
    app.apply_delta(&Delta::Reasoning("two".to_string()));
    assert_eq!(app.reasoning_streaming, "step two");

    app.apply(&event(EventKind::AssistantMessage(AssistantMessage {
        blocks: vec![
            ContentBlock::Reasoning {
                text: "step two".to_string(),
                signature: None,
            },
            ContentBlock::Text {
                text: "done".to_string(),
            },
        ],
        provider_raw: json!({}),
        usage: Usage::default(),
    })));
    assert!(app.reasoning_streaming.is_empty());
    assert!(matches!(
        &app.blocks[0],
        Block::Reasoning(entry) if entry.text == "step two"
    ));
}

#[test]
fn child_events_are_nested_and_do_not_move_parent_status() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::TurnStarted(TurnStarted {
        input: "go".to_string(),
    })));
    app.apply(&event(EventKind::ToolCall(ToolCall {
        call_id: "c1".to_string(),
        name: "subagent".to_string(),
        arguments: json!({"task": "investigate"}),
    })));

    let child = |seq: u64, kind: EventKind| Event::new(seq, "child-1", 1, kind);
    app.apply_child(&child(
        1,
        EventKind::SessionStarted(SessionStarted {
            model: "mock-1".to_string(),
            provider: "mock".to_string(),
            cwd: "/tmp".to_string(),
            version: "0.1.0".to_string(),
        }),
    ));
    app.apply_child(&child(
        2,
        EventKind::AssistantMessage(AssistantMessage {
            blocks: vec![ContentBlock::Text {
                text: "child thinking".to_string(),
            }],
            provider_raw: json!({}),
            usage: Usage::default(),
        }),
    ));
    // Same call_id as the parent: the session prefix must keep them apart.
    app.apply_child(&child(
        3,
        EventKind::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: json!({"path": "a.txt"}),
        }),
    ));
    app.apply_child(&child(
        4,
        EventKind::ToolResult(ToolResult {
            call_id: "c1".to_string(),
            status: ToolStatus::Ok,
            output: "contents".to_string(),
            metadata: Default::default(),
        }),
    ));
    app.apply_child(&child(
        5,
        EventKind::TurnEnded(qcode_core::event::TurnEnded {
            status: TurnStatus::Completed,
            usage: Usage::default(),
            cost_usd: 0.0,
        }),
    ));

    // Parent turn is still running and the parent tool row is untouched.
    assert!(app.status.running);
    let parent = app
        .blocks
        .iter()
        .find_map(|block| match block {
            Block::Tool(tool) if !tool.child && tool.call_id == "c1" => Some(tool),
            _ => None,
        })
        .expect("parent tool row");
    assert_eq!(parent.status, None);
    assert_eq!(parent.group.as_deref(), Some("child-1"));

    let child_tool = app
        .blocks
        .iter()
        .find_map(|block| match block {
            Block::Tool(tool) if tool.child && tool.name == "read_file" => Some(tool),
            _ => None,
        })
        .expect("child tool row");
    assert_eq!(child_tool.status, Some(ToolStatus::Ok));
    assert_eq!(child_tool.output, "contents");

    assert!(
        app.blocks
            .iter()
            .any(|block| matches!(block, Block::Subagent { text, .. } if text == "child thinking"))
    );
    assert!(
        app.blocks.iter().any(
            |block| matches!(block, Block::Subagent { text, .. } if text.contains("finished"))
        )
    );
    assert_eq!(app.child_groups, vec!["child-1"]);
    assert_eq!(app.child_block_count("child-1"), 4);
}

#[test]
fn context_deltas_update_the_status_fields() {
    let mut app = App::new("mock", "mock-1");
    assert_eq!(app.context_tokens, 0);
    assert_eq!(app.context_window, None);

    app.apply_delta(&Delta::Context {
        tokens: 15_360,
        window: 128_000,
    });
    assert_eq!(app.context_tokens, 15_360);
    assert_eq!(app.context_window, Some(128_000));
}

#[test]
fn ctrl_b_folds_the_newest_child_group() {
    let mut app = App::new("mock", "mock-1");
    assert!(app.child_groups.is_empty());

    app.apply(&event(EventKind::ToolCall(ToolCall {
        call_id: "p1".to_string(),
        name: "subagent".to_string(),
        arguments: json!({"task": "x"}),
    })));
    app.apply_child(&Event::new(
        1,
        "child-1",
        1,
        EventKind::SessionStarted(SessionStarted {
            model: "mock-1".to_string(),
            provider: "mock".to_string(),
            cwd: "/tmp".to_string(),
            version: "0.1.0".to_string(),
        }),
    ));
    assert_eq!(app.child_groups, vec!["child-1"]);
    assert!(!app.child_group_folded("child-1"));

    press(&mut app, "ctrl-b");
    assert!(app.child_group_folded("child-1"));
    press(&mut app, "ctrl-b");
    assert!(!app.child_group_folded("child-1"));
}

#[test]
fn subagents_command_lists_and_toggles_groups() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::ToolCall(ToolCall {
        call_id: "p1".to_string(),
        name: "subagent".to_string(),
        arguments: json!({"task": "x"}),
    })));
    app.apply_child(&Event::new(
        1,
        "child-1",
        1,
        EventKind::SessionStarted(SessionStarted {
            model: "mock-1".to_string(),
            provider: "mock".to_string(),
            cwd: "/tmp".to_string(),
            version: "0.1.0".to_string(),
        }),
    ));

    type_text(&mut app, "/subagents");
    assert_eq!(press(&mut app, "enter"), None);
    assert!(matches!(
        app.blocks.last(),
        Some(Block::Notice(text)) if text.contains("sub-agent groups (1)") && text.contains("shown")
    ));

    type_text(&mut app, "/subagents 1");
    assert_eq!(press(&mut app, "enter"), None);
    assert!(app.child_group_folded("child-1"));
}

#[test]
fn ctrl_t_cycles_the_context_display() {
    let mut app = App::new("mock", "mock-1");
    assert_eq!(app.context_display, ContextDisplay::Auto);

    press(&mut app, "ctrl-t");
    assert_eq!(app.context_display, ContextDisplay::Percent);
    press(&mut app, "ctrl-t");
    assert_eq!(app.context_display, ContextDisplay::Tokens);
    press(&mut app, "ctrl-t");
    assert_eq!(app.context_display, ContextDisplay::Auto);
}

#[test]
fn streamed_text_is_replaced_by_the_final_message() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::TurnStarted(TurnStarted {
        input: "go".to_string(),
    })));
    app.apply_delta(&Delta::Text("hel".to_string()));
    app.apply_delta(&Delta::Text("lo".to_string()));
    assert_eq!(app.streaming, "hello");

    app.apply(&event(EventKind::AssistantMessage(AssistantMessage {
        blocks: vec![ContentBlock::Text {
            text: "hello".to_string(),
        }],
        provider_raw: json!({}),
        usage: Usage::default(),
    })));
    assert!(app.streaming.is_empty());
    assert_eq!(app.blocks, vec![Block::Assistant("hello".to_string())]);
}

#[test]
fn turn_ended_updates_status_and_cost() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::TurnStarted(TurnStarted {
        input: "go".to_string(),
    })));
    app.apply(&event(EventKind::TurnEnded(qcode_core::event::TurnEnded {
        status: TurnStatus::Completed,
        usage: Usage::default(),
        cost_usd: 0.000_015,
    })));
    assert!(!app.status.running);
    assert_eq!(app.status.last, Some(TurnStatus::Completed));
    assert!((app.status.cost_usd - 0.000_015).abs() < f64::EPSILON);
}

#[test]
fn follow_up_while_running_is_queued() {
    let mut app = App::new("mock", "mock-1");
    app.status.running = true;
    type_text(&mut app, "next");
    let action = press(&mut app, "enter");

    assert_eq!(action, Some(UiAction::Queue("next".to_string())));
    assert!(app.composer.text().is_empty(), "the draft is consumed");
    assert!(app.status.running, "the running turn is unchanged");
}

#[test]
fn composer_state_tracks_pending_and_restoring() {
    use qcode_core::event::{ApprovalDecided, Decision, Scope};

    let mut app = App::new("mock", "mock-1");
    assert_eq!(app.composer_state(), ComposerState::Idle);

    app.apply(&event(EventKind::TurnStarted(TurnStarted {
        input: "hi".to_string(),
    })));
    assert_eq!(app.composer_state(), ComposerState::Running);

    app.approval = Some(ApprovalPrompt {
        call_id: "c1".to_string(),
        tool: "run_shell".to_string(),
        summary: "ls".to_string(),
        reason: "policy".to_string(),
        rule: None,
        argv: None,
        cwd: None,
    });
    assert_eq!(app.composer_state(), ComposerState::Pending);

    // Answering enters Restoring and locks the interaction's submissions
    // until the engine confirms with approval_decided.
    let answer = app.handle_action(AppAction::Approval(true, false));
    assert!(matches!(answer, Some(UiAction::Approve { .. })));
    assert_eq!(app.composer_state(), ComposerState::Restoring);

    type_text(&mut app, "too early");
    assert_eq!(press(&mut app, "enter"), None, "submit is locked");
    assert_eq!(press(&mut app, "ctrl-s"), None, "steer is locked");

    app.apply(&event(EventKind::ApprovalDecided(ApprovalDecided {
        call_id: "c1".to_string(),
        decision: Decision::Allow,
        rule_id: None,
        scope: Scope::Session,
    })));
    assert_eq!(app.composer_state(), ComposerState::Running);

    app.apply(&event(EventKind::TurnEnded(qcode_core::event::TurnEnded {
        status: TurnStatus::Completed,
        usage: Usage::default(),
        cost_usd: 0.0,
    })));
    assert_eq!(app.composer_state(), ComposerState::Idle);
}

#[test]
fn steer_on_send_turns_a_running_submit_into_steering() {
    let mut app = App::new("mock", "mock-1");
    app.steer_on_send = true;
    app.status.running = true;
    type_text(&mut app, "hurry");

    assert_eq!(
        press(&mut app, "enter"),
        Some(UiAction::Steer("hurry".to_string()))
    );
    assert!(app.composer.text().is_empty());
}

#[test]
fn steer_requires_a_running_turn_and_takes_the_draft() {
    let mut app = App::new("mock", "mock-1");
    type_text(&mut app, "hurry");
    assert_eq!(press(&mut app, "ctrl-s"), None, "idle: steer is refused");
    assert!(
        !app.composer.text().is_empty(),
        "the draft stays when steer is refused"
    );

    app.status.running = true;
    assert_eq!(
        press(&mut app, "ctrl-s"),
        Some(UiAction::Steer("hurry".to_string()))
    );
    assert!(app.composer.text().is_empty());
    assert!(app.status.running);
}

#[test]
fn queue_commands_map_to_frontend_actions() {
    let mut app = App::new("mock", "mock-1");

    type_text(&mut app, "/queue");
    assert_eq!(press(&mut app, "enter"), Some(UiAction::QueueList));

    type_text(&mut app, "/steer 2");
    assert_eq!(press(&mut app, "enter"), Some(UiAction::QueueSteer(2)));

    type_text(&mut app, "/drop 1");
    assert_eq!(press(&mut app, "enter"), Some(UiAction::QueueDrop(1)));

    type_text(&mut app, "/drop 0");
    assert_eq!(press(&mut app, "enter"), None);
    assert!(
        app.blocks
            .iter()
            .any(|block| matches!(block, Block::Notice(text) if text.contains("usage")))
    );
}

#[test]
fn steering_commands_map_to_frontend_actions() {
    let mut app = App::new("mock", "mock-1");

    type_text(&mut app, "/steering");
    assert_eq!(press(&mut app, "enter"), Some(UiAction::SteeringList));

    type_text(&mut app, "/unsteer 1");
    assert_eq!(press(&mut app, "enter"), Some(UiAction::SteeringUndo(1)));

    type_text(&mut app, "/unsteer x");
    assert_eq!(press(&mut app, "enter"), None);
    assert!(
        app.blocks
            .iter()
            .any(|block| matches!(block, Block::Notice(text) if text.contains("usage")))
    );
}

#[test]
fn submitted_text_is_rendered_once_the_event_arrives() {
    let mut app = App::new("mock", "mock-1");
    app.composer.apply(InputEvent::InsertStr("hi".to_string()));
    press(&mut app, "enter");
    app.apply(&event(EventKind::UserMessage(UserMessage {
        starts_segment: false,
        queued_id: None,
        text: "hi".to_string(),
        attachments: Vec::new(),
    })));
    assert_eq!(app.blocks, vec![Block::User("hi".to_string())]);
}

#[test]
fn toggle_tool_flips_only_the_requested_card() {
    let mut app = App::new("mock", "mock-1");
    app.apply(&event(EventKind::ToolCall(qcode_core::event::ToolCall {
        call_id: "c1".to_string(),
        name: "read_file".to_string(),
        arguments: serde_json::json!({"path": "a"}),
    })));
    app.apply(&event(EventKind::ToolCall(qcode_core::event::ToolCall {
        call_id: "c2".to_string(),
        name: "read_file".to_string(),
        arguments: serde_json::json!({"path": "b"}),
    })));
    app.toggle_tool("c1");
    let expanded: Vec<bool> = app
        .blocks
        .iter()
        .filter_map(|block| match block {
            Block::Tool(tool) => Some(tool.expanded),
            _ => None,
        })
        .collect();
    assert_eq!(expanded, [true, false]);
    app.toggle_tool("c1");
    assert!(matches!(
        &app.blocks[0],
        Block::Tool(tool) if !tool.expanded
    ));
}

#[test]
fn reset_transcript_keeps_configuration_and_clears_state() {
    let mut app = App::new("deepseek", "deepseek-flash");
    app.push_notice("old");
    app.status.running = true;
    app.status.turn = 3;
    app.status.cost_usd = 1.5;
    app.context_tokens = 42;
    app.child_groups.push("child".to_string());
    app.reset_transcript();
    assert!(app.blocks.is_empty());
    assert!(app.streaming.is_empty());
    assert!(!app.status.running);
    assert_eq!(app.status.turn, 0);
    assert_eq!(app.status.cost_usd, 0.0);
    assert_eq!(app.status.provider, "deepseek");
    assert_eq!(app.status.model, "deepseek-flash");
    assert_eq!(app.context_tokens, 0);
    assert!(app.child_groups.is_empty());
}
