use super::*;
use crate::platform::pick_directory;
use crate::shell::schedule::{ScheduleRuleKind, ScheduleTargetKind};
use gpui_kit::component::date_picker::DatePicker;
use gpui_kit::component::time_field::TimeField;

impl Shell {
    pub(crate) fn render_task_delete_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some((task_id, title)) = self.pending_task_delete.clone() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(440.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .p_4()
                    .text_color(theme.popover_foreground)
                    .child(qcode_i18n::trp(
                        "gui.schedule.dialog.delete_task.title",
                        &[&title],
                    ))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child(qcode_i18n::tr("gui.schedule.dialog.delete_task.body")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-delete-task")
                                    .label(qcode_i18n::tr("gui.schedule.button.cancel"))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.pending_task_delete = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("confirm-delete-task")
                                    .label(qcode_i18n::tr("gui.schedule.button.delete"))
                                    .danger()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.pending_task_delete = None;
                                        this.delete_task_now(&task_id, window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
    pub(crate) fn render_rename_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = &self.rename_task else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let input = state.input.clone();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .p_4()
                    .text_color(theme.popover_foreground)
                    .child(qcode_i18n::tr("gui.schedule.dialog.rename.title"))
                    .child(Input::new(&input).w_full())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-rename-task")
                                    .label(qcode_i18n::tr("gui.schedule.button.cancel"))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.rename_task = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("confirm-rename-task")
                                    .label(qcode_i18n::tr("gui.schedule.button.save"))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.save_rename_task(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 手动创建定时任务的表单弹窗：名称 / 内容 / 周期 / 目标 / 审批超时。
    ///
    /// 周期与目标不再是手写表达式，而是下拉菜单：周期先选类型（每天 / 每周 /
    /// 间隔 / 一次性 / 自定义 Cron），再填该类型自己的参数；目标选「新会话
    /// （复用 / 每次新建）」或「指定会话」，选会话时再下拉挑一个已有会话。
    pub(crate) fn render_schedule_form_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = &self.schedule_form else {
            return div().into_any_element();
        };
        // 立刻解构为主题值（都是 Copy），结束对 `*cx` 的借用：后面 `form_select`
        // 要拿 `&mut cx`，若还挂着 theme 的借用会冲突。
        let popover = cx.theme().popover;
        let border = cx.theme().border;
        let popover_foreground = cx.theme().popover_foreground;
        let accent = cx.theme().tokens.accent;
        let accent_foreground = cx.theme().accent_foreground;

        // 周期：类型下拉的菜单项与选中项。
        let rule_items: Vec<(String, String)> = ScheduleRuleKind::ALL
            .iter()
            .map(|kind| (kind.as_stored().to_string(), kind.label().to_string()))
            .collect();
        // 星期：0=周日，同 crontab。多选 pill 组按周一起始排列。
        let weekday_labels = weekday_labels();
        // 目标：类型下拉。
        let target_items: Vec<(String, String)> = ScheduleTargetKind::ALL
            .iter()
            .map(|kind| (kind.as_stored().to_string(), kind.label().to_string()))
            .collect();
        // 周期类型对应的参数行。
        let param_field: AnyElement = match state.rule_kind {
            ScheduleRuleKind::Daily | ScheduleRuleKind::Weekly => div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(cx.theme().muted_foreground)
                        .child(qcode_i18n::tr("gui.schedule.form.field.time")),
                )
                .child(TimeField::new(&state.time).w_full())
                .into_any_element(),
            ScheduleRuleKind::Monthly => {
                let day = state.monthly_day.clone();
                let time = state.time.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(self.form_field(
                        cx,
                        &qcode_i18n::tr("gui.schedule.form.field.monthly_day"),
                        &day,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(sp(CAPTION_SIZE))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(qcode_i18n::tr("gui.schedule.form.field.time")),
                            )
                            .child(TimeField::new(&time).w_full()),
                    )
                    .into_any_element()
            }
            ScheduleRuleKind::Interval => self
                .form_field(
                    cx,
                    &qcode_i18n::tr("gui.schedule.form.field.interval_secs"),
                    &state.interval_secs,
                )
                .into_any_element(),
            ScheduleRuleKind::Once => {
                let picker = state.once_picker.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(cx.theme().muted_foreground)
                            .child(qcode_i18n::tr("gui.schedule.form.field.once_at")),
                    )
                    .child(DatePicker::new(&picker).w_full())
                    .into_any_element()
            }
            ScheduleRuleKind::Cron => self
                .form_field(
                    cx,
                    &qcode_i18n::tr("gui.schedule.form.field.cron"),
                    &state.cron_expr,
                )
                .into_any_element(),
        };
        // 只有「每周」才出现的星期多选（pill 组）；只有「指定会话」才出现的
        // 会话下拉。
        let weekday_pills = (state.rule_kind == ScheduleRuleKind::Weekly).then(|| {
            let weekdays = state.weekdays.clone();
            let shell = cx.entity().downgrade();
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(cx.theme().muted_foreground)
                        .child(qcode_i18n::tr("gui.schedule.form.field.weekdays")),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .children(
                            // 显示顺序：周一..周日（1..7，0=周日）。
                            [1u8, 2, 3, 4, 5, 6, 0].into_iter().map({
                                let shell = shell.clone();
                                let weekdays = weekdays.clone();
                                move |day| {
                                    let selected = weekdays.contains(&day);
                                    let shell = shell.clone();
                                    div()
                                        .id(format!("schedule-weekday-pill-{day}"))
                                        .flex_1()
                                        .h(px(30.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(6.0))
                                        .text_size(sp(12.0))
                                        .when(selected, |this| {
                                            this.bg(accent).text_color(accent_foreground)
                                        })
                                        .on_click(move |_event, _window, cx| {
                                            if let Some(shell) = shell.upgrade() {
                                                shell.update(cx, |shell, cx| {
                                                    if let Some(form) = shell.schedule_form.as_mut()
                                                    {
                                                        if form.weekdays.contains(&day) {
                                                            form.weekdays.retain(|d| d != &day);
                                                        } else {
                                                            form.weekdays.push(day);
                                                        }
                                                    }
                                                    cx.notify();
                                                });
                                            }
                                        })
                                        .child(weekday_labels[day as usize].clone())
                                }
                            }),
                        )
                        .into_any_element(),
                )
                .into_any_element()
        });

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| {
                    // 点击遮罩（弹窗之外的任意位置）关闭表单。
                    this.schedule_form = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(460.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(popover)
                    .border_1()
                    .border_color(border)
                    .p_4()
                    .text_color(popover_foreground)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _event, _window, cx| {
                            // 点击弹窗内部不冒泡到遮罩，避免误关。
                            cx.stop_propagation();
                        }),
                    )
                    .child(
                        div()
                            .text_size(sp(16.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if state.editing.is_some() {
                                qcode_i18n::tr("gui.schedule.form.title.edit")
                            } else {
                                qcode_i18n::tr("gui.schedule.form.title.create")
                            }),
                    )
                    .child(self.form_field(
                        cx,
                        &qcode_i18n::tr("gui.schedule.form.field.name"),
                        &state.title,
                    ))
                    .child(self.form_field(
                        cx,
                        &qcode_i18n::tr("gui.schedule.form.field.prompt"),
                        &state.prompt,
                    ))
                    .child(self.workdir_field(cx))
                    .child(self.form_select(
                        cx,
                        &qcode_i18n::tr("gui.schedule.form.field.rule"),
                        "schedule-rule-kind",
                        state.rule_kind.label().to_string(),
                        state.rule_menu_open,
                        rule_items,
                        state.rule_kind.as_stored().to_string(),
                        |shell| {
                            if let Some(form) = shell.schedule_form.as_mut() {
                                form.rule_menu_open = !form.rule_menu_open;
                            }
                        },
                        |shell| {
                            if let Some(form) = shell.schedule_form.as_mut() {
                                form.rule_menu_open = false;
                            }
                        },
                        |shell, value| {
                            if let Some(form) = shell.schedule_form.as_mut() {
                                form.rule_kind = ScheduleRuleKind::from_stored(&value);
                                form.rule_menu_open = false;
                            }
                        },
                    ))
                    .when_some(weekday_pills, |this, field| this.child(field))
                    .child(param_field)
                    .child(self.form_select(
                        cx,
                        &qcode_i18n::tr("gui.schedule.form.field.target"),
                        "schedule-target-kind",
                        state.target_kind.label().to_string(),
                        state.target_menu_open,
                        target_items,
                        state.target_kind.as_stored().to_string(),
                        |shell| {
                            if let Some(form) = shell.schedule_form.as_mut() {
                                form.target_menu_open = !form.target_menu_open;
                            }
                        },
                        |shell| {
                            if let Some(form) = shell.schedule_form.as_mut() {
                                form.target_menu_open = false;
                            }
                        },
                        |shell, value| {
                            if let Some(form) = shell.schedule_form.as_mut() {
                                form.target_kind = ScheduleTargetKind::from_stored(&value);
                                form.target_menu_open = false;
                            }
                        },
                    ))
                    .child(self.form_field(
                        cx,
                        &qcode_i18n::tr("gui.schedule.form.field.approval_timeout"),
                        &state.timeout_secs,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-schedule-form")
                                    .label(qcode_i18n::tr("gui.schedule.button.cancel"))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.schedule_form = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("confirm-schedule-form")
                                    .label(if state.editing.is_some() {
                                        qcode_i18n::tr("gui.schedule.button.save")
                                    } else {
                                        qcode_i18n::tr("gui.schedule.button.create")
                                    })
                                    .primary()
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.submit_schedule_form(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 表单里的一行下拉选择：标签 + 触发按钮 +（展开时）锚定的菜单弹层。
    ///
    /// 与侧栏 ⋮ 菜单同一套弹层方案（`deferred(anchored())` + `POPUP_PRIORITY`）：
    /// 从触发按钮下方展开、点菜单外任意位置关闭；菜单项带当前选中的高亮勾选。
    /// 变更经 `toggle` / `close` / `pick` 三个纯闭包写回 `Shell`（弹层在独立
    /// 层里，不借用 `cx` 的 entity）。
    #[allow(clippy::too_many_arguments)]
    fn form_select(
        &self,
        cx: &mut Context<Self>,
        label: &str,
        id: &'static str,
        current: String,
        open: bool,
        items: Vec<(String, String)>,
        checked: String,
        toggle: impl Fn(&mut Shell) + 'static,
        close: impl Fn(&mut Shell) + 'static,
        pick: impl Fn(&mut Shell, String) + 'static,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let popover = theme.popover;
        let border = theme.border;
        let popover_foreground = theme.popover_foreground;
        let accent = theme.tokens.accent;
        let accent_foreground = theme.accent_foreground;
        let shell = cx.entity().downgrade();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(theme.muted_foreground)
                    .child(label.to_string()),
            )
            .child(
                div().relative().child(
                    Button::new(id)
                        .w_full()
                        .label(current)
                        .dropdown_caret(true)
                        .on_click({
                            let shell = shell.clone();
                            move |_event, _window, cx| {
                                if let Some(shell) = shell.upgrade() {
                                    shell.update(cx, |shell, cx| {
                                        toggle(shell);
                                        cx.notify();
                                    });
                                }
                            }
                        }),
                ),
            )
            .when(open, |this| {
                this.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.0), px(30.0)))
                            .snap_to_window_with_margin(px(8.0))
                            .child(
                                div()
                                    .id(format!("{id}-popover"))
                                    .w(px(240.0))
                                    .max_h(px(240.0))
                                    .overflow_y_scroll()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .p_1()
                                    .rounded(px(8.0))
                                    .bg(popover)
                                    .border_1()
                                    .border_color(border)
                                    .text_color(popover_foreground)
                                    .on_mouse_down_out({
                                        let shell = shell.clone();
                                        move |_event, _window, cx| {
                                            if let Some(shell) = shell.upgrade() {
                                                shell.update(cx, |shell, cx| {
                                                    close(shell);
                                                    cx.notify();
                                                });
                                            }
                                        }
                                    })
                                    .children(items.into_iter().map({
                                        // `pick` 会被每个菜单项闭包各自调用，放进
                                        // Rc 让 FnMut 的 map 闭包可以克隆它。
                                        let shell = shell.clone();
                                        let pick = std::rc::Rc::new(pick);
                                        move |(value, item_label)| {
                                            let selected = checked == value;
                                            let shell = shell.clone();
                                            let pick = pick.clone();
                                            let value = value.clone();
                                            let item_label = item_label.clone();
                                            div()
                                                .id(value.clone())
                                                .h(px(30.0))
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap_2()
                                                .px_2()
                                                .rounded(px(6.0))
                                                .when(selected, |this| {
                                                    this.bg(accent).text_color(accent_foreground)
                                                })
                                                .on_click(move |_event, _window, cx| {
                                                    if let Some(shell) = shell.upgrade() {
                                                        shell.update(cx, |shell, cx| {
                                                            pick(shell, value.clone());
                                                            cx.notify();
                                                        });
                                                    }
                                                })
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .text_size(sp(12.0))
                                                        .child(item_label),
                                                )
                                                .when(selected, |this| {
                                                    this.child(Icon::new(IconName::Check).xsmall())
                                                })
                                                .into_any_element()
                                        }
                                    })),
                            ),
                    )
                    .with_priority(gpui_kit::base::POPUP_PRIORITY),
                )
            })
    }

    /// 表单里的一行：标签 + 输入框。
    fn form_field(
        &self,
        cx: &Context<Self>,
        label: &str,
        input: &Entity<InputState>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(cx.theme().muted_foreground)
                    .child(label.to_string()),
            )
            .child(Input::new(input).w_full())
    }

    /// 提问面板里「周期」题的结构化选择器：类型下拉 + 各自参数，与定时任务
    /// 编辑表单的周期区同构。选择结果在提交时由 `RulePickerState::rule_text`
    /// 拼成规范表达式回填给 `schedule_create` 工具。
    pub(crate) fn render_question_rule_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let accent = theme.tokens.accent;
        let accent_foreground = theme.accent_foreground;
        let muted_foreground = theme.muted_foreground;
        let Some(panel) = self.ui.question.as_ref() else {
            return div().into_any_element();
        };
        let Some(rule) = panel.rule.as_ref() else {
            return div().into_any_element();
        };
        let rule_items: Vec<(String, String)> = ScheduleRuleKind::ALL
            .iter()
            .map(|kind| (kind.as_stored().to_string(), kind.label().to_string()))
            .collect();
        // 星期：0=周日，同 crontab。多选 pill 组按周一起始排列。
        let weekday_labels = weekday_labels();
        let shell = cx.entity().downgrade();

        // 周期类型对应的参数行。
        let param_field: AnyElement = match rule.kind {
            ScheduleRuleKind::Daily | ScheduleRuleKind::Weekly => div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(muted_foreground)
                        .child(qcode_i18n::tr("gui.schedule.form.field.time")),
                )
                .child(TimeField::new(&rule.time).w_full())
                .into_any_element(),
            ScheduleRuleKind::Monthly => {
                let day = rule.monthly_day.clone();
                let time = rule.time.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(self.form_field(
                        cx,
                        &qcode_i18n::tr("gui.schedule.form.field.monthly_day"),
                        &day,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(sp(CAPTION_SIZE))
                                    .text_color(muted_foreground)
                                    .child(qcode_i18n::tr("gui.schedule.form.field.time")),
                            )
                            .child(TimeField::new(&time).w_full()),
                    )
                    .into_any_element()
            }
            ScheduleRuleKind::Interval => self
                .form_field(
                    cx,
                    &qcode_i18n::tr("gui.schedule.form.field.interval_secs"),
                    &rule.interval_secs,
                )
                .into_any_element(),
            ScheduleRuleKind::Once => {
                let picker = rule.once_picker.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(muted_foreground)
                            .child(qcode_i18n::tr("gui.schedule.form.field.once_at")),
                    )
                    .child(DatePicker::new(&picker).w_full())
                    .into_any_element()
            }
            ScheduleRuleKind::Cron => self
                .form_field(
                    cx,
                    &qcode_i18n::tr("gui.schedule.form.field.cron"),
                    &rule.cron_expr,
                )
                .into_any_element(),
        };

        // 只有「每周」才出现的星期多选（pill 组）。
        let weekday_pills = (rule.kind == ScheduleRuleKind::Weekly).then(|| {
            let weekdays = rule.weekdays.clone();
            let shell = shell.clone();
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(muted_foreground)
                        .child(qcode_i18n::tr("gui.schedule.form.field.weekdays")),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .children(
                            // 显示顺序：周一..周日（1..7，0=周日）。
                            [1u8, 2, 3, 4, 5, 6, 0].into_iter().map({
                                let shell = shell.clone();
                                let weekdays = weekdays.clone();
                                move |day| {
                                    let selected = weekdays.contains(&day);
                                    let shell = shell.clone();
                                    div()
                                        .id(format!("question-rule-weekday-{day}"))
                                        .flex_1()
                                        .h(px(30.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(6.0))
                                        .text_size(sp(12.0))
                                        .when(selected, |this| {
                                            this.bg(accent).text_color(accent_foreground)
                                        })
                                        .on_click(move |_event, _window, cx| {
                                            if let Some(shell) = shell.upgrade() {
                                                shell.update(cx, |shell, cx| {
                                                    if let Some(panel) = shell.ui.question.as_mut()
                                                        && let Some(rule) = panel.rule.as_mut()
                                                    {
                                                        if rule.weekdays.contains(&day) {
                                                            rule.weekdays.retain(|d| d != &day);
                                                        } else {
                                                            rule.weekdays.push(day);
                                                        }
                                                    }
                                                    cx.notify();
                                                });
                                            }
                                        })
                                        .child(weekday_labels[day as usize].clone())
                                        .into_any_element()
                                }
                            }),
                        )
                        .into_any_element(),
                )
                .into_any_element()
        });

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.form_select(
                cx,
                &qcode_i18n::tr("gui.schedule.form.field.rule"),
                "question-rule-kind",
                rule.kind.label().to_string(),
                rule.rule_menu_open,
                rule_items,
                rule.kind.as_stored().to_string(),
                |shell| {
                    if let Some(panel) = shell.ui.question.as_mut()
                        && let Some(rule) = panel.rule.as_mut()
                    {
                        rule.rule_menu_open = !rule.rule_menu_open;
                    }
                },
                |shell| {
                    if let Some(panel) = shell.ui.question.as_mut()
                        && let Some(rule) = panel.rule.as_mut()
                    {
                        rule.rule_menu_open = false;
                    }
                },
                |shell, value| {
                    if let Some(panel) = shell.ui.question.as_mut()
                        && let Some(rule) = panel.rule.as_mut()
                    {
                        rule.kind = ScheduleRuleKind::from_stored(&value);
                        rule.rule_menu_open = false;
                    }
                },
            ))
            .when_some(weekday_pills, |this, field| this.child(field))
            .child(param_field)
            .into_any_element()
    }

    /// 定时任务表单的工作目录行：显示当前值（或「未指定」）+「选择目录」按钮，
    /// 点击弹出系统目录选择对话框（`pick_directory`）；已选时多一个「清除」。
    fn workdir_field(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let popover = theme.popover;
        let border = theme.border;
        let muted_foreground = theme.muted_foreground;
        let shell = cx.entity().downgrade();
        let Some(state) = self.schedule_form.as_ref() else {
            return div();
        };
        let cwd = state.cwd.clone();
        let cwd_label = cwd
            .clone()
            .unwrap_or_else(|| qcode_i18n::tr("gui.schedule.form.workdir_unset").into_owned());
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(muted_foreground)
                    .child(qcode_i18n::tr("gui.schedule.form.field.workdir")),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .h(px(30.0))
                            .flex()
                            .items_center()
                            .px_2()
                            .rounded(px(6.0))
                            .bg(popover)
                            .border_1()
                            .border_color(border)
                            .text_size(sp(12.0))
                            .child(cwd_label),
                    )
                    .child(
                        Button::new("schedule-choose-cwd")
                            .label(qcode_i18n::tr("gui.schedule.button.choose_directory"))
                            .on_click({
                                let shell = shell.clone();
                                move |_event, _window, cx| match pick_directory() {
                                    Ok(Some(path)) => {
                                        if let Some(shell) = shell.upgrade() {
                                            shell.update(cx, |shell, cx| {
                                                if let Some(form) = shell.schedule_form.as_mut() {
                                                    form.cwd = Some(path.display().to_string());
                                                }
                                                cx.notify();
                                            });
                                        }
                                    }
                                    // 用户在对话框里取消了：什么都不改。
                                    Ok(None) => {}
                                    Err(error) => {
                                        if let Some(shell) = shell.upgrade() {
                                            shell.update(cx, |shell, cx| {
                                                shell.ui.app.push_notice(error);
                                                cx.notify();
                                            });
                                        }
                                    }
                                }
                            }),
                    )
                    .when(cwd.is_some(), |this| {
                        this.child(
                            Button::new("schedule-clear-cwd")
                                .label(qcode_i18n::tr("gui.schedule.button.clear"))
                                .on_click({
                                    let shell = shell.clone();
                                    move |_event, _window, cx| {
                                        if let Some(shell) = shell.upgrade() {
                                            shell.update(cx, |shell, cx| {
                                                if let Some(form) = shell.schedule_form.as_mut() {
                                                    form.cwd = None;
                                                }
                                                cx.notify();
                                            });
                                        }
                                    }
                                }),
                        )
                    }),
            )
    }
    pub(crate) fn render_delete_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some((task_id, label, path)) = self.pending_root_delete.clone() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .p_4()
                    .text_color(theme.popover_foreground)
                    .child(qcode_i18n::trp(
                        "gui.schedule.dialog.remove_root.title",
                        &[&label],
                    ))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child(qcode_i18n::trp(
                                "gui.schedule.dialog.remove_root.body",
                                &[&path],
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-remove-root")
                                    .label(qcode_i18n::tr("gui.schedule.button.cancel"))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.pending_root_delete = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("confirm-remove-root")
                                    .label(qcode_i18n::tr("gui.schedule.button.remove"))
                                    .danger()
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.pending_root_delete = None;
                                        this.remove_task_root_now(&task_id, &label, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// 退出确认：只在任务运行中或等待审批时出现。
    pub(crate) fn render_quit_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.pending_quit {
            return div().into_any_element();
        }
        let theme = cx.theme();
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(440.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .p_4()
                    .text_color(theme.popover_foreground)
                    .child(qcode_i18n::tr("gui.schedule.dialog.quit.title"))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child(qcode_i18n::tr("gui.schedule.dialog.quit.body")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-quit")
                                    .label(qcode_i18n::tr("gui.schedule.button.cancel"))
                                    .on_click(
                                        cx.listener(|this, _, _window, cx| this.cancel_quit(cx)),
                                    ),
                            )
                            .child(
                                Button::new("confirm-quit")
                                    .label(qcode_i18n::tr("gui.schedule.button.quit"))
                                    .danger()
                                    .on_click(
                                        cx.listener(|this, _, _window, cx| this.confirm_quit(cx)),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Shell {
    /// A paste that would run more than one command asks first (ADR-0062 §5).
    pub(crate) fn render_terminal_paste_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some((_, lines)) = self.pending_terminal_paste.clone() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .p_4()
                    .text_color(theme.popover_foreground)
                    .child(qcode_i18n::trp(
                        "gui.schedule.dialog.paste.title",
                        &[lines.to_string()],
                    ))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child(qcode_i18n::tr("gui.schedule.dialog.paste.body")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("terminal-paste-cancel")
                                    .label(qcode_i18n::tr("gui.schedule.button.cancel"))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.pending_terminal_paste = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("terminal-paste-ok")
                                    .label(qcode_i18n::tr("gui.schedule.button.paste"))
                                    .primary()
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.confirm_terminal_paste(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

/// 七天的短名。**函数而不是 const**：`tr` 是运行期查表，`const` 里调不了。
fn weekday_labels() -> [std::borrow::Cow<'static, str>; 7] {
    [
        qcode_i18n::tr("gui.schedule.weekday.sun"),
        qcode_i18n::tr("gui.schedule.weekday.mon"),
        qcode_i18n::tr("gui.schedule.weekday.tue"),
        qcode_i18n::tr("gui.schedule.weekday.wed"),
        qcode_i18n::tr("gui.schedule.weekday.thu"),
        qcode_i18n::tr("gui.schedule.weekday.fri"),
        qcode_i18n::tr("gui.schedule.weekday.sat"),
    ]
}
