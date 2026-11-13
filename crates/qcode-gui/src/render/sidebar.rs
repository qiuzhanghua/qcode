use super::*;

/// The app mark, as bytes. Same file the running row uses: it is already
/// `currentColor`, so the theme drives it and there is no second copy of the
/// artwork to keep in sync.
const QC_MARK: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../qcode/res/icons/src/qcode-mark-current.svg"
));

/// The product label in the sidebar header.
///
/// Deliberately not the binary name (`qc`) or the crate name (`qcode`): the
/// Chinese labels around it — 我的任务 / 定时任务 / 终端 / 设置 — are feature
/// names, and this one is the product's. A placeholder until a real name exists;
/// renaming it is this one string (ADR-0063 §2).
const PRODUCT_NAME: &str = "AI 工具";

/// 定时任务列表的状态过滤选项（`None` = 全部）。
const SCHEDULE_FILTERS: [Option<qcode_core::schedule::TaskStatus>; 4] = [
    None,
    Some(qcode_core::schedule::TaskStatus::Active),
    Some(qcode_core::schedule::TaskStatus::Paused),
    Some(qcode_core::schedule::TaskStatus::Ended),
];

/// 过滤条件的中文标签。
fn schedule_filter_label(filter: Option<qcode_core::schedule::TaskStatus>) -> &'static str {
    match filter {
        None => "全部",
        Some(qcode_core::schedule::TaskStatus::Active) => "已开启",
        Some(qcode_core::schedule::TaskStatus::Paused) => "已暂停",
        Some(qcode_core::schedule::TaskStatus::Ended) => "已结束",
    }
}

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
            .w(px(120.0))
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
                    // 菜单项无边框：常态透明，悬浮/按下灰色底；字重保持常规。
                    .font_weight(FontWeight::NORMAL)
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(cx.theme().transparent)
                            .hover(rgb(0x888888).opacity(0.2).into())
                            .active(rgb(0x888888).opacity(0.45).into())
                            .foreground(cx.theme().popover_foreground),
                    )
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
                    // 菜单项无边框：常态透明，悬浮/按下灰色底；字重保持常规。
                    .font_weight(FontWeight::NORMAL)
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(cx.theme().transparent)
                            .hover(rgb(0x888888).opacity(0.2).into())
                            .active(rgb(0x888888).opacity(0.45).into())
                            .foreground(cx.theme().popover_foreground),
                    )
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
                    .disabled(running)
                    .tooltip(if running {
                        "结束当前 Turn 后可删除"
                    } else {
                        "删除任务"
                    })
                    // 菜单项无边框：常态透明，悬浮/按下灰色底；字重保持常规，
                    // 文字用危险红。
                    .font_weight(FontWeight::NORMAL)
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(cx.theme().transparent)
                            .hover(rgb(0x888888).opacity(0.2).into())
                            .active(rgb(0x888888).opacity(0.45).into())
                            .foreground(cx.theme().danger),
                    )
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

    /// The expanded sidebar: header, two independently folding sections, footer
    /// (ADR-0063). Each section header toggles only its own section: both can be
    /// open at once, both can be collapsed.
    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let tasks_open = self.tasks_section_open;

        // Three fixed zones around one scrolling one: the header never moves, the
        // two section headers never move, and only the open section's list scrolls.
        // Making the whole column scrollable (which this was before v0.8.0) pushed
        // the bottom rows out of view exactly when the list got long.
        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .id("task-sidebar")
            // Sidebar sits on the base `sidebar` token, one step lighter than the
            // row hover (`sidebar_accent` at reduced opacity), so the list reads as
            // a surface and the hovered row still stands out.
            .bg(cx.theme().sidebar)
            .child(self.render_sidebar_header(cx))
            .child(self.render_section_header(
                SidebarSection::Tasks,
                "我的任务",
                Some((
                    "new-task",
                    "+",
                    "新建任务",
                    // 新建任务只从展开态的段头进入：图标轨里没有 `+`
                    // （ADR-0063 §1、§4）。A2 的代价就是这个，用户已确认。
                    SidebarAction::NewTask,
                )),
                tasks_open,
                cx,
            ))
            .when(tasks_open, |this| this.child(self.render_task_list(cx)))
            .child(self.render_schedule_section_header(cx))
            .when(self.schedule_section_open, |this| {
                this.child(self.render_schedule_list(cx))
            })
            // Fixed at the bottom, separated from the list by a hairline: above is
            // content, below is tools. `mt_auto` pins it to the bottom even when
            // both sections are collapsed and no `flex_1` list is there to push it.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .pt_2()
                    .pb_2()
                    .mt_auto()
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

    /// One element per task row, plus the roots editor under the active one.
    fn render_task_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let row_hover = cx.theme().sidebar_accent.opacity(0.55);
        // 选中行背景：比 hover 略深、但比原来的整块 sidebar_accent 浅一档。
        // 定时任务列表的选中行沿用同一配色（见 `render_schedule_list`）。
        let row_active = cx.theme().sidebar_accent.opacity(0.7);
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
            let label = format!("{title}{wonky}");
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
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        // 状态圆点在前，定时任务时钟在后，之后是标题（ADR-0059）。
                        .child(div().text_size(sp(13.0)).whitespace_nowrap().child(marker))
                        .when(row.has_scheduled, |this| {
                            this.child(
                                Icon::new(IconName::CalendarClock)
                                    .with_size(Size::Small)
                                    .text_color(cx.theme().muted_foreground),
                            )
                        })
                        .child(div().text_size(sp(13.0)).whitespace_nowrap().child(label)),
                );
            // 「任务操作」按钮；弹出菜单挂在按钮自己的 relative 容器上，与
            // 定时任务段头按钮同款：基准点 = 紧跟按钮之后（按钮底部），
            // 向下 4px 弹出。
            let mut task_menu_wrapper = div().relative().child(
                Button::new(format!("task-menu-{}", row.task.id))
                    .compact()
                    .icon(IconName::Ellipsis)
                    .tooltip("任务操作")
                    // 透明背景：常态字体灰，悬浮/按下变黑。Custom 变体的
                    // foreground 决定悬浮文字色，调用层 text_color 只是
                    // 常态底色（悬浮时被覆盖）。
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(cx.theme().transparent)
                            .hover(cx.theme().transparent)
                            .active(cx.theme().transparent)
                            .foreground(cx.theme().foreground),
                    )
                    .text_color(cx.theme().muted_foreground)
                    .on_click(cx.listener({
                        let task_id = row.task.id.clone();
                        move |this, _, _window, cx| {
                            cx.stop_propagation();
                            this.task_menu = if this.task_menu.as_deref() == Some(task_id.as_str())
                            {
                                None
                            } else {
                                Some(task_id.clone())
                            };
                            cx.notify();
                        }
                    })),
            );
            if menu_open {
                task_menu_wrapper = task_menu_wrapper.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopLeft)
                            .offset(point(px(0.0), px(4.0)))
                            .snap_to_window_with_margin(px(8.0))
                            .child(self.render_task_menu(&row.task.id, &full_title, cx)),
                    )
                    .with_priority(gpui_kit::base::POPUP_PRIORITY),
                );
            }
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
                .child(task_menu_wrapper)
                .child(
                    Button::new(format!("task-add-dir-{}", row.task.id))
                        .compact()
                        .icon(IconName::FolderPlus)
                        .tooltip("添加目录")
                        // 与「任务操作」按钮一致：透明背景，常态灰色、悬浮黑色。
                        .custom(
                            ButtonCustomVariant::new(cx)
                                .color(cx.theme().transparent)
                                .hover(cx.theme().transparent)
                                .active(cx.theme().transparent)
                                .foreground(cx.theme().foreground),
                        )
                        .text_color(cx.theme().muted_foreground)
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
            rows.push(task_row.into_any_element());
            if active {
                rows.push(self.render_task_roots(cx));
            }
        }
        rows
    }

    /// Top row: mark, product name, collapse button (ADR-0063 §2).
    ///
    /// The mark is the same `currentColor` svg the running row uses, so it follows
    /// the theme without a second copy of the artwork.
    fn render_sidebar_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px_2()
            .pt_2()
            .pb_1()
            .gap_2()
            .child(
                // 与下面的段头、以及底部两行按钮共用同一个左内缩 `px_2p5`：
                // `Button` 在 `Size::Medium` 下自己带这个内缩，非按钮行必须补上，
                // 否则图标左沿差 10px。
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_w(px(0.0))
                    .pl_2p5()
                    .child(
                        Icon::default()
                            .data(QC_MARK)
                            .size(px(18.0))
                            .text_color(cx.theme().foreground),
                    )
                    .child(
                        div()
                            .text_size(sp(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .whitespace_nowrap()
                            .overflow_hidden()
                            // A placeholder name: the binary is `qc`, the crate is
                            // `qcode`, and this is the product label the window
                            // shows. Renaming it is a one-string change.
                            .child(PRODUCT_NAME),
                    ),
            )
            .child(
                Button::new("collapse-sidebar")
                    // 与 dsh 的「收起侧边栏」同款：带边框的面板图标（PanelLeft
                    // 轮廓，16px）；背景与「新建任务」按钮一致——常态无色、
                    // 悬浮/按下灰色底。
                    .icon(IconName::PanelLeft)
                    .tooltip("折叠侧栏")
                    .accessibility_label("折叠侧栏")
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(cx.theme().transparent)
                            .hover(rgb(0x888888).opacity(0.2).into())
                            .active(rgb(0x888888).opacity(0.45).into())
                            .foreground(cx.theme().button_foreground),
                    )
                    .on_click(cx.listener(|this, _, _window, cx| this.toggle_sidebar(cx))),
            )
    }

    /// One accordion header. `action` is the段头 button (only 我的任务 has one).
    fn render_section_header(
        &self,
        section: SidebarSection,
        label: &'static str,
        action: Option<(&'static str, &'static str, &'static str, SidebarAction)>,
        open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;
        let section_for_click = section;
        div()
            .id(("sidebar-section", section as usize))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px_2()
            .pt_2()
            .pb_1()
            .cursor_pointer()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .pl_1p5()
                    .child(
                        // The open header must LOOK open (chevron down +
                        // highlighted): its click collapses the section, and a
                        // click that folds the row without any visual cue reads
                        // as broken.
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .with_size(Size::Small)
                        .text_color(muted),
                    )
                    .child(
                        // 与收起态同一个图标：段头展开时也要一眼看出是「任务」还是「定时」。
                        Icon::new(match section {
                            SidebarSection::Tasks => IconName::FolderKanban,
                            SidebarSection::Schedule => IconName::CalendarClock,
                        })
                        .with_size(Size::Medium)
                        .text_color(if open { foreground } else { muted }),
                    )
                    .child(
                        div()
                            .text_size(sp(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if open { foreground } else { muted })
                            .child(label),
                    ),
            )
            .when_some(action, |this, (id, _label, tooltip, kind)| {
                let kind_for_click = kind;
                this.child(
                    Button::new(id)
                        .icon(IconName::CirclePlus)
                        .tooltip(tooltip)
                        // 无边框圆钮：常态透明，悬浮/按下加深灰色底（自定义变体
                        // 不画边框，见 gpui-component button.rs）。
                        .custom(
                            ButtonCustomVariant::new(cx)
                                .color(cx.theme().transparent)
                                .hover(rgb(0x888888).opacity(0.2).into())
                                .active(rgb(0x888888).opacity(0.45).into())
                                .foreground(cx.theme().button_foreground),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            // The `+` sits inside a clickable header; without this
                            // the row would also toggle the section.
                            cx.stop_propagation();
                            match kind_for_click {
                                SidebarAction::NewTask => {
                                    // 「我的任务」可能是收起态：先把它打开，
                                    // 否则任务建出来了、行却在另一个段后面。
                                    this.open_sidebar_section(SidebarSection::Tasks, cx);
                                    this.new_session(window, cx);
                                }
                            }
                        })),
                )
            })
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.toggle_sidebar_section(section_for_click, cx);
            }))
            .into_any_element()
    }

    /// The open section's list: the only scrolling zone in the sidebar.
    ///
    /// `min_h(0)` lets it shrink below its content; without it the flex item
    /// refuses to and the footer is pushed off-screen. `.id()` is required, not
    /// decorative: `overflow_y_scroll` lives on `StatefulInteractiveElement`.
    fn render_task_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .children(self.render_task_rows(cx))
    }

    /// The collapsed sidebar: five icon slots (ADR-0063 §4).
    ///
    /// All four functional icons only switch the main view — none of them changes
    /// the sidebar's width, so they stay uniform. Crossing 任务/定时任务 also expands
    /// the corresponding section; the other section keeps its own state.
    pub(crate) fn render_sidebar_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .py_2()
            .gap_1()
            .id("task-sidebar-rail")
            .bg(cx.theme().sidebar)
            .child(self.rail_button(
                "rail-expand",
                IconName::ChevronRight,
                "展开侧栏",
                false,
                None,
                cx,
            ))
            .child(self.rail_button(
                "rail-tasks",
                IconName::FolderKanban,
                "任务",
                self.view == MainView::Task,
                self.task_badge(),
                cx,
            ))
            .child(div().flex_1())
            .child(self.rail_button(
                "rail-schedule",
                IconName::CalendarClock,
                "定时",
                self.view == MainView::Schedule,
                self.schedule_badge(),
                cx,
            ))
            .child(self.rail_button(
                "rail-terminal",
                IconName::SquareTerminal,
                "终端",
                self.view == MainView::Terminal,
                None,
                cx,
            ))
            .child(div().w(px(24.0)).border_t_1().border_color(border).my_1())
            .child(self.rail_button(
                "rail-settings",
                IconName::Settings,
                "设置（在新窗口中打开）",
                false,
                None,
                cx,
            ))
    }

    fn rail_button(
        &self,
        id: &'static str,
        icon: IconName,
        label: &'static str,
        active: bool,
        badge: Option<bool>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;
        let accent = cx.theme().info;
        let button = Button::new(id)
            .icon(icon)
            .tooltip(label)
            .accessibility_label(label)
            .tooltip(label)
            .cursor_pointer();

        let button = if active {
            button.text_color(foreground)
        } else {
            button.text_color(muted)
        };
        let target = match id {
            "rail-expand" => RailAction::Expand,
            "rail-tasks" => RailAction::Tasks,
            "rail-schedule" => RailAction::Schedule,
            "rail-terminal" => RailAction::Terminal,
            _ => RailAction::Settings,
        };
        let button = button.on_click(cx.listener(move |this, _, window, cx| match target {
            RailAction::Expand => this.toggle_sidebar(cx),
            RailAction::Tasks => {
                this.show_main_view(MainView::Task, Some(SidebarSection::Tasks), window, cx)
            }
            RailAction::Schedule => this.show_main_view(
                MainView::Schedule,
                Some(SidebarSection::Schedule),
                window,
                cx,
            ),
            RailAction::Terminal => this.show_main_view(MainView::Terminal, None, window, cx),
            RailAction::Settings => this.open_settings(cx),
        }));
        if badge == Some(true) {
            // A dot, not a count: 48px cannot hold a legible number, and the two
            // signals (approval/未读 vs 定时 problems) mean the same thing to the
            // reader — "there is something in here" (ADR-0063 §6).
            //
            // 叠在按钮右上角，而不是塞进按钮内容里：rail 只有 48px，塞进去会把
            // 图标挤偏；而 zero-size 的锚点会被按钮裁掉（之前那颗点根本看不见）。
            div()
                .relative()
                .child(button)
                .child(
                    div()
                        .absolute()
                        .top(px(2.0))
                        .right(px(2.0))
                        .w(px(6.0))
                        .h(px(6.0))
                        .rounded(px(3.0))
                        .bg(accent),
                )
                .into_any_element()
        } else {
            button.into_any_element()
        }
    }

    /// 定时任务段的段头：左侧与其它段头同构（chevron + 图标 + 名称），右侧
    /// 是两个动作按钮——「＋」新建（跳新对话预填创建提示）与「⋮」菜单
    /// （在对话中创建 / 手动创建 / 按状态过滤）。
    fn render_schedule_section_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;
        let open = self.schedule_section_open;
        let mut menu_wrapper = div().relative().child(
            Button::new("schedule-menu")
                .compact()
                .icon(IconName::Ellipsis)
                .tooltip("定时任务操作")
                // 无边框透明：常态无底，悬浮/按下灰色（与「在对话中创建
                // 定时任务」按钮一致）。
                .custom(
                    ButtonCustomVariant::new(cx)
                        .color(cx.theme().transparent)
                        .hover(rgb(0x888888).opacity(0.2).into())
                        .active(rgb(0x888888).opacity(0.45).into())
                        .foreground(cx.theme().button_foreground),
                )
                .on_click(cx.listener(|this, _, _window, cx| {
                    cx.stop_propagation();
                    this.toggle_schedule_menu(cx);
                })),
        );
        if self.schedule_menu_open {
            menu_wrapper = menu_wrapper.child(
                deferred(
                    anchored()
                        // 块级容器里 absolute 弹层的基准点 = 紧跟按钮之后（按钮
                        // 底部），偏移只留 4px 间距即可，不再加整个按钮高。
                        .anchor(Anchor::TopLeft)
                        .offset(point(px(0.0), px(4.0)))
                        .snap_to_window_with_margin(px(8.0))
                        .child(self.render_schedule_menu(cx)),
                )
                .with_priority(gpui_kit::base::POPUP_PRIORITY),
            );
        }
        div()
            .id(("sidebar-section", SidebarSection::Schedule as usize))
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px_2()
            .pt_2()
            .pb_1()
            .cursor_pointer()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .pl_1p5()
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .with_size(Size::Small)
                        .text_color(muted),
                    )
                    .child(
                        Icon::new(IconName::CalendarClock)
                            .with_size(Size::Medium)
                            .text_color(if open { foreground } else { muted }),
                    )
                    .child(
                        div()
                            .text_size(sp(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if open { foreground } else { muted })
                            .child("定时任务"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_0p5()
                    .child(menu_wrapper)
                    .child(
                        Button::new("new-schedule-task")
                            .icon(IconName::CirclePlus)
                            .tooltip("在对话中创建定时任务")
                            // 与「我的任务」段头「新建任务」按钮同款：无边框、
                            // 常态透明，悬浮/按下加深灰色底。
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .color(cx.theme().transparent)
                                    .hover(rgb(0x888888).opacity(0.2).into())
                                    .active(rgb(0x888888).opacity(0.45).into())
                                    .foreground(cx.theme().button_foreground),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                this.new_schedule_draft(window, cx);
                            })),
                    ),
            )
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.toggle_sidebar_section(SidebarSection::Schedule, cx);
            }))
            .into_any_element()
    }

    /// 定时任务段头「⋮」的弹出菜单：在对话中创建 / 手动创建，以及
    /// 按状态过滤（全部 / 已开启 / 已暂停 / 已结束）。
    fn render_schedule_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        div()
            .id("schedule-menu-popover")
            .w(px(168.0))
            .flex()
            .flex_col()
            .gap_1()
            .rounded(px(8.0))
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .p_1()
            .text_color(theme.popover_foreground)
            .on_mouse_down_out(cx.listener(|this, _, _window, cx| {
                this.schedule_menu_open = false;
                cx.notify();
            }))
            .child(
                Button::new("schedule-create-in-chat")
                    .label("在对话中创建")
                    .tooltip("新建会话，让模型按你的描述创建定时任务")
                    // 菜单项无边框：常态透明，悬浮/按下灰色底。
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(cx.theme().transparent)
                            .hover(rgb(0x888888).opacity(0.2).into())
                            .active(rgb(0x888888).opacity(0.45).into())
                            .foreground(cx.theme().popover_foreground),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.new_schedule_draft(window, cx);
                    })),
            )
            .child(
                Button::new("schedule-manual-create")
                    .label("手动创建")
                    .tooltip("填写名称 / 内容 / 周期等，直接创建")
                    // 菜单项无边框：常态透明，悬浮/按下灰色底。
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(cx.theme().transparent)
                            .hover(rgb(0x888888).opacity(0.2).into())
                            .active(rgb(0x888888).opacity(0.45).into())
                            .foreground(cx.theme().popover_foreground),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.open_schedule_form(window, cx);
                    })),
            )
            .child(
                // 按状态过滤：原段头的过滤按钮移到这里，功能保持不变。
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .mt_1()
                    .pt_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(theme.muted_foreground)
                            .px_1()
                            .child("按状态过滤"),
                    )
                    .children(SCHEDULE_FILTERS.iter().enumerate().map(|(index, filter)| {
                        let selected = *filter == self.schedule_filter;
                        let label = schedule_filter_label(*filter);
                        Button::new(("schedule-filter-option", index))
                            .label(if selected {
                                format!("{label} ✓")
                            } else {
                                label.to_string()
                            })
                            // 菜单项无边框：常态透明，悬浮/按下灰色底。
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .color(cx.theme().transparent)
                                    .hover(rgb(0x888888).opacity(0.2).into())
                                    .active(rgb(0x888888).opacity(0.45).into())
                                    .foreground(cx.theme().popover_foreground),
                            )
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                cx.stop_propagation();
                                this.set_schedule_filter(*filter, cx);
                            }))
                    })),
            )
            .into_any_element()
    }

    /// The open schedule section's list: one line per runtime task.
    ///
    /// The sidebar is 240px wide by default; a card per entry would repeat the
    /// name, the cron and the status in a column too narrow for any of them. The
    /// detail + run log lives in the content area (`render_schedule_view`); this
    /// is the index, and clicking a line goes there.
    fn render_schedule_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let filter = self.schedule_filter;
        // 按状态过滤后的任务行：`None`（全部）不过滤。
        let matched = self
            .schedule_tasks
            .iter()
            .filter(|task| filter.is_none_or(|status| task.status == status))
            .collect::<Vec<_>>();
        div()
            .id("schedule-list")
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .overflow_y_scroll()
            .when(self.schedule_tasks.is_empty(), |this| {
                this.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(muted)
                        .child("还没有定时任务——点「＋」在对话中创建，或用「⋮」手动创建"),
                )
            })
            .when(
                !self.schedule_tasks.is_empty() && matched.is_empty(),
                |this| {
                    this.child(
                        div()
                            .px_2()
                            .py_1()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(muted)
                            .child(format!(
                                "没有「{}」状态的定时任务",
                                schedule_filter_label(filter)
                            )),
                    )
                },
            )
            .children(matched.iter().map(|task| {
                let marker = match task.status {
                    qcode_core::schedule::TaskStatus::Active => "●",
                    qcode_core::schedule::TaskStatus::Paused => "●",
                    qcode_core::schedule::TaskStatus::Ended => "■",
                };
                // 状态圆点配色：已开启=绿，已暂停=黄，已结束=深灰。
                let dot = match task.status {
                    qcode_core::schedule::TaskStatus::Active => rgb(0x16a34a),
                    qcode_core::schedule::TaskStatus::Paused => rgb(0xca8a04),
                    qcode_core::schedule::TaskStatus::Ended => rgb(0x6b7280),
                };
                let next = task
                    .next_fire_at
                    .map_or_else(String::new, |at| format!(" · {}", at.time()));
                let task_id = task.id.clone();
                // 选中行背景与任务列表保持一致（浅灰，见 `render_task_rows`）。
                let row_hover = cx.theme().sidebar_accent.opacity(0.55);
                let row_active = cx.theme().sidebar_accent.opacity(0.7);
                let selected = self.schedule_detail.as_deref() == Some(task.id.as_str());
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .w_full()
                    .min_h(px(30.0))
                    .px_2()
                    .rounded(px(6.0))
                    .id(format!("schedule-row-{}", task.id))
                    .cursor_pointer()
                    .when(selected, |this| this.bg(row_active))
                    .when(!selected, |this| this.hover(|style| style.bg(row_hover)))
                    .when(
                        task.status == qcode_core::schedule::TaskStatus::Ended,
                        |this| this.opacity(0.55),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(div().text_size(sp(BODY_SIZE)).text_color(dot).child(marker))
                            .child(
                                div()
                                    .text_size(sp(BODY_SIZE))
                                    .whitespace_nowrap()
                                    .child(format!("{}{next}", task.title)),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.open_schedule_detail(&task_id, cx);
                    }))
                    .into_any_element()
            }))
    }

    /// Whether the 任务 icon should show a dot: anything on screen or behind it
    /// wants attention (待审批 / 未读 / 后台在跑), ADR-0063 §6.
    fn task_badge(&self) -> Option<bool> {
        let any = !self.unread.is_empty()
            || self.ui.turn_in_flight
            || self
                .dormant
                .values()
                .any(|dormant| dormant.running || !dormant.approvals.is_empty());
        Some(any)
    }

    /// Whether the 定时任务 icon should show a dot: any entry that says something
    /// is wrong (bad cron, orphaned, missed windows, failed). Same source as the
    /// per-task hints, so the two can never disagree (ADR-0059 §3).
    fn schedule_badge(&self) -> Option<bool> {
        Some(
            self.schedule_rows
                .iter()
                .any(|row| row.broken.is_some() || row.hint().is_some()),
        )
    }
}

/// What a rail icon does. A named enum keeps the click handler one closure
/// instead of five copies of the same shape.
#[derive(Clone, Copy)]
enum RailAction {
    Expand,
    Tasks,
    Schedule,
    Terminal,
    Settings,
}

/// The section-header action button, when there is one.
#[derive(Clone, Copy)]
enum SidebarAction {
    NewTask,
}
