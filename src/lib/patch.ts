import type { ConflictPolicy, JsonPatchOp } from './protocol.js';

export function isPlainObject(value: unknown): value is Record<string, unknown> {
	return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function defineJsonProp(target: Record<string, unknown>, key: string, value: unknown): void {
	if (key === '__proto__') {
		Object.defineProperty(target, key, {
			value,
			writable: true,
			enumerable: true,
			configurable: true
		});
	} else {
		target[key] = value;
	}
}

function hasDefinedKey(obj: Record<string, unknown>, key: string): boolean {
	return Object.hasOwn(obj, key) && obj[key] !== undefined;
}

function shallowClonePlainObject(obj: Record<string, unknown>): Record<string, unknown> {
	const out: Record<string, unknown> = {};
	for (const key of Object.keys(obj)) {
		const val = obj[key];
		if (val !== undefined) {
			defineJsonProp(out, key, val);
		}
	}
	return out;
}

function compareStrings(a: string, b: string): number {
	return a < b ? -1 : a > b ? 1 : 0;
}

/**
 * Deeply clones JSON containers (objects and arrays) while keeping immutable primitives
 * (strings, numbers, booleans, null) by reference without JSON.stringify/JSON.parse.
 */
export function cloneJson<T>(value: T): T {
	if (Array.isArray(value)) {
		return value.map((item) => (item === undefined ? null : cloneJson(item))) as T;
	}
	if (isPlainObject(value)) {
		const out: Record<string, unknown> = {};
		for (const key of Object.keys(value)) {
			const val = value[key];
			if (val !== undefined) {
				defineJsonProp(out, key, cloneJson(val));
			}
		}
		return out as T;
	}
	return value;
}

export function jsonEqual(a: unknown, b: unknown): boolean {
	if (a === b) {
		return true;
	}
	if (Array.isArray(a) && Array.isArray(b)) {
		if (a.length !== b.length) {
			return false;
		}
		for (let i = 0; i < a.length; i++) {
			const av = a[i] === undefined ? null : a[i];
			const bv = b[i] === undefined ? null : b[i];
			if (!jsonEqual(av, bv)) {
				return false;
			}
		}
		return true;
	}
	if (isPlainObject(a) && isPlainObject(b)) {
		let aCount = 0;
		for (const key of Object.keys(a)) {
			const av = a[key];
			if (av === undefined) {
				continue;
			}
			aCount++;
			if (!hasDefinedKey(b, key) || !jsonEqual(av, b[key])) {
				return false;
			}
		}
		let bCount = 0;
		for (const key of Object.keys(b)) {
			if (b[key] !== undefined) {
				bCount++;
			}
		}
		return aCount === bCount;
	}
	return false;
}

function addJsonSize(value: unknown, acc: number, budget: number): number {
	if (acc > budget) {
		return acc;
	}
	if (value === null || value === undefined) {
		return acc + 4;
	}
	switch (typeof value) {
		case 'boolean':
			return acc + (value ? 4 : 5);
		case 'number':
			return acc + (Number.isFinite(value) ? String(value).length : 4);
		case 'string':
			return acc + value.length + 2;
		case 'object': {
			if (Array.isArray(value)) {
				let total = acc + 2;
				for (let i = 0; i < value.length; i++) {
					if (i > 0) {
						total += 1;
					}
					total = addJsonSize(value[i] === undefined ? null : value[i], total, budget);
					if (total > budget) {
						return total;
					}
				}
				return total;
			}
			const obj = value as Record<string, unknown>;
			let total = acc + 2;
			let first = true;
			for (const key of Object.keys(obj)) {
				const child = obj[key];
				if (child === undefined) {
					continue;
				}
				if (!first) {
					total += 1;
				}
				first = false;
				total += key.length + 3;
				if (total > budget) {
					return total;
				}
				total = addJsonSize(child, total, budget);
				if (total > budget) {
					return total;
				}
			}
			return total;
		}
		default:
			return acc + 4;
	}
}

/**
 * Checks whether the serialized JSON size of `value` strictly exceeds `budget`,
 * short-circuiting as soon as the limit is crossed without allocating strings.
 */
export function exceedsJsonSize(value: unknown, budget: number): boolean {
	return addJsonSize(value, 0, budget) > budget;
}

export function escapePointerSegment(segment: string): string {
	if (!segment.includes('~') && !segment.includes('/')) {
		return segment;
	}
	return segment.replace(/~/g, '~0').replace(/\//g, '~1');
}

export function encodePointer(segments: readonly string[]): string {
	if (segments.length === 0) {
		return '';
	}
	return '/' + segments.map(escapePointerSegment).join('/');
}

function unescapePointerSegment(segment: string): string {
	if (!segment.includes('~')) {
		return segment;
	}
	let out = '';
	for (let i = 0; i < segment.length; i++) {
		const ch = segment[i];
		if (ch === '~') {
			const next = segment[++i];
			if (next === '0') {
				out += '~';
			} else if (next === '1') {
				out += '/';
			} else {
				throw new Error(`invalid escape sequence in JSON Pointer segment: ${segment}`);
			}
		} else {
			out += ch;
		}
	}
	return out;
}

export function parsePointer(path: string): string[] {
	if (path === '') {
		return [];
	}
	if (!path.startsWith('/')) {
		throw new Error(`JSON Pointer must be empty or start with '/': ${path}`);
	}
	return path.slice(1).split('/').map(unescapePointerSegment);
}

/**
 * Computes an RFC 6902 patch transforming `oldVal` into `newVal`.
 * Nested objects are diffed recursively; arrays and primitives are replaced atomically.
 */
export function diffJson(oldVal: unknown, newVal: unknown): JsonPatchOp[] {
	const ops: JsonPatchOp[] = [];
	diffInto('', oldVal, newVal, ops);
	return ops;
}

function diffInto(path: string, oldVal: unknown, newVal: unknown, ops: JsonPatchOp[]): void {
	if (isPlainObject(oldVal) && isPlainObject(newVal)) {
		const oldKeys = Object.keys(oldVal)
			.filter((k) => oldVal[k] !== undefined)
			.sort(compareStrings);
		const newKeys = Object.keys(newVal)
			.filter((k) => newVal[k] !== undefined)
			.sort(compareStrings);

		for (const key of oldKeys) {
			if (!hasDefinedKey(newVal, key)) {
				ops.push({ op: 'remove', path: `${path}/${escapePointerSegment(key)}` });
			}
		}
		for (const key of newKeys) {
			const childPath = `${path}/${escapePointerSegment(key)}`;
			if (hasDefinedKey(oldVal, key)) {
				diffInto(childPath, oldVal[key], newVal[key], ops);
			} else {
				ops.push({ op: 'add', path: childPath, value: cloneJson(newVal[key]) });
			}
		}
		return;
	}

	if (!jsonEqual(oldVal, newVal)) {
		ops.push({ op: 'replace', path, value: cloneJson(newVal) });
	}
}

function setAtSegmentsStructural(
	node: unknown,
	segments: readonly string[],
	depth: number,
	value: unknown,
	requireExisting: boolean,
	fullPath: string
): unknown {
	if (!isPlainObject(node)) {
		throw new Error(`non-object parent along path '${fullPath}'`);
	}

	const key = segments[depth];
	const next = shallowClonePlainObject(node);
	if (depth === segments.length - 1) {
		if (requireExisting && !hasDefinedKey(node, key)) {
			throw new Error(`cannot replace missing path '${fullPath}'`);
		}
		defineJsonProp(next, key, value);
		return next;
	}

	if (!hasDefinedKey(node, key)) {
		throw new Error(`missing parent segment '${key}' along path '${fullPath}'`);
	}
	defineJsonProp(
		next,
		key,
		setAtSegmentsStructural(node[key], segments, depth + 1, value, requireExisting, fullPath)
	);
	return next;
}

function removeAtSegmentsStructural(
	node: unknown,
	segments: readonly string[],
	depth: number,
	fullPath: string
): unknown {
	if (!isPlainObject(node)) {
		throw new Error(`non-object parent along path '${fullPath}'`);
	}

	const key = segments[depth];
	const next = shallowClonePlainObject(node);
	if (depth === segments.length - 1) {
		if (!hasDefinedKey(node, key)) {
			throw new Error(`cannot remove missing path '${fullPath}'`);
		}
		delete next[key];
		return next;
	}

	if (!hasDefinedKey(node, key)) {
		throw new Error(`missing parent segment '${key}' along path '${fullPath}'`);
	}
	defineJsonProp(next, key, removeAtSegmentsStructural(node[key], segments, depth + 1, fullPath));
	return next;
}

/**
 * Applies RFC 6902 patch ops using structural sharing (copy-on-write along modified paths only).
 */
export function applyPatchStructural<T>(base: T, patch: readonly JsonPatchOp[]): T {
	let current: unknown = base;

	for (const op of patch) {
		const segments = parsePointer(op.path);
		if (op.op === 'add') {
			const val = cloneJson(op.value);
			current =
				segments.length === 0
					? val
					: setAtSegmentsStructural(current, segments, 0, val, false, op.path);
		} else if (op.op === 'replace') {
			const val = cloneJson(op.value);
			current =
				segments.length === 0
					? val
					: setAtSegmentsStructural(current, segments, 0, val, true, op.path);
		} else {
			if (segments.length === 0) {
				throw new Error('cannot remove root document');
			}
			current = removeAtSegmentsStructural(current, segments, 0, op.path);
		}
	}

	return current as T;
}

interface PathSlot {
	exists: boolean;
	value: unknown;
}

function readPath(root: unknown, segments: readonly string[]): PathSlot {
	let current: unknown = root;
	for (const seg of segments) {
		if (!isPlainObject(current) || !hasDefinedKey(current, seg)) {
			return { exists: false, value: undefined };
		}
		current = current[seg];
	}
	return { exists: true, value: current };
}

function setPathLenient(root: unknown, segments: readonly string[], value: unknown): unknown {
	if (segments.length === 0) {
		return value;
	}
	return setAtSegmentsStructural(root, segments, 0, value, false, encodePointer(segments));
}

function deletePathLenient(root: unknown, segments: readonly string[]): unknown {
	if (segments.length === 0) {
		return root;
	}
	const slot = readPath(root, segments);
	if (!slot.exists) {
		return root;
	}
	return removeAtSegmentsStructural(root, segments, 0, encodePointer(segments));
}

export interface MergeResult<T> {
	value: T;
	conflicts: string[];
}

function mergeValues(
	oldBase: unknown,
	local: unknown,
	remote: unknown,
	segments: readonly string[],
	policy: ConflictPolicy,
	conflicts: string[]
): unknown {
	if (jsonEqual(oldBase, remote)) {
		return policy === 'replace' ? cloneJson(remote) : cloneJson(local);
	}
	if (jsonEqual(oldBase, local)) {
		return cloneJson(remote);
	}
	if (jsonEqual(local, remote)) {
		return cloneJson(remote);
	}

	if (isPlainObject(oldBase) && isPlainObject(local) && isPlainObject(remote)) {
		const keySet = new Set<string>();
		for (const k of Object.keys(oldBase)) {
			if (oldBase[k] !== undefined) keySet.add(k);
		}
		for (const k of Object.keys(local)) {
			if (local[k] !== undefined) keySet.add(k);
		}
		for (const k of Object.keys(remote)) {
			if (remote[k] !== undefined) keySet.add(k);
		}

		const keys = Array.from(keySet).sort(compareStrings);
		const merged: Record<string, unknown> = {};

		for (const key of keys) {
			const inOld = hasDefinedKey(oldBase, key);
			const inLocal = hasDefinedKey(local, key);
			const inRemote = hasDefinedKey(remote, key);
			const childSegments = [...segments, key];

			if (inOld && inLocal && inRemote) {
				defineJsonProp(
					merged,
					key,
					mergeValues(oldBase[key], local[key], remote[key], childSegments, policy, conflicts)
				);
			} else if (!inOld) {
				if (inLocal && !inRemote) {
					if (policy !== 'replace') {
						defineJsonProp(merged, key, cloneJson(local[key]));
					}
				} else if (!inLocal && inRemote) {
					defineJsonProp(merged, key, cloneJson(remote[key]));
				} else if (inLocal && inRemote) {
					if (jsonEqual(local[key], remote[key])) {
						defineJsonProp(merged, key, cloneJson(remote[key]));
					} else {
						conflicts.push(encodePointer(childSegments));
						defineJsonProp(
							merged,
							key,
							policy === 'frontend-wins' ? cloneJson(local[key]) : cloneJson(remote[key])
						);
					}
				}
			} else if (!inLocal && !inRemote) {
				// Both removed; omit key.
			} else if (!inLocal && inRemote) {
				if (jsonEqual(oldBase[key], remote[key])) {
					if (policy === 'replace') {
						defineJsonProp(merged, key, cloneJson(remote[key]));
					}
				} else {
					conflicts.push(encodePointer(childSegments));
					if (policy !== 'frontend-wins') {
						defineJsonProp(merged, key, cloneJson(remote[key]));
					}
				}
			} else if (inLocal && !inRemote) {
				if (!jsonEqual(oldBase[key], local[key])) {
					conflicts.push(encodePointer(childSegments));
					if (policy === 'frontend-wins') {
						defineJsonProp(merged, key, cloneJson(local[key]));
					}
				}
			}
		}

		return merged;
	}

	conflicts.push(encodePointer(segments));
	return policy === 'frontend-wins' ? cloneJson(local) : cloneJson(remote);
}

/**
 * Three-way merges a full remote snapshot against `oldBase` and `local` according to `policy`.
 */
export function mergeSnapshotIntoLocal<T>(
	oldBase: unknown,
	local: unknown,
	remote: T,
	policy: ConflictPolicy
): MergeResult<T> {
	const conflicts: string[] = [];
	const value = mergeValues(oldBase, local, remote, [], policy, conflicts) as T;
	return { value, conflicts };
}

function compareSegmentPaths(a: readonly string[], b: readonly string[]): number {
	const min = Math.min(a.length, b.length);
	for (let i = 0; i < min; i++) {
		const cmp = compareStrings(a[i], b[i]);
		if (cmp !== 0) {
			return cmp;
		}
	}
	return a.length - b.length;
}

function isPrefixOf(prefix: readonly string[], candidate: readonly string[]): boolean {
	if (prefix.length > candidate.length) {
		return false;
	}
	for (let i = 0; i < prefix.length; i++) {
		if (prefix[i] !== candidate[i]) {
			return false;
		}
	}
	return true;
}

function slotsEqual(a: PathSlot, b: PathSlot): boolean {
	if (a.exists !== b.exists) {
		return false;
	}
	return !a.exists || jsonEqual(a.value, b.value);
}

function applySlotMerge(
	currentLocal: unknown,
	oldSlot: PathSlot,
	localSlot: PathSlot,
	newSlot: PathSlot,
	segments: readonly string[],
	policy: ConflictPolicy,
	conflicts: string[]
): unknown {
	if (slotsEqual(oldSlot, newSlot)) {
		return currentLocal;
	}
	if (slotsEqual(oldSlot, localSlot)) {
		return newSlot.exists
			? setPathLenient(currentLocal, segments, cloneJson(newSlot.value))
			: deletePathLenient(currentLocal, segments);
	}
	if (slotsEqual(localSlot, newSlot)) {
		return currentLocal;
	}

	if (
		oldSlot.exists &&
		localSlot.exists &&
		newSlot.exists &&
		isPlainObject(oldSlot.value) &&
		isPlainObject(localSlot.value) &&
		isPlainObject(newSlot.value)
	) {
		const mergedSubtree = mergeValues(
			oldSlot.value,
			localSlot.value,
			newSlot.value,
			segments,
			policy,
			conflicts
		);
		return setPathLenient(currentLocal, segments, mergedSubtree);
	}

	conflicts.push(encodePointer(segments));
	if (policy === 'frontend-wins') {
		return currentLocal;
	}
	return newSlot.exists
		? setPathLenient(currentLocal, segments, cloneJson(newSlot.value))
		: deletePathLenient(currentLocal, segments);
}

/**
 * Three-way merges an incoming delta `patch` (transforming `oldBase -> newBase`) into `local`
 * in O(patch.length * depth) time without traversing untouched sibling subtrees.
 */
export function mergePatchIntoLocal<T>(
	oldBase: unknown,
	local: T,
	newBase: T,
	patch: readonly JsonPatchOp[],
	policy: ConflictPolicy
): MergeResult<T> {
	if (policy === 'replace') {
		return { value: cloneJson(newBase), conflicts: [] };
	}
	if (patch.length === 0) {
		return { value: local, conflicts: [] };
	}

	const sortedPaths = patch.map((op) => parsePointer(op.path)).sort(compareSegmentPaths);
	const uniquePaths: string[][] = [];
	for (const segments of sortedPaths) {
		const prev = uniquePaths[uniquePaths.length - 1];
		if (!prev || !isPrefixOf(prev, segments)) {
			uniquePaths.push(segments);
		}
	}

	let currentLocal: unknown = local;
	const conflicts: string[] = [];
	const handledPrefixes: string[][] = [];

	for (const segments of uniquePaths) {
		if (handledPrefixes.some((prefix) => isPrefixOf(prefix, segments))) {
			continue;
		}
		if (segments.length === 0) {
			return mergeSnapshotIntoLocal(oldBase, currentLocal, newBase, policy);
		}

		let ancestorResolved = false;
		for (let depth = 0; depth < segments.length; depth++) {
			const prefix = segments.slice(0, depth);
			const localAnc = readPath(currentLocal, prefix);
			if (!localAnc.exists || !isPlainObject(localAnc.value)) {
				const oldAnc = readPath(oldBase, prefix);
				const newAnc = readPath(newBase, prefix);
				currentLocal = applySlotMerge(
					currentLocal,
					oldAnc,
					localAnc,
					newAnc,
					prefix,
					policy,
					conflicts
				);
				handledPrefixes.push(prefix);
				ancestorResolved = true;
				break;
			}
		}
		if (ancestorResolved) {
			continue;
		}

		const oldSlot = readPath(oldBase, segments);
		const localSlot = readPath(currentLocal, segments);
		const newSlot = readPath(newBase, segments);
		currentLocal = applySlotMerge(
			currentLocal,
			oldSlot,
			localSlot,
			newSlot,
			segments,
			policy,
			conflicts
		);
	}

	return { value: currentLocal as T, conflicts };
}
