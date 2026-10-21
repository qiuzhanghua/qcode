use qcode_gui::approvals::ApprovalQueue;
use qcode_ui_bridge::ApprovalView;

fn view(id: &str) -> ApprovalView {
    ApprovalView {
        call_id: id.to_string(),
        tool: "run_shell".to_string(),
        summary: "echo hi".to_string(),
        reason: "ask".to_string(),
        rule_id: None,
        argv: Some("echo hi".to_string()),
        cwd: Some("/w".to_string()),
    }
}

#[test]
fn prompts_are_shown_and_answered_in_arrival_order() {
    let mut queue = ApprovalQueue::default();
    queue.push(view("a"));
    queue.push(view("b"));
    queue.push(view("a"));
    assert_eq!(queue.len(), 2);
    assert_eq!(queue.current().expect("first").call_id, "a");
    assert_eq!(queue.take_current().expect("answered").call_id, "a");
    assert_eq!(queue.current().expect("second").call_id, "b");
}

#[test]
fn a_cancelled_prompt_disappears_without_blocking_the_next() {
    let mut queue = ApprovalQueue::default();
    queue.push(view("a"));
    queue.push(view("b"));
    queue.closed("a");
    assert_eq!(queue.current().expect("next").call_id, "b");
    queue.closed("b");
    assert!(queue.is_empty());
}
