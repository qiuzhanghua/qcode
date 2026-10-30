use super::*;

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
                    .child(format!("移除目录 @{label}？"))
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
