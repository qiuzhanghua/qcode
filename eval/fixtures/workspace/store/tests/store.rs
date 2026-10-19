use std::path::PathBuf;

use fixture_store::Store;

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("fixture-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

#[test]
fn set_get_list_roundtrip() {
    let store = Store::new(temp_root("roundtrip"));
    assert_eq!(store.get("alpha").unwrap(), None);

    store.set("beta", "two").unwrap();
    store.set("alpha", "one").unwrap();
    store.set("alpha", "one again").unwrap();

    assert_eq!(store.get("alpha").unwrap().as_deref(), Some("one again"));
    assert_eq!(store.get("beta").unwrap().as_deref(), Some("two"));
    assert_eq!(store.list().unwrap(), vec!["alpha", "beta"]);
}

#[test]
fn values_are_whitespace_normalized() {
    let store = Store::new(temp_root("normalize"));
    store.set("note", "  hello \t world\n\n").unwrap();
    assert_eq!(store.get("note").unwrap().as_deref(), Some("hello world"));
}

#[test]
fn unsafe_keys_are_rejected() {
    let store = Store::new(temp_root("unsafe"));
    for key in [
        "",
        "../escape",
        "a/b",
        ".hidden",
        "space key",
        &"x".repeat(65),
    ] {
        let error = store.set(key, "v").expect_err("key should be rejected");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidInput,
            "key {key:?}"
        );
        assert!(store.get(key).is_err(), "key {key:?}");
    }
    assert_eq!(store.list().unwrap(), Vec::<String>::new());
}
