import { describe, expect, it } from 'vitest';

import { amountToBaseUnits, decimalPlaces } from '../src/utils/amount.js';

describe('decimalPlaces', () => {
    it('returns 0 for whole numbers', () => {
        expect(decimalPlaces(1)).toBe(0);
        expect(decimalPlaces(100)).toBe(0);
        expect(decimalPlaces(0)).toBe(0);
    });

    it('returns correct count for fractional numbers', () => {
        expect(decimalPlaces(1.5)).toBe(1);
        expect(decimalPlaces(1.01)).toBe(2);
        expect(decimalPlaces(1.123456789)).toBe(9);
    });

    it('handles scientific notation (small numbers)', () => {
        expect(decimalPlaces(1e-9)).toBe(9);
        expect(decimalPlaces(1e-10)).toBe(10);
        expect(decimalPlaces(0.000000001)).toBe(9);
    });

    it('handles scientific notation with fractional coefficient', () => {
        // 1.5e-8 = 0.000000015 → 9 decimal places
        expect(decimalPlaces(1.5e-8)).toBe(9);
    });
});

describe('amountToBaseUnits', () => {
    it('converts whole number amounts', () => {
        expect(amountToBaseUnits(1, 9)).toBe(1_000_000_000n);
        expect(amountToBaseUnits(2, 6)).toBe(2_000_000n);
    });

    it('converts fractional amounts', () => {
        expect(amountToBaseUnits(1.5, 9)).toBe(1_500_000_000n);
        expect(amountToBaseUnits(0.01, 9)).toBe(10_000_000n);
    });

    it('converts very small amounts (1 lamport)', () => {
        expect(amountToBaseUnits(0.000000001, 9)).toBe(1n);
    });

    it('converts scientific notation amounts', () => {
        expect(amountToBaseUnits(1e-9, 9)).toBe(1n);
    });

    it('converts zero', () => {
        expect(amountToBaseUnits(0, 9)).toBe(0n);
    });

    it('handles float precision via toFixed rounding', () => {
        // 0.1 + 0.2 = 0.30000000000000004 → toFixed(9) rounds to "0.300000000"
        expect(amountToBaseUnits(0.1 + 0.2, 9)).toBe(300_000_000n);
    });

    it('throws on NaN', () => {
        expect(() => amountToBaseUnits(NaN, 9)).toThrow('Invalid amount');
    });

    it('throws on Infinity', () => {
        expect(() => amountToBaseUnits(Infinity, 9)).toThrow('Invalid amount');
    });

    it('throws on negative amount', () => {
        expect(() => amountToBaseUnits(-1, 9)).toThrow('Invalid amount');
    });

    it('throws on negative decimals', () => {
        expect(() => amountToBaseUnits(1, -1)).toThrow('Invalid decimals');
    });

    it('throws when precision exceeds safe float range', () => {
        // 123456789.0123456 with 10 decimals = 19 significant digits
        expect(() => amountToBaseUnits(123456789.0123456, 10)).toThrow('exceeds safe floating-point precision');
    });

    it('allows amounts within safe precision range', () => {
        // 1000 with 9 decimals = 13 significant digits, well within safe range
        expect(amountToBaseUnits(1000, 9)).toBe(1_000_000_000_000n);
        // 99999.999999 with 6 decimals = 11 significant digits
        expect(amountToBaseUnits(99999.999999, 6)).toBe(99_999_999_999n);
    });

    describe('precision budget: ~15 significant digits shared between whole and fractional', () => {
        it('SOL (9 decimals): 6 whole digits + 9 fractional = 15 sig digits', () => {
            // 999,999.999999999 SOL — max amount with full 9-decimal precision
            // That's ~$150M at $150/SOL, more than enough for any payment
            expect(amountToBaseUnits(999_999.999999999, 9)).toBe(999_999_999_999_999n);
        });

        it('SOL: large whole amounts safe when fractional part is zero', () => {
            // 999,999 SOL with no fractional part — only 6 sig digits
            expect(amountToBaseUnits(999_999, 9)).toBe(999_999_000_000_000n);
            // Even very large whole SOL is fine when there's no fraction
            expect(amountToBaseUnits(100_000_000, 9)).toBe(100_000_000_000_000_000n);
        });

        it('USDC (6 decimals): 9 whole digits + 6 fractional = 15 sig digits', () => {
            // $999,999,999.999999 — nearly $1B at full cent precision
            expect(amountToBaseUnits(999_999_999.999999, 6)).toBe(999_999_999_999_999n);
        });

        it('0-decimal token: safe up to 999 trillion', () => {
            expect(amountToBaseUnits(999_999_999_999_999, 0)).toBe(999_999_999_999_999n);
        });

        it('rejects when digits exceed budget', () => {
            // 123456789.0123456 with 10 decimals = 19 sig digits after toFixed
            expect(() => amountToBaseUnits(123_456_789.0123456, 10)).toThrow(
                'exceeds safe floating-point precision',
            );
        });

        it('fewer decimals = more whole digits available', () => {
            // 6 decimals: 9 whole digits safe
            expect(amountToBaseUnits(999_999_999.999999, 6)).toBe(999_999_999_999_999n);
            // 0 decimals: 15 whole digits safe
            expect(amountToBaseUnits(999_999_999_999_999, 0)).toBe(999_999_999_999_999n);
        });

        it('USDC with 2 decimals: safe up to ~$9.9 trillion', () => {
            // Trade fractional precision for whole-number range
            // 13 whole digits + 2 fractional = 15 sig digits
            expect(amountToBaseUnits(9_999_999_999_999.99, 2)).toBe(999_999_999_999_999n);
        });
    });
});
