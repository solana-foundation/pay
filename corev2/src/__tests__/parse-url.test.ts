import BigNumber from 'bignumber.js';
import { address } from 'gill';
import { parseURL, ParseURLError } from '../parse-url';

describe('parseURL', () => {
    const mockRecipient = '9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM';

    describe('Transfer Request URLs', () => {
        it('should parse basic transfer request URL', () => {
            const url = `solana:${mockRecipient}`;
            const result = parseURL(url);

            expect('recipient' in result).toBe(true);
            if ('recipient' in result) {
                expect(result.recipient.toString()).toBe(mockRecipient);
                expect(result.amount).toBeUndefined();
                expect(result.splToken).toBeUndefined();
                expect(result.reference).toBeUndefined();
                expect(result.label).toBeUndefined();
                expect(result.message).toBeUndefined();
                expect(result.memo).toBeUndefined();
            }
        });

        it('should parse transfer request with amount', () => {
            const url = `solana:${mockRecipient}?amount=1.5`;
            const result = parseURL(url);

            expect('recipient' in result).toBe(true);
            if ('recipient' in result) {
                expect(result.amount?.toString()).toBe('1.5');
            }
        });

        it('should parse transfer request with SPL token', () => {
            const tokenAddress = 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v';
            const url = `solana:${mockRecipient}?spl-token=${tokenAddress}`;
            const result = parseURL(url);

            expect('recipient' in result).toBe(true);
            if ('recipient' in result) {
                expect(result.splToken?.toString()).toBe(tokenAddress);
            }
        });

        it('should parse transfer request with single reference', () => {
            const reference = 'Fbri2ABzTqzq6KyGxtbE1VLe2vMU9GiGzMzUTsjJNyLk';
            const url = `solana:${mockRecipient}?reference=${reference}`;
            const result = parseURL(url);

            expect('recipient' in result).toBe(true);
            if ('recipient' in result) {
                expect(Array.isArray(result.reference)).toBe(true);
                expect(result.reference?.[0]?.toString()).toBe(reference);
            }
        });

        it('should parse transfer request with multiple references', () => {
            const ref1 = 'Fbri2ABzTqzq6KyGxtbE1VLe2vMU9GiGzMzUTsjJNyLk';
            const ref2 = '82ZJ7nbGpixjeDCmEhUcmwXYfvurzAgGdtSMuHnUgyny';
            const url = `solana:${mockRecipient}?reference=${ref1}&reference=${ref2}`;
            const result = parseURL(url);

            expect('recipient' in result).toBe(true);
            if ('recipient' in result) {
                expect(Array.isArray(result.reference)).toBe(true);
                expect(result.reference?.length).toBe(2);
                expect(result.reference?.[0]?.toString()).toBe(ref1);
                expect(result.reference?.[1]?.toString()).toBe(ref2);
            }
        });

        it('should parse transfer request with label, message, and memo', () => {
            const url = `solana:${mockRecipient}?label=Test%20Store&message=Thanks!&memo=Order%20123`;
            const result = parseURL(url);

            expect('recipient' in result).toBe(true);
            if ('recipient' in result) {
                expect(result.label).toBe('Test Store');
                expect(result.message).toBe('Thanks!');
                expect(result.memo).toBe('Order 123');
            }
        });

        it('should parse complete transfer request URL', () => {
            const tokenAddress = 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v';
            const reference = 'Fbri2ABzTqzq6KyGxtbE1VLe2vMU9GiGzMzUTsjJNyLk';
            const url = `solana:${mockRecipient}?amount=2.5&spl-token=${tokenAddress}&reference=${reference}&label=Coffee&message=Enjoy&memo=Daily`;
            const result = parseURL(url);

            expect('recipient' in result).toBe(true);
            if ('recipient' in result) {
                expect(result.recipient.toString()).toBe(mockRecipient);
                expect(result.amount?.toString()).toBe('2.5');
                expect(result.splToken?.toString()).toBe(tokenAddress);
                expect(result.reference?.[0]?.toString()).toBe(reference);
                expect(result.label).toBe('Coffee');
                expect(result.message).toBe('Enjoy');
                expect(result.memo).toBe('Daily');
            }
        });
    });

    describe('Transaction Request URLs', () => {
        it('should parse transaction request URL', () => {
            const url = 'solana:https://example.com/api/transaction';
            const result = parseURL(url);

            expect('link' in result).toBe(true);
            if ('link' in result) {
                expect(result.link.toString()).toBe('https://example.com/api/transaction');
                expect(result.label).toBeUndefined();
                expect(result.message).toBeUndefined();
            }
        });

        it('should parse transaction request with label and message', () => {
            const url = 'solana:https://example.com/api/transaction?label=Payment&message=Complete%20transaction';
            const result = parseURL(url);

            expect('link' in result).toBe(true);
            if ('link' in result) {
                expect(result.link.toString()).toBe('https://example.com/api/transaction');
                expect(result.label).toBe('Payment');
                expect(result.message).toBe('Complete transaction');
            }
        });
    });

    describe('Error cases', () => {
        it('should throw ParseURLError for invalid protocol', () => {
            expect(() => {
                parseURL('https://example.com');
            }).toThrow(ParseURLError);
        });

        it('should throw ParseURLError for missing pathname', () => {
            expect(() => {
                parseURL('solana:');
            }).toThrow(ParseURLError);
        });

        it('should throw ParseURLError for invalid recipient', () => {
            expect(() => {
                parseURL('solana:invalid-address');
            }).toThrow(ParseURLError);
        });

        it('should throw ParseURLError for invalid amount', () => {
            expect(() => {
                parseURL(`solana:${mockRecipient}?amount=invalid`);
            }).toThrow(ParseURLError);
        });

        it('should throw ParseURLError for negative amount', () => {
            expect(() => {
                parseURL(`solana:${mockRecipient}?amount=-1`);
            }).toThrow(ParseURLError);
        });

        it('should throw ParseURLError for amount with too many decimals', () => {
            expect(() => {
                parseURL(`solana:${mockRecipient}?amount=1.1234567890123`);
            }).toThrow(ParseURLError);
        });

        it('should throw ParseURLError for invalid SPL token', () => {
            expect(() => {
                parseURL(`solana:${mockRecipient}?spl-token=invalid-token`);
            }).toThrow(ParseURLError);
        });

        it('should throw ParseURLError for invalid reference', () => {
            expect(() => {
                parseURL(`solana:${mockRecipient}?reference=invalid-reference`);
            }).toThrow(ParseURLError);
        });

        it('should throw ParseURLError for URL that is too long', () => {
            const longUrl = 'solana:' + mockRecipient + '?message=' + 'a'.repeat(2048);
            expect(() => {
                parseURL(longUrl);
            }).toThrow(ParseURLError);
        });

        it('should throw ParseURLError for invalid transaction link protocol', () => {
            expect(() => {
                parseURL('solana:ftp://example.com/api');
            }).toThrow(ParseURLError);
        });
    });

    describe('Edge cases', () => {
        it('should handle URL objects', () => {
            const url = new URL(`solana:${mockRecipient}?amount=1`);
            const result = parseURL(url);

            expect('recipient' in result).toBe(true);
            if ('recipient' in result) {
                expect(result.recipient.toString()).toBe(mockRecipient);
                expect(result.amount?.toString()).toBe('1');
            }
        });

        it('should handle zero amount', () => {
            const url = `solana:${mockRecipient}?amount=0`;
            const result = parseURL(url);

            expect('recipient' in result).toBe(true);
            if ('recipient' in result) {
                expect(result.amount?.toString()).toBe('0');
            }
        });

        it('should handle amount with maximum decimals', () => {
            const url = `solana:${mockRecipient}?amount=1.123456789`;
            const result = parseURL(url);

            expect('recipient' in result).toBe(true);
            if ('recipient' in result) {
                expect(result.amount?.toString()).toBe('1.123456789');
            }
        });
    });
});