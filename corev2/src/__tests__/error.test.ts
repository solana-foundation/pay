import { CreateTransferError } from '../error';

describe('CreateTransferError', () => {
    it('should create error with message', () => {
        const message = 'Test error message';
        const error = new CreateTransferError(message);
        
        expect(error.message).toBe(message);
        expect(error.name).toBe('CreateTransferError');
        expect(error instanceof Error).toBe(true);
    });

    it('should be instanceof CreateTransferError', () => {
        const error = new CreateTransferError('test');
        
        expect(error instanceof CreateTransferError).toBe(true);
        expect(error instanceof Error).toBe(true);
    });

    it('should preserve stack trace', () => {
        const error = new CreateTransferError('test');
        
        expect(error.stack).toBeDefined();
        expect(typeof error.stack).toBe('string');
    });
});