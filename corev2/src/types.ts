import BigNumber from 'bignumber.js';
import type { Address } from 'gill';


export type Recipient = Address;

export type Amount = BigNumber;

export type SPLToken = Address;

export type Reference = Address;

export type References = Reference | Reference[];

export type Label = string;

export type Message = string;

export type Memo = string;

export type Link = URL;