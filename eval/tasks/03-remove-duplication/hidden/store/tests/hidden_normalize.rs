use fixture_store::Store;

#[test]
fn unicode_whitespace_is_still_normalized_after_the_refactor() {
    let root = std::env::temp_dir().join(format!("fixture-normalize-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = Store::new(root);

    store
        .set("note", " \u{00a0}alpha\tbeta \n gamma  ")
        .unwrap();
    assert_eq!(
        store.get("note").unwrap().as_deref(),
        Some("alpha beta gamma")
    );
}
