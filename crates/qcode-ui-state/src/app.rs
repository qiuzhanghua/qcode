use crate::*;

#[derive(Debug, Clone)]
pub struct App {
    pub blocks: Vec<Block>,
    /// Turn view over `blocks`; block indices are stable for the session.
    pub turns: Vec<Turn>,
    /// The open turn, if any.
    pub current_turn: Option<usize>,
    /// Timestamp of the most recent applied event. A replayed log can end right
    /// after `turn_started` (the process died mid-turn); sealing that turn at
    /// this timestamp keeps it from rendering as forever running.
    pub last_ts: Option<OffsetDateTime>,
    /// Blocks that are not part of a turn (pre-session notices, skill dumps).
    pub loose_blocks: Vec<usize>,
    /// Live reasoning for the open turn, replaced by the authoritative message.
    pub reasoning_streaming: String,
    /// Block holding the live reasoning row so it keeps its place in the
    /// chronological process order (reasoning before the next tool call).
    pub streaming_reasoning_block: Option<usize>,
    /// Text streamed for the message currently being generated. Cleared when
    /// the authoritative `assistant_message` event arrives.
    pub streaming: String,
    /// Multi-line composer. Editing is toolkit-neutral; the renderer draws it.
    /// cursor movement; the app only decides when Enter submits (Ctrl-N and
    /// Shift/Alt+Enter insert a newline).
    pub composer: Composer,
    /// Rows scrolled up from the bottom of the transcript (0 = follow tail).
    pub scroll: usize,
    pub status: Status,
    /// Active sandbox backend (`none`, `sandbox-exec`, `bwrap`, `landlock`).
    /// Kept as a string so the TUI does not depend on `qcode-tools`.
    pub sandbox: String,
    /// Estimated prompt tokens reported by the engine before the last call.
    pub context_tokens: u64,
    /// Context window from `[context] window_tokens`; `None` until configured
    /// or reported by the engine.
    pub context_window: Option<u64>,
    /// Status-bar format for context usage (Ctrl-T cycles Auto/Percent/Tokens).
    pub context_display: ContextDisplay,
    /// Child groups in order, one per `subagent` call (child session ids).
    pub child_groups: Vec<String>,
    /// Child groups whose nested blocks are folded.
    pub collapsed_children: HashSet<String>,
    pub approval: Option<ApprovalPrompt>,
    /// Ctrl-C has been pressed once; the next one quits.
    pub quit_armed: bool,
    /// Skills available to this session, for the `/skills` debug commands.
    pub skills: Vec<SkillView>,
    /// An approval/probe answer was submitted and the engine has not confirmed
    /// it yet; the composer stays locked until `approval_decided` arrives.
    pub restoring: bool,
    /// `[ui] busy_delivery = "steer"`: sending while a turn runs steers the
    /// draft into it instead of queueing a follow-up.
    pub steer_on_send: bool,
}

impl App {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            blocks: Vec::new(),
            turns: Vec::new(),
            current_turn: None,
            last_ts: None,
            loose_blocks: Vec::new(),
            reasoning_streaming: String::new(),
            streaming_reasoning_block: None,
            streaming: String::new(),
            composer: Composer::new(),
            scroll: 0,
            status: Status {
                provider: provider.into(),
                model: model.into(),
                turn: 0,
                cost_usd: 0.0,
                running: false,
                last: None,
                queued: 0,
                steering: 0,
            },
            sandbox: "none".to_string(),
            context_tokens: 0,
            context_window: None,
            context_display: ContextDisplay::default(),
            child_groups: Vec::new(),
            collapsed_children: HashSet::new(),
            approval: None,
            quit_armed: false,
            skills: Vec::new(),
            restoring: false,
            steer_on_send: false,
        }
    }

    /// Clear the transcript before replaying a resumed session.
    ///
    /// Provider/model/sandbox/skills are configuration, not transcript state;
    /// the replayed `session_started` event refreshes the provider and model.
    pub fn reset_transcript(&mut self) {
        self.blocks.clear();
        self.turns.clear();
        self.current_turn = None;
        self.last_ts = None;
        self.loose_blocks.clear();
        self.reasoning_streaming.clear();
        self.streaming_reasoning_block = None;
        self.streaming.clear();
        self.scroll = 0;
        self.status.turn = 0;
        self.status.cost_usd = 0.0;
        self.status.running = false;
        self.status.last = None;
        self.context_tokens = 0;
        self.child_groups.clear();
        self.collapsed_children.clear();
        self.approval = None;
        self.restoring = false;
    }

    /// A non-text action from the frontend: submit, scroll, approve, quit…
    /// The composer state, for renderers and frontends (ADR-0042 §1).
    pub fn composer_state(&self) -> ComposerState {
        if self.approval.is_some() {
            ComposerState::Pending
        } else if self.restoring {
            ComposerState::Restoring
        } else if self.status.running {
            ComposerState::Running
        } else {
            ComposerState::Idle
        }
    }

    /// Turn a key press into an action. Unhandled keys return `None`.
    /// Apply one editing event. While an approval prompt is up, `y`/`a`/`n`
    /// answer it and everything else is swallowed.
    pub fn handle_input(&mut self, event: InputEvent) -> Option<UiAction> {
        self.quit_armed = false;
        if self.approval.is_some() {
            let InputEvent::Insert(ch) = event else {
                return None;
            };
            return match ch {
                'y' | 'Y' => self.answer(true, false),
                'a' | 'A' => self.answer(true, true),
                'n' | 'N' => self.answer(false, false),
                _ => None,
            };
        }
        // Typing while scrolled up returns to the live tail.
        self.scroll = 0;
        self.composer.apply(event);
        None
    }

    pub fn handle_action(&mut self, action: AppAction) -> Option<UiAction> {
        // Ctrl-C is special: the first press cancels a running turn, the second
        // quits, and any other key disarms it.
        if let AppAction::Interrupt = action {
            if self.quit_armed {
                return Some(UiAction::Quit);
            }
            self.quit_armed = true;
            return self.status.running.then_some(UiAction::Cancel);
        }
        self.quit_armed = false;

        if self.restoring && matches!(action, AppAction::Submit | AppAction::Steer) {
            // The decision is in flight: lock the interaction's submissions
            // until the engine confirms it (ADR-0042 §1, Restoring).
            return None;
        }

        // An approval prompt swallows everything except its own answers; the
        // event loop must not mutate the turn while a decision is pending.
        if self.approval.is_some() {
            return match action {
                AppAction::Approval(allow, session) => self.answer(allow, session),
                AppAction::Escape => self.answer(false, false),
                _ => None,
            };
        }

        match action {
            AppAction::Interrupt => None,
            AppAction::Approval(..) => None,
            AppAction::Newline => {
                self.composer.apply(InputEvent::Newline);
                None
            }
            AppAction::Submit => self.submit(),
            AppAction::Steer => self.steer(),
            AppAction::Escape => self.status.running.then_some(UiAction::Cancel),
            AppAction::PageUp => {
                self.scroll = self.scroll.saturating_add(SCROLL_STEP);
                None
            }
            AppAction::PageDown => {
                self.scroll = self.scroll.saturating_sub(SCROLL_STEP);
                None
            }
            // Arrows scroll only while the draft is a single line: a multi-line
            // draft still needs them to move the cursor.
            AppAction::Up => {
                if self.composer.line_count() <= 1 {
                    self.scroll = self.scroll.saturating_add(SCROLL_STEP);
                } else {
                    self.composer.apply(InputEvent::MoveUp);
                }
                None
            }
            AppAction::Down => {
                if self.composer.line_count() <= 1 {
                    self.scroll = self.scroll.saturating_sub(SCROLL_STEP);
                } else {
                    self.composer.apply(InputEvent::MoveDown);
                }
                None
            }
            AppAction::ScrollToTail => {
                self.scroll = 0;
                None
            }
            AppAction::ExpandLastTool => {
                self.toggle_last_tool();
                None
            }
            AppAction::CycleContext => {
                self.context_display = self.context_display.next();
                None
            }
            AppAction::ToggleChildGroup => {
                if let Some(group) = self.child_groups.last().cloned() {
                    self.toggle_child_group(&group);
                }
                None
            }
            AppAction::PasteImage => Some(UiAction::PasteImage),
        }
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.push_process(Block::Notice(text.into()));
    }

    /// Fold or unfold one sub-agent group.
    pub fn toggle_child_group(&mut self, group: &str) {
        if !self.collapsed_children.remove(group) {
            self.collapsed_children.insert(group.to_string());
        }
    }

    pub fn child_group_folded(&self, group: &str) -> bool {
        self.collapsed_children.contains(group)
    }

    /// Number of nested blocks belonging to a group (parent row label).
    pub fn child_block_count(&self, group: &str) -> usize {
        self.blocks
            .iter()
            .filter(|block| match block {
                Block::Subagent { group: id, .. } => id == group,
                Block::Tool(tool) => tool.child && tool.group.as_deref() == Some(group),
                _ => false,
            })
            .count()
    }

    /// Toggle one tool card by call id (GUI click).
    pub fn toggle_tool(&mut self, call_id: &str) {
        if let Some(Block::Tool(tool)) = self
            .blocks
            .iter_mut()
            .find(|block| matches!(block, Block::Tool(tool) if tool.call_id == call_id))
        {
            tool.expanded = !tool.expanded;
        }
    }

    pub(crate) fn toggle_last_tool(&mut self) {
        if let Some(tool) = self.blocks.iter_mut().rev().find_map(|block| match block {
            Block::Tool(tool) => Some(tool),
            _ => None,
        }) {
            tool.expanded = !tool.expanded;
        }
    }
}
