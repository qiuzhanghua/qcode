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
        let running = self.task_running(task_id);
        // 点击菜单外的任意位置（含其它任务行、空白区）关闭菜单；菜单内
        // 的按钮命中菜单 bounds，不会触发。
        let task_id_for_close = task_id.to_string();
        div()
            .id(format!("task-menu-popover-{task_id}"))
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
            .on_mouse_down_out(cx.listener(move |this, _, _window, cx| {
                // 点击「…」触发按钮时（所在行的 hover 仍指向本任务）不在此
                // 关闭，交给按钮的 on_click toggle；点击其它任意位置（其它
                // 任务行、空白区、主区域）则关闭菜单。
                if this.task_menu.as_deref() == Some(task_id_for_close.as_str())
                    && this.hovered_task.as_deref() != Some(task_id_for_close.as_str())
                {
                    this.task_menu = None;
                    cx.notify();
                }
            }))
            .child(
                Button::new(format!("task-stop-{task_id}"))
                    .label("停止")
                    .disabled(!running)
                    .tooltip(if running {
                        "停止这个任务（不影响其它任务）"
                    } else {
                        "这个任务没有在跑"
                    })
                    .on_click(cx.listener({
                        let task_id = task_id.to_string();
                        move |this, _, _window, cx| {
                            cx.stop_propagation();
                            this.task_menu = None;
                            this.stop_task(&task_id, cx);
                        }
                    })),
            )
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
        let pending = self.pending_roots.contains_key(&task_id);
        let mut section = div().flex().flex_col().gap_1().pl_4().pt_1();
        for root in roots {
            let label = root.label.clone();
            let path = root.path.display().to_string();
            let missing = root.state() == RootState::Missing;
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
                        .label(label.clone())
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
        } else if self.ui.turn_in_flight {
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
            // The row is the only place the window can show what a task that is
            // *not* on screen is doing (ADR-0058 §7). The active row keeps its
            // filled dot, since it is on screen and its state is right there.
            let state = self.task_state(&row.task.id);
            let marker = if active {
                "●"
            } else {
                match state {
                    TaskState::Running => "▶",
                    TaskState::WaitingApproval => "⚠",
                    TaskState::Unread => "◉",
                    TaskState::Idle => "○",
                }
            };
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
            } else if !active && state == TaskState::WaitingApproval {
                " · 待审批".to_string()
            } else if let Some(hint) = self.schedule_hints.get(&id) {
                format!(" · {hint}")
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
                    } else if this.hovered_task.as_deref() == Some(hover_id.as_str()) {
                        // 移出本行时清除，避免操作按钮残留在行尾；移到别的
                        // 行时由新行的进入事件接管，不在此处误清。
                        this.hovered_task = None;
                    }
                    cx.notify();
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
                    // 「任务操作」按钮；弹出菜单挂在 controls 容器上，锚定到
                    // 其左下角（TopLeft + 行高 30px），即按钮正下方。
                    div().relative().child(
                        Button::new(format!("task-menu-{}", row.task.id))
                            .compact()
                            .label("…")
                            .tooltip("任务操作")
                            // 常态/悬浮/按下背景都保持纯白不变色：Custom 常态
                            // 背景会被组件混入 0.2 透明度，这里再用调用层 bg
                            // 覆盖回纯白；文字固定深色以保证在白色上的可读性。
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .color(rgb(0xffffff).into())
                                    .hover(rgb(0xffffff).into())
                                    .active(rgb(0xffffff).into())
                                    .foreground(rgb(0x1a1a1a).into()),
                            )
                            .bg(rgb(0xffffff))
                            .text_color(rgb(0x1a1a1a))
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
                    ),
                )
                .child(
                    Button::new(format!("task-add-dir-{}", row.task.id))
                        .compact()
                        .label("+")
                        .tooltip("添加目录")
                        // 与「任务操作」按钮一致：三态背景纯白、文字深色、hover 不变色。
                        .custom(
                            ButtonCustomVariant::new(cx)
                                .color(rgb(0xffffff).into())
                                .hover(rgb(0xffffff).into())
                                .active(rgb(0xffffff).into())
                                .foreground(rgb(0x1a1a1a).into()),
                        )
                        .bg(rgb(0xffffff))
                        .text_color(rgb(0x1a1a1a))
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
                // 弹框挂回整行（锚点最稳），右对齐行右缘、顶部贴行底，
                // 任务操作按钮也在行右侧，视觉上即按钮正下方。
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

        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;

        // Three zones, not one scrolling column: the list is the only thing that may
        // scroll, so 我的任务 stays at the top and 终端/设置 stay at the bottom however
        // many tasks there are. Making the whole column scrollable (which this was)
        // pushed the bottom two out of view exactly when the list got long.
        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .id("task-sidebar")
            // Sidebar sits on the base `sidebar` token, one step lighter than
            // the row hover (`sidebar_accent` at reduced opacity), so the list
            // reads as a surface and the hovered row still stands out.
            .bg(cx.theme().sidebar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .child(
                        // 标题行不是按钮，但下面的「终端」「设置」两行是：`Button` 在
                        // `Size::Medium` 下把内容右推 `px_2p5`（button.rs 里
                        // `Size::Medium => this.h_8().px_2p5()`），所以这里补同一个
                        // `px_2p5`，图标左沿才与下面两行的图标落在同一条竖线上。
                        // 用同一个函数而不是写死 10px：两者都按 `rem_size` 缩放，
                        // 缩放之后仍然对齐。
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            // 与 `Button` 在 `Size::Medium` 下给图标和内容的间距一致。
                            .gap_2()
                            .pl_2p5()
                            .child(Icon::new(IconName::FolderKanban).with_size(Size::Medium))
                            .child("我的任务"),
                    )
                    .child(
                        Button::new("new-task")
                            .label("+")
                            .tooltip("新建任务")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_session(window, cx);
                            })),
                    ),
            )
            // The one scrolling zone. `min_h(0)` lets it shrink below its content;
            // without it the flex item refuses to and the footer is pushed off-screen.
            // `.id()` is required, not decorative: `overflow_y_scroll` lives on
            // `StatefulInteractiveElement`, which is keyed by id.
            .child(
                div()
                    .id("task-list")
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .overflow_y_scroll()
                    .children(rows),
            )
            // Fixed at the bottom, separated from the list by a hairline: above is
            // content, below is tools.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .pt_2()
                    .pb_2()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        Button::new("open-terminal")
                            .icon(IconName::SquareTerminal)
                            .accessibility_label("终端")
                            // 按钮内容默认居中（`h_flex().size_full().justify_center()`），
                            // 所以一个裸文字子元素会把图标拉到中间。撑成整行才让图标贴左，
                            // 与下面「设置」那行对齐——它本来就是因为徽标需要整行才左对齐的。
                            .child(div().w_full().text_size(sp(14.0)).child("终端"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_terminal_view(window, cx);
                            })),
                    )
                    .child(
                        Button::new("open-settings")
                            .icon(IconName::Settings)
                            // The badge says "this one opens a separate window":
                            // 终端 switches the main view, 设置 opens a window of its
                            // own. Side by side at the bottom, they would otherwise
                            // read as the same kind of thing.
                            .tooltip("设置（在新窗口中打开）")
                            .accessibility_label("设置，在新窗口中打开")
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    // 汉字比按钮默认字号（14px）大两档，随全局字号缩放联动。
                                    .child(div().text_size(sp(14.0)).child("设置"))
                                    .child(
                                        Icon::new(IconName::ExternalLink)
                                            .with_size(Size::Small)
                                            .text_color(muted),
                                    ),
                            )
                            .on_click(cx.listener(|this, _, _window, cx| this.open_settings(cx))),
                    ),
            )
    }
}
