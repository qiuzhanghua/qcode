use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use qcode_core::event::{Decision, Event, EventKind, PauseInfo, PauseLimit, ToolStatus};
use qcode_kernel::config;
use qcode_mcp::McpConfig;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// JSONL on stdout. `provider_raw` is diagnostic noise for most consumers, so
/// it is hidden unless `--include-raw` is passed (the log on disk keeps it).
pub(crate) fn print_event(event: &Event, include_raw: bool) {
    if include_raw {
        println!(
            "{}",
            serde_json::to_string(event).expect("event should serialize")
        );
        return;
    }
    let mut value = serde_json::to_value(event).expect("event should serialize");
    if let Some(data) = value.get_mut("data").and_then(Value::as_object_mut) {
        data.remove("provider_raw");
    }
    println!(
        "{}",
        serde_json::to_string(&value).expect("event should serialize")
    );
}

pub(crate) fn print_config(config: &config::Config) {
    println!(
        "schema = {} (this build supports {})",
        config.schema,
        config::CONFIG_VERSION
    );
    println!("default_provider = {:?}", config.default_provider);
    println!("providers ({}):", config.providers.len());
    for (name, provider) in &config.providers {
        println!(
            "  {name}: base_url={:?} model={:?} api_key_env={:?} stream_usage={}",
            provider.base_url, provider.default_model, provider.api_key_env, provider.stream_usage
        );
    }
    let plan = config.network_plan();
    println!(
        "sandbox.network = {} (proxy: {}, raw: {})",
        config.sandbox_network_mode().label(),
        plan.proxy,
        plan.raw
    );
    // `[sandbox] network` decides whether there is network at all; this line is
    // the part the switch does not cover (ADR-0065).
    println!(
        "network.allow = {:?}, allow_private = {}, unknown = {:?}, audit = {}",
        config.network.allow,
        config.network.allow_private,
        config.network.unknown,
        config.network.audit
    );
    println!("context.window_tokens = {}", config.context.window_tokens);
    println!("agent.max_turns = {}", config.agent.max_turns);
    println!("agent.max_cost_usd = {:?}", config.agent.max_cost_usd);
    println!("agent.snapshot_shell = {}", config.agent.snapshot_shell);
    println!(
        "search.enabled = {}, backend = {:?}, api_key_env = {:?}, max_results = {}, base_url = {:?}",
        config.search.enabled,
        config.search.backend,
        config.search.key_env_name(),
        config.search.max_results,
        config.search.base_url
    );
    println!(
        "update.enabled = {}, check = {}, check_days = {}, repo = {}, base_url = {:?}",
        config.update.enabled,
        config.update.check,
        config.update.check_days,
        config.update.repo,
        config.update.base_url
    );
    println!(
        "sandbox.mode = {}, network = {}, extra_writable = {:?}",
        config.sandbox.mode.label(),
        config.sandbox_network_mode().label(),
        config.sandbox.extra_writable
    );
    println!(
        "skills.enabled = {}, roots = {:?}",
        config.skills.enabled, config.skills.roots
    );
    println!("approval rules ({}):", config.approval.rules.len());
    for rule in &config.approval.rules {
        println!(
            "  {} action={:?} tool={:?} prefix={:?} glob={:?} scope={:?}",
            rule.id, rule.action, rule.tool, rule.command_prefix, rule.path_glob, rule.scope
        );
    }
    print_mcp(&config.mcp);
}

pub(crate) fn print_mcp(config: &McpConfig) {
    println!("mcp servers ({}):", config.servers.len());
    for server in &config.servers {
        println!(
            "  {} enabled={} timeout={}s command={:?} args={:?}",
            server.name, server.enabled, server.timeout_secs, server.command, server.args
        );
        // The sandbox decision and its explicit extras (ADR-0066). The full
        // grant is computed at startup; this is what the file asks for.
        let sandbox = server.sandbox_mode(config.sandbox).label();
        if sandbox != "confine"
            || !server.read_paths.is_empty()
            || !server.writable_paths.is_empty()
            || !server.credentials.is_empty()
        {
            let paths = |paths: &[std::path::PathBuf]| {
                if paths.is_empty() {
                    "-".to_string()
                } else {
                    paths
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                }
            };
            let credentials = if server.credentials.is_empty() {
                "-".to_string()
            } else {
                server
                    .credentials
                    .iter()
                    .map(|preset| preset.label())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            println!(
                "    sandbox={sandbox} read_paths={} writable_paths={} credentials={credentials}",
                paths(&server.read_paths),
                paths(&server.writable_paths)
            );
        }
    }
}

pub(crate) fn install_ctrl_c(cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut presses = 0u32;
        while tokio::signal::ctrl_c().await.is_ok() {
            presses += 1;
            if presses == 1 {
                eprintln!(
                    "\n[cancel] stopping the current turn… (Ctrl-C again to quit immediately)"
                );
                cancel.cancel();
            } else {
                std::process::exit(130);
            }
        }
    });
}

/// Whole-turn deadline. It cancels through the same token as Ctrl-C, so a
/// running shell still gets its process group killed before the turn ends.
pub(crate) fn install_timeout(
    cancel: CancellationToken,
    timeout: Duration,
    fired: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        fired.store(true, Ordering::SeqCst);
        eprintln!(
            "\n[timeout] turn exceeded {}s; stopping…",
            timeout.as_secs()
        );
        cancel.cancel();
    });
}

/// Human-mode progress on stderr. Without this a 10-minute test run looks like
/// a hang, because the engine only reports the final text on stdout.
pub(crate) fn progress(event: &Event) {
    match &event.kind {
        EventKind::ToolCall(call) => {
            let summary = call
                .arguments
                .get("command")
                .and_then(|value| value.as_str())
                .or_else(|| call.arguments.get("path").and_then(|value| value.as_str()))
                .unwrap_or_default();
            eprintln!("\n[tool] {} {}", call.name, truncate_chars(summary, 120));
        }
        EventKind::ToolResult(result) => {
            let status = match result.status {
                ToolStatus::Ok => "ok",
                ToolStatus::Error => "error",
                ToolStatus::Cancelled => "cancelled",
                ToolStatus::Interrupted => "interrupted",
            };
            eprintln!("[tool] {status} ({} bytes)", result.metadata.bytes);
        }
        EventKind::ApprovalDecided(decided) => {
            let decision = if decided.decision == Decision::Allow {
                "allowed"
            } else {
                "denied"
            };
            eprintln!("[approval] {decision}");
        }
        EventKind::BudgetHint(hint) => {
            eprintln!(
                "[budget] {}/{} turns used; one message away from pausing",
                hint.turns, hint.max_turns
            );
        }
        EventKind::TurnEnded(ended) => {
            eprintln!("[turn] {:?}", ended.status);
        }
        _ => {}
    }
}

/// One JSON object describing a paused turn, printed after the event stream so
/// `--json` consumers get the hand-off without re-reading the log.
pub(crate) fn print_pause(session_id: &str, pause: &PauseInfo) {
    let value = serde_json::json!({
        "type": "pause",
        "data": {
            "session_id": session_id,
            "reason": pause.reason,
            "limit": pause.limit,
            "used": pause.used,
            "window": pause.window,
            "summary": pause.summary,
        }
    });
    println!("{value}");
}

/// `已用 64/64 轮`, or `成本 $5.00/$5.00`, for the one-line pause report.
pub(crate) fn pause_line(pause: &PauseInfo) -> String {
    match pause.limit {
        PauseLimit::Turns(limit) => format!("used {}/{} turns", pause.used.turns, limit),
        PauseLimit::Usd(limit) => format!("cost ${:.2} of ${limit:.2}", pause.used.cost_usd),
    }
}

pub(crate) fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    format!("{}…", text.chars().take(limit).collect::<String>())
}
