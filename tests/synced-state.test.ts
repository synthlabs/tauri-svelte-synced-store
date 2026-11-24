import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
	applyPatchStructural,
	diffJson,
	mergePatchIntoLocal,
	mergeSnapshotIntoLocal
} from '../src/lib/patch.js';
import type {
	StateSnapshot,
	StateUpdatePayload,
	SyncMessage,
	UpdateOutcome
} from '../src/lib/protocol.js';

type InvokeCall = {
	cmd: string;
	args?: Record<string, unknown>;
};

class MockChannel<T = unknown> {
	static nextId = 1;
	id: number;
	onmessage: (response: T) => void;

	constructor(onmessage?: (response: T) => void) {
		this.id = MockChannel.nextId++;
		this.onmessage = onmessage ?? (() => {});
	}

	emit(message: T): void {
		this.onmessage(message);
	}
}

const invokeMock = vi.fn<(cmd: string, args?: Record<string, unknown>) => Promise<unknown>>();

vi.mock('@tauri-apps/api/core', () => ({
	Channel: MockChannel,
	invoke: (cmd: string, args?: Record<string, unknown>) => invokeMock(cmd, args)
}));

const { SyncedState } = await import('../src/lib/index.svelte.js');

interface CounterState {
	count: number;
	label: string;
}

interface RichState {
	count: number;
	label: string;
	padding: string;
	nested: {
		enabled: boolean;
		tag: string;
		extra?: string;
	};
	items: number[];
}

function deferred<T>() {
	let resolve!: (value: T) => void;
	let reject!: (reason?: unknown) => void;
	const promise = new Promise<T>((res, rej) => {
		resolve = res;
		reject = rej;
	});
	return { promise, resolve, reject };
}

describe('patch & merge utilities', () => {
	it('mergePatchIntoLocal matches full-tree mergeSnapshotIntoLocal across nested and ancestor conflicts', () => {
		const scenarios: Array<{
			name: string;
			oldBase: unknown;
			local: unknown;
			remote: unknown;
		}> = [
			{
				name: 'disjoint leaf edits + add + remove',
				oldBase: { a: 1, b: 2, nested: { x: 1, y: 2, drop: 'old' } },
				local: { a: 10, b: 2, nested: { x: 1, y: 20, drop: 'old' } },
				remote: { a: 1, b: 200, nested: { x: 100, y: 2, added: true } }
			},
			{
				name: 'overlapping leaf edits + array replace',
				oldBase: { a: 1, arr: [1, 2], nested: { x: 'base', y: 'same' } },
				local: { a: 2, arr: [1, 3], nested: { x: 'local', y: 'same' } },
				remote: { a: 3, arr: [4, 5], nested: { x: 'remote', y: 'remote-y' } }
			},
			{
				name: 'local replaces nested object with primitive while remote edits multiple children',
				oldBase: { nested: { x: 1, y: 2 }, keep: 'ok' },
				local: { nested: null as unknown, keep: 'local-ok' },
				remote: { nested: { x: 10, y: 20 }, keep: 'ok' }
			},
			{
				name: 'local deletes nested object while remote edits inside it',
				oldBase: { nested: { x: 1, y: 2 }, keep: 1 },
				local: { keep: 2 },
				remote: { nested: { x: 9, y: 2 }, keep: 1 }
			},
			{
				name: 'escaped keys (~ and / and empty key)',
				oldBase: { '': 0, 'a/b': { 'c~d': 1, stay: 2 } },
				local: { '': 1, 'a/b': { 'c~d': 1, stay: 9 } },
				remote: { '': 2, 'a/b': { 'c~d': 5, stay: 2 } }
			}
		];

		for (const scenario of scenarios) {
			const patch = diffJson(scenario.oldBase, scenario.remote);
			const newBase = applyPatchStructural(scenario.oldBase, patch);
			expect(newBase).toEqual(scenario.remote);

			for (const policy of ['backend-wins', 'frontend-wins'] as const) {
				const fromPatch = mergePatchIntoLocal(
					scenario.oldBase,
					scenario.local,
					newBase,
					patch,
					policy
				);
				const fromSnapshot = mergeSnapshotIntoLocal(
					scenario.oldBase,
					scenario.local,
					scenario.remote,
					policy
				);
				expect(fromPatch, `${scenario.name} (${policy})`).toEqual(fromSnapshot);
			}
		}
	});
});

describe('SyncedState (Tauri Channel delta transport)', () => {
	beforeEach(() => {
		invokeMock.mockReset();
		MockChannel.nextId = 1;
	});

	it('subscribes over a Channel, applies initial snapshot and deltas in order, and tracks revision', async () => {
		let capturedChannel: MockChannel<SyncMessage<CounterState>> | undefined;
		const subscribeGate = deferred<string>();
		const subscribeArgs: Array<Record<string, unknown> | undefined> = [];

		invokeMock.mockImplementation((cmd, args) => {
			if (cmd === 'subscribe_state') {
				subscribeArgs.push(args);
				capturedChannel = args?.channel as MockChannel<SyncMessage<CounterState>>;
				return subscribeGate.promise;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<CounterState>('counter', { count: 0, label: 'local' });
		expect(state.ready).toBe(false);
		expect(state.revision).toBeUndefined();
		expect(state.mode).toBe('delta');
		expect(state.conflictPolicy).toBe('backend-wins');
		expect(subscribeArgs[0]?.mode).toBe('delta');

		capturedChannel!.emit({
			kind: 'snapshot',
			revision: 0,
			value: { count: 1, label: 'server' }
		});
		subscribeGate.resolve('sub-1');

		await state.initialized;
		expect(state.ready).toBe(true);
		expect(state.revision).toBe(0);
		expect(state.obj).toEqual({ count: 1, label: 'server' });

		capturedChannel!.emit({
			kind: 'delta',
			baseRevision: 0,
			revision: 1,
			patch: [{ op: 'replace', path: '/count', value: 2 }]
		});
		expect(state.revision).toBe(1);
		expect(state.obj).toEqual({ count: 2, label: 'server' });

		// Duplicate/stale revision frame is ignored
		capturedChannel!.emit({
			kind: 'delta',
			baseRevision: 0,
			revision: 1,
			patch: [{ op: 'replace', path: '/count', value: 999 }]
		});
		expect(state.obj.count).toBe(2);
	});

	it('ignores explicit undefined options and keeps defaults', async () => {
		const subscribeArgs: Array<Record<string, unknown> | undefined> = [];
		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				subscribeArgs.push(args);
				const ch = args?.channel as MockChannel<SyncMessage<CounterState>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { count: 0, label: '' } });
				return 'sub-opt';
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<CounterState>('counter', undefined, {
			mode: undefined,
			conflictPolicy: undefined
		});
		await state.initialized;
		expect(state.mode).toBe('delta');
		expect(state.conflictPolicy).toBe('backend-wins');
		expect(subscribeArgs[0]?.mode).toBe('delta');
	});

	it('sync() sends delta when smaller, full when tiny or in snapshot mode, skips unchanged, and rejects on error/close', async () => {
		const calls: InvokeCall[] = [];
		const bigPadding = 'x'.repeat(512);

		invokeMock.mockImplementation(async (cmd, args) => {
			calls.push({ cmd, args });
			if (cmd === 'subscribe_state') {
				const ch = args?.channel as MockChannel<SyncMessage<RichState>>;
				ch.emit({
					kind: 'snapshot',
					revision: 0,
					value: {
						count: 1,
						label: 'init',
						padding: bigPadding,
						nested: { enabled: false, tag: 'v1' },
						items: [1, 2]
					}
				});
				return 'sub-42';
			}
			if (cmd === 'update_state') {
				const update = args?.state as StateUpdatePayload<RichState>;
				if (
					update.payload.kind === 'delta' &&
					update.payload.patch.some(
						(op) => op.path === '/count' && op.op === 'replace' && op.value === -1
					)
				) {
					throw { code: 'invalidValue', message: 'negative count' };
				}
				return {
					status: 'synced',
					revision: update.baseRevision + 1
				} satisfies UpdateOutcome<RichState>;
			}
			if (cmd === 'unsubscribe_state') {
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<RichState>('rich');
		await state.initialized;

		// No local edit -> unchanged with zero IPC
		const noop = await state.sync();
		expect(noop).toEqual({ status: 'unchanged', revision: 0 });
		expect(calls.filter((c) => c.cmd === 'update_state')).toHaveLength(0);

		// Single field edit on large state -> sends compact Delta payload
		state.obj.count = 10;
		const synced = await state.sync();
		expect(synced).toEqual({ status: 'synced', revision: 1 });
		expect(state.revision).toBe(1);

		const updateCall = calls.find((c) => c.cmd === 'update_state');
		expect(updateCall?.args).toEqual({
			state: {
				name: 'rich',
				baseRevision: 0,
				payload: {
					kind: 'delta',
					patch: [{ op: 'replace', path: '/count', value: 10 }]
				},
				subscriberId: 'sub-42'
			}
		});

		// Backend validation error rejects
		state.obj.count = -1;
		await expect(state.sync()).rejects.toEqual({
			code: 'invalidValue',
			message: 'negative count'
		});

		// Snapshot mode always sends Full payload even for large state
		const snapState = new SyncedState<RichState>('rich', undefined, { mode: 'snapshot' });
		await snapState.initialized;
		snapState.obj.count = 77;
		const snapRes = await snapState.sync();
		expect(snapRes).toEqual({ status: 'synced', revision: 1 });
		const snapUpdate = calls.filter((c) => c.cmd === 'update_state').at(-1);
		expect((snapUpdate?.args?.state as StateUpdatePayload<RichState>).payload.kind).toBe('full');

		await state.close();
		await expect(state.sync()).rejects.toThrow('SyncedState "rich" is closed');
	});

	it('falls back to full payload on sync() when state is smaller than the patch', async () => {
		const updatePayloads: StateUpdatePayload<CounterState>[] = [];

		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				const ch = args?.channel as MockChannel<SyncMessage<CounterState>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { count: 1, label: 'a' } });
				return 'sub-tiny';
			}
			if (cmd === 'update_state') {
				const payload = args?.state as StateUpdatePayload<CounterState>;
				updatePayloads.push(payload);
				return { status: 'synced', revision: 1 } satisfies UpdateOutcome<CounterState>;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<CounterState>('tiny');
		await state.initialized;
		state.obj.count = 2;
		await state.sync();

		expect(updatePayloads[0]?.payload).toEqual({
			kind: 'full',
			value: { count: 2, label: 'a' }
		});
	});

	it('applies non-blocking 3-way merge on incoming deltas for backend-wins, frontend-wins, and replace policies', async () => {
		const runPolicy = async (policy: 'backend-wins' | 'frontend-wins' | 'replace') => {
			let ch!: MockChannel<SyncMessage<{ a: number; b: number; c: number }>>;
			invokeMock.mockImplementation(async (cmd, args) => {
				if (cmd === 'subscribe_state') {
					ch = args?.channel as MockChannel<SyncMessage<{ a: number; b: number; c: number }>>;
					ch.emit({ kind: 'snapshot', revision: 0, value: { a: 0, b: 0, c: 0 } });
					return `sub-${policy}`;
				}
				throw new Error(`unexpected command: ${cmd}`);
			});

			const state = new SyncedState<{ a: number; b: number; c: number }>('p', undefined, {
				conflictPolicy: policy
			});
			await state.initialized;

			// Local edits /a (will conflict) and /b (disjoint)
			state.obj.a = 1;
			state.obj.b = 10;

			// Remote edits /a (conflict) and /c (disjoint)
			ch.emit({
				kind: 'delta',
				baseRevision: 0,
				revision: 1,
				patch: [
					{ op: 'replace', path: '/a', value: 2 },
					{ op: 'replace', path: '/c', value: 30 }
				]
			});
			return state.obj;
		};

		expect(await runPolicy('backend-wins')).toEqual({ a: 2, b: 10, c: 30 });
		expect(await runPolicy('frontend-wins')).toEqual({ a: 1, b: 10, c: 30 });
		expect(await runPolicy('replace')).toEqual({ a: 2, b: 0, c: 30 });
	});

	it('stale sync() never auto-retries, merges snapshot per policy, returns stale or conflict, and never blocks follow-up sync()', async () => {
		let updateCalls = 0;

		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				const ch = args?.channel as MockChannel<SyncMessage<{ a: number; b: number; c: number }>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { a: 0, b: 0, c: 0 } });
				return 'sub-stale';
			}
			if (cmd === 'update_state') {
				updateCalls++;
				const update = args?.state as StateUpdatePayload<{ a: number; b: number; c: number }>;
				if (update.baseRevision === 0) {
					// Disjoint remote change on /b
					return {
						status: 'stale',
						snapshot: { revision: 1, value: { a: 0, b: 5, c: 0 } }
					} satisfies UpdateOutcome<{ a: number; b: number; c: number }>;
				}
				if (update.baseRevision === 1) {
					return { status: 'synced', revision: 2 } satisfies UpdateOutcome<{
						a: number;
						b: number;
						c: number;
					}>;
				}
				if (update.baseRevision === 2) {
					// Overlapping remote change on /c
					return {
						status: 'stale',
						snapshot: { revision: 3, value: { a: 1, b: 5, c: 99 } }
					} satisfies UpdateOutcome<{ a: number; b: number; c: number }>;
				}
				return { status: 'synced', revision: update.baseRevision + 1 } satisfies UpdateOutcome<{
					a: number;
					b: number;
					c: number;
				}>;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<{ a: number; b: number; c: number }>('s');
		await state.initialized;

		// 1. Disjoint stale write -> preserves local /a, merges /b, returns stale with 1 IPC call (no auto-retry)
		state.obj.a = 1;
		const staleRes = await state.sync();
		expect(staleRes).toEqual({ status: 'stale', revision: 1 });
		expect(updateCalls).toBe(1);
		expect(state.obj).toEqual({ a: 1, b: 5, c: 0 });
		expect(state.revision).toBe(1);

		// 2. Caller explicitly retries -> succeeds cleanly
		const retryRes = await state.sync();
		expect(retryRes).toEqual({ status: 'synced', revision: 2 });
		expect(updateCalls).toBe(2);

		// 3. Overlapping stale write -> applies backend-wins, returns conflict with paths, does not block next sync()
		state.obj.c = 7;
		const conflictRes = await state.sync();
		expect(conflictRes).toEqual({ status: 'conflict', revision: 3, paths: ['/c'] });
		expect(state.obj).toEqual({ a: 1, b: 5, c: 99 });

		// 4. Subsequent edit and sync() are never blocked
		state.obj.a = 42;
		const afterConflict = await state.sync();
		expect(afterConflict).toEqual({ status: 'synced', revision: 4 });
	});

	it('replace policy on stale sync() reports conflict on overlap and stale on disjoint edit (never unchanged)', async () => {
		let remoteSnap: StateSnapshot<{ a: number; b: number }> = {
			revision: 1,
			value: { a: 0, b: 5 }
		};

		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				const ch = args?.channel as MockChannel<SyncMessage<{ a: number; b: number }>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { a: 0, b: 0 } });
				return 'sub-rep';
			}
			if (cmd === 'update_state') {
				return { status: 'stale', snapshot: remoteSnap } satisfies UpdateOutcome<{
					a: number;
					b: number;
				}>;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<{ a: number; b: number }>('rep', undefined, {
			conflictPolicy: 'replace'
		});
		await state.initialized;

		// Disjoint stale write under replace -> replaces obj, reports 'stale'
		state.obj.a = 1;
		expect(await state.sync()).toEqual({ status: 'stale', revision: 1 });
		expect(state.obj).toEqual({ a: 0, b: 5 });

		// Overlapping stale write under replace -> replaces obj, reports 'conflict' with paths
		remoteSnap = { revision: 2, value: { a: 9, b: 5 } };
		state.obj.a = 2;
		expect(await state.sync()).toEqual({ status: 'conflict', revision: 2, paths: ['/a'] });
		expect(state.obj).toEqual({ a: 9, b: 5 });
	});

	it('buffers channel deltas arriving while sync() is in flight and preserves in-flight local edits (including canonical normalization)', async () => {
		let ch!: MockChannel<SyncMessage<{ a: number; b: number; c: number }>>;
		const updateStarted = deferred<void>();
		const updateGate = deferred<UpdateOutcome<{ a: number; b: number; c: number }>>();

		invokeMock.mockImplementation((cmd, args) => {
			if (cmd === 'subscribe_state') {
				ch = args?.channel as MockChannel<SyncMessage<{ a: number; b: number; c: number }>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { a: 0, b: 0, c: 0 } });
				return Promise.resolve('sub-buf');
			}
			if (cmd === 'update_state') {
				updateStarted.resolve();
				return updateGate.promise;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<{ a: number; b: number; c: number }>('buf');
		await state.initialized;

		state.obj.a = 10;
		const syncPromise = state.sync();

		// Wait until update_state is actually in flight
		await updateStarted.promise;

		// User edits /b while sync() is in flight
		state.obj.b = 20;

		// Backend commits rev 1 (our write, normalized /a -> 11) and immediately broadcasts rev 1 -> 2 (/c -> 30)
		// before invoke('update_state') resolves on the JS side!
		ch.emit({
			kind: 'delta',
			baseRevision: 1,
			revision: 2,
			patch: [{ op: 'replace', path: '/c', value: 30 }]
		});

		// Buffered delta has not been applied prematurely
		expect(state.revision).toBe(0);
		expect(state.obj.c).toBe(0);

		updateGate.resolve({
			status: 'synced',
			revision: 1,
			canonical: { a: 11, b: 0, c: 0 }
		});

		expect(await syncPromise).toEqual({ status: 'synced', revision: 1 });
		// Canonical /a applied, in-flight local /b preserved, buffered delta /c applied!
		expect(state.revision).toBe(2);
		expect(state.obj).toEqual({ a: 11, b: 20, c: 30 });
	});

	it('automatically recovers via snapshot_state on revision gap or invalid patch, retries after failed recovery, and supports manual resync()', async () => {
		const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
		let ch!: MockChannel<SyncMessage<{ a: number; b: number }>>;
		let snapshotAttempts = 0;
		let failNextSnapshot = true;

		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				ch = args?.channel as MockChannel<SyncMessage<{ a: number; b: number }>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { a: 0, b: 0 } });
				return 'sub-rec';
			}
			if (cmd === 'snapshot_state') {
				snapshotAttempts++;
				if (failNextSnapshot) {
					failNextSnapshot = false;
					throw new Error('temporary offline');
				}
				return {
					revision: 5,
					value: { a: 50, b: 0 }
				} satisfies StateSnapshot<{ a: number; b: number }>;
			}
			if (cmd === 'update_state') {
				const update = args?.state as StateUpdatePayload<{ a: number; b: number }>;
				expect(update.baseRevision).toBe(5);
				return { status: 'synced', revision: 6 } satisfies UpdateOutcome<{ a: number; b: number }>;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<{ a: number; b: number }>('rec');
		await state.initialized;

		// Local unsynced edit on /b
		state.obj.b = 7;

		// Gap delta triggers automatic recovery; first attempt fails cleanly
		ch.emit({
			kind: 'delta',
			baseRevision: 4,
			revision: 5,
			patch: [{ op: 'replace', path: '/a', value: 50 }]
		});
		await vi.waitFor(() => expect(snapshotAttempts).toBe(1));
		expect(state.revision).toBe(0);

		// Calling sync() detects pending resync, recovers snapshot (rev 5) preserving local /b, then syncs rev 5 -> 6!
		const res = await state.sync();
		expect(snapshotAttempts).toBe(2);
		expect(res).toEqual({ status: 'synced', revision: 6 });
		expect(state.obj).toEqual({ a: 50, b: 7 });
		expect(state.revision).toBe(6);

		// Manual resync() pulls newer snapshot cleanly
		invokeMock.mockImplementationOnce(async (cmd) => {
			expect(cmd).toBe('snapshot_state');
			return { revision: 7, value: { a: 70, b: 7 } } satisfies StateSnapshot<{
				a: number;
				b: number;
			}>;
		});
		await state.resync();
		expect(state.revision).toBe(7);
		expect(state.obj).toEqual({ a: 70, b: 7 });

		// Invalid incoming patch also triggers snapshot_state recovery cleanly
		invokeMock.mockImplementationOnce(async (cmd) => {
			expect(cmd).toBe('snapshot_state');
			return { revision: 8, value: { a: 80, b: 7 } } satisfies StateSnapshot<{
				a: number;
				b: number;
			}>;
		});
		ch.emit({
			kind: 'delta',
			baseRevision: 7,
			revision: 8,
			patch: [{ op: 'replace', path: '/nonexistent/deep', value: 1 }]
		});
		await vi.waitFor(() => expect(state.revision).toBe(8));
		expect(state.obj).toEqual({ a: 80, b: 7 });

		warnSpy.mockRestore();
	});

	it('performs zero full-state JSON.stringify calls on a 5 MB state for both incoming deltas and outgoing single-field sync()', async () => {
		let ch!: MockChannel<SyncMessage<{ a: number; b: number; blob: string }>>;
		const bigBlob = 'x'.repeat(5_000_000);

		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				ch = args?.channel as MockChannel<SyncMessage<{ a: number; b: number; blob: string }>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { a: 0, b: 0, blob: bigBlob } });
				return 'sub-perf';
			}
			if (cmd === 'update_state') {
				const update = args?.state as StateUpdatePayload<{ a: number; b: number; blob: string }>;
				expect(update.payload.kind).toBe('delta');
				return { status: 'synced', revision: update.baseRevision + 1 } satisfies UpdateOutcome<{
					a: number;
					b: number;
					blob: string;
				}>;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<{ a: number; b: number; blob: string }>('perf');
		await state.initialized;

		const origStringify = JSON.stringify;
		let largeStringifies = 0;
		const spy = vi.spyOn(JSON, 'stringify').mockImplementation(((...args: unknown[]) => {
			const out = (origStringify as (...a: unknown[]) => string)(...args);
			if (typeof out === 'string' && out.length > 1_000_000) {
				largeStringifies++;
			}
			return out;
		}) as typeof JSON.stringify);

		try {
			// Incoming 1-op delta on 5 MB state
			ch.emit({
				kind: 'delta',
				baseRevision: 0,
				revision: 1,
				patch: [{ op: 'replace', path: '/a', value: 1 }]
			});
			expect(state.obj.a).toBe(1);
			expect(state.obj.blob.length).toBe(5_000_000);

			// Outgoing 1-field sync() on 5 MB state
			state.obj.b = 2;
			const res = await state.sync();
			expect(res).toEqual({ status: 'synced', revision: 2 });
			expect(largeStringifies).toBe(0);
		} finally {
			spy.mockRestore();
		}
	});

	it('rejects initialized when subscribe_state fails and keeps ready false', async () => {
		const backendError = { code: 'unknownKey', message: 'missing' };
		invokeMock.mockRejectedValueOnce(backendError);

		const state = new SyncedState<CounterState>('missing');
		await expect(state.initialized).rejects.toEqual(backendError);
		expect(state.ready).toBe(false);
	});

	it('close() unsubscribes once, ignores subsequent channel messages, and is idempotent', async () => {
		let capturedChannel: MockChannel<SyncMessage<CounterState>> | undefined;
		const unsubscribeCalls: Array<Record<string, unknown> | undefined> = [];

		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				capturedChannel = args?.channel as MockChannel<SyncMessage<CounterState>>;
				capturedChannel.emit({
					kind: 'snapshot',
					revision: 0,
					value: { count: 1, label: 'init' }
				});
				return 'sub-7';
			}
			if (cmd === 'unsubscribe_state') {
				unsubscribeCalls.push(args);
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<CounterState>('counter');
		await state.initialized;

		await Promise.all([state.close(), state.close()]);
		await state.close();

		expect(unsubscribeCalls).toEqual([{ name: 'counter', subscriberId: 'sub-7' }]);

		capturedChannel!.emit({
			kind: 'delta',
			baseRevision: 0,
			revision: 1,
			patch: [{ op: 'replace', path: '/count', value: 99 }]
		});
		expect(state.obj).toEqual({ count: 1, label: 'init' });
	});

	it('close() called while subscribe_state is in flight still unsubscribes once resolved', async () => {
		const subscribeGate = deferred<string>();
		const unsubscribeCalls: Array<Record<string, unknown> | undefined> = [];

		invokeMock.mockImplementation((cmd, args) => {
			if (cmd === 'subscribe_state') {
				return subscribeGate.promise;
			}
			if (cmd === 'unsubscribe_state') {
				unsubscribeCalls.push(args);
				return Promise.resolve(true);
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<CounterState>('counter');
		const closePromise = state.close();

		await expect(state.initialized).resolves.toBeUndefined();
		expect(state.ready).toBe(false);

		subscribeGate.resolve('sub-late');
		await closePromise;

		expect(unsubscribeCalls).toEqual([{ name: 'counter', subscriberId: 'sub-late' }]);
	});

	it('close() resolves cleanly even if subscribe_state or unsubscribe_state rejects', async () => {
		const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});

		invokeMock.mockRejectedValueOnce(new Error('subscribe boom'));
		const s1 = new SyncedState<CounterState>('a');
		await expect(s1.initialized).rejects.toThrow('subscribe boom');
		await expect(s1.close()).resolves.toBeUndefined();

		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				const ch = args?.channel as MockChannel<SyncMessage<CounterState>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { count: 1, label: 'ok' } });
				return 'sub-9';
			}
			if (cmd === 'unsubscribe_state') {
				throw new Error('unsubscribe boom');
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const s2 = new SyncedState<CounterState>('b');
		await s2.initialized;
		await expect(s2.close()).resolves.toBeUndefined();
		expect(warnSpy).toHaveBeenCalledTimes(1);

		warnSpy.mockRestore();
	});
});
