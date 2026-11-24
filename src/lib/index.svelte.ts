import { Channel, invoke } from '@tauri-apps/api/core';
import { untrack } from 'svelte';
import {
	applyPatchStructural,
	cloneJson,
	diffJson,
	exceedsJsonSize,
	mergePatchIntoLocal,
	mergeSnapshotIntoLocal
} from './patch.js';
import type {
	ConflictPolicy,
	DeliveryMode,
	StateSnapshot,
	StateUpdatePayload,
	SyncMessage,
	SyncResult,
	SyncedStateOptions,
	UpdateOutcome,
	UpdatePayload
} from './protocol.js';

export type {
	ConflictPolicy,
	DeliveryMode,
	JsonPatchOp,
	StateSnapshot,
	StateUpdatePayload,
	SyncMessage,
	SyncResult,
	SyncedStateOptions,
	UpdateOutcome,
	UpdatePayload
} from './protocol.js';

export class SyncedState<T> {
	readonly name: string;
	readonly mode: DeliveryMode;
	readonly conflictPolicy: ConflictPolicy;

	obj: T = $state({} as T);
	ready: boolean = $state(false);
	revision: number | undefined = $state(undefined);
	initialized: Promise<void>;

	#channel: Channel<SyncMessage<T>> | undefined;
	#subscriberId: string | undefined;
	#baseline: unknown = undefined;
	#revision: number | undefined = undefined;
	#closed = false;
	#initSettled = false;
	#writing = false;
	#recovering = false;
	#needsResync = false;
	#pendingMessages: SyncMessage<T>[] = [];
	#opTail: Promise<void> = Promise.resolve();
	#resolveInit!: () => void;
	#rejectInit!: (reason?: unknown) => void;
	#subscribePromise: Promise<string>;
	#closePromise: Promise<void> | undefined;

	constructor(name: string, object?: T, options?: SyncedStateOptions) {
		this.name = name;
		this.mode = options?.mode ?? 'delta';
		this.conflictPolicy = options?.conflictPolicy ?? 'backend-wins';

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
			channel,
			mode: this.mode
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

	#enqueue<R>(task: () => Promise<R>): Promise<R> {
		const run = this.#opTail.then(task, task);
		this.#opTail = run.then(
			() => {},
			() => {}
		);
		return run;
	}

	#handleMessage(message: SyncMessage<T>): void {
		if (this.#closed) {
			return;
		}
		if (this.#writing || this.#recovering) {
			this.#pendingMessages.push(message);
			return;
		}
		this.#processMessage(message);
	}

	#processMessage(message: SyncMessage<T>): void {
		if (this.#closed) {
			return;
		}

		if (message.kind === 'snapshot') {
			this.#applyIncomingSnapshot({
				revision: message.revision,
				value: message.value
			});
			return;
		}

		if (this.#revision === undefined) {
			this.#triggerRecovery();
			return;
		}
		if (message.revision <= this.#revision) {
			return;
		}
		if (message.baseRevision !== this.#revision || this.#needsResync) {
			this.#triggerRecovery();
			return;
		}

		let newBase: T;
		try {
			newBase = applyPatchStructural(this.#baseline as T, message.patch);
		} catch (err) {
			console.warn(`[SyncedStore]: failed to apply delta patch for "${this.name}", resyncing`, err);
			this.#triggerRecovery();
			return;
		}

		const local = untrack(() => this.obj);
		const merged = mergePatchIntoLocal(
			this.#baseline,
			local,
			newBase,
			message.patch,
			this.conflictPolicy
		);

		this.#baseline = newBase;
		this.#revision = message.revision;
		this.revision = message.revision;
		if (merged.value !== local) {
			this.obj = merged.value;
		}
	}

	#applyIncomingSnapshot(snapshot: StateSnapshot<T>): void {
		if (this.#revision !== undefined && snapshot.revision <= this.#revision) {
			return;
		}

		const nextBase = cloneJson(snapshot.value);
		if (this.#revision === undefined) {
			this.#baseline = nextBase;
			this.#revision = snapshot.revision;
			this.revision = snapshot.revision;
			this.obj = cloneJson(snapshot.value);
			this.#needsResync = false;
			if (!this.ready) {
				this.ready = true;
			}
			if (!this.#initSettled) {
				this.#initSettled = true;
				this.#resolveInit();
			}
			return;
		}

		const local = untrack(() => this.obj);
		const merged = mergeSnapshotIntoLocal(
			this.#baseline,
			local,
			snapshot.value,
			this.conflictPolicy
		);

		this.#baseline = nextBase;
		this.#revision = snapshot.revision;
		this.revision = snapshot.revision;
		this.#needsResync = false;
		this.obj = merged.value;
	}

	#flushPending(): void {
		while (
			!this.#closed &&
			!this.#writing &&
			!this.#recovering &&
			this.#pendingMessages.length > 0
		) {
			const next = this.#pendingMessages.shift()!;
			this.#processMessage(next);
		}
	}

	#triggerRecovery(): void {
		if (this.#closed || this.#recovering) {
			return;
		}

		this.#recovering = true;
		this.#needsResync = true;

		void this.#enqueue(async () => {
			try {
				if (this.#closed || !this.#needsResync) {
					return;
				}
				await this.#fetchAndApplySnapshot(false);
			} catch (err) {
				console.warn(`[SyncedStore]: failed to recover snapshot for "${this.name}"`, err);
			} finally {
				this.#recovering = false;
				if (!this.#needsResync) {
					this.#flushPending();
				} else {
					this.#pendingMessages.length = 0;
				}
			}
		});
	}

	async #fetchAndApplySnapshot(manageRecoverFlag: boolean): Promise<void> {
		if (this.#closed) {
			throw new Error(`SyncedState "${this.name}" is closed`);
		}

		if (manageRecoverFlag) {
			this.#recovering = true;
		}

		try {
			const snapshot = await invoke<StateSnapshot<T>>('snapshot_state', {
				name: this.name,
				...(this.#subscriberId ? { subscriberId: this.#subscriberId } : {})
			});
			if (this.#closed) {
				return;
			}
			this.#applyIncomingSnapshot(snapshot);
			this.#needsResync = false;
		} finally {
			if (manageRecoverFlag) {
				this.#recovering = false;
				this.#flushPending();
			}
		}
	}

	resync(): Promise<void> {
		if (this.#closed) {
			return Promise.reject(new Error(`SyncedState "${this.name}" is closed`));
		}

		return this.#enqueue(async () => {
			if (!this.ready) {
				await this.initialized;
			}
			if (this.#closed) {
				throw new Error(`SyncedState "${this.name}" is closed`);
			}
			await this.#fetchAndApplySnapshot(true);
		});
	}

	close(): Promise<void> {
		if (this.#closePromise) {
			return this.#closePromise;
		}

		this.#closed = true;
		this.#pendingMessages.length = 0;
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

	sync(): Promise<SyncResult> {
		if (this.#closed) {
			return Promise.reject(new Error(`SyncedState "${this.name}" is closed`));
		}

		return this.#enqueue(async () => {
			if (!this.ready) {
				await this.initialized;
			}
			if (this.#closed) {
				throw new Error(`SyncedState "${this.name}" is closed`);
			}

			if (this.#needsResync) {
				await this.#fetchAndApplySnapshot(true);
			}

			const baseRevision = this.#revision;
			if (baseRevision === undefined) {
				throw new Error(`SyncedState "${this.name}" is not initialized`);
			}

			const submitted = untrack(() => cloneJson(this.obj));
			const patch = diffJson(this.#baseline, submitted);
			if (patch.length === 0) {
				return { status: 'unchanged', revision: baseRevision };
			}

			let payload: UpdatePayload<T>;
			if (this.mode === 'snapshot') {
				payload = { kind: 'full', value: submitted };
			} else {
				const patchSize = JSON.stringify(patch).length;
				payload = exceedsJsonSize(submitted, patchSize)
					? { kind: 'delta', patch }
					: { kind: 'full', value: submitted };
			}

			const state: StateUpdatePayload<T> = {
				name: this.name,
				baseRevision,
				payload,
				...(this.#subscriberId ? { subscriberId: this.#subscriberId } : {})
			};

			this.#writing = true;
			try {
				const outcome = await invoke<UpdateOutcome<T>>('update_state', { state });
				if (this.#closed) {
					throw new Error(`SyncedState "${this.name}" is closed`);
				}

				if (outcome.status === 'synced') {
					if (outcome.canonical !== undefined && outcome.canonical !== null) {
						const local = untrack(() => this.obj);
						const merged = mergeSnapshotIntoLocal(
							submitted,
							local,
							outcome.canonical,
							this.conflictPolicy
						);
						this.#baseline = cloneJson(outcome.canonical);
						this.#revision = outcome.revision;
						this.revision = outcome.revision;
						this.obj = merged.value;
					} else {
						this.#baseline = submitted;
						this.#revision = outcome.revision;
						this.revision = outcome.revision;
					}
					return { status: 'synced', revision: outcome.revision };
				}

				if (outcome.status === 'unchanged') {
					this.#revision = outcome.revision;
					this.revision = outcome.revision;
					return { status: 'unchanged', revision: outcome.revision };
				}

				const local = untrack(() => this.obj);
				const merged = mergeSnapshotIntoLocal(
					this.#baseline,
					local,
					outcome.snapshot.value,
					this.conflictPolicy
				);
				this.#baseline = cloneJson(outcome.snapshot.value);
				this.#revision = outcome.snapshot.revision;
				this.revision = outcome.snapshot.revision;
				this.obj = merged.value;

				if (merged.conflicts.length > 0) {
					return {
						status: 'conflict',
						revision: outcome.snapshot.revision,
						paths: merged.conflicts
					};
				}
				return { status: 'stale', revision: outcome.snapshot.revision };
			} finally {
				this.#writing = false;
				this.#flushPending();
			}
		});
	}
}
