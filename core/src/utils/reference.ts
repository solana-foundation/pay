import type { Reference, References } from '../types.js';

/**
 * Normalize a single reference or array of references into a consistent array form.
 * Returns undefined when the input is undefined.
 */
export function normalizeReferences(reference: References | undefined): Reference[] | undefined {
    if (!reference) return undefined;
    return Array.isArray(reference) ? reference : [reference];
}
