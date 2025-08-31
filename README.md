# tauri-svelte-synced-store

> NOTE: still in early early development

This module is designed to seamlessly keep state synced between the UI (Svelte) and the backend (Tauri). The store allows you to persist and sync string:object pairs. On the Svelte side it exposes the object as a fully typed `$state(T)` object, and on the Rust side it's an `Item<T>` that syncs its state when the item goes out of scope.

## Context

I mostly extracted this from the system I've been using in [Pepo](https://github.com/synthlabs/pepo) and [Scrybe](https://github.com/synthlabs/scrybe) to manage shared state between the rust core and the svelte UI. I wanted to move it to a shared place, to not only keep things consistent between my projects, but also so I can better refine features; as well as share it with other people that may have similar use cases.

Svelte already lends itself incredibly well to modeling the dynamic parts of the UI as a state machine. So when you combine this with a rust backend that can update that state, you end up with a really nice separation of concerns.

## Plans

Right now this is a very rough skeleton as I work through the ergonomics of using it, but some of the things I have in mind that I'd like to support

- No need to manually define the update and emit handlers
- Support partial updates
- Support syncing to disk
- Full type safety (no serialize/deserialize intermediate)

## Example

Here's a rough example of how it's used

### Rust

Here's how you use it in a Tauri command
```rust
#[tauri::command]
#[specta::specta]
fn login(name: String, state_syncer: tauri::State<'_, state::Syncer>) -> String {
    info!(name, "login");

    let internal_state_ref = state_syncer.get::<InternalState>("InternalState");
    let mut internal_state = internal_state_ref.lock().unwrap();

    internal_state.authenticated = true;

    format!("Hello, {}! You've been greeted from Rust!", name)
} // when internal_state_ref goes out of scope its state (and any changes you made) will be synced
```

### Typescript

```ts
<script lang="ts">
    import { commands } from "$lib/bindings.ts";
    import { SyncedState } from "$lib/state.svelte";
    import type { InternalState } from "$lib/bindings.ts";
    import { onDestroy } from "svelte";

    let internal_state = new SyncedState<InternalState>("InternalState", {
        authenticated: false,
        name: "",
    });

    let greetMsg = $state("");

    async function login(event: Event) {
        event.preventDefault();
        await internal_state.sync(); // trigger a sync to the backend
        greetMsg = await commands.login(internal_state.obj.name);
    }

    onDestroy(() => {
        internal_state.close();
    });
</script>

<main class="container">
    {#if internal_state.obj.authenticated}
        <h1>Welcome {internal_state.obj.name}</h1>
    {/if}

    <form class="row" onsubmit={login}>
        <input
            id="greet-input"
            placeholder="Enter a name..."
            bind:value={internal_state.obj.name}
        />
        <button type="submit">Greet</button>
    </form>
    <p>{greetMsg}</p>
</main>
```
