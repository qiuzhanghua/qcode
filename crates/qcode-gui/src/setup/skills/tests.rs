//! The two pure decisions behind the 技能 switches (ADR-0110 §5/§6).
//!
//! They live in `skills.rs` outside `impl SetupView` precisely so they can be
//! tested here: the view methods need a `Window`, and this crate's headless
//! harness cannot be built on this machine (its `test-support` pulls a
//! `proptest ^1.12` the mirror does not carry). Splitting the decision out from
//! the `Context::notify` around it is what makes the behaviour checkable at all.

use super::{SkillSummary, stale_disabled, toggle_disabled};

fn row(key: &str, name: &str) -> SkillSummary {
    SkillSummary {
        key: key.to_string(),
        name: name.to_string(),
        source: "user".to_string(),
        description: String::new(),
        location: "/x".to_string(),
        files: Vec::new(),
        served: false,
        disabled: false,
    }
}

#[test]
fn switching_a_skill_off_appends_its_key() {
    let keys = toggle_disabled(&[], "pdf", false);
    assert_eq!(keys, ["pdf"]);
}

#[test]
fn switching_a_skill_on_removes_only_its_key() {
    let raw = ["pdf".to_string(), "docs".to_string()];
    let keys = toggle_disabled(&raw, "pdf", true);
    assert_eq!(keys, ["docs"], "the other switch keeps its state");
}

#[test]
fn switching_off_twice_does_not_duplicate_the_entry() {
    let raw = ["pdf".to_string()];
    let keys = toggle_disabled(&raw, "pdf", false);
    assert_eq!(keys, ["pdf"], "one skill, one entry — not two");
}

/// The config is hand-editable, so the same skill can arrive spelled with
/// whitespace. Comparing parsed forms is what keeps a stray duplicate out.
#[test]
fn an_untidy_entry_is_recognised_as_the_same_key() {
    let raw = ["  pdf  ".to_string()];
    let keys = toggle_disabled(&raw, "pdf", true);
    assert!(
        keys.is_empty(),
        "the padded entry is the same skill: {keys:?}"
    );
}

/// A blank entry means nothing, and the switch is not the place to fix it: the
/// key is dropped rather than silently deleted, so 「清理」 decides what to do
/// with an entry nobody can parse.
#[test]
fn a_blank_entry_is_kept_for_the_clean_up_button_to_decide() {
    let raw = ["   ".to_string(), "pdf".to_string()];
    let keys = toggle_disabled(&raw, "pdf", false);
    assert_eq!(
        keys,
        ["   ", "pdf"],
        "a switch flips one skill; it does not police the rest of the file"
    );
}

#[test]
fn a_scoped_key_only_touches_its_own_origin() {
    let raw = ["user:report".to_string(), "mcp:github:report".to_string()];
    let keys = toggle_disabled(&raw, "mcp:github:report", true);
    assert_eq!(keys, ["user:report"], "the local one stays off");
}

#[test]
fn an_entry_that_matches_a_row_is_not_stale() {
    let rows = [row("pdf", "pdf"), row("user:report", "report")];
    let raw = ["pdf".to_string(), "report".to_string()];
    assert!(
        stale_disabled(&raw, &rows).is_empty(),
        "a bare name matches the row with that name"
    );
}

#[test]
fn an_entry_that_matches_nothing_is_stale() {
    let rows = [row("pdf", "pdf")];
    let raw = ["pdf".to_string(), "ghost".to_string()];
    assert_eq!(
        stale_disabled(&raw, &rows),
        ["ghost"],
        "a renamed server or a typo has to stay visible to be fixable"
    );
}

#[test]
fn an_unparseable_entry_is_stale() {
    let rows = [row("pdf", "pdf")];
    let raw = ["   ".to_string()];
    assert_eq!(stale_disabled(&raw, &rows).len(), 1);
}

/// One row per skill means the qualified key is the only identity a switch can
/// write; a bare name would hit both origins.
#[test]
fn a_qualified_entry_does_not_match_a_different_origin() {
    let rows = [row("user:report", "report")];
    let raw = ["mcp:github:report".to_string()];
    assert_eq!(
        stale_disabled(&raw, &rows),
        ["mcp:github:report"],
        "the served one is gone; its entry is stale, the local row is another skill"
    );
}
