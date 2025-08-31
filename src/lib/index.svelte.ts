// Reexport your entry components here
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';

export class SyncedState<T> {
	name: string;
	obj: T = $state({} as T);
	#un_sub: UnlistenFn | undefined;

	constructor(name: string, object: T) {
		this.name = name;
		this.obj = object;

		// TODO: update this to the type safe event system
		listen<T>(`${this.name}_update`, (event) => {
			console.log(`DEBUG [SyncedStore]: ${this.name}_update event`, event.payload);
			this.obj = event.payload;
		}).then((f) => {
			this.#un_sub = f;
			invoke('emit_state', { name: this.name });
		});
	}

	close() {
		if (this.#un_sub) {
			this.#un_sub();
		}
	}

	async sync(): Promise<boolean> {
		const val = $state.snapshot(this.obj);
		console.log(`DEBUG [SyncedStore]: ${this.name} - syncing`, val);
		return invoke('update_state', {
			state: { version: null, name: this.name, value: JSON.stringify(val) }
		});
	}
}
