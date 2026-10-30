use super::*;
pub(crate) fn task_titles(
    tasks: &[TaskRow],
    backend: &dyn Backend,
) -> std::collections::BTreeMap<String, String> {
    tasks
        .iter()
        .filter_map(|row| {
            let title = row
                .task
                .title
                .clone()
                .or_else(|| backend.title(&row.task.id));
            title.map(|title| (row.task.id.clone(), title))
        })
        .collect()
}

impl Shell {
    pub(crate) fn render_task_menu(
        &self,
        task_id: &str,
        title: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let running = self.active.as_deref() == Some(task_id) && self.turn_in_flight;
        div()
            .w(px(160.0))
            .flex()
            .flex_col()
            .gap_1()
            .rounded(px(8.0))
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .p_1()
            .text_color(theme.popover_foreground)
            .child(
                Button::new(format!("task-rename-{task_id}"))
                    .label("重命名")
                    .tooltip(if running {
                        "运行中改名将在 Turn 结束后生效"
                    } else {
                        "重命名任务"
                    })
                    .on_click(cx.listener({
                        let task_id = task_id.to_string();
                        move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open_rename_task(&task_id, window, cx);
                        }
                    })),
            )
            .child(
                Button::new(format!("task-delete-{task_id}"))
                    .label("删除任务")
                    .danger()
                    .disabled(running)
                    .tooltip(if running {
                        "结束当前 Turn 后可删除"
                    } else {
                        "删除任务"
                    })
                    .on_click(cx.listener({
                        let task_id = task_id.to_string();
                        let title = title.to_string();
                        move |this, _, _window, cx| {
                            cx.stop_propagation();
                            this.request_delete_task(&task_id, title.clone(), cx);
                        }
                    })),
            )
            .into_any_element()
    }
    pub(crate) fn render_task_roots(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.active_task_row() else {
            return div().into_any_element();
        };
        let task_id = row.task.id.clone();
        let roots = self.task_roots_for_edit(&task_id);
        let pending = self
            .pending_roots
            .as_ref()
            .is_some_and(|(id, _)| id == &task_id);
        let mut section = div().flex().flex_col().gap_1().pl_4().pt_1();
        for root in roots {
            let label = root.label.clone();
            let path = root.path.display().to_string();
            let missing = root.state() == RootState::Missing;
            let access = match root.access {
                RootAccess::ReadOnly => "RO",
                RootAccess::ReadWrite => "RW",
            };
            let primary = if root.primary { "★" } else { "☆" };
            let mut line = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .w_full()
                .h(px(24.0))
                .child(
                    Button::new(format!("root-alias-{task_id}-{label}"))
                        .text()
                        .compact()
                        .label(format!("@{label}"))
                        .tooltip(path.clone()),
                );
            if missing {
                line = line.child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(rgb(0xd06060))
                        .child("丢失"),
                );
            }
            let line = line
                .child(
                    Button::new(format!("root-access-{task_id}-{label}"))
                        .compact()
                        .label(access)
                        .tooltip("切换只读 / 读写")
                        .on_click({
                            let task_id = task_id.clone();
                            let label = label.clone();
                            cx.listener(move |this, _, _window, cx| {
                                this.toggle_task_root_access(&task_id, &label, cx);
                            })
                        }),
                )
                .child(
                    Button::new(format!("root-primary-{task_id}-{label}"))
                        .compact()
                        .label(primary)
                        .tooltip("设为主目录")
                        .on_click({
                            let task_id = task_id.clone();
                            let label = label.clone();
                            cx.listener(move |this, _, _window, cx| {
                                this.set_task_primary_root(&task_id, &label, cx);
                            })
                        }),
                )
                .child(
                    Button::new(format!("root-relink-{task_id}-{label}"))
                        .compact()
                        .label("↗")
                        .tooltip("重新关联目录")
                        .on_click({
                            let task_id = task_id.clone();
                            let label = label.clone();
                            cx.listener(move |this, _, _window, cx| {
                                this.relink_task_root(&task_id, &label, cx);
                            })
                        }),
                )
                .child(
                    Button::new(format!("root-remove-{task_id}-{label}"))
                        .compact()
                        .label("×")
                        .tooltip("移除目录")
                        .on_click({
                            let task_id = task_id.clone();
                            let label = label.clone();
                            cx.listener(move |this, _, _window, cx| {
                                this.request_remove_task_root(&task_id, &label, path.clone(), cx);
                            })
                        }),
                );
            section = section.child(line);
        }
        if pending {
            section = section.child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(rgb(0x888888))
                    .child("当前 Turn 结束后应用"),
            );
        } else if self.turn_in_flight {
            section = section.child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(rgb(0x888888))
                    .child("Turn 结束后应用"),
            );
        }
        section.into_any_element()
    }
    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let row_hover = cx.theme().sidebar_accent.opacity(0.55);
        let row_active = cx.theme().sidebar_accent;
        let mut rows: Vec<AnyElement> = Vec::new();
        for row in &self.tasks {
            let id = row.task.id.clone();
            let active = self.active.as_deref() == Some(row.task.id.as_str());
            let marker = if active { "●" } else { "○" };
            let full_title = self
                .titles
                .get(&row.task.id)
                .cloned()
                .unwrap_or_else(|| "新任务".to_string());
            let title = full_title.clone();
            // A task with a vanished directory is broken until it is relinked,
            // so say so in the row itself instead of hiding it behind the
            // expanded root editor.
            let missing = row
                .task
                .roots
                .iter()
                .filter(|root| root.state() == RootState::Missing)
                .count();
            let wonky = if missing > 0 {
                " · 目录丢失".to_string()
            } else {
                String::new()
            };
            let label = format!("{marker} {title}{wonky}");
            let menu_open = self.task_menu.as_deref() == Some(id.as_str());
            let show_actions = self.hovered_task.as_deref() == Some(id.as_str()) || menu_open;
            let hover_id = id.clone();
            let mut task_row = div()
                .relative()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .w_full()
                .h(px(30.0))
                .px_2()
                .rounded(px(6.0))
                .cursor_pointer()
                .when(active, |this| {
                    this.bg(row_active).font_weight(FontWeight::SEMIBOLD)
                })
                .when(!active, |this| this.hover(|style| style.bg(row_hover)))
                .when(!rows.is_empty(), |this| this.mt_2())
                .id(format!("task-row-{}", row.task.id))
                .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                    if *hovered {
                        this.hovered_task = Some(hover_id.clone());
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.task_menu = None;
                    this.resume(id.clone(), window, cx)
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .text_size(sp(BODY_SIZE))
                        .whitespace_nowrap()
                        .child(label),
                );
            let controls = div()
                .id(format!("task-actions-{}", row.task.id))
                .occlude()
                .absolute()
                .right_1()
                .top_0()
                .h(px(30.0))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .invisible()
                .when(show_actions, |this| this.visible())
                .child(
                    Button::new(format!("task-menu-{}", row.task.id))
                        .compact()
                        .label("…")
                        .tooltip("任务操作")
                        .on_click(cx.listener({
                            let task_id = row.task.id.clone();
                            move |this, _, _window, cx| {
                                cx.stop_propagation();
                                this.task_menu =
                                    if this.task_menu.as_deref() == Some(task_id.as_str()) {
                                        None
                                    } else {
                                        Some(task_id.clone())
                                    };
                                cx.notify();
                            }
                        })),
                )
                .child(
                    Button::new(format!("task-add-dir-{}", row.task.id))
                        .compact()
                        .label("+")
                        .tooltip("添加目录")
                        .on_click(cx.listener({
                            let task_id = row.task.id.clone();
                            move |this, _, _window, cx| {
                                cx.stop_propagation();
                                this.task_menu = None;
                                this.add_task_root(&task_id, cx);
                            }
                        })),
                );
            task_row = task_row.child(controls);
            if menu_open {
                task_row = task_row.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopRight)
                            .offset(point(px(0.0), px(30.0)))
                            .snap_to_window_with_margin(px(8.0))
                            .child(self.render_task_menu(&row.task.id, &full_title, cx)),
                    )
                    .with_priority(gpui_kit::base::POPUP_PRIORITY),
                );
            }
            rows.push(task_row.into_any_element());
            if active {
                rows.push(self.render_task_roots(cx));
            }
        }

        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .id("task-sidebar")
            // Sidebar sits on the base `sidebar` token, one step lighter than
            // the row hover (`sidebar_accent` at reduced opacity), so the list
            // reads as a surface and the hovered row still stands out.
            .bg(cx.theme().sidebar)
            .overflow_y_scroll()
            .child(
                Button::new("open-settings")
                    .label("Settings")
                    .on_click(cx.listener(|this, _, _window, cx| this.open_settings(cx))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child("我的任务")
                    .child(
                        Button::new("new-task")
                            .label("+")
                            .tooltip("新建任务")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_session(window, cx);
                            })),
                    ),
            )
            .children(rows)
    }
}
