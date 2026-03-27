import { createSolanaRpc } from '@solana/kit';
import { CLUSTER_ENDPOINT } from './env';

export const rpc = createSolanaRpc(CLUSTER_ENDPOINT);
