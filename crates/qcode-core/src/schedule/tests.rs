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
        cron_text: "0 3 * * *".to_string(),
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

// ==== 运行时定时任务（Scheduled Tasks）====

fn task_with(
    rule: Rule,
    target: Target,
    status: TaskStatus,
    next: Option<PrimitiveDateTime>,
) -> ScheduledTask {
    ScheduledTask {
        id: "t1".to_string(),
        title: "nightly".to_string(),
        prompt: "review".to_string(),
        rule,
        target,
        overlap: Overlap::Queue,
        approval_timeout: None,
        status,
        stakeholders: Vec::new(),
        last_receipt: None,
        delivery_log: Vec::new(),
        earlier_records_pruned: false,
        reserved_session_id: None,
        next_fire_at: next,
        rearm_after: Duration::ZERO,
    }
}

#[test]
fn parse_rule_recognizes_all_five_kinds() {
    assert_eq!(
        parse_rule("cron 0 3 * * *"),
        Ok(Rule::Cron {
            expr: "0 3 * * *".to_string()
        })
    );
    assert_eq!(
        parse_rule("0 3 * * *"),
        Ok(Rule::Cron {
            expr: "0 3 * * *".to_string()
        })
    );
    assert_eq!(
        parse_rule("every 3600"),
        Ok(Rule::Interval { seconds: 3600 })
    );
    assert_eq!(
        parse_rule("daily 09:30"),
        Ok(Rule::Daily {
            hour: 9,
            minute: 30
        })
    );
    assert_eq!(
        parse_rule("weekly mon 09:30"),
        Ok(Rule::Weekly {
            weekday: 1,
            hour: 9,
            minute: 30
        })
    );
    assert_eq!(
        parse_rule("weekly 7 09:30"),
        Ok(Rule::Weekly {
            weekday: 0,
            hour: 9,
            minute: 30
        }),
        "7 folds onto Sunday (0)"
    );
    assert!(matches!(
        parse_rule("once 2026-11-10 09:30"),
        Ok(Rule::Once { .. })
    ));
}

#[test]
fn parse_rule_rejects_bad_input() {
    assert!(matches!(
        parse_rule("every 30"),
        Err(RuleError::Invalid { .. })
    ));
    assert!(matches!(
        parse_rule("daily 25:00"),
        Err(RuleError::Invalid { .. })
    ));
    assert!(matches!(
        parse_rule("cron 60 3 * * *"),
        Err(RuleError::Cron { .. })
    ));
    assert!(matches!(
        parse_rule("banana"),
        Err(RuleError::UnknownKind { .. })
    ));
}

#[test]
fn interval_daily_and_weekly_compute_the_next_occurrence() {
    // 2026-06-01 is a Monday.
    let now = at(2026, June, 1, 10, 0);
    assert_eq!(
        Rule::Interval { seconds: 3600 }.next_after(now),
        Some(at(2026, June, 1, 11, 0))
    );
    // 09:30 already passed today → tomorrow.
    assert_eq!(
        Rule::Daily {
            hour: 9,
            minute: 30
        }
        .next_after(now),
        Some(at(2026, June, 2, 9, 30))
    );
    assert_eq!(
        Rule::Daily {
            hour: 12,
            minute: 0
        }
        .next_after(now),
        Some(at(2026, June, 1, 12, 0))
    );
    // Weekly Monday 09:30 passed → next Monday.
    assert_eq!(
        Rule::Weekly {
            weekday: 1,
            hour: 9,
            minute: 30
        }
        .next_after(now),
        Some(at(2026, June, 8, 9, 30))
    );
    assert_eq!(
        Rule::Weekly {
            weekday: 1,
            hour: 12,
            minute: 0
        }
        .next_after(now),
        Some(at(2026, June, 1, 12, 0))
    );
    // A cron rule reuses CronSpec's own calendar.
    assert_eq!(
        Rule::Cron {
            expr: "0 3 * * *".to_string()
        }
        .next_after(now),
        Some(at(2026, June, 2, 3, 0))
    );
}

#[test]
fn once_rule_fires_once_then_ends() {
    let now = at(2026, June, 1, 10, 0);
    let future = Rule::Once {
        at: at(2026, June, 2, 9, 0),
    };
    assert_eq!(future.next_after(now), Some(at(2026, June, 2, 9, 0)));
    let past = Rule::Once {
        at: at(2026, June, 1, 8, 0),
    };
    assert_eq!(past.next_after(now), None);
    assert_eq!(advance_rule(&past, now), Advance::Ended);
    assert_eq!(
        advance_rule(&Rule::Interval { seconds: 60 }, now),
        Advance::Next(at(2026, June, 1, 10, 1))
    );
}

#[test]
fn task_tick_considers_status_target_and_overlap() {
    let now = at(2026, June, 1, 10, 0);
    let fire = task_with(
        Rule::Interval { seconds: 60 },
        Target::Session {
            id: "s1".to_string(),
        },
        TaskStatus::Active,
        Some(now),
    );
    assert_eq!(task_tick(&fire, now, false, true), TaskTick::Fire);
    // Paused never fires.
    let paused = task_with(
        Rule::Interval { seconds: 60 },
        Target::Session {
            id: "s1".to_string(),
        },
        TaskStatus::Paused,
        Some(now),
    );
    assert_eq!(task_tick(&paused, now, false, true), TaskTick::NotYet);
    // Window not yet come.
    let early = task_with(
        Rule::Interval { seconds: 60 },
        Target::Session {
            id: "s1".to_string(),
        },
        TaskStatus::Active,
        Some(at(2026, June, 1, 11, 0)),
    );
    assert_eq!(task_tick(&early, now, false, true), TaskTick::NotYet);
    // Session target gone → orphaned.
    assert_eq!(task_tick(&fire, now, false, false), TaskTick::Orphaned);
    // Busy + skip → skip; busy + queue → still fire.
    let mut skip = fire.clone();
    skip.overlap = Overlap::Skip;
    assert_eq!(task_tick(&skip, now, true, true), TaskTick::SkipBusy);
    assert_eq!(task_tick(&fire, now, true, true), TaskTick::Fire);
}

#[test]
fn delivery_log_is_pruned_to_the_retention_policy() {
    let now = at(2026, June, 1, 10, 0);
    let mut log = Vec::new();
    for day in 0..40 {
        let at = now - TimeSpan::days(day as i64);
        append_delivery(
            &mut log,
            DeliveryReceipt {
                occurrence_at: at,
                delivered_at: at,
                message_id: format!("m{day}"),
                target_session_id: None,
            },
            now,
        );
    }
    // 最近 30 天（含第 30 天边界）保留。
    assert_eq!(log.len(), 31);

    let mut log = Vec::new();
    let mut pruned = false;
    for index in 0..205 {
        pruned = append_delivery(
            &mut log,
            DeliveryReceipt {
                occurrence_at: now,
                delivered_at: now,
                message_id: format!("m{index}"),
                target_session_id: None,
            },
            now,
        );
    }
    assert_eq!(log.len(), DELIVERY_LOG_MAX_RECORDS);
    assert!(pruned);
}

#[test]
fn import_config_schedule_carries_state_and_target_over() {
    let entry = schedule(Overlap::Queue, Some("t1"));
    let imported = import_config_schedule(
        &entry,
        Some(&state(at(2026, June, 2, 3, 0))),
        at(2026, June, 1, 10, 0),
    );
    assert_eq!(imported.id, "nightly");
    assert_eq!(
        imported.rule,
        Rule::Cron {
            expr: "0 3 * * *".to_string()
        }
    );
    assert_eq!(
        imported.target,
        Target::Session {
            id: "t1".to_string()
        }
    );
    assert_eq!(imported.status, TaskStatus::Active);
    assert_eq!(imported.next_fire_at, Some(at(2026, June, 2, 3, 0)));

    // 无状态 → 按 now 计算下一个窗口。
    let fresh = import_config_schedule(&entry, None, at(2026, June, 1, 10, 0));
    assert_eq!(fresh.next_fire_at, Some(at(2026, June, 2, 3, 0)));
}

#[test]
fn target_status_and_overlap_round_trip_through_serde() {
    let task = task_with(
        Rule::Daily {
            hour: 9,
            minute: 30,
        },
        Target::NewOnce,
        TaskStatus::Active,
        Some(at(2026, June, 2, 9, 30)),
    );
    let encoded = serde_json::to_string(&task).expect("serialize");
    let decoded: ScheduledTask = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(decoded, task);
}

#[test]
fn local_offset_is_measured_not_hardcoded() {
    // The old Windows bug: local_offset() returned UTC unconditionally, so on a
    // +8 machine now_local() ran 8 hours behind and `daily 08:00` scheduled
    // "today 08:00" long after it had passed. now_local() must agree with the
    // wall clock at (UTC + local_offset), and the offset must sit within the
    // valid timezone bounds.
    let offset = local_offset();
    let shifted = time::OffsetDateTime::now_utc().to_offset(offset);
    let shifted_wall = time::PrimitiveDateTime::new(shifted.date(), shifted.time());
    let drift = (now_local() - shifted_wall).whole_seconds();
    assert!(
        drift.abs() <= 60,
        "now_local() drifts from local_offset(): {drift}s"
    );
    assert!(
        (-86_400..=86_400).contains(&offset.whole_seconds()),
        "local_offset() out of timezone bounds: {offset}"
    );
}
