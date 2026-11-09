import { address } from '@solana/kit';
import { describe, expect, it } from 'vitest';

import { encodeURL } from '../src/index.js';

describe('encodeURL', () => {
    describe('TransferRequestURL', () => {
        it('encodes a URL', () => {
            const recipient = address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa');
            const amount = 0.000000001;
            const splToken = address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
            const reference1 = address('9aE476sH92Vz7DMPyq5WLPkrKWivxeuTKEFKd2sZZcde');
            const reference2 = address('2jDmYQMRCBnXUQeFRvQABcU6hLcvjVTdG7AoHravxWJX');

            const reference = [reference1, reference2];
            const label = 'label';
            const message = 'message';
            const memo = 'memo';

            const url = encodeURL({ recipient, amount, splToken, reference, label, message, memo });

            expect(String(url)).toBe(
                `solana:${recipient}?amount=0.000000001&spl-token=${splToken}&reference=${reference1}&reference=${reference2}&label=${label}&message=${message}&memo=${memo}`,
            );
        });

        it('encodes a url with recipient', () => {
            const recipient = address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa');

            const url = encodeURL({ recipient });

            expect(String(url)).toBe(`solana:${recipient}`);
        });

        it('encodes a memo of 566 UTF-8 bytes and rejects 567', () => {
            const recipient = address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa');
            const ok = 'a'.repeat(564) + 'é';
            const tooLong = 'a'.repeat(567);

            expect(String(encodeURL({ recipient, memo: ok }))).toContain(`memo=${encodeURIComponent(ok)}`);
            expect(() => encodeURL({ recipient, memo: tooLong })).toThrow('memo invalid');
            // 566 UTF-16 code units, 567 UTF-8 bytes — must not pass a character-count check
            expect(() => encodeURL({ recipient, memo: 'a'.repeat(565) + 'é' })).toThrow('memo invalid');
        });

        it('encodes a url with recipient and amount', () => {
            const recipient = address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa');
            const amount = 1;

            const url = encodeURL({ recipient, amount });

            expect(String(url)).toBe(`solana:${recipient}?amount=1`);
        });

        it('encodes a url with recipient, amount and token', () => {
            const recipient = address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa');
            const amount = 1.01;
            const splToken = address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');

            const url = encodeURL({ recipient, amount, splToken });

            expect(String(url)).toBe(`solana:${recipient}?amount=1.01&spl-token=${splToken}`);
        });

        it('encodes a url with recipient, amount and references', () => {
            const recipient = address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa');
            const amount = 100000.123456;
            const reference1 = address('9aE476sH92Vz7DMPyq5WLPkrKWivxeuTKEFKd2sZZcde');
            const reference = [reference1];

            const url = encodeURL({ recipient, amount, reference });

            expect(String(url)).toBe(`solana:${recipient}?amount=100000.123456&reference=${reference1}`);
        });

        it('encodes a url with recipient, amount and label', () => {
            const recipient = address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa');
            const amount = 1.99;
            const label = 'label';

            const url = encodeURL({ recipient, amount, label });

            expect(String(url)).toBe(`solana:${recipient}?amount=1.99&label=${label}`);
        });

        it('encodes a url with recipient, amount and message', () => {
            const recipient = address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa');
            const amount = 1;
            const message = 'message';

            const url = encodeURL({ recipient, amount, message });

            expect(String(url)).toBe(`solana:${recipient}?amount=1&message=${message}`);
        });

        it('encodes a url with recipient, amount and memo', () => {
            const recipient = address('FnHyam9w4NZoWR6mKN1CuGBritdsEWZQa4Z4oawLZGxa');
            const amount = 100;
            const memo = 'memo';

            const url = encodeURL({ recipient, amount, memo });

            expect(String(url)).toBe(`solana:${recipient}?amount=100&memo=${memo}`);
        });
    });

    describe('TransactionRequestURL', () => {
        it('encodes a URL', () => {
            const link = 'https://example.com';
            const label = 'label';
            const message = 'message';

            const url = encodeURL({ link: new URL(link), label, message });

            expect(String(url)).toBe(`solana:${link}?label=${label}&message=${message}`);
        });

        it('encodes a URL with query parameters', () => {
            const link = 'https://example.com?query=param';
            const label = 'label';
            const message = 'message';

            const url = encodeURL({ link: new URL(link), label, message });

            expect(String(url)).toBe(`solana:${encodeURIComponent(link)}?label=${label}&message=${message}`);
        });
    });
});
