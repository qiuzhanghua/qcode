//! 定时任务的内容区视图：从侧栏点进某个任务后的「详情 + 运行记录」页
//! （ADR-0063 §5 的 config 只读看板已升级为运行时任务详情；config 条目启动时
//! 迁移进运行时任务，见 `Scheduler::import_config`）。
//!
//! 数据来自 `Shell::schedule_tasks`（`Backend::schedule_tasks()` 的缓存，
//! 随任务列表一起刷新）；动作经 `Backend::schedule_op()` 下发，结果由
//! `ScheduleChanged` / `ScheduleNotice` 事件回传。

use super::*;
use crate::shell::schedule::{SCHEDULE_LOG_PAGE_SIZE, ScheduleAction};
use qcode_core::schedule::{ScheduledTask, TaskStatus, now_local};

/// Relative "when will this fire", computed at render time.
///
/// Computed here rather than stored, so "3 分钟后" counts down on its own without
/// a refresh. A past instant is a missed window, not a bug: say so instead of
/// showing a negative countdown.
fn format_next_fire(next: Option<time::PrimitiveDateTime>, now: time::PrimitiveDateTime) -> String {
    let Some(next) = next else {
        return "—".to_string();
    };
    let delta = next - now;
    let minutes = delta.whole_minutes();
    if delta.is_negative() {
        return qcode_i18n::tr("gui.schedule.next_fire.overdue").into_owned();
    }
    if minutes < 1 {
        return qcode_i18n::tr("gui.schedule.next_fire.soon").into_owned();
    }
    if minutes < 60 {
        return qcode_i18n::trp("gui.schedule.next_fire.minutes", &[minutes.to_string()]);
    }
    let hours = delta.whole_hours();
    if hours < 24 {
        let rest = minutes - hours * 60;
        return if rest == 0 {
            qcode_i18n::trp("gui.schedule.next_fire.hours", &[hours.to_string()])
        } else {
            qcode_i18n::trp(
                "gui.schedule.next_fire.hours_minutes",
                &[hours.to_string(), rest.to_string()],
            )
        };
    }
    let days = delta.whole_days();
    qcode_i18n::trp("gui.schedule.next_fire.days", &[days.to_string()])
}

/// 任务状态的中文标签。
fn status_label(status: TaskStatus) -> std::borrow::Cow<'static, str> {
    match status {
        TaskStatus::Active => qcode_i18n::tr("gui.schedule.status.active"),
        TaskStatus::Paused => qcode_i18n::tr("gui.schedule.status.paused"),
        TaskStatus::Ended => qcode_i18n::tr("gui.schedule.status.ended"),
    }
}

impl Shell {
    /// 内容区的定时任务视图：选中任务时展示「详情 + 运行记录」，否则是空态。
    pub(crate) fn render_schedule_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self
            .schedule_detail
            .as_deref()
            .and_then(|id| self.schedule_tasks.iter().find(|task| task.id == id));
        match selected {
            Some(task) => self.render_schedule_detail(task, cx).into_any_element(),
            None => self.render_schedule_empty(cx).into_any_element(),
        }
    }

    /// 空态：没有选中任务时给一个引导，而不是一块白板。
    fn render_schedule_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .child(
                div().px(px(MAIN_PAD_L)).pt_4().child(
                    div()
                        .text_size(sp(18.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(qcode_i18n::tr("gui.schedule.empty.title")),
                ),
            )
            .child(
                div().px(px(MAIN_PAD_L)).pt_1().child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(muted)
                        .child(qcode_i18n::tr("gui.schedule.empty.hint")),
                ),
            )
    }

    /// 详情 + 运行记录页。
    fn render_schedule_detail(
        &self,
        task: &ScheduledTask,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let border = theme.border;
        // 相对差值必须用本地墙钟（`next_fire_at` 是本地墙钟）：取 UTC 会
        // 让非零时区的"下次触发"凭空多出时区偏移（如 UTC+8 显示成 8 小时后）。
        let now = now_local();
        let status = task.status;
        let task_id = task.id.clone();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px(px(MAIN_PAD_L))
                    .pt_4()
                    .child(
                        Button::new("schedule-back")
                            .label("←")
                            .tooltip(qcode_i18n::tr("gui.schedule.detail.back_tooltip"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.schedule_detail = None;
                                this.view = MainView::Schedule;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_size(sp(18.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(task.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(sp(CAPTION_SIZE))
                            // 状态配色：已开启=绿，已暂停=黄，已结束=灰。
                            .text_color(match status {
                                TaskStatus::Active => rgb(0x16a34a),
                                TaskStatus::Paused => rgb(0xca8a04),
                                TaskStatus::Ended => muted.into(),
                            })
                            .child(status_label(status)),
                    ),
            )
            .child(
                div()
                    .id("schedule-detail")
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px(px(MAIN_PAD_L))
                    .pt_3()
                    .pb_4()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .p_3()
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(border)
                            .child(self.detail_line(
                                cx,
                                &qcode_i18n::tr("gui.schedule.detail.label.rule"),
                                &task.rule.describe(),
                            ))
                            .child(self.detail_line(
                                cx,
                                &qcode_i18n::tr("gui.schedule.detail.label.target"),
                                &task.target.describe(),
                            ))
                            .child(self.detail_line(
                                cx,
                                &qcode_i18n::tr("gui.schedule.detail.label.next_fire"),
                                &task.next_fire_at.map_or_else(
                                    || "—".to_string(),
                                    |at| {
                                        qcode_i18n::trp(
                                            "gui.schedule.detail.value.next_fire",
                                            &[at.to_string(), format_next_fire(Some(at), now)],
                                        )
                                    },
                                ),
                            ))
                            .child(
                                self.detail_line(
                                    cx,
                                    &qcode_i18n::tr("gui.schedule.detail.label.min_interval"),
                                    &qcode_i18n::trp(
                                        "gui.schedule.detail.value.min_interval",
                                        &[qcode_core::schedule::MIN_DELIVERY_INTERVAL
                                            .whole_seconds()
                                            .to_string()],
                                    ),
                                ),
                            )
                            .when_some(task.approval_timeout, |this, timeout| {
                                this.child(self.detail_line(
                                    cx,
                                    &qcode_i18n::tr("gui.schedule.detail.label.approval_timeout"),
                                    &format!("{}s", timeout.as_secs()),
                                ))
                            })
                            .when_some(task.reserved_session_id.as_deref(), |this, session| {
                                this.child(self.detail_line(
                                    cx,
                                    &qcode_i18n::tr("gui.schedule.detail.label.reserved_session"),
                                    session,
                                ))
                            })
                            .when_some(task.last_receipt.as_ref(), |this, receipt| {
                                this.child(
                                    self.detail_line(
                                        cx,
                                        &qcode_i18n::tr("gui.schedule.detail.label.last_delivery"),
                                        &qcode_i18n::trp(
                                            "gui.schedule.detail.value.last_delivery",
                                            &[
                                                receipt.delivered_at.to_string(),
                                                receipt
                                                    .target_session_id
                                                    .as_deref()
                                                    .unwrap_or("-")
                                                    .to_string(),
                                                receipt.message_id.to_string(),
                                            ],
                                        ),
                                    ),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("schedule-run-now")
                                    .label(if self.schedule_running.contains_key(&task_id) {
                                        qcode_i18n::tr("gui.schedule.detail.action.running")
                                    } else {
                                        qcode_i18n::tr("gui.schedule.detail.action.run_now")
                                    })
                                    .disabled(
                                        status != TaskStatus::Active
                                            || self.schedule_running.contains_key(&task_id)
                                            // 驱动不在跑：**真禁用**（ADR-0098 §4）。
                                            // 这一颗是「执行」，不是「登记」，所以它
                                            // 与列表相反——能点但点了没反应更糟。
                                            || self.driver_tone()
                                                == crate::shell::daemon::DriverTone::Dormant,
                                    )
                                    .tooltip(
                                        if self.driver_tone()
                                            == crate::shell::daemon::DriverTone::Dormant
                                        {
                                            qcode_i18n::tr(
                                                "gui.schedule.detail.tooltip.run_now_dormant",
                                            )
                                        } else {
                                            qcode_i18n::tr("gui.schedule.detail.tooltip.run_now")
                                        },
                                    )
                                    .on_click({
                                        let task_id = task_id.clone();
                                        cx.listener(move |this, _, _window, cx| {
                                            this.schedule_action(
                                                &task_id,
                                                ScheduleAction::RunNow,
                                                cx,
                                            );
                                        })
                                    }),
                            )
                            .child(if status == TaskStatus::Active {
                                Button::new("schedule-pause")
                                    .label(qcode_i18n::tr("gui.schedule.detail.action.pause"))
                                    .on_click({
                                        let task_id = task_id.clone();
                                        cx.listener(move |this, _, _window, cx| {
                                            this.schedule_action(
                                                &task_id,
                                                ScheduleAction::Pause,
                                                cx,
                                            );
                                        })
                                    })
                            } else {
                                Button::new("schedule-resume")
                                    .label(qcode_i18n::tr("gui.schedule.detail.action.resume"))
                                    .disabled(status == TaskStatus::Ended)
                                    .tooltip(qcode_i18n::tr(
                                        "gui.schedule.detail.tooltip.resume_ended",
                                    ))
                                    .on_click({
                                        let task_id = task_id.clone();
                                        cx.listener(move |this, _, _window, cx| {
                                            this.schedule_action(
                                                &task_id,
                                                ScheduleAction::Resume,
                                                cx,
                                            );
                                        })
                                    })
                            })
                            .when_some(
                                // 固定会话目标直接可跳；New/NewOnce 投递后落在
                                // 预留会话里，同样可跳。
                                task.target
                                    .session_id()
                                    .map(str::to_owned)
                                    .or_else(|| task.reserved_session_id.clone()),
                                |this, session| {
                                    this.child(
                                        Button::new("schedule-open-session")
                                            .label(qcode_i18n::tr(
                                                "gui.schedule.detail.action.open_session",
                                            ))
                                            .on_click({
                                                cx.listener(move |this, _, window, cx| {
                                                    this.open_sidebar_section(
                                                        SidebarSection::Tasks,
                                                        cx,
                                                    );
                                                    this.resume(session.clone(), window, cx);
                                                })
                                            }),
                                    )
                                },
                            )
                            .child(
                                Button::new("schedule-edit")
                                    .label(qcode_i18n::tr("gui.schedule.detail.action.edit"))
                                    .tooltip(qcode_i18n::tr("gui.schedule.detail.tooltip.edit"))
                                    .on_click({
                                        let task_id = task_id.clone();
                                        cx.listener(move |this, _, window, cx| {
                                            this.open_schedule_edit(&task_id, window, cx);
                                        })
                                    }),
                            )
                            .child(
                                Button::new("schedule-delete")
                                    .label(qcode_i18n::tr("gui.schedule.button.delete"))
                                    .danger()
                                    .tooltip(qcode_i18n::tr("gui.schedule.detail.tooltip.delete"))
                                    .on_click({
                                        let task_id = task_id.clone();
                                        cx.listener(move |this, _, _window, cx| {
                                            this.schedule_action(
                                                &task_id,
                                                ScheduleAction::Delete,
                                                cx,
                                            );
                                        })
                                    }),
                            ),
                    )
                    .child(div().text_size(sp(CAPTION_SIZE)).text_color(muted).child(
                        qcode_i18n::trp(
                            "gui.schedule.detail.log.count",
                            &[task.delivery_log.len().to_string()],
                        ),
                    ))
                    .child(
                        // 运行记录独立滚动区：详情卡片与操作按钮固定在上方，列表
                        // 自己滚；「加载更多」在列表末尾，没加载完就一直在。
                        div()
                            .id("schedule-log-scroll")
                            .flex_1()
                            .min_h(px(0.0))
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .overflow_y_scroll()
                            .children(
                                task.delivery_log
                                    .iter()
                                    .rev()
                                    .take(self.schedule_log_count)
                                    .map(|receipt| {
                                        div()
                                            .w_full()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .p_2()
                                            .rounded(px(8.0))
                                            .border_1()
                                            .border_color(border)
                                            // 主内容 = 执行的任务（prompt）；旧记录
                                            // 没有则回退到发生时刻。
                                            .child(div().text_size(sp(BODY_SIZE)).child(
                                                if receipt.prompt.is_empty() {
                                                    qcode_i18n::trp(
                                                        "gui.schedule.detail.log.legacy_entry",
                                                        &[receipt.occurrence_at.to_string()],
                                                    )
                                                } else {
                                                    receipt.prompt.clone()
                                                },
                                            ))
                                            .child(
                                                div()
                                                    .text_size(sp(CAPTION_SIZE))
                                                    .text_color(muted)
                                                    .child(qcode_i18n::trp(
                                                        "gui.schedule.detail.log.entry_meta",
                                                        &[
                                                            // 投递时间用实际投递确认时刻
                                                            // （`delivered_at`）：点击
                                                            // 「立即执行」时记录的就是那次
                                                            // 执行的时间，而不是计划触发点
                                                            // （`occurrence_at`）。
                                                            receipt.delivered_at.to_string(),
                                                            receipt
                                                                .target_session_id
                                                                .as_deref()
                                                                .unwrap_or("-")
                                                                .to_string(),
                                                            receipt.message_id.to_string(),
                                                        ],
                                                    )),
                                            )
                                            .into_any_element()
                                    }),
                            )
                            // 还有更早的记录没加载时，底部保留「加载更多」按钮；
                            // 全部加载完按钮自然消失。
                            .when(self.schedule_log_count < task.delivery_log.len(), |this| {
                                this.child(
                                    div().flex().justify_center().pt_1().child(
                                        Button::new("schedule-log-more")
                                            .label(qcode_i18n::tr("gui.schedule.detail.log.more"))
                                            .tooltip(qcode_i18n::tr(
                                                "gui.schedule.detail.log.more_tooltip",
                                            ))
                                            .on_click(cx.listener(|this, _, _window, cx| {
                                                this.schedule_log_count += SCHEDULE_LOG_PAGE_SIZE;
                                                cx.notify();
                                            })),
                                    ),
                                )
                            })
                            .when(task.earlier_records_pruned, |this| {
                                this.child(
                                    div()
                                        .text_size(sp(CAPTION_SIZE))
                                        .text_color(muted)
                                        .child(qcode_i18n::tr("gui.schedule.detail.log.pruned")),
                                )
                            }),
                    ),
            )
    }

    /// 详情页里的一行「键：值」。
    ///
    /// 值占满键位之后的剩余宽度（`flex_1`），并用 `min_w(0)` 允许在无空格
    /// 的长串（如很长的 session id / message id）处断行——否则超长的「最近
    /// 投递」会把整行撑出详情卡片。
    fn detail_line(&self, cx: &Context<Self>, key: &str, value: &str) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap_2()
            .text_size(sp(CAPTION_SIZE))
            .child(div().w(px(72.0)).text_color(muted).child(key.to_string()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(sp(BODY_SIZE))
                    .child(value.to_string()),
            )
    }
}
