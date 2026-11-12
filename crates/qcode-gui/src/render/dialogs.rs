use super::*;
use crate::shell::schedule::{ScheduleRuleKind, ScheduleTargetKind};

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
                    .child(format!("删除任务 “{title}”？"))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child(
                                "将删除本地任务记录和对话历史，不可恢复。不会删除目录中的文件。",
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("cancel-delete-task").label("取消").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.pending_task_delete = None;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("confirm-delete-task")
                                    .label("删除")
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
                    .child("重命名任务")
                    .child(Input::new(&input).w_full())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("cancel-rename-task").label("取消").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.rename_task = None;
                                    cx.notify();
                                }),
                            ))
                            .child(Button::new("confirm-rename-task").label("保存").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.save_rename_task(cx);
                                }),
                            )),
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

        // 周期：类型下拉的菜单项与选中项。
        let rule_items: Vec<(String, String)> = ScheduleRuleKind::ALL
            .iter()
            .map(|kind| (kind.as_stored().to_string(), kind.label().to_string()))
            .collect();
        // 星期下拉：0=周日，同 crontab。
        const WEEKDAY_LABELS: [&str; 7] = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
        let weekday_items: Vec<(String, String)> = (0u8..7)
            .map(|day| (day.to_string(), WEEKDAY_LABELS[day as usize].to_string()))
            .collect();
        let weekday_label = WEEKDAY_LABELS[state.weekday as usize].to_string();
        // 目标：类型下拉。
        let target_items: Vec<(String, String)> = ScheduleTargetKind::ALL
            .iter()
            .map(|kind| (kind.as_stored().to_string(), kind.label().to_string()))
            .collect();
        // 目标=指定会话时，会话下拉列出已有任务。
        let session_items: Vec<(String, String)> = self
            .tasks
            .iter()
            .map(|row| {
                let id = row.task.id.clone();
                let label = self
                    .titles
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| format!("会话 {id}"));
                (id, label)
            })
            .collect();
        let session_label = match state.session_id.as_deref() {
            Some(id) => self
                .titles
                .get(id)
                .cloned()
                .unwrap_or_else(|| format!("会话 {id}")),
            None => "选择会话…".to_string(),
        };
        // 周期类型对应的参数行。
        let param_field: AnyElement = match state.rule_kind {
            ScheduleRuleKind::Daily | ScheduleRuleKind::Weekly => self
                .form_field(cx, "时刻（HH:MM）", &state.time)
                .into_any_element(),
            ScheduleRuleKind::Interval => self
                .form_field(cx, "间隔秒数（≥ 60）", &state.interval_secs)
                .into_any_element(),
            ScheduleRuleKind::Once => self
                .form_field(cx, "触发时刻（YYYY-MM-DD HH:MM）", &state.once_at)
                .into_any_element(),
            ScheduleRuleKind::Cron => self
                .form_field(cx, "Cron 表达式（5 字段）", &state.cron_expr)
                .into_any_element(),
        };
        // 只有「每周」才出现的星期下拉、只有「指定会话」才出现的会话下拉。
        let weekday_field = (state.rule_kind == ScheduleRuleKind::Weekly).then(|| {
            self.form_select(
                cx,
                "星期",
                "schedule-weekday",
                weekday_label,
                state.weekday_menu_open,
                weekday_items,
                state.weekday.to_string(),
                |shell| {
                    if let Some(form) = shell.schedule_form.as_mut() {
                        form.weekday_menu_open = !form.weekday_menu_open;
                    }
                },
                |shell| {
                    if let Some(form) = shell.schedule_form.as_mut() {
                        form.weekday_menu_open = false;
                    }
                },
                |shell, value| {
                    if let Some(form) = shell.schedule_form.as_mut() {
                        if let Ok(day) = value.parse::<u8>() {
                            form.weekday = day % 7;
                        }
                        form.weekday_menu_open = false;
                    }
                },
            )
            .into_any_element()
        });
        let session_field = (state.target_kind == ScheduleTargetKind::Session).then(|| {
            self.form_select(
                cx,
                "目标会话",
                "schedule-session",
                session_label,
                state.session_menu_open,
                session_items,
                state.session_id.clone().unwrap_or_default(),
                |shell| {
                    if let Some(form) = shell.schedule_form.as_mut() {
                        form.session_menu_open = !form.session_menu_open;
                    }
                },
                |shell| {
                    if let Some(form) = shell.schedule_form.as_mut() {
                        form.session_menu_open = false;
                    }
                },
                |shell, value| {
                    if let Some(form) = shell.schedule_form.as_mut() {
                        form.session_id = Some(value);
                        form.session_menu_open = false;
                    }
                },
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
                            .child("手动创建定时任务"),
                    )
                    .child(self.form_field(cx, "名称", &state.title))
                    .child(self.form_field(cx, "内容", &state.prompt))
                    .child(self.form_select(
                        cx,
                        "周期",
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
                    .child(param_field)
                    .when_some(weekday_field, |this, field| this.child(field))
                    .child(self.form_select(
                        cx,
                        "目标",
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
                    .when_some(session_field, |this, field| this.child(field))
                    .child(self.form_field(cx, "审批超时(秒)", &state.timeout_secs))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("cancel-schedule-form").label("取消").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.schedule_form = None;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("confirm-schedule-form")
                                    .label("创建")
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
                                                        .text_size(sp(13.0))
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
                    .child(format!("移除目录 {label}？"))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child(format!("不会删除 {path} 中的文件，只会从当前任务中移除。")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("cancel-remove-root").label("取消").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.pending_root_delete = None;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("confirm-remove-root")
                                    .label("移除")
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
                    .child("退出 QCode？")
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child("当前任务仍在运行，退出会中断它。"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-quit").label("取消").on_click(
                                    cx.listener(|this, _, _window, cx| this.cancel_quit(cx)),
                                ),
                            )
                            .child(Button::new("confirm-quit").label("退出").danger().on_click(
                                cx.listener(|this, _, _window, cx| this.confirm_quit(cx)),
                            )),
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
                    .child(format!("粘贴 {lines} 行？"))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child("终端没有开启 bracketed paste，粘贴的换行会被立即执行。"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("terminal-paste-cancel").label("取消").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.pending_terminal_paste = None;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("terminal-paste-ok")
                                    .label("粘贴")
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
