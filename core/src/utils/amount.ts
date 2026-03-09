/**
 * Convert a decimal amount to base units via string manipulation (avoids float precision issues).
 *
 * **Precision note:** JavaScript `number` has ~15 significant digits of precision.
 * For tokens with high decimals (e.g., 10), amounts above ~100,000 with full fractional
 * precision may exceed this limit. The function throws when this happens rather than
 * silently rounding. If you need arbitrary-precision amounts, convert to base units
 * (bigint) at the source rather than passing through a `number`.
 *
 * @internal
 */
export function amountToBaseUnits(amount: number, decimals: number): bigint {
    if (!Number.isFinite(amount) || amount < 0) {
        throw new Error(`Invalid amount: ${amount}`);
    }
    if (!Number.isInteger(decimals) || decimals < 0) {
        throw new Error(`Invalid decimals: ${decimals}`);
    }

    // Detect amounts where float64 has lost integer precision
    const str = amount.toFixed(decimals);
    const [whole, frac] = str.split('.');
    const significantDigits = whole.replace(/^0+/, '').length + (frac?.replace(/0+$/, '').length ?? 0);
    if (significantDigits > 15) {
        throw new Error(
            `Amount ${amount} with ${decimals} decimals exceeds safe floating-point precision (${significantDigits} significant digits > 15). ` +
                'Use a smaller amount or fewer decimals to avoid silent precision loss.',
        );
    }

    return BigInt(whole + (frac ?? ''));
}

/** Count decimal places. @internal */
export function decimalPlaces(n: number): number {
    const s = n.toString();
    // Handle scientific notation (e.g., 1e-9, 1.5e-8)
    if (s.includes('e-')) {
        const [coeff, exp] = s.split('e-');
        const coeffDecimals = (coeff.split('.')[1] || '').length;
        return coeffDecimals + Number(exp);
    }
    const dot = s.indexOf('.');
    return dot === -1 ? 0 : s.length - dot - 1;
}
