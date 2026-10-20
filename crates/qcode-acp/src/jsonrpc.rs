//! JSON-RPC 2.0 envelopes for the ACP transport.
//!
//! ACP carries one JSON object per line on stdio. Requests and responses share
//! the envelope with notifications; the difference is only which fields are
//! present, so parsing is a single shape with everything optional.

use serde::Deserialize;
use serde_json::{Value, json};

/// One line from the client.
#[derive(Debug, Clone, Deserialize)]
pub struct Incoming {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
}

impl Incoming {
    /// A request is an id plus a method; a response has an id and no method.
    pub fn is_request(&self) -> bool {
        self.method.is_some() && self.id.is_some()
    }

    pub fn is_notification(&self) -> bool {
        self.method.is_some() && self.id.is_none()
    }

    pub fn is_response(&self) -> bool {
        self.method.is_none() && self.id.is_some()
    }
}

pub fn response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

pub fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// JSON-RPC error codes we use; anything unknown is an internal error so the
/// client sees a message instead of a dropped request.
pub mod codes {
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Incoming {
        serde_json::from_str(line).expect("json-rpc line")
    }

    #[test]
    fn envelopes_are_classified_by_their_fields() {
        let request = parse(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        assert!(request.is_request());
        assert!(!request.is_notification());

        let notification = parse(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{}}"#);
        assert!(notification.is_notification());

        let response =
            parse(r#"{"jsonrpc":"2.0","id":7,"result":{"outcome":{"outcome":"cancelled"}}}"#);
        assert!(response.is_response());
    }

    #[test]
    fn outgoing_messages_keep_the_envelope_shape() {
        assert_eq!(response(json!(1), json!({"ok": true}))["jsonrpc"], "2.0");
        assert_eq!(
            error_response(json!(2), codes::METHOD_NOT_FOUND, "no")["error"]["code"],
            -32601
        );
        assert_eq!(
            notification("session/update", json!({}))["method"],
            "session/update"
        );
        assert_eq!(request(9, "session/request_permission", json!({}))["id"], 9);
    }
}
