import type {
	ConflictPolicy,
	DeliveryMode,
	SharedSyncedStateOptions,
	SyncResult,
	SyncedStateHandle
} from './protocol.js';
import { SyncedState } from './synced-state.svelte.js';

interface SharedEntry<T = unknown> {
	readonly name: string;
	readonly state: SyncedState<T>;
	readonly mode: DeliveryMode;
	readonly conflictPolicy: ConflictPolicy;
	refCount: number;
	releaseDelayMs: number;
	releaseTimer: ReturnType<typeof setTimeout> | undefined;
	closing: boolean;
	closePromise: Promise<void> | undefined;
}

// Intentionally non-reactive: registry lookups must not subscribe caller effects.
// eslint-disable-next-line svelte/prefer-svelte-reactivity
const entries = new Map<string, SharedEntry<unknown>>();
let pagehideAttached = false;

function handlePageHide(): void {
	void closeAllSyncedStates();
}

function attachPagehideListener(): void {
	if (
		pagehideAttached ||
		typeof window === 'undefined' ||
		typeof window.addEventListener !== 'function'
	) {
		return;
	}
	window.addEventListener('pagehide', handlePageHide);
	pagehideAttached = true;
}

function detachPagehideListenerIfEmpty(): void {
	if (
		!pagehideAttached ||
		entries.size > 0 ||
		typeof window === 'undefined' ||
		typeof window.removeEventListener !== 'function'
	) {
		return;
	}
	window.removeEventListener('pagehide', handlePageHide);
	pagehideAttached = false;
}

function closeEntry<T>(entry: SharedEntry<T>): Promise<void> {
	if (entry.releaseTimer !== undefined) {
		clearTimeout(entry.releaseTimer);
		entry.releaseTimer = undefined;
	}
	if (entries.get(entry.name) === entry) {
		entries.delete(entry.name);
	}
	detachPagehideListenerIfEmpty();
	entry.closing = true;
	if (!entry.closePromise) {
		entry.closePromise = entry.state.close();
	}
	return entry.closePromise;
}

function validateReleaseDelay(delay: number | undefined): number | undefined {
	if (delay === undefined) {
		return undefined;
	}
	if (typeof delay !== 'number' || !Number.isFinite(delay) || delay < 0) {
		throw new RangeError(`releaseDelayMs must be a finite non-negative number, got ${delay}`);
	}
	return delay;
}

class SyncedStateHandleImpl<T> implements SyncedStateHandle<T> {
	readonly #entry: SharedEntry<T>;
	#released = false;
	#releasePromise: Promise<void> | undefined;

	constructor(entry: SharedEntry<T>) {
		this.#entry = entry;
	}

	get name(): string {
		return this.#entry.name;
	}

	get mode(): DeliveryMode {
		return this.#entry.mode;
	}

	get conflictPolicy(): ConflictPolicy {
		return this.#entry.conflictPolicy;
	}

	get obj(): T {
		return this.#entry.state.obj;
	}

	set obj(value: T) {
		if (this.released) {
			throw new Error(`SyncedStateHandle "${this.#entry.name}" is released`);
		}
		this.#entry.state.obj = value;
	}

	get ready(): boolean {
		return this.#entry.state.ready;
	}

	get revision(): number | undefined {
		return this.#entry.state.revision;
	}

	get initialized(): Promise<void> {
		return this.#entry.state.initialized;
	}

	get released(): boolean {
		return this.#released || this.#entry.closing;
	}

	sync(): Promise<SyncResult> {
		if (this.released) {
			return Promise.reject(new Error(`SyncedStateHandle "${this.#entry.name}" is released`));
		}
		return this.#entry.state.sync();
	}

	resync(): Promise<void> {
		if (this.released) {
			return Promise.reject(new Error(`SyncedStateHandle "${this.#entry.name}" is released`));
		}
		return this.#entry.state.resync();
	}

	release(): Promise<void> {
		if (this.#releasePromise) {
			return this.#releasePromise;
		}

		this.#released = true;
		const entry = this.#entry;
		entry.refCount = Math.max(0, entry.refCount - 1);

		if (entry.closing) {
			this.#releasePromise = entry.closePromise ?? Promise.resolve();
			return this.#releasePromise;
		}

		if (entry.refCount > 0) {
			this.#releasePromise = Promise.resolve();
			return this.#releasePromise;
		}

		if (entry.releaseDelayMs === 0) {
			this.#releasePromise = closeEntry(entry);
			return this.#releasePromise;
		}

		if (entry.releaseTimer === undefined) {
			entry.releaseTimer = setTimeout(() => {
				entry.releaseTimer = undefined;
				if (entry.refCount === 0 && !entry.closing) {
					void closeEntry(entry);
				}
			}, entry.releaseDelayMs);
		}

		this.#releasePromise = Promise.resolve();
		return this.#releasePromise;
	}
}

/**
 * Acquires a ref-counted handle to a shared `SyncedState<T>` keyed by `name`.
 * Call `handle.release()` when finished; when the last consumer releases, the underlying
 * backend Channel subscription closes (immediately by default, or after `releaseDelayMs`).
 */
export function acquireSyncedState<T>(
	name: string,
	object?: T,
	options?: SharedSyncedStateOptions
): SyncedStateHandle<T> {
	const releaseDelayMs = validateReleaseDelay(options?.releaseDelayMs);
	const existing = entries.get(name);

	if (existing) {
		if (options?.mode !== undefined && options.mode !== existing.mode) {
			throw new Error(
				`SyncedState "${name}" is already open with mode "${existing.mode}", cannot acquire with mode "${options.mode}"`
			);
		}
		if (
			options?.conflictPolicy !== undefined &&
			options.conflictPolicy !== existing.conflictPolicy
		) {
			throw new Error(
				`SyncedState "${name}" is already open with conflictPolicy "${existing.conflictPolicy}", cannot acquire with conflictPolicy "${options.conflictPolicy}"`
			);
		}
		if (existing.releaseTimer !== undefined) {
			clearTimeout(existing.releaseTimer);
			existing.releaseTimer = undefined;
		}
		if (releaseDelayMs !== undefined) {
			existing.releaseDelayMs = Math.max(existing.releaseDelayMs, releaseDelayMs);
		}
		existing.refCount += 1;
		return new SyncedStateHandleImpl<T>(existing as SharedEntry<T>);
	}

	attachPagehideListener();

	const state = new SyncedState<T>(name, object, {
		mode: options?.mode,
		conflictPolicy: options?.conflictPolicy
	});

	const entry: SharedEntry<T> = {
		name,
		state,
		mode: state.mode,
		conflictPolicy: state.conflictPolicy,
		refCount: 1,
		releaseDelayMs: releaseDelayMs ?? 0,
		releaseTimer: undefined,
		closing: false,
		closePromise: undefined
	};

	entries.set(name, entry as SharedEntry<unknown>);

	// Evict failed subscriptions automatically so future callers can retry cleanly
	// without swallowing the rejection for holders awaiting `handle.initialized`.
	state.initialized.then(undefined, () => {
		if (entries.get(name) === entry) {
			void closeEntry(entry);
		}
	});

	return new SyncedStateHandleImpl<T>(entry);
}

/**
 * Acquires a shared `SyncedStateHandle<T>` tied to the active Svelte component or `$effect.root()`
 * lifecycle, releasing it automatically on teardown.
 */
export function useSyncedState<T>(
	name: string,
	object?: T,
	options?: SharedSyncedStateOptions
): SyncedStateHandle<T> {
	const ref: { handle?: SyncedStateHandle<T> } = {};
	try {
		$effect.pre(() => {
			return () => {
				if (ref.handle) {
					void ref.handle.release();
				}
			};
		});
	} catch (err) {
		throw new Error(
			`useSyncedState("${name}") must be called inside a Svelte component or $effect.root(); use acquireSyncedState() for manual lifecycle management.`,
			{ cause: err }
		);
	}

	const handle = acquireSyncedState(name, object, options);
	ref.handle = handle;
	return handle;
}

/**
 * Immediately closes and unsubscribes all active (and grace-period) shared store handles.
 */
export async function closeAllSyncedStates(): Promise<void> {
	const active = Array.from(entries.values());
	entries.clear();
	detachPagehideListenerIfEmpty();

	const promises = active.map((entry) => closeEntry(entry));
	await Promise.all(promises);
}

const hot = (import.meta as { hot?: { dispose(cb: () => void): void } }).hot;
if (hot) {
	hot.dispose(() => {
		void closeAllSyncedStates();
	});
}
