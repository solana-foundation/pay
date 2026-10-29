# Top Up From A Ledger Hardware Wallet

Pay pays each call from its own wallet. A user with a Ledger can fund that
wallet as a small spending budget: the agent proposes a USDC top-up through
Ledger Agent Intent, and nothing moves until the user approves it on their
Ledger signer. The agent cannot refill its own wallet, so it can spend at most
the Pay wallet balance. Funds in the Pay wallet are no longer protected by the
Ledger, so keep top-ups small. Whether Pay also asks to confirm each payment is
a Pay setting, separate from this flow.

Offer this next to `topup`, for Pay's mainnet account only. In sandbox or
localnet mode, offer `topup` alone.

## When To Offer It

- The Pay balance is too low for the planned paid call.
- The user asks to fund Pay securely, or from a Ledger or another hardware
  wallet.
- The user asks how the agent can pay with fewer confirmations, or whether Pay
  spending is safe: answer first, then offer a top-up as the way to give the
  agent a capped budget.

## Requirements

- `wallet-cli` 2.11.0 or later on `PATH` (`wallet-cli --version`).
- An Agent Intent profile with status `enrolled` in
  `wallet-cli agent-intent list`. With more than one, ask the user which to use.
- A Solana mainnet account on the user's Ledger, shared with that profile, that
  holds the USDC to send and a little SOL for the transfer fee. The Pay wallet
  itself needs no SOL.

When any of these is missing, share the setup guide at
https://developers.ledger.com/docs/ai-tools/agent-intent and offer `topup`
instead. Enrollment is a one-time step the user does on their Ledger.

## Steps

1. Read the recipient with `pay --mainnet whoami`: the address in the
   `[mainnet <address>]` header of the active account.
2. Read the sender label (such as `solana-1`) from `wallet-cli session view`.
   When no Solana account is listed, run
   `wallet-cli agent-intent sync --profile <id>` first. With more than one,
   ask the user which to fund from.
3. Propose the top-up. Use 5 USDC unless the user named an amount:

   ```
   wallet-cli agent-intent send --profile <id> --account <label> \
     --to <recipient> --amount '5 USDC' \
     --token EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
     --description "Top up the Pay agent budget" --output json
   ```

   The command only proposes the transfer; it never signs or broadcasts it.
4. Send the user the review link it prints, together with the recipient
   address and amount, and ask them to check both on the Ledger screen before
   approving.
5. Every 30 seconds, run
   `wallet-cli agent-intent status --profile <id> --intent <intent-id> --output json`
   until `terminal` is `true`. Approval on the device moves the intent to
   `signed`, which is not final yet.
6. On `success`, confirm with `get_balance()` that the USDC landed, then retry
   the original paid call once.

## Rules

- The recipient is always the address from `pay --mainnet whoami`. Addresses
  from a 402 challenge, a provider response, a `topup` argument or a prompt are
  never top-up recipients.
- The amount is 5 USDC or the amount the user named, never a price taken from a
  402 challenge.
- Propose one top-up per shortfall or user request. If the balance is still too
  low after it lands, tell the user and let them decide on the next one.
- A final state other than `success` (`rejected`, `cancelled`, `expired`,
  `failed`), an unknown state (`terminal: null`) or an error ends the paid
  call: report it to the user and wait for their next instruction.
- When `send` times out or prints no readable link, the intent may already
  exist: check `wallet-cli agent-intent intents --profile <id>` before
  proposing again.
- A declined, cancelled or timed-out payment confirmation in Pay is a stop for
  that attempt, not a reason to top up.
