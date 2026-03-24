import { Keypair } from '@solana/web3.js';
import BigNumber from 'bignumber.js';
import { encodeURL } from '../src';

describe('encodeURL', () => {
    describe('TransferRequestURL', () => {
        it('encodes a URL', () => {
            const recipient = Keypair.generate().publicKey;
            const amount = new BigNumber('0.000000001');
            const splToken = Keypair.generate().publicKey;
            const reference1 = Keypair.generate().publicKey;
            const reference2 = Keypair.generate().publicKey;

            const reference = [reference1, reference2];
            const label = 'label';
            const message = 'message';
            const memo = 'memo';

            const url = encodeURL({ recipient, amount, splToken, reference, label, message, memo });

            expect(String(url)).toBe(
                `solana:${recipient}?amount=0.000000001&spl-token=${splToken}&reference=${reference1}&reference=${reference2}&label=${label}&message=${message}&memo=${memo}`
            );
        });

        it('encodes a url with recipient', () => {
            const recipient = Keypair.generate().publicKey;

            const url = encodeURL({ recipient });

            expect(String(url)).toBe(`solana:${recipient}`);
        });

        it('encodes a url with recipient and amount', () => {
            const recipient = Keypair.generate().publicKey;
            const amount = new BigNumber('1');

            const url = encodeURL({ recipient, amount });

            expect(String(url)).toBe(`solana:${recipient}?amount=1`);
        });

        it('encodes a url with recipient, amount and token', () => {
            const recipient = Keypair.generate().publicKey;
            const amount = new BigNumber('1.01');
            const splToken = Keypair.generate().publicKey;

            const url = encodeURL({ recipient, amount, splToken });

            expect(String(url)).toBe(`solana:${recipient}?amount=1.01&spl-token=${splToken}`);
        });

        it('encodes a url with recipient, amount and references', () => {
            const recipient = Keypair.generate().publicKey;
            const amount = new BigNumber('100000.123456');
            const reference1 = Keypair.generate().publicKey;
            const reference = [reference1];

            const url = encodeURL({ recipient, amount, reference });

            expect(String(url)).toBe(`solana:${recipient}?amount=100000.123456&reference=${reference1}`);
        });

        it('encodes a url with recipient, amount and label', () => {
            const recipient = Keypair.generate().publicKey;
            const amount = new BigNumber('1.99');
            const label = 'label';

            const url = encodeURL({ recipient, amount, label });

            expect(String(url)).toBe(`solana:${recipient}?amount=1.99&label=${label}`);
        });

        it('encodes a url with recipient, amount and message', () => {
            const recipient = Keypair.generate().publicKey;
            const amount = new BigNumber('1');
            const message = 'message';

            const url = encodeURL({ recipient, amount, message });

            expect(String(url)).toBe(`solana:${recipient}?amount=1&message=${message}`);
        });

        it('encodes a url with recipient, amount and memo', () => {
            const recipient = Keypair.generate().publicKey;
            const amount = new BigNumber('100');
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
