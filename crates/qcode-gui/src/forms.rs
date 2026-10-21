//! Schema for the Setup wizard and the Settings page.
//!
//! Every key in `qcode_kernel::config::known_keys()` must be represented here,
//! with `version` as the only automatic exception. The UI renders these specs;
//! the coverage test in `tests/schema.rs` fails when a new config key is added
//! without a control.

/// One page of the Setup wizard. Settings reuses the same pages without the
/// wizard navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Welcome,
    Provider,
    ModelKey,
    Agent,
    NetworkSandbox,
    Search,
    Mcp,
    Skills,
    ApprovalPricing,
    UpdateAppearance,
    Review,
}

impl Page {
    pub const ALL: [Page; 11] = [
        Page::Welcome,
        Page::Provider,
        Page::ModelKey,
        Page::Agent,
        Page::NetworkSandbox,
        Page::Search,
        Page::Mcp,
        Page::Skills,
        Page::ApprovalPricing,
        Page::UpdateAppearance,
        Page::Review,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::Provider => "Provider",
            Self::ModelKey => "Model & Key",
            Self::Agent => "Agent & Context",
            Self::NetworkSandbox => "Network & Sandbox",
            Self::Search => "Search",
            Self::Mcp => "MCP",
            Self::Skills => "Skills",
            Self::ApprovalPricing => "Approval & Pricing",
            Self::UpdateAppearance => "Update & Appearance",
            Self::Review => "Review",
        }
    }
}

/// How a scalar is edited. Tables add columns of the same kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    OptionalText,
    Integer,
    Float,
    Bool,
    Select(&'static [&'static str]),
    /// One item per line (paths, host patterns, argv).
    Lines,
}

#[derive(Debug, Clone, Copy)]
pub struct FieldSpec {
    pub path: &'static [&'static str],
    pub label: &'static str,
    pub page: Page,
    pub kind: FieldKind,
    pub help: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct ColumnSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
}

#[derive(Debug, Clone, Copy)]
pub struct TableSpec {
    pub path: &'static [&'static str],
    pub title: &'static str,
    pub page: Page,
    pub columns: &'static [ColumnSpec],
}

const SANDBOX_MODES: &[&str] = &["none", "sandbox-exec", "bwrap", "landlock", "windows-acl"];
const NETWORK_MODES: &[&str] = &["off", "proxy", "strict"];
const UNKNOWN_HOSTS: &[&str] = &["ask", "deny", "allow"];
const SEARCH_BACKENDS: &[&str] = &["brave", "tavily", "searxng"];
const RULE_ACTIONS: &[&str] = &["allow", "ask", "deny"];
const RULE_SCOPES: &[&str] = &["session", "project", "user"];

pub const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        path: &["default_provider"],
        label: "Default provider",
        page: Page::ModelKey,
        kind: FieldKind::OptionalText,
        help: "Provider used when --provider is not passed",
    },
    FieldSpec {
        path: &["context", "window_tokens"],
        label: "Context window (tokens)",
        page: Page::Agent,
        kind: FieldKind::Integer,
        help: "Compaction starts at 78% of this value",
    },
    FieldSpec {
        path: &["agent", "max_turns"],
        label: "Max turns",
        page: Page::Agent,
        kind: FieldKind::Integer,
        help: "Model calls allowed for one user message",
    },
    FieldSpec {
        path: &["agent", "max_cost_usd"],
        label: "Max cost (USD)",
        page: Page::Agent,
        kind: FieldKind::Float,
        help: "Optional; needs a pricing entry for the model",
    },
    FieldSpec {
        path: &["agent", "snapshot_shell"],
        label: "Snapshot before shell",
        page: Page::Agent,
        kind: FieldKind::Bool,
        help: "Enables rollback for shell/MCP edits; capped at 64 MB",
    },
    FieldSpec {
        path: &["sandbox", "mode"],
        label: "Sandbox mode",
        page: Page::NetworkSandbox,
        kind: FieldKind::Select(SANDBOX_MODES),
        help: "OS sandbox for run_shell; none keeps approval as the boundary",
    },
    FieldSpec {
        path: &["sandbox", "network"],
        label: "Allow network in sandbox",
        page: Page::NetworkSandbox,
        kind: FieldKind::Bool,
        help: "Ignored when network.mode = strict",
    },
    FieldSpec {
        path: &["sandbox", "extra_writable"],
        label: "Extra writable roots",
        page: Page::NetworkSandbox,
        kind: FieldKind::Lines,
        help: "One path per line; windows-acl only",
    },
    FieldSpec {
        path: &["network", "mode"],
        label: "Network mode",
        page: Page::NetworkSandbox,
        kind: FieldKind::Select(NETWORK_MODES),
        help: "strict is macOS sandbox-exec proxy-only hard mode",
    },
    FieldSpec {
        path: &["network", "enabled"],
        label: "Start egress proxy",
        page: Page::NetworkSandbox,
        kind: FieldKind::Bool,
        help: "Required by strict mode",
    },
    FieldSpec {
        path: &["network", "allow"],
        label: "Allowed hosts",
        page: Page::NetworkSandbox,
        kind: FieldKind::Lines,
        help: "host or host:port; provider host is added automatically",
    },
    FieldSpec {
        path: &["network", "allow_private"],
        label: "Allow private/loopback hosts",
        page: Page::NetworkSandbox,
        kind: FieldKind::Bool,
        help: "Only for local Ollama or test servers",
    },
    FieldSpec {
        path: &["network", "unknown"],
        label: "Unknown host policy",
        page: Page::NetworkSandbox,
        kind: FieldKind::Select(UNKNOWN_HOSTS),
        help: "ask is enforced by tool approvals; shell/MCP deny",
    },
    FieldSpec {
        path: &["network", "audit"],
        label: "Audit network",
        page: Page::NetworkSandbox,
        kind: FieldKind::Bool,
        help: "Append allow/deny records to logs/network.jsonl",
    },
    FieldSpec {
        path: &["search", "enabled"],
        label: "Enable web_search",
        page: Page::Search,
        kind: FieldKind::Bool,
        help: "Without a usable backend the tool is not registered",
    },
    FieldSpec {
        path: &["search", "backend"],
        label: "Search backend",
        page: Page::Search,
        kind: FieldKind::Select(SEARCH_BACKENDS),
        help: "brave / tavily / searxng",
    },
    FieldSpec {
        path: &["search", "api_key_env"],
        label: "Search key env",
        page: Page::Search,
        kind: FieldKind::Text,
        help: "Empty uses BRAVE_API_KEY or TAVILY_API_KEY",
    },
    FieldSpec {
        path: &["search", "api_key_command"],
        label: "Search key command",
        page: Page::Search,
        kind: FieldKind::Lines,
        help: "One argv per line; used when the env key is absent",
    },
    FieldSpec {
        path: &["search", "max_results"],
        label: "Max results",
        page: Page::Search,
        kind: FieldKind::Integer,
        help: "1..=20",
    },
    FieldSpec {
        path: &["search", "base_url"],
        label: "Search base URL",
        page: Page::Search,
        kind: FieldKind::OptionalText,
        help: "SearXNG instance or custom endpoint",
    },
    FieldSpec {
        path: &["skills", "enabled"],
        label: "Enable Agent Skills",
        page: Page::Skills,
        kind: FieldKind::Bool,
        help: "Discover SKILL.md roots on startup",
    },
    FieldSpec {
        path: &["skills", "roots"],
        label: "Extra skill roots",
        page: Page::Skills,
        kind: FieldKind::Lines,
        help: "One path per line; ~/ is expanded",
    },
    FieldSpec {
        path: &["update", "enabled"],
        label: "Enable version checks",
        page: Page::UpdateAppearance,
        kind: FieldKind::Bool,
        help: "Never replaces the binary automatically",
    },
    FieldSpec {
        path: &["update", "check"],
        label: "Check on startup",
        page: Page::UpdateAppearance,
        kind: FieldKind::Bool,
        help: "Cached for check_days",
    },
    FieldSpec {
        path: &["update", "check_days"],
        label: "Check interval (days)",
        page: Page::UpdateAppearance,
        kind: FieldKind::Integer,
        help: "Minimum days between checks",
    },
    FieldSpec {
        path: &["update", "repo"],
        label: "Update repo",
        page: Page::UpdateAppearance,
        kind: FieldKind::Text,
        help: "GitHub owner/repo whose releases are the channel",
    },
    FieldSpec {
        path: &["update", "base_url"],
        label: "Update API base URL",
        page: Page::UpdateAppearance,
        kind: FieldKind::OptionalText,
        help: "Mirror or local test server",
    },
];

pub const TABLES: &[TableSpec] = &[
    TableSpec {
        path: &["providers"],
        title: "Providers",
        page: Page::Provider,
        columns: &[
            ColumnSpec {
                key: "name",
                label: "Name",
                kind: FieldKind::Text,
            },
            ColumnSpec {
                key: "base_url",
                label: "Base URL",
                kind: FieldKind::Text,
            },
            ColumnSpec {
                key: "api_key_env",
                label: "Key env",
                kind: FieldKind::OptionalText,
            },
            ColumnSpec {
                key: "api_key_command",
                label: "Key command (one argv per line)",
                kind: FieldKind::Lines,
            },
            ColumnSpec {
                key: "default_model",
                label: "Default model",
                kind: FieldKind::OptionalText,
            },
            ColumnSpec {
                key: "stream_usage",
                label: "stream_usage",
                kind: FieldKind::Bool,
            },
        ],
    },
    TableSpec {
        path: &["pricing"],
        title: "Pricing (USD per million tokens)",
        page: Page::ApprovalPricing,
        columns: &[
            ColumnSpec {
                key: "model",
                label: "Model",
                kind: FieldKind::Text,
            },
            ColumnSpec {
                key: "input_usd_per_mtok",
                label: "Input",
                kind: FieldKind::Float,
            },
            ColumnSpec {
                key: "output_usd_per_mtok",
                label: "Output",
                kind: FieldKind::Float,
            },
            ColumnSpec {
                key: "cache_read_usd_per_mtok",
                label: "Cache read",
                kind: FieldKind::Float,
            },
            ColumnSpec {
                key: "cache_write_usd_per_mtok",
                label: "Cache write",
                kind: FieldKind::Float,
            },
        ],
    },
    TableSpec {
        path: &["approval", "rules"],
        title: "Approval rules",
        page: Page::ApprovalPricing,
        columns: &[
            ColumnSpec {
                key: "id",
                label: "ID",
                kind: FieldKind::Text,
            },
            ColumnSpec {
                key: "tool",
                label: "Tool",
                kind: FieldKind::OptionalText,
            },
            ColumnSpec {
                key: "command_prefix",
                label: "Command prefix",
                kind: FieldKind::OptionalText,
            },
            ColumnSpec {
                key: "path_glob",
                label: "Path glob",
                kind: FieldKind::OptionalText,
            },
            ColumnSpec {
                key: "action",
                label: "Action",
                kind: FieldKind::Select(RULE_ACTIONS),
            },
            ColumnSpec {
                key: "scope",
                label: "Scope",
                kind: FieldKind::Select(RULE_SCOPES),
            },
        ],
    },
    TableSpec {
        path: &["mcp", "servers"],
        title: "MCP servers",
        page: Page::Mcp,
        columns: &[
            ColumnSpec {
                key: "name",
                label: "Name",
                kind: FieldKind::Text,
            },
            ColumnSpec {
                key: "command",
                label: "Command",
                kind: FieldKind::Text,
            },
            ColumnSpec {
                key: "args",
                label: "Args (one per line)",
                kind: FieldKind::Lines,
            },
            ColumnSpec {
                key: "env",
                label: "Env (KEY=VALUE per line)",
                kind: FieldKind::Lines,
            },
            ColumnSpec {
                key: "enabled",
                label: "Enabled",
                kind: FieldKind::Bool,
            },
            ColumnSpec {
                key: "timeout_secs",
                label: "Timeout (s)",
                kind: FieldKind::Integer,
            },
        ],
    },
];

/// GUI-only preferences live in `gui.json`, not `config.toml`.
pub const GUI_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        path: &["gui", "theme"],
        label: "Theme",
        page: Page::UpdateAppearance,
        kind: FieldKind::Select(&["system", "light", "dark"]),
        help: "Follow the system appearance or force a theme",
    },
    FieldSpec {
        path: &["gui", "font_size"],
        label: "Font size",
        page: Page::UpdateAppearance,
        kind: FieldKind::Integer,
        help: "Base UI font size in pixels",
    },
    FieldSpec {
        path: &["gui", "code_font"],
        label: "Code font",
        page: Page::UpdateAppearance,
        kind: FieldKind::Text,
        help: "Monospace family used for code blocks",
    },
    FieldSpec {
        path: &["gui", "density"],
        label: "Density",
        page: Page::UpdateAppearance,
        kind: FieldKind::Select(&["comfortable", "compact"]),
        help: "Spacing of transcript rows",
    },
];

pub fn fields_for(page: Page) -> impl Iterator<Item = &'static FieldSpec> {
    FIELDS.iter().filter(move |field| field.page == page)
}

pub fn tables_for(page: Page) -> impl Iterator<Item = &'static TableSpec> {
    TABLES.iter().filter(move |table| table.page == page)
}
