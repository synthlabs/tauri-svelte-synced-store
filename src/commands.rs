use tauri::ipc::Channel;

use crate::{StateSyncer, StateUpdate, SyncError, SyncMessage};

#[tauri::command]
#[specta::specta]
pub fn subscribe_state(
    name: String,
    channel: Channel<SyncMessage>,
    webview: tauri::Webview<tauri::Wry>,
    state_syncer: tauri::State<'_, StateSyncer>,
) -> Result<String, SyncError> {
    state_syncer.subscribe(&name, &webview, channel)
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
pub fn update_state(
    state: StateUpdate,
    state_syncer: tauri::State<'_, StateSyncer>,
) -> Result<bool, SyncError> {
    state_syncer.update_from_frontend(state)?;
    Ok(true)
}
