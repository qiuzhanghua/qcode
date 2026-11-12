//! 定时任务（Scheduled Tasks）的 Shell 侧动作：侧栏 ＋/⋮ 新建、详情页、
//! 手动创建表单与动作下发。数据经 `Backend::schedule_tasks()` 读缓存、
//! `Backend::schedule_op()` 发命令（结果经 `ScheduleChanged` / `ScheduleCreated`
//! / `ScheduleNotice` 事件回传，见 `shell::poll`）。

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

/// 手动创建表单里「周期」的选择：每种规则一套自己的参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleRuleKind {
    Daily,
    Weekly,
    Interval,
    Once,
    Cron,
}

impl ScheduleRuleKind {
    /// 菜单里的显示顺序。
    pub(crate) const ALL: [Self; 5] = [
        Self::Daily,
        Self::Weekly,
        Self::Interval,
        Self::Once,
        Self::Cron,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Daily => "每天固定时刻",
            Self::Weekly => "每周固定时刻",
            Self::Interval => "每隔 N 秒",
            Self::Once => "一次性",
            Self::Cron => "自定义 Cron",
        }
    }

    pub(crate) fn from_stored(value: &str) -> Self {
        match value {
            "weekly" => Self::Weekly,
            "interval" => Self::Interval,
            "once" => Self::Once,
            "cron" => Self::Cron,
            _ => Self::Daily,
        }
    }

    pub(crate) fn as_stored(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Interval => "interval",
            Self::Once => "once",
            Self::Cron => "cron",
        }
    }
}

/// 手动创建表单里「目标」的选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleTargetKind {
    NewOnce,
    New,
    Session,
}

impl ScheduleTargetKind {
    pub(crate) const ALL: [Self; 3] = [Self::NewOnce, Self::New, Self::Session];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NewOnce => "新会话（首次投递创建并复用）",
            Self::New => "新会话（每次投递都新建）",
            Self::Session => "指定会话",
        }
    }

    pub(crate) fn from_stored(value: &str) -> Self {
        match value {
            "new" => Self::New,
            "session" => Self::Session,
            _ => Self::NewOnce,
        }
    }

    pub(crate) fn as_stored(self) -> &'static str {
        match self {
            Self::NewOnce => "new_once",
            Self::New => "new",
            Self::Session => "session",
        }
    }
}

/// 手动创建定时任务的表单：名称 / 内容 / 审批超时是输入框；周期与目标是
/// 下拉菜单 + 各自的参数。提交时把结构化状态翻译回规范表达式统一校验。
pub(crate) struct ScheduleFormState {
    pub(crate) title: Entity<InputState>,
    pub(crate) prompt: Entity<InputState>,
    pub(crate) timeout_secs: Entity<InputState>,
    // 周期：类型下拉 + 各类型自己的参数。
    pub(crate) rule_kind: ScheduleRuleKind,
    /// daily / weekly 的时刻（HH:MM）。
    pub(crate) time: Entity<InputState>,
    /// weekly 的星期（0=周日，同 crontab）。
    pub(crate) weekday: u8,
    /// interval 的秒数（≥ 60）。
    pub(crate) interval_secs: Entity<InputState>,
    /// once 的时刻（YYYY-MM-DD HH:MM）。
    pub(crate) once_at: Entity<InputState>,
    /// cron 的 5 字段表达式。
    pub(crate) cron_expr: Entity<InputState>,
    // 目标：类型下拉 + 会话选择。
    pub(crate) target_kind: ScheduleTargetKind,
    pub(crate) session_id: Option<String>,
    // 各下拉菜单的开关。
    pub(crate) rule_menu_open: bool,
    pub(crate) weekday_menu_open: bool,
    pub(crate) target_menu_open: bool,
    pub(crate) session_menu_open: bool,
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
        const PREFILL: &str = "请根据需求创建一个定时任务。\n";
        self.ui.input.update(cx, |input, cx| {
            input.set_value(PREFILL, window, cx);
            // 合成器是多行文本域：set_value 会把选择清到 0..0（开头），
            // 光标落在第一行而不是预填句子的下一行。把光标移到文本末尾，
            // 用户直接在下行输入需求。
            let end = input.value().len();
            input.set_selected_range(end..end, cx);
            cx.notify();
        });
    }

    pub(crate) fn toggle_schedule_menu(&mut self, cx: &mut Context<Self>) {
        self.schedule_menu_open = !self.schedule_menu_open;
        cx.notify();
    }

    /// 手动创建：弹出表单（字段：名称 / 内容 / 周期 / 目标 / 审批超时）。
    ///
    /// 周期与目标是下拉菜单：周期默认「每天 09:00」，目标默认「新会话（复用）」，
    /// 用户先选类型再填该类型自己的参数，不用手写 `daily 09:00` 这类表达式。
    pub(crate) fn open_schedule_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.schedule_menu_open = false;
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("任务名称"));
        let prompt = cx.new(|cx| InputState::new(window, cx).placeholder("到点后投递给会话的内容"));
        let time = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("HH:MM，如 09:00");
            input.set_value("09:00", window, cx);
            input
        });
        let interval_secs = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("秒数（≥ 60）");
            input.set_value("3600", window, cx);
            input
        });
        let once_at = cx.new(|cx| {
            InputState::new(window, cx).placeholder("YYYY-MM-DD HH:MM，如 2026-01-01 10:00")
        });
        let cron_expr =
            cx.new(|cx| InputState::new(window, cx).placeholder("5 字段，如 0 9 * * *"));
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
            timeout_secs,
            rule_kind: ScheduleRuleKind::Daily,
            time,
            weekday: 1,
            interval_secs,
            once_at,
            cron_expr,
            target_kind: ScheduleTargetKind::NewOnce,
            session_id: None,
            rule_menu_open: false,
            weekday_menu_open: false,
            target_menu_open: false,
            session_menu_open: false,
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
        let timeout_text = form.timeout_secs.read(cx).value().trim().to_string();
        if title.is_empty() {
            return reject_form(self, form, cx, "请填写任务名称");
        }
        if prompt.is_empty() {
            return reject_form(self, form, cx, "请填写投递内容");
        }
        // 把结构化周期翻译回规范表达式，复用 parse_rule 的校验与错误文案：
        // 用户从菜单选类型、填参数，落盘仍是我们一直用的那种规则描述。
        let rule_text = match form.rule_kind {
            ScheduleRuleKind::Daily => format!("daily {}", form.time.read(cx).value().trim()),
            ScheduleRuleKind::Weekly => format!(
                "weekly {} {}",
                form.weekday,
                form.time.read(cx).value().trim()
            ),
            ScheduleRuleKind::Interval => {
                format!("every {}", form.interval_secs.read(cx).value().trim())
            }
            ScheduleRuleKind::Once => format!("once {}", form.once_at.read(cx).value().trim()),
            ScheduleRuleKind::Cron => format!("cron {}", form.cron_expr.read(cx).value().trim()),
        };
        let rule = match parse_rule(&rule_text) {
            Ok(rule) => rule,
            Err(error) => {
                return reject_form(self, form, cx, &format!("周期无效：{error}"));
            }
        };
        let target = match form.target_kind {
            ScheduleTargetKind::NewOnce => qcode_core::schedule::Target::NewOnce,
            ScheduleTargetKind::New => qcode_core::schedule::Target::New,
            // 用引用匹配：`_` 分支还要把 form 原样放回（reject_form）。
            ScheduleTargetKind::Session => match &form.session_id {
                Some(id) if !id.is_empty() => {
                    qcode_core::schedule::Target::Session { id: id.clone() }
                }
                _ => return reject_form(self, form, cx, "请选择目标会话"),
            },
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
            ScheduleAction::RunNow => {
                // 记录执行中状态，按钮立即进入等待态，直到投递落盘确认。
                self.schedule_running
                    .insert(id.to_string(), qcode_core::schedule::now_local());
                ScheduleOp::RunNow { id: id.to_string() }
            }
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
