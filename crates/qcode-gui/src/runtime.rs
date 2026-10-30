//! Engine runtime: turns `UiCommand`s into `run_turn` calls, owns the session
//! and the follow-up inbox, and streams `UiEvent`s back.
//!
//! Deliberately free of GPUI: it only needs qcode-core, qcode-ui-bridge and
//! tokio, so the queue/turn policy can be tested without opening a window.

use qcode_core::engine::{RunOptions, TurnContext, run_turn};
use qcode_core::event::SessionStarted;
use qcode_core::inbox::Inbox;
use qcode_core::store::SessionStore;
use qcode_ui_bridge::{KernelPort, QueueOp, UiCommand};
use std::path::PathBuf;
use std::sync::Arc;
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
    pub tools: qcode_core::tool::ToolRegistry,
    pub policy: qcode_core::policy::Policy,
    pub workspace: PathBuf,
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
pub(crate) fn create_empty_task(engine: &Engine, port: &KernelPort, model: &str) -> Option<String> {
    let meta = SessionStarted {
        model: model.to_string(),
        provider: engine.provider_name.clone(),
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
            let mut pricing = engine.pricing;
            let (done_tx, mut done_rx) =
                tokio::sync::mpsc::unbounded_channel::<Result<(), String>>();
            let inbox = Arc::new(Inbox::new());

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
                                provider: engine.provider_name.clone(),
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
                    let provider = engine.provider.clone();
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
                    let task_inbox = inbox.clone();
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
                                    session = create_empty_task(&engine, &port, &model);
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
                            UiCommand::SetModel { model: next } => {
                                model = next;
                                pricing = engine.pricing_table.get(&model).copied();
                            }
                            UiCommand::Prompt { text, attachments } => {
                                // All new messages enter the follow-up queue; an
                                // idle engine claims the head immediately, a
                                // running one drains it when the turn ends.
                                inbox.followup(text, attachments);
                                if active.is_none()
                                    && let Some(message) = inbox.claim_next_turn()
                                {
                                    start_turn!(message.text, message.attachments);
                                }
                                port.send_queue_changed(inbox.queued());
                            }
                            UiCommand::QueueOp { id, op } => {
                                match op {
                                    // Steering only reaches the running turn;
                                    // an idle engine leaves the queue untouched.
                                    QueueOp::Steer => {
                                        if active.is_some() {
                                            inbox.steer_queued(&id);
                                        }
                                    }
                                    QueueOp::MoveUp => {
                                        inbox.move_queued(&id, -1);
                                    }
                                    QueueOp::MoveDown => {
                                        inbox.move_queued(&id, 1);
                                    }
                                    QueueOp::Remove => {
                                        inbox.remove_queued(&id);
                                    }
                                }
                                port.send_queue_changed(inbox.queued());
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
                                    session = create_empty_task(&engine, &port, &model);
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
                        if let Some(message) = inbox.claim_next_turn() {
                            start_turn!(message.text, message.attachments);
                        }
                        port.send_queue_changed(inbox.queued());
                    }
                }
            }
        });
    });
}
