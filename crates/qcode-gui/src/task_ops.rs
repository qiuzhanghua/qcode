//! 任务操作的纯决策逻辑，便于在集成测试里覆盖。

/// 运行中的活动任务是否要把重命名排队到 Turn 结束。
///
/// 跑 turn 时 `SessionWriter` 持有会话文件的排他锁（`engine.rs`），
/// `SessionStore::rename_task` 用非阻塞 `flock` 会直接返回 `Locked`。
/// 所以对正在跑的那个任务，先把标题放进 pending，等 `TurnFinished`
/// 锁释放后再落盘；其他任务不受影响。
pub fn should_queue_rename(turn_in_flight: bool, active: Option<&str>, task_id: &str) -> bool {
    turn_in_flight && active == Some(task_id)
}
