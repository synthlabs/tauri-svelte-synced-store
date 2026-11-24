# tauri-svelte-synced-store

> NOTE: still in early development

This module keeps typed state synchronized between a Svelte 5 frontend and a Tauri v2 Rust backend over per-subscriber **Tauri Channels**. On the Svelte side it exposes the object as a reactive `$state<T>` object via `SyncedState<T>`, and on the Rust side it provides `StateSyncer` with RAII `Item<T>` guards that publish and persist state when dropped.

## Setup

### Rust

Register the `tauri_svelte_synced_store::plugin` on your `tauri::Builder` (which manages `StateSyncer` and automatically clears subscribers on webview page reloads and window teardown), and expose the sync commands via `state_handlers!()` or `tauri_svelte_synced_store::commands::*`:

```rust
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::Manager;
use tauri_specta::{Builder, collect_commands};
use tauri_svelte_synced_store::{StateSyncer, StateSyncerConfig, state_handlers};

#[derive(Serialize, Deserialize, Type, Clone, Debug, Default)]
struct InternalState {
    authenticated: bool,
    name: String,
}

state_handlers!();

#[tauri::command]
#[specta::specta]
fn login(name: String, state_syncer: tauri::State<'_, StateSyncer>) -> String {
    let internal_state_ref = state_syncer.get::<InternalState>("InternalState");
    let mut internal_state = internal_state_ref.lock().unwrap();
    internal_state.authenticated = true;
    format!("Hello, {name}! You've been greeted from Rust!")
} // dropping internal_state_ref publishes a snapshot to all subscribed Channels

fn main() {
    let specta_builder = Builder::<tauri::Wry>::new().commands(collect_commands![
        subscribe_state,
        unsubscribe_state,
        update_state,
        login,
    ]);

    tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_svelte_synced_store::plugin(StateSyncerConfig::default()))
        .invoke_handler(specta_builder.invoke_handler())
        .setup(|app| {
            let syncer = app.state::<StateSyncer>();
            syncer.set("InternalState", InternalState::default());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

If you need a custom `update_state` handler, omit `state_handlers!()` and register `tauri_svelte_synced_store::commands::subscribe_state` and `tauri_svelte_synced_store::commands::unsubscribe_state` alongside your own `update_state` command.

### TypeScript / Svelte 5

```svelte
<script lang="ts">
	import { onDestroy } from 'svelte';
	import { SyncedState } from 'tauri-svelte-synced-store';
	import { commands, type InternalState } from '$lib/bindings';

	const internalState = new SyncedState<InternalState>('InternalState', {
		authenticated: false,
		name: ''
	});

	let greetMsg = $state('');

	async function login(event: Event) {
		event.preventDefault();
		await internalState.sync();
		greetMsg = await commands.login(internalState.obj.name);
	}

	onDestroy(() => {
		void internalState.close();
	});
</script>

<main class="container">
	{#if internalState.obj.authenticated}
		<h1>Welcome {internalState.obj.name}</h1>
	{/if}

	<form class="row" onsubmit={login}>
		<input placeholder="Enter a name..." bind:value={internalState.obj.name} />
		<button type="submit">Greet</button>
	</form>
	<p>{greetMsg}</p>
</main>
```
