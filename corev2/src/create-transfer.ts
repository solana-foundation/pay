import type { Amount, Memo, Recipient, References, SPLToken } from './types';
import {
  address,
  Address,
  pipe,
  createTransactionMessage,
  setTransactionMessageLifetimeUsingBlockhash,
  appendTransactionMessageInstructions,
  Rpc,
  SolanaRpcApi,
  TransactionMessageWithBlockhashLifetime,
  TransactionSigner,
} from 'gill';
import { createSolTransfer } from './create-sol-transfer';
import { createSplTransfer } from './create-spl-transfer';
import { CreateTransferError } from './error';

export interface CreateTransferFields {
  recipient: Recipient;
  amount: Amount;
  splToken?: SPLToken;
  reference?: References;
  memo?: Memo;
}

export async function createTransfer(
  rpc: Rpc<SolanaRpcApi>,
  sender: Address,
  { recipient, amount, splToken, reference, memo }: CreateTransferFields,
): Promise<TransactionMessageWithBlockhashLifetime> {
  const senderInfo = await rpc.getAccountInfo(sender).send();
  if (!senderInfo.value) throw new CreateTransferError('sender not found');

  const recipientAddress = address(recipient);
  const recipientInfo = await rpc.getAccountInfo(recipientAddress).send();
  if (!recipientInfo.value) throw new CreateTransferError('recipient not found');

  const { value: latestBlockhash } = await rpc.getLatestBlockhash().send();

  const instructions = splToken
    ? await createSplTransfer(rpc, sender, {
        recipient,
        amount,
        splToken,
        reference,
        memo,
      })
    : await createSolTransfer(rpc, sender, {
        recipient,
        amount,
        reference,
        memo,
      });

  // Build transaction using functional pipe pattern as recommended by Solana Kit docs
  return pipe(
    createTransactionMessage({ version: 0 }),
    (tx) => setTransactionMessageLifetimeUsingBlockhash(latestBlockhash, tx),
    (tx) => appendTransactionMessageInstructions(instructions, tx)
  );
}