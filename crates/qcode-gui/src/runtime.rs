//! Engine runtime: turns `UiCommand`s into `run_turn` calls, owns the session
//! and the follow-up inbox, and streams `UiEvent`s back.
//!
//! Deliberately free of GPUI: it only needs qcode-core, qcode-ui-bridge and
//! tokio, so the queue/turn policy can be tested without opening a window.

use qcode_core::engine::{DeltaSink, RunOptions, TurnContext, run_turn};
use qcode_core::event::SessionStarted;
use qcode_core::inbox::{QueuedMessage, SessionQueue, Submission};
use qcode_core::store::SessionStore;
use qcode_ui_bridge::{KernelPort, QueueOp, SessionTarget, TaggedPort, UiCommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::host::SessionHost;

/// The pieces a running window needs to execute turns.
///
/// Everything here comes from `qcode_kernel::assemble`; the window never wires
/// its own provider or tools. Held once behind [`crate::host::Deps`] so every
/// session shares it instead of cloning a provider per session (ADR-0058 §1).
pub struct Engine {
    pub store: SessionStore,
    pub provider: std::sync::Arc<dyn qcode_core::provider::Provider>,
    pub provider_name: String,
    /// Egress proxy URL of the assembled kernel; a cross-provider model
    /// switch rebuilds the provider with the same proxy.
    pub proxy_url: Option<String>,
    pub tools: qcode_core::tool::ToolRegistry,
    pub policy: qcode_core::policy::Policy,
    pub workspace: PathBuf,
    /// Config file the engine was assembled from; "切即默认" persists the
    /// switched model into its `[default-model]` section.
    pub config_path: PathBuf,
    pub model: String,
    pub pricing: Option<qcode_core::cost::Pricing>,
    pub pricing_table: std::collections::BTreeMap<String, qcode_core::cost::Pricing>,
    pub max_turns: u32,
    pub max_cost_usd: Option<f64>,
    pub context_window: Option<u64>,
    pub network: Option<qcode_core::network::SharedNetwork>,
    pub snapshots: Option<qcode_core::snapshot::SnapshotStore>,
}

/// Create a fresh empty task (no roots) and tell the UI to switch to it.
/// `provider_name` is the engine's current provider (may change after a
/// cross-provider model switch).
pub(crate) fn create_empty_task(
    engine: &Engine,
    provider_name: &str,
    port: &KernelPort,
    model: &str,
) -> Option<String> {
    let meta = SessionStarted {
        model: model.to_string(),
        provider: provider_name.to_string(),
        cwd: engine.workspace.display().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    match engine.store.create_task_with_roots(&meta, Vec::new()) {
        Ok(created) => {
            let target = SessionTarget::Session(created.id.clone());
            // A new task is a new session: its own "Always" list, not the one
            // the previous session accumulated.
            port.clear_allow(&target);
            let port = port.tagged(target);
            port.send_event(&created.started);
            port.send_event(&created.configured);
            port.send_session_changed(Some(created.id.clone()));
            Some(created.id)
        }
        Err(error) => {
            eprintln!("qcode-gui: cannot create task: {error}");
            port.tagged(SessionTarget::Draft)
                .send_turn_failed(error.to_string());
            None
        }
    }
}

/// What the UI was last told about the queues, so the ticker only publishes
/// real changes (the engine claims steering from its own thread).
#[derive(Default, PartialEq, Eq)]
struct Published {
    queue: Vec<QueuedMessage>,
    steering: Vec<QueuedMessage>,
}

fn publish(port: &TaggedPort, inbox: &SessionQueue, last: &mut Published) {
    let queue = inbox.queued();
    if queue != last.queue {
        port.send_queue_changed(queue.clone());
        last.queue = queue;
    }
    let steering = inbox.pending_step();
    if steering != last.steering {
        port.send_steering_changed(steering.clone());
        last.steering = steering;
    }
}

/// Reports a panicking turn to the app loop.
///
/// `run_turn` runs inside `tokio::spawn`, so a panic — a bug in a tool, in a
/// provider decoder, or an `unwrap` on model output — unwinds the spawned task
/// and the `done` message is never sent. Without this the loop keeps `active`
/// set forever: the UI stays on "running", Stop targets a task that is gone,
/// and new sessions keep being refused. Dropping the guard on unwind turns
/// that into the same `TurnFailed` a returned `Err` produces (ADR-0058 §9 P0).
struct TurnGuard {
    done: tokio::sync::mpsc::UnboundedSender<Result<(), String>>,
    armed: bool,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.done.send(Err(
                "the turn task panicked; the engine is idle again, see the logs".to_string(),
            ));
        }
    }
}

// 恢复会话不改 model/provider：SessionStarted 里只是历史快照，拿它发请求
// 会把旧会话的模型名（如 mock-1）发给当前 provider。界面和引擎都以当前
// 配置/用户选择的模型为准。

/// Run the engine on its own thread: `run_turn` is async, the window is not.
pub(crate) fn spawn(engine: Engine, port: KernelPort, mut commands: UnboundedReceiver<UiCommand>) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("qcode-gui: cannot start the runtime: {error}");
                return;
            }
        };
        runtime.block_on(async move {
            // One host per window: the authority for "which sessions are live".
            // P1 wires a single slot through it (see `host.rs`).
            let host = Arc::new(SessionHost::new(engine));
            let deps = host.deps().clone();
            let engine = &deps.engine;

            let mut session: Option<String> = None;
            // The session the UI is currently looking at. Every event sent
            // from this loop is tagged with it (ADR-0058 §2).
            let mut current = SessionTarget::Draft;
            let mut active: Option<CancellationToken> = None;
            let mut pending: Option<UiCommand> = None;
            let mut model = engine.model.clone();
            // Mutable provider + provider_name: a cross-provider model switch
            // (grouped header picker, V2.1) rebuilds the provider in place.
            let mut provider = engine.provider.clone();
            let mut provider_name = engine.provider_name.clone();
            let mut pricing = engine.pricing;
            let (done_tx, mut done_rx) =
                tokio::sync::mpsc::unbounded_channel::<Result<(), String>>();
            let mut ticker = tokio::time::interval(Duration::from_millis(250));
            let mut published = Published::default();

            // Run one queued message as its own turn on the engine task. The
            // engine drains the `next-step` inbox before every model request,
            // so steering sent while this turn is live lands in it.
            macro_rules! start_turn {
                ($text:expr, $attachments:expr) => {{
                    start_turn!($text, $attachments, None, engine.max_turns, engine.max_cost_usd)
                }};
                ($text:expr, $attachments:expr, $continuation:expr, $max_turns:expr, $max_cost:expr) => {{
                    let session_id = match session.clone() {
                        Some(id) => id,
                        None => {
                            let created = match engine.store.create_task(&SessionStarted {
                                model: model.clone(),
                                provider: provider_name.clone(),
                                cwd: engine.workspace.display().to_string(),
                                version: env!("CARGO_PKG_VERSION").to_string(),
                            }) {
                                Ok(created) => created,
                                Err(error) => {
                                    eprintln!("qcode-gui: cannot create a task: {error}");
                                    port.tagged(current.clone())
                                        .send_turn_failed(error.to_string());
                                    continue;
                                }
                            };
                            let id = created.id.clone();
                            let new_target = SessionTarget::Session(id.clone());
                            // The draft becomes a session: drop whatever the
                            // draft slot held so nothing leaks by target.
                            host.close_if_idle(&current);
                            current = new_target;
                            port.tagged(current.clone()).send_event(&created.started);
                            port.tagged(current.clone()).send_event(&created.configured);
                            port.tagged(current.clone())
                                .send_session_changed(Some(id.clone()));
                            session = Some(id.clone());
                            id
                        }
                    };
                    // Every turn of a session runs on that session's slot:
                    // its cancel token and its follow-up inbox.
                    let target = SessionTarget::Session(session_id.clone());
                    current = target.clone();
                    let (token, task_queue) = host.open(&target);
                    host.begin_turn(&target);
                    let sink = port.tagged(target.clone());
                    active = Some(token.clone());
                    let store = engine.store.clone();
                    let provider = provider.clone();
                    let tools = engine.tools.clone();
                    let policy = engine.policy.clone();
                    let snapshots = engine.snapshots.clone();
                    let network = engine.network.clone();
                    let workspace = engine.workspace.clone();
                    let max_turns = $max_turns;
                    let max_cost_usd = $max_cost;
                    let context_window = engine.context_window;
                    let task_model = model.clone();
                    let task_session = session_id.clone();
                    let task_approver = sink.approver();
                    let done = done_tx.clone();
                    let guard = TurnGuard {
                        done: done.clone(),
                        armed: true,
                    };
                    let cancel = token.clone();
                    let task_inbox = task_queue.inbox();
                    tokio::spawn(async move {
                        // A panic anywhere below unwinds this task without
                        // reaching `done.send`; `guard` covers that path.
                        let mut guard = guard;
                        // 把引擎的流式 Delta（含上下文 token 估算）转发给 UI，
                        // 驱动打字机效果与 ctx% 进度。
                        let delta_port = sink.clone();
                        let options = RunOptions {
                            session_id: &task_session,
                            model: &task_model,
                            input: $text,
                            cwd: workspace,
                            max_turns,
                            cancel,
                            pricing,
                            max_cost_usd,
                            on_delta: Some(DeltaSink::new(move |delta| {
                                delta_port.send_delta(delta)
                            })),
                            context_window,
                            agents_md: None,
                            skills: None,
                            network,
                            attachments: $attachments,
                            inbox: Some(task_inbox),
                            continuation: $continuation,
                        };
                        let result = run_turn(
                            TurnContext {
                                store: &store,
                                provider: provider.as_ref(),
                                tools: &tools,
                                policy: &policy,
                                approver: &task_approver,
                                snapshots: snapshots.as_ref(),
                            },
                            options,
                            move |event| sink.send_event(event),
                        )
                        .await;
                        guard.armed = false;
                        let _ = done.send(result.map(|_| ()).map_err(|error| error.to_string()));
                    });
                }};
            }

            loop {
                tokio::select! {
                    // The engine claims steering from another thread, so the
                    // pending rows are refreshed on a slow tick rather than
                    // tied to a UI event.
                    _ = ticker.tick() => {
                        if let Some(queue) = host.queue(&current) {
                            publish(&port.tagged(current.clone()), &queue, &mut published);
                        }
                    }
                    command = commands.recv() => {
                        let Some(command) = command else {
                            break;
                        };
                        match command {
                            UiCommand::Answer {
                                call_id,
                                allow,
                                session: remember,
                            } => port.dispatch(UiCommand::Answer {
                                call_id,
                                allow,
                                session: remember,
                            }),
                            UiCommand::Cancel { session } => {
                                // Per-session stop (ADR-0058 §6): the session's
                                // own token is tripped even if the engine is not
                                // on it, and the running turn's token only when
                                // it belongs to that session.
                                let target = session
                                    .map(SessionTarget::Session)
                                    .unwrap_or(SessionTarget::Draft);
                                host.cancel(&target);
                                if target == current
                                    && let Some(token) = &active
                                {
                                    token.cancel();
                                }
                            }
                            UiCommand::CancelAll => {
                                host.cancel_all();
                                if let Some(token) = &active {
                                    token.cancel();
                                }
                            }
                            UiCommand::NewSession => {
                                if let Some(token) = &active {
                                    token.cancel();
                                    pending = Some(UiCommand::NewSession);
                                } else {
                                    host.close_if_idle(&current);
                                    session = create_empty_task(engine, &provider_name, &port, &model);
                                    current = session
                                        .clone()
                                        .map(SessionTarget::Session)
                                        .unwrap_or(SessionTarget::Draft);
                                }
                            }
                            UiCommand::Resume { id } => {
                                if let Some(token) = &active {
                                    token.cancel();
                                    pending = Some(UiCommand::Resume { id });
                                } else {
                                    let target = SessionTarget::Session(id.clone());
                                    host.close_if_idle(&current);
                                    current = target.clone();
                                    session = Some(id.clone());
                                    // A resumed session starts with a clean
                                    // "Always" list, like a fresh window.
                                    port.clear_allow(&target);
                                    port.tagged(target).send_session_changed(Some(id));
                                }
                            }
                            UiCommand::SetModel {
                                provider: next_provider,
                                model: next,
                            } => {
                                // Cross-provider switch: rebuild the provider
                                // for the target route, keeping the session,
                                // tools and policy intact. Failure keeps the
                                // previous provider and reports the reason.
                                if next_provider != provider_name {
                                    match qcode_kernel::build_provider_for(
                                        Some(&engine.config_path),
                                        &engine.workspace,
                                        &next_provider,
                                        &next,
                                        engine.proxy_url.as_deref(),
                                    ) {
                                        Ok(new_provider) => {
                                            provider = new_provider.into();
                                            provider_name = next_provider;
                                        }
                                        Err(error) => {
                                            eprintln!(
                                                "qcode-gui: cannot switch provider: {error}"
                                            );
                                            port.tagged(current.clone()).send_turn_failed(format!(
                                                "无法切换到 {next_provider}：{error}"
                                            ));
                                        }
                                    }
                                }
                                model = next;
                                pricing = engine.pricing_table.get(&model).copied();
                                // “切即默认”：切换模型同时写进 [default-model]，
                                // 新会话默认用它；写失败只记日志，不阻断切换。
                                if let Err(error) = qcode_kernel::config_edit::set_default_model(
                                    &engine.config_path,
                                    Some(provider_name.as_str()),
                                    Some(model.as_str()),
                                ) {
                                    eprintln!("qcode-gui: cannot persist default model: {error}");
                                }
                            }
                            UiCommand::Prompt { text, attachments } => {
                                // Running: the message waits in the follow-up
                                // queue; idle: it starts its own turn now.
                                let running = active.is_some();
                                let queue = host
                                    .queue(&current)
                                    .unwrap_or_else(|| host.open(&current).1);
                                match queue.submit(text, attachments, running) {
                                    Submission::Start(message) => {
                                        start_turn!(message.text, message.attachments)
                                    }
                                    Submission::Queued => {}
                                }
                                publish(&port.tagged(current.clone()), &queue, &mut published);
                            }
                            UiCommand::Continue { raise } => {
                                // Only an idle engine continues: a live turn
                                // already has a window, and a queue submit
                                // while paused is itself a continue.
                                if active.is_none() {
                                    let raise = raise.unwrap_or_default();
                                    let renewal = Some(qcode_core::event::Continuation {
                                        // The engine fills these in from the log.
                                        turn: 0,
                                        window: 1,
                                        raise: (raise.turns.is_some() || raise.usd.is_some())
                                            .then(|| raise.into()),
                                    });
                                    // The new limits apply to this window only;
                                    // the next turn falls back to the config.
                                    let turns = raise.turns.unwrap_or(engine.max_turns);
                                    let cost = raise.usd.or(engine.max_cost_usd);
                                    start_turn!(
                                        qcode_core::engine::CONTINUE_PROMPT.to_string(),
                                        Vec::new(),
                                        renewal,
                                        turns,
                                        cost
                                    );
                                }
                            }
                            UiCommand::Steer { text, attachments } => {
                                // Only a live turn can take steering; the dock
                                // does not offer it while idle.
                                let queue = host
                                    .queue(&current)
                                    .unwrap_or_else(|| host.open(&current).1);
                                queue.steer(text, attachments, active.is_some());
                                publish(&port.tagged(current.clone()), &queue, &mut published);
                            }
                            UiCommand::QueueOp { id, op } => {
                                let queue = host
                                    .queue(&current)
                                    .unwrap_or_else(|| host.open(&current).1);
                                match op {
                                    // Steering only reaches the running turn;
                                    // an idle engine leaves the queue untouched.
                                    QueueOp::Steer => {
                                        queue.steer_queued(&id, active.is_some());
                                    }
                                    QueueOp::MoveUp => {
                                        queue.move_queued(&id, -1);
                                    }
                                    QueueOp::MoveDown => {
                                        queue.move_queued(&id, 1);
                                    }
                                    QueueOp::Remove => {
                                        queue.remove_queued(&id);
                                    }
                                    QueueOp::Unsteer => {
                                        queue.unsteer(&id);
                                    }
                                }
                                publish(&port.tagged(current.clone()), &queue, &mut published);
                            }
                        }
                    }
                    Some(result) = done_rx.recv() => {
                        active = None;
                        if let Err(error) = result {
                            eprintln!("qcode-gui: turn failed: {error}");
                            port.tagged(current.clone()).send_turn_failed(error);
                        }
                        host.end_turn(&current);
                        port.tagged(current.clone()).send_turn_finished(session.clone());
                        if let Some(pending) = pending.take() {
                            match pending {
                                UiCommand::NewSession => {
                                    host.close_if_idle(&current);
                                    session = create_empty_task(engine, &provider_name, &port, &model);
                                    current = session
                                        .clone()
                                        .map(SessionTarget::Session)
                                        .unwrap_or(SessionTarget::Draft);
                                }
                                UiCommand::Resume { id } => {
                                    let target = SessionTarget::Session(id.clone());
                                    host.close_if_idle(&current);
                                    current = target.clone();
                                    session = Some(id.clone());
                                    port.clear_allow(&target);
                                    port.tagged(target).send_session_changed(Some(id));
                                }
                                _ => {}
                            }
                        }
                        // Drain the follow-up queue: the next queued message
                        // starts its own turn now that the engine is idle.
                        if let Some(queue) = host.queue(&current)
                            && let Some(message) = queue.finish()
                        {
                            start_turn!(message.text, message.attachments);
                        }
                        if let Some(queue) = host.queue(&current) {
                            publish(&port.tagged(current.clone()), &queue, &mut published);
                        }
                    }
                }
            }
        });
    });
}
