//! 定时任务的内容区视图：从侧栏点进某个任务后的「详情 + 运行记录」页
//! （ADR-0063 §5 的 config 只读看板已升级为运行时任务详情；config 条目启动时
//! 迁移进运行时任务，见 `Scheduler::import_config`）。
//!
//! 数据来自 `Shell::schedule_tasks`（`Backend::schedule_tasks()` 的缓存，
//! 随任务列表一起刷新）；动作经 `Backend::schedule_op()` 下发，结果由
//! `ScheduleChanged` / `ScheduleNotice` 事件回传。

use super::*;
use crate::shell::schedule::ScheduleAction;
use qcode_core::schedule::{ScheduledTask, TaskStatus};

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
        return "已过期".to_string();
    }
    if minutes < 1 {
        return "不到 1 分钟后".to_string();
    }
    if minutes < 60 {
        return format!("{minutes} 分钟后");
    }
    let hours = delta.whole_hours();
    if hours < 24 {
        let rest = minutes - hours * 60;
        return if rest == 0 {
            format!("{hours} 小时后")
        } else {
            format!("{hours} 小时 {rest} 分钟后")
        };
    }
    let days = delta.whole_days();
    format!("{days} 天后")
}

/// 任务状态的中文标签。
fn status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Active => "运行中",
        TaskStatus::Paused => "已暂停",
        TaskStatus::Ended => "已结束",
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
                        .child("定时任务"),
                ),
            )
            .child(
                div().px(px(MAIN_PAD_L)).pt_1().child(
                    div().text_size(sp(CAPTION_SIZE)).text_color(muted).child(
                        "从左侧选择一个定时任务查看详情与运行记录；点「＋」在对话中创建，或用「⋮」手动创建。",
                    ),
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
        let now = {
            let utc = time::OffsetDateTime::from(std::time::SystemTime::now());
            time::PrimitiveDateTime::new(utc.date(), utc.time())
        };
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
                            .tooltip("返回定时任务列表")
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
                            .text_color(if status == TaskStatus::Active {
                                theme.info
                            } else {
                                muted
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
                    .overflow_y_scroll()
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
                            .child(self.detail_line(cx, "周期", &task.rule.describe()))
                            .child(self.detail_line(cx, "目标", &task.target.describe()))
                            .child(self.detail_line(
                                cx,
                                "下次触发",
                                &task.next_fire_at.map_or_else(
                                    || "—".to_string(),
                                    |at| format!("{at}（{}）", format_next_fire(Some(at), now)),
                                ),
                            ))
                            .child(self.detail_line(
                                cx,
                                "投递间隔",
                                &format!("{}s", task.rearm_after.as_secs()),
                            ))
                            .when_some(task.approval_timeout, |this, timeout| {
                                this.child(self.detail_line(
                                    cx,
                                    "审批超时",
                                    &format!("{}s", timeout.as_secs()),
                                ))
                            })
                            .when_some(task.reserved_session_id.as_deref(), |this, session| {
                                this.child(self.detail_line(cx, "预留会话", session))
                            })
                            .when_some(task.last_receipt.as_ref(), |this, receipt| {
                                this.child(self.detail_line(
                                    cx,
                                    "最近投递",
                                    &format!(
                                        "{} → {}（message {}）",
                                        receipt.delivered_at,
                                        receipt.target_session_id.as_deref().unwrap_or("-"),
                                        receipt.message_id,
                                    ),
                                ))
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
                                        "执行中…"
                                    } else {
                                        "立即执行"
                                    })
                                    .disabled(
                                        status != TaskStatus::Active
                                            || self.schedule_running.contains_key(&task_id),
                                    )
                                    .tooltip("立即投递一次（不影响周期）")
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
                                Button::new("schedule-pause").label("暂停").on_click({
                                    let task_id = task_id.clone();
                                    cx.listener(move |this, _, _window, cx| {
                                        this.schedule_action(&task_id, ScheduleAction::Pause, cx);
                                    })
                                })
                            } else {
                                Button::new("schedule-resume")
                                    .label("恢复")
                                    .disabled(status == TaskStatus::Ended)
                                    .tooltip("已结束的任务不能恢复")
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
                                            .label("打开会话")
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
                                Button::new("schedule-delete")
                                    .label("删除")
                                    .danger()
                                    .tooltip("删除任务及其运行记录")
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
                    .child(
                        div()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(muted)
                            .child(format!(
                                "运行记录：共 {} 条（保留最近 30 天 / 200 条）",
                                task.delivery_log.len()
                            )),
                    )
                    .children(task.delivery_log.iter().rev().take(30).map(|receipt| {
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .p_2()
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(border)
                            .child(
                                div()
                                    .text_size(sp(BODY_SIZE))
                                    .child(format!("{}", receipt.occurrence_at)),
                            )
                            .child(div().text_size(sp(CAPTION_SIZE)).text_color(muted).child(
                                format!(
                                    "投递到 {} · message {}",
                                    receipt.target_session_id.as_deref().unwrap_or("-"),
                                    receipt.message_id,
                                ),
                            ))
                            .into_any_element()
                    }))
                    .when(task.earlier_records_pruned, |this| {
                        this.child(
                            div()
                                .text_size(sp(CAPTION_SIZE))
                                .text_color(muted)
                                .child("（更早的记录已被裁剪）"),
                        )
                    }),
            )
    }

    /// 详情页里的一行「键：值」。
    fn detail_line(&self, cx: &Context<Self>, key: &str, value: &str) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap_2()
            .text_size(sp(CAPTION_SIZE))
            .child(div().w(px(72.0)).text_color(muted).child(key.to_string()))
            .child(div().text_size(sp(BODY_SIZE)).child(value.to_string()))
    }
}
