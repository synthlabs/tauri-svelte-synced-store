export type DeliveryMode = 'delta' | 'snapshot';

export type ConflictPolicy = 'backend-wins' | 'frontend-wins' | 'replace';

export interface SyncedStateOptions {
	mode?: DeliveryMode;
	conflictPolicy?: ConflictPolicy;
}

export type JsonPatchOp =
	| { op: 'add'; path: string; value: unknown }
	| { op: 'remove'; path: string }
	| { op: 'replace'; path: string; value: unknown };

export interface StateSnapshot<T = unknown> {
	revision: number;
	value: T;
}

export type SyncMessage<T = unknown> =
	| {
			kind: 'snapshot';
			revision: number;
			value: T;
	  }
	| {
			kind: 'delta';
			baseRevision: number;
			revision: number;
			patch: JsonPatchOp[];
	  };

export type UpdatePayload<T = unknown> =
	| {
			kind: 'full';
			value: T;
	  }
	| {
			kind: 'delta';
			patch: JsonPatchOp[];
	  };

export interface StateUpdatePayload<T = unknown> {
	name: string;
	baseRevision: number;
	payload: UpdatePayload<T>;
	subscriberId?: string;
}

export type UpdateOutcome<T = unknown> =
	| {
			status: 'synced';
			revision: number;
			canonical?: T | null;
	  }
	| {
			status: 'unchanged';
			revision: number;
	  }
	| {
			status: 'stale';
			snapshot: StateSnapshot<T>;
	  };

export type SyncResult =
	| { status: 'synced'; revision: number }
	| { status: 'unchanged'; revision: number }
	| { status: 'stale'; revision: number }
	| { status: 'conflict'; revision: number; paths: string[] };
