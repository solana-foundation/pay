import { getTransferInstruction } from 'gill/programs';
import type { Amount, Memo, Recipient, References } from './types';
import {
  address,
  Address,
  Rpc,
  SolanaRpcApi,
  TransactionSigner,
  Instruction,
} from 'gill';
import { getTransferSolInstruction } from 'gill/programs';

export interface CreateSolTransferFields {
  recipient: Recipient;
  amount: Amount;
  reference?: References;
  memo?: Memo;
}

export async function createSolTransfer(
  rpc: Rpc<SolanaRpcApi>,
  sender: Address,
  { recipient, amount, reference, memo }: CreateSolTransferFields,
): Promise<Instruction[]> {
  const recipientAddress = address(recipient);

  return [
    getTransferInstruction({
      source: sender,
      destination: recipientAddress,
      amount: amount.toNumber(),
      authority: sender,
    }),
  ];
}