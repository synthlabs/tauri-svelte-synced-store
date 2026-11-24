# tauri-svelte-synced-store

> NOTE: still in early development

This module keeps typed state synchronized between a Svelte 5 frontend and a Tauri v2 Rust backend over per-subscriber **Tauri Channels** using revisioned, bidirectional **RFC 6902 JSON Patch deltas** with automatic adaptive fallback to full snapshots.

On the Svelte side it exposes state as a deeply reactive `$state<T>` proxy updated **in-place** (so unchanged subtrees and sibling fields keep reference identity and don't trigger unrelated `$effect`/`$derived` readers), with non-blocking field-level 3-way merge and shared ref-counted store handles (`useSyncedState` / `acquireSyncedState`). On the Rust side it provides `StateSyncer` with monotonic revisions, RAII `Item<T>` guards that publish and persist on drop when serialized state changes, and automatic subscriber cleanup on reload/window teardown.

## Setup

### Rust

Register `tauri_svelte_synced_store::plugin` on your `tauri::Builder` (which manages `StateSyncer` and automatically clears subscribers on webview page reloads and window teardown), and expose the four sync commands via `state_handlers!()` or `tauri_svelte_synced_store::commands::*`:

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
} // dropping internal_state_ref publishes a delta (or snapshot if smaller) when state changed

fn main() {
    let specta_builder = Builder::<tauri::Wry>::new().commands(collect_commands![
        subscribe_state,
        unsubscribe_state,
        snapshot_state,
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

If you need a custom `update_state` handler, omit `state_handlers!()` and register `subscribe_state`, `unsubscribe_state`, and `snapshot_state` from `tauri_svelte_synced_store::commands::*` alongside your own `update_state` command.

To opt a specific key out of delta diffing (for example, tiny high-frequency telemetry), configure `delivery_modes` on `StateSyncerConfig`:

```rust
use std::collections::HashMap;
use tauri_svelte_synced_store::{DeliveryMode, StateSyncerConfig};

let cfg = StateSyncerConfig {
    delivery_modes: HashMap::from([("audio_metrics".into(), DeliveryMode::Snapshot)]),
    ..Default::default()
};
```

### TypeScript / Svelte 5

In Svelte components (or `$effect.root()`), prefer `useSyncedState<T>(name, initial?, options?)`. Multiple components using the same store `name` share one backend Tauri Channel subscription and release automatically on unmount:

```svelte
<script lang="ts">
	import { useSyncedState } from 'tauri-svelte-synced-store';
	import { commands, type InternalState } from '$lib/bindings';

	const internalState = useSyncedState<InternalState>('InternalState', {
		authenticated: false,
		name: ''
	});

	let greetMsg = $state('');

	async function login(event: Event) {
		event.preventDefault();
		const result = await internalState.sync();
		if (result.status === 'synced' || result.status === 'unchanged') {
			greetMsg = await commands.login(internalState.obj.name);
		}
	}
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

Outside Svelte component / `$effect.root()` lifecycles (e.g. module stores or imperative handlers), use `acquireSyncedState<T>(name, initial?, options?)` and call `await handle.release()` when done:

```ts
import { acquireSyncedState, closeAllSyncedStates } from 'tauri-svelte-synced-store';

const settings = acquireSyncedState<AppSettings>('app_settings', defaultSettings, {
	// Optional grace period to keep the channel open across quick route transitions (default: 0)
	releaseDelayMs: 250
});
await settings.initialized;
// ...
await settings.release();

// Or tear down all active shared handles explicitly (also runs automatically on `pagehide` / HMR)
await closeAllSyncedStates();
```

If you need an isolated, unshared subscription (for example, a modal draft with a different `mode` or `conflictPolicy`), instantiate `new SyncedState<T>(name, initial?, options?)` directly and call `await state.close()` on teardown.

## Sync Semantics

- **Fine-grained in-place Svelte 5 reactivity**: Incoming snapshots, deltas, `resync()`, canonical normalizations, and stale merges mutate existing Svelte 5 `$state` plain-object proxies in-place rather than replacing `obj` wholesale. Unchanged sibling subtrees and fields preserve object identity (`===`) and do not re-trigger unrelated `$effect`/`$derived` readers; array properties are replaced whole only when their contents change.
- **Shared ref-counted handles**: `useSyncedState` and `acquireSyncedState` share one `SyncedState<T>` and backend Tauri Channel per `name`. Acquiring an already-open `name` with a conflicting explicit `mode` or `conflictPolicy` throws immediately (omitted/`undefined` options inherit the open config). When the last consumer releases, the subscription closes immediately (or after `releaseDelayMs`), and any remaining handles are cleaned up automatically on `pagehide` or Vite HMR disposal.
- **Adaptive delta delivery (default)**: Every subscriber receives a revisioned `Snapshot` on subscribe and compact RFC 6902 `Delta` patches (`add` / `remove` / `replace`, with whole-array replacement) whenever the patch is smaller than a full snapshot. Rust proactively falls back to a `Snapshot` if a snapshot is smaller, if a silent mutation (`set` / `update(..., false)`) caused a subscriber's revision to diverge, or if the key/subscriber opted into `mode: 'snapshot'`.
- **No-op suppression**: Dropping an unmodified `Item<T>` or calling `update(..., true/false)` with an unchanged serialized value does not bump the revision, emit, or persist. Explicit `StateSyncer::emit()` always broadcasts a full `Snapshot`.
- **Non-blocking conflict policy**: `useSyncedState` / `acquireSyncedState` / `new SyncedState` default to `mode: 'delta'` and `conflictPolicy: 'backend-wins'` (`'backend-wins' | 'frontend-wins' | 'replace'`). Incoming backend updates preserve non-overlapping local edits and apply `conflictPolicy` on overlapping paths without blocking subsequent `sync()` calls.
- **Optimistic `sync()` results**: `sync()` sends a delta (or full value if smaller / in `'snapshot'` mode) tagged with `baseRevision` and returns a `SyncResult`:
  - `{ status: 'synced', revision }` — committed by Rust.
  - `{ status: 'unchanged', revision }` — no local diff (zero IPC) or no canonical change.
  - `{ status: 'stale', revision }` — backend revision had moved ahead; remote changes merged cleanly without overlap (no automatic retry — call `sync()` again if desired).
  - `{ status: 'conflict', revision, paths }` — backend revision had moved ahead with overlapping fields; `conflictPolicy` was applied and colliding JSON Pointer paths are reported.
  - Hard failures (`SyncError` such as `invalidValue`, `invalidPatch`, `unknownKey`, or closed state) reject the returned `Promise`.
- **Recovery**: Revision gaps or patch failures automatically invoke `snapshot_state` to resync; callers can also trigger a manual resync via `await state.resync()`.
