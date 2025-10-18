import BigNumber from 'bignumber.js';
import { address } from 'gill';
import { encodeURL, encodeTransferRequestURL, encodeTransactionRequestURL } from '../encode-url';

describe('encodeURL', () => {
    const mockRecipient = address('9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM');

    describe('encodeTransferRequestURL', () => {
        it('should encode a basic transfer request URL', () => {
            const url = encodeTransferRequestURL({
                recipient: mockRecipient,
            });

            expect(url.protocol).toBe('solana:');
            expect(url.pathname).toBe('9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM');
            expect(url.searchParams.size).toBe(0);
        });

        it('should encode transfer request with amount', () => {
            const url = encodeTransferRequestURL({
                recipient: mockRecipient,
                amount: new BigNumber('1.5'),
            });

            expect(url.searchParams.get('amount')).toBe('1.5');
        });

        it('should encode transfer request with SPL token', () => {
            const tokenAddress = address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
            const url = encodeTransferRequestURL({
                recipient: mockRecipient,
                splToken: tokenAddress,
            });

            expect(url.searchParams.get('spl-token')).toBe('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
        });

        it('should encode transfer request with single reference', () => {
            const reference = address('Fbri2ABzTqzq6KyGxtbE1VLe2vMU9GiGzMzUTsjJNyLk');
            const url = encodeTransferRequestURL({
                recipient: mockRecipient,
                reference,
            });

            expect(url.searchParams.get('reference')).toBe('Fbri2ABzTqzq6KyGxtbE1VLe2vMU9GiGzMzUTsjJNyLk');
        });

        it('should encode transfer request with multiple references', () => {
            const references = [
                address('Fbri2ABzTqzq6KyGxtbE1VLe2vMU9GiGzMzUTsjJNyLk'),
                address('82ZJ7nbGpixjeDCmEhUcmwXYfvurzAgGdtSMuHnUgyny'),
            ];
            const url = encodeTransferRequestURL({
                recipient: mockRecipient,
                reference: references,
            });

            expect(url.searchParams.getAll('reference')).toEqual([
                'Fbri2ABzTqzq6KyGxtbE1VLe2vMU9GiGzMzUTsjJNyLk',
                '82ZJ7nbGpixjeDCmEhUcmwXYfvurzAgGdtSMuHnUgyny',
            ]);
        });

        it('should encode transfer request with label, message, and memo', () => {
            const url = encodeTransferRequestURL({
                recipient: mockRecipient,
                label: 'Test Store',
                message: 'Thanks for your purchase!',
                memo: 'Order #12345',
            });

            expect(url.searchParams.get('label')).toBe('Test Store');
            expect(url.searchParams.get('message')).toBe('Thanks for your purchase!');
            expect(url.searchParams.get('memo')).toBe('Order #12345');
        });

        it('should encode complete transfer request', () => {
            const url = encodeTransferRequestURL({
                recipient: mockRecipient,
                amount: new BigNumber('2.5'),
                splToken: address('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'),
                reference: address('Fbri2ABzTqzq6KyGxtbE1VLe2vMU9GiGzMzUTsjJNyLk'),
                label: 'Coffee Shop',
                message: 'Enjoy your coffee!',
                memo: 'Daily special',
            });

            expect(url.protocol).toBe('solana:');
            expect(url.pathname).toBe('9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM');
            expect(url.searchParams.get('amount')).toBe('2.5');
            expect(url.searchParams.get('spl-token')).toBe('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v');
            expect(url.searchParams.get('reference')).toBe('Fbri2ABzTqzq6KyGxtbE1VLe2vMU9GiGzMzUTsjJNyLk');
            expect(url.searchParams.get('label')).toBe('Coffee Shop');
            expect(url.searchParams.get('message')).toBe('Enjoy your coffee!');
            expect(url.searchParams.get('memo')).toBe('Daily special');
        });
    });

    describe('encodeTransactionRequestURL', () => {
        it('should encode transaction request with HTTPS link', () => {
            const url = encodeTransactionRequestURL({
                link: new URL('https://example.com/api/transaction'),
            });

            expect(url.protocol).toBe('https:');
            expect(url.pathname).toBe('/api/transaction');
        });

        it('should encode transaction request with label and message', () => {
            const url = encodeTransactionRequestURL({
                link: new URL('https://example.com/api/transaction'),
                label: 'Payment Request',
                message: 'Complete your transaction',
            });

            expect(url.searchParams.get('label')).toBe('Payment Request');
            expect(url.searchParams.get('message')).toBe('Complete your transaction');
        });

        it('should handle links with trailing slashes', () => {
            const url = encodeTransactionRequestURL({
                link: new URL('https://example.com/api/'),
            });

            expect(url.pathname).toBe('/api');
        });

        it('should wrap non-HTTP URLs with solana protocol', () => {
            const url = encodeTransactionRequestURL({
                link: new URL('ftp://example.com/api'),
            });
            
            expect(url.protocol).toBe('solana:');
            expect(url.pathname).toBe('ftp://example.com/api');
        });
    });

    describe('encodeURL polymorphic function', () => {
        it('should route to transfer request encoding', () => {
            const result = encodeURL({
                recipient: mockRecipient,
                amount: new BigNumber('1'),
            });

            expect(result.protocol).toBe('solana:');
            expect(result.pathname).toBe('9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM');
        });

        it('should route to transaction request encoding', () => {
            const result = encodeURL({
                link: new URL('https://example.com/api/transaction'),
                label: 'Test',
            });

            expect(result.protocol).toBe('https:');
            expect(result.pathname).toBe('/api/transaction');
        });
    });
});