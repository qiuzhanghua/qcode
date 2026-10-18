//! Frame tests: render an [`App`] into `TestBackend` and assert what the
//! screen actually contains. No terminal or tty is involved.

use qcode_core::event::{
    ApprovalRequested, AssistantMessage, ContentBlock, Event, EventKind, SessionStarted, ToolCall,
    ToolResult, ToolStatus, TurnStarted, TurnStatus, Usage, UserMessage,
};
use qcode_tui::{App, render};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;

fn apply(app: &mut App, seq: u64, kind: EventKind) {
    app.apply(&Event::new(seq, "s1", 1, kind));
}

fn app_with_transcript() -> App {
    let mut app = App::new("mock", "mock-1");
    apply(
        &mut app,
        1,
        EventKind::SessionStarted(SessionStarted {
            model: "mock-1".to_string(),
            provider: "mock".to_string(),
            cwd: "/work/repo".to_string(),
            version: "0.1.0".to_string(),
        }),
    );
    apply(
        &mut app,
        2,
        EventKind::TurnStarted(TurnStarted {
            input: "fix tests".to_string(),
        }),
    );
    apply(
        &mut app,
        3,
        EventKind::UserMessage(UserMessage {
            text: "fix tests".to_string(),
            attachments: Vec::new(),
        }),
    );
    apply(
        &mut app,
        4,
        EventKind::AssistantMessage(AssistantMessage {
            blocks: vec![ContentBlock::Text {
                text: "running the suite".to_string(),
            }],
            provider_raw: json!({"provider": "mock"}),
            usage: Usage::default(),
        }),
    );
    apply(
        &mut app,
        5,
        EventKind::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "run_shell".to_string(),
            arguments: json!({"command": "cargo test"}),
        }),
    );
    apply(
        &mut app,
        6,
        EventKind::ToolResult(ToolResult {
            call_id: "c1".to_string(),
            status: ToolStatus::Ok,
            output: "test result: ok. 86 passed".to_string(),
            metadata: Default::default(),
        }),
    );
    app
}

fn draw(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| render(frame, app))
        .expect("draw frame");
    terminal.backend().to_string()
}

#[test]
fn renders_status_transcript_and_composer() {
    let app = app_with_transcript();
    let screen = draw(&app, 80, 24);

    assert!(
        screen.contains("mock:mock-1"),
        "status line missing:\n{screen}"
    );
    assert!(screen.contains("turn 1"), "turn counter missing:\n{screen}");
    assert!(
        screen.contains("fix tests"),
        "user message missing:\n{screen}"
    );
    assert!(
        screen.contains("running the suite"),
        "assistant text missing:\n{screen}"
    );
    assert!(screen.contains("run_shell"), "tool name missing:\n{screen}");
    assert!(
        screen.contains("cargo test"),
        "tool summary missing:\n{screen}"
    );
    assert!(
        screen.contains("[ok · ") && screen.contains("ms]"),
        "tool status or duration missing:\n{screen}"
    );
    assert!(screen.contains("message"), "composer missing:\n{screen}");
}

#[test]
fn collapsed_tool_hides_output_until_expanded() {
    let app = app_with_transcript();
    let screen = draw(&app, 80, 24);
    assert!(
        !screen.contains("86 passed"),
        "collapsed tool leaked its output:\n{screen}"
    );

    let mut expanded = app.clone();
    expanded.blocks.iter_mut().for_each(block_toggle);

    let screen = draw(&expanded, 80, 24);
    assert!(
        screen.contains("86 passed"),
        "expanded tool did not show output:\n{screen}"
    );
}

fn block_toggle(block: &mut qcode_tui::Block) {
    if let qcode_tui::Block::Tool(tool) = block {
        tool.expanded = true;
    }
}

#[test]
fn approval_overlay_shows_what_will_run() {
    let mut app = App::new("mock", "mock-1");
    apply(
        &mut app,
        1,
        EventKind::ToolCall(ToolCall {
            call_id: "c1".to_string(),
            name: "run_shell".to_string(),
            arguments: json!({"command": "cargo test"}),
        }),
    );
    apply(
        &mut app,
        2,
        EventKind::ApprovalRequested(ApprovalRequested {
            call_id: "c1".to_string(),
            rule_id: None,
            reason: "process execution".to_string(),
        }),
    );

    let screen = draw(&app, 80, 24);
    assert!(screen.contains("Approval"), "overlay missing:\n{screen}");
    assert!(screen.contains("run_shell"), "tool missing:\n{screen}");
    assert!(screen.contains("cargo test"), "summary missing:\n{screen}");
    assert!(
        screen.contains("process execution"),
        "reason missing:\n{screen}"
    );
    assert!(screen.contains("y] allow"), "allow hint missing:\n{screen}");
    assert!(screen.contains("n] deny"), "deny hint missing:\n{screen}");
    assert!(
        screen.contains("awaiting approval"),
        "status did not switch:\n{screen}"
    );
}

#[test]
fn completed_turn_shows_final_state_and_cost() {
    let mut app = app_with_transcript();
    apply(
        &mut app,
        7,
        EventKind::TurnEnded(qcode_core::event::TurnEnded {
            status: TurnStatus::Completed,
            usage: Usage::default(),
            cost_usd: 0.000_03,
        }),
    );

    let screen = draw(&app, 80, 24);
    assert!(screen.contains("completed"), "status missing:\n{screen}");
    assert!(screen.contains("$0.0000"), "cost missing:\n{screen}");
}

#[test]
fn first_ctrl_c_shows_the_quit_hint() {
    let mut app = App::new("mock", "mock-1");
    app.quit_armed = true;
    let screen = draw(&app, 80, 24);
    assert!(
        screen.contains("ctrl-c again to quit"),
        "quit hint missing:\n{screen}"
    );
}

#[test]
fn streaming_preview_is_rendered_with_a_cursor() {
    use qcode_core::engine::Delta;

    let mut app = App::new("mock", "mock-1");
    apply(
        &mut app,
        1,
        EventKind::TurnStarted(TurnStarted {
            input: "go".to_string(),
        }),
    );
    app.apply_delta(&Delta::Text("partial text".to_string()));

    let screen = draw(&app, 80, 24);
    assert!(
        screen.contains("partial text"),
        "streaming preview missing:\n{screen}"
    );
    assert!(screen.contains("▍"), "cursor missing:\n{screen}");
}

#[test]
fn page_keys_scroll_the_transcript_window() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = App::new("mock", "mock-1");
    for index in 0..60u64 {
        apply(
            &mut app,
            index + 1,
            EventKind::UserMessage(UserMessage {
                text: format!("line-{index:03}"),
                attachments: Vec::new(),
            }),
        );
    }

    let bottom = draw(&app, 40, 10);
    assert!(
        bottom.contains("line-059"),
        "newest line missing at the bottom:\n{bottom}"
    );
    assert!(
        !bottom.contains("line-000"),
        "window should start near the tail:\n{bottom}"
    );

    // The transcript area is six rows tall here; six pages cover all 60 lines.
    for _ in 0..6 {
        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    }
    let top = draw(&app, 40, 10);
    assert!(
        top.contains("line-000"),
        "PageUp did not reach the top:\n{top}"
    );
    assert!(
        !top.contains("line-059"),
        "old content should no longer be visible:\n{top}"
    );

    app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    let back = draw(&app, 40, 10);
    assert!(
        back.contains("line-059"),
        "End did not return to the tail:\n{back}"
    );
}

#[test]
fn scrolled_view_shows_the_offset_in_the_status_bar() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = App::new("mock", "mock-1");
    for index in 0..40u64 {
        apply(
            &mut app,
            index + 1,
            EventKind::UserMessage(UserMessage {
                text: format!("line-{index:03}"),
                attachments: Vec::new(),
            }),
        );
    }
    app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    let screen = draw(&app, 80, 10);
    assert!(screen.contains("↑10"), "scroll offset missing:\n{screen}");
}

#[test]
fn composer_grows_with_multiline_input() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let single = App::new("mock", "mock-1");

    let mut multi = App::new("mock", "mock-1");
    for character in ['a', 'b', 'c'] {
        multi.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        multi.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    }
    multi.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));

    let top_row = |screen: &str| {
        screen
            .lines()
            .position(|line| line.contains("message "))
            .expect("composer border")
    };

    let single_screen = draw(&single, 40, 12);
    let multi_screen = draw(&multi, 40, 12);
    assert!(
        top_row(&multi_screen) < top_row(&single_screen),
        "composer did not grow:\n{single_screen}\n---\n{multi_screen}"
    );
}
