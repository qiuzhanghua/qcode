//! ACP v1 wire types.
//!
//! Only the parts qcode speaks are modelled: the client's baseline methods, the
//! `session/update` variants we emit, and the permission round trip. Everything
//! unknown is ignored or rejected by the caller instead of being guessed at.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol version this build speaks. A client asking for a newer one is
/// answered with this version and is expected to disconnect if it cannot.
pub const PROTOCOL_VERSION: u16 = 1;

pub const METHOD_INITIALIZE: &str = "initialize";
pub const METHOD_AUTHENTICATE: &str = "authenticate";
pub const METHOD_SESSION_NEW: &str = "session/new";
pub const METHOD_SESSION_LOAD: &str = "session/load";
pub const METHOD_SESSION_PROMPT: &str = "session/prompt";
pub const METHOD_SESSION_CANCEL: &str = "session/cancel";
pub const METHOD_SESSION_UPDATE: &str = "session/update";
pub const METHOD_REQUEST_PERMISSION: &str = "session/request_permission";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Implementation {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemCapabilities {
    #[serde(default)]
    pub read_text_file: bool,
    #[serde(default)]
    pub write_text_file: bool,
}

/// What the client says it can do. `fs` and `terminal` decide whether file and
/// shell work may be delegated; this build keeps both local, so the flags are
/// recorded only to report them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default)]
    pub fs: FileSystemCapabilities,
    #[serde(default)]
    pub terminal: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    #[serde(default)]
    pub protocol_version: u16,
    #[serde(default)]
    pub client_capabilities: ClientCapabilities,
    #[serde(default)]
    pub client_info: Option<Implementation>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    pub image: bool,
    pub audio: bool,
    pub embedded_context: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    pub load_session: bool,
    pub prompt_capabilities: PromptCapabilities,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub protocol_version: u16,
    pub agent_capabilities: AgentCapabilities,
    pub agent_info: Implementation,
    pub auth_methods: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default)]
    pub additional_directories: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionRequest {
    pub session_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelNotification {
    pub session_id: String,
}

/// Inbound prompt content. Image and audio are rejected: the agent advertises
/// `promptCapabilities` with both false.
#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ResourceLink {
        uri: String,
        #[serde(default)]
        name: Option<String>,
    },
    Resource {
        resource: EmbeddedResource,
    },
    Image {
        #[serde(default)]
        data: String,
        #[serde(default)]
        mime_type: String,
    },
    Audio {
        #[serde(default)]
        data: String,
        #[serde(default)]
        mime_type: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedResource {
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub blob: Option<String>,
}

/// Content the agent sends back.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutgoingContent {
    Text { text: String },
}

/// One `session/update` payload.
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "sessionUpdate",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SessionUpdate {
    UserMessageChunk {
        content: OutgoingContent,
        message_id: Option<String>,
    },
    AgentMessageChunk {
        content: OutgoingContent,
        message_id: Option<String>,
    },
    AgentThoughtChunk {
        content: OutgoingContent,
        message_id: Option<String>,
    },
    ToolCall {
        tool_call_id: String,
        title: String,
        kind: String,
        status: String,
        name: Option<String>,
        raw_input: Option<Value>,
    },
    ToolCallUpdate {
        tool_call_id: String,
        status: Option<String>,
        content: Option<Vec<ToolCallContent>>,
        raw_output: Option<Value>,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    UsageUpdate {
        used: u64,
        size: u64,
        cost: Option<Cost>,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ToolCallContent {
    Content { content: OutgoingContent },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanEntry {
    pub content: String,
    pub priority: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Cost {
    pub amount: f64,
    pub currency: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallRef {
    pub tool_call_id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub raw_input: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    pub session_id: String,
    pub tool_call: ToolCallRef,
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "outcome",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RequestPermissionOutcome {
    Selected { option_id: String },
    Cancelled,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequestPermissionResponse {
    pub outcome: RequestPermissionOutcome,
}

/// The three answers qcode asks for; the option ids are what come back.
pub const OPTION_ALLOW_ONCE: &str = "allow-once";
pub const OPTION_ALLOW_SESSION: &str = "allow-session";
pub const OPTION_REJECT_ONCE: &str = "reject-once";

pub fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            option_id: OPTION_ALLOW_ONCE.to_string(),
            name: "Allow once".to_string(),
            kind: "allow_once".to_string(),
        },
        PermissionOption {
            option_id: OPTION_ALLOW_SESSION.to_string(),
            name: "Allow this tool for the session".to_string(),
            kind: "allow_always".to_string(),
        },
        PermissionOption {
            option_id: OPTION_REJECT_ONCE.to_string(),
            name: "Reject".to_string(),
            kind: "reject_once".to_string(),
        },
    ]
}

/// Map a qcode tool name onto the protocol's `ToolKind`.
pub fn tool_kind(name: &str) -> &'static str {
    match name {
        "read_file" | "list_dir" => "read",
        "write_file" | "edit_file" | "apply_patch" => "edit",
        "glob" | "grep" => "search",
        "run_shell" => "execute",
        "web_fetch" | "web_search" => "fetch",
        "todo_write" => "think",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_prompt_parses_and_images_are_visible_as_such() {
        let request: PromptRequest = serde_json::from_str(
            r#"{
                "sessionId": "s1",
                "prompt": [
                    {"type": "text", "text": "hello"},
                    {"type": "resource_link", "uri": "file:///a.rs", "name": "a.rs"},
                    {"type": "image", "data": "aGk=", "mimeType": "image/png"}
                ]
            }"#,
        )
        .expect("prompt");

        assert_eq!(request.session_id, "s1");
        assert!(matches!(request.prompt[0], ContentBlock::Text { .. }));
        assert!(matches!(
            request.prompt[1],
            ContentBlock::ResourceLink { .. }
        ));
        assert!(matches!(request.prompt[2], ContentBlock::Image { .. }));
    }

    #[test]
    fn updates_carry_the_discriminator_the_schema_asks_for() {
        let update = SessionUpdate::AgentMessageChunk {
            content: OutgoingContent::Text {
                text: "hi".to_string(),
            },
            message_id: None,
        };
        let value = serde_json::to_value(&update).expect("json");
        assert_eq!(value["sessionUpdate"], "agent_message_chunk");
        assert_eq!(value["content"]["type"], "text");
        assert_eq!(value["content"]["text"], "hi");
        assert!(
            value.get("messageId").is_some(),
            "null is allowed, absence is not"
        );

        let call = SessionUpdate::ToolCall {
            tool_call_id: "c1".to_string(),
            title: "run_shell".to_string(),
            kind: "execute".to_string(),
            status: "pending".to_string(),
            name: Some("run_shell".to_string()),
            raw_input: Some(serde_json::json!({"command": "ls"})),
        };
        let value = serde_json::to_value(&call).expect("json");
        assert_eq!(value["sessionUpdate"], "tool_call");
        assert_eq!(value["toolCallId"], "c1");
        assert_eq!(value["rawInput"]["command"], "ls");
    }

    #[test]
    fn a_permission_answer_round_trips_through_the_outcome_union() {
        let selected: RequestPermissionResponse = serde_json::from_str(
            r#"{"outcome": {"outcome": "selected", "optionId": "allow-once"}}"#,
        )
        .expect("selected");
        assert_eq!(
            selected.outcome,
            RequestPermissionOutcome::Selected {
                option_id: OPTION_ALLOW_ONCE.to_string()
            }
        );

        let cancelled: RequestPermissionResponse =
            serde_json::from_str(r#"{"outcome": {"outcome": "cancelled"}}"#).expect("cancelled");
        assert_eq!(cancelled.outcome, RequestPermissionOutcome::Cancelled);
    }

    #[test]
    fn tool_names_map_to_protocol_kinds() {
        assert_eq!(tool_kind("read_file"), "read");
        assert_eq!(tool_kind("apply_patch"), "edit");
        assert_eq!(tool_kind("grep"), "search");
        assert_eq!(tool_kind("run_shell"), "execute");
        assert_eq!(tool_kind("web_fetch"), "fetch");
        assert_eq!(tool_kind("something_custom"), "other");
    }
}
