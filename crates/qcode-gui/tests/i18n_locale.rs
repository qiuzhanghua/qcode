//! 一次 `set_locale` 是否同时翻转框架文案与我们的文案（ADR-0105 §2）。
//!
//! 这条测试是那个**既有 bug** 的回归防线：`qcode-gui` 以前从不设语言，于是
//! gpui-kit 一直停在默认 `en`，中文界面里的剪贴板菜单写着 `Copy`。
//!
//! 它证明的是机制（两边读同一个 `CURRENT_LOCALE`），不是像素。像素还得靠人在
//! 真机上开一次右键菜单确认 —— 这里能保证的是「机制不会再断」。
//!
//! 一个进程只能有一种语言，所以**两个断言必须在同一个测试里按顺序做**：
//! 拆成两个测试会因为并行与共享全局状态互相打死。

#[test]
fn one_set_locale_flips_both_the_framework_and_us() {
    // 1. 没设之前，框架读到的就是默认 `en` —— 这就是 bug 的现场，写下来免得
    //    以后有人以为它天然是中文。
    assert_eq!(
        &*gpui_component::locale(),
        "en",
        "rust-i18n 的初值应当是 en；若这里变了，说明有人在别处提前设了语言"
    );

    // 2. 我们设一次。
    qcode_i18n::pin_for_tests();
    assert_eq!(qcode_i18n::locale(), "zh-CN");

    // 3. 框架那边**跟着变**。这一条就是 ADR-0105 §2 的核心主张：gpui-kit 与我们
    //    用同一份 rust-i18n（lock 里只有 4.2.4 一份），所以一次设置两边生效。
    //    若哪天链进了第二个 rust-i18n 主版本，这里会先红。
    assert_eq!(
        &*gpui_component::locale(),
        "zh-CN",
        "框架没跟着变：多半是 rust-i18n 被链成了两份"
    );
}
