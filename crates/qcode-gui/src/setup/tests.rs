use super::{extra_body_from_lines, stale_model_default};

#[test]
fn switching_provider_drops_the_mock_default() {
    assert_eq!(stale_model_default("deepseek", Some("mock-1".into())), None);
    assert_eq!(
        stale_model_default("mock", Some("mock-1".into())),
        Some("mock-1".into())
    );
    assert_eq!(
        stale_model_default("deepseek", Some("deepseek-flash".into())),
        Some("deepseek-flash".into())
    );
}

#[test]
fn extra_body_cell_parses_json_and_falls_back_to_strings() {
    let body = extra_body_from_lines(&[
        "reasoning_split = true".to_string(),
        "foo = bar".to_string(),
        "quoted = \"a string\"".to_string(),
        "= no-key".to_string(),
    ]);
    assert_eq!(body.get("reasoning_split"), Some(&serde_json::json!(true)));
    assert_eq!(body.get("foo"), Some(&serde_json::json!("bar")));
    assert_eq!(body.get("quoted"), Some(&serde_json::json!("a string")));
    assert!(!body.contains_key(""), "a line without a key is dropped");
    assert!(extra_body_from_lines(&[]).is_empty());
}
