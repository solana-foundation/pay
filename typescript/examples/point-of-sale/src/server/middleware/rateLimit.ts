import expressRateLimit from 'express-rate-limit';
import { RATE_LIMIT, RATE_LIMIT_INTERVAL } from '../core';
import { wrapExpressHandler } from './wrapExpressHandler';

// Just basic IP rate-limiting for now
export const rateLimit = wrapExpressHandler(
    expressRateLimit({
        keyGenerator: (req) =>
            (req.headers['x-real-ip'] as string | undefined) ?? req.socket.remoteAddress ?? 'UNKNOWN',
        max: RATE_LIMIT ?? 10,
        windowMs: RATE_LIMIT_INTERVAL ?? 60,
    })
);
