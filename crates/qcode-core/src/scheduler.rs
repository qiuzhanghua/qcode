//! 定时任务管理服务（运行时任务，区别于 config 只读声明）。
//!
//! 只负责"任务定义"的管理与校验：创建 / 编辑 / 启停 / 立即执行 / 删除 / 列表 /
//! 查看 / 迁移。投递本身由引擎侧的 `ScheduleDriver` 负责（见 `qcode-gui::runtime`），
//! 这里不读时钟——`now` 由调用方传入，方便用假时钟测试。
//!
//! 任务定义落在 `SessionStore` 的 `schedule/tasks.json`（见 `store.rs`）。

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use time::PrimitiveDateTime;

use crate::schedule::{
    Rule, Schedule, ScheduleState, ScheduledTask, Target, TaskStatus, import_config_schedule,
};
use crate::store::{SessionStore, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("schedule task: {0}")]
    Invalid(String),
    #[error("schedule task `{0}` not found")]
    NotFound(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Rule(#[from] crate::schedule::RuleError),
}

/// 新建任务的输入（`rule` 已解析为冻结规则，见 `schedule::parse_rule`）。
#[derive(Debug, Clone)]
pub struct NewTask {
    pub title: String,
    pub prompt: String,
    pub rule: Rule,
    pub target: Target,
    pub overlap: crate::schedule::Overlap,
    pub approval_timeout: Option<Duration>,
    /// 最小投递间隔：空闲后也要等这么久才能再投。
    pub rearm_after: Duration,
}

/// 编辑任务的增量补丁（`None` = 不修改该字段）。
#[derive(Debug, Clone, Default)]
pub struct EditTask {
    pub title: Option<String>,
    pub prompt: Option<String>,
    pub rule: Option<Rule>,
    pub target: Option<Target>,
    pub overlap: Option<crate::schedule::Overlap>,
    /// `Some(None)` = 清除审批超时。
    pub approval_timeout: Option<Option<Duration>>,
    pub rearm_after: Option<Duration>,
}

/// 定时任务管理服务。多个调用面（模型工具 / GUI）共用同一个实例。
#[derive(Debug, Clone)]
pub struct Scheduler {
    store: SessionStore,
}

impl Scheduler {
    pub fn new(store: SessionStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    pub fn list(&self) -> Vec<ScheduledTask> {
        self.store.load_schedule_tasks()
    }

    pub fn find(&self, id: &str) -> Option<ScheduledTask> {
        self.store
            .load_schedule_tasks()
            .into_iter()
            .find(|task| task.id == id)
    }

    /// 校验并创建任务：先落盘再返回，驱动侧只负责投递。
    pub fn create(
        &self,
        input: NewTask,
        now: PrimitiveDateTime,
    ) -> Result<ScheduledTask, ScheduleError> {
        let title = input.title.trim();
        if title.is_empty() {
            return Err(ScheduleError::Invalid(
                "title must not be empty".to_string(),
            ));
        }
        if title.chars().count() > 120 {
            return Err(ScheduleError::Invalid(
                "title must be at most 120 characters".to_string(),
            ));
        }
        if input.prompt.trim().is_empty() {
            return Err(ScheduleError::Invalid(
                "prompt must not be empty".to_string(),
            ));
        }
        if let Rule::Once { at } = &input.rule
            && *at <= now
        {
            return Err(ScheduleError::Invalid(
                "a one-shot task must be scheduled in the future".to_string(),
            ));
        }
        let id = ulid::Ulid::new().to_string();
        let next_fire_at = input.rule.next_after(now);
        let stakeholders = input
            .target
            .session_id()
            .map(str::to_owned)
            .into_iter()
            .collect();
        let task = ScheduledTask {
            id,
            title: input.title.clone(),
            prompt: input.prompt,
            rule: input.rule,
            target: input.target,
            overlap: input.overlap,
            approval_timeout: input.approval_timeout,
            status: TaskStatus::Active,
            stakeholders,
            last_receipt: None,
            delivery_log: Vec::new(),
            earlier_records_pruned: false,
            reserved_session_id: None,
            next_fire_at,
            rearm_after: input.rearm_after,
        };
        let mut tasks = self.store.load_schedule_tasks();
        tasks.push(task.clone());
        self.store.save_schedule_tasks(&tasks)?;
        Ok(task)
    }

    /// 编辑任务。改规则时从 `now` 重算下一次触发点。
    pub fn edit(
        &self,
        id: &str,
        patch: EditTask,
        now: PrimitiveDateTime,
    ) -> Result<ScheduledTask, ScheduleError> {
        let mut tasks = self.store.load_schedule_tasks();
        let updated = {
            let task = tasks
                .iter_mut()
                .find(|task| task.id == id)
                .ok_or_else(|| ScheduleError::NotFound(id.to_string()))?;
            if let Some(title) = patch.title {
                let trimmed = title.trim();
                if trimmed.is_empty() {
                    return Err(ScheduleError::Invalid(
                        "title must not be empty".to_string(),
                    ));
                }
                if trimmed.chars().count() > 120 {
                    return Err(ScheduleError::Invalid(
                        "title must be at most 120 characters".to_string(),
                    ));
                }
                task.title = title;
            }
            if let Some(prompt) = patch.prompt {
                if prompt.trim().is_empty() {
                    return Err(ScheduleError::Invalid(
                        "prompt must not be empty".to_string(),
                    ));
                }
                task.prompt = prompt;
            }
            if let Some(rule) = patch.rule {
                if let Rule::Once { at } = &rule
                    && *at <= now
                {
                    return Err(ScheduleError::Invalid(
                        "a one-shot task must be scheduled in the future".to_string(),
                    ));
                }
                task.rule = rule;
                if task.status == TaskStatus::Active {
                    task.next_fire_at = task.rule.next_after(now);
                }
            }
            if let Some(target) = patch.target {
                task.target = target.clone();
                task.stakeholders = target.session_id().map(str::to_owned).into_iter().collect();
            }
            if let Some(overlap) = patch.overlap {
                task.overlap = overlap;
            }
            if let Some(approval_timeout) = patch.approval_timeout {
                task.approval_timeout = approval_timeout;
            }
            if let Some(rearm_after) = patch.rearm_after {
                task.rearm_after = rearm_after;
            }
            task.clone()
        };
        self.store.save_schedule_tasks(&tasks)?;
        Ok(updated)
    }

    pub fn pause(&self, id: &str) -> Result<(), ScheduleError> {
        let mut tasks = self.store.load_schedule_tasks();
        let task = tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| ScheduleError::NotFound(id.to_string()))?;
        task.status = TaskStatus::Paused;
        self.store.save_schedule_tasks(&tasks)?;
        Ok(())
    }

    pub fn resume(&self, id: &str, now: PrimitiveDateTime) -> Result<(), ScheduleError> {
        let mut tasks = self.store.load_schedule_tasks();
        let task = tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| ScheduleError::NotFound(id.to_string()))?;
        task.status = TaskStatus::Active;
        if task.next_fire_at.is_none() {
            task.next_fire_at = task.rule.next_after(now);
        }
        self.store.save_schedule_tasks(&tasks)?;
        Ok(())
    }

    /// 立即执行一次：把下一次触发点拨到 `now`，驱动扫描即视为到期。
    pub fn run_now(&self, id: &str, now: PrimitiveDateTime) -> Result<(), ScheduleError> {
        let mut tasks = self.store.load_schedule_tasks();
        let task = tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| ScheduleError::NotFound(id.to_string()))?;
        if task.status != TaskStatus::Active {
            return Err(ScheduleError::Invalid(
                "cannot run a task that is paused or ended".to_string(),
            ));
        }
        task.next_fire_at = Some(now);
        self.store.save_schedule_tasks(&tasks)?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<(), ScheduleError> {
        let mut tasks = self.store.load_schedule_tasks();
        let before = tasks.len();
        tasks.retain(|task| task.id != id);
        if tasks.len() == before {
            return Err(ScheduleError::NotFound(id.to_string()));
        }
        self.store.save_schedule_tasks(&tasks)?;
        Ok(())
    }

    /// 内部读-改-写：驱动投递后写回执 / 推进发生点 / 换状态等引擎侧变更。
    pub fn update(
        &self,
        id: &str,
        f: impl FnOnce(&mut ScheduledTask),
    ) -> Result<(), ScheduleError> {
        let mut tasks = self.store.load_schedule_tasks();
        let Some(task) = tasks.iter_mut().find(|task| task.id == id) else {
            return Err(ScheduleError::NotFound(id.to_string()));
        };
        f(task);
        self.store.save_schedule_tasks(&tasks)?;
        Ok(())
    }

    /// 把 config 声明的 `[schedules.<name>]` 条目一次性导入为运行时任务（幂等）：
    /// 同名的任务已存在则跳过。
    pub fn import_config(
        &self,
        schedules: &[Schedule],
        states: &BTreeMap<String, ScheduleState>,
        now: PrimitiveDateTime,
    ) -> Result<usize, ScheduleError> {
        let mut tasks = self.store.load_schedule_tasks();
        let mut existing: HashSet<String> = tasks.iter().map(|task| task.id.clone()).collect();
        let mut added = 0;
        for schedule in schedules {
            if !existing.insert(schedule.name.clone()) {
                continue;
            }
            tasks.push(import_config_schedule(
                schedule,
                states.get(&schedule.name),
                now,
            ));
            added += 1;
        }
        if added > 0 {
            self.store.save_schedule_tasks(&tasks)?;
        }
        Ok(added)
    }
}

/// 模型工具访问定时任务管理的门面。
///
/// 引擎把运行时 `Scheduler` 以 trait object 注入 `ToolContext`（见 `tool.rs`），
/// qcode-tools 只依赖这个接口，不依赖具体实现与存储。方法都是同步文件操作，
/// 与 GUI 事件循环里对 `Scheduler` 的直接调用保持一致。
pub trait SchedulerHandle: Send + Sync + std::fmt::Debug {
    fn list(&self) -> Vec<ScheduledTask>;
    fn find(&self, id: &str) -> Option<ScheduledTask>;
    fn create(
        &self,
        input: NewTask,
        now: PrimitiveDateTime,
    ) -> Result<ScheduledTask, ScheduleError>;
    fn edit(
        &self,
        id: &str,
        patch: EditTask,
        now: PrimitiveDateTime,
    ) -> Result<ScheduledTask, ScheduleError>;
    fn pause(&self, id: &str) -> Result<(), ScheduleError>;
    fn resume(&self, id: &str, now: PrimitiveDateTime) -> Result<(), ScheduleError>;
    fn run_now(&self, id: &str, now: PrimitiveDateTime) -> Result<(), ScheduleError>;
    fn delete(&self, id: &str) -> Result<(), ScheduleError>;
}

impl SchedulerHandle for Scheduler {
    fn list(&self) -> Vec<ScheduledTask> {
        self.list()
    }

    fn find(&self, id: &str) -> Option<ScheduledTask> {
        self.find(id)
    }

    fn create(
        &self,
        input: NewTask,
        now: PrimitiveDateTime,
    ) -> Result<ScheduledTask, ScheduleError> {
        self.create(input, now)
    }

    fn edit(
        &self,
        id: &str,
        patch: EditTask,
        now: PrimitiveDateTime,
    ) -> Result<ScheduledTask, ScheduleError> {
        self.edit(id, patch, now)
    }

    fn pause(&self, id: &str) -> Result<(), ScheduleError> {
        self.pause(id)
    }

    fn resume(&self, id: &str, now: PrimitiveDateTime) -> Result<(), ScheduleError> {
        self.resume(id, now)
    }

    fn run_now(&self, id: &str, now: PrimitiveDateTime) -> Result<(), ScheduleError> {
        self.run_now(id, now)
    }

    fn delete(&self, id: &str) -> Result<(), ScheduleError> {
        self.delete(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{Overlap, Rule, TaskStatus};
    use time::Month::June;

    fn store() -> (tempfile::TempDir, Scheduler) {
        let dir = tempfile::tempdir().expect("tempdir");
        let scheduler = Scheduler::new(SessionStore::new(dir.path()));
        (dir, scheduler)
    }

    fn now() -> PrimitiveDateTime {
        crate::schedule::at(2026, June, 1, 10, 0)
    }

    fn new_task() -> NewTask {
        NewTask {
            title: "nightly".to_string(),
            prompt: "review the night's changes".to_string(),
            rule: Rule::Cron {
                expr: "0 3 * * *".to_string(),
            },
            target: Target::Session {
                id: "t1".to_string(),
            },
            overlap: Overlap::Queue,
            approval_timeout: None,
            rearm_after: Duration::ZERO,
        }
    }

    #[test]
    fn create_persists_and_lists() {
        let (_dir, scheduler) = store();
        let created = scheduler.create(new_task(), now()).expect("create");
        assert_eq!(created.status, TaskStatus::Active);
        assert_eq!(
            created.next_fire_at,
            Some(crate::schedule::at(2026, June, 2, 3, 0))
        );
        assert_eq!(created.stakeholders, vec!["t1".to_string()]);
        assert_eq!(scheduler.list().len(), 1);
        assert_eq!(scheduler.find(&created.id).expect("find").id, created.id);
    }

    #[test]
    fn create_validates_title_prompt_and_future_once() {
        let (_dir, scheduler) = store();
        let mut bad = new_task();
        bad.title = "  ".to_string();
        assert!(matches!(
            scheduler.create(bad, now()),
            Err(ScheduleError::Invalid(_))
        ));
        let mut bad = new_task();
        bad.prompt = String::new();
        assert!(matches!(
            scheduler.create(bad, now()),
            Err(ScheduleError::Invalid(_))
        ));
        let mut past = new_task();
        past.rule = Rule::Once {
            at: crate::schedule::at(2026, June, 1, 8, 0),
        };
        assert!(matches!(
            scheduler.create(past, now()),
            Err(ScheduleError::Invalid(_))
        ));
    }

    #[test]
    fn edit_pause_resume_run_now_and_delete() {
        let (_dir, scheduler) = store();
        let created = scheduler.create(new_task(), now()).expect("create");

        scheduler.pause(&created.id).expect("pause");
        assert_eq!(
            scheduler.find(&created.id).expect("find").status,
            TaskStatus::Paused
        );

        scheduler.resume(&created.id, now()).expect("resume");
        assert_eq!(
            scheduler.find(&created.id).expect("find").status,
            TaskStatus::Active
        );

        scheduler.run_now(&created.id, now()).expect("run now");
        assert_eq!(
            scheduler.find(&created.id).expect("find").next_fire_at,
            Some(now())
        );

        scheduler.delete(&created.id).expect("delete");
        assert!(scheduler.find(&created.id).is_none());
        assert!(matches!(
            scheduler.delete(&created.id),
            Err(ScheduleError::NotFound(_))
        ));
    }

    #[test]
    fn edit_recomputes_the_next_fire_when_the_rule_changes() {
        let (_dir, scheduler) = store();
        let created = scheduler.create(new_task(), now()).expect("create");
        let edited = scheduler
            .edit(
                &created.id,
                EditTask {
                    rule: Some(Rule::Daily {
                        hour: 9,
                        minute: 30,
                    }),
                    ..EditTask::default()
                },
                now(),
            )
            .expect("edit");
        assert_eq!(
            edited.rule,
            Rule::Daily {
                hour: 9,
                minute: 30
            }
        );
        assert_eq!(
            edited.next_fire_at,
            Some(crate::schedule::at(2026, June, 2, 9, 30))
        );
    }

    #[test]
    fn import_config_is_idempotent_and_preserves_state() {
        let (_dir, scheduler) = store();
        let schedule = Schedule {
            name: "nightly".to_string(),
            cron: crate::schedule::CronSpec::parse("0 3 * * *").expect("cron"),
            cron_text: "0 3 * * *".to_string(),
            task: Some("t1".to_string()),
            prompt: "review".to_string(),
            overlap: Overlap::Queue,
            approval_timeout: None,
            enabled: true,
        };
        let states = BTreeMap::new();
        assert_eq!(
            scheduler
                .import_config(&[schedule.clone()], &states, now())
                .expect("import"),
            1
        );
        assert_eq!(
            scheduler
                .import_config(&[schedule.clone()], &states, now())
                .expect("import"),
            0
        );
        let imported = scheduler.find("nightly").expect("found");
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
    }
}
