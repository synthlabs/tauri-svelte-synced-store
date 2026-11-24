use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::fmt;

/// Specta-exportable JSON value type for `Typescript::default()`.
///
/// Uses `f64` for numbers so `specta-typescript` does not reject `serde_json::Number`'s `i64`/`u64`
/// variants with `BigIntForbidden`.
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(HashMap<String, JsonValue>),
}

/// Message streamed from Rust to a subscribed frontend `SyncedState` over a Tauri `Channel`.
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SyncMessage {
    Snapshot {
        #[specta(type = JsonValue)]
        value: serde_json::Value,
    },
}

/// Payload sent by the frontend `update_state` command.
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StateUpdate {
    pub name: String,
    #[specta(type = JsonValue)]
    pub value: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscriber_id: Option<String>,
}

/// Structured error returned by sync commands and fallible `StateSyncer` methods.
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "code", content = "message", rename_all = "camelCase")]
pub enum SyncError {
    UnknownKey(String),
    TypeMismatch(String),
    InvalidValue(String),
    SerializeFailed(String),
    ChannelSendFailed(String),
    PersistenceFailed(String),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKey(msg) => write!(f, "unknown state key: {msg}"),
            Self::TypeMismatch(msg) => write!(f, "state type mismatch: {msg}"),
            Self::InvalidValue(msg) => write!(f, "invalid state value: {msg}"),
            Self::SerializeFailed(msg) => write!(f, "failed to serialize state: {msg}"),
            Self::ChannelSendFailed(msg) => write!(f, "failed to send channel message: {msg}"),
            Self::PersistenceFailed(msg) => write!(f, "failed to persist state: {msg}"),
        }
    }
}

impl std::error::Error for SyncError {}
