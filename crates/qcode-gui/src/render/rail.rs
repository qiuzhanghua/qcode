use super::*;

impl Shell {
    pub(crate) fn render_turn_rail(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Expire any pending rail-popup close timer whose time has come.
        if let Some(at) = self.rail_popup_close_at
            && std::time::Instant::now() >= at
        {
            self.rail_popup = None;
            self.rail_popup_close_at = None;
        }
        if self.app.turns.len() < 2 || window.bounds().size.width.as_f32() < 900.0 {
            return div().into_any_element();
        }
        let total = self.app.turns.len();
        let active = self.active_turn_index();
        // At most 15 dots are shown; with more turns the window scrolls so the
        // active dot stays centred (clamped at the list edges).
        const MAX_DOTS: usize = 15;
        let start = if total <= MAX_DOTS {
            0
        } else {
            let center = active.unwrap_or(total - 1).min(total - 1);
            (center.saturating_sub(MAX_DOTS / 2)).min(total - MAX_DOTS)
        };
        let end = (start + MAX_DOTS).min(total);
        let mut rail = div()
            .w(px(TURN_RAIL_WIDTH))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2();
        for index in start..end {
            // Colour fades with distance from the active turn: the current one
            // is darkest, one step away slightly lighter, farther ones lightest.
            let dot_color = match active
                .map(|active| index.abs_diff(active))
                .unwrap_or(usize::MAX)
            {
                0 => rgb(0x7a7a7a),
                1 => rgb(0xbcbcbc),
                _ => rgb(0xd8d8d8),
            };
            let show_popover = self.rail_popup == Some(index);
            let dot = div()
                .w(px(8.0))
                .h(px(8.0))
                .rounded(px(4.0))
                .bg(dot_color)
                .hover(|style| style.bg(rgb(0xa8a8a8)))
                .cursor_pointer()
                .id(format!("turn-rail-trigger-{index}"))
                .on_hover(cx.listener(move |this, hovered, _window, cx| {
                    if *hovered {
                        this.rail_popup = Some(index);
                        this.rail_popup_close_at = None;
                    } else if this.rail_popup == Some(index) {
                        // Short grace window only to let the pointer travel
                        // from the dot into the popover. Once the pointer
                        // leaves both, the popover must hide — the popover's
                        // own outer div on_hover(false) handles that path,
                        // and this timer catches the case where the user
                        // jumps straight from dot → nowhere.
                        this.rail_popup_close_at =
                            Some(std::time::Instant::now() + std::time::Duration::from_millis(200));
                    }
                    cx.notify();
                }))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.jump_to_turn(index, cx);
                }));
            let popup =
                gpui_kit::base::Popup::new(format!("rail-popup-{index}"), dot.into_any_element())
                    // RightCenter: the popover's right edge snaps to the dot's
                    // left centre, so the card appears on the *left* side of
                    // the rail (over the transcript).
                    .anchor(gpui_kit::Anchor::RightCenter)
                    .offset(px(6.0));
            let popup = if show_popover {
                popup.content(self.build_rail_popover(cx, index))
            } else {
                popup
            };
            rail = rail.child(popup);
        }
        rail.into_any_element()
    }

    /// Build the hover-popover body for a rail dot: the turn's question and
    /// answer stacked in two labelled sections. Long answers are truncated
    /// with an ellipsis. Clicking anywhere in the card jumps to that turn.
    pub(crate) fn build_rail_popover(
        &mut self,
        cx: &mut Context<Self>,
        turn_index: usize,
    ) -> gpui_kit::AnyElement {
        const QUESTION_LIMIT: usize = 120;
        const ANSWER_LIMIT: usize = 260;
        let (question, answer) = match self.app.turns.get(turn_index) {
            Some(turn) => {
                let question = turn
                    .input_block
                    .and_then(|idx| self.app.blocks.get(idx))
                    .and_then(|block| match block {
                        Block::User(text) => Some(text.trim()),
                        _ => None,
                    })
                    .map(|text| short_preview(text, QUESTION_LIMIT))
                    .unwrap_or_else(|| format!("Turn {}", turn.number));
                let answer = turn
                    .answer_block
                    .and_then(|idx| self.app.blocks.get(idx))
                    .and_then(|block| match block {
                        Block::Assistant(text) => Some(text.trim()),
                        _ => None,
                    })
                    .map(|text| short_preview(text, ANSWER_LIMIT))
                    .unwrap_or_else(|| {
                        if turn.running() {
                            "（回答中…）".to_string()
                        } else {
                            "（暂无回答）".to_string()
                        }
                    });
                (question, answer)
            }
            None => (String::new(), String::new()),
        };
        let question_color = rgb(0x222222);
        let answer_color = rgb(0x777777);
        div()
            .id(format!("rail-popover-{turn_index}"))
            .w(px(300.0))
            .flex()
            .flex_col()
            .bg(rgb(0xffffff))
            .rounded(px(6.0))
            .border_1()
            .border_color(rgb(0xe3e3e3))
            .cursor_pointer()
            .on_hover(cx.listener(move |this, hovered, _window, cx| {
                if *hovered {
                    // Pointer inside the popover — cancel any pending close
                    // so the dot's 200ms grace window doesn't shut us down.
                    this.rail_popup_close_at = None;
                } else {
                    // Pointer fully left the popover. Schedule a zero-delay
                    // close (expires next render) so the card snaps hidden.
                    this.rail_popup_close_at = Some(std::time::Instant::now());
                }
                cx.notify();
            }))
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.rail_popup = None;
                this.rail_popup_close_at = None;
                this.jump_to_turn(turn_index, cx);
            }))
            // 问题：字号大、颜色深；回答：字号小、颜色浅，靠这两点区分，
            // 不再需要标题或分隔线。
            .child(
                div()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .text_size(sp(BODY_SIZE))
                    .text_color(question_color)
                    .child(question),
            )
            .child(
                div()
                    .px_2()
                    .pt_1()
                    .pb_2()
                    .text_size(sp(12.0))
                    .text_color(answer_color)
                    .child(answer),
            )
            .into_any_element()
    }

    pub(crate) fn render_floating_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.app.turns.is_empty() {
            return div().into_any_element();
        }
        div()
            .absolute()
            .bottom(px(20.0))
            .right(px(28.0))
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .p_1()
            .rounded(px(10.0))
            .bg(rgba(0x7f7f7f20))
            .border_1()
            .border_color(rgba(0x7f7f7f20))
            .child(
                Button::new("jump-latest")
                    .icon(IconName::ArrowDown)
                    .accessibility_label("跳转到底部")
                    .tooltip("跳转到底部")
                    .on_click(cx.listener(|this, _, _window, cx| this.jump_to_bottom(cx))),
            )
            .into_any_element()
    }
}
