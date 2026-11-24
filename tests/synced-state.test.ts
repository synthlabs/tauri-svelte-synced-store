import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { StateUpdatePayload, SyncMessage } from '../src/lib/protocol.js';

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

function deferred<T>() {
	let resolve!: (value: T) => void;
	let reject!: (reason?: unknown) => void;
	const promise = new Promise<T>((res, rej) => {
		resolve = res;
		reject = rej;
	});
	return { promise, resolve, reject };
}

describe('SyncedState (Tauri Channel transport)', () => {
	beforeEach(() => {
		invokeMock.mockReset();
		MockChannel.nextId = 1;
	});

	it('subscribes over a Channel, applies snapshots in order, and resolves initialized on first snapshot', async () => {
		let capturedChannel: MockChannel<SyncMessage<CounterState>> | undefined;
		const subscribeGate = deferred<string>();

		invokeMock.mockImplementation((cmd, args) => {
			if (cmd === 'subscribe_state') {
				capturedChannel = args?.channel as MockChannel<SyncMessage<CounterState>>;
				return subscribeGate.promise;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<CounterState>('counter', { count: 0, label: 'local' });
		expect(state.ready).toBe(false);
		expect(state.obj).toEqual({ count: 0, label: 'local' });
		expect(capturedChannel).toBeDefined();

		// Initial snapshot arrives on the channel before subscribe_state resolves.
		capturedChannel!.emit({ kind: 'snapshot', value: { count: 1, label: 'server' } });
		subscribeGate.resolve('sub-1');

		await state.initialized;
		expect(state.ready).toBe(true);
		expect(state.obj).toEqual({ count: 1, label: 'server' });

		// Subsequent updates replace obj reactively.
		capturedChannel!.emit({ kind: 'snapshot', value: { count: 2, label: 'next' } });
		expect(state.obj).toEqual({ count: 2, label: 'next' });
	});

	it('sync() sends structured StateUpdatePayload with subscriberId and rejects on backend failure or after close', async () => {
		const calls: InvokeCall[] = [];
		let capturedChannel: MockChannel<SyncMessage<CounterState>> | undefined;

		invokeMock.mockImplementation(async (cmd, args) => {
			calls.push({ cmd, args });
			if (cmd === 'subscribe_state') {
				capturedChannel = args?.channel as MockChannel<SyncMessage<CounterState>>;
				capturedChannel.emit({ kind: 'snapshot', value: { count: 1, label: 'init' } });
				return 'sub-42';
			}
			if (cmd === 'update_state') {
				const stateArg = args?.state as StateUpdatePayload<CounterState>;
				if (stateArg.value.count < 0) {
					throw { code: 'invalidValue', message: 'negative count' };
				}
				return true;
			}
			if (cmd === 'unsubscribe_state') {
				return true;
			}
			throw new Error(`unexpected command: ${cmd}`);
		});

		const state = new SyncedState<CounterState>('counter');
		await state.initialized;

		state.obj.count = 10;
		state.obj.label = 'edited';
		const ok = await state.sync();
		expect(ok).toBe(true);

		const updateCall = calls.find((c) => c.cmd === 'update_state');
		expect(updateCall?.args).toEqual({
			state: {
				name: 'counter',
				value: { count: 10, label: 'edited' },
				subscriberId: 'sub-42'
			}
		});

		state.obj.count = -1;
		await expect(state.sync()).rejects.toEqual({
			code: 'invalidValue',
			message: 'negative count'
		});

		await state.close();
		await expect(state.sync()).rejects.toThrow('SyncedState "counter" is closed');
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
				capturedChannel.emit({ kind: 'snapshot', value: { count: 1, label: 'init' } });
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

		// Messages emitted after close are ignored.
		capturedChannel!.emit({ kind: 'snapshot', value: { count: 99, label: 'stale' } });
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

		// initialized settles cleanly without rejection while ready remains false.
		await expect(state.initialized).resolves.toBeUndefined();
		expect(state.ready).toBe(false);

		subscribeGate.resolve('sub-late');
		await closePromise;

		expect(unsubscribeCalls).toEqual([{ name: 'counter', subscriberId: 'sub-late' }]);
	});

	it('close() resolves cleanly even if subscribe_state or unsubscribe_state rejects', async () => {
		const warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});

		// Case 1: subscribe fails before close
		invokeMock.mockRejectedValueOnce(new Error('subscribe boom'));
		const s1 = new SyncedState<CounterState>('a');
		await expect(s1.initialized).rejects.toThrow('subscribe boom');
		await expect(s1.close()).resolves.toBeUndefined();

		// Case 2: unsubscribe fails during close
		invokeMock.mockImplementation(async (cmd, args) => {
			if (cmd === 'subscribe_state') {
				const ch = args?.channel as MockChannel<SyncMessage<CounterState>>;
				ch.emit({ kind: 'snapshot', value: { count: 1, label: 'ok' } });
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
