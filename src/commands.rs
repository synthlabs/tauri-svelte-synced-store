use tauri::ipc::Channel;

use crate::{
    DeliveryMode, StateSnapshot, StateSyncer, StateUpdate, SyncError, SyncMessage, UpdateOutcome,
};

#[tauri::command]
#[specta::specta]
pub fn subscribe_state(
    name: String,
    channel: Channel<SyncMessage>,
    mode: Option<DeliveryMode>,
    webview: tauri::Webview<tauri::Wry>,
    state_syncer: tauri::State<'_, StateSyncer>,
) -> Result<String, SyncError> {
    state_syncer.subscribe(&name, &webview, channel, mode)
}

#[tauri::command]
#[specta::specta]
pub fn unsubscribe_state(
    name: String,
    subscriber_id: String,
    webview: tauri::Webview<tauri::Wry>,
    state_syncer: tauri::State<'_, StateSyncer>,
) -> Result<bool, SyncError> {
    state_syncer.unsubscribe(&name, webview.label(), &subscriber_id)
}

#[tauri::command]
#[specta::specta]
pub fn snapshot_state(
    name: String,
    subscriber_id: Option<String>,
    webview: tauri::Webview<tauri::Wry>,
    state_syncer: tauri::State<'_, StateSyncer>,
) -> Result<StateSnapshot, SyncError> {
    state_syncer.sync_snapshot(&name, Some(webview.label()), subscriber_id.as_deref())
}

#[tauri::command]
#[specta::specta]
pub fn update_state(
    state: StateUpdate,
    state_syncer: tauri::State<'_, StateSyncer>,
) -> Result<UpdateOutcome, SyncError> {
    state_syncer.update_from_frontend(state)
}
