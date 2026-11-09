use super::*;

use time::Month::{December, February, January, July, June, March};

fn parses(expr: &str) -> CronSpec {
    CronSpec::parse(expr).expect(expr)
}

#[test]
fn the_five_fields_are_minute_hour_day_month_weekday() {
    let every_minute = parses("* * * * *");
    assert!(every_minute.matches(at(2026, June, 1, 3, 7)));
    assert!(every_minute.matches(at(2026, June, 1, 0, 0)));

    let nightly = parses("0 3 * * *");
    assert!(nightly.matches(at(2026, June, 1, 3, 0)));
    assert!(!nightly.matches(at(2026, June, 1, 3, 1)));
    assert!(!nightly.matches(at(2026, June, 1, 4, 0)));
}

#[test]
fn field_lists_ranges_and_steps_work() {
    let spec = parses("*/15 9-17 * * *");
    for minute in [0, 15, 30, 45] {
        assert!(
            spec.matches(at(2026, June, 1, 9, minute)),
            "minute {minute}"
        );
    }
    assert!(!spec.matches(at(2026, June, 1, 9, 10)));
    assert!(!spec.matches(at(2026, June, 1, 18, 0)));
    assert!(spec.matches(at(2026, June, 1, 17, 45)));

    let weekdays_only = parses("30 8 * * mon-fri");
    assert!(weekdays_only.matches(at(2026, June, 1, 8, 30))); // Monday
    assert!(!weekdays_only.matches(at(2026, June, 6, 8, 30))); // Saturday
}

#[test]
fn names_and_sunday_as_seven_are_accepted() {
    let monthly = parses("0 0 1 jan,jul *");
    assert!(monthly.matches(at(2026, January, 1, 0, 0)));
    assert!(monthly.matches(at(2026, July, 1, 0, 0)));
    assert!(!monthly.matches(at(2026, June, 1, 0, 0)));
    assert!(!monthly.matches(at(2026, March, 1, 0, 0)));

    // 2026-06-07 is a Sunday.
    assert!(parses("0 9 * * 0").matches(at(2026, June, 7, 9, 0)));
    assert!(parses("0 9 * * 7").matches(at(2026, June, 7, 9, 0)));
    assert!(parses("0 9 * * sun").matches(at(2026, June, 7, 9, 0)));
}

#[test]
fn day_of_month_and_day_of_week_are_ored_when_both_are_given() {
    // crontab(5): "the 1st, or any Monday".
    let spec = parses("0 3 1 * mon");
    assert!(spec.matches(at(2026, June, 1, 3, 0))); // both
    assert!(spec.matches(at(2026, June, 8, 3, 0))); // Monday only
    assert!(spec.matches(at(2026, July, 1, 3, 0))); // the 1st only
    assert!(!spec.matches(at(2026, June, 2, 3, 0)));
}

#[test]
fn a_starred_field_does_not_count_as_a_restriction() {
    // "the 1st" and "every Monday" stay independent when the other is `*`.
    assert!(parses("0 3 1 * *").matches(at(2026, July, 1, 3, 0)));
    assert!(!parses("0 3 1 * *").matches(at(2026, July, 6, 3, 0)));
    assert!(parses("0 3 * * mon").matches(at(2026, July, 6, 3, 0)));
    assert!(!parses("0 3 * * mon").matches(at(2026, July, 7, 3, 0)));
}

#[test]
fn bad_expressions_are_rejected_with_a_reason() {
    assert_eq!(CronSpec::parse("0 3 * *"), Err(CronError::FieldCount(4)));
    assert!(matches!(
        CronSpec::parse("60 3 * * *"),
        Err(CronError::Range { .. })
    ));
    assert!(matches!(
        CronSpec::parse("0 3 * * funday"),
        Err(CronError::Field { .. })
    ));
    assert!(matches!(
        CronSpec::parse("*/0 3 * * *"),
        Err(CronError::Field { .. })
    ));
    assert!(matches!(
        CronSpec::parse("5-1 3 * * *"),
        Err(CronError::Field { .. })
    ));
}

#[test]
fn next_after_is_strictly_after_and_minute_aligned() {
    let nightly = parses("0 3 * * *");
    let from = at(2026, June, 1, 3, 0);
    assert_eq!(
        nightly.next_after(from),
        Some(at(2026, June, 2, 3, 0)),
        "firing at 03:00 must schedule the *next* 03:00, not the same one"
    );
    assert_eq!(
        parses("* * * * *").next_after(from),
        Some(at(2026, June, 1, 3, 1))
    );
}

#[test]
fn next_after_crosses_months_and_years() {
    let quarterly = parses("0 0 1 jan,apr,jul,oct *");
    assert_eq!(
        quarterly.next_after(at(2026, June, 15, 12, 0)),
        Some(at(2026, July, 1, 0, 0))
    );
    assert_eq!(
        quarterly.next_after(at(2026, December, 31, 23, 59)),
        Some(at(2027, January, 1, 0, 0))
    );
}

#[test]
fn an_expression_that_can_never_fire_returns_none() {
    // February 30th.
    assert_eq!(
        parses("0 0 30 2 *").next_after(at(2026, June, 1, 0, 0)),
        None
    );
    // A leap day does fire, within the four-year scan.
    assert_eq!(
        parses("0 0 29 2 *").next_after(at(2026, June, 1, 0, 0)),
        Some(at(2028, February, 29, 0, 0))
    );
}

fn schedule(overlap: Overlap, task: Option<&str>) -> Schedule {
    Schedule {
        name: "nightly".to_string(),
        cron: parses("0 3 * * *"),
        task: task.map(str::to_string),
        prompt: "review the night's changes".to_string(),
        overlap,
        approval_timeout: None,
        enabled: true,
    }
}

fn state(next: PrimitiveDateTime) -> ScheduleState {
    ScheduleState {
        next_fire_at: Some(next),
        ..ScheduleState::default()
    }
}

#[test]
fn an_entry_with_no_next_window_only_gets_one_scheduled() {
    let entry = schedule(Overlap::Queue, None);
    assert_eq!(
        tick(
            &entry,
            &ScheduleState::default(),
            at(2026, June, 1, 3, 0),
            false,
            true
        ),
        Tick::NotYet,
        "starting the process must not fire every entry at once"
    );
    assert_eq!(
        recover(&entry, &ScheduleState::default(), at(2026, June, 1, 3, 0)),
        Recovery {
            missed: 0,
            next_fire_at: Some(at(2026, June, 2, 3, 0)),
        }
    );
}

#[test]
fn the_window_fires_when_it_comes() {
    let entry = schedule(Overlap::Queue, Some("t1"));
    let now = at(2026, June, 2, 3, 0);
    assert_eq!(tick(&entry, &state(now), now, false, true), Tick::Fire);
    assert_eq!(
        tick(&entry, &state(now), at(2026, June, 2, 2, 59), false, true),
        Tick::NotYet
    );
}

#[test]
fn a_busy_session_queues_or_skips_as_configured() {
    let now = at(2026, June, 2, 3, 0);
    assert_eq!(
        tick(
            &schedule(Overlap::Queue, Some("t1")),
            &state(now),
            now,
            true,
            true
        ),
        Tick::Fire,
        "queue means submit and let SessionQueue hold it"
    );
    assert_eq!(
        tick(
            &schedule(Overlap::Skip, Some("t1")),
            &state(now),
            now,
            true,
            true
        ),
        Tick::SkipBusy
    );
    assert_eq!(
        tick(
            &schedule(Overlap::Skip, Some("t1")),
            &state(now),
            now,
            false,
            true
        ),
        Tick::Fire
    );
}

#[test]
fn a_disabled_entry_never_fires_and_a_missing_task_is_orphaned() {
    let now = at(2026, June, 2, 3, 0);
    let mut disabled = schedule(Overlap::Queue, Some("t1"));
    disabled.enabled = false;
    assert_eq!(tick(&disabled, &state(now), now, false, true), Tick::NotYet);

    assert_eq!(
        tick(
            &schedule(Overlap::Queue, Some("gone")),
            &state(now),
            now,
            false,
            false
        ),
        Tick::Orphaned,
        "a deleted task must be reported, not silently skipped"
    );
    // A fresh task per fire has nothing to be orphaned.
    assert_eq!(
        tick(
            &schedule(Overlap::Queue, None),
            &state(now),
            now,
            false,
            false
        ),
        Tick::Fire
    );
}

#[test]
fn windows_that_passed_while_asleep_are_counted_not_replayed() {
    let entry = schedule(Overlap::Queue, None);
    // Next window was 03:00 on the 1st; the process wakes at 09:00 on the 2nd.
    let loaded = state(at(2026, June, 1, 3, 0));
    let recovery = recover(&entry, &loaded, at(2026, June, 2, 9, 0));
    assert_eq!(recovery.missed, 2, "the 1st and the 2nd both came and went");
    assert_eq!(recovery.next_fire_at, Some(at(2026, June, 3, 3, 0)));
}

#[test]
fn recovery_keeps_an_existing_future_window() {
    let entry = schedule(Overlap::Queue, None);
    let future = at(2026, June, 2, 3, 0);
    assert_eq!(
        recover(&entry, &state(future), at(2026, June, 1, 12, 0)),
        Recovery {
            missed: 0,
            next_fire_at: Some(future)
        }
    );
}

#[test]
fn advancing_records_the_decision_and_the_next_window() {
    let entry = schedule(Overlap::Queue, None);
    let now = at(2026, June, 2, 3, 0);
    let next = advance(&entry, now, ScheduleStatus::Skipped);
    assert_eq!(next.last_status, Some(ScheduleStatus::Skipped));
    assert_eq!(next.last_fired_at, Some(now));
    assert_eq!(next.next_fire_at, Some(at(2026, June, 3, 3, 0)));
    assert_eq!(next.missed_windows, 0);
}

#[test]
fn statuses_round_trip_through_the_store_encoding() {
    for status in [
        ScheduleStatus::Fired,
        ScheduleStatus::Skipped,
        ScheduleStatus::Orphaned,
        ScheduleStatus::Missed(3),
        ScheduleStatus::Failed("no session".to_string()),
    ] {
        assert_eq!(ScheduleStatus::parse(&status.as_str()), Some(status));
    }
    assert_eq!(ScheduleStatus::parse("nonsense"), None);
}

#[test]
fn overlap_parsing_matches_the_config_spelling() {
    assert_eq!(Overlap::parse("queue"), Some(Overlap::Queue));
    assert_eq!(Overlap::parse("skip"), Some(Overlap::Skip));
    assert_eq!(Overlap::parse("later"), None);
    assert_eq!(Overlap::default(), Overlap::Queue);
}
