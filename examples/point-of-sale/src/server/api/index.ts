import { createTransfer } from '@solana/pay';
import type { Address } from '@solana/kit';
import {
    address,
    createNoopSigner,
    createTransactionMessage,
    setTransactionMessageFeePayer,
    setTransactionMessageLifetimeUsingBlockhash,
    appendTransactionMessageInstructions,
    compileTransaction,
    getBase64EncodedWireTransaction,
} from '@solana/kit';
import BigNumber from 'bignumber.js';
import { NextApiHandler } from 'next';
import { rpc } from '../core';
import { cors, rateLimit } from '../middleware';

interface GetResponse {
    label: string;
    icon: string;
}

const get: NextApiHandler<GetResponse> = async (request, response) => {
    const label = request.query.label;
    if (!label) throw new Error('missing label');
    if (typeof label !== 'string') throw new Error('invalid label');

    const icon = `https://${request.headers.host}/solana-pay-logo.svg`;

    response.status(200).send({
        label,
        icon,
    });
};

interface PostResponse {
    transaction: string;
    message?: string;
}

const post: NextApiHandler<PostResponse> = async (request, response) => {
    const recipientField = request.query.recipient;
    if (!recipientField) throw new Error('missing recipient');
    if (typeof recipientField !== 'string') throw new Error('invalid recipient');
    const recipient = address(recipientField);

    const amountField = request.query.amount;
    if (!amountField) throw new Error('missing amount');
    if (typeof amountField !== 'string') throw new Error('invalid amount');
    const amount = new BigNumber(amountField);

    const splTokenField = request.query['spl-token'];
    if (splTokenField && typeof splTokenField !== 'string') throw new Error('invalid spl-token');
    const splToken: Address | undefined = splTokenField ? address(splTokenField) : undefined;

    const referenceField = request.query.reference;
    if (!referenceField) throw new Error('missing reference');
    if (typeof referenceField !== 'string') throw new Error('invalid reference');
    const reference = address(referenceField);

    const memoParam = request.query.memo;
    if (memoParam && typeof memoParam !== 'string') throw new Error('invalid memo');
    const memo = memoParam || undefined;

    const messageParam = request.query.message;
    if (messageParam && typeof messageParam !== 'string') throw new Error('invalid message');
    const message = messageParam || undefined;

    const accountField = request.body?.account;
    if (!accountField) throw new Error('missing account');
    if (typeof accountField !== 'string') throw new Error('invalid account');
    const account = address(accountField);

    // createTransfer now returns Instruction[]
    const senderSigner = createNoopSigner(account);
    const instructions = await createTransfer(rpc, senderSigner, {
        recipient,
        amount,
        splToken,
        reference,
        memo,
    });

    // Build a transaction message from the instructions
    const { value: latestBlockhash } = await rpc.getLatestBlockhash().send();
    const txMessage = appendTransactionMessageInstructions(
        instructions,
        setTransactionMessageLifetimeUsingBlockhash(
            latestBlockhash,
            setTransactionMessageFeePayer(
                account,
                createTransactionMessage({ version: 0 })
            )
        )
    );

    // Compile and serialize to base64 wire format
    const compiled = compileTransaction(txMessage);
    const base64 = getBase64EncodedWireTransaction(compiled);

    response.status(200).send({ transaction: base64, message });
};

const index: NextApiHandler<GetResponse | PostResponse> = async (request, response) => {
    await cors(request, response);
    await rateLimit(request, response);

    if (request.method === 'GET') return get(request, response);
    if (request.method === 'POST') return post(request, response);

    throw new Error(`Unexpected method ${request.method}`);
};

export default index;
