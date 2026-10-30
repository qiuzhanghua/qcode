use crate::*;

impl App {
    /// Project a persisted event into the transcript and status line.
    pub fn apply(&mut self, event: &Event) {
        self.last_ts = Some(event.ts);
        match &event.kind {
            EventKind::SessionStarted(started) => {
                self.status.provider = started.provider.clone();
                self.status.model = started.model.clone();
            }
            EventKind::TurnStarted(_) => {
                self.status.turn = event.turn;
                self.status.running = true;
                self.status.last = None;
                self.streaming.clear();
                self.reasoning_streaming.clear();
                self.streaming_reasoning_block = None;
                self.start_turn(event);
            }
            EventKind::UserMessage(message) => {
                // v4 marks injected steering explicitly (`starts_segment`), so
                // the projection no longer has to guess. v3 logs lack the flag;
                // there, a user message that arrives while the current turn
                // already has input is the steered follow-up, split into its own
                // turn (A and B each keep their process and rail dot).
                let v3_heuristic = event.v < 4
                    && self
                        .current_turn
                        .and_then(|index| self.turns.get(index))
                        .is_some_and(|turn| turn.input_block.is_some());
                if message.starts_segment || v3_heuristic {
                    self.split_turn(event);
                }
                self.ensure_turn(event);
                if let Some(name) = activated_skill(&message.text) {
                    self.push_input(Block::Skill(format!("activated skill {name}")));
                } else {
                    self.push_input(Block::User(message.text.clone()));
                }
            }
            EventKind::AssistantMessage(message) => {
                // The completed message supersedes the streamed previews, which
                // may have been cut short by cancellation.
                self.streaming.clear();
                self.reasoning_streaming.clear();
                self.ensure_turn(event);
                let turn = self.current_turn.expect("ensured turn");
                let mut live_reasoning = self.streaming_reasoning_block.take();
                let mut answer = String::new();
                for block in &message.blocks {
                    match block {
                        ContentBlock::Reasoning { text, .. } => {
                            let elapsed =
                                (event.ts - self.turns[turn].started_at).max(Duration::ZERO);
                            let replaced = live_reasoning.take().and_then(|index| {
                                match self.blocks.get_mut(index) {
                                    Some(Block::Reasoning(entry)) => {
                                        entry.text = text.clone();
                                        entry.started_at = self.turns[turn].started_at;
                                        entry.elapsed = Some(elapsed);
                                        Some(())
                                    }
                                    _ => None,
                                }
                            });
                            if replaced.is_none() {
                                self.push_process(Block::Reasoning(ReasoningEntry {
                                    text: text.clone(),
                                    started_at: self.turns[turn].started_at,
                                    elapsed: Some(elapsed),
                                }));
                            }
                        }
                        ContentBlock::Text { text } => {
                            if !answer.is_empty() {
                                answer.push_str("\n\n");
                            }
                            answer.push_str(text);
                        }
                        _ => {}
                    }
                }
                if !answer.is_empty() {
                    let index = self.append_block(Block::Assistant(answer));
                    self.turns[turn].answer_block = Some(index);
                }
            }
            EventKind::ToolCall(call) => {
                self.ensure_turn(event);
                self.push_process(Block::Tool(ToolEntry {
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    summary: summarize_call(&call.name, &call.arguments),
                    arguments: call.arguments.clone(),
                    status: None,
                    output: String::new(),
                    expanded: false,
                    started: event.ts,
                    elapsed: None,
                    child: false,
                    group: None,
                }));
            }
            EventKind::ToolResult(result) => {
                let todo_update = self.tool_mut(&result.call_id).and_then(|tool| {
                    tool.status = Some(result.status);
                    tool.output = result.output.clone();
                    tool.elapsed = Some((event.ts - tool.started).max(Duration::ZERO));
                    if result.status != ToolStatus::Ok {
                        tool.expanded = true;
                    }
                    if tool.name == "todo_write" && result.status == ToolStatus::Ok {
                        todo::from_arguments(&tool.arguments).ok()
                    } else {
                        None
                    }
                });
                if let Some(items) = todo_update {
                    if items.is_empty() {
                        self.push_process(Block::Notice("todo list cleared".to_string()));
                    } else {
                        self.push_process(Block::Todo(items));
                    }
                }
            }
            EventKind::ApprovalRequested(request) => {
                let (tool, summary) = self
                    .tool_mut(&request.call_id)
                    .map(|tool| (tool.name.clone(), tool.summary.clone()))
                    .unwrap_or_else(|| (request.call_id.clone(), String::new()));
                self.approval = Some(ApprovalPrompt {
                    call_id: request.call_id.clone(),
                    tool,
                    summary,
                    reason: request.reason.clone(),
                    rule: request.rule_id.clone(),
                    argv: None,
                    cwd: None,
                });
            }
            EventKind::ApprovalDecided(_) => {
                self.approval = None;
                self.restoring = false;
            }
            EventKind::TurnEnded(ended) => {
                self.restoring = false;
                self.streaming.clear();
                self.reasoning_streaming.clear();
                self.streaming_reasoning_block = None;
                self.status.running = false;
                self.status.last = Some(ended.status);
                self.status.cost_usd = ended.cost_usd;
                self.close_turn(event, ended.status, ended.usage.clone(), ended.cost_usd);
            }
            EventKind::Error(error) => {
                self.push_process(Block::Notice(format!("{}: {}", error.kind, error.message)));
            }
            EventKind::Compaction(_) => {
                self.push_process(Block::Notice("context compacted".to_string()));
            }
            // Task/root configuration is not rendered in the transcript yet.
            EventKind::TaskConfigured(_)
            | EventKind::TaskRenamed(_)
            | EventKind::RootsChanged(_) => {}
        }
    }

    /// Project an event from a child agent.
    ///
    /// Child turns must not move the parent status machine, so this only
    /// appends nested transcript rows. Tool call ids are namespaced with the
    /// child session id to avoid clashing with parent calls.
    pub fn apply_child(&mut self, event: &Event) {
        let group = event.session.clone();
        match &event.kind {
            EventKind::SessionStarted(started) => {
                if !self.child_groups.iter().any(|known| known == &group) {
                    self.child_groups.push(group.clone());
                }
                // Bind the child session to the parent `subagent` row that
                // spawned it. Sub-agent calls are serial, so the newest
                // unbound row is the right one.
                if let Some(tool) = self.blocks.iter_mut().rev().find_map(|block| match block {
                    Block::Tool(tool)
                        if !tool.child && tool.name == "subagent" && tool.group.is_none() =>
                    {
                        Some(tool)
                    }
                    _ => None,
                }) {
                    tool.group = Some(group.clone());
                }
                self.push_process(Block::Subagent {
                    text: format!(
                        "subagent {} started ({}/{})",
                        short_id(&group),
                        started.provider,
                        started.model
                    ),
                    group,
                });
            }
            EventKind::AssistantMessage(message) => {
                for block in &message.blocks {
                    if let ContentBlock::Text { text } = block {
                        self.push_process(Block::Subagent {
                            text: text.clone(),
                            group: group.clone(),
                        });
                    }
                }
            }
            EventKind::ToolCall(call) => {
                self.push_process(Block::Tool(ToolEntry {
                    call_id: child_call_id(&group, &call.call_id),
                    name: call.name.clone(),
                    summary: summarize_call(&call.name, &call.arguments),
                    arguments: call.arguments.clone(),
                    status: None,
                    output: String::new(),
                    expanded: false,
                    started: event.ts,
                    elapsed: None,
                    child: true,
                    group: Some(group),
                }));
            }
            EventKind::ToolResult(result) => {
                let call_id = child_call_id(&group, &result.call_id);
                if let Some(tool) = self.tool_mut(&call_id) {
                    tool.status = Some(result.status);
                    tool.output = result.output.clone();
                    tool.elapsed = Some((event.ts - tool.started).max(Duration::ZERO));
                }
            }
            EventKind::Error(error) => {
                self.push_process(Block::Subagent {
                    text: format!("error: {}: {}", error.kind, error.message),
                    group,
                });
            }
            EventKind::Compaction(_) => {
                self.push_process(Block::Subagent {
                    text: "context compacted".to_string(),
                    group,
                });
            }
            EventKind::TurnEnded(ended) => {
                self.push_process(Block::Subagent {
                    text: format!("finished ({:?})", ended.status),
                    group,
                });
            }
            // Approval prompts come through the approver channel; the child
            // never owns the parent status line.
            EventKind::UserMessage(_)
            | EventKind::TurnStarted(_)
            | EventKind::ApprovalRequested(_)
            | EventKind::ApprovalDecided(_)
            | EventKind::TaskConfigured(_)
            | EventKind::TaskRenamed(_)
            | EventKind::RootsChanged(_) => {}
        }
    }

    /// Append a transient stream delta. Reasoning is kept in its own buffer
    /// so the GUI can show a Thinking row before the authoritative message.
    pub fn apply_delta(&mut self, delta: &Delta) {
        match delta {
            Delta::Text(text) => self.streaming.push_str(text),
            Delta::Context { tokens, window } => {
                self.context_tokens = *tokens;
                self.context_window = Some(*window);
            }
            Delta::Reasoning(text) => {
                self.reasoning_streaming.push_str(text);
                let current = self.reasoning_streaming.clone();
                let index = match self.streaming_reasoning_block {
                    Some(index) => index,
                    None => {
                        let index = self.push_process(Block::Reasoning(ReasoningEntry {
                            text: current.clone(),
                            started_at: OffsetDateTime::now_utc(),
                            elapsed: None,
                        }));
                        self.streaming_reasoning_block = Some(index);
                        index
                    }
                };
                if let Some(Block::Reasoning(entry)) = self.blocks.get_mut(index) {
                    entry.text = current;
                }
            }
        }
    }

    pub(crate) fn start_turn(&mut self, event: &Event) {
        // A malformed/truncated log may start a turn without ending the
        // previous one, and an aborted run may leave several split turns open.
        // Close every still-open turn at this timestamp instead of leaving it
        // permanently "running".
        for turn in &mut self.turns {
            if turn.ended_at.is_none() {
                turn.ended_at = Some(event.ts.max(turn.started_at));
            }
        }
        let index = self.turns.len();
        self.turns.push(Turn {
            id: event.seq,
            number: event.turn,
            started_at: event.ts,
            ended_at: None,
            status: None,
            input_block: None,
            process: Vec::new(),
            answer_block: None,
            usage: Usage::default(),
            cost_usd: 0.0,
        });
        self.current_turn = Some(index);
    }

    /// A queued follow-up message steered into the running turn: the backend
    /// appends it inside the same turn number without a new `turn_started`, so
    /// the UI splits it into its own question here. Unlike [`start_turn`] this
    /// must not close the parent turn — it stays open until the run's
    /// `turn_ended` closes every split turn together.
    pub(crate) fn split_turn(&mut self, event: &Event) {
        let index = self.turns.len();
        self.turns.push(Turn {
            id: event.seq,
            number: event.turn,
            started_at: event.ts,
            ended_at: None,
            status: None,
            input_block: None,
            process: Vec::new(),
            answer_block: None,
            usage: Usage::default(),
            cost_usd: 0.0,
        });
        self.current_turn = Some(index);
    }

    pub(crate) fn ensure_turn(&mut self, event: &Event) -> usize {
        match self.current_turn {
            Some(index) => index,
            None => {
                self.start_turn(event);
                self.current_turn.expect("just started")
            }
        }
    }

    pub(crate) fn append_block(&mut self, block: Block) -> usize {
        let index = self.blocks.len();
        self.blocks.push(block);
        index
    }

    pub(crate) fn push_process(&mut self, block: Block) -> usize {
        let index = self.append_block(block);
        match self.current_turn {
            Some(turn) => self.turns[turn].process.push(index),
            None => self.loose_blocks.push(index),
        }
        index
    }

    pub(crate) fn push_input(&mut self, block: Block) -> usize {
        let index = self.append_block(block);
        match self.current_turn {
            Some(turn) => {
                if self.turns[turn].input_block.is_none() {
                    self.turns[turn].input_block = Some(index);
                } else {
                    self.turns[turn].process.push(index);
                }
            }
            None => self.loose_blocks.push(index),
        }
        index
    }

    pub(crate) fn close_turn(
        &mut self,
        event: &Event,
        status: TurnStatus,
        usage: Usage,
        cost_usd: f64,
    ) {
        // 排队追加的消息被拆成独立 turn 后，一次 run 可能同时开着多个 turn
        // （A 和插入其中的 B）；`turn_ended` 属于整个 run，把仍开着的全部收口。
        self.current_turn = None;
        let ts = event.ts;
        for turn in &mut self.turns {
            if turn.ended_at.is_none() {
                turn.ended_at = Some(ts.max(turn.started_at));
                turn.status = Some(status.clone());
                turn.usage = usage.clone();
                turn.cost_usd = cost_usd;
            }
        }
    }

    /// 历史事件重放结束后调用。
    ///
    /// 重放里的 `TurnStarted` 只说明“当时开始过”，不代表这个进程里现在
    /// 还有 turn 在跑。恢复被中断的会话时日志没有对应的 `TurnEnded`，
    /// 不复位就会一直显示“运行中”，还会挡住输入和切换会话。
    pub fn finish_replay(&mut self) {
        self.status.running = false;
        self.restoring = false;
        // 日志可能停在 `turn_started`（进程在 turn 中途退出，没有 TurnEnded）。
        // 用最后一个事件的时间戳把仍开着的 turn（含排队追加拆出的）全部封口，
        // 否则它们会一直显示“running”，耗时持续累加，执行过程也永远打不开。
        let ended = self.last_ts;
        self.current_turn = None;
        for turn in &mut self.turns {
            if turn.ended_at.is_none() {
                turn.ended_at = Some(ended.unwrap_or(turn.started_at).max(turn.started_at));
                turn.status = Some(TurnStatus::Cancelled);
            }
        }
    }

    /// 引擎在发出 `TurnEnded` 之前就结束了（运行失败/被中断）时收口仍开着的
    /// turn（含排队追加拆出的）。
    ///
    /// 没有这一步，`Turn::running()` 会一直是 true：卡片上的耗时不停增长，
    /// 执行过程被强制展开而无法折叠。
    pub fn abort_turn(&mut self, status: TurnStatus) {
        self.restoring = false;
        let ended = OffsetDateTime::now_utc();
        self.current_turn = None;
        for turn in &mut self.turns {
            if turn.ended_at.is_none() {
                turn.ended_at = Some(ended.max(turn.started_at));
                turn.status = Some(status.clone());
            }
        }
        self.status.running = false;
    }

    pub(crate) fn tool_mut(&mut self, call_id: &str) -> Option<&mut ToolEntry> {
        self.blocks.iter_mut().rev().find_map(|block| match block {
            Block::Tool(tool) if tool.call_id == call_id => Some(tool),
            _ => None,
        })
    }
}
