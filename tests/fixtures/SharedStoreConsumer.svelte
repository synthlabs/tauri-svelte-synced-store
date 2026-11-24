<script lang="ts">
	import { untrack } from 'svelte';
	import { useSyncedState } from '../../src/lib/index.svelte.js';
	import type { SharedSyncedStateOptions, SyncedStateHandle } from '../../src/lib/protocol.js';

	export interface ConsumerState {
		count: number;
		label: string;
		nested: {
			a: number;
			b: string;
		};
	}

	let {
		name,
		initial,
		options,
		onHandle,
		onARun,
		onBRun
	}: {
		name: string;
		initial?: ConsumerState;
		options?: SharedSyncedStateOptions;
		onHandle?: (handle: SyncedStateHandle<ConsumerState>) => void;
		onARun?: (value: number) => void;
		onBRun?: (value: string) => void;
	} = $props();

	const storeName = untrack(() => name);
	const initialState = untrack(() => initial);
	const storeOptions = untrack(() => options);

	const handle = useSyncedState<ConsumerState>(storeName, initialState, storeOptions);
	untrack(() => onHandle)?.(handle);

	$effect(() => {
		if (handle.obj.nested) {
			onARun?.(handle.obj.nested.a);
		}
	});

	$effect(() => {
		if (handle.obj.nested) {
			onBRun?.(handle.obj.nested.b);
		}
	});
</script>

<div data-testid="count">{handle.obj.count}</div>
<div data-testid="label">{handle.obj.label}</div>
{#if handle.obj.nested}
	<div data-testid="nested-a">{handle.obj.nested.a}</div>
	<div data-testid="nested-b">{handle.obj.nested.b}</div>
{/if}
