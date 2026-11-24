use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::Manager;
use tauri_specta::{Builder, collect_commands};
use tauri_svelte_synced_store::{StateSyncer, StateSyncerConfig, state_handlers};

#[derive(Serialize, Deserialize, Type, Clone, Debug, Default, PartialEq)]
struct AppState {
    count: u32,
}

state_handlers!();

fn main() {
    let macro_builder = Builder::<tauri::Wry>::new().commands(collect_commands![
        subscribe_state,
        unsubscribe_state,
        update_state,
    ]);

    let module_builder = Builder::<tauri::Wry>::new().commands(collect_commands![
        tauri_svelte_synced_store::commands::subscribe_state,
        tauri_svelte_synced_store::commands::unsubscribe_state,
        tauri_svelte_synced_store::commands::update_state,
    ]);

    let _ = (macro_builder, module_builder);

    let _app_builder = tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_svelte_synced_store::plugin(
            StateSyncerConfig::default(),
        ))
        .setup(|app| {
            let syncer = app.state::<StateSyncer>();
            syncer.set("app_state", AppState::default());
            Ok(())
        });
}
