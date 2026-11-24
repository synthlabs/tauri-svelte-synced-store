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

/// Controls whether updates are sent as adaptive deltas or full snapshots.
#[derive(Serialize, Deserialize, Type, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DeliveryMode {
    #[default]
    Delta,
    Snapshot,
}

/// RFC 6902 JSON Patch operation using RFC 6901 JSON Pointer paths.
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum JsonPatchOp {
    Add {
        path: String,
        #[specta(type = JsonValue)]
        value: serde_json::Value,
    },
    Remove {
        path: String,
    },
    Replace {
        path: String,
        #[specta(type = JsonValue)]
        value: serde_json::Value,
    },
}

/// Revisioned full state snapshot returned by `snapshot_state` and stale `update_state` outcomes.
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StateSnapshot {
    #[specta(type = f64)]
    pub revision: u64,
    #[specta(type = JsonValue)]
    pub value: serde_json::Value,
}

/// Message streamed from Rust to a subscribed frontend `SyncedState` over a Tauri `Channel`.
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SyncMessage {
    Snapshot {
        #[specta(type = f64)]
        revision: u64,
        #[specta(type = JsonValue)]
        value: serde_json::Value,
    },
    Delta {
        #[serde(rename = "baseRevision")]
        #[specta(type = f64)]
        base_revision: u64,
        #[specta(type = f64)]
        revision: u64,
        patch: Vec<JsonPatchOp>,
    },
}

/// Update payload submitted by the frontend `update_state` command.
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UpdatePayload {
    Full {
        #[specta(type = JsonValue)]
        value: serde_json::Value,
    },
    Delta {
        patch: Vec<JsonPatchOp>,
    },
}

/// Payload sent by the frontend `update_state` command.
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StateUpdate {
    pub name: String,
    #[specta(type = f64)]
    pub base_revision: u64,
    pub payload: UpdatePayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscriber_id: Option<String>,
}

/// Structured outcome returned by `update_state` and `StateSyncer::update_from_frontend`.
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum UpdateOutcome {
    Synced {
        #[specta(type = f64)]
        revision: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(type = Option<JsonValue>)]
        canonical: Option<serde_json::Value>,
    },
    Unchanged {
        #[specta(type = f64)]
        revision: u64,
    },
    Stale {
        snapshot: StateSnapshot,
    },
}

/// Structured error returned by sync commands and fallible `StateSyncer` methods.
#[derive(Serialize, Deserialize, Type, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "code", content = "message", rename_all = "camelCase")]
pub enum SyncError {
    UnknownKey(String),
    TypeMismatch(String),
    InvalidPatch(String),
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
            Self::InvalidPatch(msg) => write!(f, "invalid state patch: {msg}"),
            Self::InvalidValue(msg) => write!(f, "invalid state value: {msg}"),
            Self::SerializeFailed(msg) => write!(f, "failed to serialize state: {msg}"),
            Self::ChannelSendFailed(msg) => write!(f, "failed to send channel message: {msg}"),
            Self::PersistenceFailed(msg) => write!(f, "failed to persist state: {msg}"),
        }
    }
}

impl std::error::Error for SyncError {}
