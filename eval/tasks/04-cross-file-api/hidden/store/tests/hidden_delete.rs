use std::path::PathBuf;

use fixture_store::Store;

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("fixture-delete-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

#[test]
fn delete_reports_whether_a_key_existed() {
    let root = temp_root("store");
    let store = Store::new(&root);
    store.set("alpha", "one").unwrap();

    assert!(store.delete("alpha").unwrap());
    assert_eq!(store.get("alpha").unwrap(), None);
    assert!(!store.delete("alpha").unwrap());
    assert_eq!(store.list().unwrap(), Vec::<String>::new());
}

#[test]
fn delete_rejects_invalid_keys() {
    let root = temp_root("invalid");
    let store = Store::new(&root);
    for key in ["", "../escape", "a/b"] {
        let error = store.delete(key).expect_err("key should be rejected");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidInput,
            "key {key:?}"
        );
    }
}
