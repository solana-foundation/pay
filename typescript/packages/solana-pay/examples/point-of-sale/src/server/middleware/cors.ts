import expressCors from 'cors';
import { wrapExpressHandler } from './wrapExpressHandler';

export const cors = wrapExpressHandler(expressCors({ origin: true, methods: ['POST'] }));
