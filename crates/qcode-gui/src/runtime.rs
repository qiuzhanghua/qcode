//! Engine runtime: turns `UiCommand`s into `run_turn` calls, owns the session
//! and the follow-up inbox, and streams `UiEvent`s back.
//!
//! Deliberately free of GPUI: it only needs qcode-core, qcode-ui-bridge and
//! tokio, so the queue/turn policy can be tested without opening a window.

use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::SessionStarted;
use qcode_core::inbox::{QueuedMessage, SessionQueue, Submission};
use qcode_core::store::SessionStore;
use qcode_ui_bridge::{KernelPort, QueueOp, UiCommand};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

/// The pieces a running window needs to execute turns.
///
/// Everything here comes from `qcode_kernel::assemble`; the window never wires
/// its own provider or tools.
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
            port.send_event(&created.started);
            port.send_event(&created.configured);
            port.clear_session_allow();
            port.send_session_changed(Some(created.id.clone()));
            Some(created.id)
        }
        Err(error) => {
            eprintln!("qcode-gui: cannot create task: {error}");
            port.send_turn_failed(error.to_string());
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

fn publish(port: &KernelPort, inbox: &SessionQueue, last: &mut Published) {
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
            let approver = std::sync::Arc::new(port.approver());
            let mut session: Option<String> = None;
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
            let queue = SessionQueue::new();
            let mut ticker = tokio::time::interval(Duration::from_millis(250));
            let mut published = Published::default();

            // Run one queued message as its own turn on the engine task. The
            // engine drains the `next-step` inbox before every model request,
            // so steering sent while this turn is live lands in it.
            macro_rules! start_turn {
                ($text:expr, $attachments:expr) => {{
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
                                    port.send_turn_failed(error.to_string());
                                    continue;
                                }
                            };
                            let id = created.id.clone();
                            port.send_event(&created.started);
                            port.send_event(&created.configured);
                            port.send_session_changed(Some(id.clone()));
                            session = Some(id.clone());
                            id
                        }
                    };
                    let sink = port.clone();
                    let token = CancellationToken::new();
                    active = Some(token.clone());
                    let store = engine.store.clone();
                    let provider = provider.clone();
                    let tools = engine.tools.clone();
                    let policy = engine.policy.clone();
                    let snapshots = engine.snapshots.clone();
                    let network = engine.network.clone();
                    let workspace = engine.workspace.clone();
                    let max_turns = engine.max_turns;
                    let max_cost_usd = engine.max_cost_usd;
                    let context_window = engine.context_window;
                    let task_model = model.clone();
                    let task_session = session_id.clone();
                    let task_approver = approver.clone();
                    let done = done_tx.clone();
                    let cancel = token.clone();
                    let task_inbox = queue.inbox();
                    tokio::spawn(async move {
                        let options = RunOptions {
                            session_id: &task_session,
                            model: &task_model,
                            input: $text,
                            cwd: workspace,
                            max_turns,
                            cancel,
                            pricing,
                            max_cost_usd,
                            on_delta: None,
                            context_window,
                            agents_md: None,
                            skills: None,
                            network,
                            attachments: $attachments,
                            inbox: Some(task_inbox),
                        };
                        let result = run_turn(
                            TurnContext {
                                store: &store,
                                provider: provider.as_ref(),
                                tools: &tools,
                                policy: &policy,
                                approver: task_approver.as_ref(),
                                snapshots: snapshots.as_ref(),
                            },
                            options,
                            move |event| sink.send_event(event),
                        )
                        .await;
                        let _ = done.send(result.map(|_| ()).map_err(|error| error.to_string()));
                    });
                }};
            }

            loop {
                tokio::select! {
                    // The engine claims steering from another thread, so the
                    // pending rows are refreshed on a slow tick rather than
                    // tied to a UI event.
                    _ = ticker.tick() => publish(&port, &queue, &mut published),
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
                            UiCommand::Cancel => {
                                if let Some(token) = &active {
                                    token.cancel();
                                }
                            }
                            UiCommand::NewSession => {
                                if let Some(token) = &active {
                                    token.cancel();
                                    pending = Some(UiCommand::NewSession);
                                } else {
                                    session = create_empty_task(&engine, &provider_name, &port, &model);
                                }
                            }
                            UiCommand::Resume { id } => {
                                if let Some(token) = &active {
                                    token.cancel();
                                    pending = Some(UiCommand::Resume { id });
                                } else {
                                    session = Some(id.clone());
                                    port.clear_session_allow();
                                    port.send_session_changed(Some(id));
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
                                            port.send_turn_failed(format!(
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
                                match queue.submit(text, attachments, active.is_some()) {
                                    Submission::Start(message) => {
                                        start_turn!(message.text, message.attachments)
                                    }
                                    Submission::Queued => {}
                                }
                                publish(&port, &queue, &mut published);
                            }
                            UiCommand::Steer { text, attachments } => {
                                // Only a live turn can take steering; the dock
                                // does not offer it while idle.
                                queue.steer(text, attachments, active.is_some());
                                publish(&port, &queue, &mut published);
                            }
                            UiCommand::QueueOp { id, op } => {
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
                                publish(&port, &queue, &mut published);
                            }
                        }
                    }
                    Some(result) = done_rx.recv() => {
                        active = None;
                        if let Err(error) = result {
                            eprintln!("qcode-gui: turn failed: {error}");
                            port.send_turn_failed(error);
                        }
                        port.send_turn_finished(session.clone());
                        if let Some(pending) = pending.take() {
                            match pending {
                                UiCommand::NewSession => {
                                    session = create_empty_task(&engine, &provider_name, &port, &model);
                                }
                                UiCommand::Resume { id } => {
                                    session = Some(id.clone());
                                    port.clear_session_allow();
                                    port.send_session_changed(Some(id));
                                }
                                _ => {}
                            }
                        }
                        // Drain the follow-up queue: the next queued message
                        // starts its own turn now that the engine is idle.
                        if let Some(message) = queue.finish() {
                            start_turn!(message.text, message.attachments);
                        }
                        publish(&port, &queue, &mut published);
                    }
                }
            }
        });
    });
}
