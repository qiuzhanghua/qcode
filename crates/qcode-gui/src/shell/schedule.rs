//! 定时任务（Scheduled Tasks）的 Shell 侧动作：侧栏 ＋/⋮ 新建、详情页、
//! 手动创建表单与动作下发。数据经 `Backend::schedule_tasks()` 读缓存、
//! `Backend::schedule_op()` 发命令（结果经 `ScheduleChanged` / `ScheduleCreated`
//! / `ScheduleNotice` 事件回传，见 `shell::poll`）。

use super::*;
use gpui_kit::component::date_picker::DatePickerState;
use gpui_kit::component::time_field::{HourCycle, TimeFieldState, TimePrecision};
use qcode_core::schedule::{
    Overlap, Rule, ScheduledTask, Target, TaskStatus, now_local, parse_rule,
};
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
    Monthly,
    Cron,
}

impl ScheduleRuleKind {
    /// 菜单里的显示顺序。
    pub(crate) const ALL: [Self; 6] = [
        Self::Daily,
        Self::Weekly,
        Self::Monthly,
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
            Self::Monthly => "每月固定时刻",
            Self::Cron => "自定义 Cron",
        }
    }

    pub(crate) fn from_stored(value: &str) -> Self {
        match value {
            "weekly" => Self::Weekly,
            "interval" => Self::Interval,
            "once" => Self::Once,
            "monthly" => Self::Monthly,
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
            Self::Monthly => "monthly",
            Self::Cron => "cron",
        }
    }
}

/// 手动创建表单里「目标」的选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleTargetKind {
    NewOnce,
    New,
}

impl ScheduleTargetKind {
    pub(crate) const ALL: [Self; 2] = [Self::NewOnce, Self::New];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NewOnce => "新会话（首次投递创建并复用）",
            Self::New => "新会话（每次投递都新建）",
        }
    }

    pub(crate) fn from_stored(value: &str) -> Self {
        match value {
            "new" => Self::New,
            _ => Self::NewOnce,
        }
    }

    pub(crate) fn as_stored(self) -> &'static str {
        match self {
            Self::NewOnce => "new_once",
            Self::New => "new",
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
    /// daily / weekly / monthly 的时刻：数字时间字段（HH:MM，不含日期）。
    pub(crate) time: Entity<TimeFieldState>,
    /// weekly 的星期多选（0=周日，同 crontab；去重后至少一个）。
    pub(crate) weekdays: Vec<u8>,
    /// monthly 的日期（1-31）。
    pub(crate) monthly_day: Entity<InputState>,
    /// interval 的秒数（≥ 60）。
    pub(crate) interval_secs: Entity<InputState>,
    /// once 的日期与时刻：官方 DatePicker 一条龙（日历选日期 + 数字时间字段，
    /// 参考 a.html 的 `input[type=time]` 样式）。提交时读其 `date_time()`。
    pub(crate) once_picker: Entity<DatePickerState>,
    /// cron 的 5 字段表达式。
    pub(crate) cron_expr: Entity<InputState>,
    // 目标：类型下拉。
    pub(crate) target_kind: ScheduleTargetKind,
    /// 投递回合的工作目录（绝对路径）；`None` = 用会话 / 引擎默认。
    pub(crate) cwd: Option<String>,
    /// 编辑模式：要修改的任务 id；`None` = 手动创建。
    pub(crate) editing: Option<String>,
    // 各下拉菜单的开关。
    pub(crate) rule_menu_open: bool,
    pub(crate) target_menu_open: bool,
    pub(crate) _subscription: gpui_kit::Subscription,
}

impl Shell {
    /// 侧栏「定时任务」＋ / ⋮·在对话中创建：新建会话并预填创建提示。
    ///
    /// 任务列表里已经有一个「新任务」（尚无标题 / 内容的空任务）时，直接
    /// 跳过去复用，不再新建——避免点一次就堆一个空的「新任务」行。其余情况
    /// 复用「我的任务」的 `new_session`（含草稿保护）：复用了当前空会话时
    /// 预填依然成立；被草稿保护拦下时，预填同样写进用户正在编辑的会话——
    /// 他点的是「创建定时任务」，这个预填正是他要的。
    pub(crate) fn new_schedule_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.schedule_menu_open = false;
        // 任务列表里已存在的空任务（`titles` 里没有条目 = 尚无标题 = 「新任务」）。
        let existing_new = self
            .tasks
            .iter()
            .find(|row| !self.titles.contains_key(&row.task.id))
            .map(|row| row.task.id.clone());
        match existing_new {
            Some(id) if self.active.as_deref() != Some(id.as_str()) => {
                self.resume(id, window, cx);
            }
            _ => {
                self.new_session(window, cx);
                // 从定时任务详情页点进来时，`new_session` 可能因「复用空任务 /
                // 草稿保护」提前返回（不切视图），这里必须切回任务视图，
                // 预填才看得到。
                self.enter_task_view(cx);
            }
        }
        // 预填提示保持简短；「未明确周期时先向用户确认」的引导在
        // schedule_create 工具描述里（模型调用工具时读到），不占输入框。
        // 注意：title 生成会剥掉这个前缀（见 `qcode_core::task::SCHEDULE_DRAFT_PREFILL`）。
        // set_value 直接覆盖：输入框原本有内容也一并清掉。
        let prefill = format!("{}\n", qcode_core::task::SCHEDULE_DRAFT_PREFILL);
        self.ui.input.update(cx, |input, cx| {
            input.set_value(prefill, window, cx);
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

    /// 设置状态过滤条件（`None` = 全部）并关闭 ⋮ 菜单。
    pub(crate) fn set_schedule_filter(
        &mut self,
        filter: Option<TaskStatus>,
        cx: &mut Context<Self>,
    ) {
        self.schedule_filter = filter;
        self.schedule_menu_open = false;
        cx.notify();
    }

    /// 手动创建：弹出表单（字段：名称 / 内容 / 周期 / 目标 / 审批超时）。
    ///
    /// 周期与目标是下拉菜单：周期默认「每天 09:00」，目标默认「新会话（复用）」，
    /// 用户先选类型再填该类型自己的参数，不用手写 `daily 09:00` 这类表达式。
    pub(crate) fn open_schedule_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.schedule_menu_open = false;
        self.schedule_form_with(None, window, cx);
    }

    /// 详情页「修改」：打开编辑表单并预填原任务内容。
    pub(crate) fn open_schedule_edit(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self
            .schedule_tasks
            .iter()
            .find(|task| task.id == id)
            .cloned()
        else {
            return;
        };
        self.schedule_form_with(Some(&task), window, cx);
    }

    /// 构造手动创建 / 编辑定时任务的表单。
    ///
    /// 实体先按创建默认值构造，`prefill`（编辑模式）随后把原任务的字段写回
    /// 对应输入框 / 参数。周期与目标下拉默认值同 `open_schedule_form`。
    fn schedule_form_with(
        &mut self,
        prefill: Option<&ScheduledTask>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("任务名称"));
        let prompt = cx.new(|cx| InputState::new(window, cx).placeholder("到点后投递给会话的内容"));
        let time = cx.new(|cx| {
            let mut state = TimeFieldState::new(window, cx)
                .precision(TimePrecision::Minute)
                .hour_cycle(HourCycle::H23);
            state.set_time(
                chrono::NaiveTime::from_hms_opt(9, 0, 0).expect("09:00"),
                window,
                cx,
            );
            state
        });
        let interval_secs = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("秒数（≥ 60）");
            input.set_value("3600", window, cx);
            input
        });
        let monthly_day = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("日（1-31）");
            input.set_value("15", window, cx);
            input
        });
        // 一次性默认「明天 09:00」：once 要求未来时刻，默认值保证选完周期即可
        // 提交；用户仍可在日历里改日期、在数字时间字段里改时刻。
        let once_picker = cx.new(|cx| {
            let mut state = DatePickerState::new(window, cx)
                .date_format("%Y-%m-%d %H:%M")
                .time_precision(TimePrecision::Minute)
                .hour_cycle(HourCycle::H23)
                .default_time(chrono::NaiveTime::from_hms_opt(9, 0, 0).expect("09:00"));
            let tomorrow = now_local().date().saturating_add(time::Duration::days(1));
            let naive = chrono::NaiveDate::from_ymd_opt(
                tomorrow.year(),
                tomorrow.month() as u32,
                tomorrow.day() as u32,
            )
            .expect("tomorrow is a real date");
            state.set_date_time(naive.and_hms_opt(9, 0, 0).expect("09:00"), window, cx);
            state
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

        // 创建默认值；编辑模式随后用原任务字段覆盖。
        let mut rule_kind = ScheduleRuleKind::Daily;
        let mut weekdays = vec![1];
        let mut target_kind = ScheduleTargetKind::NewOnce;
        let mut cwd = None;
        let mut editing = None;

        if let Some(task) = prefill {
            editing = Some(task.id.clone());
            cwd = task.cwd.clone();
            title.update(cx, |input, cx| {
                input.set_value(&task.title, window, cx);
            });
            prompt.update(cx, |input, cx| {
                input.set_value(&task.prompt, window, cx);
            });
            timeout_secs.update(cx, |input, cx| {
                if let Some(timeout) = task.approval_timeout {
                    input.set_value(timeout.as_secs().to_string(), window, cx);
                }
            });
            match &task.rule {
                Rule::Daily { hour, minute } => {
                    rule_kind = ScheduleRuleKind::Daily;
                    set_time(&time, *hour, *minute, window, cx);
                }
                Rule::Weekly {
                    weekdays: days,
                    weekday,
                    hour,
                    minute,
                } => {
                    rule_kind = ScheduleRuleKind::Weekly;
                    weekdays = Rule::effective_weekdays(days, *weekday);
                    set_time(&time, *hour, *minute, window, cx);
                }
                Rule::Interval { seconds } => {
                    rule_kind = ScheduleRuleKind::Interval;
                    interval_secs.update(cx, |input, cx| {
                        input.set_value(seconds.to_string(), window, cx);
                    });
                }
                Rule::Once { at } => {
                    rule_kind = ScheduleRuleKind::Once;
                    let naive = chrono::NaiveDate::from_ymd_opt(
                        at.year(),
                        at.month() as u32,
                        at.day() as u32,
                    )
                    .and_then(|date| date.and_hms_opt(at.hour() as u32, at.minute() as u32, 0))
                    .expect("once occurrence is a real date");
                    once_picker.update(cx, |picker, cx| {
                        picker.set_date_time(naive, window, cx);
                    });
                }
                Rule::Monthly { day, hour, minute } => {
                    rule_kind = ScheduleRuleKind::Monthly;
                    monthly_day.update(cx, |input, cx| {
                        input.set_value(day.to_string(), window, cx);
                    });
                    set_time(&time, *hour, *minute, window, cx);
                }
                Rule::Cron { expr } => {
                    rule_kind = ScheduleRuleKind::Cron;
                    cron_expr.update(cx, |input, cx| {
                        input.set_value(expr, window, cx);
                    });
                }
            }
            // 固定会话目标（session:<id>）表单无法表达：编辑时保持原目标不变
            //（提交走 `target: None`），下拉仅作占位显示。
            target_kind = match task.target {
                Target::New => ScheduleTargetKind::New,
                _ => ScheduleTargetKind::NewOnce,
            };
        }

        self.schedule_form = Some(ScheduleFormState {
            title,
            prompt,
            timeout_secs,
            rule_kind,
            time,
            weekdays,
            monthly_day,
            interval_secs,
            once_picker,
            cron_expr,
            target_kind,
            cwd,
            editing,
            rule_menu_open: false,
            target_menu_open: false,
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
            ScheduleRuleKind::Daily | ScheduleRuleKind::Weekly | ScheduleRuleKind::Monthly => {
                // 时间字段读出即格式化，避免在 match 里借用 form（reject_form
                // 之后要接管它）。
                let clock = form.time.read(cx).time().format("%H:%M");
                match form.rule_kind {
                    ScheduleRuleKind::Daily => format!("daily {clock}"),
                    ScheduleRuleKind::Weekly => {
                        let mut weekdays = form.weekdays.clone();
                        weekdays.sort_unstable();
                        weekdays.dedup();
                        if weekdays.is_empty() {
                            return reject_form(self, form, cx, "请至少选择一个星期");
                        }
                        format!(
                            "weekly {} {clock}",
                            weekdays
                                .iter()
                                .map(|day| day.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        )
                    }
                    _ => {
                        let day = form.monthly_day.read(cx).value().trim().to_string();
                        if day.is_empty() {
                            return reject_form(self, form, cx, "请填写每月日期");
                        }
                        format!("monthly {day} {clock}")
                    }
                }
            }
            ScheduleRuleKind::Interval => {
                format!("every {}", form.interval_secs.read(cx).value().trim())
            }
            ScheduleRuleKind::Once => {
                // 读一次即结束借用：`reject_form` 要接管 form。
                let picked = form.once_picker.read(cx).date_time().start();
                let Some(naive) = picked else {
                    return reject_form(self, form, cx, "请选择日期");
                };
                format!("once {}", naive.format("%Y-%m-%d %H:%M"))
            }
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
        };
        let editing = form.editing.clone();
        let approval_timeout = if timeout_text.is_empty() {
            None
        } else {
            match timeout_text.parse::<u64>() {
                Ok(secs) if secs > 0 => Some(std::time::Duration::from_secs(secs)),
                _ => return reject_form(self, form, cx, "审批超时需为正整数秒"),
            }
        };
        match editing {
            Some(id) => {
                // 编辑：target / overlap 不可改（固定会话目标表单无法表达），
                // 传 None 保持原值；审批超时留空 = 清除。
                self.backend.schedule_op(ScheduleOp::Edit {
                    id,
                    title: Some(title),
                    prompt: Some(prompt),
                    rule: Some(rule),
                    target: None,
                    overlap: None,
                    approval_timeout: Some(approval_timeout),
                });
            }
            None => {
                self.backend.schedule_op(ScheduleOp::Create {
                    title,
                    prompt,
                    rule,
                    target,
                    overlap: Overlap::Queue,
                    approval_timeout,
                    cwd: form.cwd,
                });
            }
        }
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

/// 把 HH:MM 写进表单的时间字段（daily / weekly / monthly 预填共用）。
fn set_time(
    field: &Entity<TimeFieldState>,
    hour: u8,
    minute: u8,
    window: &mut Window,
    cx: &mut Context<Shell>,
) {
    field.update(cx, |state, cx| {
        state.set_time(
            chrono::NaiveTime::from_hms_opt(hour as u32, minute as u32, 0).expect("HH:MM"),
            window,
            cx,
        );
    });
}
