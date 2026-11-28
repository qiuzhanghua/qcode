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
    /// One model bar: default model + the provider routes table.
    Models,
    Agent,
    NetworkSandbox,
    Search,
    Mcp,
    Skills,
    ApprovalPricing,
    Appearance,
    Update,
    Review,
}

impl Page {
    pub const ALL: [Page; 11] = [
        Page::Welcome,
        Page::Models,
        Page::Agent,
        Page::NetworkSandbox,
        Page::Search,
        Page::Mcp,
        Page::Skills,
        Page::ApprovalPricing,
        Page::Appearance,
        Page::Update,
        Page::Review,
    ];

    /// 侧栏里的页面名（已翻译）。
    ///
    /// 这里原本硬编码 11 个英文名，中文界面下整条侧栏是英文，而中文那条门禁
    /// （只查汉字）对它完全沉默。译文 key 直接写成 `tr("…")` 的字面量：门禁 6
    /// 靠这些字面量才知道「这些 key 有人在用」，绕一层 `*_key()` 它就看不见了。
    pub fn title(self) -> std::borrow::Cow<'static, str> {
        use qcode_i18n::tr;
        match self {
            Self::Welcome => tr("gui.setup.page.welcome"),
            Self::Models => tr("gui.setup.page.models"),
            Self::Agent => tr("gui.setup.page.agent"),
            Self::NetworkSandbox => tr("gui.setup.page.network_sandbox"),
            Self::Search => tr("gui.setup.page.search"),
            Self::Mcp => tr("gui.setup.page.mcp"),
            Self::Skills => tr("gui.setup.page.skills"),
            Self::ApprovalPricing => tr("gui.setup.page.approval_pricing"),
            Self::Appearance => tr("gui.setup.page.appearance"),
            Self::Update => tr("gui.setup.page.update"),
            Self::Review => tr("gui.setup.page.review"),
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
    /// 输入框名字的**译文 key**（`gui.setup.spec.<path>.label`）。
    ///
    /// 这里原本放英文原文。中文界面下整张设置页的字段名与说明因此一直是英文，
    /// 而「禁止裸中文」那条门禁只查汉字，对它完全沉默（I18N.md §10.12）。
    pub label: &'static str,
    pub page: Page,
    pub kind: FieldKind,
    /// 一行说明的译文 key（`gui.setup.spec.<path>.help`）。
    pub help: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct ColumnSpec {
    pub key: &'static str,
    /// 列名的译文 key（`gui.setup.spec.<table>.col.<key>`）。
    pub label: &'static str,
    pub kind: FieldKind,
}

#[derive(Debug, Clone, Copy)]
pub struct TableSpec {
    pub path: &'static [&'static str],
    /// 表标题的译文 key（`gui.setup.spec.<table>.title`）。
    pub title: &'static str,
    pub page: Page,
    pub columns: &'static [ColumnSpec],
}

// Sandbox mode 按平台展示：每种系统只列出可用的沙箱方案，跨平台迁移配置时
// 其它平台的值仍可解析，但设置页下拉不再出现不可用的选项。
#[cfg(target_os = "windows")]
const SANDBOX_MODES: &[&str] = &["none", "windows-acl"];
#[cfg(target_os = "linux")]
const SANDBOX_MODES: &[&str] = &["none", "landlock", "bwrap"];
#[cfg(target_os = "macos")]
const SANDBOX_MODES: &[&str] = &["none", "sandbox-exec"];
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
const SANDBOX_MODES: &[&str] = &["none"];
const NETWORK_MODES: &[&str] = &["off", "proxy", "strict", "open"];

/// Whether `[sandbox] network = "strict"` may be selected for a mode.
///
/// ADR-0104 §7: `strict` is implemented only for macOS `sandbox-exec`
/// (`sandbox::build` rejects `ProxyOnly` for every other backend), so the
/// settings page hides the option unless that backend is selected — a user
/// must not be able to pick a combination that fails at assembly.
pub fn network_strict_allowed(mode: &str) -> bool {
    mode == "sandbox-exec"
}
const UNKNOWN_HOSTS: &[&str] = &["ask", "deny", "allow"];
const SEARCH_BACKENDS: &[&str] = &["brave", "tavily", "searxng"];
const RULE_ACTIONS: &[&str] = &["allow", "ask", "deny"];
const RULE_SCOPES: &[&str] = &["session", "project", "user"];
/// `[mcp] sandbox`: the crate-wide default, overridable per server (ADR-0066).
/// `confine` is the default value, so it is what an untouched config means.
const MCP_SANDBOX_MODES: &[&str] = &["confine", "off"];
/// The same choice on one server's row, where "no override" is a real state:
/// the field is `Option`, and writing an explicit `confine` would freeze that
/// server out of a later `[mcp] sandbox = "off"` (ADR-0066 §1). A third entry
/// rather than a sentinel value, so the picker cannot produce a name the loader
/// rejects.
const MCP_SERVER_SANDBOX_MODES: &[&str] = &["inherit", "confine", "off"];

pub const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        path: &["context", "window_tokens"],
        label: "gui.setup.spec.context.window_tokens.label",
        page: Page::Agent,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.context.window_tokens.help",
    },
    FieldSpec {
        path: &["agent", "max_turns"],
        label: "gui.setup.spec.agent.max_turns.label",
        page: Page::Agent,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.agent.max_turns.help",
    },
    FieldSpec {
        path: &["agent", "max_cost_usd"],
        label: "gui.setup.spec.agent.max_cost_usd.label",
        page: Page::Agent,
        kind: FieldKind::Float,
        help: "gui.setup.spec.agent.max_cost_usd.help",
    },
    FieldSpec {
        path: &["session_title", "enabled"],
        label: "gui.setup.spec.session_title.enabled.label",
        page: Page::Agent,
        kind: FieldKind::Bool,
        help: "gui.setup.spec.session_title.enabled.help",
    },
    FieldSpec {
        path: &["session_title", "max_title_bytes"],
        label: "gui.setup.spec.session_title.max_title_bytes.label",
        page: Page::Agent,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.session_title.max_title_bytes.help",
    },
    FieldSpec {
        path: &["session_title", "fallback_max_words"],
        label: "gui.setup.spec.session_title.fallback_max_words.label",
        page: Page::Agent,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.session_title.fallback_max_words.help",
    },
    FieldSpec {
        path: &["session_title", "fallback_max_bytes"],
        label: "gui.setup.spec.session_title.fallback_max_bytes.label",
        page: Page::Agent,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.session_title.fallback_max_bytes.help",
    },
    FieldSpec {
        path: &["session_title", "max_input_bytes"],
        label: "gui.setup.spec.session_title.max_input_bytes.label",
        page: Page::Agent,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.session_title.max_input_bytes.help",
    },
    FieldSpec {
        path: &["session_title", "max_output_tokens"],
        label: "gui.setup.spec.session_title.max_output_tokens.label",
        page: Page::Agent,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.session_title.max_output_tokens.help",
    },
    FieldSpec {
        path: &["session_title", "timeout_ms"],
        label: "gui.setup.spec.session_title.timeout_ms.label",
        page: Page::Agent,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.session_title.timeout_ms.help",
    },
    FieldSpec {
        path: &["session_title", "target_words"],
        label: "gui.setup.spec.session_title.target_words.label",
        page: Page::Agent,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.session_title.target_words.help",
    },
    FieldSpec {
        path: &["session_title", "target_cjk_chars"],
        label: "gui.setup.spec.session_title.target_cjk_chars.label",
        page: Page::Agent,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.session_title.target_cjk_chars.help",
    },
    FieldSpec {
        path: &["session_title", "provider"],
        label: "gui.setup.spec.session_title.provider.label",
        page: Page::Agent,
        kind: FieldKind::OptionalText,
        help: "gui.setup.spec.session_title.provider.help",
    },
    FieldSpec {
        path: &["session_title", "model"],
        label: "gui.setup.spec.session_title.model.label",
        page: Page::Agent,
        kind: FieldKind::OptionalText,
        help: "gui.setup.spec.session_title.model.help",
    },
    FieldSpec {
        path: &["terminal", "shell"],
        label: "gui.setup.spec.terminal.shell.label",
        page: Page::Appearance,
        kind: FieldKind::Text,
        help: "gui.setup.spec.terminal.shell.help",
    },
    FieldSpec {
        path: &["terminal", "start_cwd"],
        label: "gui.setup.spec.terminal.start_cwd.label",
        page: Page::Appearance,
        kind: FieldKind::Text,
        help: "gui.setup.spec.terminal.start_cwd.help",
    },
    FieldSpec {
        path: &["terminal", "font"],
        label: "gui.setup.spec.terminal.font.label",
        page: Page::Appearance,
        kind: FieldKind::Text,
        help: "gui.setup.spec.terminal.font.help",
    },
    FieldSpec {
        path: &["terminal", "max_scrollback"],
        label: "gui.setup.spec.terminal.max_scrollback.label",
        page: Page::Appearance,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.terminal.max_scrollback.help",
    },
    FieldSpec {
        path: &["terminal", "max_terminals"],
        label: "gui.setup.spec.terminal.max_terminals.label",
        page: Page::Appearance,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.terminal.max_terminals.help",
    },
    FieldSpec {
        path: &["shell", "env_sampling"],
        label: "gui.setup.spec.shell.env_sampling.label",
        page: Page::Appearance,
        kind: FieldKind::Select(&["auto", "off"]),
        help: "gui.setup.spec.shell.env_sampling.help",
    },
    FieldSpec {
        path: &["shell", "env_command"],
        label: "gui.setup.spec.shell.env_command.label",
        page: Page::Appearance,
        kind: FieldKind::Text,
        help: "gui.setup.spec.shell.env_command.help",
    },
    FieldSpec {
        path: &["shell", "env_timeout_ms"],
        label: "gui.setup.spec.shell.env_timeout_ms.label",
        page: Page::Appearance,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.shell.env_timeout_ms.help",
    },
    FieldSpec {
        path: &["shell", "env_ttl_secs"],
        label: "gui.setup.spec.shell.env_ttl_secs.label",
        page: Page::Appearance,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.shell.env_ttl_secs.help",
    },
    FieldSpec {
        path: &["shell", "env_include"],
        label: "gui.setup.spec.shell.env_include.label",
        page: Page::Appearance,
        kind: FieldKind::Lines,
        help: "gui.setup.spec.shell.env_include.help",
    },
    FieldSpec {
        path: &["shell", "env_exclude"],
        label: "gui.setup.spec.shell.env_exclude.label",
        page: Page::Appearance,
        kind: FieldKind::Lines,
        help: "gui.setup.spec.shell.env_exclude.help",
    },
    FieldSpec {
        path: &["shell", "env_sample_sandboxed"],
        label: "gui.setup.spec.shell.env_sample_sandboxed.label",
        page: Page::Appearance,
        kind: FieldKind::Bool,
        help: "gui.setup.spec.shell.env_sample_sandboxed.help",
    },
    FieldSpec {
        path: &["shell", "audit"],
        label: "gui.setup.spec.shell.audit.label",
        page: Page::Appearance,
        kind: FieldKind::Select(&["names", "off"]),
        help: "gui.setup.spec.shell.audit.help",
    },
    FieldSpec {
        path: &["ui", "busy_delivery"],
        label: "gui.setup.spec.ui.busy_delivery.label",
        page: Page::Agent,
        kind: FieldKind::Select(&["queue", "steer"]),
        help: "gui.setup.spec.ui.busy_delivery.help",
    },
    // 语言放在 `FIELDS`（config.toml）而不是 `GUI_FIELDS`（gui.json）：CLI 与
    // daemon 也要读它，gui.json 它们看不见。
    //
    // 页面选 Agent 而不是 Appearance：`Appearance` 按设计只收 `gui.json` 的字段
    // （见 SETTINGS-UI-V2.md §4.2 的拆分理由），把一条 config.toml 字段放进去会
    // 让那一页出现两种保存位置。这里和 `busy_delivery` 挨着——同属 `[ui]`。
    FieldSpec {
        path: &["ui", "language"],
        label: "gui.setup.spec.ui.language.label",
        page: Page::Agent,
        kind: FieldKind::Select(&["auto", "zh-CN", "en"]),
        help: "gui.setup.spec.ui.language.help",
    },
    FieldSpec {
        path: &["agent", "snapshot_shell"],
        label: "gui.setup.spec.agent.snapshot_shell.label",
        page: Page::Agent,
        kind: FieldKind::Bool,
        help: "gui.setup.spec.agent.snapshot_shell.help",
    },
    FieldSpec {
        path: &["sandbox", "mode"],
        label: "gui.setup.spec.sandbox.mode.label",
        page: Page::NetworkSandbox,
        kind: FieldKind::Select(SANDBOX_MODES),
        help: "gui.setup.spec.sandbox.mode.help",
    },
    FieldSpec {
        path: &["sandbox", "network"],
        label: "gui.setup.spec.sandbox.network.label",
        page: Page::NetworkSandbox,
        kind: FieldKind::Select(NETWORK_MODES),
        help: "gui.setup.spec.sandbox.network.help",
    },
    FieldSpec {
        path: &["approval", "allow"],
        label: "gui.setup.spec.approval.allow.label",
        page: Page::NetworkSandbox,
        kind: FieldKind::Lines,
        help: "gui.setup.spec.approval.allow.help",
    },
    FieldSpec {
        path: &["approval", "deny"],
        label: "gui.setup.spec.approval.deny.label",
        page: Page::NetworkSandbox,
        kind: FieldKind::Lines,
        help: "gui.setup.spec.approval.deny.help",
    },
    FieldSpec {
        path: &["sandbox", "extra_writable"],
        label: "gui.setup.spec.sandbox.extra_writable.label",
        page: Page::NetworkSandbox,
        kind: FieldKind::Lines,
        help: "gui.setup.spec.sandbox.extra_writable.help",
    },
    FieldSpec {
        path: &["sandbox", "extra_read"],
        label: "gui.setup.spec.sandbox.extra_read.label",
        page: Page::NetworkSandbox,
        kind: FieldKind::Lines,
        help: "gui.setup.spec.sandbox.extra_read.help",
    },
    FieldSpec {
        path: &["sandbox", "allow_pty"],
        label: "gui.setup.spec.sandbox.allow_pty.label",
        page: Page::NetworkSandbox,
        kind: FieldKind::Bool,
        help: "gui.setup.spec.sandbox.allow_pty.help",
    },
    FieldSpec {
        path: &["network", "allow"],
        label: "gui.setup.spec.network.allow.label",
        page: Page::NetworkSandbox,
        kind: FieldKind::Lines,
        help: "gui.setup.spec.network.allow.help",
    },
    FieldSpec {
        path: &["network", "allow_private"],
        label: "gui.setup.spec.network.allow_private.label",
        page: Page::NetworkSandbox,
        kind: FieldKind::Bool,
        help: "gui.setup.spec.network.allow_private.help",
    },
    FieldSpec {
        path: &["network", "unknown"],
        label: "gui.setup.spec.network.unknown.label",
        page: Page::NetworkSandbox,
        kind: FieldKind::Select(UNKNOWN_HOSTS),
        help: "gui.setup.spec.network.unknown.help",
    },
    FieldSpec {
        path: &["network", "audit"],
        label: "gui.setup.spec.network.audit.label",
        page: Page::NetworkSandbox,
        kind: FieldKind::Bool,
        help: "gui.setup.spec.network.audit.help",
    },
    FieldSpec {
        path: &["search", "enabled"],
        label: "gui.setup.spec.search.enabled.label",
        page: Page::Search,
        kind: FieldKind::Bool,
        help: "gui.setup.spec.search.enabled.help",
    },
    FieldSpec {
        path: &["search", "backend"],
        label: "gui.setup.spec.search.backend.label",
        page: Page::Search,
        kind: FieldKind::Select(SEARCH_BACKENDS),
        help: "gui.setup.spec.search.backend.help",
    },
    FieldSpec {
        path: &["search", "api_key_env"],
        label: "gui.setup.spec.search.api_key_env.label",
        page: Page::Search,
        kind: FieldKind::Text,
        help: "gui.setup.spec.search.api_key_env.help",
    },
    FieldSpec {
        path: &["search", "api_key_command"],
        label: "gui.setup.spec.search.api_key_command.label",
        page: Page::Search,
        kind: FieldKind::Lines,
        help: "gui.setup.spec.search.api_key_command.help",
    },
    FieldSpec {
        path: &["search", "max_results"],
        label: "gui.setup.spec.search.max_results.label",
        page: Page::Search,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.search.max_results.help",
    },
    FieldSpec {
        path: &["search", "base_url"],
        label: "gui.setup.spec.search.base_url.label",
        page: Page::Search,
        kind: FieldKind::OptionalText,
        help: "gui.setup.spec.search.base_url.help",
    },
    FieldSpec {
        path: &["skills", "enabled"],
        label: "gui.setup.spec.skills.enabled.label",
        page: Page::Skills,
        kind: FieldKind::Bool,
        help: "gui.setup.spec.skills.enabled.help",
    },
    FieldSpec {
        path: &["skills", "roots"],
        label: "gui.setup.spec.skills.roots.label",
        page: Page::Skills,
        kind: FieldKind::Lines,
        help: "gui.setup.spec.skills.roots.help",
    },
    FieldSpec {
        path: &["skills", "mcp"],
        label: "gui.setup.spec.skills.mcp.label",
        page: Page::Skills,
        kind: FieldKind::Bool,
        help: "gui.setup.spec.skills.mcp.help",
    },
    FieldSpec {
        path: &["update", "enabled"],
        label: "gui.setup.spec.update.enabled.label",
        page: Page::Update,
        kind: FieldKind::Bool,
        help: "gui.setup.spec.update.enabled.help",
    },
    FieldSpec {
        path: &["update", "check"],
        label: "gui.setup.spec.update.check.label",
        page: Page::Update,
        kind: FieldKind::Bool,
        help: "gui.setup.spec.update.check.help",
    },
    FieldSpec {
        path: &["update", "check_days"],
        label: "gui.setup.spec.update.check_days.label",
        page: Page::Update,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.update.check_days.help",
    },
    FieldSpec {
        path: &["update", "repo"],
        label: "gui.setup.spec.update.repo.label",
        page: Page::Update,
        kind: FieldKind::Text,
        help: "gui.setup.spec.update.repo.help",
    },
    FieldSpec {
        path: &["update", "base_url"],
        label: "gui.setup.spec.update.base_url.label",
        page: Page::Update,
        kind: FieldKind::OptionalText,
        help: "gui.setup.spec.update.base_url.help",
    },
    FieldSpec {
        path: &["log", "level"],
        label: "gui.setup.spec.log.level.label",
        page: Page::Appearance,
        kind: FieldKind::Text,
        help: "gui.setup.spec.log.level.help",
    },
    FieldSpec {
        path: &["mcp", "sandbox"],
        label: "gui.setup.spec.mcp.sandbox.label",
        page: Page::Mcp,
        kind: FieldKind::Select(MCP_SANDBOX_MODES),
        help: "gui.setup.spec.mcp.sandbox.help",
    },
];

pub const TABLES: &[TableSpec] = &[
    TableSpec {
        path: &["providers"],
        title: "gui.setup.spec.providers.title",
        page: Page::Models,
        columns: &[
            ColumnSpec {
                key: "name",
                label: "gui.setup.spec.providers.col.name",
                kind: FieldKind::Text,
            },
            ColumnSpec {
                key: "base_url",
                label: "gui.setup.spec.providers.col.base_url",
                kind: FieldKind::Text,
            },
            ColumnSpec {
                key: "protocol",
                label: "gui.setup.spec.providers.col.protocol",
                kind: FieldKind::Select(&["openai", "anthropic"]),
            },
            ColumnSpec {
                key: "api_key_env",
                label: "gui.setup.spec.providers.col.api_key_env",
                kind: FieldKind::OptionalText,
            },
            ColumnSpec {
                key: "images",
                label: "gui.setup.spec.providers.col.images",
                kind: FieldKind::Bool,
            },
            ColumnSpec {
                key: "api_key_command",
                label: "gui.setup.spec.providers.col.api_key_command",
                kind: FieldKind::Lines,
            },
            ColumnSpec {
                key: "default_model",
                label: "gui.setup.spec.providers.col.default_model",
                kind: FieldKind::OptionalText,
            },
            ColumnSpec {
                key: "extra_body",
                label: "gui.setup.spec.providers.col.extra_body",
                kind: FieldKind::Lines,
            },
            ColumnSpec {
                key: "stream_usage",
                label: "gui.setup.spec.providers.col.stream_usage",
                kind: FieldKind::Bool,
            },
        ],
    },
    TableSpec {
        path: &["pricing"],
        title: "gui.setup.spec.pricing.title",
        page: Page::ApprovalPricing,
        columns: &[
            ColumnSpec {
                key: "model",
                label: "gui.setup.spec.pricing.col.model",
                kind: FieldKind::Text,
            },
            ColumnSpec {
                key: "input_usd_per_mtok",
                label: "gui.setup.spec.pricing.col.input_usd_per_mtok",
                kind: FieldKind::Float,
            },
            ColumnSpec {
                key: "output_usd_per_mtok",
                label: "gui.setup.spec.pricing.col.output_usd_per_mtok",
                kind: FieldKind::Float,
            },
            ColumnSpec {
                key: "cache_read_usd_per_mtok",
                label: "gui.setup.spec.pricing.col.cache_read_usd_per_mtok",
                kind: FieldKind::Float,
            },
            ColumnSpec {
                key: "cache_write_usd_per_mtok",
                label: "gui.setup.spec.pricing.col.cache_write_usd_per_mtok",
                kind: FieldKind::Float,
            },
        ],
    },
    TableSpec {
        path: &["approval", "rules"],
        title: "gui.setup.spec.approval.rules.title",
        page: Page::ApprovalPricing,
        columns: &[
            ColumnSpec {
                key: "id",
                label: "gui.setup.spec.approval.rules.col.id",
                kind: FieldKind::Text,
            },
            ColumnSpec {
                key: "tool",
                label: "gui.setup.spec.approval.rules.col.tool",
                kind: FieldKind::OptionalText,
            },
            ColumnSpec {
                key: "command_prefix",
                label: "gui.setup.spec.approval.rules.col.command_prefix",
                kind: FieldKind::OptionalText,
            },
            ColumnSpec {
                key: "path_glob",
                label: "gui.setup.spec.approval.rules.col.path_glob",
                kind: FieldKind::OptionalText,
            },
            ColumnSpec {
                key: "action",
                label: "gui.setup.spec.approval.rules.col.action",
                kind: FieldKind::Select(RULE_ACTIONS),
            },
            ColumnSpec {
                key: "scope",
                label: "gui.setup.spec.approval.rules.col.scope",
                kind: FieldKind::Select(RULE_SCOPES),
            },
        ],
    },
    TableSpec {
        path: &["mcp", "servers"],
        title: "gui.setup.spec.mcp.servers.title",
        page: Page::Mcp,
        columns: &[
            ColumnSpec {
                key: "name",
                label: "gui.setup.spec.mcp.servers.col.name",
                kind: FieldKind::Text,
            },
            ColumnSpec {
                key: "command",
                label: "gui.setup.spec.mcp.servers.col.command",
                kind: FieldKind::Text,
            },
            ColumnSpec {
                key: "args",
                label: "gui.setup.spec.mcp.servers.col.args",
                kind: FieldKind::Lines,
            },
            ColumnSpec {
                key: "env",
                label: "gui.setup.spec.mcp.servers.col.env",
                kind: FieldKind::Lines,
            },
            ColumnSpec {
                key: "enabled",
                label: "gui.setup.spec.mcp.servers.col.enabled",
                kind: FieldKind::Bool,
            },
            ColumnSpec {
                key: "timeout_secs",
                label: "gui.setup.spec.mcp.servers.col.timeout_secs",
                kind: FieldKind::Integer,
            },
            // ADR-0066 §1: the grant set, editable here so the settings screen
            // is not a lossy round trip. Before these columns existed the grid
            // preserved the four fields by copying them from the file on save
            // — invisible, and unusable for "give this server its project
            // directory".
            ColumnSpec {
                key: "read_paths",
                label: "gui.setup.spec.mcp.servers.col.read_paths",
                kind: FieldKind::Lines,
            },
            ColumnSpec {
                key: "writable_paths",
                label: "gui.setup.spec.mcp.servers.col.writable_paths",
                kind: FieldKind::Lines,
            },
            ColumnSpec {
                key: "credentials",
                label: "gui.setup.spec.mcp.servers.col.credentials",
                kind: FieldKind::Lines,
            },
            ColumnSpec {
                key: "sandbox",
                label: "gui.setup.spec.mcp.servers.col.sandbox",
                kind: FieldKind::Select(MCP_SERVER_SANDBOX_MODES),
            },
        ],
    },
];

/// GUI-only preferences live in `gui.json`, not `config.toml`.
pub const GUI_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        path: &["gui", "theme"],
        label: "gui.setup.spec.gui.theme.label",
        page: Page::Appearance,
        kind: FieldKind::Select(&["system", "light", "dark"]),
        help: "gui.setup.spec.gui.theme.help",
    },
    FieldSpec {
        path: &["gui", "font_size"],
        label: "gui.setup.spec.gui.font_size.label",
        page: Page::Appearance,
        kind: FieldKind::Integer,
        help: "gui.setup.spec.gui.font_size.help",
    },
    FieldSpec {
        path: &["gui", "code_font"],
        label: "gui.setup.spec.gui.code_font.label",
        page: Page::Appearance,
        kind: FieldKind::Text,
        help: "gui.setup.spec.gui.code_font.help",
    },
    FieldSpec {
        path: &["gui", "density"],
        label: "gui.setup.spec.gui.density.label",
        page: Page::Appearance,
        kind: FieldKind::Select(&["comfortable", "compact"]),
        help: "gui.setup.spec.gui.density.help",
    },
    FieldSpec {
        path: &["gui", "send_mode"],
        label: "gui.setup.spec.gui.send_mode.label",
        page: Page::Appearance,
        kind: FieldKind::Select(&["enter", "cmd_enter"]),
        help: "gui.setup.spec.gui.send_mode.help",
    },
];

pub fn fields_for(page: Page) -> impl Iterator<Item = &'static FieldSpec> {
    FIELDS.iter().filter(move |field| field.page == page)
}

/// `Network & Sandbox` 页「沙箱」组的字段，按渲染顺序排列。
///
/// 审批两条归在沙箱组：`mode = none` 时审批就是唯一边界（ADR-0065）。
pub const NETWORK_SANDBOX_SANDBOX_GROUP: &[&[&str]] = &[
    &["sandbox", "mode"],
    &["sandbox", "network"],
    &["sandbox", "extra_writable"],
    &["sandbox", "extra_read"],
    &["sandbox", "allow_pty"],
    &["approval", "allow"],
    &["approval", "deny"],
];

/// 「网络出口」组在当前沙箱网络模式下的**有效**字段。
///
/// 只有代理启动（`proxy` / `strict`）时白名单、私有网段、未知主机策略和审计
/// 才有执行者；`off` 由沙箱直接断网、`open` 直连裸网络，这四项都无从生效，
/// 于是返回空表，让页面用一行说明代替整组。
///
/// 旧写法 `[network] mode` / `enabled` 不在这里：兼容完全交给加载逻辑
/// （`Config::network_mode_with` 在 `[sandbox] network` 未写时回退推导），
/// 设置页只呈现权威键，保存时也不会改写这两个键。
pub fn network_egress_fields(mode: &str) -> Vec<&'static [&'static str]> {
    if matches!(mode, "proxy" | "strict") {
        vec![
            &["network", "allow"],
            &["network", "allow_private"],
            &["network", "unknown"],
            &["network", "audit"],
        ]
    } else {
        Vec::new()
    }
}

/// 按路径取字段定义（分组渲染时用；路径不存在返回 `None`）。
pub fn field_at(path: &[&str]) -> Option<&'static FieldSpec> {
    FIELDS.iter().find(|field| field.path == path)
}

pub fn tables_for(page: Page) -> impl Iterator<Item = &'static TableSpec> {
    TABLES.iter().filter(move |table| table.page == page)
}
