//! 让 `locales/` 参与增量编译判定。
//!
//! `rust_i18n::i18n!("locales", …)` 在**宏展开时**用 proc-macro 读这些 yml 并把
//! 译文嵌进二进制 —— 那是一次普通的文件读取，cargo 看不见，因此不会因为 yml 变了
//! 而重编这个 crate。
//!
//! 后果很隐蔽：只改 yml（不动任何 `.rs`）时，二进制里还是旧译文，界面上出现
//! `<locale>.<key>` 这种回退字符串，而 `cargo test` 里那些**在测试运行时读文件**的
//! 门禁照样通过。排查方向会被引向「key 拼错了」或「yml 结构错了」，实际只是没重编。
//!
//! 这条 `rerun-if-changed` 把 yml 纳入指纹，改了就重编。

fn main() {
    println!("cargo:rerun-if-changed=locales");
    // 新增/删除 yml 文件时，目录本身的 mtime 会变，但保险起见把已知文件名也列上。
    if let Ok(entries) = std::fs::read_dir("locales") {
        for entry in entries.flatten() {
            let path = entry.path();
            if matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("yml" | "yaml")
            ) {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}
