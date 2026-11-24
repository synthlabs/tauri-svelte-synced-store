export function createEffectRoot(fn: () => void | (() => void)): () => void {
	return $effect.root(fn);
}

export function trackEffect(fn: () => void | (() => void)): void {
	$effect(fn);
}
