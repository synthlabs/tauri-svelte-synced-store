export type SyncMessage<T = unknown> = {
	kind: 'snapshot';
	value: T;
};

export interface StateUpdatePayload<T = unknown> {
	name: string;
	value: T;
	subscriberId?: string;
}
