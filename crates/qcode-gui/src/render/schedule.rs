//! The schedule board: a read-only view of `[schedules.<name>]` (ADR-0063 §5).
//!
//! Read-only is a decision, not a shortcut. Every edit would have to write
//! `config.toml`, and `reload_on_config_change` turns a config change into a
//! backend rebuild plus a `resume` — so "edit from the GUI" means "rebuild the
//! world on every keystroke's commit", and it races the settings window's
//! writes on the same file. ADR-0059 §1 already ruled that state must never be
//! written back to config; the same reasoning keeps edits out of this view.
//!
//! The data comes from `Backend::schedules()`, cached in `Shell::schedule_rows`
//! and refreshed with the task list — never per frame, since it reads config and
//! the store.

use super::*;

/// Relative "when will this fire", computed at render time.
///
/// Computed here rather than stored, so "3 分钟后" counts down on its own without
/// a refresh (ADR-0063 §8). A past instant is a missed window, not a bug: say so
/// instead of showing a negative countdown.
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

impl Shell {
    /// The content area's schedule board (ADR-0063 §5).
    pub(crate) fn render_schedule_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;
        let secondary = cx.theme().secondary;

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
                        "在 config.toml 的 [schedules.<名字>] 里声明。这里只读，不做增删改。",
                    ),
                ),
            )
            .child(
                div()
                    .id("schedule-board")
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px(px(MAIN_PAD_L))
                    .pt_4()
                    .pb_4()
                    .overflow_y_scroll()
                    .when(self.schedule_rows.is_empty(), |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(sp(BODY_SIZE))
                                        .text_color(foreground)
                                        .child("还没有定时任务"),
                                )
                                .child(
                                    div().text_size(sp(CAPTION_SIZE)).text_color(muted).child(
                                        format!(
                                            "在 {} 里加一段 [schedules.<名字>]，例如 cron = \"0 3 * * *\"。",
                                            self.config_path.display()
                                        ),
                                    ),
                                ),
                        )
                    })
                    .children(
                        self.schedule_rows
                            .iter()
                            .map(|row| self.render_schedule_card(row, cx)),
                    ),
            )
            .child(
                div()
                    .px(px(MAIN_PAD_L))
                    .pb_3()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(secondary)
                    .child(format!("共 {} 条", self.schedule_rows.len())),
            )
    }

    fn render_schedule_card(&self, row: &ScheduleRow, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let now = std::time::SystemTime::now();
        let now = {
            // `PrimitiveDateTime` is what the schedule module uses; the crate has
            // no `now_local`, so convert through the local offset when the
            // platform reports one and fall back to UTC (display only).
            let utc = time::OffsetDateTime::from(now);
            time::PrimitiveDateTime::new(utc.date(), utc.time())
        };

        // The task column: a link when the entry points at a task, otherwise the
        // honest answer for `new = true` (ADR-0063 §5).
        let target_task = row.task.clone();
        let target_label = match &row.task {
            Some(id) => self
                .titles
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.chars().take(8).collect()),
            None => "每次新建任务".to_string(),
        };

        div()
            .id(format!("schedule-card-{}", row.name))
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .rounded(px(8.0))
            .border_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_size(sp(BODY_SIZE))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(row.name.clone()),
                    )
                    .child(
                        // The cron string verbatim: the user wrote it, and
                        // translating it to prose loses the thing they edit.
                        div()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(muted)
                            .child(row.cron.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(muted)
                    .child(format!("下次 {}", format_next_fire(row.next_fire_at, now)))
                    .child(match &row.task {
                        Some(_) => format!("任务 {target_label}"),
                        None => target_label,
                    })
                    .when(row.missed_windows > 0, |this| {
                        this.child(format!("错过 {} 次", row.missed_windows))
                    })
                    .when_some(row.status.as_ref(), |this, status| {
                        this.child(schedule_status_label(status))
                    }),
            )
            .when_some(row.hint(), |this, hint| {
                // Same string the task row would show (ADR-0059 §3), so the two
                // places can never disagree about what went wrong.
                this.child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(cx.theme().danger)
                        .child(hint),
                )
            })
            .when_some(target_task, |this, id| {
                this.child(
                    Button::new(format!("schedule-open-{}", row.name))
                        .label("打开任务")
                        .tooltip("切到这个任务")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_sidebar_section(SidebarSection::Tasks, cx);
                            this.resume(id.clone(), window, cx);
                        })),
                )
            })
            .into_any_element()
    }
}

fn schedule_status_label(status: &qcode_core::schedule::ScheduleStatus) -> String {
    use qcode_core::schedule::ScheduleStatus;
    match status {
        ScheduleStatus::Orphaned => "失效".to_string(),
        ScheduleStatus::Missed(_) => "错过".to_string(),
        ScheduleStatus::Failed(_) => "失败".to_string(),
        other => format!("{other:?}"),
    }
}
