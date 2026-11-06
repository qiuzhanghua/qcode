use super::*;
use gpui_kit::base::actions::{Confirm, SelectDown, SelectUp};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::popover::{Popover, PopoverState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::v_flex;

/// 按搜索词过滤模型分组：仅匹配模型名（大小写不敏感子串匹配），过滤后
/// 为空的 Provider 组整组剔除；搜索词为空时返回全量分组。
fn filter_model_groups(
    groups: &[(String, Vec<String>)],
    query: &str,
) -> Vec<(String, Vec<String>)> {
    let q = query.trim().to_lowercase();
    groups
        .iter()
        .map(|(provider, models)| {
            (
                provider.clone(),
                models
                    .iter()
                    .filter(|model| model.to_lowercase().contains(&q))
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        })
        .filter(|(_, models)| q.is_empty() || !models.is_empty())
        .collect()
}

/// 键盘可导航的扁平模型行（provider, model）：按分组顺序展开；若内核当前
/// provider 不在配置里（恢复的会话 / 命令行传入），追加兜底组模型作为
/// 最后一行，保证键盘能选中它。
fn flat_model_rows(
    groups: &[(String, Vec<String>)],
    query: &str,
    current: &(String, String),
) -> Vec<(String, String)> {
    let filtered = filter_model_groups(groups, query);
    let mut flat: Vec<(String, String)> = filtered
        .iter()
        .flat_map(|(provider, models)| models.iter().map(move |m| (provider.clone(), m.clone())))
        .collect();
    if !groups.iter().any(|(p, _)| p == &current.0) {
        flat.push((current.0.clone(), current.1.clone()));
    }
    flat
}

impl Shell {
    /// Provider/Model 选择弹层：顶部搜索框实时过滤模型（仅匹配模型名，
    /// 大小写不敏感），列表最多显示 10 行、超出出现纵向滚动条，保留
    /// ↑↓ 移动高亮 / Enter 选中 / Esc 关闭的键盘操作。
    /// 弹层从输入框发送按钮左侧（窗口底部）向上展开：`Anchor::BottomLeft`
    /// 让弹层从触发器上方展开，避免超出窗口。
    fn render_model_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let provider = self.app.status.provider.clone();
        let model = self.app.status.model.clone();
        let config_path = self.config_path.clone();
        let shell = cx.entity().downgrade();
        let search_input = self.model_search_input.clone();
        let search_focus = search_input.read(cx).focus_handle(cx);
        // 渲染快照：搜索输入 → Change → 整棵树重渲染，闭包重建拿到新值。
        let query = self.model_search_query.clone();
        let index = self.model_search_index;
        let groups = crate::provider_models(&config_path);
        let current = (provider.clone(), model.clone());

        // 弹层 id 与 trigger Button 的 "model-menu" 区分开，避免 GPUI
        // element state（keyed state / 按钮状态）串用。
        Popover::new("model-menu-popover")
            .anchor(Anchor::BottomLeft)
            .appearance(true)
            .overlay_closable(true)
            .trigger(
                Button::new("model-menu")
                    .label(format!("{provider} / {model}"))
                    .dropdown_caret(true),
            )
            .track_focus(&search_focus)
            .on_open_change({
                let shell = cx.entity().downgrade();
                let search_input = self.model_search_input.clone();
                move |open, window, cx| {
                    // 重新打开菜单：清空搜索词与键盘高亮。
                    if *open {
                        search_input.update(cx, |state, cx| {
                            state.set_value("", window, cx);
                        });
                        if let Some(shell) = shell.upgrade() {
                            shell.update(cx, |shell, cx| {
                                shell.model_search_query.clear();
                                shell.model_search_index = 0;
                                cx.notify();
                            });
                        }
                    }
                }
            })
            .content(move |_state, _window, cx| {
                let filtered = filter_model_groups(&groups, &query);
                let flat_len = filtered.iter().map(|(_, models)| models.len()).sum::<usize>();
                // 键盘高亮只落在配置内的模型行上；兜底组的行高亮单独按
                // `index == flat_len` 判断（见下方兜底组渲染）。
                let selected = (index < flat_len).then_some(index);
                let state_entity = cx.entity().downgrade();

                // 组标题行（灰色小字、不可点击）+ 模型行（可点击、高亮）。
                let mut rows = Vec::new();
                let mut flat_ix = 0usize;
                for (provider_name, models) in &filtered {
                    rows.push(
                        div()
                            .h(px(26.))
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .items_center()
                            .px_2()
                            .child(provider_name.clone())
                            .into_any_element(),
                    );
                    for model in models {
                        let ix = flat_ix;
                        flat_ix += 1;
                        let p = provider_name.clone();
                        let m = model.clone();
                        let checked = current.0 == p && current.1 == m;
                        let highlighted = selected == Some(ix);
                        rows.push(
                            div()
                                .id(ix)
                                .h(px(26.))
                                .flex()
                                .flex_row()
                                .items_center()
                                .px_2()
                                .rounded(cx.theme().radius)
                                .gap_2()
                                .when(highlighted, |this| {
                                    this.bg(cx.theme().tokens.accent)
                                        .text_color(cx.theme().accent_foreground)
                                })
                                .on_hover({
                                    let shell = shell.clone();
                                    move |hovered, _window, cx| {
                                        // 鼠标悬停与键盘高亮共用同一个下标。
                                        if *hovered {
                                            if let Some(shell) = shell.upgrade() {
                                                shell.update(cx, |shell, cx| {
                                                    shell.model_search_index = ix;
                                                    cx.notify();
                                                });
                                            }
                                        }
                                    }
                                })
                                .on_click({
                                    let p = p.clone();
                                    let m = m.clone();
                                    let shell = shell.clone();
                                    let state_entity = state_entity.clone();
                                    move |_event, window, cx| {
                                        if let Some(shell) = shell.upgrade() {
                                            shell.update(cx, |shell, cx| {
                                                shell.switch_model(p.clone(), m.clone(), cx);
                                            });
                                        }
                                        if let Some(state) = state_entity.upgrade() {
                                            state.update(cx, |state, cx| {
                                                state.dismiss(window, cx);
                                            });
                                        }
                                    }
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .text_size(sp(13.0))
                                        .child(model.clone()),
                                )
                                .when(checked, |this| {
                                    this.child(
                                        Icon::new(IconName::Check)
                                            .xsmall()
                                            .text_color(cx.theme().accent_foreground),
                                    )
                                })
                                .into_any_element(),
                        );
                    }
                }
                // The kernel's provider may not be in the config (resumed
                // session or a model passed on the command line): keep it
                // reachable as its own group.
                if !groups.iter().any(|(p, _)| p == &current.0) {
                    rows.push(
                        div()
                            .h(px(26.))
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .items_center()
                            .px_2()
                            .child(current.0.clone())
                            .into_any_element(),
                    );
                    let p = current.0.clone();
                    let m = current.1.clone();
                    rows.push(
                        div()
                            .id(flat_len)
                            .h(px(26.))
                            .flex()
                            .flex_row()
                            .items_center()
                            .px_2()
                            .rounded(cx.theme().radius)
                            .gap_2()
                            .when(index == flat_len, |this| {
                                this.bg(cx.theme().tokens.accent)
                                    .text_color(cx.theme().accent_foreground)
                            })
                            .on_click({
                                let p = p.clone();
                                let m = m.clone();
                                let shell = shell.clone();
                                let state_entity = state_entity.clone();
                                move |_event, window, cx| {
                                    if let Some(shell) = shell.upgrade() {
                                        shell.update(cx, |shell, cx| {
                                            shell.switch_model(p.clone(), m.clone(), cx);
                                        });
                                    }
                                    if let Some(state) = state_entity.upgrade() {
                                        state.update(cx, |state, cx| {
                                            state.dismiss(window, cx);
                                        });
                                    }
                                }
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(sp(15.0))
                                    .child(m.clone()),
                            )
                            .child(
                                Icon::new(IconName::Check)
                                    .xsmall()
                                    .text_color(cx.theme().accent_foreground),
                            )
                            .into_any_element(),
                    );
                }

                let search_input = search_input.clone();
                v_flex()
                    .id("model-menu-content")
                    .key_context("ModelMenu")
                    // 键盘：↑↓ 移动高亮（跳过 Provider 标题行，标题行不在
                    // 扁平模型列表里）；Enter 选中当前高亮；Esc 由底层
                    // Popover 的 CONTENT_CONTEXT Cancel 处理并关闭。
                    .on_action(cx.listener({
                        let shell = shell.clone();
                        let current = current.clone();
                        move |_state: &mut PopoverState,
                             _event: &SelectUp,
                             _window: &mut Window,
                             cx: &mut Context<PopoverState>| {
                            if let Some(shell) = shell.upgrade() {
                                shell.update(cx, |shell, cx| {
                                    let groups = crate::provider_models(&shell.config_path);
                                    let n = flat_model_rows(&groups, &shell.model_search_query, &current)
                                        .len();
                                    if n > 0 {
                                        shell.model_search_index = if shell.model_search_index == 0 {
                                            n - 1
                                        } else {
                                            shell.model_search_index - 1
                                        };
                                        cx.notify();
                                    }
                                });
                            }
                        }
                    }))
                    .on_action(cx.listener({
                        let shell = shell.clone();
                        let current = current.clone();
                        move |_state: &mut PopoverState,
                             _event: &SelectDown,
                             _window: &mut Window,
                             cx: &mut Context<PopoverState>| {
                            if let Some(shell) = shell.upgrade() {
                                shell.update(cx, |shell, cx| {
                                    let groups = crate::provider_models(&shell.config_path);
                                    let n = flat_model_rows(&groups, &shell.model_search_query, &current)
                                        .len();
                                    if n > 0 {
                                        shell.model_search_index =
                                            (shell.model_search_index + 1) % n;
                                        cx.notify();
                                    }
                                });
                            }
                        }
                    }))
                    .on_action(cx.listener({
                        let shell = shell.clone();
                        let current = current.clone();
                        move |state: &mut PopoverState,
                             _event: &Confirm,
                             window: &mut Window,
                             cx: &mut Context<PopoverState>| {
                            cx.stop_propagation();
                            if let Some(shell) = shell.upgrade() {
                                shell.update(cx, |shell, cx| {
                                    let groups = crate::provider_models(&shell.config_path);
                                    let flat = flat_model_rows(
                                        &groups,
                                        &shell.model_search_query,
                                        &current,
                                    );
                                    if let Some((p, m)) = flat.get(shell.model_search_index) {
                                        shell.switch_model(p.clone(), m.clone(), cx);
                                    }
                                });
                                state.dismiss(window, cx);
                            }
                        }
                    }))
                    .child(
                        Input::new(&search_input)
                            .w(px(240.))
                            .h(px(28.))
                            .focus_bordered(true),
                    )
                    .child(
                        // 列表区域：最多 10 行（26px × 10），超出后纵向滚动。
                        v_flex()
                            .w_full()
                            .mt_1()
                            .gap_y_0p5()
                            .max_h(px(260.))
                            .overflow_y_scrollbar()
                            .children(rows),
                    )
            })
            .into_any_element()
    }

    /// Sandbox directory access for the active task: RW（工作区可写）vs
    /// RO（仅可读取）。切换对该任务的全部目录统一生效，原侧边栏每条目录
    /// 上的 RO/RW 按钮已移除。按钮位于输入框「选择文件」右侧（窗口底部），
    /// 弹层向上弹出：`Anchor::BottomLeft` 让菜单从触发器上方展开。
    /// 任务没有目录时按钮隐藏，不渲染空状态。
    fn render_sandbox_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        // 无活动任务或任务没有目录时隐藏按钮。
        let Some(task_id) = self.active.clone() else {
            return div().into_any_element();
        };
        let roots = self.task_roots_for_edit(&task_id);
        if roots.is_empty() {
            return div().into_any_element();
        }
        // 按钮对任务下全部目录统一生效，因此不出现「混合」状态：
        // 仅当所有目录都为只读时显示「仅可读取」，否则显示「工作区可写」。
        let all_ro = roots.iter().all(|root| root.access == RootAccess::ReadOnly);
        let label = if all_ro {
            "仅可读取"
        } else {
            "工作区可写"
        };
        let current_rw = roots
            .iter()
            .all(|root| root.access == RootAccess::ReadWrite);
        let current_ro = all_ro;
        let shell = cx.entity().downgrade();
        Button::new("sandbox-menu")
            .label(label)
            .icon(IconName::Shield)
            .dropdown_caret(true)
            .tooltip("沙箱目录权限（对当前任务全部目录生效）")
            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _window, _cx| {
                menu.item({
                    let shell = shell.clone();
                    let task_id = task_id.clone();
                    PopupMenuItem::new("工作区可写")
                        .checked(current_rw)
                        .on_click(move |_, _, cx| {
                            if let Some(shell) = shell.upgrade() {
                                shell.update(cx, |shell, cx| {
                                    shell.set_task_all_roots_access(
                                        &task_id,
                                        RootAccess::ReadWrite,
                                        cx,
                                    );
                                });
                            }
                        })
                })
                .item({
                    let shell = shell.clone();
                    let task_id = task_id.clone();
                    PopupMenuItem::new("仅可读取")
                        .checked(current_ro)
                        .on_click(move |_, _, cx| {
                            if let Some(shell) = shell.upgrade() {
                                shell.update(cx, |shell, cx| {
                                    shell.set_task_all_roots_access(
                                        &task_id,
                                        RootAccess::ReadOnly,
                                        cx,
                                    );
                                });
                            }
                        })
                })
            })
            .into_any_element()
    }

    /// The pending queue dock above the composer: each queued message shows
    /// its preview plus steer / move / remove actions.
    /// Steer is only offered while a turn is running.
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
        // 任务累计 token 用量：汇总所有 turn 的输入/输出/缓存 token。
        let total_tokens: u64 = self
            .app
            .turns
            .iter()
            .flat_map(|turn| {
                [
                    turn.usage.input_tokens,
                    turn.usage.output_tokens,
                    turn.usage.cache_read_tokens,
                    turn.usage.cache_write_tokens,
                ]
                .into_iter()
                .flatten()
            })
            .sum();
        // Turns 超限提示：达到配置 max_turns 轮数且当前任务没被「忽略」时显示。
        let round_hint_visible = self.app.turns.len() >= self.round_hint_threshold
            && !self
                .active
                .as_ref()
                .is_some_and(|id| self.round_hint_dismissed_tasks.contains(id));
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
        let running_color = theme.muted_foreground;
        let running_divider = border;
        let list = MessageScroller::new("transcript", self.scroller.clone(), {
            let entity = entity.clone();
            move |index, _window, cx| {
                let shell = entity.read(cx);
                unit_row(index, shell, &entity, running_color, running_divider)
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

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .py_4()
            .child(header_label.clone())
            .child(div().flex_grow(1.0));

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
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pl(px(MAIN_PAD_L))
                    .pr(px(MAIN_PAD_R))
                    .pb_2()
                    .children(
                        round_hint_visible.then(|| {
                                // 和权限问询同款卡片：黄色 title「Turns 超限」，
                                // 内容区为当前提示文案，两个按钮：新建任务 / 忽略。
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
                                                    .child("Turns 超限"),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .px_3()
                                            .py_2()
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            // 内容区约三行文本高度。
                                            .child(
                                                div()
                                                    .text_size(sp(15.0))
                                                    .line_height(sp(24.0))
                                                    .text_color(rgb(0x000000))
                                                    .child(format!(
                                                        "当前任务已达到 {} 轮。",
                                                        self.round_hint_threshold
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .text_size(sp(15.0))
                                                    .line_height(sp(24.0))
                                                    .text_color(rgb(0x000000))
                                                    .child("创建新任务可能会获得更好的结果。"),
                                            )
                                            .child(
                                                div()
                                                    .text_size(sp(15.0))
                                                    .line_height(sp(24.0))
                                                    .text_color(rgb(0x666666))
                                                    .child(
                                                        "超过 Max turns 上限后继续对话，可能影响回复质量与上下文管理。",
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .pt_1()
                                                    .flex()
                                                    .flex_row()
                                                    .gap_2()
                                                    .child(
                                                        Button::new("round-hint-new-task")
                                                            .label("新建任务")
                                                            .on_click(cx.listener(
                                                                |this, _, window, cx| {
                                                                    this.new_session(window, cx);
                                                                },
                                                            )),
                                                    )
                                                    .child(
                                                        Button::new("round-hint-ignore")
                                                            .label("忽略")
                                                            .on_click(cx.listener(
                                                                |this, _, _window, cx| {
                                                                    // 每个任务各自记住
                                                                    // 忽略状态，切走再切回
                                                                    // 也不会再次提示。
                                                                    if let Some(id) =
                                                                        this.active.clone()
                                                                    {
                                                                        this.round_hint_dismissed_tasks
                                                                            .insert(id);
                                                                    }
                                                                    cx.notify();
                                                                },
                                                            )),
                                                    ),
                                            ),
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
                        // Turns 超限提示也占输入框位置（与权限/报错框一致），
                        // 显示时输入框隐藏，卡片落在输入框处。
                        self.approvals.current().is_none()
                            && self.error_banner.is_none()
                            && self.restoring.is_none()
                            && !round_hint_visible,
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
                                        div()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                Button::new("upload")
                                                    .label("选择文件")
                                                    .icon(IconName::Plus)
                                                    .on_click(cx.listener(|this, _, _window, cx| {
                                                        this.upload_file(cx)
                                                    })),
                                            )
                                            .child(self.render_sandbox_menu(cx)),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap_2()
                                            .child(self.render_model_menu(cx))
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
                                        "{} turn(s) - {} token(s)",
                                        self.app.status.turn, total_tokens
                                    ))
                                    .right(context_label(&self.app))
                                    .right(format!("sandbox {}", self.app.sandbox)),
                            ),
                    ),
            )
    }
}
