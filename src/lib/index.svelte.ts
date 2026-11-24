export { SyncedState } from './synced-state.svelte.js';
export { acquireSyncedState, closeAllSyncedStates, useSyncedState } from './registry.svelte.js';
export type {
	ConflictPolicy,
	DeliveryMode,
	JsonPatchOp,
	SharedSyncedStateOptions,
	StateSnapshot,
	StateUpdatePayload,
	SyncMessage,
	SyncResult,
	SyncedStateHandle,
	SyncedStateOptions,
	UpdateOutcome,
	UpdatePayload
} from './protocol.js';
