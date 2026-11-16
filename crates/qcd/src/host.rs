//! daemon 对 [`qcode_driver::runtime::Host`] 的回答（ADR-0092 §6）。
//!
//! 十个方法里，daemon 与 GUI 的差别只有三处：
//!
//! 1. [`executor`](Host::executor) 是 `Unattended` —— 这是整个 daemon 的存在理由。
//! 2. [`is_busy`](Host::is_busy) 看 daemon 自己的集合，不是窗口的实时表。
//! 3. [`deliver`](Host::deliver) 没有收件箱可投：它直接把消息交给「立刻开跑」，
//!    因为 daemon 自己就是那个跑回合的人。
//!
//! 其余都是同一个 store 上的同一个问题。

use std::collections::HashSet;
use std::time::Duration;

use qcode_core::event::SessionStarted;
use qcode_core::schedule::{ScheduledMessage, ScheduledTask};
use qcode_driver::runtime::{Delivery, Host, Landed, StartTurn};
use qcode_driver::{Executor, Probe};
use qcode_kernel::Assembled;

pub struct DaemonHost<'a> {
    pub assembled: &'a Assembled,
    /// 正在跑回合的会话。
    pub live: &'a HashSet<String>,
    /// 这一拍刚投递、即将开跑的会话。`deliver` 往里写，`is_busy` 要读它。
    pub starting: &'a mut HashSet<String>,
    /// 写进心跳，供「谁在开车」分辨 gui / daemon。
    pub identity: &'a str,
    /// 墙钟。生产是系统时间，测试可快进（ADR-0092 验证 1 要进 CI）。
    pub clock: &'a dyn crate::WallClock,
}

impl Host for DaemonHost<'_> {
    fn now(&self) -> time::PrimitiveDateTime {
        self.clock.now()
    }

    fn heartbeat(&mut self, at: time::PrimitiveDateTime) {
        // 与 GUI 同一个 key：心跳回答「多久没动」，锁回答「有没有」（ADR-0091）。
        // 身份串让 `qc schedule list` 能说清是哪一个在开车。
        if let Err(error) = self.assembled.store.set_runtime_status(
            qcode_core::scheduler::DRIVER_HEARTBEAT_KEY,
            self.identity,
            at,
        ) {
            tracing::warn!(%error, "cannot write driver heartbeat");
        }
    }

    fn executor(&self) -> Executor {
        // daemon 的全部意义所在：没有人工。缺省审批截止时间从这里开始生效
        // （ADR-0092 §7）——不补的后果是会话写锁被握到进程退出。
        Executor::Unattended
    }

    fn tasks(&self) -> Vec<ScheduledTask> {
        qcode_core::scheduler::Scheduler::new(self.assembled.store.clone()).list()
    }

    fn update_task(&mut self, id: &str, patch: &mut dyn FnMut(&mut ScheduledTask)) -> bool {
        qcode_core::scheduler::Scheduler::new(self.assembled.store.clone())
            .update(id, patch)
            .is_ok()
    }

    fn is_busy(&self, task: &ScheduledTask) -> bool {
        match task.target.session_id() {
            Some(session) => self.live.contains(session) || self.starting.contains(session),
            // 没有绑定会话（`New` / `NewOnce`）就没有「忙」可言：它每次自建一个。
            None => false,
        }
    }

    fn exists(&self, task: &ScheduledTask) -> bool {
        match task.target.session_id() {
            Some(session) => self.assembled.store.task(session).is_ok(),
            None => true,
        }
    }

    fn create_session(&mut self) -> Option<String> {
        let started = SessionStarted {
            model: self.assembled.resolved.model.clone(),
            provider: self.assembled.resolved.name.clone(),
            cwd: self.assembled.store.root().display().to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            parent: None,
        };
        match self.assembled.store.create_task(&started) {
            Ok(created) => Some(created.id),
            Err(error) => {
                tracing::warn!(%error, "cannot create a session for a scheduled task");
                None
            }
        }
    }

    fn deliver(
        &mut self,
        session: &str,
        task: &ScheduledTask,
        approval: Option<Duration>,
    ) -> Option<Delivery> {
        let scheduled = ScheduledMessage {
            task_id: task.id.clone(),
            title: task.title.clone(),
            prompt: task.prompt.clone(),
            at: task
                .next_fire_at
                .map(|at| at.to_string())
                .unwrap_or_default(),
        };
        let busy = self.live.contains(session) || self.starting.contains(session);

        // 没有收件箱可投：daemon 自己就是跑回合的人，所以「不忙」就等于「立刻开跑」。
        // 消息 id 仍然要给——驱动按它查「这条落盘了吗」，而落盘发生在 `run_turn`
        // 开头（`RunOptions.scheduled` 那条事件）。
        let message_id = format!("{}:{}:{}", task.id, session, scheduled.at);

        let start = if busy {
            None
        } else {
            self.starting.insert(session.to_string());
            Some(StartTurn {
                session: session.to_string(),
                text: task.prompt.clone(),
                scheduled,
                approval,
            })
        };

        Some(Delivery { message_id, start })
    }

    fn landed(&self, session: &str, task_id: &str, at: &str) -> Landed {
        match self
            .assembled
            .store
            .schedule_delivery_landed(session, task_id, at)
        {
            Ok(true) => Landed::Yes,
            Ok(false) => Landed::NotYet,
            // 读不出来（会话被删、日志坏了）**不能**算 `NotYet`：那会让这个槽位永久
            // 卡在 in-flight，任务再也不会被投递。驱动拿到 `Gone` 会记一次放弃。
            Err(_) => Landed::Gone,
        }
    }

    fn changed(&mut self) {
        // 阶段 2 只写日志（ADR-0092 阶段 5 才做「待查记录」供前端查询）。
        // 有它是为了让 `qc logs` 能回答「这一拍到底改了什么」。
        tracing::debug!("schedule tasks changed");
    }
}

/// `Probe` 也实现一遍，因为一些驱动路径直接用它而不是 `Host` 的两个方法。
impl Probe for DaemonHost<'_> {
    fn is_busy(&self, task: &ScheduledTask) -> bool {
        <Self as Host>::is_busy(self, task)
    }

    fn exists(&self, task: &ScheduledTask) -> bool {
        <Self as Host>::exists(self, task)
    }
}
