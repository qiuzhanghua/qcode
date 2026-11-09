use super::*;
use crate::config::{Config, load};

fn entries(toml_text: &str) -> BTreeMap<String, ScheduleConfig> {
    toml::from_str(toml_text).expect("schedule table")
}

fn one(toml_text: &str) -> (Vec<Schedule>, Vec<ScheduleConfigError>) {
    resolve_schedules(&entries(toml_text))
}

const VALID: &str = r#"
[nightly-review]
cron = "0 3 * * *"
task = "01JABC"
prompt = "review the night's changes"
"#;

#[test]
fn a_valid_entry_resolves_with_the_documented_defaults() {
    let (schedules, errors) = one(VALID);
    assert_eq!(errors, Vec::new());
    assert_eq!(schedules.len(), 1);
    let schedule = &schedules[0];
    assert_eq!(schedule.name, "nightly-review");
    assert_eq!(schedule.task.as_deref(), Some("01JABC"));
    assert_eq!(schedule.prompt, "review the night's changes");
    assert_eq!(schedule.overlap, Overlap::Queue, "queue is the default");
    assert_eq!(schedule.approval_timeout, None, "0 = wait for a human");
    assert!(schedule.enabled, "entries are enabled unless switched off");
    assert!(!schedule.creates_task());
}

#[test]
fn new_task_per_fire_needs_no_task_id() {
    let (schedules, errors) = one(r#"
[morning]
cron = "30 8 * * mon-fri"
new = true
prompt = "summarize the inbox"
overlap = "skip"
"#);
    assert_eq!(errors, Vec::new());
    assert_eq!(schedules[0].task, None);
    assert!(schedules[0].creates_task());
    assert_eq!(schedules[0].overlap, Overlap::Skip);
}

#[test]
fn approval_timeout_understands_units_and_zero_means_forever() {
    let (schedules, errors) = one(r#"
[a]
cron = "0 3 * * *"
new = true
prompt = "p"
approval_timeout = "30m"

[b]
cron = "0 4 * * *"
new = true
prompt = "p"
approval_timeout = "0"

[c]
cron = "0 5 * * *"
new = true
prompt = "p"
approval_timeout = "90s"

[d]
cron = "0 6 * * *"
new = true
prompt = "p"
approval_timeout = "2h"
"#);
    assert_eq!(errors, Vec::new());
    let timeout = |name: &str| {
        schedules
            .iter()
            .find(|schedule| schedule.name == name)
            .and_then(|schedule| schedule.approval_timeout)
    };
    assert_eq!(timeout("a"), Some(Duration::from_secs(30 * 60)));
    assert_eq!(timeout("b"), None);
    assert_eq!(timeout("c"), Some(Duration::from_secs(90)));
    assert_eq!(timeout("d"), Some(Duration::from_secs(2 * 60 * 60)));
}

#[test]
fn a_disabled_entry_is_resolved_but_off() {
    let (schedules, errors) = one(r#"
[paused]
cron = "0 3 * * *"
new = true
prompt = "p"
enabled = false
"#);
    assert_eq!(errors, Vec::new());
    assert!(!schedules[0].enabled);
}

#[test]
fn a_bad_entry_is_reported_without_losing_the_others() {
    let (schedules, errors) = resolve_schedules(&entries(
        r#"
[good]
cron = "0 3 * * *"
new = true
prompt = "p"

[bad-cron]
cron = "0 99 * * *"
new = true
prompt = "p"

[empty-prompt]
cron = "0 3 * * *"
new = true
prompt = "   "

[bad-overlap]
cron = "0 3 * * *"
new = true
prompt = "p"
overlap = "later"

[bad-timeout]
cron = "0 3 * * *"
new = true
prompt = "p"
approval_timeout = "30 minutes"

[no-target]
cron = "0 3 * * *"
prompt = "p"

[both-targets]
cron = "0 3 * * *"
task = "01J"
new = true
prompt = "p"
"#,
    ));
    let names: Vec<&str> = schedules
        .iter()
        .map(|schedule| schedule.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["good"],
        "one bad entry must not drop the others"
    );
    let mut kinds: Vec<String> = errors.iter().map(|error| error.to_string()).collect();
    kinds.sort();
    assert_eq!(kinds.len(), 6, "{kinds:?}");
    assert!(kinds.iter().any(|kind| kind.contains("bad-cron")));
    assert!(
        kinds
            .iter()
            .any(|kind| kind.contains("prompt must not be empty"))
    );
    assert!(kinds.iter().any(|kind| kind.contains("`later`")));
    assert!(kinds.iter().any(|kind| kind.contains("30 minutes")));
    assert!(kinds.iter().any(|kind| kind.contains("not neither")));
    assert!(kinds.iter().any(|kind| kind.contains("mutually exclusive")));
}

#[test]
fn an_empty_section_is_no_schedules() {
    let (schedules, errors) = resolve_schedules(&BTreeMap::new());
    assert!(schedules.is_empty());
    assert!(errors.is_empty());
}

#[test]
fn a_config_without_the_table_still_loads_and_an_old_binary_ignores_it() {
    // The whole point of making the table optional (ADR-0059 §1): older files
    // need no migration, and a newer file's `[schedules]` is unknown to an older
    // build, which ignores it rather than failing.
    let config: Config = toml::from_str("version = 1").expect("minimal config");
    assert!(config.schedules.is_empty());

    let whole = VALID.replace("[nightly-review]", "[schedules.nightly-review]");
    let config: Config = toml::from_str(&whole).expect("config with schedules");
    assert_eq!(config.schedules.len(), 1);
    assert_eq!(
        config.schedules["nightly-review"].cron, "0 3 * * *",
        "the file spells the table as [schedules.<name>]"
    );
    assert!(
        !load::unknown_keys(&toml::from_str::<toml::Table>(VALID).expect("table"))
            .contains(&"schedules".to_string()),
        "the table must be a known key so no warning is printed"
    );
}
