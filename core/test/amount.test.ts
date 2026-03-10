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
});
