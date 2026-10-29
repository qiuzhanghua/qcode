//! 运行中改名排队的决策逻辑。

use qcode_gui::task_ops::should_queue_rename;

#[test]
fn running_active_task_queues_the_rename() {
    assert!(should_queue_rename(true, Some("t1"), "t1"));
}

#[test]
fn idle_or_other_task_renames_immediately() {
    assert!(!should_queue_rename(false, Some("t1"), "t1"));
    assert!(!should_queue_rename(true, Some("t1"), "t2"));
    assert!(!should_queue_rename(true, None, "t1"));
}
