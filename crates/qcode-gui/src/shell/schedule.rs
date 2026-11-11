//! 定时任务（Scheduled Tasks）的 Shell 侧动作：侧栏 ＋/⋮ 新建、详情页、
//! 手动创建表单与动作下发。数据经 `Backend::schedule_tasks()` 读缓存、
//! `Backend::schedule_op()` 发命令（结果经 `ScheduleChanged` / `ScheduleNotice`
//! 事件回传，见 `shell::poll`）。

use super::*;
use qcode_core::schedule::{Overlap, TaskStatus, now_local, parse_rule};
use qcode_ui_bridge::ScheduleOp;

/// 详情页 / 侧栏对单个任务可做的动作。
#[derive(Debug, Clone, Copy)]
pub(crate) enum ScheduleAction {
    Pause,
    Resume,
    RunNow,
    Delete,
}

/// 手动创建定时任务的表单：每个字段一个输入框实体，提交时统一校验。
pub(crate) struct ScheduleFormState {
    pub(crate) title: Entity<InputState>,
    pub(crate) prompt: Entity<InputState>,
    pub(crate) rule: Entity<InputState>,
    pub(crate) target: Entity<InputState>,
    pub(crate) timeout_secs: Entity<InputState>,
    pub(crate) _subscription: gpui_kit::Subscription,
}

impl Shell {
    /// 侧栏「定时任务」＋ / ⋮·在对话中创建：新建会话并预填创建提示。
    ///
    /// 复用「我的任务」的 `new_session`（含草稿保护）：复用了当前空会话时
    /// 预填依然成立；被草稿保护拦下时，预填同样写进用户正在编辑的会话——
    /// 他点的是「创建定时任务」，这个预填正是他要的。
    pub(crate) fn new_schedule_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.schedule_menu_open = false;
        self.new_session(window, cx);
        // 预填提示保持简短；「未明确周期时先向用户确认」的引导在
        // schedule_create 工具描述里（模型调用工具时读到），不占输入框。
        self.ui.input.update(cx, |input, cx| {
            input.set_value("请根据需求创建一个定时任务。\n", window, cx);
            cx.notify();
        });
    }

    pub(crate) fn toggle_schedule_menu(&mut self, cx: &mut Context<Self>) {
        self.schedule_menu_open = !self.schedule_menu_open;
        cx.notify();
    }

    /// 手动创建：弹出表单（字段：名称 / 内容 / 周期 / 目标 / 审批超时）。
    pub(crate) fn open_schedule_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.schedule_menu_open = false;
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("任务名称"));
        let prompt = cx.new(|cx| InputState::new(window, cx).placeholder("到点后投递给会话的内容"));
        let rule = cx.new(|cx| {
            InputState::new(window, cx).placeholder("如 daily 09:00 / every 3600 / cron 0 9 * * *")
        });
        let target = cx.new(|cx| {
            InputState::new(window, cx).placeholder("new_once（默认） / new / session:会话ID")
        });
        let timeout_secs =
            cx.new(|cx| InputState::new(window, cx).placeholder("审批超时秒数（可留空）"));
        let _subscription = cx.subscribe(&title, |shell, _input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                shell.submit_schedule_form(cx);
            }
        });
        self.schedule_form = Some(ScheduleFormState {
            title,
            prompt,
            rule,
            target,
            timeout_secs,
            _subscription,
        });
        let focus = self
            .schedule_form
            .as_ref()
            .expect("form just set")
            .title
            .read(cx)
            .focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// 校验并提交手动创建表单；失败时保留表单并提示，成功则关闭并刷新。
    pub(crate) fn submit_schedule_form(&mut self, cx: &mut Context<Self>) {
        // 先接管表单（其输入框实体归本函数所有，读值不借 self），失败再放回。
        let Some(form) = self.schedule_form.take() else {
            return;
        };
        if self.backend.provider().is_none() {
            // 没有内核时 `schedule_op` 是空实现：不拦一下的话点了「创建」什么
            // 都不会发生，看起来像按钮坏了。
            self.schedule_form = Some(form);
            self.ui
                .app
                .push_notice("还没有配置 provider，先在 Settings 里配置再创建定时任务");
            cx.notify();
            return;
        }
        let title = form.title.read(cx).value().trim().to_string();
        let prompt = form.prompt.read(cx).value().trim().to_string();
        let rule_text = form.rule.read(cx).value().trim().to_string();
        let target_text = form.target.read(cx).value().trim().to_string();
        let timeout_text = form.timeout_secs.read(cx).value().trim().to_string();
        if title.is_empty() {
            return reject_form(self, form, cx, "请填写任务名称");
        }
        if prompt.is_empty() {
            return reject_form(self, form, cx, "请填写投递内容");
        }
        let rule = match parse_rule(&rule_text) {
            Ok(rule) => rule,
            Err(error) => {
                return reject_form(
                    self,
                    form,
                    cx,
                    &format!("周期无效：{error}（如 daily 09:00 / every 3600）"),
                );
            }
        };
        let target = if target_text.is_empty() {
            qcode_core::schedule::Target::NewOnce
        } else if target_text == "new" {
            qcode_core::schedule::Target::New
        } else if let Some(id) = target_text.strip_prefix("session:") {
            qcode_core::schedule::Target::Session { id: id.to_string() }
        } else {
            return reject_form(self, form, cx, "目标需为 new_once / new / session:<会话ID>");
        };
        let approval_timeout = if timeout_text.is_empty() {
            None
        } else {
            match timeout_text.parse::<u64>() {
                Ok(secs) if secs > 0 => Some(std::time::Duration::from_secs(secs)),
                _ => return reject_form(self, form, cx, "审批超时需为正整数秒"),
            }
        };
        self.backend.schedule_op(ScheduleOp::Create {
            title,
            prompt,
            rule,
            target,
            overlap: Overlap::Queue,
            approval_timeout,
            rearm_after: std::time::Duration::from_secs(300),
        });
        self.view = MainView::Schedule;
        self.schedule_detail = None;
        cx.notify();
    }

    /// 点侧栏任务行：进入详情 + 运行记录页。
    pub(crate) fn open_schedule_detail(&mut self, id: &str, cx: &mut Context<Self>) {
        self.view = MainView::Schedule;
        self.schedule_detail = Some(id.to_string());
        cx.notify();
    }

    /// 详情页动作下发（暂停 / 恢复 / 立即执行 / 删除）。
    pub(crate) fn schedule_action(
        &mut self,
        id: &str,
        action: ScheduleAction,
        cx: &mut Context<Self>,
    ) {
        if self.backend.provider().is_none() {
            self.ui
                .app
                .push_notice("还没有配置 provider，定时任务操作不可用");
            cx.notify();
            return;
        }
        let op = match action {
            ScheduleAction::Pause => ScheduleOp::Pause { id: id.to_string() },
            ScheduleAction::Resume => ScheduleOp::Resume { id: id.to_string() },
            ScheduleAction::RunNow => ScheduleOp::RunNow { id: id.to_string() },
            ScheduleAction::Delete => ScheduleOp::Delete { id: id.to_string() },
        };
        self.backend.schedule_op(op);
        cx.notify();
    }

    /// 会话视图页头时钟标记：当前会话是被某个 active 任务盯着的会话时，显示
    /// 该任务的下次触发时刻。取最近的一条，避免页头堆多个标签。
    pub(crate) fn schedule_clock_hint(&self) -> Option<String> {
        let session = self.active.as_deref()?;
        let now = now_local();
        self.schedule_tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Active)
            .filter(|task| task.target.session_id() == Some(session))
            .min_by_key(|task| task.next_fire_at.unwrap_or(now))
            .and_then(|task| {
                let at = task.next_fire_at?;
                Some(format!("⏱ {}", at.time()))
            })
    }
}

/// 表单校验失败：把表单放回（保留用户已填的内容）并提示。
fn reject_form(this: &mut Shell, form: ScheduleFormState, cx: &mut Context<Shell>, message: &str) {
    this.schedule_form = Some(form);
    this.ui.app.push_notice(message.to_string());
    cx.notify();
}
