import { createSPLToken, createRecipient, isValidSolanaAddress } from '../utils';

describe('utils', () => {
    const validAddress = '9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM';
    const invalidAddress = 'invalid-address';

    describe('createSPLToken', () => {
        it('should create SPL token from valid address', () => {
            const token = createSPLToken(validAddress);
            expect(token.toString()).toBe(validAddress);
        });

        it('should throw error for invalid address', () => {
            expect(() => {
                createSPLToken(invalidAddress);
            }).toThrow('Invalid SPL token address');
        });
    });

    describe('createRecipient', () => {
        it('should create recipient from valid address', () => {
            const recipient = createRecipient(validAddress);
            expect(recipient.toString()).toBe(validAddress);
        });

        it('should throw error for invalid address', () => {
            expect(() => {
                createRecipient(invalidAddress);
            }).toThrow('Invalid recipient address');
        });
    });

    describe('isValidSolanaAddress', () => {
        it('should return true for valid address', () => {
            expect(isValidSolanaAddress(validAddress)).toBe(true);
        });

        it('should return false for invalid address', () => {
            expect(isValidSolanaAddress(invalidAddress)).toBe(false);
        });

        it('should return false for empty string', () => {
            expect(isValidSolanaAddress('')).toBe(false);
        });

        it('should return false for undefined', () => {
            expect(isValidSolanaAddress(undefined as any)).toBe(false);
        });
    });
});