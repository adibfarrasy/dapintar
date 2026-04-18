use serde::{Deserialize, Serialize};

/// An incoming DAP request from the editor.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DapRequest {
    pub seq: u64,
    #[serde(rename = "type")]
    pub type_: String,
    pub command: String,
    pub arguments: Option<serde_json::Value>,
}

/// An outgoing DAP response to a request.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DapResponse {
    pub seq: u64,
    #[serde(rename = "type")]
    pub type_: String,
    pub request_seq: u64,
    pub success: bool,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// An outgoing DAP event (unsolicited notification to the editor).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DapEvent {
    pub seq: u64,
    #[serde(rename = "type")]
    pub type_: String,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
}

/// Server capabilities declared in the initialize response body.
/// Fields are added here as features are implemented.
#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub supports_configuration_done_request: bool,
}

impl DapResponse {
    pub fn ok(seq: u64, request_seq: u64, command: &str, body: Option<serde_json::Value>) -> Self {
        Self {
            seq,
            type_: "response".into(),
            request_seq,
            success: true,
            command: command.into(),
            body,
            message: None,
        }
    }

    pub fn err(seq: u64, request_seq: u64, command: &str, message: &str) -> Self {
        Self {
            seq,
            type_: "response".into(),
            request_seq,
            success: false,
            command: command.into(),
            body: None,
            message: Some(message.into()),
        }
    }
}

impl DapEvent {
    pub fn new(seq: u64, event: &str, body: Option<serde_json::Value>) -> Self {
        Self {
            seq,
            type_: "event".into(),
            event: event.into(),
            body,
        }
    }
}
