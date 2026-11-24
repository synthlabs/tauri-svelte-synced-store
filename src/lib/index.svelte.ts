import { Channel, invoke } from '@tauri-apps/api/core';
import type { StateUpdatePayload, SyncMessage } from './protocol.js';

export type { StateUpdatePayload, SyncMessage } from './protocol.js';

export class SyncedState<T> {
	readonly name: string;
	obj: T = $state({} as T);
	ready: boolean = $state(false);
	initialized: Promise<void>;

	#channel: Channel<SyncMessage<T>> | undefined;
	#subscriberId: string | undefined;
	#closed = false;
	#initSettled = false;
	#resolveInit!: () => void;
	#rejectInit!: (reason?: unknown) => void;
	#subscribePromise: Promise<string>;
	#closePromise: Promise<void> | undefined;

	constructor(name: string, object?: T) {
		this.name = name;
		if (object !== undefined) {
			this.obj = object;
		}

		this.initialized = new Promise<void>((resolve, reject) => {
			this.#resolveInit = resolve;
			this.#rejectInit = reject;
		});

		const channel = new Channel<SyncMessage<T>>((message) => {
			this.#handleMessage(message);
		});
		this.#channel = channel;

		this.#subscribePromise = invoke<string>('subscribe_state', {
			name: this.name,
			channel
		});

		this.#subscribePromise.then(
			(subscriberId) => {
				if (!this.#closed) {
					this.#subscriberId = subscriberId;
				}
			},
			(err) => {
				if (this.#channel) {
					this.#channel.onmessage = () => {};
					this.#channel = undefined;
				}
				if (!this.#initSettled) {
					this.#initSettled = true;
					this.#rejectInit(err);
				}
			}
		);
	}

	#handleMessage(message: SyncMessage<T>): void {
		if (this.#closed) {
			return;
		}

		switch (message.kind) {
			case 'snapshot':
				this.obj = message.value;
				if (!this.ready) {
					this.ready = true;
				}
				if (!this.#initSettled) {
					this.#initSettled = true;
					this.#resolveInit();
				}
				break;
		}
	}

	close(): Promise<void> {
		if (this.#closePromise) {
			return this.#closePromise;
		}

		this.#closed = true;
		if (this.#channel) {
			this.#channel.onmessage = () => {};
		}
		if (!this.#initSettled) {
			this.#initSettled = true;
			this.#resolveInit();
		}

		this.#closePromise = (async () => {
			let subscriberId = this.#subscriberId;
			if (!subscriberId) {
				try {
					subscriberId = await this.#subscribePromise;
				} catch {
					return;
				}
			}

			this.#subscriberId = undefined;
			this.#channel = undefined;

			if (!subscriberId) {
				return;
			}

			try {
				await invoke<boolean>('unsubscribe_state', {
					name: this.name,
					subscriberId
				});
			} catch (err) {
				console.warn(`[SyncedStore]: failed to unsubscribe "${this.name}"`, err);
			}
		})();

		return this.#closePromise;
	}

	async sync(): Promise<boolean> {
		if (this.#closed) {
			throw new Error(`SyncedState "${this.name}" is closed`);
		}

		const value = $state.snapshot(this.obj) as T;
		const state: StateUpdatePayload<T> = {
			name: this.name,
			value,
			...(this.#subscriberId ? { subscriberId: this.#subscriberId } : {})
		};

		return invoke<boolean>('update_state', { state });
	}
}
