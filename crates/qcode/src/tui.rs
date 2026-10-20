//! Terminal frontend: crossterm setup, the event loop, and the channel-based
//! approver that bridges `run_turn` and the UI.
//!
//! The state machine and renderer live in `qcode-tui`; this module owns the
//! parts that need a real terminal. The engine runs as a future polled by the
//! same select as keyboard input, so no `'static` task or shared ownership is
//! needed: events reach the UI through an unbounded channel, approvals travel
//! back through a one-shot.

use std::future::Future;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use qcode_core::approval::{Answer, ApprovalRequest, Approver};
use qcode_core::cost::Pricing;
use qcode_core::engine::{
    Delta, DeltaSink, EngineError, RunOptions, RunOutcome, TurnContext, run_turn,
};
use qcode_core::event::{Event, SessionStarted, TurnStatus};
use qcode_core::policy::Policy;
use qcode_core::provider::Provider;
use qcode_core::store::SessionStore;
use qcode_core::tool::{Tool, ToolRegistry};
use qcode_tools::subagent::SubagentTool;
use qcode_tui::{App, ApprovalPrompt, SkillView, UiAction, render};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyEvent, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub struct TuiOptions<'a> {
    pub store: &'a SessionStore,
    pub provider: Arc<dyn Provider>,
    pub tools: &'a ToolRegistry,
    pub policy: &'a Policy,
    pub workspace: PathBuf,
    pub provider_name: &'a str,
    pub model: &'a str,
    pub pricing: Option<Pricing>,
    pub max_cost_usd: Option<f64>,
    pub max_turns: u32,
    pub context_window: Option<u64>,
    pub agents_md: Option<String>,
    /// Rendered `# Skills` index for the system prompt.
    pub skills_prompt: Option<String>,
    /// Skills for the local `/skills` debug command.
    pub skills: Vec<SkillView>,
    /// Active sandbox backend name, shown in the status bar.
    pub sandbox: String,
    /// Existing session to resume; `None` creates a new one.
    pub resume: Option<String>,
    /// Also snapshot the workspace before run_shell ([agent] snapshot_shell).
    pub snapshot_shell: bool,
    /// Egress proxy shared with tools and child agents.
    pub network: Option<qcode_core::network::SharedNetwork>,
}

enum TuiMsg {
    Event(Event),
    /// Progress from a child agent, rendered nested and not applied to the
    /// parent status machine.
    Child(Event),
    Delta(Delta),
    Key(KeyEvent),
    Approval {
        call_id: String,
        tool: String,
        summary: String,
        reason: String,
        rule: Option<String>,
        argv: Option<String>,
        cwd: Option<String>,
        reply: oneshot::Sender<Answer>,
    },
}

/// Bridges the engine's `Approver` port to the UI loop. The engine awaits a
/// one-shot; the UI answers it when the user presses y or n.
pub struct TuiApprover {
    tx: mpsc::UnboundedSender<TuiMsg>,
    /// Tools the user allowed for the rest of the session. A skill run asks for
    /// `run_shell` dozens of times; answering every one is friction, not
    /// safety, because a denial is one keystroke away either way.
    session_allowed: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

impl TuiApprover {
    fn new(tx: mpsc::UnboundedSender<TuiMsg>) -> Self {
        Self {
            tx,
            session_allowed: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        }
    }
}

#[async_trait]
impl Approver for TuiApprover {
    async fn approve(&self, request: &ApprovalRequest, cancel: CancellationToken) -> Answer {
        if self
            .session_allowed
            .lock()
            .map(|allowed| allowed.contains(&request.tool))
            .unwrap_or(false)
        {
            return Answer::Allow;
        }

        let (reply, answer) = oneshot::channel();
        if self
            .tx
            .send(TuiMsg::Approval {
                call_id: request.call_id.clone(),
                tool: request.tool.clone(),
                summary: request.summary.clone(),
                reason: request.reason.clone(),
                rule: request.rule_id.clone(),
                argv: request.argv.clone(),
                cwd: request.cwd.clone(),
                reply,
            })
            .is_err()
        {
            return Answer::Deny;
        }

        let answer = tokio::select! {
            biased;
            _ = cancel.cancelled() => Answer::Deny,
            answer = answer => answer.unwrap_or(Answer::Deny),
        };
        if answer == Answer::AllowSession
            && let Ok(mut allowed) = self.session_allowed.lock()
        {
            allowed.insert(request.tool.clone());
        }
        answer
    }
}

pub async fn run(options: TuiOptions<'_>) -> Result<(), Box<dyn std::error::Error>> {
    // Validate the session before touching the terminal.
    let history = options
        .resume
        .as_ref()
        .map(|id| options.store.repair_interrupted(id))
        .transpose()?;
    let session_id = match (&options.resume, &history) {
        (Some(id), Some(_)) => id.clone(),
        _ => {
            let (id, _) = options.store.create(&SessionStarted {
                model: options.model.to_string(),
                provider: options.provider_name.to_string(),
                cwd: options.workspace.display().to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            })?;
            id
        }
    };

    let (tx, mut rx) = mpsc::unbounded_channel();
    let approver: Arc<dyn Approver> = Arc::new(TuiApprover::new(tx.clone()));
    let input = InputThread::spawn(tx.clone());
    let mut terminal = TerminalGuard::enter()?;

    // The child registry excludes `subagent`, so nesting stops at depth one.
    let mut tools = options.tools.clone();
    let child_sender = tx.clone();
    let on_child: crate::subagent::ChildEventSink = Arc::new(move |event: &Event| {
        let _ = child_sender.send(TuiMsg::Child(event.clone()));
    });
    // Pre-images for `/rollback`: same data dir as the session log.
    let snapshots = qcode_core::snapshot::SnapshotStore::at_data_dir(options.store.root())
        .with_shell(options.snapshot_shell);
    let runner = crate::subagent::build(
        options.store,
        options.provider.clone(),
        tools.clone(),
        options.policy,
        approver.clone(),
        &options.workspace,
        options.model,
        options.context_window,
        options.pricing,
        options.network.clone(),
        Some(snapshots.clone()),
        Some(on_child),
    );
    tools.extend([Arc::new(SubagentTool::new(runner)) as Arc<dyn Tool>]);

    let mut app = App::new(options.provider_name, options.model);
    app.skills = options.skills.clone();
    app.sandbox = options.sandbox.clone();
    app.context_window = options.context_window;
    if let Some(events) = &history {
        for event in events {
            app.apply(event);
        }
        // A resumed session is idle even if its last turn was interrupted.
        app.streaming.clear();
        app.status.running = false;
        app.status.provider = options.provider_name.to_string();
        app.status.model = options.model.to_string();
        app.push_notice(format!(
            "resumed session {session_id} ({} events) · Enter 发送 · Ctrl-N 换行 · Esc 取消 · Ctrl-O 展开 · Ctrl-B 子组 · Ctrl-T ctx 显示 · ↑/↓ 或 Ctrl-U/D 滚动 · /subagents 子组 · /checkpoint 检查点 · /rollback <turn> · Ctrl-V 粘贴图片 · /exit 退出 · Ctrl-C 两次退出",
            events.len()
        ));
    } else {
        app.push_notice(format!(
            "session {session_id} · Enter 发送 · Ctrl-N 换行 · Esc 取消 · Ctrl-O 展开 · Ctrl-B 子组 · Ctrl-T ctx 显示 · ↑/↓ 或 Ctrl-U/D 滚动 · /skills 技能 · /subagents 子组 · /checkpoint 检查点 · /rollback <turn> · Ctrl-V 粘贴图片 · /exit 退出 · Ctrl-C 两次退出"
        ));
    }

    // Pasted images wait here until the next submit.
    let mut pending_images: Vec<qcode_core::engine::Attachment> = Vec::new();
    let mut cancel = CancellationToken::new();
    let mut pending_approval: Option<(String, oneshot::Sender<Answer>)> = None;
    let mut engine_running = false;
    let mut engine: Pin<Box<dyn Future<Output = Result<RunOutcome, EngineError>> + '_>> =
        Box::pin(std::future::pending());

    // Coalesce stream deltas into at most one draw per frame (60Hz).
    let mut ticker = tokio::time::interval(Duration::from_millis(16));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;

    let loop_result: Result<(), Box<dyn std::error::Error>> = loop {
        tokio::select! {
            biased;
            _ = ticker.tick() => {
                if dirty {
                    if let Err(err) = terminal.terminal.draw(|frame| render(frame, &app)) {
                        break Err(err.into());
                    }
                    dirty = false;
                }
            }
            message = rx.recv() => {
                dirty = true;
                let Some(message) = message else { break Ok(()) };
                match message {
                    TuiMsg::Event(event) => app.apply(&event),
                    TuiMsg::Child(event) => app.apply_child(&event),
                    TuiMsg::Key(key) => {
                        let Some(press) = qcode_tui::translate(key) else {
                            continue;
                        };
                        let action = match press {
                            qcode_tui::KeyPress::Input(event) => app.handle_input(event),
                            qcode_tui::KeyPress::Action(action) => app.handle_action(action),
                        };
                        let Some(action) = action else { continue };
                        match action {
                            UiAction::Submit(text) => {
                                // A fresh token per turn: cancelling one turn
                                // must not poison the next one.
                                cancel = CancellationToken::new();
                                pending_approval = None;
                                engine_running = true;
                                let turn_cancel = cancel.clone();
                                let sender = tx.clone();
                                let delta_sender = tx.clone();
                                engine = Box::pin(run_turn(
                                    TurnContext {
                                        store: options.store,
                                        provider: options.provider.as_ref(),
                                        tools: &tools,
                                        policy: options.policy,
                                        approver: approver.as_ref(),
                                        snapshots: Some(&snapshots),
                                    },
                                    RunOptions {
                                        session_id: &session_id,
                                        model: options.model,
                                        input: text,
                                        workspace: options.workspace.clone(),
                                        max_turns: options.max_turns,
                                        cancel: turn_cancel,
                                        pricing: options.pricing,
                                        max_cost_usd: options.max_cost_usd,
                                        on_delta: Some(DeltaSink::new(move |delta| {
                                            let _ = delta_sender.send(TuiMsg::Delta(delta));
                                        })),
                                        context_window: options.context_window,
                                        agents_md: options.agents_md.clone(),
                                        skills: options.skills_prompt.clone(),
                                        network: options.network.clone(),
                                        attachments: std::mem::take(&mut pending_images),
                                    },
                                    move |event| {
                                        let _ = sender.send(TuiMsg::Event(event.clone()));
                                    },
                                ));
                            }
                            UiAction::Approve {
                                call_id,
                                allow,
                                session,
                            } => {
                                if let Some((pending_call, reply)) = pending_approval.take()
                                    && pending_call == call_id
                                {
                                    let _ = reply.send(match (allow, session) {
                                        (true, true) => Answer::AllowSession,
                                        (true, false) => Answer::Allow,
                                        (false, _) => Answer::Deny,
                                    });
                                }
                            }
                            UiAction::PasteImage => match crate::clipboard::read_image() {
                                Ok(image) => {
                                    use base64::Engine as _;
                                    let size_kb = image.png.len() / 1024;
                                    pending_images.push(qcode_core::engine::Attachment {
                                        mime_type: "image/png".to_string(),
                                        data_base64: base64::engine::general_purpose::STANDARD
                                            .encode(&image.png),
                                    });
                                    app.push_notice(format!(
                                        "image attached: {}x{} ({} KB) — sent with the next message ({} pending)",
                                        image.width,
                                        image.height,
                                        size_kb,
                                        pending_images.len()
                                    ));
                                }
                                Err(error) => app.push_notice(format!("paste failed: {error}")),
                            },
                            UiAction::Cancel => cancel.cancel(),
                            UiAction::Checkpoints => {
                                let turns = snapshots.turns(&session_id).unwrap_or_default();
                                if turns.is_empty() {
                                    app.push_notice(
                                        "no checkpoints yet: they are recorded when the agent writes a file"
                                            .to_string(),
                                    );
                                } else {
                                    for turn in turns {
                                        let files = snapshots
                                            .files(&session_id, turn)
                                            .unwrap_or_default();
                                        app.push_notice(format!(
                                            "turn {turn}: {} file(s) · {}",
                                            files.len(),
                                            files.join(", ")
                                        ));
                                    }
                                }
                            }
                            UiAction::Rollback(turn) => {
                                match snapshots.restore(&session_id, turn, &options.workspace) {
                                    Ok(report) => app.push_notice(format!(
                                        "rolled back to before turn {turn}: restored {}, removed {} created (turns {:?})",
                                        report.restored, report.deleted, report.turns
                                    )),
                                    Err(error) => {
                                        app.push_notice(format!("rollback failed: {error}"))
                                    }
                                }
                            }
                            UiAction::Quit => {
                                // Only a live turn needs the shutdown grace: an
                                // idle TUI must quit immediately.
                                if engine_running {
                                    cancel.cancel();
                                    let _ = tokio::time::timeout(
                                        Duration::from_secs(3),
                                        &mut engine,
                                    )
                                    .await;
                                }
                                break Ok(());
                            }
                        }
                    }
                    TuiMsg::Delta(delta) => app.apply_delta(&delta),
                    TuiMsg::Approval {
                        call_id,
                        tool,
                        summary,
                        reason,
                        rule,
                        argv,
                        cwd,
                        reply,
                    } => {
                        // Child agents do not forward their tool events, so the
                        // prompt has to come from the approval request itself.
                        app.approval = Some(ApprovalPrompt {
                            call_id: call_id.clone(),
                            tool,
                            summary,
                            reason,
                            rule,
                            argv,
                            cwd,
                        });
                        pending_approval = Some((call_id, reply));
                    }
                }
            }
            result = &mut engine => {
                dirty = true;
                engine = Box::pin(std::future::pending());
                engine_running = false;
                if let Err(err) = result {
                    app.streaming.clear();
                    app.status.running = false;
                    app.status.last = Some(TurnStatus::Failed);
                    app.push_notice(format!("engine error: {err}"));
                }
            }
        }
    };

    drop(input);
    terminal.restore();
    loop_result
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    restored: bool,
}

/// Restore the terminal before a panic message is printed.
///
/// The default hook writes to stderr, which inside the alternate screen is a
/// buffer thrown away on restore: a panic during the first frame looked like
/// "the TUI silently exited" with no message at all. Leaving the screen first
/// keeps the message where the user can read it.
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, PopKeyboardEnhancementFlags, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            default_hook(info);
        }));
    });
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(err);
        }
        install_panic_hook();

        // Progressive keyboard enhancement is a nicety: it makes Shift+Enter
        // and Esc arrive as distinct keys. The legacy Windows console API does
        // not implement it and answers with an error, which must not abort the
        // TUI — Ctrl-N still inserts a newline there.
        if let Err(err) = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        ) {
            tracing::debug!(%err, "terminal has no keyboard enhancement; Ctrl-N for newline");
        }

        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self {
                terminal,
                restored: false,
            }),
            Err(err) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(err)
            }
        }
    }

    fn restore(&mut self) {
        if self.restored {
            return;
        }
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            PopKeyboardEnhancementFlags,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
        self.restored = true;
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Blocking crossterm reads on their own thread keep the async loop responsive
/// without depending on crossterm's optional event-stream feature.
struct InputThread {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl InputThread {
    fn spawn(tx: mpsc::UnboundedSender<TuiMsg>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => match event::read() {
                        Ok(TermEvent::Key(key)) => {
                            if tx.send(TuiMsg::Key(key)).is_err() {
                                break;
                            }
                        }
                        // Resize and mouse events are handled by the next draw.
                        Ok(_) => {}
                        Err(_) => break,
                    },
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for InputThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            call_id: "call-1".to_string(),
            tool: "run_shell".to_string(),
            summary: "echo hi".to_string(),
            reason: "process execution".to_string(),
            rule_id: None,
            argv: Some("echo hi".to_string()),
            cwd: Some("/tmp".to_string()),
        }
    }

    #[tokio::test]
    async fn approval_round_trips_through_the_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approver = TuiApprover::new(tx);
        let handle =
            tokio::spawn(
                async move { approver.approve(&request(), CancellationToken::new()).await },
            );

        let TuiMsg::Approval { call_id, reply, .. } = rx.recv().await.expect("approval message")
        else {
            panic!("unexpected message");
        };
        assert_eq!(call_id, "call-1");
        reply.send(Answer::Allow).expect("send answer");
        assert_eq!(handle.await.expect("join"), Answer::Allow);
    }

    #[tokio::test]
    async fn a_session_answer_stops_asking_for_that_tool() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approver = std::sync::Arc::new(TuiApprover::new(tx));

        let first = {
            let approver = approver.clone();
            tokio::spawn(
                async move { approver.approve(&request(), CancellationToken::new()).await },
            )
        };
        let TuiMsg::Approval { reply, .. } = rx.recv().await.expect("approval message") else {
            panic!("unexpected message");
        };
        reply.send(Answer::AllowSession).expect("send answer");
        assert_eq!(first.await.expect("join"), Answer::AllowSession);

        // The second call is answered without another prompt.
        let second = approver.approve(&request(), CancellationToken::new()).await;
        assert_eq!(second, Answer::Allow);
        assert!(rx.try_recv().is_err(), "the UI should not be asked again");
    }

    #[tokio::test]
    async fn cancellation_denies_without_an_answer() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let approver = TuiApprover::new(tx);
        let cancel = CancellationToken::new();
        let turn_cancel = cancel.clone();
        let handle = tokio::spawn(async move { approver.approve(&request(), turn_cancel).await });

        let message = rx.recv().await.expect("approval message");
        cancel.cancel();
        assert_eq!(handle.await.expect("join"), Answer::Deny);
        drop(message);
    }

    #[tokio::test]
    async fn a_closed_ui_channel_denies() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let approver = TuiApprover::new(tx);
        let answer = approver.approve(&request(), CancellationToken::new()).await;
        assert_eq!(answer, Answer::Deny);
    }
}
