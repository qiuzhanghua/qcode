use super::*;

impl Shell {
    /// The pending queue dock above the composer: each queued message shows
    /// its preview plus steer / move / remove actions, mirroring dsh's queue
    /// dock. Steer is only offered while a turn is running.
    pub(crate) fn render_queue_dock(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let border = theme.border;
        let running = self.turn_in_flight;
        let mut rows = Vec::with_capacity(self.steering.len() + self.queue.len());
        // Steering already handed to the engine but not claimed yet: keep it
        // visible as pending so the user can take it back.
        for item in &self.steering {
            let id = item.id.clone();
            let preview: String = item.text.replace('\n', " ").chars().take(48).collect();
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(6.0))
                    .bg(rgba(0x2f6bff14))
                    .child(
                        div()
                            .flex_1()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(theme.muted_foreground)
                            .child(preview),
                    )
                    .child(
                        div()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(rgb(0x5b7fbf))
                            .child("等待下一步"),
                    )
                    .child(
                        Button::new(format!("steer-undo-{id}"))
                            .label("撤回")
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.backend.queue_op(id.clone(), QueueOp::Unsteer);
                            })),
                    )
                    .into_any_element(),
            );
        }
        for item in &self.queue {
            let id = item.id.clone();
            let preview: String = item.text.replace('\n', " ").chars().take(48).collect();
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded(px(6.0))
                .bg(rgba(0x7f7f7f14))
                .child(
                    div()
                        .flex_1()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(theme.muted_foreground)
                        .child(preview),
                );
            if running {
                let steer_id = id.clone();
                row = row.child(
                    Button::new(format!("queue-steer-{steer_id}"))
                        .label("推送")
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.backend.queue_op(steer_id.clone(), QueueOp::Steer);
                        })),
                );
            }
            let up_id = id.clone();
            row = row.child(
                Button::new(format!("queue-up-{up_id}"))
                    .label("↑")
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.backend.queue_op(up_id.clone(), QueueOp::MoveUp);
                    })),
            );
            let down_id = id.clone();
            row = row.child(
                Button::new(format!("queue-down-{down_id}"))
                    .label("↓")
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.backend.queue_op(down_id.clone(), QueueOp::MoveDown);
                    })),
            );
            let remove_id = id.clone();
            row = row.child(
                Button::new(format!("queue-remove-{remove_id}"))
                    .label("×")
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.backend.queue_op(remove_id.clone(), QueueOp::Remove);
                    })),
            );
            rows.push(row.into_any_element());
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .rounded(px(10.0))
            .border_1()
            .border_color(border)
            .bg(rgba(0x7f7f7f0a))
            .px_2()
            .py_1()
            .children(rows)
            .into_any_element()
    }
    pub(crate) fn render_main(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.poll(cx);
        // 执行中且输入框有内容时，发送按钮应保持可用（发送进排队队列）；
        // 只有执行中且输入框为空时，按钮才变成停止红点。
        let composer_has_draft = !self.input.read(cx).value().trim().is_empty()
            || !self.attachments.is_empty()
            || !self.file_refs.is_empty();
        if self.app.status.running && !self.waiting_ticker {
            self.waiting_ticker = true;
            self.tick_waiting(cx);
        }
        if self.pending_send {
            self.pending_send = false;
            self.send_prompt(window, cx);
        }
        let entity = cx.entity();
        let theme = cx.theme();
        let popover = theme.popover;
        let border = theme.border;
        let list = MessageScroller::new("transcript", self.scroller.clone(), {
            let entity = entity.clone();
            move |index, _window, cx| {
                let shell = entity.read(cx);
                unit_row(index, shell, &entity)
            }
        })
        .scrollbar(false)
        .jump_button(false)
        // The scroller applies its own top/bottom padding to the virtual list.
        // That padding is included in the list's internal scroll extent but
        // not in `max_offset_for_scrollbar`, so the wheel clamp (`content_size
        // - viewport`) stops a few pixels short of the real end and never
        // re-engages tail follow. Keep the list itself unpadded and hold the
        // breathing room on the surrounding container instead.
        .with_list_style(StyleRefinement::default().py(px(0.0)));

        // Header label: the session title when the task has one (first user
        // message), else 「新任务」——一个还没有第一条消息的任务就是新任务，
        // 不展示不可识别的原始 session id。
        let header_label = self
            .active
            .as_deref()
            .and_then(|id| self.titles.get(id))
            .cloned()
            .unwrap_or_else(|| "新任务".to_string());

        // Keep the picker's selection in sync with the kernel's model
        // (a resume or a settings rebuild may change it).
        let current_model = self.app.status.model.clone();
        let needs_sync = {
            let selected = self.model_select.read(cx).selected_value().cloned();
            selected.as_ref() != Some(&current_model)
        };
        if needs_sync {
            self.model_select.update(cx, |state, cx| {
                state.set_selected_value(&current_model, window, cx);
            });
        }
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .py_4()
            .child(format!("{header_label} · {} turn(s)", self.app.turns.len()))
            .child(div().flex_grow(1.0))
            .child(div().w(px(220.0)).child(
                // Default `Length::Auto` sizes the dropdown to the trigger
                // width, so the popup matches the box above it.
                Select::new(&self.model_select).placeholder("model"),
            ));

        // 本次任务执行时间：只在新 turn 运行中显示，数值与「执行过程」头部的
        // 实时耗时一致（靠 200ms ticker 刷新）；turn 中断或结束、执行过程耗时
        // 消失时，这行蓝字也一并隐藏。
        let task_elapsed = self.app.turns.last().and_then(|turn| {
            turn.running().then(|| {
                let now = time::OffsetDateTime::now_utc();
                let live = (now - turn.started_at).max(time::Duration::ZERO);
                format!("本次任务耗时 {}", format_duration(live))
            })
        });

        div()
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .min_w_0()
            .pl(px(MAIN_PAD_L))
            .pr(px(MAIN_PAD_R))
            .child(header)
            .child({
                let rail = self.render_turn_rail(window, cx);
                div()
                    .relative()
                    .flex()
                    .flex_row()
                    .flex_grow(1.0)
                    .min_h_0()
                    .child(
                        div()
                            .id("transcript-scroll")
                            .flex()
                            .flex_col()
                            .flex_grow(1.0)
                            .min_h_0()
                            .pl(px(TRANSCRIPT_PAD_L))
                            .pr(px(TRANSCRIPT_PAD_R))
                            .pt(px(8.0))
                            .pb(px(32.0))
                            // GPUI keeps a text selection live until the mouse
                            // button-up arrives, and a wheel scroll then
                            // drags/extends it along the pointer — so a
                            // phantom button state (or a lost mouse-up) makes
                            // mere scrolling select text. Cancel the selection
                            // on scroll, the way browsers do.
                            .on_scroll(|_, window, cx| {
                                gpui_kit::base::TextSelection::clear(window, cx);
                            })
                            .child(list),
                    )
                    .child(rail)
                    .child(self.render_floating_actions(cx))
            })
            .children(self.diff.as_ref().map(|state| {
                let height = if self.diff_expanded {
                    px(480.0)
                } else {
                    px(220.0)
                };
                div()
                    .flex()
                    .flex_col()
                    .h(height)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .child(
                                Button::new("toggle-diff")
                                    .label(if self.diff_expanded {
                                        "Collapse diff"
                                    } else {
                                        "Expand diff"
                                    })
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.diff_expanded = !this.diff_expanded;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("close-diff")
                                    .label("Close")
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.diff = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(Diff::new(state))
            }))
            // 执行时间：一行无背景的蓝色字，放在所有提示框（权限、报错、
            // 50 轮）上方、输入框之上；无提示框时则直接位于输入框上方。
            .children(task_elapsed.map(|label| {
                div()
                    .w_full()
                    .pl(px(MAIN_PAD_L + 10.0))
                    .pr(px(MAIN_PAD_R))
                    .pb_1()
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(rgb(0x3b82f6))
                            .child(label),
                    )
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pl(px(MAIN_PAD_L))
                    .pr(px(MAIN_PAD_R))
                    .pb_2()
                    .children(
                        (self.app.turns.len() >= ROUND_HINT_THRESHOLD
                            && self.round_hint_dismissed_for != self.active)
                            .then(|| {
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .w_full()
                                    // 只圆上两角，下沿直角。
                                    .rounded_tl(px(8.0))
                                    .rounded_tr(px(8.0))
                                    .bg(rgba(0x7f7f7f0d))
                                    .px_2()
                                    .py_1()
                                    .child(
                                        div()
                                            .flex_grow(1.0)
                                            .text_size(sp(SECONDARY_SIZE))
                                            .child(
                                                "当前任务已达到 50 轮。创建新任务可能会获得更好的结果。",
                                            ),
                                    )
                                    .child(
                                        Button::new("round-hint-new-task")
                                            .compact()
                                            .label("新建任务")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.new_session(window, cx);
                                            })),
                                    )
                                    .child(
                                        div()
                                            .id("round-hint-close")
                                            .cursor_pointer()
                                            .text_color(rgb(0x000000))
                                            .text_size(sp(SECONDARY_SIZE))
                                            .on_click(cx.listener(|this, _, _window, cx| {
                                                this.round_hint_dismissed_for =
                                                    this.active.clone();
                                                cx.notify();
                                            }))
                                            .child("×"),
                                    )
                            })
                            // 和权限框 / 报错框同样的内缩量，三者宽度一致。
                            .map(|hint| {
                                div()
                                    .w_full()
                                    .pl(px(PROMPT_INSET))
                                    .pr(px(PROMPT_INSET))
                                    .child(hint)
                            }),
                    )
                    .children(
                        // 权限问询：有问询时以提示卡片占位展示（输入框隐藏），
                        // 避免输入框自身的边框/圆角从提示框背后透出来。
                        self.approvals.current().map(|view| {
                            let kind: String = match view.tool.as_str() {
                                "run_shell" | "shell" => "执行命令".to_string(),
                                "read_file" | "write_file" | "edit_file" | "apply_patch"
                                | "list_dir" | "glob" | "read" | "write" | "edit"
                                | "search" => "读写文件".to_string(),
                                _ => tool_label(&view.tool),
                            };
                            // 拦截原因映射成中文，其余情况直接展示原文。
                            let reason_cn = match view.reason.as_str() {
                                reason if reason.contains("process execution") => "进程执行策略",
                                reason if reason.contains("write outside workspace") => {
                                    "工作区外写入策略"
                                }
                                reason if reason.contains("multi-file write") => {
                                    "多文件写入策略"
                                }
                                reason if reason.contains("external tool") => "外部工具策略",
                                reason if reason.contains("network access") => "网络访问策略",
                                reason if reason.contains("matched rule") => "权限规则",
                                reason => reason,
                            };
                            let dir = if self.cwd.is_empty() {
                                "当前目录".to_string()
                            } else {
                                self.cwd.clone()
                            };
                            // 第一部分：说明文案（黑字）；第二部分：具体的执行
                            // 内容（灰字），字体都比此前略大。
                            let explain = format!(
                                "是否允许本次操作使用{kind}权限：执行{summary}时，被{reason_cn}拦截，无法在{dir}目录上执行写入，需要提升操作权限；此次操作不会改动任何文件的内容和所有者。",
                                kind = kind,
                                summary = view.summary,
                                reason_cn = reason_cn,
                                dir = dir,
                            );
                            div()
                                .w_full()
                                .rounded_t(px(10.0))
                                .border_1()
                                .border_color(border)
                                .bg(rgb(0xffffff))
                                .flex()
                                .flex_col()
                                // 整卡一圈边框、上两角圆角；title 是黄色色块，
                                // 内容区无独立边框（自然没有上边框）、四角直角。
                                .child(
                                    div()
                                        .rounded_tl(px(10.0))
                                        .rounded_tr(px(10.0))
                                        .bg(rgb(0xfacc15))
                                        .px_3()
                                        .py_1()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .child(
                                            div()
                                                .text_size(sp(16.0))
                                                .line_height(sp(26.0))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(0x000000))
                                                .child("等待审批"),
                                        ),
                                )
                                .child(
                                    div()
                                        .px_3()
                                        .pb_2()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_size(sp(15.0))
                                                .line_height(sp(24.0))
                                                .text_color(rgb(0x000000))
                                                .child(explain),
                                        )
                                        .child(
                                            div()
                                                .pl(px(20.0))
                                                .text_size(sp(15.0))
                                                .line_height(sp(24.0))
                                                .text_color(rgb(0x666666))
                                                .child(format!("> {}", view.summary)),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_row()
                                                .gap_2()
                                                .child(
                                                    Button::new("allow").label("允许")
                                                        .on_click(cx.listener(
                                                            |this, _, _window, cx| {
                                                                this.answer(true, false, cx);
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    Button::new("allow-session")
                                                        .label("总是允许")
                                                        .on_click(cx.listener(
                                                            |this, _, _window, cx| {
                                                                this.answer(true, true, cx);
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    Button::new("deny").label("拒绝")
                                                        .on_click(cx.listener(
                                                            |this, _, _window, cx| {
                                                                this.answer(false, false, cx);
                                                            },
                                                        )),
                                                ),
                                        ),
                                )
                        }),
                    )
                    .children(
                        // 报错提示：同款卡片，红色 title，单「关闭」按钮。
                        self.error_banner.clone().map(|message| {
                            div()
                                .w_full()
                                .rounded_t(px(10.0))
                                .border_1()
                                .border_color(border)
                                .bg(rgb(0xffffff))
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .rounded_tl(px(10.0))
                                        .rounded_tr(px(10.0))
                                        .bg(rgb(0xe8463a))
                                        .px_3()
                                        .py_1()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .child(
                                            div()
                                                .text_size(sp(16.0))
                                                .line_height(sp(26.0))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(0x000000))
                                                .child("请求报错 · 完整信息"),
                                        ),
                                )
                                .child(
                                    div()
                                        .px_3()
                                        .pb_2()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_size(sp(15.0))
                                                .line_height(sp(24.0))
                                                .text_color(rgb(0x000000))
                                                .child(message),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_row()
                                                .gap_2()
                                                .child(
                                                    Button::new("dismiss-error")
                                                        .label("关闭")
                                                        .on_click(cx.listener(
                                                            |this, _, _window, cx| {
                                                                this.error_banner = None;
                                                                cx.notify();
                                                            },
                                                        )),
                                                ),
                                        ),
                                )
                        }),
                    )
                    .children(self.restoring.as_ref().map(|_| {
                        div()
                            .w_full()
                            .rounded(px(10.0))
                            .border_1()
                            .border_color(border)
                            .bg(rgb(0xffffff))
                            .px_3()
                            .py_2()
                            .text_size(sp(SECONDARY_SIZE))
                            .line_height(sp(SECONDARY_LINE))
                            .text_color(rgb(0x777777))
                            .child("已提交，等待引擎确认…")
                    }))
                    .when(
                        self.approvals.current().is_none()
                            && self.error_banner.is_none()
                            && self.restoring.is_none(),
                        |this| {
                            this.child(
                                div()
                                    .relative()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .w_full()
                                    .rounded(px(10.0))
                            .bg(popover)
                            .border_1()
                            .border_color(border)
                            .px_3()
                            .py_2()
                            .child({
                let entity = cx.entity();
                let mut input_box = div()
                    .flex()
                    .flex_col()
                    .gap_1();
                // Pending follow-up queue shown above the input; each row can
                // steer into the running turn, move, or drop.
                if !self.queue.is_empty() || !self.steering.is_empty() {
                    input_box = input_box.child(self.render_queue_dock(cx));
                }
                // Pending file references shown as removable chips above the
                // input, aligned to its left edge; wraps once too wide.
                if !self.file_refs.is_empty() {
                    let mut chips = div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_1();
                    for path in &self.file_refs {
                        let name = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("<file>")
                            .to_string();
                        let remove_path = path.clone();
                        let remove_entity = entity.clone();
                        chips = chips.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .rounded(px(4.0))
                                .bg(rgba(0x7f7f7f14))
                                .px_2()
                                .py_1()
                                .child(
                                    div()
                                        .text_size(sp(CAPTION_SIZE))
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .id(format!("file-remove-{}", remove_path.display()))
                                        .cursor_pointer()
                                        .text_color(rgb(0xe8463a))
                                        .on_click(move |_, _, cx| {
                                            remove_entity.update(cx, |shell, cx| {
                                                shell.remove_file_ref(&remove_path, cx);
                                            });
                                        })
                                        .child("×"),
                                ),
                        );
                    }
                    input_box = input_box.child(chips);
                }
                input_box
                    .on_key_down(move |event, window, cx| {
                                        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
                                            eprintln!(
                                                "qcode-gui key: key={} platform={} secondary={} shift={}",
                                                event.keystroke.key,
                                                event.keystroke.modifiers.platform,
                                                event.keystroke.modifiers.secondary(cx),
                                                event.keystroke.modifiers.shift,
                                            );
                                        }
                                        if is_send_shortcut(&event.keystroke) {
                                            entity.update(cx, |shell, cx| {
                                                shell.send_prompt(window, cx);
                                            });
                                        }
                                    })
                                    .child(
                                        Textarea::new(&self.input)
                                            .bordered(false)
                                            .text_size(sp(15.0))
                                            .aria_label("Message qcode"),
                                    )
                            })
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        Button::new("upload")
                                            .label("选择文件")
                                            .on_click(cx.listener(|this, _, _window, cx| {
                                                this.upload_file(cx)
                                            })),
                                    )
                                    .child(
                                        // Round action button: a blue up-arrow sends
                                        // (queueing while a turn runs), a red dot
                                        // interrupts the running turn — shown only
                                        // when the turn is running and the draft is
                                        // empty, so a draft always keeps Send.
                                        if self.app.status.running && !composer_has_draft {
                                            div()
                                                .w(px(36.0))
                                                .h(px(36.0))
                                                .rounded(px(18.0))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .cursor_pointer()
                                                .bg(rgba(0x0000000d))
                                                .hover(|style| style.bg(rgba(0x0000001a)))
                                                .id("interrupt")
                                                .on_click(cx.listener(|this, _, _window, cx| {
                                                    this.stop();
                                                    cx.notify();
                                                }))
                                                .child(
                                                    div()
                                                        .w(px(12.0))
                                                        .h(px(12.0))
                                                        .rounded(px(6.0))
                                                        .bg(rgb(0xe8463a)),
                                                )
                                        } else {
                                            div()
                                                .w(px(36.0))
                                                .h(px(36.0))
                                                .rounded(px(18.0))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .cursor_pointer()
                                                .bg(rgb(0x3b82f6))
                                                .hover(|style| style.bg(rgb(0x2563eb)))
                                                .id("send")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.send_prompt(window, cx)
                                                }))
                                                .child(
                                                    Icon::new(IconName::ArrowUp)
                                                        .with_size(Size::Small)
                                                        .text_color(rgb(0xffffff)),
                                                )
                                        },
                                    ),
                            )
                    )
                        }
                    ),
            )
            .child(
                div()
                    .pl(px(MAIN_PAD_L))
                    .pr(px(MAIN_PAD_R))
                    .pb_2()
                    .child(
                        div()
                            .rounded(px(8.0))
                            .bg(popover)
                            .border_1()
                            .border_color(border)
                            .child(
                                StatusBar::new()
                                    .left(self.notice.clone().unwrap_or_default())
                                    .left(format!(
                                        "{}:{}",
                                        self.app.status.provider, self.app.status.model
                                    ))
                                    .left(self.cwd.clone())
                                    .right(context_label(&self.app))
                                    .right(format!("${:.4}", self.app.status.cost_usd))
                                    .right(format!("sandbox {}", self.app.sandbox))
                                    .right(if self.app.status.running {
                                        format!(
                                            "{} 等待回复…",
                                            WAITING_SPINNER[self.waiting_frame as usize]
                                        )
                                    } else {
                                        "空闲".to_string()
                                    }),
                            ),
                    ),
            )
    }
}
