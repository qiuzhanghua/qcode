//! The per-server tool table and its freshness rule (ADR-0067 §5).
//!
//! `2026-07-28` made `tools/list` a cacheable result: the response carries
//! `ttlMs` and `cacheScope` (SEP-2549), and a server may push
//! `notifications/tools/list_changed` when the table moves. That turns the tool
//! list from a constant into something with a lifetime, which is the one place
//! this ADR has to change the architecture: ADR-0002's "list once at startup"
//! is superseded here.
//!
//! Two rules shape everything below.
//!
//! 1. **Refetching is decided here, applying is decided by the engine.** This
//!    module answers "is my table stale"; `ToolSource::refresh` is only called
//!    at a turn boundary (see `qcode_core::tool::ToolSource`), so a table never
//!    changes under a running turn.
//! 2. **A failed refetch keeps the old table.** SEP-2549 lets a client serve a
//!    stale result when the re-fetch fails, and dropping every tool because the
//!    network blipped would be a far worse outcome than a table that is a turn
//!    old. The dirty flag stays set, so the next turn tries again.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::model::{PaginatedRequestParams, Tool as McpToolDefinition};
use rmcp::service::NotificationContext;
use rmcp::{ClientHandler, RoleClient};

/// A server that reports `tools/list` changes, so the next turn re-reads.
///
/// rmcp already invalidates its own `tools/list` response cache on this
/// notification; this is what tells *our* table, which exists because the
/// client-side cache cannot report a diff.
#[derive(Clone, Default, Debug)]
pub struct ListChangedHandler {
    dirty: Arc<AtomicBool>,
}

impl ListChangedHandler {
    pub fn new() -> Self {
        Self::default()
    }

    /// A shared flag the caller keeps so it can observe the notification.
    pub fn handle(&self) -> Arc<AtomicBool> {
        self.dirty.clone()
    }
}

impl ClientHandler for ListChangedHandler {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.dirty.store(true, Ordering::SeqCst);
    }
}

/// One server's tool definitions plus when they stop being usable.
#[derive(Debug)]
pub struct ToolTable {
    state: Mutex<State>,
    /// Set by the notification handler; shared so the handler can outlive the
    /// borrow.
    dirty: Arc<AtomicBool>,
}

#[derive(Debug)]
struct State {
    tools: Vec<McpToolDefinition>,
    /// `None` means the server said nothing (a legacy server): no lifetime
    /// information, so the table never goes stale on its own — which is exactly
    /// the old "list once" behaviour.
    ttl: Option<Duration>,
    fetched: Instant,
}

impl ToolTable {
    pub fn new(
        tools: Vec<McpToolDefinition>,
        ttl: Option<Duration>,
        dirty: Arc<AtomicBool>,
    ) -> Self {
        Self {
            state: Mutex::new(State {
                tools,
                ttl,
                fetched: Instant::now(),
            }),
            dirty,
        }
    }

    pub fn ttl(&self) -> Option<Duration> {
        self.lock().ttl
    }

    /// Whether the table should be re-read before the next turn.
    ///
    /// * dirty (a `list_changed` arrived) → yes, immediately;
    /// * no `ttlMs` → no, ever (legacy servers behave as they always did);
    /// * `ttlMs = 0` → yes, every turn ("this is never cacheable");
    /// * `ttlMs = n` → yes once `n` milliseconds have passed since the fetch.
    pub fn is_stale(&self) -> bool {
        if self.dirty.load(Ordering::SeqCst) {
            return true;
        }
        let state = self.lock();
        match state.ttl {
            None => false,
            Some(ttl) if ttl.is_zero() => true,
            Some(ttl) => state.fetched.elapsed() >= ttl,
        }
    }

    pub fn tools(&self) -> Vec<McpToolDefinition> {
        self.lock().tools.clone()
    }

    /// Replace the table with a fresh fetch.
    ///
    /// Returns the added and removed names (bare, as the server spells them), or
    /// `None` when the fetch produced the same set — a refresh that changes
    /// nothing must not write an event.
    pub fn replace(
        &self,
        tools: Vec<McpToolDefinition>,
        ttl: Option<Duration>,
    ) -> Option<(Vec<String>, Vec<String>)> {
        let mut state = self.lock();
        let previous: Vec<String> = state
            .tools
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        let next: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
        let added: Vec<String> = next
            .iter()
            .filter(|name| !previous.contains(name))
            .cloned()
            .collect();
        let removed: Vec<String> = previous
            .iter()
            .filter(|name| !next.contains(name))
            .cloned()
            .collect();
        state.tools = tools;
        state.ttl = ttl;
        state.fetched = Instant::now();
        drop(state);
        // Cleared only on a successful fetch: a failed one must retry next turn.
        self.dirty.store(false, Ordering::SeqCst);
        if added.is_empty() && removed.is_empty() {
            None
        } else {
            Some((added, removed))
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic while holding the table would poison it; the contents are a
        // plain list, so recovering is better than propagating the panic into
        // every later turn.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One page of `tools/list` plus the lifetime the first page declared.
pub type Fetched = (Vec<McpToolDefinition>, Option<Duration>);

/// Fold a `list_tools` response into `(tools, ttl)`.
///
/// Split out from the paging loop so the "first page wins" rule is testable
/// without a server: SEP-2549 puts the lifetime on the *list*, and a
/// conforming server repeats it on every page, but a non-conforming one might
/// only set it on the first.
pub fn absorb_page(accumulated: &mut Fetched, ttl_ms: Option<u64>, tools: Vec<McpToolDefinition>) {
    if accumulated.1.is_none() {
        accumulated.1 = ttl_ms.map(Duration::from_millis);
    }
    accumulated.0.extend(tools);
}

/// Cursor to ask for, if any. An empty cursor is what a server that has nothing
/// more sends; treating it as "next page" would loop.
pub fn next_cursor(cursor: Option<String>) -> Option<String> {
    cursor.filter(|value| !value.is_empty())
}

/// Build the paging request for a cursor.
pub fn page_params(cursor: Option<String>) -> Option<PaginatedRequestParams> {
    cursor.map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> McpToolDefinition {
        serde_json::from_value(json!({
            "name": name,
            "inputSchema": {"type": "object"}
        }))
        .expect("tool")
    }

    fn table(names: &[&str], ttl: Option<Duration>) -> ToolTable {
        let tools = names.iter().map(|name| tool(name)).collect();
        ToolTable::new(tools, ttl, Arc::new(AtomicBool::new(false)))
    }

    /// No `ttlMs` is the legacy case: the table never goes stale, which is what
    /// "list once at startup" used to mean.
    #[test]
    fn a_server_without_ttl_ms_is_never_stale() {
        let table = table(&["a"], None);
        assert!(!table.is_stale());
        assert_eq!(table.ttl(), None);
    }

    /// `ttlMs = 0` means "never cacheable", so every turn re-reads.
    #[test]
    fn ttl_zero_is_always_stale() {
        let table = table(&["a"], Some(Duration::ZERO));
        assert!(table.is_stale());
    }

    /// A positive TTL starts fresh and only expires with time.
    #[test]
    fn a_positive_ttl_starts_fresh() {
        let table = table(&["a"], Some(Duration::from_secs(60)));
        assert!(!table.is_stale(), "a fetch that just happened is fresh");
    }

    /// `list_changed` makes the table stale regardless of the TTL: the whole
    /// point of the notification is that the TTL cannot be trusted.
    #[test]
    fn the_notification_makes_a_fresh_table_stale() {
        let dirty = Arc::new(AtomicBool::new(false));
        let table = ToolTable::new(
            vec![tool("a")],
            Some(Duration::from_secs(3600)),
            dirty.clone(),
        );
        assert!(!table.is_stale());
        dirty.store(true, Ordering::SeqCst);
        assert!(table.is_stale(), "a list_changed must win over a long TTL");
    }

    #[test]
    fn replace_reports_added_and_removed_and_clears_dirty() {
        let dirty = Arc::new(AtomicBool::new(false));
        let table = ToolTable::new(
            vec![tool("a"), tool("b")],
            Some(Duration::from_secs(3600)),
            dirty.clone(),
        );
        dirty.store(true, Ordering::SeqCst);

        let diff = table
            .replace(vec![tool("b"), tool("c")], Some(Duration::from_secs(3600)))
            .expect("the set changed");
        assert_eq!(diff.0, vec!["c".to_string()], "added");
        assert_eq!(diff.1, vec!["a".to_string()], "removed");
        assert!(!table.is_stale(), "a successful fetch clears dirty");
        assert_eq!(
            table
                .tools()
                .iter()
                .map(|tool| tool.name.to_string())
                .collect::<Vec<_>>(),
            vec!["b".to_string(), "c".to_string()]
        );
    }

    /// An unchanged refetch must not fabricate a change, or every turn against a
    /// `ttlMs = 0` server would write an empty event.
    #[test]
    fn replace_of_the_same_set_is_not_a_change() {
        let table = table(&["a", "b"], Some(Duration::ZERO));
        assert_eq!(
            table.replace(vec![tool("a"), tool("b")], Some(Duration::ZERO)),
            None
        );
    }

    #[test]
    fn the_first_page_speaks_for_the_whole_list() {
        let mut accumulated: Fetched = (Vec::new(), None);
        absorb_page(&mut accumulated, Some(0), vec![tool("a")]);
        absorb_page(&mut accumulated, None, vec![tool("b")]);
        absorb_page(&mut accumulated, None, vec![tool("c")]);
        assert_eq!(accumulated.0.len(), 3, "every page's tools are kept");
        assert_eq!(
            accumulated.1,
            Some(Duration::ZERO),
            "the lifetime comes from the first page; later pages must not \
             overwrite it with their silence"
        );
    }

    /// A server that echoes the cursor it was given would page forever.
    #[test]
    fn an_empty_cursor_ends_paging() {
        assert_eq!(next_cursor(Some(String::new())), None);
        assert_eq!(
            next_cursor(Some("abc".to_string())),
            Some("abc".to_string())
        );
        assert_eq!(next_cursor(None), None);
        assert!(page_params(None).is_none());
    }
}
