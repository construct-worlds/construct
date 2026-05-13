//! JSON-RPC 2.0 envelope types.
//!
//! Both AHP and IPC use these. A message on the wire is one of:
//!
//! - a [`Request`] (has both `id` and `method`)
//! - a [`Response`] (has `id`, plus exactly one of `result`/`error`)
//! - a [`Notification`] (has `method`, no `id`)

use serde::{Deserialize, Serialize};

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl Request {
    pub fn new(id: impl Into<serde_json::Value>, method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Response {
    pub fn ok(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: serde_json::Value, error: ErrorObject) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("jsonrpc {code}: {message}")]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl ErrorObject {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_request(msg: impl Into<String>) -> Self {
        Self::new(error_codes::INVALID_REQUEST, msg)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(error_codes::METHOD_NOT_FOUND, format!("method not found: {method}"))
    }

    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self::new(error_codes::INVALID_PARAMS, msg)
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(error_codes::INTERNAL_ERROR, msg)
    }
}

pub mod error_codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const SESSION_NOT_FOUND: i64 = -32001;
    pub const ADAPTER_NOT_FOUND: i64 = -32002;
    pub const ADAPTER_FAILED: i64 = -32003;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Request,
    Response,
    Notification,
}

/// Classify a raw JSON value as one of the JSON-RPC envelope kinds, based on
/// which fields are present.
pub fn classify(v: &serde_json::Value) -> Option<MessageKind> {
    let obj = v.as_object()?;
    let has_id = obj.contains_key("id");
    let has_method = obj.contains_key("method");
    Some(match (has_id, has_method) {
        (true, true) => MessageKind::Request,
        (true, false) => MessageKind::Response,
        (false, true) => MessageKind::Notification,
        (false, false) => return None,
    })
}
