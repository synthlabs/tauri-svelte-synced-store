import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type {
	StateSnapshot,
	StateUpdatePayload,
	SyncMessage,
	SyncedStateHandle,
	UpdateOutcome
} from '../src/lib/protocol.js';
import { createEffectRoot, trackEffect } from './fixtures/reactivity.svelte.js';

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

const { SyncedState, acquireSyncedState, closeAllSyncedStates, useSyncedState } = await import(
	'../src/lib/index.svelte.js'
);
const { default: SharedStoreConsumer } = await import('./fixtures/SharedStoreConsumer.svelte');

interface NestedStore {
	count: number;
	label: string;
	nested: {
		a: number;
		b: string;
		extra?: string;
	};
	sibling: {
		flag: boolean;
	};
	items: number[];
}

interface CounterState {
	count: number;
	label: string;
}

describe('fine-grained in-place Svelte 5 reactivity', () => {
	beforeEach(() => {
		invokeMock.mockReset();
		MockChannel.nextId = 1;
	});

	afterEach(async () => {
		await closeAllSyncedStates();
		vi.useRealTimers();
	});

	it('updates a single nested leaf in-place without triggering unrelated sibling readers or replacing proxy identity', async () => {
		let ch!: MockChannel<SyncMessage<NestedStore>>;
		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				ch = args?.channel as MockChannel<SyncMessage<NestedStore>>;
				ch.emit({
					kind: 'snapshot',
					revision: 0,
					value: {
						count: 1,
						label: 'init',
						nested: { a: 10, b: 'stay' },
						sibling: { flag: false },
						items: [1, 2]
					}
				});
				return 'sub-fine';
			}
			if (cmd === 'unsubscribe_state') {
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<NestedStore>('nested', {
			count: 1,
			label: 'init',
			nested: { a: 10, b: 'stay' },
			sibling: { flag: false },
			items: [1, 2]
		});
		const rootRef = state.obj;
		const nestedRef = state.obj.nested;
		const siblingRef = state.obj.sibling;
		const itemsRef = state.obj.items;

		let aRuns = 0;
		let bRuns = 0;
		let siblingRuns = 0;
		let itemsRuns = 0;

		const stop = createEffectRoot(() => {
			trackEffect(() => {
				void state.obj.nested.a;
				aRuns++;
			});
			trackEffect(() => {
				void state.obj.nested.b;
				bRuns++;
			});
			trackEffect(() => {
				void state.obj.sibling.flag;
				siblingRuns++;
			});
			trackEffect(() => {
				void state.obj.items.length;
				itemsRuns++;
			});
		});

		flushSync();
		await state.initialized;
		flushSync();

		// Initial snapshot matched constructor initial values -> zero extra runs and preserved identities
		expect(state.obj).toBe(rootRef);
		expect(state.obj.nested).toBe(nestedRef);
		expect(state.obj.sibling).toBe(siblingRef);
		expect(state.obj.items).toBe(itemsRef);
		expect([aRuns, bRuns, siblingRuns, itemsRuns]).toEqual([1, 1, 1, 1]);

		// Patch only /nested/a
		ch.emit({
			kind: 'delta',
			baseRevision: 0,
			revision: 1,
			patch: [{ op: 'replace', path: '/nested/a', value: 42 }]
		});
		flushSync();

		expect(state.obj).toBe(rootRef);
		expect(state.obj.nested).toBe(nestedRef);
		expect(state.obj.sibling).toBe(siblingRef);
		expect(state.obj.nested.a).toBe(42);
		expect(aRuns).toBe(2);
		expect(bRuns).toBe(1);
		expect(siblingRuns).toBe(1);
		expect(itemsRuns).toBe(1);

		stop();
		await state.close();
	});

	it('handles key addition, key removal, and array replacement with targeted reactivity', async () => {
		let ch!: MockChannel<SyncMessage<NestedStore>>;
		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				ch = args?.channel as MockChannel<SyncMessage<NestedStore>>;
				ch.emit({
					kind: 'snapshot',
					revision: 0,
					value: {
						count: 1,
						label: 'init',
						nested: { a: 1, b: 'ok' },
						sibling: { flag: true },
						items: [1, 2]
					}
				});
				return 'sub-keys';
			}
			if (cmd === 'unsubscribe_state') {
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<NestedStore>('keys');
		await state.initialized;

		let extraRuns = 0;
		let keysRuns = 0;
		let bRuns = 0;
		let itemsRuns = 0;
		let lastExtra: string | undefined;
		let lastNestedKeys: string[] = [];

		const stop = createEffectRoot(() => {
			trackEffect(() => {
				lastExtra = state.obj.nested.extra;
				extraRuns++;
			});
			trackEffect(() => {
				lastNestedKeys = Object.keys(state.obj.nested).sort();
				keysRuns++;
			});
			trackEffect(() => {
				void state.obj.nested.b;
				bRuns++;
			});
			trackEffect(() => {
				void state.obj.items;
				itemsRuns++;
			});
		});
		flushSync();
		expect([extraRuns, keysRuns, bRuns, itemsRuns]).toEqual([1, 1, 1, 1]);
		expect(lastExtra).toBeUndefined();
		expect(lastNestedKeys).toEqual(['a', 'b']);

		// Key addition
		ch.emit({
			kind: 'delta',
			baseRevision: 0,
			revision: 1,
			patch: [{ op: 'add', path: '/nested/extra', value: 'hello' }]
		});
		flushSync();
		expect(lastExtra).toBe('hello');
		expect(lastNestedKeys).toEqual(['a', 'b', 'extra']);
		expect(extraRuns).toBe(2);
		expect(keysRuns).toBe(2);
		expect(bRuns).toBe(1);
		expect(itemsRuns).toBe(1);

		// Key removal
		ch.emit({
			kind: 'delta',
			baseRevision: 1,
			revision: 2,
			patch: [{ op: 'remove', path: '/nested/extra' }]
		});
		flushSync();
		expect(lastExtra).toBeUndefined();
		expect('extra' in state.obj.nested).toBe(false);
		expect(lastNestedKeys).toEqual(['a', 'b']);
		expect(extraRuns).toBe(3);
		expect(keysRuns).toBe(3);
		expect(bRuns).toBe(1);
		expect(itemsRuns).toBe(1);

		// Array replacement (different contents)
		const prevItems = state.obj.items;
		ch.emit({
			kind: 'delta',
			baseRevision: 2,
			revision: 3,
			patch: [{ op: 'replace', path: '/items', value: [1, 2, 3] }]
		});
		flushSync();
		expect(state.obj.items).not.toBe(prevItems);
		expect(state.obj.items).toEqual([1, 2, 3]);
		expect(itemsRuns).toBe(2);
		expect(bRuns).toBe(1);

		// Snapshot with identical array preserves array identity and does not re-trigger array readers
		const itemsAfterDelta = state.obj.items;
		ch.emit({
			kind: 'snapshot',
			revision: 4,
			value: {
				count: 2,
				label: 'init',
				nested: { a: 1, b: 'ok' },
				sibling: { flag: true },
				items: [1, 2, 3]
			}
		});
		flushSync();
		expect(state.obj.count).toBe(2);
		expect(state.obj.items).toBe(itemsAfterDelta);
		expect(itemsRuns).toBe(2);
		expect(bRuns).toBe(1);

		stop();
		await state.close();
	});

	it('reconciles in-place across snapshot mode, replace policy, canonical sync(), stale sync(), and resync()', async () => {
		let ch!: MockChannel<SyncMessage<{ a: number; b: number; nested: { x: number; y: number } }>>;
		let updateStep = 0;

		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				ch = args?.channel as MockChannel<
					SyncMessage<{ a: number; b: number; nested: { x: number; y: number } }>
				>;
				ch.emit({
					kind: 'snapshot',
					revision: 0,
					value: { a: 1, b: 10, nested: { x: 1, y: 2 } }
				});
				return 'sub-all';
			}
			if (cmd === 'update_state') {
				updateStep++;
				const payload = args?.state as StateUpdatePayload<{
					a: number;
					b: number;
					nested: { x: number; y: number };
				}>;
				if (updateStep === 1) {
					return {
						status: 'synced',
						revision: payload.baseRevision + 1,
						canonical: { a: 99, b: 10, nested: { x: 1, y: 2 } }
					} satisfies UpdateOutcome<{ a: number; b: number; nested: { x: number; y: number } }>;
				}
				return {
					status: 'stale',
					snapshot: {
						revision: payload.baseRevision + 2,
						value: { a: 99, b: 20, nested: { x: 1, y: 2 } }
					}
				} satisfies UpdateOutcome<{ a: number; b: number; nested: { x: number; y: number } }>;
			}
			if (cmd === 'snapshot_state') {
				return {
					revision: 10,
					value: { a: 99, b: 20, nested: { x: 5, y: 2 } }
				} satisfies StateSnapshot<{ a: number; b: number; nested: { x: number; y: number } }>;
			}
			if (cmd === 'unsubscribe_state') {
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<{ a: number; b: number; nested: { x: number; y: number } }>(
			'all',
			undefined,
			{ conflictPolicy: 'replace' }
		);
		await state.initialized;

		const rootRef = state.obj;
		const nestedRef = state.obj.nested;

		let aRuns = 0;
		let yRuns = 0;
		const stop = createEffectRoot(() => {
			trackEffect(() => {
				void state.obj.a;
				aRuns++;
			});
			trackEffect(() => {
				void state.obj.nested.y;
				yRuns++;
			});
		});
		flushSync();
		expect([aRuns, yRuns]).toEqual([1, 1]);

		// Canonical sync() normalization only touches /a
		state.obj.a = 50;
		flushSync();
		expect(aRuns).toBe(2);
		const synced = await state.sync();
		flushSync();
		expect(synced).toEqual({ status: 'synced', revision: 1 });
		expect(state.obj).toBe(rootRef);
		expect(state.obj.nested).toBe(nestedRef);
		expect(state.obj.a).toBe(99);
		expect(aRuns).toBe(3);
		expect(yRuns).toBe(1);

		// Stale sync() under replace reverts disjoint local edit and applies remote /b, leaving /nested/y untouched
		state.obj.nested.x = 77;
		const stale = await state.sync();
		flushSync();
		expect(stale).toEqual({ status: 'stale', revision: 3 });
		expect(state.obj).toBe(rootRef);
		expect(state.obj.nested).toBe(nestedRef);
		expect(state.obj.b).toBe(20);
		expect(state.obj.nested.x).toBe(1);
		expect(yRuns).toBe(1);

		// Incoming delta under replace reverts unsynced local /a while preserving /nested/y
		state.obj.a = 123;
		flushSync();
		ch.emit({
			kind: 'delta',
			baseRevision: 3,
			revision: 4,
			patch: [{ op: 'replace', path: '/b', value: 25 }]
		});
		flushSync();
		expect(state.obj).toBe(rootRef);
		expect(state.obj.a).toBe(99);
		expect(state.obj.b).toBe(25);
		expect(yRuns).toBe(1);

		// Manual resync() only touches changed /nested/x
		await state.resync();
		flushSync();
		expect(state.obj).toBe(rootRef);
		expect(state.obj.nested).toBe(nestedRef);
		expect(state.obj.nested.x).toBe(5);
		expect(yRuns).toBe(1);

		stop();
		await state.close();
	});

	it('replaces root cleanly for primitive and array roots', async () => {
		let arrCh!: MockChannel<SyncMessage<number[]>>;
		let primCh!: MockChannel<SyncMessage<number>>;
		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				if (args?.name === 'arr-root') {
					arrCh = args.channel as MockChannel<SyncMessage<number[]>>;
					return 'sub-arr';
				}
				if (args?.name === 'prim-root') {
					primCh = args.channel as MockChannel<SyncMessage<number>>;
					return 'sub-prim';
				}
			}
			if (cmd === 'unsubscribe_state') {
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const arrState = new SyncedState<number[]>('arr-root', []);
		let arrRuns = 0;
		let latestArr: number[] = [];
		const stopArr = createEffectRoot(() => {
			trackEffect(() => {
				latestArr = [...arrState.obj];
				arrRuns++;
			});
		});
		flushSync();
		expect(latestArr).toEqual([]);
		expect(arrRuns).toBe(1);

		arrCh.emit({ kind: 'snapshot', revision: 0, value: [1, 2] });
		await arrState.initialized;
		flushSync();
		expect(latestArr).toEqual([1, 2]);
		expect(arrRuns).toBe(2);

		arrCh.emit({
			kind: 'delta',
			baseRevision: 0,
			revision: 1,
			patch: [{ op: 'replace', path: '', value: [3, 4, 5] }]
		});
		flushSync();
		expect(latestArr).toEqual([3, 4, 5]);
		expect(arrRuns).toBe(3);

		const primState = new SyncedState<number>('prim-root', 0);
		let primRuns = 0;
		let latestPrim = -1;
		const stopPrim = createEffectRoot(() => {
			trackEffect(() => {
				latestPrim = primState.obj;
				primRuns++;
			});
		});
		flushSync();
		expect(latestPrim).toBe(0);
		expect(primRuns).toBe(1);

		primCh.emit({ kind: 'snapshot', revision: 0, value: 42 });
		await primState.initialized;
		flushSync();
		expect(latestPrim).toBe(42);
		expect(primRuns).toBe(2);

		primCh.emit({
			kind: 'delta',
			baseRevision: 0,
			revision: 1,
			patch: [{ op: 'replace', path: '', value: 99 }]
		});
		flushSync();
		expect(latestPrim).toBe(99);
		expect(primRuns).toBe(3);

		stopArr();
		stopPrim();
		await Promise.all([arrState.close(), primState.close()]);
	});
});

describe('shared ref-counted frontend handles', () => {
	beforeEach(() => {
		invokeMock.mockReset();
		MockChannel.nextId = 1;
	});

	afterEach(async () => {
		await closeAllSyncedStates();
		vi.useRealTimers();
	});

	it('shares one backend Channel across multiple mounted Svelte 5 components and unsubscribes when the last unmounts', async () => {
		let ch!: MockChannel<
			SyncMessage<{ count: number; label: string; nested: { a: number; b: string } }>
		>;
		let subscribeCalls = 0;
		const unsubscribeCalls: Array<Record<string, unknown> | undefined> = [];

		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				subscribeCalls++;
				ch = args?.channel as MockChannel<
					SyncMessage<{ count: number; label: string; nested: { a: number; b: string } }>
				>;
				ch.emit({
					kind: 'snapshot',
					revision: 0,
					value: { count: 1, label: 'shared', nested: { a: 10, b: 'keep' } }
				});
				return `sub-${subscribeCalls}`;
			}
			if (cmd === 'unsubscribe_state') {
				unsubscribeCalls.push(args);
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const targetA = document.createElement('div');
		const targetB = document.createElement('div');
		document.body.append(targetA, targetB);

		const handles: Array<
			SyncedStateHandle<{ count: number; label: string; nested: { a: number; b: string } }>
		> = [];
		let compAARuns = 0;
		let compABRuns = 0;
		let compBARuns = 0;
		let compBBRuns = 0;

		const compA = mount(SharedStoreConsumer, {
			target: targetA,
			props: {
				name: 'app_settings',
				onHandle: (h) => handles.push(h),
				onARun: () => compAARuns++,
				onBRun: () => compABRuns++
			}
		});
		const compB = mount(SharedStoreConsumer, {
			target: targetB,
			props: {
				name: 'app_settings',
				onHandle: (h) => handles.push(h),
				onARun: () => compBARuns++,
				onBRun: () => compBBRuns++
			}
		});
		flushSync();
		await handles[0].initialized;
		flushSync();

		expect(subscribeCalls).toBe(1);
		expect(handles).toHaveLength(2);
		expect(handles[0].obj).toBe(handles[1].obj);
		expect(targetA.querySelector('[data-testid="count"]')?.textContent).toBe('1');
		expect(targetB.querySelector('[data-testid="count"]')?.textContent).toBe('1');

		// Incoming leaf delta updates both components without firing sibling B effects
		const prevAB = compABRuns;
		const prevBB = compBBRuns;
		ch.emit({
			kind: 'delta',
			baseRevision: 0,
			revision: 1,
			patch: [{ op: 'replace', path: '/nested/a', value: 77 }]
		});
		flushSync();
		expect(targetA.querySelector('[data-testid="nested-a"]')?.textContent).toBe('77');
		expect(targetB.querySelector('[data-testid="nested-a"]')?.textContent).toBe('77');
		expect(compAARuns).toBeGreaterThan(1);
		expect(compBARuns).toBeGreaterThan(1);
		expect(compABRuns).toBe(prevAB);
		expect(compBBRuns).toBe(prevBB);

		// Local edit from component A's handle is immediately reactive in component B
		handles[0].obj.count = 5;
		flushSync();
		expect(targetB.querySelector('[data-testid="count"]')?.textContent).toBe('5');

		// Unmounting first component keeps subscription alive
		await unmount(compA);
		expect(handles[0].released).toBe(true);
		expect(handles[1].released).toBe(false);
		expect(unsubscribeCalls).toHaveLength(0);

		// Unmounting last component closes subscription cleanly
		await unmount(compB);
		await vi.waitFor(() => expect(unsubscribeCalls).toHaveLength(1));
		expect(handles[1].released).toBe(true);
		expect(unsubscribeCalls).toEqual([{ name: 'app_settings', subscriberId: 'sub-1' }]);

		targetA.remove();
		targetB.remove();
	});

	it('useSyncedState works inside $effect.root (even with immediate teardown) and throws outside a reactive root', async () => {
		let subscribeCalls = 0;
		const unsubscribeCalls: Array<Record<string, unknown> | undefined> = [];

		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				subscribeCalls++;
				const ch = args?.channel as MockChannel<SyncMessage<CounterState>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { count: 1, label: 'ok' } });
				return `sub-${subscribeCalls}`;
			}
			if (cmd === 'unsubscribe_state') {
				unsubscribeCalls.push(args);
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		// Outside component / $effect.root -> throws before subscribing
		expect(() => useSyncedState<CounterState>('outside')).toThrow(
			'useSyncedState("outside") must be called inside a Svelte component or $effect.root()'
		);
		expect(subscribeCalls).toBe(0);

		// Inside $effect.root with immediate synchronous teardown (no flushSync in between)
		let handle!: SyncedStateHandle<CounterState>;
		const stop = createEffectRoot(() => {
			handle = useSyncedState<CounterState>('root-store');
		});
		expect(subscribeCalls).toBe(1);
		stop();
		expect(handle.released).toBe(true);
		await vi.waitFor(() => expect(unsubscribeCalls).toHaveLength(1));
		expect(unsubscribeCalls).toEqual([{ name: 'root-store', subscriberId: 'sub-1' }]);
	});

	it('enforces handle contract: no close(), idempotent release(), post-release guards, and standalone isolation', async () => {
		let subscribeCalls = 0;
		const unsubscribeCalls: Array<Record<string, unknown> | undefined> = [];

		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				subscribeCalls++;
				const ch = args?.channel as MockChannel<SyncMessage<CounterState>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { count: 1, label: 'x' } });
				return `sub-${subscribeCalls}`;
			}
			if (cmd === 'unsubscribe_state') {
				unsubscribeCalls.push(args);
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const h1 = acquireSyncedState<CounterState>('shared');
		const h2 = acquireSyncedState<CounterState>('shared');
		const standalone = new SyncedState<CounterState>('shared');
		await Promise.all([h1.initialized, standalone.initialized]);

		expect(subscribeCalls).toBe(2); // 1 for shared registry, 1 for standalone
		expect('close' in h1).toBe(false);
		expect(h1.obj).toBe(h2.obj);
		expect(h1.obj).not.toBe(standalone.obj);

		// Releasing h1 multiple times never decrements h2's ref
		await Promise.all([h1.release(), h1.release()]);
		await h1.release();
		expect(h1.released).toBe(true);
		expect(h2.released).toBe(false);
		expect(unsubscribeCalls).toHaveLength(0);

		// Post-release guards on h1 while h2 remains open
		expect(h1.obj.count).toBe(1); // safe to read during teardown
		expect(() => {
			h1.obj = { count: 9, label: 'nope' };
		}).toThrow('SyncedStateHandle "shared" is released');
		await expect(h1.sync()).rejects.toThrow('SyncedStateHandle "shared" is released');
		await expect(h1.resync()).rejects.toThrow('SyncedStateHandle "shared" is released');

		await h2.release();
		expect(unsubscribeCalls).toEqual([{ name: 'shared', subscriberId: 'sub-1' }]);

		await standalone.close();
		expect(unsubscribeCalls).toHaveLength(2);
	});

	it('throws on conflicting explicit mode or conflictPolicy while inheriting omitted options', async () => {
		let subscribeCalls = 0;
		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				subscribeCalls++;
				const ch = args?.channel as MockChannel<SyncMessage<CounterState>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { count: 0, label: '' } });
				return `sub-${subscribeCalls}`;
			}
			if (cmd === 'unsubscribe_state') {
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const h1 = acquireSyncedState<CounterState>('cfg', undefined, {
			mode: 'snapshot',
			conflictPolicy: 'frontend-wins'
		});
		await h1.initialized;

		// Omitted / explicit undefined options inherit existing config cleanly
		const h2 = acquireSyncedState<CounterState>('cfg', undefined, {
			mode: undefined,
			conflictPolicy: undefined
		});
		expect(h2.mode).toBe('snapshot');
		expect(h2.conflictPolicy).toBe('frontend-wins');

		// Conflicting explicit mode or conflictPolicy throws without bumping refCount
		expect(() => acquireSyncedState<CounterState>('cfg', undefined, { mode: 'delta' })).toThrow(
			/already open with mode "snapshot"/
		);
		expect(() =>
			acquireSyncedState<CounterState>('cfg', undefined, { conflictPolicy: 'backend-wins' })
		).toThrow(/already open with conflictPolicy "frontend-wins"/);

		await h1.release();
		await h2.release();
		expect(subscribeCalls).toBe(1);
	});

	it('supports releaseDelayMs grace period, cancels pending close on re-acquire, and validates invalid delays', async () => {
		vi.useFakeTimers();
		let subscribeCalls = 0;
		const unsubscribeCalls: Array<Record<string, unknown> | undefined> = [];

		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				subscribeCalls++;
				const ch = args?.channel as MockChannel<SyncMessage<CounterState>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { count: 1, label: 'grace' } });
				return `sub-${subscribeCalls}`;
			}
			if (cmd === 'unsubscribe_state') {
				unsubscribeCalls.push(args);
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		expect(() =>
			acquireSyncedState<CounterState>('bad', undefined, { releaseDelayMs: -1 })
		).toThrow(RangeError);
		expect(() =>
			acquireSyncedState<CounterState>('bad', undefined, { releaseDelayMs: Number.NaN })
		).toThrow(RangeError);
		expect(subscribeCalls).toBe(0);

		const h1 = acquireSyncedState<CounterState>('route_state', undefined, { releaseDelayMs: 150 });
		await h1.initialized;
		await h1.release();

		// Still open during grace window
		await vi.advanceTimersByTimeAsync(100);
		expect(unsubscribeCalls).toHaveLength(0);

		// Re-acquiring before expiry cancels teardown and reuses the same channel
		const h2 = acquireSyncedState<CounterState>('route_state');
		expect(subscribeCalls).toBe(1);
		expect(h2.obj).toBe(h1.obj);

		await vi.advanceTimersByTimeAsync(500);
		expect(unsubscribeCalls).toHaveLength(0);

		// Release again and let the grace period expire
		await h2.release();
		await vi.advanceTimersByTimeAsync(149);
		expect(unsubscribeCalls).toHaveLength(0);
		await vi.advanceTimersByTimeAsync(1);
		expect(unsubscribeCalls).toEqual([{ name: 'route_state', subscriberId: 'sub-1' }]);
	});

	it('evicts failed initial subscriptions automatically so subsequent acquires can retry cleanly', async () => {
		let attempt = 0;
		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				attempt++;
				if (attempt === 1) {
					throw { code: 'unknownKey', message: 'not registered yet' };
				}
				const ch = args?.channel as MockChannel<SyncMessage<CounterState>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { count: 7, label: 'recovered' } });
				return 'sub-retry';
			}
			if (cmd === 'unsubscribe_state') {
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const failed = acquireSyncedState<CounterState>('late_key');
		await expect(failed.initialized).rejects.toEqual({
			code: 'unknownKey',
			message: 'not registered yet'
		});
		// Flush microtask eviction handler
		await Promise.resolve();
		expect(failed.released).toBe(true);

		const retried = acquireSyncedState<CounterState>('late_key');
		await retried.initialized;
		expect(retried.ready).toBe(true);
		expect(retried.obj).toEqual({ count: 7, label: 'recovered' });
		expect(attempt).toBe(2);

		await retried.release();
	});

	it('cleans up active and grace-period handles on window pagehide and closeAllSyncedStates()', async () => {
		vi.useFakeTimers();
		let subscribeCalls = 0;
		const unsubscribeCalls: Array<Record<string, unknown> | undefined> = [];

		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				subscribeCalls++;
				const ch = args?.channel as MockChannel<SyncMessage<CounterState>>;
				ch.emit({ kind: 'snapshot', revision: 0, value: { count: subscribeCalls, label: 'ok' } });
				return `sub-${subscribeCalls}`;
			}
			if (cmd === 'unsubscribe_state') {
				unsubscribeCalls.push(args);
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const active = acquireSyncedState<CounterState>('active_store');
		const delayed = acquireSyncedState<CounterState>('delayed_store', undefined, {
			releaseDelayMs: 5_000
		});
		await Promise.all([active.initialized, delayed.initialized]);
		await delayed.release(); // sitting in grace timer

		window.dispatchEvent(new Event('pagehide'));
		await vi.waitFor(() => expect(unsubscribeCalls).toHaveLength(2));

		expect(active.released).toBe(true);
		expect(delayed.released).toBe(true);
		expect(unsubscribeCalls).toEqual([
			{ name: 'active_store', subscriberId: 'sub-1' },
			{ name: 'delayed_store', subscriberId: 'sub-2' }
		]);

		// Re-acquiring after pagehide opens a fresh channel and closeAllSyncedStates() closes it cleanly
		const fresh = acquireSyncedState<CounterState>('active_store');
		await fresh.initialized;
		expect(subscribeCalls).toBe(3);

		await closeAllSyncedStates();
		expect(fresh.released).toBe(true);
		expect(unsubscribeCalls).toHaveLength(3);
	});
});
