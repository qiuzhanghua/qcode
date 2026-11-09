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
        if self.ui.app.turns.len() < 2 || window.bounds().size.width.as_f32() < 900.0 {
            return div().into_any_element();
        }
        let total = self.ui.app.turns.len();
        let active = self.active_turn_index();
        // The rail is a ruler: every turn gets a tick, right-aligned, at a
        // pitch that adapts to the transcript height. When a long session would
        // squeeze the pitch under `MIN_PITCH`, ticks are sampled instead of
        // shrunk — the active one is always drawn.
        const MAX_PITCH: f32 = 14.0;
        const MIN_PITCH: f32 = 6.0;
        let available = (self.transcript_height.get() - 32.0).max(200.0);
        let mut stride = 1usize;
        let mut pitch = (available / total as f32).min(MAX_PITCH);
        while pitch < MIN_PITCH {
            stride += 1;
            let shown = total.div_ceil(stride);
            pitch = available / shown.max(1) as f32;
        }
        let mut rail = div()
            .w(px(TURN_RAIL_WIDTH))
            .flex()
            .flex_col()
            .items_end()
            .justify_center();
        let mut indices: Vec<usize> = (0..total).step_by(stride).collect();
        if let Some(active) = active
            && !indices.contains(&active)
        {
            indices.push(active);
            indices.sort_unstable();
        }
        for index in indices {
            // Right-aligned ticks on a shared edge, like a ruler: every turn is
            // a faint 11×3 hairline, the current one is near-black and grows
            // leftwards to 28×3.5, so the marker reads as a longer dash rather
            // than a different colour.
            let current = active == Some(index);
            let (bar_width, bar_height, bar_color) = if current {
                (28.0, 3.5, rgb(0x141414))
            } else {
                (11.0, 3.0, rgb(0xe2e2e2))
            };
            let show_popover = self.rail_popup == Some(index);
            // The tick is a hairline, so the pointer gets the whole row (pitch
            // tall, rail wide) as its hit target.
            let dot = div()
                .w(px(TURN_RAIL_WIDTH))
                .h(px(pitch))
                .flex()
                .items_center()
                .justify_end()
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
                }))
                .child(
                    div()
                        .w(px(bar_width))
                        .h(px(bar_height))
                        .rounded(px(bar_height / 2.0))
                        .bg(bar_color)
                        .hover(|style| style.bg(rgb(0x9a9a9a))),
                );
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
    /// answer as two message cards, each shaped like the transcript itself —
    /// a bold first line and the remaining lines in muted grey. Clicking
    /// anywhere in the card jumps to that turn.
    ///
    /// The card is allowed half of the *transcript* viewport (not the window:
    /// the composer must keep its space), and the text is fitted to that
    /// budget rather than clipped mid-line.
    pub(crate) fn build_rail_popover(
        &mut self,
        cx: &mut Context<Self>,
        turn_index: usize,
    ) -> gpui_kit::AnyElement {
        let budget = (self.transcript_height.get() * 0.5).clamp(220.0, 620.0);
        // 14px padding top and bottom, two 22px titles, a 13px divider, gaps.
        const CHROME: f32 = 96.0;
        const LINE: f32 = 20.0;
        let body_total = (((budget - CHROME) / LINE).floor() as usize).max(2);
        let question_lines = (body_total * 3 / 5).max(1);
        let answer_lines = body_total.saturating_sub(question_lines).max(1);

        let (question_title, question_body) = match self.ui.app.turns.get(turn_index) {
            Some(turn) => turn
                .input_block
                .and_then(|idx| self.ui.app.blocks.get(idx))
                .and_then(|block| match block {
                    Block::User { text, .. } => Some(head_and_body(text)),
                    _ => None,
                })
                .unwrap_or_else(|| (format!("第 {} 轮", turn.number), String::new())),
            None => (String::new(), String::new()),
        };
        let (answer_title, answer_body) = match self.ui.app.turns.get(turn_index) {
            Some(turn) => turn
                .answer_block
                .and_then(|idx| self.ui.app.blocks.get(idx))
                .and_then(|block| match block {
                    Block::Assistant(text) => Some(head_and_body(text)),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    if turn.running() {
                        ("（回答中…）".to_string(), String::new())
                    } else {
                        ("（暂无回答）".to_string(), String::new())
                    }
                }),
            None => (String::new(), String::new()),
        };

        div()
            .id(format!("rail-popover-{turn_index}"))
            .w(px(520.0))
            .max_h(px(budget))
            .flex()
            .flex_col()
            .gap_2()
            .p(px(14.0))
            .overflow_hidden()
            .bg(rgb(0xffffff))
            .rounded(px(12.0))
            .border_1()
            .border_color(rgb(0xe6e6e6))
            // The card floats over the transcript; without elevation it reads
            // as part of the conversation.
            .shadow_lg()
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
            // A click has to be caught on the way *down*: the popup dismisses
            // itself when the press lands, so an `on_click` on the card would
            // never fire — the element is gone before the button comes up.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _window, cx| {
                    this.rail_popup = None;
                    this.rail_popup_close_at = None;
                    this.jump_to_turn(turn_index, cx);
                }),
            )
            .child(message_section(
                &question_title,
                &question_body,
                question_lines,
                rgb(0x111111),
                rgb(0x6b6b6b),
            ))
            .child(div().h(px(1.0)).flex_none().bg(rgb(0xe8e8e8)))
            .child(message_section(
                &answer_title,
                &answer_body,
                answer_lines,
                rgb(0x333333),
                rgb(0x808080),
            ))
            .into_any_element()
    }

    pub(crate) fn render_floating_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.ui.app.turns.is_empty() {
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

/// Splits a message into a title line and the rest. Only the title loses its
/// markdown noise (`#`, `>`, bullets, leading spaces) — the body stays
/// verbatim, markdown included, because this is a preview and not rendered
/// output.
pub(crate) fn head_and_body(text: &str) -> (String, String) {
    let trimmed = text.trim();
    let mut lines = trimmed.lines();
    let title = lines
        .next()
        .map(|line| {
            line.trim_start_matches(['#', '>', '-', '*', ' '])
                .trim()
                .to_string()
        })
        .unwrap_or_default();
    let body = lines.collect::<Vec<_>>().join("\n");
    (title, body.trim().to_string())
}

/// Fits `text` into `budget` visual lines, appending an ellipsis when it does
/// not fit. A visual line is a rough wrap estimate: 44 characters covers a
/// wide CJK line on a 520px card, and over-estimating only costs a few unused
/// pixels.
pub(crate) fn fit_lines(text: &str, budget: usize) -> String {
    const PER_LINE: usize = 44;
    if text.is_empty() || budget == 0 {
        return String::new();
    }
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0;
    for line in text.lines() {
        let span = line.chars().count().max(1).div_ceil(PER_LINE);
        if used + span <= budget {
            used += span;
            kept.push(line.to_string());
            continue;
        }
        let room = budget.saturating_sub(used);
        if room > 0 {
            let mut clipped: String = line.chars().take(room * PER_LINE - 1).collect();
            clipped.push('…');
            kept.push(clipped.trim_end().to_string());
        } else if let Some(last) = kept.last_mut() {
            last.push('…');
        }
        return kept.join("\n");
    }
    kept.join("\n")
}

/// One message card section: bold first line, muted body.
fn message_section(
    title: &str,
    body: &str,
    body_lines: usize,
    title_color: impl Into<Hsla>,
    body_color: impl Into<Hsla>,
) -> Div {
    let (title_color, body_color) = (title_color.into(), body_color.into());
    let body = fit_lines(body, body_lines);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .min_w_0()
        .child(
            div()
                .text_size(sp(15.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(title_color)
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .child(title.to_string()),
        )
        .children((!body.is_empty()).then(|| {
            div()
                .text_size(sp(13.0))
                .line_height(sp(20.0))
                .text_color(body_color)
                .child(body)
        }))
}
